#[cfg(test)]
use super::file_uri_path_identity;
use super::{
    file_path_identity, format_paths, ExternalIndexMode, LspLogger, LspServerOptions, ServerEvent,
    ServerEventSender,
};
use crate::addon_sources::{
    load_all_cached_addon_indexes, load_cached_loaded_addon_indexes,
    load_or_build_dependency_addon_indexes, load_or_build_loaded_addon_indexes,
    loaded_addon_sources_are_current, loaded_workbench_graph_matches_scope, AddonScopeAuthority,
    LoadedAddonIndexResult, LoadedAddonInstanceIdentity,
};
use crate::index::SymbolIndex;
use crate::index_cache::RuntimeIndexSummary;
use crate::model::{
    source_category_for_path, SourceFileMetadata, SourceKind, SOURCE_PRIORITY_WORKSPACE,
};
use crate::parser::parse_source;
use crate::semantic_file::{FileContribution, SemanticFile};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Instant;

const MAX_DOCUMENT_EXCLUDED_WORKSPACE_INDEXES: usize = 4;

fn index_phase(scope_authority: AddonScopeAuthority) -> &'static str {
    match scope_authority {
        AddonScopeAuthority::ProjectDependencies => "offline",
        AddonScopeAuthority::WorkbenchLoaded => "workbench-reconciliation",
    }
}

fn default_workbench_profile_directory() -> Option<PathBuf> {
    crate::workbench::default_profile_directory()
}

#[derive(Clone)]
pub(crate) struct ExternalIndexHandle {
    state: Arc<Mutex<ExternalIndexState>>,
    control: crate::index_build::IndexBuildControl,
    mode: ExternalIndexMode,
    workspace_updates: Arc<OnceLock<WorkspaceUpdateSender>>,
}

struct WorkspaceChange {
    path: PathBuf,
    replacement: Option<Arc<WorkspaceIndexedFile>>,
}

#[derive(Debug, Clone)]
struct WorkspaceUpdateSender {
    queue: Arc<WorkspaceUpdateQueue>,
}

#[derive(Debug)]
struct WorkspaceUpdateQueue {
    pending: Mutex<BTreeMap<PathBuf, Option<Arc<WorkspaceIndexedFile>>>>,
    wake: Condvar,
}

impl WorkspaceUpdateSender {
    fn submit(&self, change: WorkspaceChange) -> Result<(), String> {
        let mut pending = self.queue.pending.lock().unwrap();
        pending.insert(change.path, change.replacement);
        self.queue.wake.notify_one();
        Ok(())
    }

    fn take_batch(&self) -> BTreeMap<PathBuf, Option<Arc<WorkspaceIndexedFile>>> {
        let mut pending = self.queue.pending.lock().unwrap();
        while pending.is_empty() {
            pending = self.queue.wake.wait(pending).unwrap();
        }
        std::mem::take(&mut *pending)
    }
}

#[derive(Debug)]
struct ExternalIndexState {
    status: ExternalIndexStatus,
    generation: u64,
    workspace_index: Option<Arc<SymbolIndex>>,
    workspace_exclusions: BTreeMap<PathBuf, Arc<DocumentExcludedWorkspaceIndex>>,
    workspace_paths_by_identity: BTreeMap<String, PathBuf>,
    game_data_index: Option<Arc<SymbolIndex>>,
    workspace_files: Arc<BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>>,
    workspace_live_changes: BTreeMap<PathBuf, Option<Arc<WorkspaceIndexedFile>>>,
    workspace_last_sequences: BTreeMap<String, u64>,
    workspace_generation: u64,
    workspace_startup_pending: bool,
    workspace_roots: Vec<PathBuf>,
    addon_index_storage: Option<PathBuf>,
    graph_generation: u64,
    summary: Option<RuntimeIndexSummary>,
    workspace_summary: RuntimeIndexSummary,
    game_data_summary: Option<RuntimeIndexSummary>,
    game_data_scope_authority: Option<AddonScopeAuthority>,
    game_data_scope_instances: Vec<LoadedAddonInstanceIdentity>,
    cache_status: Option<String>,
    cache_detail: Option<String>,
    fingerprint: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceIndexedFile {
    /// The versioned public contract admitted for this workspace generation.
    /// Workspace aggregation reconstructs its query projection from this
    /// contribution, so no parallel per-file index representation is retained.
    contribution: FileContribution,
    metadata: SourceFileMetadata,
    bytes: usize,
    parse_diagnostics: usize,
}

#[derive(Debug)]
pub(crate) struct DocumentExcludedWorkspaceIndex {
    files: Arc<BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>>,
    excluded_path: PathBuf,
    projected: OnceLock<Option<Arc<SymbolIndex>>>,
}

impl DocumentExcludedWorkspaceIndex {
    fn new(
        files: Arc<BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>>,
        excluded_path: PathBuf,
    ) -> Self {
        Self {
            files,
            excluded_path,
            projected: OnceLock::new(),
        }
    }

