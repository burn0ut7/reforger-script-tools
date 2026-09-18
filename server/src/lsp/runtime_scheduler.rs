use super::open_documents::{file_index_for_syntax, DocumentSyntax};
use super::semantic_tokens::{
    LspSemanticTokenProjection, RichSemanticProjectionCache, RichSemanticProjectionCacheContext,
};
use super::{
    completion_debug_markdown, completion_report_for_cached_analysis_with_external_indexes,
    debug_hover_report_for_cached_analysis_with_external_indexes, selected_label_from_debug_report,
    semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled,
    signature_help_debug_markdown, signature_help_report_for_cached_analysis_with_external_indexes,
    BracketColoringMode, ExternalIndexSnapshot, ExternalIndexStatusSummary, FileIndexAnalysis,
    FileIndexAnalysisTimings, LspPosition, DEBUG_COMPLETION_METHOD, DEBUG_HOVER_METHOD,
    FOREGROUND_RUNTIME_WORKERS, MAX_BACKGROUND_RUNTIME_WORKERS,
};
use crate::analysis_runtime::{AnalysisTask, PositionIndex, TaskClass, TaskIdentity};
use crate::lexer::lex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

pub(super) enum ServerEvent {
    TransportClosed,
    Incoming {
        received_at: Instant,
        result: Result<Value, String>,
    },
    RichSemanticTokensReady {
        task: TaskIdentity,
        uri: String,
        revision: u64,
        external_generation: u64,
        external_status: &'static str,
        workspace_excludes_document: bool,
        projection: LspSemanticTokenProjection,
        cache: RichSemanticProjectionCache,
        elapsed_ms: u128,
    },
    RichSemanticTokensSkipped {
        task: TaskIdentity,
        uri: String,
        revision: u64,
        external_generation: u64,
        reason: String,
        elapsed_ms: u128,
    },
    DocumentAnalysisReady {
        task: TaskIdentity,
        analysis: FileIndexAnalysis,
        timings: FileIndexAnalysisTimings,
        elapsed_ms: u128,
    },
    ForegroundDocumentReady {
        task: TaskIdentity,
        positions: PositionIndex,
        syntax: Arc<DocumentSyntax>,
        elapsed_ms: u128,
    },
    ForegroundDocumentSkipped {
        task: TaskIdentity,
        reason: String,
        elapsed_ms: u128,
    },
    DocumentAnalysisSkipped {
        task: TaskIdentity,
        reason: String,
        elapsed_ms: u128,
    },
    DebugRequestReady {
        task: TaskIdentity,
        id: Value,
        method: &'static str,
        uri: String,
        revision: u64,
        details: String,
        result: Value,
        elapsed_ms: u128,
    },
    ExternalIndexProgress {
        phase: String,
    },
    ExternalIndexChanged,
}

#[derive(Clone)]
pub(super) enum ServerEventSender {
    Async(mpsc::Sender<ServerEvent>),
    Bounded(mpsc::SyncSender<ServerEvent>),
}

impl ServerEventSender {
    pub(super) fn send(&self, event: ServerEvent) -> Result<(), mpsc::SendError<ServerEvent>> {
        match self {
            Self::Async(sender) => sender.send(event),
            Self::Bounded(sender) => sender.send(event),
        }
    }
}

impl From<mpsc::Sender<ServerEvent>> for ServerEventSender {
    fn from(sender: mpsc::Sender<ServerEvent>) -> Self {
        Self::Async(sender)
    }
}

impl From<mpsc::SyncSender<ServerEvent>> for ServerEventSender {
    fn from(sender: mpsc::SyncSender<ServerEvent>) -> Self {
        Self::Bounded(sender)
    }
}

pub(super) struct RichSemanticTokensJob {
    pub(super) task: AnalysisTask,
    pub(super) uri: String,
    pub(super) revision: u64,
    pub(super) external_generation: u64,
    pub(super) scheduled_at: Instant,
    pub(super) analysis: FileIndexAnalysis,
    pub(super) external_snapshot: ExternalIndexSnapshot,
    pub(super) bracket_coloring: BracketColoringMode,
    pub(super) previous_cache: Option<Arc<RichSemanticProjectionCache>>,
}

pub(super) enum DebugRequestJob {
    Hover(DebugHoverJob),
    Completion(DebugCompletionJob),
}

