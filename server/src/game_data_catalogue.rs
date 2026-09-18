use crate::addon_sources::{
    load_all_cached_addon_indexes, load_cached_dependency_addon_indexes,
    read_cached_combined_addon_indexes, read_cached_loaded_addon_indexes,
    read_cached_virtual_source, read_cached_virtual_sources, LoadedAddonIndexInstance,
    LoadedAddonIndexResult, BASE_GAME_GUID, ENFUSION_CORE_GUID,
};
use crate::game_data_inspection::{
    inspect, read_source as read_source_evidence, GameDataInspectionError,
    GameDataInspectionOutput, GameDataSourceReadRequest,
};
use crate::game_data_intent::{
    research_game_data, GameDataIntentError, GameDataIntentRequest, GameDataIntentResult,
};
use crate::game_data_research::{
    list_members, GameDataExamplePage, GameDataExampleSearchRequest, GameDataMemberPage,
    GameDataMemberRequest, GameDataRelationshipPage, GameDataRelationshipRequest,
    GameDataResearchError,
};
use crate::game_data_search::{
    search_scoped, GameDataAddonIdentity, GameDataAddonMap, GameDataSearchError,
    GameDataSearchPage, GameDataSearchRequest, SourceLineStarts,
};
use crate::index::{SourceFileId, SymbolIndex};
use crate::index_build::{IndexBuildControl, INDEX_BUILD_CANCELLED};
#[cfg(test)]
use crate::index_cache::IndexCacheTimings;
use crate::index_cache::{RuntimeIndexSummary, SourceFingerprint};
use crate::model::SymbolKind;
use crate::source_relationships::{
    SourceAuthority, SourceRelationshipQuery, SourceRelationshipSnapshot,
};
use crate::text_search::{
    page as page_text, physical_source_uri, scan as scan_text, TextSearchCorpus, TextSearchError,
    TextSearchOptions, TextSearchPage, TextSearchRequest, TextSearchResultSet, TextSource,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

pub const GAME_DATA_INITIALIZATION_DEADLINE_MS: u64 = 120_000;
pub const MAX_STRUCTURED_RESULT_BYTES: usize = 256 * 1024;
const MAX_CACHED_TEXT_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TEXT_SOURCE_READ_WORKERS: usize = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GameDataExternalIndexMode {
    All,
    #[default]
    Loaded,
    None,
}

#[derive(Debug, Clone, Default)]
pub struct GameDataCatalogueConfig {
    pub addon_source_inventory: Option<PathBuf>,
    pub addon_index_storage: Option<PathBuf>,
    pub external_index_mode: GameDataExternalIndexMode,
    pub workspace_roots: Vec<PathBuf>,
    pub dependency_project_files: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct GameDataCatalogue {
    config: GameDataCatalogueConfig,
    state: Mutex<Option<GameDataCatalogueState>>,
    text_search_cache:
        Mutex<BTreeMap<(String, String, TextSearchOptions, Vec<String>), Arc<TextSearchResultSet>>>,
    text_source_cache: Mutex<Option<CachedTextSources>>,
    relationships: SourceRelationshipQuery,
    initialized: AtomicBool,
    #[cfg(all(feature = "test-hooks", debug_assertions))]
    panic_once: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone)]
struct GameDataCatalogueState {
    status: GameDataStatus,
    // Ticket #17 adds semantic queries over this exact immutable index.
    index: Option<Arc<SymbolIndex>>,
    source_line_starts: Arc<BTreeMap<SourceFileId, SourceLineStarts>>,
    addon_map: Arc<GameDataAddonMap>,
    addon_instances: Arc<Vec<LoadedAddonIndexInstance>>,
}

#[derive(Debug)]
struct CachedTextSources {
    revision: String,
    addon_guids: Vec<String>,
    corpus: Arc<TextSearchCorpus>,
}

impl CachedTextSources {
    fn matches(&self, revision: &str, addon_guids: &[String]) -> bool {
        self.revision == revision && self.addon_guids == addon_guids
    }
}

impl GameDataCatalogue {
    pub fn new(config: GameDataCatalogueConfig) -> Self {
        Self {
            config,
            state: Mutex::new(None),
            text_search_cache: Mutex::new(BTreeMap::new()),
            text_source_cache: Mutex::new(None),
            relationships: SourceRelationshipQuery::default(),
            initialized: AtomicBool::new(false),
            #[cfg(all(feature = "test-hooks", debug_assertions))]
            panic_once: std::sync::atomic::AtomicBool::new(
                std::env::var("REFORGER_MCP_TEST_PANIC_ONCE").as_deref() == Ok("1"),
            ),
        }
    }

    pub fn status(&self, control: &IndexBuildControl) -> Result<GameDataStatus, String> {
        control.check()?;
        let mut state = self.lock_state(control)?;
        if let Some(state) = state.as_ref() {
            return Ok(state.status.clone());
        }

        self.before_initialization(control)?;
        let initialized = initialize_catalogue(&self.config, control)?;
        let status = initialized.status.clone();
        *state = Some(initialized);
        self.initialized.store(true, Ordering::Release);
        Ok(status)
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn search(
        &self,
        control: &IndexBuildControl,
        request: GameDataSearchRequest,
    ) -> Result<GameDataSearchPage, GameDataCatalogueSearchError> {
        self.before_operation(control)
            .map_err(GameDataCatalogueSearchError::Initialization)?;
        let status = self
            .status(control)
            .map_err(GameDataCatalogueSearchError::Initialization)?;
        if !status.available {
            return Err(GameDataCatalogueSearchError::Unavailable);
        }
        let state = self
            .lock_state(control)
            .map_err(GameDataCatalogueSearchError::Initialization)?;
        let snapshot = state
            .as_ref()
            .ok_or(GameDataCatalogueSearchError::Unavailable)?;
        let index = snapshot
            .index
            .clone()
            .ok_or(GameDataCatalogueSearchError::Unavailable)?;
        let source_line_starts = snapshot.source_line_starts.clone();
        let addon_map = snapshot.addon_map.clone();
        drop(state);
        search_scoped(
            &index,
            &source_line_starts,
            &addon_map,
            control,
            status
                .catalogue_revision
                .as_deref()
                .ok_or(GameDataCatalogueSearchError::Unavailable)?,
            request,
        )
        .map_err(GameDataCatalogueSearchError::Search)
    }

    pub fn search_text(
        &self,
        control: &IndexBuildControl,
        request: TextSearchRequest,
    ) -> Result<TextSearchPage, GameDataCatalogueTextSearchError> {
        self.before_operation(control)
            .map_err(GameDataCatalogueTextSearchError::Initialization)?;
        let status = self
            .status(control)
            .map_err(GameDataCatalogueTextSearchError::Initialization)?;
        if !status.available {
            return Err(GameDataCatalogueTextSearchError::Unavailable);
        }
        let revision = status
            .catalogue_revision
            .clone()
            .ok_or(GameDataCatalogueTextSearchError::Unavailable)?;
        let state = self
            .lock_state(control)
            .map_err(GameDataCatalogueTextSearchError::Initialization)?;
        let snapshot = state
            .as_ref()
            .ok_or(GameDataCatalogueTextSearchError::Unavailable)?;
        let index = snapshot
            .index
            .clone()
            .ok_or(GameDataCatalogueTextSearchError::Unavailable)?;
        let addon_map = snapshot.addon_map.clone();
        let addon_instances = snapshot.addon_instances.clone();
        drop(state);
        let addon_guids = canonical_catalogue_guids(request.addon_guids.as_deref(), &addon_map)
            .map_err(|message| {
                GameDataCatalogueTextSearchError::TextSearch(TextSearchError::InvalidRequest(
                    message,
                ))
            })?;
        let mut request = request;
        request.addon_guids = Some(addon_guids.clone());
        let cache_key = (
            revision.clone(),
            request.query.clone(),
            request.options,
            addon_guids.clone(),
        );
        if let Some(result_set) = self
            .text_search_cache
            .lock()
            .unwrap()
            .get(&cache_key)
            .cloned()
        {
            return page_text(&result_set, control, request)
                .map_err(GameDataCatalogueTextSearchError::TextSearch);
        }
        let source_read_started = Instant::now();
        let cached_corpus = self
            .text_source_cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|cached| cached.matches(&revision, &addon_guids))
            .map(|cached| cached.corpus.clone());
        let (mut corpus, retain_corpus) = if let Some(cached_corpus) = cached_corpus {
            let mut corpus = cached_corpus.as_ref().clone();
            corpus.source_read_ms = duration_ms(source_read_started.elapsed());
            corpus
                .source_read_ms_by_addon
                .values_mut()
                .for_each(|elapsed| *elapsed = 0);
            (corpus, false)
        } else {
            let mut sources = Vec::new();
            let mut virtual_sources =
                BTreeMap::<String, Vec<(String, String, String, Option<String>)>>::new();
            let mut source_read_failures = 0;
            let mut source_read_failures_by_addon = BTreeMap::<String, usize>::new();
            let mut source_read_time_by_addon = BTreeMap::<String, Duration>::new();
            let mut files_considered = 0;
            for file in index.files() {
                control.check().map_err(|_| {
                    GameDataCatalogueTextSearchError::TextSearch(TextSearchError::Cancelled)
                })?;
                let Some(addon) = addon_map.get(&file.id) else {
                    continue;
                };
                if addon_guids.binary_search(&addon.guid).is_err() {
                    continue;
                }
                files_considered += 1;
                let Some(relative_path) = file
                    .metadata
                    .relative_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                else {
                    source_read_failures += 1;
                    continue;
                };
                if let Some(virtual_source) = &file.metadata.virtual_source {
                    virtual_sources
                        .entry(addon.guid.clone())
                        .or_default()
                        .push((
                            relative_path,
                            virtual_source.uri.clone(),
                            addon.label.clone(),
                            addon.thumbnail_color.clone(),
                        ));
                    continue;
                }
                let read_started = Instant::now();
                let source_uri = file
                    .metadata
                    .absolute_path
                    .as_deref()
                    .and_then(physical_source_uri);
                let source = if let Some(path) = &file.metadata.absolute_path {
                    fs::read(path)
                        .ok()
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                } else {
                    None
                };
                *source_read_time_by_addon
                    .entry(addon.guid.clone())
                    .or_insert(Duration::ZERO) += read_started.elapsed();
                let Some(source) = source else {
                    source_read_failures += 1;
                    *source_read_failures_by_addon
                        .entry(addon.guid.clone())
                        .or_insert(0) += 1;
                    continue;
                };
                sources.push(TextSource {
                    relative_path,
                    addon_guid: Some(addon.guid.clone()),
                    addon_label: Some(addon.label.clone()),
                    thumbnail_color: addon.thumbnail_color.clone(),
                    source_uri,
                    content: Arc::<str>::from(source),
                });
            }
            let virtual_source_jobs = virtual_sources
                .into_iter()
                .enumerate()
                .map(|(sequence, (guid, addon_sources))| {
                    let cache_path = addon_instances
                        .iter()
                        .find(|instance| instance.guid.eq_ignore_ascii_case(&guid))
                        .map(|instance| instance.cache_path.clone())
                        .ok_or(GameDataCatalogueTextSearchError::Unavailable)?;
                    Ok((sequence, guid, addon_sources, cache_path))
                })
                .collect::<Result<Vec<_>, GameDataCatalogueTextSearchError>>()?;
            let worker_count = virtual_source_jobs.len().min(MAX_TEXT_SOURCE_READ_WORKERS);
            let mut partitions = (0..worker_count).map(|_| Vec::new()).collect::<Vec<_>>();
            for (index, job) in virtual_source_jobs.into_iter().enumerate() {
                partitions[index % worker_count].push(job);
            }
            let mut completed_batches = std::thread::scope(|scope| {
                let workers = partitions
                    .into_iter()
                    .map(|partition| {
                        scope.spawn(move || {
                            partition
                                .into_iter()
                                .map(|(sequence, guid, addon_sources, cache_path)| {
                                    let uris = addon_sources
                                        .iter()
                                        .map(|(_, uri, _, _)| uri.clone())
                                        .collect::<Vec<_>>();
                                    let read_started = Instant::now();
                                    let batch =
                                        read_cached_virtual_sources(&uris, &cache_path, control);
                                    (sequence, guid, addon_sources, read_started.elapsed(), batch)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect::<Vec<_>>();
                let mut completed = Vec::new();
                for worker in workers {
                    let batches = worker
                        .join()
                        .map_err(|_| "Game Data text source reader panicked".to_string())?;
                    completed.extend(batches);
                }
                Ok::<_, String>(completed)
            })
            .map_err(GameDataCatalogueTextSearchError::Initialization)?;
            completed_batches.sort_by_key(|(sequence, _, _, _, _)| *sequence);
            for (_, guid, addon_sources, read_elapsed, batch) in completed_batches {
                let batch = batch.map_err(|error| {
                    if error == INDEX_BUILD_CANCELLED {
                        GameDataCatalogueTextSearchError::TextSearch(TextSearchError::Cancelled)
                    } else {
                        GameDataCatalogueTextSearchError::Initialization(error)
                    }
                })?;
                *source_read_time_by_addon
                    .entry(guid.clone())
                    .or_insert(Duration::ZERO) += read_elapsed;
                for ((relative_path, source_uri, addon_label, thumbnail_color), source) in
                    addon_sources.into_iter().zip(batch.sources.into_iter())
                {
                    match source {
                        Ok(source) => sources.push(TextSource {
                            relative_path,
                            addon_guid: Some(guid.clone()),
                            addon_label: Some(addon_label),
                            thumbnail_color,
                            source_uri: Some(source_uri),
                            content: Arc::<str>::from(source),
                        }),
                        Err(_) => {
                            source_read_failures += 1;
                            *source_read_failures_by_addon
                                .entry(guid.clone())
                                .or_insert(0) += 1;
                        }
                    }
                }
            }
            let source_read_ms_by_addon = source_read_time_by_addon
                .into_iter()
                .map(|(guid, elapsed)| (guid, duration_ms(elapsed)))
                .collect();
            let mut corpus = TextSearchCorpus {
                files_considered,
                source_read_ms: 0,
                sources,
                source_read_failures,
                source_read_failures_by_addon,
                source_read_ms_by_addon,
            };
            let retain_corpus = retained_text_source_bytes(&corpus.sources)
                .is_some_and(|bytes| bytes <= MAX_CACHED_TEXT_SOURCE_BYTES);
            if !retain_corpus {
                self.text_source_cache.lock().unwrap().take();
            }
            corpus.source_read_ms = duration_ms(source_read_started.elapsed());
            (corpus, retain_corpus)
        };
        let result_set = scan_text(&mut corpus, control, &revision, &request)
            .map_err(GameDataCatalogueTextSearchError::TextSearch)
            .map(Arc::new)?;
        if retain_corpus {
            // One slot follows the active revision and selected add-on scope.
            // Replacing it keeps retention bounded without an eviction policy.
            *self.text_source_cache.lock().unwrap() = Some(CachedTextSources {
                revision: revision.clone(),
                addon_guids: addon_guids.clone(),
                corpus: Arc::new(corpus),
            });
        }
        let mut cache = self.text_search_cache.lock().unwrap();
        cache.insert(cache_key, result_set.clone());
        while cache.len() > 8 {
            let oldest = cache.keys().next().cloned();
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            }
        }
        drop(cache);
        page_text(&result_set, control, request)
            .map_err(GameDataCatalogueTextSearchError::TextSearch)
    }

    pub fn research_intent(
        &self,
        control: &IndexBuildControl,
        request: GameDataIntentRequest,
    ) -> Result<GameDataIntentResult, GameDataCatalogueResearchError> {
        let (revision, index, starts, addon_map) = self.research_snapshot(control)?;
        research_game_data(&index, &starts, &addon_map, control, &revision, request)
            .map_err(GameDataCatalogueResearchError::Intent)
    }

    pub fn inspect(
        &self,
        control: &IndexBuildControl,
        symbol_ref: String,
    ) -> Result<GameDataInspectionOutput, GameDataInspectionError> {
        self.before_operation(control)
            .map_err(GameDataInspectionError::Initialization)?;
        let status = self
            .status(control)
            .map_err(GameDataInspectionError::Initialization)?;
        let revision = status
            .catalogue_revision
            .as_deref()
            .ok_or(GameDataInspectionError::Unavailable)?;
        let state = self
            .lock_state(control)
            .map_err(GameDataInspectionError::Initialization)?;
        let snapshot = state.as_ref().ok_or(GameDataInspectionError::Unavailable)?;
        let index = snapshot
            .index
            .clone()
            .ok_or(GameDataInspectionError::Unavailable)?;
        let starts = snapshot.source_line_starts.clone();
        let addon_map = snapshot.addon_map.clone();
        drop(state);
        inspect(&index, &starts, &addon_map, control, revision, &symbol_ref)
    }

    pub fn search_examples(
        &self,
        control: &IndexBuildControl,
        request: GameDataExampleSearchRequest,
    ) -> Result<GameDataExamplePage, GameDataCatalogueResearchError> {
        let _ = request;
        self.research_snapshot(control)?;
        Err(GameDataCatalogueResearchError::SourceEvidenceUnavailable)
    }

    pub fn list_members(
        &self,
        control: &IndexBuildControl,
        request: GameDataMemberRequest,
    ) -> Result<GameDataMemberPage, GameDataCatalogueResearchError> {
        let (revision, index, starts, addon_map) = self.research_snapshot(control)?;
        list_members(&index, &starts, &addon_map, control, &revision, request)
            .map_err(GameDataCatalogueResearchError::Research)
    }

    pub fn query_relationships(
        &self,
        control: &IndexBuildControl,
        request: GameDataRelationshipRequest,
    ) -> Result<GameDataRelationshipPage, GameDataCatalogueResearchError> {
        let _ = request;
        let snapshot = self.relationship_snapshot(control)?;
        self.relationships
            .query_restricted_legacy(control, snapshot, request)
            .map_err(GameDataCatalogueResearchError::Research)?
            .ok_or(GameDataCatalogueResearchError::SourceEvidenceUnavailable)
    }

    pub fn read_source(
        &self,
        control: &IndexBuildControl,
        request: GameDataSourceReadRequest,
    ) -> Result<serde_json::Value, GameDataInspectionError> {
        self.before_operation(control)
            .map_err(GameDataInspectionError::Initialization)?;
        let status = self
            .status(control)
            .map_err(GameDataInspectionError::Initialization)?;
        let revision = status
            .catalogue_revision
            .as_deref()
            .ok_or(GameDataInspectionError::Unavailable)?;
        let state = self
            .lock_state(control)
            .map_err(GameDataInspectionError::Initialization)?;
        let snapshot = state.as_ref().ok_or(GameDataInspectionError::Unavailable)?;
        let index = snapshot
            .index
            .clone()
            .ok_or(GameDataInspectionError::Unavailable)?;
        let addon_map = snapshot.addon_map.clone();
        let addon_instances = snapshot.addon_instances.clone();
        let (source_file_id, virtual_source, absolute_path) = index
            .files()
            .iter()
            .find(|file| {
                file.metadata.relative_path.as_ref().is_some_and(|path| {
                    path.to_string_lossy().replace('\\', "/") == request.relative_path
                }) && addon_map
                    .get(&file.id)
                    .map(|identity| identity.guid.as_str())
                    == request.addon_guid.as_deref()
            })
            .map(|file| {
                (
                    file.id,
                    file.metadata.virtual_source.clone(),
                    file.metadata.absolute_path.clone(),
                )
            })
            .ok_or_else(|| {
                GameDataInspectionError::InvalidSource(
                    "relativePath is not in the catalogue".to_string(),
                )
            })?;
        drop(state);
        let source = if let Some(virtual_source) = &virtual_source {
            let cache_path = addon_instances
                .iter()
                .find(|instance| {
                    instance
                        .guid
                        .eq_ignore_ascii_case(&virtual_source.addon_guid)
                })
                .map(|instance| instance.cache_path.as_path())
                .ok_or(GameDataInspectionError::Unavailable)?;
            read_cached_virtual_source(&virtual_source.uri, &cache_path).map_err(|error| {
                GameDataInspectionError::SourceReadFailed(format!(
                    "Failed to read Game Data source {}: {error}",
                    virtual_source.uri
                ))
            })?
        } else if let Some(path) = absolute_path {
            String::from_utf8_lossy(&fs::read(&path).map_err(|error| {
                GameDataInspectionError::SourceReadFailed(format!(
                    "Failed to read Game Data source {}: {error}",
                    path.display()
                ))
            })?)
            .into_owned()
        } else {
            return Err(GameDataInspectionError::SourceEvidenceUnavailable);
        };
        let mut source_texts = BTreeMap::new();
        source_texts.insert(source_file_id, Arc::<str>::from(source));
        read_source_evidence(
            &index,
            &addon_map,
            control,
            revision,
            &source_texts,
            request,
        )
    }

    fn research_snapshot(
        &self,
        control: &IndexBuildControl,
    ) -> Result<
        (
            String,
            Arc<SymbolIndex>,
            Arc<BTreeMap<SourceFileId, SourceLineStarts>>,
            Arc<GameDataAddonMap>,
        ),
        GameDataCatalogueResearchError,
    > {
        self.before_operation(control)
            .map_err(GameDataCatalogueResearchError::Initialization)?;
        let status = self
            .status(control)
            .map_err(GameDataCatalogueResearchError::Initialization)?;
        let revision = status
            .catalogue_revision
            .ok_or(GameDataCatalogueResearchError::Unavailable)?;
        let state = self
            .lock_state(control)
            .map_err(GameDataCatalogueResearchError::Initialization)?;
        let snapshot = state
            .as_ref()
            .ok_or(GameDataCatalogueResearchError::Unavailable)?;
        Ok((
            revision,
            snapshot
                .index
                .clone()
                .ok_or(GameDataCatalogueResearchError::Unavailable)?,
            snapshot.source_line_starts.clone(),
            snapshot.addon_map.clone(),
        ))
    }

    pub(crate) fn relationship_snapshot(
        &self,
        control: &IndexBuildControl,
    ) -> Result<SourceRelationshipSnapshot, GameDataCatalogueResearchError> {
        self.before_operation(control)
            .map_err(GameDataCatalogueResearchError::Initialization)?;
        let status = self
            .status(control)
            .map_err(GameDataCatalogueResearchError::Initialization)?;
        let revision = status
            .catalogue_revision
            .ok_or(GameDataCatalogueResearchError::Unavailable)?;
        let order_authoritative = status.scope_authority.as_deref() == Some("workbench-loaded");
        let state = self
            .lock_state(control)
            .map_err(GameDataCatalogueResearchError::Initialization)?;
        let snapshot = state
            .as_ref()
            .ok_or(GameDataCatalogueResearchError::Unavailable)?;
        Ok(SourceRelationshipSnapshot {
            authority: SourceAuthority::GameData,
            revision,
            index: snapshot
                .index
                .clone()
                .ok_or(GameDataCatalogueResearchError::Unavailable)?,
            starts: snapshot.source_line_starts.clone(),
            addon_map: snapshot.addon_map.clone(),
            addon_order: Arc::new(
                snapshot
                    .addon_instances
                    .iter()
                    .map(|instance| instance.guid.clone())
                    .collect(),
            ),
            addon_order_authoritative: order_authoritative,
        })
    }

    fn lock_state(
        &self,
        control: &IndexBuildControl,
    ) -> Result<MutexGuard<'_, Option<GameDataCatalogueState>>, String> {
        loop {
            control.check()?;
            match self.state.try_lock() {
                Ok(state) => return Ok(state),
                Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    #[cfg(all(feature = "test-hooks", debug_assertions))]
    fn before_initialization(&self, control: &IndexBuildControl) -> Result<(), String> {
        use std::io::Write;
        use std::sync::atomic::Ordering;

        if let Ok(marker) = std::env::var("REFORGER_MCP_TEST_INITIALIZATION_STARTED_MARKER") {
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(marker)
            {
                let _ = writeln!(file, "started");
            }
        }
        let uninterruptible_delay_ms = std::env::var("REFORGER_MCP_TEST_UNINTERRUPTIBLE_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        std::thread::sleep(Duration::from_millis(uninterruptible_delay_ms));
        if self.panic_once.swap(false, Ordering::AcqRel) {
            panic!("intentional MCP initialization test panic");
        }
        let delay_ms = std::env::var("REFORGER_MCP_TEST_INITIALIZATION_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(delay_ms) {
            control.check()?;
            std::thread::sleep(Duration::from_millis(5));
        }
        control.check()
    }

    #[cfg(not(all(feature = "test-hooks", debug_assertions)))]
    fn before_initialization(&self, control: &IndexBuildControl) -> Result<(), String> {
        control.check()
    }

    #[cfg(all(feature = "test-hooks", debug_assertions))]
    fn before_operation(&self, control: &IndexBuildControl) -> Result<(), String> {
        let delay_ms = std::env::var("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(delay_ms) {
            control.check()?;
            std::thread::sleep(Duration::from_millis(5));
        }
        control.check()
    }

    #[cfg(not(all(feature = "test-hooks", debug_assertions)))]
    fn before_operation(&self, control: &IndexBuildControl) -> Result<(), String> {
        control.check()
    }
}

#[derive(Debug)]
pub enum GameDataCatalogueSearchError {
    Initialization(String),
    Unavailable,
    Search(GameDataSearchError),
}

#[derive(Debug)]
pub enum GameDataCatalogueTextSearchError {
    Initialization(String),
    Unavailable,
    TextSearch(TextSearchError),
}

#[derive(Debug)]
pub enum GameDataCatalogueResearchError {
    Initialization(String),
    Unavailable,
    SourceEvidenceUnavailable,
    Intent(GameDataIntentError),
    Research(GameDataResearchError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataStatus {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalogue_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Opaque revision for the exact selectable add-on scope. Copy it from status; do not construct it."
    )]
    pub scope_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Authority that selected the current add-on scope, such as the live Workbench graph or a labelled provisional dependency scope."
    )]
    pub scope_authority: Option<String>,
    #[schemars(
        description = "Loaded add-ons that may be selected with addonGuids in Game Data symbol or text search. Entries with available=false are diagnostic only."
    )]
    pub addons: Vec<GameDataAddonStatus>,
    pub authorities: GameDataAuthorities,
    pub source: GameDataSourceStatus,
    pub coverage: GameDataCoverage,
    pub counts: GameDataCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<GameDataCacheStatus>,
    pub timings_ms: GameDataTimingsMs,
    pub limits: GameDataLimits,
    pub warnings: Vec<GameDataNotice>,
    pub recovery: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataAddonStatus {
    #[schemars(description = "Canonical uppercase GUID used as the public search-scope ID.")]
    pub addon_guid: String,
    pub display_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Alpha-weighted average thumbnail color as a CSS #RRGGBB value.")]
    pub thumbnail_color: Option<String>,
    pub script_count: usize,
    pub available: bool,
    pub pinned: bool,
    pub default_selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataAuthorities {
    pub source_evidence: FactAuthority,
    pub source_metadata: FactAuthority,
    pub semantic_catalogue: FactAuthority,
    pub cache: FactAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FactAuthority {
    Filesystem,
    LanguageEngine,
    EvidenceCatalogue,
    Workbench,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataSourceStatus {
    pub acquisition: GameDataAcquisition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GameDataAcquisition {
    LocalPack,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataCoverage {
    pub files: usize,
    pub bytes: usize,
    pub indexed_symbols: usize,
    pub parse_diagnostics: usize,
    pub lossy_files: usize,
    pub lossless_files: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataCounts {
    pub symbols_by_kind: BTreeMap<String, usize>,
    pub files_by_source_category: BTreeMap<String, usize>,
    pub symbols_by_source_category: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataCacheStatus {
    pub outcome: GameDataCacheOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuild_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GameDataCacheOutcome {
    Loaded,
    Rebuilt,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataTimingsMs {
    pub cache_file_read: u64,
    pub cache_decode: u64,
    pub cache_validate: u64,
    pub map_rebuild: u64,
    pub rebuild: u64,
    pub cache_write: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataLimits {
    pub initialization_deadline_ms: u64,
    pub max_structured_result_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameDataNotice {
    pub code: String,
    pub message: String,
}

fn initialize_catalogue(
    config: &GameDataCatalogueConfig,
    control: &IndexBuildControl,
) -> Result<GameDataCatalogueState, String> {
    control.check()?;
    let started = Instant::now();
    let source = source_status(None);
    if config.external_index_mode == GameDataExternalIndexMode::None {
        return Ok(unavailable_state(
            source,
            started.elapsed(),
            "game_data_external_index_disabled",
            "External Game Data indexing is disabled.",
            "Set reforgerScriptTools.workbench.externalIndexMode to loaded or all, regenerate the MCP configuration, then restart MCP.",
        ));
    }

    if config.external_index_mode == GameDataExternalIndexMode::All {
        let Some(storage) = config.addon_index_storage.as_ref() else {
            return Ok(unavailable_state(
                source,
                started.elapsed(),
                "game_data_addon_scope_not_configured",
                "The parser-owned add-on index storage is not configured.",
                "Regenerate the MCP configuration from the extension.",
            ));
        };
        let loaded = load_all_cached_addon_indexes(storage, &config.workspace_roots, control)?;
        if loaded.loaded_instances == 0 {
            return Ok(unavailable_state(
                source,
                started.elapsed(),
                "game_data_addon_scope_unavailable",
                "No compatible cached add-on indexes are available.",
                "Activate the language server so it indexes external add-ons, then restart MCP.",
            ));
        }
        return Ok(ready_layered_state(loaded));
    }

    if !config.dependency_project_files.is_empty() {
        let Some(storage) = config.addon_index_storage.as_ref() else {
            return Ok(unavailable_state(
                source,
                started.elapsed(),
                "game_data_addon_scope_not_configured",
                "The parser-owned add-on index storage is not configured.",
                "Regenerate the MCP configuration from the extension.",
            ));
        };
        let loaded = if let Some(inventory) = config.addon_source_inventory.as_ref() {
            read_cached_combined_addon_indexes(
                inventory,
                &config.dependency_project_files,
                storage,
                &config.workspace_roots,
                control,
            )?
        } else {
            load_cached_dependency_addon_indexes(
                &config.dependency_project_files,
                storage,
                &config.workspace_roots,
                control,
            )?
        };
        if loaded.loaded_instances == 0 {
            return Ok(unavailable_state(
                source,
                started.elapsed(),
                "game_data_addon_scope_unavailable",
                "No compatible indexed dependencies are available for the opened workspace project.",
                "Activate the language server so it indexes the workspace project dependencies, then restart MCP.",
            ));
        }
        return Ok(ready_layered_state(loaded));
    }

    match (&config.addon_source_inventory, &config.addon_index_storage) {
        (Some(inventory), Some(storage)) => {
            let loaded = read_cached_loaded_addon_indexes(
                inventory,
                storage,
                &config.workspace_roots,
                control,
            )?;
            if loaded.loaded_instances == 0 {
                return Ok(unavailable_state(
                    source,
                    started.elapsed(),
                    "game_data_addon_scope_unavailable",
                    "No compatible indexed add-ons are available in the current Workbench scope.",
                    "Activate the language server so it publishes the loaded add-on indexes, then restart MCP.",
                ));
            }
            return Ok(ready_layered_state(loaded));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Ok(unavailable_state(
                source,
                started.elapsed(),
                "game_data_addon_scope_not_configured",
                "The loaded add-on inventory and index storage must be configured together.",
                "Regenerate the MCP configuration from the extension.",
            ));
        }
        (None, None) => {}
    }
    Ok(unavailable_state(
        source,
        started.elapsed(),
        "game_data_addon_scope_not_configured",
        "The parser-owned add-on scope is not configured.",
        "Regenerate the MCP configuration from the extension.",
    ))
}

fn ready_layered_state(result: LoadedAddonIndexResult) -> GameDataCatalogueState {
    let scope_revision = layered_scope_revision(&result.instances, &result.scope_instances);
    let mut warnings = catalogue_warnings(&result.summary);
    if result.missing_instances > 0 {
        warnings.push(GameDataNotice {
            code: "addon_indexes_missing".to_string(),
            message: format!(
                "{} loaded add-ons do not have a compatible searchable index yet.",
                result.missing_instances
            ),
        });
    }
    let addon_map = addon_map(&result.instances);
    let mut addons = result
        .instances
        .iter()
        .filter(|instance| instance.script_count > 0)
        .map(addon_status)
        .collect::<Vec<_>>();
    addons.extend(
        result
            .unavailable_instances
            .iter()
            .map(|instance| GameDataAddonStatus {
                addon_guid: instance.guid.to_ascii_uppercase(),
                display_id: instance.display_id.clone(),
                title: instance.title.clone(),
                thumbnail_color: None,
                script_count: 0,
                available: false,
                pinned: false,
                default_selected: false,
            }),
    );
    addons.sort_by(|left, right| {
        addon_status_rank(&left.addon_guid)
            .cmp(&addon_status_rank(&right.addon_guid))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.addon_guid.cmp(&right.addon_guid))
    });
    let source_line_starts = result
        .source_line_starts
        .into_iter()
        .map(|(file, starts)| (file, SourceLineStarts::from_cached_starts(starts)))
        .collect();
    let status = GameDataStatus {
        available: !addons.is_empty(),
        catalogue_revision: Some(scope_revision.clone()),
        scope_revision: Some(scope_revision),
        scope_authority: Some(result.scope_authority.as_str().to_string()),
        addons,
        authorities: authorities(),
        source: source_status(None),
        coverage: coverage(&result.summary),
        counts: counts(&result.index),
        cache: None,
        timings_ms: GameDataTimingsMs {
            cache_file_read: duration_ms(result.timings.index_load_or_build),
            total: duration_ms(result.timings.total),
            ..GameDataTimingsMs::default()
        },
        limits: limits(),
        warnings,
        recovery: vec![
            "Activate the language server to refresh the loaded add-on indexes, then restart MCP."
                .to_string(),
        ],
    };
    GameDataCatalogueState {
        status,
        index: Some(result.index),
        source_line_starts: Arc::new(source_line_starts),
        addon_map: Arc::new(addon_map),
        addon_instances: Arc::new(result.instances),
    }
}

fn unavailable_state(
    source: GameDataSourceStatus,
    elapsed: Duration,
    code: &str,
    message: &str,
    recovery: &str,
) -> GameDataCatalogueState {
    GameDataCatalogueState {
        status: GameDataStatus {
            available: false,
            catalogue_revision: None,
            scope_revision: None,
            scope_authority: None,
            addons: Vec::new(),
            authorities: authorities(),
            source,
            coverage: GameDataCoverage::default(),
            counts: GameDataCounts::default(),
            cache: None,
            timings_ms: GameDataTimingsMs {
                total: duration_ms(elapsed),
                ..GameDataTimingsMs::default()
            },
            limits: limits(),
            warnings: vec![GameDataNotice {
                code: code.to_string(),
                message: message.to_string(),
            }],
            recovery: vec![recovery.to_string()],
        },
        index: None,
        source_line_starts: Arc::new(BTreeMap::new()),
        addon_map: Arc::new(BTreeMap::new()),
        addon_instances: Arc::new(Vec::new()),
    }
}

fn catalogue_warnings(summary: &RuntimeIndexSummary) -> Vec<GameDataNotice> {
    let mut warnings = Vec::new();
    if summary.parse_diagnostics > 0 {
        warnings.push(GameDataNotice {
            code: "parse_diagnostics_present".to_string(),
            message: format!(
                "{} parser diagnostics were recorded while building the catalogue.",
                summary.parse_diagnostics
            ),
        });
    }
    if summary.lossy_files > 0 {
        warnings.push(GameDataNotice {
            code: "lossy_files_present".to_string(),
            message: format!(
                "{} source files required lossy UTF-8 decoding.",
                summary.lossy_files
            ),
        });
    }
    warnings
}

fn layered_scope_revision(
    instances: &[LoadedAddonIndexInstance],
    identities: &[crate::addon_sources::LoadedAddonInstanceIdentity],
) -> String {
    let mut digest = Sha256::new();
    for (instance, identity) in instances.iter().zip(identities) {
        digest.update(instance.guid.as_bytes());
        digest.update([0]);
        digest.update(identity.source_root.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(instance.revision.as_bytes());
        if let Some(color) = &instance.thumbnail_color {
            digest.update(color.as_bytes());
        }
        digest.update([0xff]);
    }
    format!("gd2:{:x}", digest.finalize())
}

fn addon_status_rank(guid: &str) -> u8 {
    match guid {
        BASE_GAME_GUID => 0,
        ENFUSION_CORE_GUID => 1,
        _ => 2,
    }
}

fn addon_map(instances: &[LoadedAddonIndexInstance]) -> GameDataAddonMap {
    let mut map = GameDataAddonMap::new();
    for instance in instances {
        let identity = GameDataAddonIdentity {
            guid: instance.guid.to_ascii_uppercase(),
            label: instance.title.clone(),
            thumbnail_color: instance.thumbnail_color.clone(),
        };
        for file in instance.file_start..instance.file_start + instance.file_count {
            map.insert(SourceFileId(file), identity.clone());
        }
    }
    map
}

fn addon_status(instance: &LoadedAddonIndexInstance) -> GameDataAddonStatus {
    let guid = instance.guid.to_ascii_uppercase();
    GameDataAddonStatus {
        addon_guid: guid.clone(),
        display_id: instance.display_id.clone(),
        title: instance.title.clone(),
        thumbnail_color: instance.thumbnail_color.clone(),
        script_count: instance.script_count,
        available: true,
        pinned: matches!(guid.as_str(), BASE_GAME_GUID | ENFUSION_CORE_GUID),
        default_selected: matches!(guid.as_str(), BASE_GAME_GUID | ENFUSION_CORE_GUID),
    }
}

fn canonical_catalogue_guids(
    requested: Option<&[String]>,
    addon_map: &GameDataAddonMap,
) -> Result<Vec<String>, &'static str> {
    let available = addon_map
        .values()
        .map(|identity| identity.guid.clone())
        .collect::<BTreeSet<_>>();
    let Some(requested) = requested else {
        return Ok(available.into_iter().collect());
    };
    if requested.is_empty() {
        return Err("addonGuids must be non-empty when provided");
    }
    let mut selected = BTreeSet::new();
    for value in requested {
        let guid = value.to_ascii_uppercase();
        if guid.len() != 16
            || !guid.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !selected.insert(guid.clone())
            || !available.contains(&guid)
        {
            return Err("addonGuids must contain unique loaded 16-character hexadecimal GUIDs");
        }
    }
    Ok(selected.into_iter().collect())
}

fn source_status(fingerprint: Option<&SourceFingerprint>) -> GameDataSourceStatus {
    let _ = fingerprint;

    GameDataSourceStatus {
        acquisition: GameDataAcquisition::LocalPack,
        branch: None,
        commit_sha: None,
        commit_date: None,
        downloaded_at: None,
    }
}

fn coverage(summary: &RuntimeIndexSummary) -> GameDataCoverage {
    GameDataCoverage {
        files: summary.files,
        bytes: summary.bytes,
        indexed_symbols: summary.indexed_symbols,
        parse_diagnostics: summary.parse_diagnostics,
        lossy_files: summary.lossy_files,
        lossless_files: summary.files.saturating_sub(summary.lossy_files),
    }
}

fn counts(index: &SymbolIndex) -> GameDataCounts {
    let mut symbols_by_kind = BTreeMap::new();
    let mut files_by_source_category = BTreeMap::new();
    let mut symbols_by_source_category = BTreeMap::new();

    for file in index.files() {
        *files_by_source_category
            .entry(file.metadata.category.as_str().to_string())
            .or_insert(0) += 1;
    }
    for symbol in index.symbol_iter() {
        *symbols_by_kind
            .entry(symbol_kind_name(symbol.kind).to_string())
            .or_insert(0) += 1;
        if let Some(file) = index.file(symbol.id.file_id) {
            *symbols_by_source_category
                .entry(file.metadata.category.as_str().to_string())
                .or_insert(0) += 1;
        }
    }

    GameDataCounts {
        symbols_by_kind,
        files_by_source_category,
        symbols_by_source_category,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addon_sources::{load_or_build_loaded_addon_indexes, LoadedAddonInstanceIdentity};
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cached_text_sources_are_revision_and_scope_bound() {
        let cached = CachedTextSources {
            revision: "revision-a".to_string(),
            addon_guids: vec!["1111111111111111".to_string()],
            corpus: Arc::new(TextSearchCorpus::default()),
        };

        assert!(cached.matches("revision-a", &["1111111111111111".to_string()]));
        assert!(!cached.matches("revision-b", &["1111111111111111".to_string()]));
        assert!(!cached.matches("revision-a", &["2222222222222222".to_string()]));
    }

    #[test]
    fn retained_text_source_size_includes_content_and_identity_metadata() {
        let source = TextSource {
            relative_path: "Game/Feature.c".to_string(),
            addon_guid: Some("1111111111111111".to_string()),
            addon_label: Some("Feature".to_string()),
            thumbnail_color: Some("#123456".to_string()),
            source_uri: Some("reforger-pak://example".to_string()),
            content: Arc::from("class Feature {}"),
        };
        let dynamic_bytes = source.relative_path.len()
            + source.addon_guid.as_deref().map_or(0, str::len)
            + source.addon_label.as_deref().map_or(0, str::len)
            + source.thumbnail_color.as_deref().map_or(0, str::len)
            + source.source_uri.as_deref().map_or(0, str::len)
            + source.content.len();

        assert_eq!(
            retained_text_source_bytes(&[source]),
            Some(std::mem::size_of::<TextSource>() + dynamic_bytes)
        );
    }

    #[test]
    fn all_mode_publishes_every_cached_addon_not_only_the_current_graph() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "reforger_game_data_all_scope_{}_{}",
            std::process::id(),
            nonce
        ));
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(first.join("Scripts")).unwrap();
        fs::create_dir_all(second.join("Scripts")).unwrap();
        fs::write(first.join("Scripts/First.c"), "class First {}\n").unwrap();
        fs::write(second.join("Scripts/Second.c"), "class Second {}\n").unwrap();
        let graph = root.join("graph.json");
        let storage = root.join("indexes");
        let addon = |guid: &str, id: &str, source_root: &Path| {
            serde_json::json!({
                "guid": guid,
                "id": id,
                "title": id,
                "sourceRoot": source_root,
            })
        };
        fs::write(
            &graph,
            serde_json::to_vec(&serde_json::json!({
                "schema": "reforger-workbench-loaded-addon-graph-v1",
                "bridgeVersion": "1.52.0",
                "protocolVersion": 1,
                "addons": [
                    addon("1111111111111111", "First", &first),
                    addon("2222222222222222", "Second", &second),
                ],
            }))
            .unwrap(),
        )
        .unwrap();
        load_or_build_loaded_addon_indexes(&graph, &storage, &[], &IndexBuildControl::default())
            .unwrap();

        fs::write(
            &graph,
            serde_json::to_vec(&serde_json::json!({
                "schema": "reforger-workbench-loaded-addon-graph-v1",
                "bridgeVersion": "1.52.0",
                "protocolVersion": 1,
                "addons": [addon("1111111111111111", "First", &first)],
            }))
            .unwrap(),
        )
        .unwrap();

        let loaded_status = GameDataCatalogue::new(GameDataCatalogueConfig {
            addon_source_inventory: Some(graph.clone()),
            addon_index_storage: Some(storage.clone()),
            external_index_mode: GameDataExternalIndexMode::Loaded,
            ..GameDataCatalogueConfig::default()
        })
        .status(&IndexBuildControl::default())
        .unwrap();
        let all_catalogue = GameDataCatalogue::new(GameDataCatalogueConfig {
            addon_source_inventory: Some(graph),
            addon_index_storage: Some(storage),
            external_index_mode: GameDataExternalIndexMode::All,
            ..GameDataCatalogueConfig::default()
        });
        let all_status = all_catalogue.status(&IndexBuildControl::default()).unwrap();

        assert_eq!(loaded_status.addons.len(), 1);
        assert_eq!(all_status.addons.len(), 2);
        assert_eq!(
            all_status.scope_authority.as_deref(),
            Some("cached-instances")
        );
        assert!(
            !all_catalogue
                .relationship_snapshot(&IndexBuildControl::default())
                .unwrap()
                .addon_order_authoritative
        );
        assert!(all_status
            .addons
            .iter()
            .any(|addon| addon.addon_guid == "2222222222222222"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn loaded_mode_combines_opened_project_dependencies_with_the_workbench_graph() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "reforger_game_data_dependency_scope_{}_{}",
            std::process::id(),
            nonce
        ));
        let unrelated = root.join("unrelated");
        let dependency = root.join("dependency");
        let workspace = root.join("workspace");
        for source_root in [&unrelated, &dependency] {
            fs::create_dir_all(source_root.join("Scripts")).unwrap();
        }
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            unrelated.join("Scripts/Unrelated.c"),
            "class Unrelated {}\n",
        )
        .unwrap();
        fs::write(
            dependency.join("Scripts/Dependency.c"),
            "class Dependency {}\n",
        )
        .unwrap();
        let graph = root.join("graph.json");
        let storage = root.join("indexes");
        let addon = |guid: &str, id: &str, source_root: &Path| {
            serde_json::json!({
                "guid": guid,
                "id": id,
                "title": id,
                "sourceRoot": source_root,
            })
        };
        fs::write(
            &graph,
            serde_json::to_vec(&serde_json::json!({
                "schema": "reforger-workbench-loaded-addon-graph-v1",
                "bridgeVersion": "1.52.0",
                "protocolVersion": 1,
                "addons": [
                    addon("1111111111111111", "Unrelated", &unrelated),
                    addon("2222222222222222", "Dependency", &dependency),
                ],
            }))
            .unwrap(),
        )
        .unwrap();
        load_or_build_loaded_addon_indexes(&graph, &storage, &[], &IndexBuildControl::default())
            .unwrap();
        fs::write(
            &graph,
            serde_json::to_vec(&serde_json::json!({
                "schema": "reforger-workbench-loaded-addon-graph-v1",
                "bridgeVersion": "1.52.0",
                "protocolVersion": 1,
                "addons": [addon("1111111111111111", "Unrelated", &unrelated)],
            }))
            .unwrap(),
        )
        .unwrap();
        let project = workspace.join("addon.gproj");
        fs::write(
            &project,
            "GameProject {\n GUID \"AAAAAAAAAAAAAAAA\"\n Dependencies {\n  \"2222222222222222\"\n }\n}",
        )
        .unwrap();

        let status = GameDataCatalogue::new(GameDataCatalogueConfig {
            addon_source_inventory: Some(graph),
            addon_index_storage: Some(storage),
            dependency_project_files: vec![project],
            external_index_mode: GameDataExternalIndexMode::Loaded,
            ..GameDataCatalogueConfig::default()
        })
        .status(&IndexBuildControl::default())
        .unwrap();

        assert_eq!(
            status.scope_authority.as_deref(),
            Some("project-dependencies-and-workbench-loaded")
        );
        assert!(status
            .addons
            .iter()
            .any(|addon| addon.addon_guid == "2222222222222222"));
        assert!(status
            .addons
            .iter()
            .any(|addon| addon.addon_guid == "1111111111111111"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scope_revision_changes_with_exact_instance_root() {
        let mut instance = LoadedAddonIndexInstance {
            guid: BASE_GAME_GUID.to_string(),
            display_id: "ArmaReforger".to_string(),
            title: "Arma Reforger".to_string(),
            thumbnail_color: None,
            pack_count: 1,
            script_count: 1,
            file_start: 0,
            file_count: 1,
            cache_path: PathBuf::from("cache/symbols.bin"),
            revision: "same-content".to_string(),
            cache_status: "loaded".to_string(),
            cache_detail: None,
            summary: RuntimeIndexSummary::default(),
            timings: IndexCacheTimings::default(),
            cache_file_bytes: None,
        };
        let left = LoadedAddonInstanceIdentity {
            guid: BASE_GAME_GUID.to_string(),
            source_root: PathBuf::from("left"),
        };
        let right = LoadedAddonInstanceIdentity {
            guid: BASE_GAME_GUID.to_string(),
            source_root: PathBuf::from("right"),
        };

        let without_thumbnail =
            layered_scope_revision(std::slice::from_ref(&instance), std::slice::from_ref(&left));
        instance.thumbnail_color = Some("#123456".to_string());
        let status = addon_status(&instance);

        assert_eq!(status.thumbnail_color.as_deref(), Some("#123456"));
        assert_ne!(
            without_thumbnail,
            layered_scope_revision(std::slice::from_ref(&instance), std::slice::from_ref(&left)),
        );

        assert_ne!(
            layered_scope_revision(std::slice::from_ref(&instance), &[left]),
            layered_scope_revision(&[instance], &[right]),
        );
    }
}

fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "class",
        SymbolKind::TypeParameter => "typeParameter",
        SymbolKind::Enum => "enum",
        SymbolKind::EnumMember => "enumMember",
        SymbolKind::Typedef => "typedef",
        SymbolKind::Function => "function",
        SymbolKind::GlobalField => "globalField",
        SymbolKind::Field => "field",
        SymbolKind::Method => "method",
        SymbolKind::Constructor => "constructor",
        SymbolKind::Destructor => "destructor",
        SymbolKind::Parameter => "parameter",
        SymbolKind::LocalVariable => "localVariable",
        SymbolKind::PreprocessorMacro => "preprocessorMacro",
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn retained_text_source_bytes(sources: &[TextSource]) -> Option<usize> {
    sources.iter().try_fold(0_usize, |total, source| {
        [
            std::mem::size_of::<TextSource>(),
            source.relative_path.len(),
            source.addon_guid.as_deref().map_or(0, str::len),
            source.addon_label.as_deref().map_or(0, str::len),
            source.thumbnail_color.as_deref().map_or(0, str::len),
            source.source_uri.as_deref().map_or(0, str::len),
            source.content.len(),
        ]
        .into_iter()
        .try_fold(total, usize::checked_add)
    })
}

fn limits() -> GameDataLimits {
    GameDataLimits {
        initialization_deadline_ms: GAME_DATA_INITIALIZATION_DEADLINE_MS,
        max_structured_result_bytes: MAX_STRUCTURED_RESULT_BYTES,
    }
}

fn authorities() -> GameDataAuthorities {
    GameDataAuthorities {
        source_evidence: FactAuthority::EvidenceCatalogue,
        source_metadata: FactAuthority::Filesystem,
        semantic_catalogue: FactAuthority::LanguageEngine,
        cache: FactAuthority::Filesystem,
    }
}