    fn projection(&self) -> Option<Arc<SymbolIndex>> {
        self.projected
            .get_or_init(|| workspace_aggregate_excluding(&self.files, &self.excluded_path))
            .clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalIndexStatus {
    Missing,
    Building,
    Updating,
    Ready,
    Failed,
}

impl ExternalIndexStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Building => "building",
            Self::Updating => "updating",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ExternalIndexStatusSummary {
    pub(crate) status: &'static str,
    pub(crate) generation: u64,
    pub(crate) files: usize,
    pub(crate) symbols: usize,
    pub(crate) parse_diagnostics: usize,
    pub(crate) workspace_files: usize,
    pub(crate) workspace_symbols: usize,
    pub(crate) workspace_parse_diagnostics: usize,
    pub(crate) game_data_files: usize,
    pub(crate) game_data_symbols: usize,
    pub(crate) game_data_parse_diagnostics: usize,
    pub(crate) cache_status: Option<String>,
    pub(crate) cache_detail: Option<String>,
    pub(crate) fingerprint: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExternalIndexSnapshot {
    pub(crate) status: &'static str,
    pub(crate) workspace: Option<Arc<SymbolIndex>>,
    pub(crate) game_data: Option<Arc<SymbolIndex>>,
    pub(crate) workspace_exclusion: Option<Arc<DocumentExcludedWorkspaceIndex>>,
}

impl ExternalIndexSnapshot {
    pub(crate) fn available_layers(&self) -> &'static str {
        match (self.workspace.is_some(), self.game_data.is_some()) {
            (true, true) => "workspace,game-data",
            (true, false) => "workspace",
            (false, true) => "game-data",
            (false, false) => "none",
        }
    }

    pub(crate) fn workspace_for_projection(&self) -> Option<Arc<SymbolIndex>> {
        self.workspace_exclusion
            .as_ref()
            .map(|exclusion| exclusion.projection())
            .unwrap_or_else(|| self.workspace.clone())
    }

    pub(crate) fn workspace_excludes_document(&self) -> bool {
        self.workspace_exclusion.is_some()
    }

    #[cfg(test)]
    pub(crate) fn test_document_excluded() -> Self {
        Self {
            status: "ready",
            workspace: None,
            game_data: None,
            workspace_exclusion: Some(Arc::new(DocumentExcludedWorkspaceIndex::new(
                Arc::new(BTreeMap::new()),
                PathBuf::from("test-document.c"),
            ))),
        }
    }
}

impl ExternalIndexHandle {
    fn missing() -> Self {
        let state = Arc::new(Mutex::new(ExternalIndexState {
            status: ExternalIndexStatus::Missing,
            generation: 0,
            workspace_index: None,
            workspace_exclusions: BTreeMap::new(),
            workspace_paths_by_identity: BTreeMap::new(),
            game_data_index: None,
            workspace_files: Arc::new(BTreeMap::new()),
            workspace_live_changes: BTreeMap::new(),
            workspace_last_sequences: BTreeMap::new(),
            workspace_generation: 0,
            workspace_startup_pending: false,
            workspace_roots: Vec::new(),
            addon_index_storage: None,
            graph_generation: 0,
            summary: None,
            workspace_summary: RuntimeIndexSummary::default(),
            game_data_summary: None,
            game_data_scope_authority: None,
            game_data_scope_instances: Vec::new(),
            cache_status: None,
            cache_detail: None,
            fingerprint: None,
            error: None,
        }));
        Self {
            control: crate::index_build::IndexBuildControl::default(),
            mode: ExternalIndexMode::Loaded,
            state,
            workspace_updates: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn status_summary(&self) -> ExternalIndexStatusSummary {
        let state = self.state.lock().unwrap();
        let summary = state.summary.as_ref();
        let game_data_summary = state.game_data_summary.as_ref();
        ExternalIndexStatusSummary {
            status: state.status.as_str(),
            generation: state.generation,
            files: summary.map(|summary| summary.files).unwrap_or(0),
            symbols: summary.map(|summary| summary.indexed_symbols).unwrap_or(0),
            parse_diagnostics: summary
                .map(|summary| summary.parse_diagnostics)
                .unwrap_or(0),
            workspace_files: state.workspace_summary.files,
            workspace_symbols: state.workspace_summary.indexed_symbols,
            workspace_parse_diagnostics: state.workspace_summary.parse_diagnostics,
            game_data_files: game_data_summary.map(|summary| summary.files).unwrap_or(0),
            game_data_symbols: game_data_summary
                .map(|summary| summary.indexed_symbols)
                .unwrap_or(0),
            game_data_parse_diagnostics: game_data_summary
                .map(|summary| summary.parse_diagnostics)
                .unwrap_or(0),
            cache_status: state.cache_status.clone(),
            cache_detail: state.cache_detail.clone(),
            fingerprint: state.fingerprint.clone(),
            error: state.error.clone(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.control.cancel();
    }

    pub(crate) fn load_workbench_graph(
        &self,
        inventory_path: PathBuf,
        logger: LspLogger,
        event_sender: Option<ServerEventSender>,
    ) -> Result<(), String> {
        if matches!(self.mode, ExternalIndexMode::All | ExternalIndexMode::None) {
            return Ok(());
        }
        let (storage, workspace_roots, warm_scope, graph_generation) = {
            let mut state = self.state.lock().unwrap();
            let storage = state
                .addon_index_storage
                .clone()
                .ok_or_else(|| "add-on index storage is unavailable".to_string())?;
            state.status = ExternalIndexStatus::Updating;
            state.graph_generation += 1;
            (
                storage,
                state.workspace_roots.clone(),
                state.game_data_scope_instances.clone(),
                state.graph_generation,
            )
        };
        if let Some(sender) = &event_sender {
            let _ = sender.send(ServerEvent::ExternalIndexProgress {
                phase: "inventory-load-start".to_string(),
            });
        }
        let state = self.state.clone();
        let control = self.control.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let warm_scope_reused = !warm_scope.is_empty()
                && loaded_workbench_graph_matches_scope(
                    &inventory_path,
                    &workspace_roots,
                    &warm_scope,
                )
                .unwrap_or(false);
            if warm_scope_reused {
                if let Ok(mut state) = state.lock() {
                    if state.graph_generation == graph_generation {
                        state.status = ExternalIndexStatus::Ready;
                        state.game_data_scope_authority =
                            Some(AddonScopeAuthority::WorkbenchLoaded);
                        state.cache_status = Some("optimistic-loaded".to_string());
                        state.cache_detail = Some(format!(
                            "scopeAuthority={} loadedInstances={} sourceValidation=pending warmSnapshot=reused",
                            AddonScopeAuthority::WorkbenchLoaded.as_str(),
                            warm_scope.len(),
                        ));
                        state.error = None;
                    }
                }
                logger.diagnostic_lazy("externalIndex.warmSnapshotReused", || {
                    json!({
                        "phase": "workbench-reconciliation",
                        "elapsedMs": started.elapsed().as_millis(),
                        "instances": warm_scope.len(),
                        "sourceValidation": "pending"
                    })
                });
                if let Some(sender) = &event_sender {
                    let _ = sender.send(ServerEvent::ExternalIndexChanged);
                }
                match loaded_addon_sources_are_current(
                    &inventory_path,
                    &storage,
                    &workspace_roots,
                    &control,
                ) {
                    Ok(true) => {
                        if let Ok(mut state) = state.lock() {
                            if state.graph_generation == graph_generation {
                                state.cache_status = Some("loaded".to_string());
                                state.cache_detail = Some(format!(
                                    "scopeAuthority={} loadedInstances={} sourceValidation=complete warmSnapshot=reused",
                                    AddonScopeAuthority::WorkbenchLoaded.as_str(),
                                    warm_scope.len(),
                                ));
                                state.error = None;
                            }
                        }
                        logger.diagnostic_lazy("externalIndex.warmSnapshotValidated", || {
                            json!({
                                "phase": "workbench-reconciliation",
                                "elapsedMs": started.elapsed().as_millis(),
                                "instances": warm_scope.len(),
                                "reused": true
                            })
                        });
                        return;
                    }
                    Ok(false) => {
                        logger.diagnostic_lazy("externalIndex.warmSnapshotChanged", || {
                            json!({
                                "phase": "workbench-reconciliation",
                                "elapsedMs": started.elapsed().as_millis(),
                                "instances": warm_scope.len(),
                                "reused": false
                            })
                        });
                    }
                    Err(error) => {
                        if let Ok(mut state) = state.lock() {
                            if state.graph_generation == graph_generation {
                                state.error = Some(error.clone());
                                state.cache_status = Some("optimistic-loaded".to_string());
                                state.cache_detail = Some(format!(
                                    "scopeAuthority={} loadedInstances={} sourceValidation=failed",
                                    AddonScopeAuthority::WorkbenchLoaded.as_str(),
                                    warm_scope.len(),
                                ));
                                state.status = ExternalIndexStatus::Ready;
                            }
                        }
                        logger.diagnostic_lazy(
                            "externalIndex.deferredSourceValidationFailed",
                            || {
                                json!({
                                    "phase": "workbench-reconciliation",
                                    "elapsedMs": started.elapsed().as_millis(),
                                    "error": error
                                })
                            },
                        );
                        return;
                    }
                }
            }
            let result = load_or_build_loaded_addon_indexes(
                &inventory_path,
                &storage,
                &workspace_roots,
                &control,
            );
            let mut state = state.lock().unwrap();
            if state.graph_generation != graph_generation {
                return;
            }
            let reconciliation_succeeded = result.is_ok();
            match result {
                Ok(result) => {
                    log_loaded_addon_index_diagnostics(&logger, &result);
                    publish_loaded_addon_result(&mut state, &result, false);
                    logger.diagnostic_lazy("externalIndex.graphDelivered", || json!({"phase": index_phase(result.scope_authority), "elapsedMs": started.elapsed().as_millis(), "loadedInstances": result.loaded_instances, "rebuiltInstances": result.rebuilt_instances, "workspaceExcludedInstances": result.workspace_excluded_instances}));
                }
                Err(error) => {
                    let keep_offline_scope = state.game_data_scope_authority
                        == Some(AddonScopeAuthority::ProjectDependencies);
                    if !keep_offline_scope {
                        state.game_data_index = None;
                        state.game_data_summary = None;
                        state.game_data_scope_authority = None;
                        state.cache_status = None;
                        state.cache_detail = None;
                        state.fingerprint = None;
                    }
                    state.error = Some(error.clone());
                    let game_data = state.game_data_summary.clone();
                    recompute_summary(&mut state, game_data);
                    state.generation += 1;
                    state.status =
                        if state.workspace_index.is_some() || state.game_data_index.is_some() {
                            ExternalIndexStatus::Ready
                        } else {
                            ExternalIndexStatus::Failed
                        };
                    logger.diagnostic_lazy(
                        "externalIndex.graphDeliveryFailed",
                        || json!({"phase": "workbench-reconciliation", "elapsedMs": started.elapsed().as_millis(), "error": error}),
                    );
                }
            }
            drop(state);
            if let Some(sender) = event_sender {
                if reconciliation_succeeded {
                    let _ = sender.send(ServerEvent::ExternalIndexProgress {
                        phase: "workbench-reconciliation".to_string(),
                    });
                }
                let _ = sender.send(ServerEvent::ExternalIndexChanged);
            }
        });
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> ExternalIndexSnapshot {
        let state = self.state.lock().unwrap();
        ExternalIndexSnapshot {
            status: state.status.as_str(),
            workspace: state.workspace_index.clone(),
            game_data: state.game_data_index.clone(),
            workspace_exclusion: None,
        }
    }

    /// Returns the external facts used by rich semantic projection without
    /// the active document's workspace contribution. The excluded aggregate
    /// is constructed lazily by the rich worker and cached in a small bounded
    /// set for the current workspace generation. The request path captures
    /// only immutable file contributions and never rebuilds an index.
    pub(crate) fn snapshot_for_document_identity(
        &self,
        identity: Option<&str>,
    ) -> ExternalIndexSnapshot {
        let Some(identity) = identity else {
            return self.snapshot();
        };
        let mut state = self.state.lock().unwrap();
        let Some(path) = state.workspace_paths_by_identity.get(identity).cloned() else {
            return ExternalIndexSnapshot {
                status: state.status.as_str(),
                workspace: state.workspace_index.clone(),
                game_data: state.game_data_index.clone(),
                workspace_exclusion: None,
            };
        };
        let exclusion = state
            .workspace_exclusions
            .get(&path)
            .cloned()
            .unwrap_or_else(|| {
                if state.workspace_exclusions.len() >= MAX_DOCUMENT_EXCLUDED_WORKSPACE_INDEXES {
                    if let Some(evicted) = state.workspace_exclusions.keys().next().cloned() {
                        state.workspace_exclusions.remove(&evicted);
                    }
                }
                let exclusion = Arc::new(DocumentExcludedWorkspaceIndex::new(
                    state.workspace_files.clone(),
                    path.clone(),
                ));
                state.workspace_exclusions.insert(path, exclusion.clone());
                exclusion
            });
        ExternalIndexSnapshot {
            status: state.status.as_str(),
            workspace: state.workspace_index.clone(),
            game_data: state.game_data_index.clone(),
            workspace_exclusion: Some(exclusion),
        }
    }

    pub(crate) fn update_workspace_file(
        &self,
        path: PathBuf,
        text: String,
        sequence: u64,
    ) -> Result<Option<(usize, usize)>, String> {
        if !self.accept_workspace_sequence(&path, sequence) {
            return Ok(None);
        }
        let normalized_path = normalize_workspace_path(&path);
        let root = {
            let state = self.state.lock().unwrap();
            workspace_root_for_file(&state.workspace_roots, &path)
                .or_else(|| workspace_root_for_file(&state.workspace_roots, &normalized_path))
        }
        .unwrap_or_else(|| {
            normalized_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });

        let indexed = Arc::new(build_workspace_file_index(&root, &normalized_path, &text));
        let symbol_count = indexed.contribution.symbols.len();
        let parse_diagnostics = indexed.parse_diagnostics;
        self.workspace_sender().submit(WorkspaceChange {
            path: normalized_path,
            replacement: Some(indexed),
        })?;
        Ok(Some((symbol_count, parse_diagnostics)))
    }

    pub(crate) fn delete_workspace_file(&self, path: &Path, sequence: u64) -> Option<bool> {
        if !self.accept_workspace_sequence(path, sequence) {
            return None;
        }
        let normalized_path = normalize_workspace_path(path);
        Some(
            self.workspace_sender()
                .submit(WorkspaceChange {
                    path: normalized_path,
                    replacement: None,
                })
                .is_ok(),
        )
    }

    fn workspace_sender(&self) -> &WorkspaceUpdateSender {
        self.workspace_updates
            .get_or_init(|| start_workspace_update_worker(self.state.clone(), None))
    }

    fn accept_workspace_sequence(&self, path: &Path, sequence: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        let key = workspace_sequence_key(path);
        if state
            .workspace_last_sequences
            .get(&key)
            .is_some_and(|last_sequence| *last_sequence >= sequence)
        {
            return false;
        }
        state.workspace_last_sequences.insert(key, sequence);
        true
    }
}

fn publish_loaded_addon_result(
    state: &mut ExternalIndexState,
    result: &LoadedAddonIndexResult,
    validation_pending: bool,
) {
    state.game_data_index = Some(result.index.clone());
    state.game_data_summary = Some(result.summary.clone());
    state.game_data_scope_authority = Some(result.scope_authority);
    state.game_data_scope_instances = result.scope_instances.clone();
    state.cache_status = Some(
        if validation_pending {
            "optimistic-loaded"
        } else if result.rebuilt_instances == 0 {
            "loaded"
        } else {
            "rebuilt"
        }
        .to_string(),
    );
    state.cache_detail = Some(format!(
        "scopeAuthority={} loadedInstances={} rebuiltInstances={} missingInstances={} workspaceExcludedInstances={}{}",
        result.scope_authority.as_str(),
        result.loaded_instances,
        result.rebuilt_instances,
        result.missing_instances,
        result.workspace_excluded_instances,
        if validation_pending { " sourceValidation=pending" } else { "" },
    ));
    state.fingerprint = Some(format!(
        "workbench-loaded-addons:{}",
        result.loaded_instances + result.rebuilt_instances
    ));
    state.error = None;
    let game_data = state.game_data_summary.clone();
    recompute_summary(state, game_data);
    state.generation += 1;
    state.status = ExternalIndexStatus::Ready;
}

pub(crate) fn start_external_index(
    options: &LspServerOptions,
    logger: LspLogger,
    event_sender: Option<ServerEventSender>,
) -> ExternalIndexHandle {
    let can_load_game_data = match options.external_index_mode {
        ExternalIndexMode::All => options.addon_index_storage.is_some(),
        ExternalIndexMode::Loaded => {
            options.addon_source_inventory.is_some() || !options.dependency_project_files.is_empty()
        }
        ExternalIndexMode::None => false,
    };
    if !can_load_game_data && options.workspace_scripts.is_empty() {
        return ExternalIndexHandle::missing();
    }

    logger.diagnostic_lazy("externalIndex.started", || {
        json!({
            "gameDataConfigured": options.addon_source_inventory.is_some(),
            "offlineDependencyProjects": options.dependency_project_files.len(),
            "indexStorageConfigured": options.addon_index_storage.is_some(),
            "workspaceRoots": options.workspace_scripts.len(),
        })
    });

    let control = crate::index_build::IndexBuildControl::default();
    let state = Arc::new(Mutex::new(ExternalIndexState {
        status: ExternalIndexStatus::Building,
        generation: 0,
        workspace_index: None,
        workspace_exclusions: BTreeMap::new(),
        workspace_paths_by_identity: BTreeMap::new(),
        game_data_index: None,
        workspace_files: Arc::new(BTreeMap::new()),
        workspace_live_changes: BTreeMap::new(),
        workspace_last_sequences: BTreeMap::new(),
        workspace_generation: 0,
        workspace_startup_pending: true,
        workspace_roots: options.workspace_scripts.clone(),
        addon_index_storage: options.addon_index_storage.clone(),
        graph_generation: 0,
        summary: None,
        workspace_summary: RuntimeIndexSummary::default(),
        game_data_summary: None,
        game_data_scope_authority: None,
        game_data_scope_instances: Vec::new(),
        cache_status: None,
        cache_detail: None,
        fingerprint: None,
        error: None,
    }));
    let workspace_updates = Arc::new(OnceLock::new());
    workspace_updates
        .set(start_workspace_update_worker(
            state.clone(),
            event_sender.clone(),
        ))
        .expect("workspace update worker is initialized once");
    let handle = ExternalIndexHandle {
        control: control.clone(),
        mode: options.external_index_mode,
        state,
        workspace_updates,
    };

    let state = handle.state.clone();
    let addon_source_inventory = options.addon_source_inventory.clone();
    let addon_index_storage = options.addon_index_storage.clone();
    let external_index_mode = options.external_index_mode;
    let workspace_roots = options.workspace_scripts.clone();
    let dependency_project_files = options.dependency_project_files.clone();
    thread::spawn(move || {
        let thread_logger = logger.clone();
        let panic_state = state.clone();
        let progress_sender = event_sender.clone();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            run_external_index_thread(
                state,
                addon_source_inventory,
                addon_index_storage,
                external_index_mode,
                workspace_roots,
                dependency_project_files,
                logger,
                progress_sender,
                control,
            );
        })) {
            let panic_message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic>");
            thread_logger
                .log_lazy(|| format!("externalIndex thread panic error={}", panic_message));
            if let Ok(mut state) = panic_state.lock() {
                state.status = ExternalIndexStatus::Failed;
                state.error = Some(format!("external-index startup panicked: {panic_message}"));
                state.generation += 1;
            }
        }
        if let Some(sender) = event_sender {
            let _ = sender.send(ServerEvent::ExternalIndexChanged);
        }
    });

    handle
}

fn start_workspace_update_worker(
    state: Arc<Mutex<ExternalIndexState>>,
    event_sender: Option<ServerEventSender>,
) -> WorkspaceUpdateSender {
    let sender = WorkspaceUpdateSender {
        queue: Arc::new(WorkspaceUpdateQueue {
            pending: Mutex::new(BTreeMap::new()),
            wake: Condvar::new(),
        }),
    };
    let worker_queue = sender.clone();
    thread::spawn(move || loop {
        let changes = worker_queue.take_batch();
        if publish_workspace_changes(&state, &changes) {
            if let Some(sender) = &event_sender {
                let _ = sender.send(ServerEvent::ExternalIndexChanged);
            }
        }
    });
    sender
}

fn publish_workspace_changes(
    state: &Arc<Mutex<ExternalIndexState>>,
    changes: &BTreeMap<PathBuf, Option<Arc<WorkspaceIndexedFile>>>,
) -> bool {
    loop {
        let (mut files, workspace_generation, startup_pending) = {
            let state = state.lock().unwrap();
            (
                state.workspace_files.as_ref().clone(),
                state.workspace_generation,
                state.workspace_startup_pending,
            )
        };
        let mut published_change = false;
        for (path, replacement) in changes {
            if let Some(indexed) = replacement {
                files.insert(path.clone(), indexed.clone());
                published_change = true;
            } else if files.remove(path).is_some() || startup_pending {
                published_change = true;
            }
        }
        let (workspace_index, workspace_summary) = workspace_aggregate(&files);
        let workspace_paths_by_identity = workspace_identity_paths(&files);
        let mut state = state.lock().unwrap();
        if state.workspace_generation != workspace_generation {
            continue;
        }
        state.status = ExternalIndexStatus::Updating;
        if startup_pending {
            for (path, replacement) in changes {
                state
                    .workspace_live_changes
                    .insert(path.clone(), replacement.clone());
            }
        }
        state.workspace_files = Arc::new(files);
        state.workspace_index = workspace_index;
        state.workspace_exclusions.clear();
        state.workspace_paths_by_identity = workspace_paths_by_identity;
        state.workspace_summary = workspace_summary;
        let game_data_summary = state.game_data_summary.clone();
        recompute_summary(&mut state, game_data_summary);
        if published_change {
            state.workspace_generation += 1;
            state.generation += 1;
        }
        state.status = if state.workspace_index.is_some() || state.game_data_index.is_some() {
            ExternalIndexStatus::Ready
        } else {
            ExternalIndexStatus::Missing
        };
        return published_change;
    }
}

/// Emits the bounded per-add-on cache breakdown for both startup and a graph
/// delivered after LSP initialization. The latter is the normal editor path,
/// so it must expose the same evidence as direct startup indexing.
fn log_loaded_addon_index_diagnostics(logger: &LspLogger, result: &LoadedAddonIndexResult) {
    for instance in &result.instances {
        logger.diagnostic_lazy("externalIndex.addonCompleted", || {
            json!({
                "phase": index_phase(result.scope_authority),
                "scopeAuthority": result.scope_authority.as_str(),
                "guid": instance.guid,
                "displayId": instance.display_id,
                "cacheStatus": instance.cache_status,
                "cacheDetail": instance.cache_detail,
                "packs": instance.pack_count,
                "scripts": instance.script_count,
                "files": instance.summary.files,
                "bytes": instance.summary.bytes,
                "symbols": instance.summary.indexed_symbols,
                "parseDiagnostics": instance.summary.parse_diagnostics,
                "cacheFileBytes": instance.cache_file_bytes,
                "timingsMs": {
                    "inspection": instance.timings.fingerprint.as_millis(),
                    "cacheMetadataRead": instance.timings.cache_metadata_read.as_millis(),
                    "cacheFileRead": instance.timings.cache_file_read.as_millis(),
                    "cacheDecode": instance.timings.cache_decode.as_millis(),
                    "cacheValidate": instance.timings.cache_validate.as_millis(),
                    "runtimeMapRebuild": instance.timings.map_rebuild.as_millis(),
                    "runtimeProjection": instance.timings.map_projection.as_millis(),
                    "runtimeLookupMapBuild": instance.timings.map_lookup_rebuild.as_millis(),
                    "cacheReadDeserializeValidate": instance.timings.cache_read_deserialize_validate.as_millis(),
                    "sourceRebuild": instance.timings.rebuild.as_millis(),
                    "sourceBuild": {
                        "fileDiscovery": instance.timings.source_build.file_discovery.as_millis(),
                        "sourceAcquisition": instance.timings.source_build.source_acquisition.as_millis(),
                        "readDecode": instance.timings.source_build.read_decode.as_millis(),
                        "parse": instance.timings.source_build.parse.as_millis(),
                        "semanticModel": instance.timings.source_build.ast_model_catalog.as_millis(),
                        "catalogBuild": instance.timings.source_build.catalog_build.as_millis(),
                        "indexAggregation": instance.timings.source_build.index_build.as_millis(),
                        "total": instance.timings.source_build.total.as_millis(),
                    },
                    "cachePrepare": instance.timings.cache_prepare.as_millis(),
                    "cacheCompact": instance.timings.cache_compact.as_millis(),
                    "cachePayloadPrepare": instance.timings.cache_payload_prepare.as_millis(),
                    "cacheWrite": instance.timings.cache_write.as_millis(),
                    "cacheEncode": instance.timings.cache_encode.as_millis(),
                    "cacheAtomicWrite": instance.timings.cache_atomic_write.as_millis(),
                    "cacheMetadataPublish": instance.timings.cache_metadata_publish.as_millis(),
                    "total": instance.timings.total.as_millis(),
                }
            })
        });
    }
    logger.diagnostic_lazy("externalIndex.gameDataCompleted", || {
        json!({
            "phase": index_phase(result.scope_authority),
            "scopeAuthority": result.scope_authority.as_str(),
            "loadedInstances": result.loaded_instances,
            "rebuiltInstances": result.rebuilt_instances,
            "workspaceExcludedInstances": result.workspace_excluded_instances,
            "timingsMs": {
                "graphRead": result.timings.graph_read.as_millis(),
                "workspaceRootResolution": result.timings.workspace_root_resolution.as_millis(),
                "cachePrune": result.timings.cache_prune.as_millis(),
                "cacheMetadataRead": result.timings.cache_metadata_read.as_millis(),
                "sourceInspection": result.timings.source_inspection.as_millis(),
                "indexLoadOrBuild": result.timings.index_load_or_build.as_millis(),
                "layerRebase": result.timings.layer_rebase.as_millis(),
                "layerFileProjection": result.timings.layer_file_projection.as_millis(),
                "layerLookupProjection": result.timings.layer_lookup_projection.as_millis(),
                "layerCompose": result.timings.layer_compose.as_millis(),
                "total": result.timings.total.as_millis(),
            }
        })
    });
}

fn empty_loaded_addon_index_result() -> LoadedAddonIndexResult {
    LoadedAddonIndexResult {
        index: Arc::new(SymbolIndex::layered(Vec::new())),
        source_line_starts: BTreeMap::new(),
        scope_authority: crate::addon_sources::AddonScopeAuthority::WorkbenchLoaded,
        summary: RuntimeIndexSummary::default(),
        rebuilt_instances: 0,
        loaded_instances: 0,
        missing_instances: 0,
        workspace_excluded_instances: 0,
        timings: Default::default(),
        instances: Vec::new(),
        unavailable_instances: Vec::new(),
        scope_instances: Vec::new(),
    }
}

fn run_external_index_thread(
    state: Arc<Mutex<ExternalIndexState>>,
    addon_source_inventory: Option<PathBuf>,
    addon_index_storage: Option<PathBuf>,
    external_index_mode: ExternalIndexMode,
    workspace_roots: Vec<PathBuf>,
    dependency_project_files: Vec<PathBuf>,
    logger: LspLogger,
    event_sender: Option<ServerEventSender>,
    control: crate::index_build::IndexBuildControl,
) {
    let start = Instant::now();
    logger.log_lazy(|| {
        format!(
            "externalIndex start addon_source_inventory={} workspace_roots={}",
            addon_source_inventory
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unset>".to_string()),
            format_paths(&workspace_roots)
        )
    });

    // Hydrate the offline cache before any source inspection. A dependency
    // scope can then serve immediately while the Workbench graph is acquired;
    // a later graph delivery performs the authoritative reconciliation.
    if let Some(storage) = addon_index_storage.as_ref() {
        let optimistic_start = Instant::now();
        let cached = match external_index_mode {
            ExternalIndexMode::All => {
                load_all_cached_addon_indexes(storage, &workspace_roots, &control)
            }
            ExternalIndexMode::Loaded
                if let Some(inventory_path) = addon_source_inventory.as_ref() =>
            {
                load_cached_loaded_addon_indexes(
                    inventory_path,
                    storage,
                    &workspace_roots,
                    &control,
                )
            }
            _ => Ok(empty_loaded_addon_index_result()),
        };
        match cached {
            Ok(result) if result.loaded_instances > 0 => {
                log_loaded_addon_index_diagnostics(&logger, &result);
                let mut published = state.lock().unwrap();
                publish_loaded_addon_result(&mut published, &result, true);
                drop(published);
                logger.diagnostic_lazy("externalIndex.optimisticCacheDelivered", || json!({
                    "phase": index_phase(result.scope_authority),
                    "scopeAuthority": result.scope_authority.as_str(),
                    "elapsedMs": optimistic_start.elapsed().as_millis(),
                    "loadedInstances": result.loaded_instances,
                    "missingInstances": result.missing_instances,
                    "workspaceExcludedInstances": result.workspace_excluded_instances,
                    "timingsMs": {
                        "graphRead": result.timings.graph_read.as_millis(),
                        "workspaceRootResolution": result.timings.workspace_root_resolution.as_millis(),
                        "cachePrune": result.timings.cache_prune.as_millis(),
                        "cacheMetadataRead": result.timings.cache_metadata_read.as_millis(),
                        "indexLoad": result.timings.index_load_or_build.as_millis(),
                        "layerCompose": result.timings.layer_compose.as_millis(),
                        "total": result.timings.total.as_millis(),
                    }
                }));
                if let Some(sender) = &event_sender {
                    let _ = sender.send(ServerEvent::ExternalIndexProgress {
                        phase: index_phase(result.scope_authority).to_string(),
                    });
                    let _ = sender.send(ServerEvent::ExternalIndexChanged);
                }
            }
            Ok(result) => logger.diagnostic_lazy("externalIndex.optimisticCacheUnavailable", || json!({"phase": index_phase(result.scope_authority), "scopeAuthority": result.scope_authority.as_str(), "elapsedMs": optimistic_start.elapsed().as_millis(), "missingInstances": result.missing_instances, "workspaceExcludedInstances": result.workspace_excluded_instances})),
            Err(error) => logger.diagnostic_lazy("externalIndex.optimisticCacheFailed", || json!({"elapsedMs": optimistic_start.elapsed().as_millis(), "error": error})),
        }
    }

    let game_data_start = Instant::now();
    let game_data_result = match external_index_mode {
        ExternalIndexMode::None => None,
        ExternalIndexMode::All => Some(
            addon_index_storage
                .as_ref()
                .ok_or_else(|| "add-on index storage is unavailable".to_string())
                .and_then(|storage| {
                    load_all_cached_addon_indexes(storage, &workspace_roots, &control)
                }),
        ),
        ExternalIndexMode::Loaded
            if addon_source_inventory.is_none() && !dependency_project_files.is_empty() =>
        {
            Some(addon_index_storage.as_ref().map_or_else(
                || Err("add-on index storage is unavailable".to_string()),
                |storage| {
                    load_or_build_dependency_addon_indexes(
                        &dependency_project_files,
                        default_workbench_profile_directory().as_deref(),
                        storage,
                        &workspace_roots,
                        &control,
                    )
                },
            ))
        }
        ExternalIndexMode::Loaded if addon_source_inventory.is_none() => None,
        ExternalIndexMode::Loaded => addon_source_inventory.map(|inventory_path| {
            for phase in ["inventory-load-start", "pac-inspect-start"] {
                if let Some(sender) = &event_sender {
                    let _ = sender.send(ServerEvent::ExternalIndexProgress {
                        phase: phase.to_string(),
                    });
                }
            }
            let result = addon_index_storage
                .ok_or_else(|| "add-on index storage is unavailable".to_string())
                .and_then(|storage| {
                    load_or_build_loaded_addon_indexes(
                        &inventory_path,
                        &storage,
                        &workspace_roots,
                        &control,
                    )
                });
            if let Some(sender) = &event_sender {
                for phase in ["inventory-load-end", "pac-inspect-end"] {
                    let _ = sender.send(ServerEvent::ExternalIndexProgress {
                        phase: phase.to_string(),
                    });
                }
                let phase = match &result {
                    Ok(result) => index_phase(result.scope_authority),
                    Err(_) => "addon-cache-failed",
                };
                let _ = sender.send(ServerEvent::ExternalIndexProgress {
                    phase: phase.to_string(),
                });
            }
            result
        }),
    };
    let has_game_data_source = game_data_result.is_some();
    let game_data_ready_ms = game_data_start.elapsed().as_millis();
    logger.log_lazy(|| {
        format!(
            "externalIndex gameData load returned has_result={} elapsed_ms={}",
            game_data_result.is_some(),
            start.elapsed().as_millis()
        )
    });

    let workspace_start = Instant::now();
    if !workspace_roots.is_empty() {
        if let Some(sender) = &event_sender {
            let _ = sender.send(ServerEvent::ExternalIndexProgress {
                phase: "workspace-rebuild-start".to_string(),
            });
        }
    }
    logger.log_lazy(|| {
        format!(
            "externalIndex workspace start roots={} elapsed_ms={}",
            format_paths(&workspace_roots),
            start.elapsed().as_millis()
        )
    });
    let workspace_result = build_workspace_indexes(&workspace_roots, &logger, start);
    if !workspace_roots.is_empty() {
        if let Some(sender) = &event_sender {
            let _ = sender.send(ServerEvent::ExternalIndexProgress {
                phase: "workspace-rebuild-end".to_string(),
            });
        }
    }
    let workspace_ready_ms = workspace_start.elapsed().as_millis();
    logger.log_lazy(|| {
        format!(
            "externalIndex workspace load returned success={} elapsed_ms={}",
            workspace_result.is_ok(),
            start.elapsed().as_millis()
        )
    });

    logger.log_lazy(|| {
        format!(
            "externalIndex publish start elapsed_ms={}",
            start.elapsed().as_millis()
        )
    });
    let mut error_messages = Vec::new();
    let (
        game_data_index,
        game_data_summary,
        game_data_scope_authority,
        cache_status,
        cache_detail,
        fingerprint,
    ) = match game_data_result {
        Some(Ok(result)) => {
            let cache_status = if result.rebuilt_instances == 0 {
                "loaded"
            } else {
                "rebuilt"
            }
            .to_string();
            let cache_detail = Some(format!(
                "loadedInstances={} rebuiltInstances={} workspaceExcludedInstances={}",
                result.loaded_instances,
                result.rebuilt_instances,
                result.workspace_excluded_instances
            ));
            let fingerprint = format!(
                "workbench-loaded-addons:{}",
                result.loaded_instances + result.rebuilt_instances
            );
            log_loaded_addon_index_diagnostics(&logger, &result);
            logger.log_lazy(|| format!(
                    "externalIndex gameData ready cache_status={} cache_detail={} files={} symbols={} parse_diagnostics={} graph_read_ms={} workspace_root_resolution_ms={} cache_metadata_read_ms={} source_inspection_ms={} index_load_or_build_ms={} layer_rebase_ms={} layer_file_projection_ms={} layer_lookup_projection_ms={} layer_compose_ms={} game_data_total_ms={} elapsed_ms={}",
                    cache_status,
                    cache_detail.as_deref().unwrap_or("<none>"),
                    result.summary.files,
                    result.summary.indexed_symbols,
                    result.summary.parse_diagnostics,
                    result.timings.graph_read.as_millis(),
                    result.timings.workspace_root_resolution.as_millis(),
                    result.timings.cache_metadata_read.as_millis(),
                    result.timings.source_inspection.as_millis(),
                    result.timings.index_load_or_build.as_millis(),
                    result.timings.layer_rebase.as_millis(),
                    result.timings.layer_file_projection.as_millis(),
                    result.timings.layer_lookup_projection.as_millis(),
                    result.timings.layer_compose.as_millis(),
                    result.timings.total.as_millis(),
                    start.elapsed().as_millis()
                ));
            (
                Some(result.index),
                Some(result.summary),
                Some(result.scope_authority),
                Some(cache_status),
                cache_detail,
                Some(fingerprint),
            )
        }
        Some(Err(error)) => {
            logger.log_lazy(|| {
                format!(
                    "externalIndex gameData failed error={} elapsed_ms={}",
                    error,
                    start.elapsed().as_millis()
                )
            });
            error_messages.push(error);
            // A failed live graph acquisition does not erase a provisional
            // offline dependency layer; it remains useful until a live
            // Workbench graph can replace it.
            (None, None, None, None, None, None)
        }
        None => (None, None, None, None, None, None),
    };

    let baseline_workspace_files = match workspace_result {
        Ok((workspace_files, workspace_summary)) => {
            logger.log_lazy(|| format!(
                "externalIndex workspace ready roots={} files={} symbols={} parse_diagnostics={} elapsed_ms={}",
                format_paths(&workspace_roots),
                workspace_summary.files,
                workspace_summary.indexed_symbols,
                workspace_summary.parse_diagnostics,
                start.elapsed().as_millis()
            ));
            workspace_files
        }
        Err(error) => {
            logger.log_lazy(|| {
                format!(
                    "externalIndex workspace failed roots={} error={} elapsed_ms={}",
                    format_paths(&workspace_roots),
                    error,
                    start.elapsed().as_millis()
                )
            });
            error_messages.push(error);
            BTreeMap::new()
        }
    };

    // Merge startup and live workspace files before aggregation, then publish only if no live
    // edit raced the snapshot. Layer composition never runs while the overlay lock is held.
    let (status, summary, workspace_summary, published_game_data_summary) = loop {
        let (live_changes, workspace_generation) = {
            let state = state.lock().unwrap();
            (
                state.workspace_live_changes.clone(),
                state.workspace_generation,
            )
        };
        let mut workspace_files = baseline_workspace_files.clone();
        for (path, replacement) in live_changes {
            match replacement {
                Some(indexed) => {
                    workspace_files.insert(path, indexed);
                }
                None => {
                    workspace_files.remove(&path);
                }
            }
        }
        let (workspace_index, workspace_summary) = workspace_aggregate(&workspace_files);
        let workspace_paths_by_identity = workspace_identity_paths(&workspace_files);

        let mut state = state.lock().unwrap();
        if state.workspace_generation != workspace_generation {
            continue;
        }
        if has_game_data_source {
            state.game_data_index = game_data_index.clone();
            state.game_data_summary = game_data_summary.clone();
            state.game_data_scope_authority = game_data_scope_authority;
            state.cache_status = cache_status.clone();
            state.cache_detail = cache_detail.clone();
            state.fingerprint = fingerprint.clone();
        }
        state.workspace_files = Arc::new(workspace_files);
        state.workspace_index = workspace_index;
        state.workspace_exclusions.clear();
        state.workspace_paths_by_identity = workspace_paths_by_identity;
        state.workspace_summary = workspace_summary;
        state.workspace_live_changes.clear();
        state.workspace_startup_pending = false;
        let current_game_data_summary = state.game_data_summary.clone();
        recompute_summary(&mut state, current_game_data_summary);
        state.generation += 1;
        state.status = if state.workspace_index.is_some() || state.game_data_index.is_some() {
            ExternalIndexStatus::Ready
        } else if error_messages.is_empty() {
            ExternalIndexStatus::Missing
        } else {
            ExternalIndexStatus::Failed
        };
        state.error = (!error_messages.is_empty()).then(|| error_messages.join("; "));
        break (
            state.status,
            state.summary.clone(),
            state.workspace_summary.clone(),
            state.game_data_summary.clone(),
        );
    };

    logger.log_lazy(|| format!(
        "externalIndex layered status={} files={} symbols={} workspace_files={} workspace_symbols={} game_data_files={} game_data_symbols={} game_data_ready_ms={} workspace_ready_ms={} layered_ready_ms={} elapsed_ms={}",
        status.as_str(),
        summary.as_ref().map(|summary| summary.files).unwrap_or(0),
        summary.as_ref().map(|summary| summary.indexed_symbols).unwrap_or(0),
        workspace_summary.files,
        workspace_summary.indexed_symbols,
        published_game_data_summary.as_ref().map(|summary| summary.files).unwrap_or(0),
        published_game_data_summary.as_ref().map(|summary| summary.indexed_symbols).unwrap_or(0),
        game_data_ready_ms,
        workspace_ready_ms,
        start.elapsed().as_millis(),
        start.elapsed().as_millis()
    ));
    logger.diagnostic_lazy("externalIndex.completed", || {
        json!({
            "status": status.as_str(),
            "files": summary.as_ref().map(|summary| summary.files).unwrap_or(0),
            "symbols": summary.as_ref().map(|summary| summary.indexed_symbols).unwrap_or(0),
            "workspaceFiles": workspace_summary.files,
            "gameDataReadyMs": game_data_ready_ms,
            "workspaceReadyMs": workspace_ready_ms,
            "totalMs": start.elapsed().as_millis(),
            "elapsedMs": start.elapsed().as_millis(),
        })
    });
}

fn build_workspace_indexes(
    roots: &[PathBuf],
    logger: &LspLogger,
    external_start: Instant,
) -> Result<
    (
        BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>,
        RuntimeIndexSummary,
    ),
    String,
> {
    let roots = normalize_workspace_roots(roots);
    logger.log_lazy(|| {
        format!(
        "externalIndex workspace roots normalized requested={} unique={} roots={} elapsed_ms={}",
        roots.requested_count,
        roots.paths.len(),
        format_paths(&roots.paths),
        external_start.elapsed().as_millis()
    )
    });

    let mut files = Vec::new();
    for root in &roots.paths {
        if !root.is_dir() {
            return Err(format!(
                "Workspace scripts root does not exist or is not a folder: {}",
                root.display()
            ));
        }
        collect_workspace_script_files(root, &mut files)?;
    }
    files.sort();
    files.dedup();
    logger.log_lazy(|| {
        format!(
            "externalIndex workspace discovered files={} elapsed_ms={}",
            files.len(),
            external_start.elapsed().as_millis()
        )
    });

    let mut indexed_files = BTreeMap::new();
    for file in files {
        let file_start = Instant::now();
        logger.log_lazy(|| {
            format!(
                "externalIndex workspace file start path={} total_elapsed_ms={}",
                file.display(),
                external_start.elapsed().as_millis()
            )
        });
        let bytes = fs::read(&file).map_err(|error| {
            format!("Failed to read workspace file {}: {error}", file.display())
        })?;
        let source = String::from_utf8_lossy(&bytes).into_owned();
        let root = workspace_root_for_file(&roots.paths, &file).unwrap_or_else(|| {
            file.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });
        let indexed = build_workspace_file_index(&root, &file, &source);
        logger.log_lazy(|| format!(
            "externalIndex workspace file indexed path={} bytes={} symbols={} parse_diagnostics={} elapsed_ms={} total_elapsed_ms={}",
            file.display(),
            indexed.bytes,
            indexed.contribution.symbols.len(),
            indexed.parse_diagnostics,
            file_start.elapsed().as_millis(),
            external_start.elapsed().as_millis()
        ));
        indexed_files.insert(file, Arc::new(indexed));
    }

    let summary = workspace_summary_from_files(&indexed_files);
    Ok((indexed_files, summary))
}

struct NormalizedWorkspaceRoots {
    requested_count: usize,
    paths: Vec<PathBuf>,
}

fn normalize_workspace_roots(roots: &[PathBuf]) -> NormalizedWorkspaceRoots {
    let mut paths = Vec::<PathBuf>::new();
    let mut seen = BTreeMap::<String, ()>::new();
    for root in roots {
        let normalized = normalize_workspace_path(root);
        let key = workspace_path_key(&normalized);
        if seen.insert(key, ()).is_none() {
            paths.push(normalized);
        }
    }
    NormalizedWorkspaceRoots {
        requested_count: roots.len(),
        paths,
    }
}

fn build_workspace_file_index(root: &Path, file: &Path, source: &str) -> WorkspaceIndexedFile {
    let parse = parse_source(source);
    let semantic_file = SemanticFile::build(source, &parse);
    // Workspace publication retains only the versioned public projection.
    // `workspace_aggregate` reconstructs a query index from these validated
    // compiler-owned facts when it publishes the workspace generation.
    let contribution = semantic_file.contribution();
    contribution
        .validate()
        .expect("fresh compiler-owned workspace contribution is valid");
    WorkspaceIndexedFile {
        contribution,
        metadata: workspace_source_metadata(root, file),
        bytes: source.len(),
        parse_diagnostics: parse.diagnostics.len(),
    }
}

fn workspace_source_metadata(root: &Path, file: &Path) -> SourceFileMetadata {
    let relative_path = file.strip_prefix(root).unwrap_or(file).to_path_buf();
    SourceFileMetadata {
        kind: SourceKind::Workspace,
        category: source_category_for_path(SourceKind::Workspace, Some(&relative_path)),
        absolute_path: Some(file.to_path_buf()),
        virtual_source: None,
        root_path: Some(root.to_path_buf()),
        relative_path: Some(relative_path),
        priority: SOURCE_PRIORITY_WORKSPACE,
    }
}

fn collect_workspace_script_files(folder: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(folder).map_err(|error| {
        format!(
            "Failed to read workspace folder {}: {error}",
            folder.display()
        )
    })? {
        let entry = entry
            .map_err(|error| format!("Failed to read entry in {}: {error}", folder.display()))?;
        let path = entry.path();
        if workspace_directory_entry_is_physical(&entry)? {
            collect_workspace_script_files(&path, files)?;
        } else if entry
            .file_type()
            .map_err(|error| {
                format!(
                    "Failed to inspect workspace entry {}: {error}",
                    path.display()
                )
            })?
            .is_file()
            && path.extension().is_some_and(|extension| extension == "c")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn workspace_directory_entry_is_physical(entry: &fs::DirEntry) -> Result<bool, String> {
    let path = entry.path();
    let file_type = entry.file_type().map_err(|error| {
        format!(
            "Failed to inspect workspace entry {}: {error}",
            path.display()
        )
    })?;
    if !file_type.is_dir() || file_type.is_symlink() {
        return Ok(false);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "Failed to inspect workspace directory {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn workspace_root_for_file(roots: &[PathBuf], file: &Path) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| file.starts_with(root))
        .max_by_key(|root| root.components().count())
        .cloned()
}

fn workspace_identity_paths(
    files: &BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>,
) -> BTreeMap<String, PathBuf> {
    files
        .keys()
        .filter_map(|path| file_path_identity(path).map(|identity| (identity, path.clone())))
        .collect()
}

fn normalize_workspace_path(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| lexically_normalized_absolute_path(path))
}

fn workspace_sequence_key(path: &Path) -> String {
    workspace_path_key(&lexically_normalized_absolute_path(path))
}

fn lexically_normalized_absolute_path(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current_dir| current_dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn workspace_path_key(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        raw.to_ascii_lowercase()
    } else {
        raw
    }
}

fn workspace_summary_from_files(
    files: &BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>,
) -> RuntimeIndexSummary {
    let mut summary = RuntimeIndexSummary::default();
    for file in files.values() {
        summary.files += 1;
        summary.bytes += file.bytes;
        summary.indexed_symbols += file.contribution.symbols.len();
        summary.parse_diagnostics += file.parse_diagnostics;
    }
    summary
}

fn workspace_aggregate(
    files: &BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>,
) -> (Option<Arc<SymbolIndex>>, RuntimeIndexSummary) {
    let workspace_index = (!files.is_empty()).then(|| {
        let mut index = SymbolIndex::default();
        index
            .add_file_contributions(
                files
                    .values()
                    .map(|file| (&file.contribution, file.metadata.clone())),
            )
            .expect("only validated workspace contributions are retained");
        Arc::new(index)
    });
    (workspace_index, workspace_summary_from_files(files))
}

fn workspace_aggregate_excluding(
    files: &BTreeMap<PathBuf, Arc<WorkspaceIndexedFile>>,
    excluded_path: &Path,
) -> Option<Arc<SymbolIndex>> {
    let retained = files
        .iter()
        .filter(|(path, _)| path.as_path() != excluded_path)
        .map(|(_, file)| (&file.contribution, file.metadata.clone()))
        .collect::<Vec<_>>();
    if retained.is_empty() {
        return None;
    }
    let mut index = SymbolIndex::default();
    index
        .add_file_contributions(retained)
        .expect("only validated workspace contributions are retained");
    Some(Arc::new(index))
}

fn recompute_summary(
    state: &mut ExternalIndexState,
    game_data_summary: Option<RuntimeIndexSummary>,
) {
    let game_data_summary = game_data_summary.as_ref();
    state.summary = if state.workspace_index.is_some() || state.game_data_index.is_some() {
        let mut summary = state.workspace_summary.clone();
        if let Some(game_data_summary) = game_data_summary {
            summary.files += game_data_summary.files;
            summary.bytes += game_data_summary.bytes;
            summary.indexed_symbols += game_data_summary.indexed_symbols;
            summary.parse_diagnostics += game_data_summary.parse_diagnostics;
            summary.lossy_files += game_data_summary.lossy_files;
        }
        Some(summary)
    } else {
        None
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::file_uri_for_path;
    use std::sync::mpsc;
    use std::time::Duration;

    fn wait_for_workspace_generation(handle: &ExternalIndexHandle, generation: u64) {
        for _ in 0..100 {
            if handle.status_summary().generation >= generation {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "workspace generation did not reach {generation}: {:?}",
            handle.status_summary()
        );
    }

    fn wait_for_workspace_file_count(handle: &ExternalIndexHandle, count: usize) {
        for _ in 0..100 {
            if handle.state.lock().unwrap().workspace_files.len() >= count {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "workspace file count did not reach {count}: {:?}",
            handle.status_summary()
        );
    }

    #[test]
    fn index_phases_separate_offline_hydration_from_workbench_reconciliation() {
        assert_eq!(
            index_phase(AddonScopeAuthority::ProjectDependencies),
            "offline"
        );
        assert_eq!(
            index_phase(AddonScopeAuthority::WorkbenchLoaded),
            "workbench-reconciliation"
        );
    }

    #[test]
    fn external_index_publication_wakes_the_runtime_without_polling() {
        let missing_inventory = std::env::temp_dir().join(format!(
            "reforger-missing-addon-inventory-{}",
            std::process::id()
        ));
        let storage = missing_inventory.with_extension("indexes");
        let (sender, receiver) = mpsc::channel();
        let _handle = start_external_index(
            &LspServerOptions {
                external_index_mode: ExternalIndexMode::Loaded,
                addon_source_inventory: Some(missing_inventory),
                addon_index_storage: Some(storage),
                ..LspServerOptions::default()
            },
            LspLogger::new(None, None),
            Some(sender.into()),
        );

        let mut saw_progress = false;
        loop {
            match receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(ServerEvent::ExternalIndexProgress { .. }) => saw_progress = true,
                Ok(ServerEvent::ExternalIndexChanged) => break,
                Ok(_) => {}
                Err(error) => panic!("external index publication did not wake runtime: {error}"),
            }
        }
        assert!(
            saw_progress,
            "index phases must wake the runtime before publication"
        );
    }

    #[test]
    fn failed_workbench_graph_refresh_clears_prior_game_data_without_hiding_workspace_facts() {
        let root = std::env::temp_dir().join(format!(
            "reforger-external-overlay-unavailable-graph-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Workspace.c"), "class WorkspaceOnly {}\n").unwrap();
        let addon = root.join("addon");
        fs::create_dir_all(&addon).unwrap();
        write_fixture_pak(
            &addon.join("data.pak"),
            &[("Feature.c", b"class ExternalFeature {}")],
        );
        let inventory = root.join("graph.json");
        fs::write(
            &inventory,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"External","title":"External","sourceRoot":{}}}]}}"#,
                serde_json::to_string(&addon).unwrap(),
            ),
        )
        .unwrap();
        let missing_inventory = root.join("missing-graph.json");
        let storage = root.join("indexes");
        let (sender, receiver) = mpsc::channel();
        let handle = start_external_index(
            &LspServerOptions {
                addon_source_inventory: Some(inventory),
                external_index_mode: ExternalIndexMode::Loaded,
                addon_index_storage: Some(storage.clone()),
                workspace_scripts: vec![workspace.clone()],
                ..LspServerOptions::default()
            },
            LspLogger::new(None, None),
            Some(sender.into()),
        );

        loop {
            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(ServerEvent::ExternalIndexChanged) => break,
                Ok(_) => {}
                Err(error) => panic!("external index did not publish: {error}"),
            }
        }