pub(super) struct DebugHoverJob {
    pub(super) task: AnalysisTask,
    pub(super) id: Value,
    pub(super) uri: String,
    pub(super) position: LspPosition,
    pub(super) revision: u64,
    pub(super) scheduled_at: Instant,
    pub(super) analysis: FileIndexAnalysis,
    pub(super) external_snapshot: ExternalIndexSnapshot,
    pub(super) external_status: ExternalIndexStatusSummary,
}

pub(super) struct DebugCompletionJob {
    pub(super) task: AnalysisTask,
    pub(super) id: Value,
    pub(super) uri: String,
    pub(super) position: LspPosition,
    pub(super) revision: u64,
    pub(super) scheduled_at: Instant,
    pub(super) analysis: FileIndexAnalysis,
    pub(super) external_snapshot: ExternalIndexSnapshot,
}

impl DebugRequestJob {
    pub(super) fn task(&self) -> &AnalysisTask {
        match self {
            Self::Hover(job) => &job.task,
            Self::Completion(job) => &job.task,
        }
    }

    pub(super) fn execute(self) -> ServerEvent {
        match self {
            Self::Hover(job) => {
                let report = debug_hover_report_for_cached_analysis_with_external_indexes(
                    job.task.snapshot().text(),
                    &job.analysis,
                    &job.uri,
                    job.position,
                    job.external_snapshot.workspace.as_deref(),
                    job.external_snapshot.game_data.as_deref(),
                    Some(&job.external_status),
                );
                let hit = report.contains("Selected Symbol: yes");
                let label = selected_label_from_debug_report(&report)
                    .unwrap_or_else(|| "<none>".to_string());
                ServerEvent::DebugRequestReady {
                    task: job.task.identity().clone(),
                    id: job.id,
                    method: DEBUG_HOVER_METHOD,
                    uri: job.uri,
                    revision: job.revision,
                    details: format!("cached_analysis=true hit={} label={}", hit, label),
                    result: Value::String(report),
                    elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                }
            }
            Self::Completion(job) => {
                let report = completion_report_for_cached_analysis_with_external_indexes(
                    job.task.snapshot().text(),
                    &job.analysis,
                    job.position,
                    job.external_snapshot.workspace.as_deref(),
                    job.external_snapshot.game_data.as_deref(),
                );
                if job.task.is_cancelled() {
                    return ServerEvent::DebugRequestReady {
                        task: job.task.identity().clone(),
                        id: job.id,
                        method: DEBUG_COMPLETION_METHOD,
                        uri: job.uri,
                        revision: job.revision,
                        details: "cancelled-after-completion-report".to_string(),
                        result: Value::Null,
                        elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                    };
                }
                let signature_report =
                    signature_help_report_for_cached_analysis_with_external_indexes(
                        job.task.snapshot().text(),
                        &job.analysis,
                        job.position,
                        job.external_snapshot.workspace.as_deref(),
                        job.external_snapshot.game_data.as_deref(),
                    );
                let completion_context = report.completion_context.clone();
                let candidate_count = report.candidate_count;
                let signature_context = signature_report
                    .context
                    .clone()
                    .unwrap_or_else(|| "none".to_string());
                let signature_candidate_count = signature_report.candidate_count;
                let mut markdown = completion_debug_markdown(
                    &report,
                    &job.uri,
                    job.task.snapshot().text().len(),
                    job.revision,
                    job.external_snapshot.status,
                );
                markdown.push_str(&signature_help_debug_markdown(&signature_report));
                ServerEvent::DebugRequestReady {
                    task: job.task.identity().clone(), id: job.id, method: DEBUG_COMPLETION_METHOD,
                    uri: job.uri, revision: job.revision,
                    details: format!("cached_analysis=true context={} candidates={} signature_context={} signature_candidates={} external_index_status={} external_index_layers={}", completion_context, candidate_count, signature_context, signature_candidate_count, job.external_snapshot.status, job.external_snapshot.available_layers()),
                    result: Value::String(markdown), elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                }
            }
        }
    }
}

pub(super) struct OpenDocumentAnalysisJob {
    pub(super) task: AnalysisTask,
    pub(super) syntax: Arc<DocumentSyntax>,
    pub(super) scheduled_at: Instant,
}

pub(super) struct ForegroundDocumentJob {
    pub(super) task: AnalysisTask,
    pub(super) scheduled_at: Instant,
}

