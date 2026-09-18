use super::runtime_scheduler::{
    next_runnable_work_key, next_runnable_work_key_for_lane, RuntimeWorkCapacity,
    RuntimeWorkExecutor, RuntimeWorkJob, RuntimeWorkerLane,
};
use super::semantic_tokens::LspSemanticTokens;
use super::*;
use crate::analysis_runtime::UpsertOutcome;
use crate::resolver::{CandidateSource, IdentifierContext, ResolutionReason};
use crate::syntax::ParseDiagnostic;
use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

mod support;
use support::{cleanup_log, temp_test_dir, test_log_path, timestamp_millis};

fn rich_semantic_tokens_job(
    runtime: &mut AnalysisRuntime,
    uri: &str,
    version: i32,
    scheduled_at: Instant,
) -> RichSemanticTokensJob {
    assert_eq!(runtime.upsert(uri, version, ""), UpsertOutcome::Accepted);
    let task = match runtime.admit(
        TaskClass::Rich,
        runtime.latest(uri).expect("accepted snapshot"),
        1,
    ) {
        AdmissionDisposition::Enqueued { .. } => runtime.take_next().unwrap(),
        other => panic!("unexpected admission disposition: {other:?}"),
    };
    RichSemanticTokensJob {
        task,
        uri: uri.to_string(),
        revision: 1,
        external_generation: 0,
        scheduled_at,
        analysis: file_index_for_source(""),
        external_snapshot: ExternalIndexSnapshot {
            status: "missing",
            workspace: None,
            game_data: None,
            workspace_exclusion: None,
        },
        bracket_coloring: BracketColoringMode::Semantic,
        previous_cache: None,
    }
}

fn semantic_analysis_job(
    runtime: &mut AnalysisRuntime,
    uri: &str,
    version: i32,
    scheduled_at: Instant,
) -> OpenDocumentAnalysisJob {
    assert_eq!(runtime.upsert(uri, version, ""), UpsertOutcome::Accepted);
    let task = match runtime.admit(
        TaskClass::Semantic,
        runtime.latest(uri).expect("accepted snapshot"),
        1,
    ) {
        AdmissionDisposition::Enqueued { .. } => runtime.take_next().unwrap(),
        other => panic!("unexpected admission disposition: {other:?}"),
    };
    let syntax = Arc::new(open_documents::DocumentSyntax::new(task.snapshot().text()));
    OpenDocumentAnalysisJob {
        task,
        syntax,
        scheduled_at,
    }
}

fn foreground_document_job(
    runtime: &mut AnalysisRuntime,
    uri: &str,
    version: i32,
    source: &str,
    scheduled_at: Instant,
) -> ForegroundDocumentJob {
    assert_eq!(
        runtime.upsert(uri, version, source),
        UpsertOutcome::Accepted
    );
    let task = match runtime.admit(
        TaskClass::Foreground,
        runtime.latest(uri).expect("accepted snapshot"),
        1,
    ) {
        AdmissionDisposition::Enqueued { .. } => runtime.take_next().unwrap(),
        other => panic!("unexpected admission disposition: {other:?}"),
    };
    ForegroundDocumentJob { task, scheduled_at }
}

fn install_next_foreground(
    server: &mut LspServer<Vec<u8>>,
    receiver: &mpsc::Receiver<ServerEvent>,
) {
    loop {
        let event = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("foreground worker event");
        server.handle_internal_event(event).unwrap();
        if server.document_runtime.test_has_any_foreground_document() {
            return;
        }
    }
}

include!("tests/protocol.rs");
include!("tests/documents.rs");
include!("tests/runtime.rs");
include!("tests/features.rs");