        let snapshot = handle.snapshot();
        assert!(
            snapshot.game_data.is_some(),
            "the initial graph must publish game data"
        );
        assert!(
            snapshot.workspace.is_some(),
            "workspace facts remain independently available"
        );

        run_external_index_thread(
            handle.state.clone(),
            Some(missing_inventory),
            Some(storage),
            ExternalIndexMode::Loaded,
            vec![workspace],
            Vec::new(),
            LspLogger::new(None, None),
            None,
            handle.control.clone(),
        );

        let snapshot = handle.snapshot();
        assert!(
            snapshot.game_data.is_none(),
            "the Workbench layer must be unavailable"
        );
        assert!(
            snapshot.workspace.is_some(),
            "workspace facts remain independently available"
        );
        let status = handle.status_summary();
        assert_eq!(status.game_data_files, 0);
        assert_eq!(status.workspace_files, 1);
        assert!(
            status.error.is_some(),
            "the graph failure must remain observable"
        );
        handle.cancel();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delivered_workbench_graph_adds_game_data_after_workspace_startup() {
        let root = std::env::temp_dir().join(format!(
            "reforger-external-overlay-delivered-graph-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = root.join("workspace");
        let addon = root.join("addon");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&addon).unwrap();
        fs::write(workspace.join("Workspace.c"), "class WorkspaceOnly {}\n").unwrap();
        write_fixture_pak(
            &addon.join("data.pak"),
            &[("Feature.c", b"class ExternalFeature {}")],
        );
        let inventory = root.join("graph.json");
        fs::write(
            &inventory,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"External","title":"External","sourceRoot":{}}}]}}"#,
                serde_json::to_string(&addon).unwrap(),
            ),
        )
        .unwrap();
        let storage = root.join("indexes");
        let (sender, receiver) = mpsc::channel();
        let handle = start_external_index(
            &LspServerOptions {
                external_index_mode: ExternalIndexMode::Loaded,
                addon_index_storage: Some(storage),
                workspace_scripts: vec![workspace],
                ..LspServerOptions::default()
            },
            LspLogger::new(None, None),
            Some(sender.clone().into()),
        );
        loop {
            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(ServerEvent::ExternalIndexChanged) => break,
                Ok(_) => {}
                Err(error) => panic!("workspace startup did not publish: {error}"),
            }
        }
        assert!(handle.snapshot().workspace.is_some());
        assert!(handle.snapshot().game_data.is_none());

        handle
            .load_workbench_graph(inventory, LspLogger::new(None, None), Some(sender.into()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(ServerEvent::ExternalIndexChanged) if handle.snapshot().game_data.is_some() => {
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("delivered Workbench graph did not publish: {error}"),
            }
        }
        let snapshot = handle.snapshot();
        assert!(snapshot.workspace.is_some());
        assert!(snapshot.game_data.is_some());
        assert_eq!(handle.status_summary().game_data_files, 1);
        handle.cancel();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn compact_pac_inventory_publishes_symbols_and_virtual_sources_without_loose_files() {
        let root = std::env::temp_dir().join(format!(
            "reforger-external-pac-inventory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let addons = root.join("addons");
        fs::create_dir_all(addons.join("data")).unwrap();
        fs::create_dir_all(addons.join("core")).unwrap();
        fs::write(addons.join("data/ArmaReforger.gproj"), "{}").unwrap();
        write_fixture_pak(
            &addons.join("data/data007.pak"),
            &[("Feature.c", b"class Feature {}")],
        );
        write_fixture_pak(
            &addons.join("core/data.pak"),
            &[("CoreFeature.c", b"class CoreFeature {}")],
        );
        let inventory = root.join("inventory.json");
        fs::write(
            &inventory,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"58D0FB3206B6F859","id":"ArmaReforger","title":"Arma Reforger","sourceRoot":{}}},{{"guid":"5614BBCCBB55ED1C","id":"core","title":"core","sourceRoot":{}}}]}}"#,
                serde_json::to_string(&addons.join("data")).unwrap(),
                serde_json::to_string(&addons.join("core")).unwrap(),
            ),
        ).unwrap();
        let storage = root.join("indexes");
        let (sender, receiver) = mpsc::channel();
        let handle = start_external_index(
            &LspServerOptions {
                addon_source_inventory: Some(inventory),
                external_index_mode: ExternalIndexMode::Loaded,
                addon_index_storage: Some(storage.clone()),
                ..LspServerOptions::default()
            },
            LspLogger::new(None, None),
            Some(sender.into()),
        );
        loop {
            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(ServerEvent::ExternalIndexChanged) => break,
                Ok(_) => {}
                Err(error) => panic!("PAC external index did not publish: {error}"),
            }
        }

        let snapshot = handle.snapshot();
        assert_eq!(snapshot.status, "ready");
        let index = snapshot.game_data.expect("base PAC index");
        assert_eq!(index.files().len(), 2);
        let feature = index.preferred_top_level_symbols_for_name("Feature");
        assert_eq!(feature.len(), 1);
        let metadata = &index.file(feature[0].file_id).unwrap().metadata;
        let virtual_source = metadata.virtual_source.as_ref().expect("PAC locator");
        assert_eq!(
            crate::addon_sources::read_virtual_source(&virtual_source.uri).unwrap(),
            "class Feature {}"
        );
        assert!(!storage.join("scripts").exists());
        handle.cancel();
        let _ = fs::remove_dir_all(root);
    }

    fn write_fixture_pak(path: &Path, entries: &[(&str, &[u8])]) {
        let mut table = vec![0, 4];
        table.extend_from_slice(b"Root");
        table.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let table_length = 1
            + 1
            + 4
            + 4
            + entries
                .iter()
                .map(|(logical, _)| 1 + 1 + logical.len() + 24)
                .sum::<usize>();
        let mut offset = 12 + 8 + 28 + 8 + table_length + 8;
        for (logical, content) in entries {
            table.push(1);
            table.push(logical.len() as u8);
            table.extend_from_slice(logical.as_bytes());
            table.extend_from_slice(&(offset as u32).to_le_bytes());
            table.extend_from_slice(&(content.len() as u32).to_le_bytes());
            table.extend_from_slice(&(content.len() as u32).to_le_bytes());
            table.extend_from_slice(&[0; 8]);
            table.extend_from_slice(&0_u32.to_be_bytes());
            offset += content.len();
        }
        let data_length = entries
            .iter()
            .map(|(_, content)| content.len())
            .sum::<usize>();
        let mut bytes = b"FORM".to_vec();
        bytes.extend_from_slice(
            &((4 + 8 + 28 + 8 + table.len() + 8 + data_length) as u32).to_be_bytes(),
        );
        bytes.extend_from_slice(b"PAC1HEAD");
        bytes.extend_from_slice(&28_u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 28]);
        bytes.extend_from_slice(b"FILE");
        bytes.extend_from_slice(&(table.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&table);
        bytes.extend_from_slice(b"DATA");
        bytes.extend_from_slice(&(data_length as u32).to_be_bytes());
        for (_, content) in entries {
            bytes.extend_from_slice(content);
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn workspace_file_ingestion_uses_compiler_owned_semantic_facts() {
        let root = Path::new("C:/workspace");
        let file = Path::new("C:/workspace/Scripts/Example.c");
        let indexed = build_workspace_file_index(
            root,
            file,
            r#"
class Example : BaseExample
{
    int m_Value;
    void Run(string name);
}
"#,
        );

        assert_eq!(indexed.parse_diagnostics, 0);
        indexed.contribution.validate().unwrap();
        let files = BTreeMap::from([(file.to_path_buf(), Arc::new(indexed.clone()))]);
        let (index, summary) = workspace_aggregate(&files);
        let index = index.expect("a retained contribution produces a workspace index");
        assert_eq!(summary.indexed_symbols, 4);
        assert_eq!(index.files().len(), 1);
        assert_eq!(index.symbols().len(), 4);
        let metadata = &index.files()[0].metadata;
        assert_eq!(metadata.kind, SourceKind::Workspace);
        assert_eq!(metadata.root_path.as_deref(), Some(root));
        assert_eq!(
            metadata.relative_path.as_deref(),
            Some(Path::new("Scripts/Example.c"))
        );
        assert_eq!(
            index
                .symbol(index.classes_by_name("Example")[0])
                .and_then(|symbol| symbol.detail.base_type.as_deref()),
            Some("BaseExample")
        );
        assert_eq!(index.methods_by_owner_name("Example", "Run").len(), 1);
    }

    #[test]
    fn workspace_updates_are_latest_wins_across_deletes_and_path_aliases() {
        let handle = ExternalIndexHandle::missing();
        let path = PathBuf::from("sequence-test/Tracked.c");
        let alias = PathBuf::from("sequence-test/nested/../Tracked.c");

        assert!(handle
            .update_workspace_file(path.clone(), "class First {}".to_string(), 1)
            .unwrap()
            .is_some());
        assert_eq!(handle.delete_workspace_file(&alias, 2), Some(true));
        assert_eq!(
            handle
                .update_workspace_file(path.clone(), "class Stale {}".to_string(), 1)
                .unwrap(),
            None
        );
        assert_eq!(handle.delete_workspace_file(&path, 2), None);

        assert!(handle
            .update_workspace_file(alias, "class Recreated {}".to_string(), 3)
            .unwrap()
            .is_some());
        wait_for_workspace_file_count(&handle, 1);
        let state = handle.state.lock().unwrap();
        assert_eq!(state.workspace_last_sequences.len(), 1);
        assert_eq!(state.workspace_last_sequences.values().next(), Some(&3));
        assert_eq!(state.workspace_files.len(), 1);
        assert_eq!(state.workspace_generation, 1);
    }

    #[test]
    fn document_snapshot_excludes_and_caches_its_workspace_contribution() {
        let handle = ExternalIndexHandle::missing();
        let root = std::env::temp_dir().join("reforger-excluded-workspace-snapshot");
        let current = root.join("Current.c");
        let other = root.join("Other.c");
        handle
            .update_workspace_file(current.clone(), "class Current {}".to_string(), 1)
            .unwrap();
        handle
            .update_workspace_file(other.clone(), "class Other {}".to_string(), 2)
            .unwrap();
        wait_for_workspace_file_count(&handle, 2);
        let current_uri = file_uri_for_path(&current).unwrap();
        let current_uri = if cfg!(windows) {
            let drive_colon = current_uri.rfind(":/").unwrap();
            format!(
                "{}%3A{}",
                &current_uri[..drive_colon],
                &current_uri[drive_colon + 1..]
            )
        } else {
            current_uri
        };

        let full = handle.snapshot();
        assert!(!full.workspace_excludes_document());
        assert_eq!(
            full.workspace
                .as_ref()
                .unwrap()
                .classes_by_name("Current")
                .len(),
            1
        );

        let current_identity = file_uri_path_identity(&current_uri);
        let excluded = handle.snapshot_for_document_identity(current_identity.as_deref());
        assert!(excluded.workspace_excludes_document());
        assert!(
            excluded
                .workspace_exclusion
                .as_ref()
                .unwrap()
                .projected
                .get()
                .is_none(),
            "capturing a request snapshot must not aggregate the exclusion index"
        );
        let excluded_index = excluded.workspace_for_projection().unwrap();
        assert!(
            excluded
                .workspace_exclusion
                .as_ref()
                .unwrap()
                .projected
                .get()
                .is_some(),
            "the background projection boundary builds the lazy exclusion index"
        );
        assert!(excluded_index.classes_by_name("Current").is_empty());
        assert_eq!(excluded_index.classes_by_name("Other").len(), 1);

        let cached = handle.snapshot_for_document_identity(current_identity.as_deref());
        assert!(Arc::ptr_eq(
            excluded.workspace_exclusion.as_ref().unwrap(),
            cached.workspace_exclusion.as_ref().unwrap()
        ));

        let generation_before_replacement = handle.status_summary().generation;
        handle
            .update_workspace_file(other, "class Replacement {}".to_string(), 3)
            .unwrap();
        wait_for_workspace_generation(&handle, generation_before_replacement + 1);
        let refreshed = handle.snapshot_for_document_identity(current_identity.as_deref());
        assert!(!Arc::ptr_eq(
            cached.workspace_exclusion.as_ref().unwrap(),
            refreshed.workspace_exclusion.as_ref().unwrap()
        ));
        let refreshed_index = refreshed.workspace_for_projection().unwrap();
        assert_eq!(refreshed_index.classes_by_name("Replacement").len(), 1);
    }

    #[test]
    fn document_excluded_workspace_cache_is_bounded() {
        let handle = ExternalIndexHandle::missing();
        let root = std::env::temp_dir().join("reforger-bounded-excluded-workspace-snapshots");
        let paths = (0..6)
            .map(|index| root.join(format!("File{index}.c")))
            .collect::<Vec<_>>();
        for (index, path) in paths.iter().enumerate() {
            handle
                .update_workspace_file(
                    path.clone(),
                    format!("class Example{index} {{}}"),
                    index as u64 + 1,
                )
                .unwrap();
        }
        wait_for_workspace_file_count(&handle, 6);
        for path in paths {
            let uri = file_uri_for_path(&path).unwrap();
            let identity = file_uri_path_identity(&uri);
            assert!(handle
                .snapshot_for_document_identity(identity.as_deref())
                .workspace_excludes_document());
        }

        assert_eq!(
            handle.state.lock().unwrap().workspace_exclusions.len(),
            MAX_DOCUMENT_EXCLUDED_WORKSPACE_INDEXES
        );
    }

    #[test]
    fn startup_deletion_tombstone_advances_workspace_generation() {
        let handle = ExternalIndexHandle::missing();
        {
            let mut state = handle.state.lock().unwrap();
            state.workspace_startup_pending = true;
            state.status = ExternalIndexStatus::Building;
        }

        assert_eq!(
            handle.delete_workspace_file(Path::new("startup-deleted.c"), 1),
            Some(true)
        );
        wait_for_workspace_generation(&handle, 1);

        let state = handle.state.lock().unwrap();
        assert_eq!(state.workspace_generation, 1);
        assert_eq!(state.generation, 1);
        assert!(matches!(
            state
                .workspace_live_changes
                .get(&lexically_normalized_absolute_path(Path::new(
                    "startup-deleted.c"
                ))),
            Some(None)
        ));
    }

    #[test]
    fn workspace_discovery_collects_physical_files_and_skips_directory_links() {
        let root = temporary_workspace_test_directory("discovery");
        let nested = root.join("nested");
        let external = root
            .parent()
            .expect("temporary workspace has a parent")
            .join(format!(
                "{}-external",
                root.file_name().unwrap().to_string_lossy()
            ));
        let loop_link = root.join("loop");
        let external_link = root.join("external-link");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&external).unwrap();
        let physical_file = nested.join("Physical.c");
        let external_file = external.join("External.c");
        fs::write(&physical_file, "class Physical {}\n").unwrap();
        fs::write(&external_file, "class External {}\n").unwrap();

        if create_directory_link(&root, &loop_link).is_err()
            || create_directory_link(&external, &external_link).is_err()
        {
            let _ = fs::remove_dir_all(&root);
            let _ = fs::remove_dir_all(&external);
            return;
        }

        let mut files = Vec::new();
        collect_workspace_script_files(&root, &mut files).unwrap();
        files.sort();

        assert_eq!(files, vec![physical_file]);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(external);
    }

    fn temporary_workspace_test_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "reforger_external_overlay_{name}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&path);
        path
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }
}