#[derive(Clone)]
pub(super) struct RuntimeWorkExecutor {
    pub(super) state: Arc<(
        Mutex<BTreeMap<(TaskClass, String), RuntimeWorkJob>>,
        Condvar,
    )>,
    pub(super) sender: ServerEventSender,
    #[cfg(test)]
    pub(super) test_before_execute: Option<RuntimeWorkTestHook>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeWorkerLane {
    Foreground,
    /// On a single logical CPU, the sole foreground worker may advance
    /// background convergence only when no foreground work is runnable. This
    /// avoids oversubscribing the CPU while preserving eventual convergence
    /// during an idle period.
    ForegroundWithIdleBackground,
    Background,
}

impl RuntimeWorkerLane {
    pub(super) fn accepts(self, class: TaskClass) -> bool {
        match self {
            // This reservation means an edit can build its current lexical and
            // syntax snapshot even when a whole-file background job is busy.
            Self::Foreground | Self::ForegroundWithIdleBackground => class == TaskClass::Foreground,
            Self::Background => class != TaskClass::Foreground,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RuntimeWorkCapacity {
    pub(super) foreground_workers: usize,
    pub(super) background_workers: usize,
}

impl RuntimeWorkCapacity {
    pub(super) fn for_logical_cpus(logical_cpus: usize) -> Self {
        // A process may fail to discover CPU capacity; treating that as one
        // CPU is the safe choice because it never starts competing background
        // CPU work beside foreground edits.
        let logical_cpus = logical_cpus.max(1);
        Self {
            foreground_workers: FOREGROUND_RUNTIME_WORKERS,
            background_workers: logical_cpus
                .saturating_sub(FOREGROUND_RUNTIME_WORKERS)
                .min(MAX_BACKGROUND_RUNTIME_WORKERS),
        }
    }

    pub(super) fn available() -> Self {
        let logical_cpus = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        Self::for_logical_cpus(logical_cpus)
    }

    pub(super) fn foreground_lane(self) -> RuntimeWorkerLane {
        if self.background_workers == 0 {
            RuntimeWorkerLane::ForegroundWithIdleBackground
        } else {
            RuntimeWorkerLane::Foreground
        }
    }
}

#[cfg(test)]
pub(super) type RuntimeWorkTestHook = Arc<dyn Fn(TaskClass) + Send + Sync>;

pub(super) enum RuntimeWorkJob {
    Foreground(ForegroundDocumentJob),
    Semantic(OpenDocumentAnalysisJob),
    Rich(RichSemanticTokensJob),
    Debug(DebugRequestJob),
}

impl RuntimeWorkJob {
    pub(super) fn task(&self) -> &AnalysisTask {
        match self {
            Self::Foreground(job) => &job.task,
            Self::Semantic(job) => &job.task,
            Self::Rich(job) => &job.task,
            Self::Debug(job) => job.task(),
        }
    }
}

impl RuntimeWorkExecutor {
    pub(super) fn start(sender: impl Into<ServerEventSender>) -> Self {
        Self::start_with_capacity(sender, RuntimeWorkCapacity::available())
    }

    pub(super) fn start_with_capacity(
        sender: impl Into<ServerEventSender>,
        capacity: RuntimeWorkCapacity,
    ) -> Self {
        let scheduler = Self {
            state: Arc::new((Mutex::new(BTreeMap::new()), Condvar::new())),
            sender: sender.into(),
            #[cfg(test)]
            test_before_execute: None,
        };
        for _ in 0..capacity.foreground_workers {
            let worker = scheduler.clone();
            let lane = capacity.foreground_lane();
            thread::spawn(move || worker.run(lane));
        }
        for _ in 0..capacity.background_workers {
            let worker = scheduler.clone();
            thread::spawn(move || worker.run(RuntimeWorkerLane::Background));
        }
        scheduler
    }

    #[cfg(test)]
    pub(super) fn start_with_capacity_and_test_hook(
        sender: impl Into<ServerEventSender>,
        capacity: RuntimeWorkCapacity,
        test_before_execute: RuntimeWorkTestHook,
    ) -> Self {
        let scheduler = Self {
            state: Arc::new((Mutex::new(BTreeMap::new()), Condvar::new())),
            sender: sender.into(),
            test_before_execute: Some(test_before_execute),
        };
        for _ in 0..capacity.foreground_workers {
            let worker = scheduler.clone();
            let lane = capacity.foreground_lane();
            thread::spawn(move || worker.run(lane));
        }
        for _ in 0..capacity.background_workers {
            let worker = scheduler.clone();
            thread::spawn(move || worker.run(RuntimeWorkerLane::Background));
        }
        scheduler
    }

    pub(super) fn schedule(&self, job: OpenDocumentAnalysisJob) {
        self.schedule_work(RuntimeWorkJob::Semantic(job));
    }

    pub(super) fn schedule_foreground(&self, job: ForegroundDocumentJob) {
        self.schedule_work(RuntimeWorkJob::Foreground(job));
    }

    pub(super) fn schedule_rich(&self, job: RichSemanticTokensJob) {
        self.schedule_work(RuntimeWorkJob::Rich(job));
    }

    pub(super) fn schedule_debug(&self, job: DebugRequestJob) {
        self.schedule_work(RuntimeWorkJob::Debug(job));
    }

    pub(super) fn schedule_work(&self, job: RuntimeWorkJob) {
        let (lock, wake) = &*self.state;
        let mut pending = lock.lock().unwrap();
        let key = (
            job.task().identity().class(),
            job.task().identity().uri().to_string(),
        );
        // AnalysisRuntime already owns replacement, cancellation, and
        // retained-work capacity. Replacing a queued key only removes a task
        // that the runtime has already made ineligible for publication.
        pending.insert(key, job);
        // Workers serve disjoint lanes. A single wake-up can choose the wrong
        // idle lane, so every newly admitted job wakes both fixed workers.
        wake.notify_all();
    }

    pub(super) fn run(self, lane: RuntimeWorkerLane) {
        let (lock, wake) = &*self.state;
        loop {
            let mut pending = lock.lock().unwrap();
            let key = loop {
                if let Some(key) = next_runnable_work_key_for_lane(&pending, lane) {
                    break key;
                }
                pending = wake.wait(pending).unwrap();
            };
            let Some(job) = pending.remove(&key) else {
                continue;
            };
            drop(pending);
            if job.task().is_cancelled() {
                self.send_skipped(job, "cancelled-before-dispatch");
                continue;
            }
            self.execute(job);
        }
    }

    pub(super) fn execute(&self, job: RuntimeWorkJob) {
        #[cfg(test)]
        if let Some(test_before_execute) = &self.test_before_execute {
            test_before_execute(job.task().identity().class());
        }
        match job {
            RuntimeWorkJob::Foreground(job) => {
                let cancelled = || job.task.is_cancelled();
                let Some(positions) =
                    PositionIndex::new_cancellable(job.task.snapshot().text(), Some(&cancelled))
                else {
                    self.send_skipped(
                        RuntimeWorkJob::Foreground(job),
                        "cancelled-during-position-index",
                    );
                    return;
                };
                if job.task.is_cancelled() {
                    self.send_skipped(RuntimeWorkJob::Foreground(job), "cancelled-before-lexical");
                    return;
                }
                let lexer_tokens = lex(job.task.snapshot().text());
                if job.task.is_cancelled() {
                    self.send_skipped(RuntimeWorkJob::Foreground(job), "cancelled-before-syntax");
                    return;
                }
                let syntax = Arc::new(DocumentSyntax::from_tokens(
                    job.task.snapshot().text(),
                    lexer_tokens,
                ));
                let event = if job.task.is_cancelled() {
                    ServerEvent::ForegroundDocumentSkipped {
                        task: job.task.identity().clone(),
                        reason: "cancelled-during-syntax".to_string(),
                        elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                    }
                } else {
                    ServerEvent::ForegroundDocumentReady {
                        task: job.task.identity().clone(),
                        positions,
                        syntax,
                        elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                    }
                };
                let _ = self.sender.send(event);
            }
            RuntimeWorkJob::Semantic(job) => {
                let (analysis, timings) =
                    file_index_for_syntax(job.task.snapshot().text(), job.syntax);
                let event = if job.task.is_cancelled() {
                    ServerEvent::DocumentAnalysisSkipped {
                        task: job.task.identity().clone(),
                        reason: "superseded-during-analysis".to_string(),
                        elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                    }
                } else {
                    ServerEvent::DocumentAnalysisReady {
                        task: job.task.identity().clone(),
                        analysis,
                        timings,
                        elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                    }
                };
                let _ = self.sender.send(event);
            }
            RuntimeWorkJob::Rich(job) => {
                let workspace_excludes_document =
                    job.external_snapshot.workspace_excludes_document();
                let workspace = job.external_snapshot.workspace_for_projection();
                let projection =
                    semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled(
                        job.task.snapshot().text(),
                        &job.analysis,
                        workspace.as_deref(),
                        job.external_snapshot.game_data.as_deref(),
                        job.bracket_coloring,
                        RichSemanticProjectionCacheContext::new(
                            job.revision,
                            job.external_generation,
                            job.previous_cache.as_deref(),
                        ),
                        &|| job.task.is_cancelled(),
                    );
                let event = match projection {
                    Some(projection) => ServerEvent::RichSemanticTokensReady {
                        task: job.task.identity().clone(),
                        uri: job.uri,
                        revision: job.revision,
                        external_generation: job.external_generation,
                        external_status: job.external_snapshot.status,
                        workspace_excludes_document,
                        projection: projection.projection,
                        cache: projection.cache,
                        elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                    },
                    None => ServerEvent::RichSemanticTokensSkipped {
                        task: job.task.identity().clone(),
                        uri: job.uri,
                        revision: job.revision,
                        external_generation: job.external_generation,
                        reason: "cancelled-during-work".to_string(),
                        elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                    },
                };
                let _ = self.sender.send(event);
            }
            RuntimeWorkJob::Debug(job) => {
                if job.task().is_cancelled() {
                    self.send_skipped(RuntimeWorkJob::Debug(job), "cancelled-before-work");
                    return;
                }
                let event = job.execute();
                let _ = self.sender.send(event);
            }
        }
    }

    pub(super) fn send_skipped(&self, job: RuntimeWorkJob, reason: &str) {
        let event = match job {
            RuntimeWorkJob::Foreground(job) => ServerEvent::ForegroundDocumentSkipped {
                task: job.task.identity().clone(),
                reason: reason.to_string(),
                elapsed_ms: job.scheduled_at.elapsed().as_millis(),
            },
            RuntimeWorkJob::Semantic(job) => ServerEvent::DocumentAnalysisSkipped {
                task: job.task.identity().clone(),
                reason: reason.to_string(),
                elapsed_ms: job.scheduled_at.elapsed().as_millis(),
            },
            RuntimeWorkJob::Rich(job) => ServerEvent::RichSemanticTokensSkipped {
                task: job.task.identity().clone(),
                uri: job.uri,
                revision: job.revision,
                external_generation: job.external_generation,
                reason: reason.to_string(),
                elapsed_ms: job.scheduled_at.elapsed().as_millis(),
            },
            RuntimeWorkJob::Debug(job) => match job {
                DebugRequestJob::Hover(job) => ServerEvent::DebugRequestReady {
                    task: job.task.identity().clone(),
                    id: job.id,
                    method: DEBUG_HOVER_METHOD,
                    uri: job.uri,
                    revision: job.revision,
                    details: format!("skipped reason={reason}"),
                    result: Value::Null,
                    elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                },
                DebugRequestJob::Completion(job) => ServerEvent::DebugRequestReady {
                    task: job.task.identity().clone(),
                    id: job.id,
                    method: DEBUG_COMPLETION_METHOD,
                    uri: job.uri,
                    revision: job.revision,
                    details: format!("skipped reason={reason}"),
                    result: Value::Null,
                    elapsed_ms: job.scheduled_at.elapsed().as_millis(),
                },
            },
        };
        let _ = self.sender.send(event);
    }
}

#[cfg(test)]
pub(super) fn next_runnable_work_key(
    pending: &BTreeMap<(TaskClass, String), RuntimeWorkJob>,
) -> Option<(TaskClass, String)> {
    next_runnable_work_key_for_lane(pending, RuntimeWorkerLane::Background)
}

pub(super) fn next_runnable_work_key_for_lane(
    pending: &BTreeMap<(TaskClass, String), RuntimeWorkJob>,
    lane: RuntimeWorkerLane,
) -> Option<(TaskClass, String)> {
    let idle_background = lane == RuntimeWorkerLane::ForegroundWithIdleBackground
        && !pending
            .iter()
            .any(|((class, _), _)| *class == TaskClass::Foreground);
    pending
        .iter()
        .filter(|((class, _), _)| {
            lane.accepts(*class) || (idle_background && *class != TaskClass::Foreground)
        })
        .min_by_key(|((class, uri), _)| (*class, uri.as_str()))
        .map(|(key, _)| key.clone())
}
