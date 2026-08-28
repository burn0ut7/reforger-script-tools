//! Language Server Protocol adapter.
//!
//! This module owns LSP transport, document lifecycle, and LSP feature
//! projection. Shared Enfusion analysis remains in the sibling root modules.

use crate::analysis_runtime::{
    AdmissionDisposition, AnalysisTask, PositionIndex, QueryQuality, TaskClass,
};
#[cfg(test)]
use crate::analysis_runtime::{AdmissionLimits, AnalysisRuntime};
use crate::index::{GlobalSymbolId, SymbolIndex};
use crate::index_query::IndexQuery;
use crate::lexer::{lex, Keyword, TextSpan, Token, TokenKind};
use crate::model::SymbolKind;
use crate::parser::parse_source;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::io::Read;
use std::io::{self, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc;
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::thread;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

mod background_events;
mod collection_declaration;
mod completion;
mod debug_hover;
mod definition;
mod diagnostics;
mod document_query;
mod document_runtime;
mod external_indexes;
mod external_overlay;
mod feature_dispatch;
mod file_identity;
mod hover;
mod hover_render;
mod incoming_scheduler;
mod logging;
mod on_type_formatting;
mod open_documents;
mod preview_context;
mod request_router;
mod response_writer;
mod runtime_scheduler;
mod scope_delimiters;
mod semantic_tokens;
mod signature_help;
mod transport;
mod workspace_requests;

use background_events::interpret_background_event;
use completion::{
    completion_debug_markdown, completion_report_for_cached_analysis_with_external_indexes,
    completion_report_for_current_argument_labels_at_offset_with_external_indexes,
    completion_report_for_current_contextual_constructor_at_offset_with_external_indexes,
    completion_report_for_current_incomplete_callable_parameter_type_at_offset_with_external_indexes,
    completion_report_for_current_local_scope_at_offset_with_external_indexes,
    completion_report_for_current_override_at_offset_with_external_indexes,
    completion_report_for_current_preprocessor_at_offset_with_external_indexes,
    completion_report_for_current_receiver_at_offset_with_external_indexes,
    completion_report_for_current_super_at_offset_with_external_indexes,
    completion_report_for_lexical_source_at_offset_with_external_indexes,
    completion_report_for_lexical_source_with_external_indexes, empty_completion_list,
};
pub use completion::{
    completion_report_for_cached_analysis_with_external,
    completion_report_for_source_position_with_external, LspCompletionItem,
    LspCompletionItemLabelDetails, LspCompletionList, LspCompletionReport, LspCompletionTimings,
    LspTextEdit,
};
use debug_hover::debug_hover_report_for_cached_analysis_with_external_indexes;
pub use debug_hover::debug_hover_report_for_source_position;
pub(crate) use debug_hover::selected_label_from_debug_report;
pub(crate) use definition::file_uri_for_path;
pub use definition::{
    definition_report_for_cached_analysis_with_external, definition_report_for_source_position,
    definition_report_for_source_position_with_external, LspDefinitionReport, LspLocation,
    LspLocationLink,
};
use definition::{
    definition_report_for_cached_analysis_with_external_indexes,
    definition_report_for_pending_snapshot,
};
use diagnostics::{clear_diagnostics_message, publish_diagnostics_message};
pub use diagnostics::{parser_diagnostics_for_source, LspDiagnostic};
use document_query::{DocumentQuery, DocumentQueryState};
use document_runtime::DocumentRuntime;
pub(crate) use external_overlay::ExternalIndexStatusSummary;
use external_overlay::{start_external_index, ExternalIndexHandle, ExternalIndexSnapshot};
use file_identity::{file_path_identity, file_uri_path_identity};
use hover::{
    hover_report_for_cached_analysis_with_external_indexes, hover_report_for_pending_snapshot,
};
pub use hover::{
    hover_report_for_source_position, hover_report_for_source_position_with_external,
    hover_reports_for_source_positions, hover_reports_for_source_positions_with_external,
    HoverSelectionSource, LspHover, LspHoverReport,
};
use logging::LspLogger;
pub use open_documents::{file_index_for_source, FileIndexAnalysis};
pub(crate) use open_documents::{
    file_index_for_source_with_timings, FileIndexAnalysisTimings, OpenDocument,
    TokenProjectionKind, TokenResultDisposition,
};
use request_router::{classify_request, RequestCommand, RoutedRequest};
use response_writer::RuntimeEffect;
use runtime_scheduler::{
    DebugCompletionJob, DebugHoverJob, DebugRequestJob, RichSemanticTokensJob, ServerEvent,
};
use runtime_scheduler::{
    ForegroundDocumentJob, OpenDocumentAnalysisJob, RuntimeWorkExecutor, ServerEventSender,
};
#[cfg(test)]
use semantic_tokens::LspSemanticTokenProjection;
use semantic_tokens::{
    generic_angle_offsets_for_delimiters, lexical_semantic_tokens_for_source_with_bracket_coloring,
    semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled,
    LspSemanticTokensFull, SEMANTIC_TOKEN_MODIFIERS, SEMANTIC_TOKEN_TYPES,
};
pub use semantic_tokens::{
    semantic_tokens_for_source_with_external, semantic_tokens_report_for_source,
    semantic_tokens_report_for_source_with_bracket_coloring,
    semantic_tokens_report_for_source_with_external, BracketColoringMode, LspSemanticTokenReport,
    LspSemanticTokenTimings, SemanticTokenDebug,
};
use signature_help::{
    signature_help_debug_markdown, signature_help_report_for_cached_analysis_with_external_indexes,
    signature_help_report_for_pending_snapshot,
};
pub use signature_help::{
    signature_help_report_for_source_position, LspParameterInformation, LspSignatureHelp,
    LspSignatureHelpReport, LspSignatureHelpTimings, LspSignatureInformation,
};
use transport::read_message;
use workspace_requests::{
    LoadedAddonGraphParams, WorkspaceFileChangedParams, WorkspaceFileDeletedParams,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExternalIndexMode {
    All,
    #[default]
    Loaded,
    None,
}

const SERVER_NAME: &str = "reforger-language-server";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const SIGNATURE_HELP_TRIGGER_CHARACTERS: &[&str] = &[
    "(", ",", ".", ":", "_", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n",
    "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "A", "B", "C", "D", "E", "F", "G",
    "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z",
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
];
const SIGNATURE_HELP_RETRIGGER_CHARACTERS: &[&str] = SIGNATURE_HELP_TRIGGER_CHARACTERS;
const DEBUG_HOVER_METHOD: &str = "reforger/debugHover";
const DEBUG_COMPLETION_METHOD: &str = "reforger/debugCompletion";
const BLOCK_COMMENT_PAIR_METHOD: &str = "reforger/blockCommentPair";
const CONTROL_HEADER_ENTER_METHOD: &str = "reforger/inputRoute";
const ACTIVE_SCOPE_DELIMITERS_METHOD: &str = "reforger/activeScopeDelimiters";
const PREVIEW_CONTEXT_METHOD: &str = "reforger/previewContext";
const READ_PACK_SOURCE_METHOD: &str = "reforger/readPackSource";
const RANGE_FORMATTING_METHOD: &str = "textDocument/rangeFormatting";
const WORKSPACE_FILE_CHANGED_METHOD: &str = "reforger/workspaceFileChanged";
const WORKSPACE_FILE_DELETED_METHOD: &str = "reforger/workspaceFileDeleted";
const LOADED_ADDON_GRAPH_METHOD: &str = "reforger/loadedAddonGraph";
const INCOMING_EVENT_QUEUE_CAPACITY: usize = 64;
const MAX_PENDING_DOCUMENT_REQUESTS_PER_URI: usize = 32;
// The executor retains a single foreground slot. A second CPU-bearing worker
// exists only when the host actually has another logical CPU available. This
// remains one executor and one admitted-work map, rather than a second
// scheduler.
const FOREGROUND_RUNTIME_WORKERS: usize = 1;
const MAX_BACKGROUND_RUNTIME_WORKERS: usize = 1;
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LspServerOptions {
    pub log_path: Option<PathBuf>,
    /// The Wine prefix hosting Workbench, when the host does not run it
    /// natively and the extension has been pointed at a specific prefix.
    pub workbench_wine_prefix: Option<PathBuf>,
    pub diagnostic_log_path: Option<PathBuf>,
    pub addon_source_inventory: Option<PathBuf>,
    pub addon_index_storage: Option<PathBuf>,
    pub external_index_mode: ExternalIndexMode,
    pub workspace_scripts: Vec<PathBuf>,
    pub dependency_project_files: Vec<PathBuf>,
    pub bracket_coloring: BracketColoringMode,
}

pub fn run_stdio(options: LspServerOptions) -> Result<(), String> {
    let stdout = io::stdout();
    let (event_sender, event_receiver) = mpsc::sync_channel(INCOMING_EVENT_QUEUE_CAPACITY);
    let analysis_scheduler = RuntimeWorkExecutor::start(event_sender.clone());
    let mut server = LspServer::new_with_runtime_senders(
        stdout.lock(),
        options,
        Some(analysis_scheduler),
        Some(event_sender.clone().into()),
    );
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        loop {
            match read_message(&mut reader) {
                Ok(Some(message)) => {
                    if event_sender
                        .send(ServerEvent::Incoming {
                            received_at: Instant::now(),
                            result: Ok(message),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = event_sender.send(ServerEvent::TransportClosed);
                    break;
                }
                Err(error) => {
                    let _ = event_sender.send(ServerEvent::Incoming {
                        received_at: Instant::now(),
                        result: Err(error),
                    });
                    break;
                }
            }
        }
    });
    server.run_message_channels(event_receiver)
}

#[cfg(test)]
pub fn run<R: Read, W: Write>(
    reader: R,
    writer: W,
    options: LspServerOptions,
) -> Result<(), String> {
    let mut server = LspServer::new(writer, options);
    server.run(reader)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDocumentSymbol {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub kind: u32,
    pub range: LspRange,
    pub selection_range: LspRange,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<LspDocumentSymbol>,
    #[serde(skip)]
    pub(crate) repaired_full_range: Option<LspRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentSymbolRangeRepair {
    pub(crate) kind: u32,
    pub(crate) original_range: LspRange,
    pub(crate) selection_range: LspRange,
    pub(crate) repaired_range: LspRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDocumentSymbolReport {
    pub symbols: Vec<LspDocumentSymbol>,
    pub parse_diagnostics: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspMarkupContent {
    pub kind: String,
    pub value: String,
}

impl LspDocumentSymbolReport {
    pub fn total_symbol_count(&self) -> usize {
        document_symbol_count(&self.symbols)
    }
}

struct LspServer<W: Write> {
    writer: W,
    logger: LspLogger,
    external_index: ExternalIndexHandle,
    document_runtime: DocumentRuntime,
    client_initialized: bool,
    pending_external_index_progress: Option<Value>,
    event_sender: Option<ServerEventSender>,
    shutdown_requested: bool,
}

fn source_backed_request_method(method: &str) -> bool {
    // Callable completion rendering depends on the current document's
    // semantic facts.  A bounded foreground query can recover names while
    // analysis is pending, but cannot prove their callable signatures, which
    // would let a plain identifier completion race and hide a snippet.
    matches!(
        method,
        "textDocument/completion"
            | DEBUG_HOVER_METHOD
            | DEBUG_COMPLETION_METHOD
            | PREVIEW_CONTEXT_METHOD
    )
}

fn request_document_uri(params: Option<&Value>) -> Option<String> {
    params?
        .get("textDocument")?
        .get("uri")?
        .as_str()
        .map(str::to_string)
}

fn format_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "<none>".to_string();
    }
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(";")
}

#[derive(Debug, Clone, Deserialize)]
struct RpcMessage {
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidOpenTextDocumentParams {
    text_document: TextDocumentItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct TextDocumentItem {
    uri: String,
    version: i32,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidChangeTextDocumentParams {
    text_document: VersionedTextDocumentIdentifier,
    content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct VersionedTextDocumentIdentifier {
    uri: String,
    version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct TextDocumentContentChangeEvent {
    #[serde(default)]
    range: Option<Value>,
    #[serde(rename = "rangeLength")]
    #[serde(default)]
    range_length: Option<u32>,
    text: String,
}

#[derive(Debug)]
struct CoalescibleDidChange {
    uri: String,
    version: i32,
}

fn coalescible_full_sync_did_change(value: &Value) -> Option<CoalescibleDidChange> {
    let message: RpcMessage = serde_json::from_value(value.clone()).ok()?;
    if message.id.is_some() || message.method.as_deref() != Some("textDocument/didChange") {
        return None;
    }
    let params: DidChangeTextDocumentParams = serde_json::from_value(message.params?).ok()?;
    (params.content_changes.len() == 1
        && params.content_changes[0].range.is_none()
        && params.content_changes[0].range_length.is_none())
    .then_some(CoalescibleDidChange {
        uri: params.text_document.uri,
        version: params.text_document.version,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidCloseTextDocumentParams {
    text_document: TextDocumentIdentifier,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentSymbolParams {
    text_document: TextDocumentIdentifier,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HoverParams {
    text_document: TextDocumentIdentifier,
    position: LspPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompletionParams {
    text_document: TextDocumentIdentifier,
    position: LspPosition,
    #[serde(default)]
    context: Option<CompletionRequestContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompletionRequestContext {
    #[serde(default)]
    trigger_character: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputRouteParams {
    text_document: TextDocumentIdentifier,
    version: i32,
    operation: String,
    #[serde(default)]
    trace: bool,
    selections: Vec<LspRange>,
    options: BlockCommentPairOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockCommentPairParams {
    text_document: TextDocumentIdentifier,
    position: LspPosition,
    version: i32,
    options: BlockCommentPairOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveScopeDelimiterParams {
    text_document: TextDocumentIdentifier,
    version: i32,
    positions: Vec<LspPosition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadPackSourceParams {
    uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockCommentPairOptions {
    tab_size: usize,
    insert_spaces: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeFormattingParams {
    text_document: TextDocumentIdentifier,
    range: LspRange,
    #[serde(rename = "options")]
    _options: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct TextDocumentIdentifier {
    uri: String,
}

impl<W: Write> LspServer<W> {
    fn handle_message(
        &mut self,
        value: Value,
        queue_ms: Option<u128>,
        coalesced_changes: usize,
        superseded_changes: usize,
    ) -> Result<bool, String> {
        let routed = classify_request(value)?;
        self.handle_routed(routed, queue_ms, coalesced_changes, superseded_changes)
    }

    fn handle_routed(
        &mut self,
        routed: RoutedRequest,
        queue_ms: Option<u128>,
        coalesced_changes: usize,
        superseded_changes: usize,
    ) -> Result<bool, String> {
        // Lifecycle has no document-owned state. Keep it at the composition
        // root so the request router never needs transport or shutdown
        // ownership for these commands.
        if matches!(&routed.command, RequestCommand::Lifecycle(_))
            && routed.message.method.is_some()
        {
            return self.handle_lifecycle_command(
                routed,
                queue_ms,
                coalesced_changes,
                superseded_changes,
            );
        }
        if matches!(
            &routed.command,
            RequestCommand::Document(request_router::DocumentCommand::Close(_))
        ) {
            return self.handle_document_close_command(routed);
        }
        if let RequestCommand::WorkspaceIndex(
            request_router::WorkspaceIndexCommand::LoadedAddonGraph(params),
        ) = &routed.command
        {
            if let Some(params) = params {
                self.external_index.load_workbench_graph(
                    PathBuf::from(&params.inventory_path),
                    self.logger.clone(),
                    self.event_sender.clone(),
                )?;
            }
            return Ok(false);
        }
        if matches!(
            &routed.command,
            RequestCommand::Document(
                request_router::DocumentCommand::Open(_)
                    | request_router::DocumentCommand::Change(_)
            )
        ) {
            return self.handle_document_update_command(
                routed,
                queue_ms.unwrap_or(0),
                coalesced_changes,
                superseded_changes,
            );
        }
        let outcome = feature_dispatch::execute_feature_or_workspace_message(
            &mut self.external_index,
            &mut self.document_runtime,
            self.shutdown_requested,
            routed,
            queue_ms,
            coalesced_changes,
            superseded_changes,
            self.logger.operational_enabled(),
            self.logger.diagnostic_enabled(),
        )?;
        for effect in outcome.effects {
            self.deliver_effect(effect)?;
        }
        Ok(outcome.should_exit)
    }

    fn handle_document_close_command(&mut self, routed: RoutedRequest) -> Result<bool, String> {
        if let Some(error) = routed.parameter_error {
            if let Some(id) = routed.message.id {
                self.respond_error(id, -32602, &error)?;
            } else {
                self.log_lazy(|| format!("notification ignored invalid_params method=textDocument/didClose error={error}"));
            }
            return Ok(false);
        }
        let RequestCommand::Document(request_router::DocumentCommand::Close(params)) =
            routed.command
        else {
            unreachable!("close handler receives close commands");
        };
        let params = params
            .ok_or_else(|| "Invalid textDocument/didClose params: missing params".to_string())?;
        for effect in self
            .document_runtime
            .close_document(&params.text_document.uri)
        {
            self.deliver_effect(effect)?;
        }
        Ok(false)
    }

    fn handle_document_update_command(
        &mut self,
        routed: RoutedRequest,
        queue_ms: u128,
        coalesced_changes: usize,
        superseded_changes: usize,
    ) -> Result<bool, String> {
        let method = routed
            .message
            .method
            .as_deref()
            .expect("document command has a method");
        if let Some(error) = routed.parameter_error {
            if let Some(id) = routed.message.id {
                self.respond_error(id, -32602, &error)?;
            } else {
                self.log_lazy(|| {
                    format!("notification ignored invalid_params method={method} error={error}")
                });
            }
            return Ok(false);
        }
        let effects = match routed.command {
            RequestCommand::Document(request_router::DocumentCommand::Open(params)) => {
                self.document_runtime.open_document(
                    params.ok_or_else(|| {
                        "Invalid textDocument/didOpen params: missing params".to_string()
                    })?,
                    queue_ms,
                )?
            }
            RequestCommand::Document(request_router::DocumentCommand::Change(params)) => {
                self.document_runtime.change_document(
                    params.ok_or_else(|| {
                        "Invalid textDocument/didChange params: missing params".to_string()
                    })?,
                    queue_ms,
                    coalesced_changes,
                    superseded_changes,
                )?
            }
            _ => unreachable!("only open and change reach the document update executor"),
        };
        for effect in effects {
            self.deliver_effect(effect)?;
        }
        Ok(false)
    }

    fn handle_lifecycle_command(
        &mut self,
        routed: RoutedRequest,
        queue_ms: Option<u128>,
        coalesced_changes: usize,
        superseded_changes: usize,
    ) -> Result<bool, String> {
        let started_at = Instant::now();
        let queue_ms = queue_ms.unwrap_or(0);
        let method = routed
            .message
            .method
            .as_deref()
            .expect("lifecycle command has a method");
        self.logger.diagnostic_lazy("rpc.received", || {
            json!({
                "method": method,
                "command": "Lifecycle",
                "request": routed.message.id.is_some(),
                "queueMs": queue_ms,
                "coalescedChanges": coalesced_changes,
                "supersededChanges": superseded_changes,
            })
        });
        if self.shutdown_requested && method != "exit" {
            if let Some(id) = routed.message.id {
                self.respond_error(id, -32600, "Server has already received shutdown")?;
            } else {
                self.log_lazy(|| format!("notification ignored after shutdown method={method}"));
            }
            return Ok(false);
        }
        match method {
            "initialize" => {
                self.log("request initialize");
                if let Some(id) = routed.message.id {
                    self.respond(id, json!({
                        "capabilities": {
                            "textDocumentSync": {"openClose": true, "change": 1},
                            "documentSymbolProvider": true,
                            "documentRangeFormattingProvider": true,
                            "hoverProvider": true,
                            "definitionProvider": true,
                            "completionProvider": {"triggerCharacters": [".", "[", "#", " "]},
                            "signatureHelpProvider": {
                                "triggerCharacters": SIGNATURE_HELP_TRIGGER_CHARACTERS,
                                "retriggerCharacters": SIGNATURE_HELP_RETRIGGER_CHARACTERS
                            },
                            "semanticTokensProvider": {
                                "legend": {"tokenTypes": SEMANTIC_TOKEN_TYPES, "tokenModifiers": SEMANTIC_TOKEN_MODIFIERS},
                                "full": true,
                                "range": false
                            }
                        },
                        "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION}
                    }))?;
                }
            }
            "initialized" => {
                self.log("notification initialized");
                self.client_initialized = true;
                if let Some(params) = self.pending_external_index_progress.take() {
                    self.publish_external_index_progress(params)?;
                }
            }
            "shutdown" => {
                self.log("request shutdown");
                self.shutdown_requested = true;
                self.external_index.cancel();
                if let Some(id) = routed.message.id {
                    self.respond(id, Value::Null)?;
                }
            }
            "exit" => {
                self.log("notification exit");
                if !self.shutdown_requested {
                    return Err("LSP exit received before shutdown".to_string());
                }
            }
            _ => unreachable!("only lifecycle methods reach the lifecycle executor"),
        }
        let should_exit = self.shutdown_requested && method == "exit";
        self.logger.diagnostic_lazy(
            "rpc.completed",
            || json!({"method": method, "outcome": if should_exit { "exit" } else { "complete" }, "elapsedMs": started_at.elapsed().as_millis()}),
        );
        Ok(should_exit)
    }

    fn handle_internal_event(&mut self, event: ServerEvent) -> Result<(), String> {
        if let ServerEvent::ExternalIndexProgress { phase } = event {
            self.publish_external_index_progress(json!({ "phase": phase }))?;
            return Ok(());
        }
        if matches!(event, ServerEvent::ExternalIndexChanged) {
            let external_status = self.external_index.status_summary();
            self.publish_external_index_progress(json!({
                "phase": "complete",
                "status": external_status.status,
                "gameDataFiles": external_status.game_data_files,
            }))?;
            for effect in self.document_runtime.observe_semantic_external_generation(
                external_status.generation,
                external_status.status,
                None,
            ) {
                self.deliver_effect(effect)?;
            }
            return Ok(());
        }
        let external_generation = self.external_index.status_summary().generation;
        let document_identity = match &event {
            ServerEvent::DocumentAnalysisReady { task, .. } => self
                .document_runtime
                .document_path_identity(task.uri())
                .map(str::to_owned),
            _ => None,
        };
        let external_indexes = document_identity
            .as_deref()
            .map(|identity| {
                self.external_index
                    .snapshot_for_document_identity(Some(identity))
            })
            .unwrap_or_else(|| self.external_index.snapshot());
        let Some(result) = interpret_background_event(
            &mut self.document_runtime,
            event,
            external_generation,
            external_indexes,
        ) else {
            return Ok(());
        };
        for effect in result? {
            self.deliver_effect(effect)?;
        }
        Ok(())
    }

    fn publish_external_index_progress(&mut self, params: Value) -> Result<(), String> {
        if !self.client_initialized {
            self.pending_external_index_progress = Some(params);
            return Ok(());
        }
        self.deliver_effect(RuntimeEffect::Notification(json!({
            "jsonrpc": "2.0",
            "method": "reforger/externalIndexProgress",
            "params": params,
        })))
    }

    #[cfg(test)]
    fn new(writer: W, options: LspServerOptions) -> Self {
        Self::new_with_runtime_senders(writer, options, None, None)
    }

    fn new_with_runtime_senders(
        writer: W,
        options: LspServerOptions,
        analysis_scheduler: Option<RuntimeWorkExecutor>,
        event_sender: Option<ServerEventSender>,
    ) -> Self {
        let logger = LspLogger::new(
            options.log_path.clone(),
            options.diagnostic_log_path.clone(),
        );
        let operational_logging = logger.operational_enabled();
        let external_index = start_external_index(&options, logger.clone(), event_sender.clone());
        let server = Self {
            writer,
            logger,
            external_index,
            document_runtime: DocumentRuntime::new_with_bracket_coloring(
                analysis_scheduler,
                options.bracket_coloring,
                operational_logging,
            ),
            client_initialized: false,
            pending_external_index_progress: None,
            event_sender,
            shutdown_requested: false,
        };
        server.log_lazy(|| format!(
            "startup server={SERVER_NAME} version={SERVER_VERSION} addon_source_inventory={} addon_index_storage={} external_index_mode={:?} workspace_roots={} dependency_projects={} bracket_coloring={:?} external_index_status={}",
            options
                .addon_source_inventory
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unset>".to_string()),
            options
                .addon_index_storage
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unset>".to_string()),
            options.external_index_mode,
            format_paths(&options.workspace_scripts),
            options.dependency_project_files.len(),
            options.bracket_coloring,
            server.external_index.status_summary().status
        ));
        server.logger.diagnostic_lazy("startup", || {
            json!({
                "gameDataConfigured": options.addon_source_inventory.is_some()
                    || !options.dependency_project_files.is_empty(),
                "workspaceRoots": options.workspace_scripts.len(),
                "dependencyProjects": options.dependency_project_files.len(),
                "indexCacheConfigured": options.addon_index_storage.is_some(),
                "externalIndexMode": format!("{:?}", options.external_index_mode),
                "bracketColoring": format!("{:?}", options.bracket_coloring),
            })
        });
        server
    }

    #[cfg(test)]
    fn run<R: Read>(&mut self, reader: R) -> Result<(), String> {
        let mut reader = BufReader::new(reader);
        while let Some(message) = read_message(&mut reader)? {
            let should_exit = self.handle_message(message, None, 0, 0)?;
            if should_exit {
                break;
            }
        }
        self.log("exit");
        self.logger
            .diagnostic("shutdown", json!({"outcome": "normal"}));
        self.logger.flush_diagnostics();
        Ok(())
    }

    fn log(&self, message: &str) {
        self.logger.log(message);
    }

    fn log_lazy(&self, message: impl FnOnce() -> String) {
        self.logger.log_lazy(message);
    }
}

pub fn document_symbols_for_source(source: &str) -> Vec<LspDocumentSymbol> {
    document_symbol_report_for_source(source).symbols
}

/// Returns only declarations whose identity is proven by the current lexer
/// snapshot. This is the pending-analysis outline contract: it is exact for
/// the returned top-level class, enum, and typedef names, but deliberately
/// omits members and declarations that need syntax recovery or semantic facts.
/// It must never reuse an earlier revision's cached projection.
fn lexical_document_symbols_for_snapshot(
    snapshot: &crate::analysis_runtime::DocumentSnapshot,
) -> Vec<LspDocumentSymbol> {
    snapshot.positions().map_or_else(Vec::new, |positions| {
        lexical_document_symbols(snapshot.text(), &positions)
    })
}

fn lexical_document_symbols(source: &str, positions: &PositionIndex) -> Vec<LspDocumentSymbol> {
    let tokens = lex(source);
    let mut symbols = Vec::new();
    let mut brace_depth = 0usize;
    let mut index = 0usize;

    while index < tokens.len() {
        let token = tokens[index];
        match token.kind {
            TokenKind::LeftBrace => brace_depth += 1,
            TokenKind::RightBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Keyword(Keyword::Class | Keyword::Enum | Keyword::Typedef)
                if brace_depth == 0 =>
            {
                let declaration_kind = token.kind;
                if let Some((name, name_token, next_index)) =
                    lexical_outline_declaration(&tokens, index, declaration_kind, source)
                {
                    let kind = match declaration_kind {
                        TokenKind::Keyword(Keyword::Class) => 5,
                        TokenKind::Keyword(Keyword::Enum) => 10,
                        TokenKind::Keyword(Keyword::Typedef) => 26,
                        _ => unreachable!("only outline declaration keywords reach this branch"),
                    };
                    let range = lsp_range_for_span(
                        positions,
                        TextSpan::new(token.span.start, name_token.span.end),
                    );
                    let selection_range = lsp_range_for_span(positions, name_token.span);
                    symbols.push(LspDocumentSymbol {
                        name,
                        detail: None,
                        kind,
                        range,
                        selection_range,
                        children: Vec::new(),
                        repaired_full_range: None,
                    });
                    index = next_index;
                    continue;
                }
            }
            _ => {}
        }
        index += 1;
    }

    symbols
}

fn lexical_outline_declaration(
    tokens: &[Token],
    keyword_index: usize,
    declaration_kind: TokenKind,
    source: &str,
) -> Option<(String, Token, usize)> {
    let mut index = keyword_index + 1;
    let mut typedef_name = None;
    while let Some(token) = tokens.get(index).copied() {
        if token.kind.is_trivia() {
            index += 1;
            continue;
        }
        match declaration_kind {
            TokenKind::Keyword(Keyword::Class | Keyword::Enum) => {
                return (token.kind == TokenKind::Identifier).then(|| {
                    (
                        source[token.span.start..token.span.end].to_string(),
                        token,
                        index + 1,
                    )
                });
            }
            TokenKind::Keyword(Keyword::Typedef) => match token.kind {
                TokenKind::Identifier => typedef_name = Some(token),
                TokenKind::Semicolon | TokenKind::Eof => {
                    return typedef_name.map(|name| {
                        (
                            source[name.span.start..name.span.end].to_string(),
                            name,
                            index + 1,
                        )
                    });
                }
                TokenKind::LeftBrace | TokenKind::RightBrace => return None,
                _ => {}
            },
            _ => return None,
        }
        index += 1;
    }
    None
}

pub fn document_symbol_report_for_source(source: &str) -> LspDocumentSymbolReport {
    let analysis = file_index_for_source(source);
    document_symbol_report_for_cached_analysis(source, &analysis)
}

fn document_symbol_report_for_cached_analysis(
    source: &str,
    analysis: &FileIndexAnalysis,
) -> LspDocumentSymbolReport {
    debug_assert_eq!(
        analysis.semantic.declarations().len(),
        analysis.index.symbols().len(),
        "the local index must remain a complete projection of SemanticFile"
    );
    let query = IndexQuery::new(&analysis.index);
    let positions = LspPositionIndex::new(source);
    LspDocumentSymbolReport {
        symbols: document_symbols_from_index(&positions, &analysis.index, &query),
        parse_diagnostics: analysis.parse_diagnostics,
    }
}

fn lsp_range_for_span(positions: &PositionIndex, span: TextSpan) -> LspRange {
    let start = positions.position_for_offset(span.start);
    let end = positions.position_for_offset(span.end);
    LspRange {
        start: LspPosition {
            line: start.line,
            character: start.character,
        },
        end: LspPosition {
            line: end.line,
            character: end.character,
        },
    }
}

fn document_symbols_from_cached_analysis(
    source: &str,
    analysis: &FileIndexAnalysis,
) -> Vec<LspDocumentSymbol> {
    document_symbol_report_for_cached_analysis(source, analysis).symbols
}

pub fn document_symbol_count(symbols: &[LspDocumentSymbol]) -> usize {
    symbols
        .iter()
        .map(|symbol| 1 + document_symbol_count(&symbol.children))
        .sum()
}

/// Returns a bounded, source-free record of declaration-range recovery that
/// the request boundary can write to the structured diagnostic log.
pub(crate) fn document_symbol_range_repairs(
    symbols: &[LspDocumentSymbol],
    sample_limit: usize,
) -> (usize, Vec<DocumentSymbolRangeRepair>) {
    let mut count = 0usize;
    let mut samples = Vec::new();
    collect_document_symbol_range_repairs(symbols, sample_limit, &mut count, &mut samples);
    (count, samples)
}

fn collect_document_symbol_range_repairs(
    symbols: &[LspDocumentSymbol],
    sample_limit: usize,
    count: &mut usize,
    samples: &mut Vec<DocumentSymbolRangeRepair>,
) {
    for symbol in symbols {
        if let Some(original_range) = symbol.repaired_full_range {
            *count += 1;
            if samples.len() < sample_limit {
                samples.push(DocumentSymbolRangeRepair {
                    kind: symbol.kind,
                    original_range,
                    selection_range: symbol.selection_range,
                    repaired_range: symbol.range,
                });
            }
        }
        collect_document_symbol_range_repairs(&symbol.children, sample_limit, count, samples);
    }
}

fn document_symbols_from_index(
    positions: &LspPositionIndex,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
) -> Vec<LspDocumentSymbol> {
    index
        .symbols()
        .iter()
        .filter(|symbol| symbol.parent.is_none())
        .filter(|symbol| !is_document_symbol_excluded_kind(symbol.kind))
        .filter_map(|symbol| document_symbol_for_id(positions, index, query, symbol.id))
        .collect()
}

fn document_symbol_for_id(
    positions: &LspPositionIndex,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) -> Option<LspDocumentSymbol> {
    let symbol = index.symbol(id)?;
    if is_document_symbol_excluded_kind(symbol.kind) {
        return None;
    }
    let display = query.symbol_display(id)?;
    let children = index
        .children(id)
        .iter()
        .filter_map(|child| document_symbol_for_id(positions, index, query, *child))
        .collect::<Vec<_>>();
    let original_range = positions.range_for_span(symbol.span);
    let selection_range = positions.range_for_span(symbol.selection_span);
    let range = document_symbol_full_range(original_range, selection_range);

    Some(LspDocumentSymbol {
        name: display.label,
        detail: display.detail.or(display.signature),
        kind: document_symbol_kind(symbol.kind),
        range,
        selection_range,
        children,
        repaired_full_range: (range != original_range).then_some(original_range),
    })
}

/// VS Code rejects a document symbol unless the full range encloses the name
/// selection. Recovery analysis can legitimately retain a name after its
/// declaration extent has been shortened, so make that protocol invariant
/// explicit at the LSP boundary.
fn document_symbol_full_range(full_range: LspRange, selection_range: LspRange) -> LspRange {
    LspRange {
        start: position_min(full_range.start, selection_range.start),
        end: position_max(full_range.end, selection_range.end),
    }
}

fn position_min(left: LspPosition, right: LspPosition) -> LspPosition {
    ((left.line, left.character) <= (right.line, right.character))
        .then_some(left)
        .unwrap_or(right)
}

fn position_max(left: LspPosition, right: LspPosition) -> LspPosition {
    ((left.line, left.character) >= (right.line, right.character))
        .then_some(left)
        .unwrap_or(right)
}

#[derive(Debug)]
pub(crate) struct LspPositionIndex {
    positions: PositionIndex,
}

#[cfg(test)]
thread_local! {
    static POSITION_INDEX_BUILD_COUNT: Cell<usize> = const { Cell::new(0) };
}

impl LspPositionIndex {
    pub(crate) fn new(source: &str) -> Self {
        Self::new_cancellable(source, None).expect("unconditional position index build")
    }

    pub(crate) fn new_cancellable(
        source: &str,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Option<Self> {
        #[cfg(test)]
        POSITION_INDEX_BUILD_COUNT.with(|count| count.set(count.get() + 1));
        PositionIndex::new_cancellable(source, should_cancel).map(|positions| Self { positions })
    }

    pub(crate) fn position_for_offset(&self, offset: usize) -> LspPosition {
        let position = self.positions.position_for_offset(offset);
        LspPosition {
            line: position.line,
            character: position.character,
        }
    }

    pub(crate) fn range_for_span(&self, span: crate::lexer::TextSpan) -> LspRange {
        LspRange {
            start: self.position_for_offset(span.start),
            end: self.position_for_offset(span.end),
        }
    }
}

pub(crate) fn range_for_span(source: &str, span: crate::lexer::TextSpan) -> LspRange {
    LspPositionIndex::new(source).range_for_span(span)
}

pub fn position_for_offset(source: &str, offset: usize) -> LspPosition {
    let position = crate::analysis_runtime::PositionIndex::new(source).position_for_offset(offset);
    LspPosition {
        line: position.line,
        character: position.character,
    }
}

pub fn offset_for_position(source: &str, position: LspPosition) -> Option<usize> {
    let offset = crate::analysis_runtime::PositionIndex::new(source)
        .offset_for_position_recovering(crate::analysis_runtime::Position {
            line: position.line,
            character: position.character,
        })?;
    if source.as_bytes().get(offset) == Some(&b'\n')
        && offset > 0
        && source.as_bytes().get(offset - 1) == Some(&b'\r')
    {
        Some(offset + 1)
    } else {
        Some(offset)
    }
}

fn document_symbol_kind(kind: SymbolKind) -> u32 {
    match kind {
        SymbolKind::Class => 5,
        SymbolKind::TypeParameter => 26,
        SymbolKind::Enum => 10,
        SymbolKind::EnumMember => 22,
        SymbolKind::Typedef => 26,
        SymbolKind::Function => 12,
        SymbolKind::GlobalField => 13,
        SymbolKind::Field => 8,
        SymbolKind::Method => 6,
        SymbolKind::Constructor => 9,
        SymbolKind::Destructor => 6,
        SymbolKind::Parameter => 13,
        SymbolKind::LocalVariable => 13,
        SymbolKind::PreprocessorMacro => 13,
    }
}

pub fn symbol_kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "Class",
        SymbolKind::TypeParameter => "TypeParameter",
        SymbolKind::Enum => "Enum",
        SymbolKind::EnumMember => "EnumMember",
        SymbolKind::Typedef => "Typedef",
        SymbolKind::Function => "Function",
        SymbolKind::GlobalField => "GlobalField",
        SymbolKind::Field => "Field",
        SymbolKind::Method => "Method",
        SymbolKind::Constructor => "Constructor",
        SymbolKind::Destructor => "Destructor",
        SymbolKind::Parameter => "Parameter",
        SymbolKind::LocalVariable => "LocalVariable",
        SymbolKind::PreprocessorMacro => "PreprocessorMacro",
    }
}

fn is_document_symbol_excluded_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::TypeParameter
            | SymbolKind::Parameter
            | SymbolKind::LocalVariable
            | SymbolKind::PreprocessorMacro
    )
}

pub(crate) fn span_text(source: &str, span: TextSpan) -> &str {
    source.get(span.start..span.end).unwrap_or("")
}

fn validate_message_params(method: &str, params: &Option<Value>) -> Result<(), String> {
    match method {
        "textDocument/didOpen" => validate_params::<DidOpenTextDocumentParams>(params, method),
        "textDocument/didChange" => validate_params::<DidChangeTextDocumentParams>(params, method),
        "textDocument/didClose" => validate_params::<DidCloseTextDocumentParams>(params, method),
        "reforger/workspaceFileChanged" => {
            validate_params::<WorkspaceFileChangedParams>(params, method)
        }
        "reforger/workspaceFileDeleted" => {
            validate_params::<WorkspaceFileDeletedParams>(params, method)
        }
        "reforger/loadedAddonGraph" => validate_params::<LoadedAddonGraphParams>(params, method),
        "textDocument/documentSymbol" | "textDocument/semanticTokens/full" => {
            validate_params::<DocumentSymbolParams>(params, method)
        }
        "textDocument/completion" => validate_params::<CompletionParams>(params, method),
        "textDocument/signatureHelp"
        | "textDocument/hover"
        | "textDocument/definition"
        | DEBUG_HOVER_METHOD
        | DEBUG_COMPLETION_METHOD
        | PREVIEW_CONTEXT_METHOD => validate_params::<HoverParams>(params, method),
        RANGE_FORMATTING_METHOD => validate_params::<RangeFormattingParams>(params, method),
        BLOCK_COMMENT_PAIR_METHOD => validate_params::<BlockCommentPairParams>(params, method),
        CONTROL_HEADER_ENTER_METHOD => validate_params::<InputRouteParams>(params, method),
        ACTIVE_SCOPE_DELIMITERS_METHOD => {
            validate_params::<ActiveScopeDelimiterParams>(params, method)
        }
        READ_PACK_SOURCE_METHOD => validate_params::<ReadPackSourceParams>(params, method),
        _ => Ok(()),
    }
}

fn validate_params<T: for<'de> Deserialize<'de>>(
    params: &Option<Value>,
    method: &str,
) -> Result<(), String> {
    let Some(params) = params else {
        return Err(format!("Invalid params for {method}: missing params"));
    };
    serde_json::from_value::<T>(params.clone())
        .map(|_| ())
        .map_err(|error| format!("Invalid params for {method}: {error}"))
}

#[cfg(test)]
mod tests;
