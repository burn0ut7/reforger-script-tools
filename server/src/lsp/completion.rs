use crate::analysis_runtime::QueryQuality;
#[cfg(test)]
use crate::callable::callable_signature_parts;
use crate::callable::{
    builtin_callable_fact, callable_argument_context_at_offset, callable_type_owner,
    CallableParameter, CallableSignatureParts, CallableTarget,
};
use crate::construction::{
    compatible_construction_candidates, lexical_construction_context_at_operand, ConstructionQuery,
};
use crate::expression_type::{
    expression_type_from_index_symbol, preferred_class_is_modded, strip_all_type_prefixes,
};
use crate::index::SymbolIndex;
use crate::index_query::{
    completion_name_match_rank, EditorCompletionCandidate, EditorCompletionOrigin,
    EditorTopLevelCompletionMode, IndexQuery,
};
use crate::lexer::{lex, Keyword, Operator, TextSpan, Token, TokenKind};
use crate::lsp::collection_declaration::collection_declaration_before_cursor;
use crate::lsp::external_indexes::ExternalIndexes;
use crate::lsp::{
    file_index_for_source, offset_for_position, range_for_span, FileIndexAnalysis,
    LspMarkupContent, LspPosition, LspRange,
};
use crate::model::{SourceKind, SymbolKind};
use crate::preprocessor::{completion_context_at_offset, CompletionContext, COMPLETION_DIRECTIVES};
use crate::resolver::{CandidateSource, IdentifierContext, ReferenceCandidate, ReferenceResolver};
use crate::syntax::SyntaxNode;
use serde::{ser::SerializeMap, Serialize, Serializer};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const MAX_COMPLETION_ITEMS: usize = 250;
/// A foreground local query deliberately has a small, source-free admission
/// bound. Larger documents continue on the runtime semantic lane and use the
/// documented lexical/top-level result meanwhile.
/// Foreground queries inspect a fixed syntax window around the cursor.  This
/// deliberately bounds both lexing and declaration recovery for arbitrarily
/// large current snapshots; full parser/index construction stays on the
/// runtime semantic lane.
const LOCAL_SCOPE_QUERY_MAX_WINDOW_BYTES: usize = 16 * 1024;
const LOCAL_SCOPE_QUERY_TRAILING_BYTES: usize = 2 * 1024;
const OVERRIDE_QUERY_MAX_WINDOW_BYTES: usize = LOCAL_SCOPE_QUERY_MAX_WINDOW_BYTES + 1024;
/// When the fixed window begins or ends in a multi-line lexical construct, a
/// small current snapshot may be lexed once to establish its real state. This
/// preserves immediate externally-proven completion in normal large files
/// without making foreground requests scale without bound.
const LEXICAL_CONTEXT_RECOVERY_MAX_SOURCE_BYTES: usize = 128 * 1024;
const LOCAL_SCOPE_QUERY_DEADLINE: Duration = Duration::from_millis(50);
const COMMAND_TRIGGER_PARAMETER_HINTS: &str = "editor.action.triggerParameterHints";
/// A VS Code UI bridge registered by the extension. Rust decides when an
/// callable template needs enum completion; the client waits for snippet mode
/// to place the caret at each Rust-authored placeholder before dispatching Suggest.
const COMMAND_TRIGGER_SUGGEST_AT_SNIPPET_PLACEHOLDER: &str =
    "reforger-sript-tools.completion.triggerSuggestAtSnippetPlaceholder";
/// A completion-specific UI adapter which removes the one Space commit
/// character that VS Code appends after this item's snippet edit. It has an
/// exact caret-local contract and never classifies ordinary source text.
const COMMAND_NORMALIZE_IF_SPACE_COMMIT: &str =
    "reforger-sript-tools.completion.normalizeIfSpaceCommit";
const COMMAND_APPLY_DIRECTIVE_SEPARATOR: &str =
    "reforger-sript-tools.completion.applyDirectiveSeparator";
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspCompletionList {
    pub is_incomplete: bool,
    pub items: Vec<LspCompletionItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspCompletionItem {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_details: Option<LspCompletionItemLabelDetails>,
    pub kind: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<LspMarkupContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_text: Option<String>,
    /// Marks the uniquely proven contextual constructor as the editor's
    /// initially selected suggestion without accepting or inserting it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preselect: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert_text_format: Option<u32>,
    /// Characters which accept this item through the editor's single native
    /// completion transaction. This is intentionally item-specific: normal
    /// typing must not become a completion acceptance path globally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_characters: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<LspCommand>,
    pub text_edit: LspTextEdit,
    #[serde(skip)]
    pub required_parameter_count: usize,
    #[serde(skip)]
    pub optional_parameter_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspCommand {
    pub title: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspCompletionItemLabelDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspTextEdit {
    /// The regular insertion range. This remains the active word so clients
    /// can filter and show the completion list normally.
    pub range: LspRange,
    pub new_text: String,
    /// When present, serialize this edit as LSP's `InsertReplaceEdit`: the
    /// client uses `range` while presenting the list and `replace_range` only
    /// after the user accepts an item.
    pub replace_range: Option<LspRange>,
}

impl Serialize for LspTextEdit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map =
            serializer.serialize_map(Some(if self.replace_range.is_some() { 3 } else { 2 }))?;
        map.serialize_entry("newText", &self.new_text)?;
        if let Some(replace_range) = &self.replace_range {
            assert!(
                insert_replace_ranges_are_valid(&self.range, replace_range),
                "LSP InsertReplaceEdit insert range must share the replace range start and remain its prefix"
            );
            map.serialize_entry("insert", &self.range)?;
            map.serialize_entry("replace", replace_range)?;
        } else {
            map.serialize_entry("range", &self.range)?;
        }
        map.end()
    }
}

fn insert_replace_ranges_are_valid(insert: &LspRange, replace: &LspRange) -> bool {
    insert.start == replace.start
        && (insert.end.line, insert.end.character) <= (replace.end.line, replace.end.character)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspCompletionReport {
    pub list: LspCompletionList,
    /// The candidate guarantee for this current-revision result. This is an
    /// internal contract/logging field, not an LSP `isIncomplete` surrogate.
    pub query_quality: QueryQuality,
    /// Why a non-exact result was selected.
    pub recovery_reason: Option<String>,
    pub parse_diagnostics: usize,
    pub completion_context: String,
    pub receiver_text: Option<String>,
    pub owner_type: Option<String>,
    pub prefix: String,
    pub candidate_count: usize,
    pub source_kind_counts: BTreeMap<SourceKind, usize>,
    pub origin_counts: BTreeMap<String, usize>,
    pub failure_reason: Option<String>,
    pub timings: LspCompletionTimings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LspCompletionTimings {
    pub context_detection: Duration,
    pub receiver_inference: Duration,
    pub candidate_lookup: Duration,
    pub item_rendering: Duration,
    pub total: Duration,
}

#[derive(Debug, Clone, Copy)]
struct CompletionRenderContext<'a> {
    local_index: &'a SymbolIndex,
    workspace_index: Option<&'a SymbolIndex>,
    game_data_index: Option<&'a SymbolIndex>,
}

impl<'a> CompletionRenderContext<'a> {
    fn new(
        local_index: &'a SymbolIndex,
        workspace_index: Option<&'a SymbolIndex>,
        game_data_index: Option<&'a SymbolIndex>,
    ) -> Self {
        Self {
            local_index,
            workspace_index,
            game_data_index,
        }
    }

    fn is_enum_owner(self, owner: &str) -> bool {
        index_has_enum_owner(self.local_index, owner)
            || self
                .workspace_index
                .is_some_and(|index| index_has_enum_owner(index, owner))
            || self
                .game_data_index
                .is_some_and(|index| index_has_enum_owner(index, owner))
    }
}

pub fn completion_report_for_source_position_with_external(
    source: &str,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
) -> LspCompletionReport {
    let analysis = file_index_for_source(source);
    completion_report_for_cached_analysis_with_external(source, &analysis, position, external_index)
}

pub fn completion_report_for_cached_analysis_with_external(
    source: &str,
    analysis: &FileIndexAnalysis,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
) -> LspCompletionReport {
    completion_report_for_cached_analysis_with_external_indexes(
        source,
        analysis,
        position,
        None,
        external_index,
    )
}

pub(crate) fn completion_report_for_cached_analysis_with_external_indexes(
    source: &str,
    analysis: &FileIndexAnalysis,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspCompletionReport {
    let Some(offset) = offset_for_position(source, position) else {
        return empty_completion_report(analysis.parse_diagnostics);
    };
    if completion_cursor_is_in_comment_or_string(&analysis.syntax.lexer_tokens, offset) {
        return empty_completion_report(analysis.parse_diagnostics);
    }
    let mut report =
        completion_report_for_offset(source, analysis, offset, workspace_index, game_data_index);
    apply_retyped_completion_suffix_replacements(source, offset, &mut report);
    report
}

/// A completion that authors a structural suffix must replace an identical,
/// already-present suffix when the user retypes its label. Otherwise accepting
/// it turns `Call()` into `Call()()` or `name: value` into `name: : value`.
/// Keep this bounded to the active prefix and an immediately following empty
/// call or named-argument separator; ordinary expressions and non-empty calls
/// retain their native replacement behavior.
fn apply_retyped_completion_suffix_replacements(
    source: &str,
    offset: usize,
    report: &mut LspCompletionReport,
) {
    let prefix_span = raw_completion_prefix_span(source, offset);
    if prefix_span.is_empty() || prefix_span.end != offset {
        return;
    }
    let prefix_range = range_for_span(source, prefix_span);
    let call_suffix = parenthesized_suffix_after_prefix(source, prefix_span);
    let named_argument_replace_range = named_argument_separator_after_prefix(source, prefix_span)
        .map(|span| range_for_span(source, span));

    for item in &mut report.list.items {
        if item.text_edit.replace_range.is_some() || item.text_edit.range != prefix_range {
            continue;
        }
        if let Some(call_suffix) = call_suffix {
            if !completion_insert_text_authors_call_suffix(item) {
                continue;
            }
            if call_suffix.is_empty {
                item.text_edit.replace_range = Some(range_for_span(source, call_suffix.span));
            } else {
                item.text_edit.new_text = item.label.clone();
                item.insert_text_format = None;
            }
        } else if named_argument_replace_range.is_some()
            && completion_insert_text_authors_named_argument_separator(item)
        {
            item.text_edit.replace_range = named_argument_replace_range.clone();
        }
    }
}

#[derive(Clone, Copy)]
struct ParenthesizedSuffix {
    span: TextSpan,
    is_empty: bool,
}

fn parenthesized_suffix_after_prefix(
    source: &str,
    prefix_span: TextSpan,
) -> Option<ParenthesizedSuffix> {
    let tokens = lex(source)
        .into_iter()
        .filter(|token| !token.kind.is_trivia() && token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    let open_index = tokens
        .iter()
        .position(|token| token.span.start >= prefix_span.end)?;
    let open = tokens.get(open_index)?;
    if open.kind != TokenKind::LeftParen {
        return None;
    }
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open_index) {
        match token.kind {
            TokenKind::LeftParen => depth += 1,
            TokenKind::RightParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(ParenthesizedSuffix {
                        span: TextSpan::new(prefix_span.start, token.span.end),
                        is_empty: index == open_index + 1,
                    });
                }
            }
            _ => {}
        }
    }
    None
}

fn named_argument_separator_after_prefix(source: &str, prefix_span: TextSpan) -> Option<TextSpan> {
    let tokens = lex(source);
    let separator_index = tokens.iter().position(|token| {
        !token.kind.is_trivia()
            && token.kind != TokenKind::Eof
            && token.span.start >= prefix_span.end
    })?;
    let separator = tokens.get(separator_index)?;
    if separator.kind != TokenKind::Colon {
        return None;
    }
    let end = tokens
        .iter()
        .skip(separator_index + 1)
        .find(|token| !token.kind.is_trivia() && token.kind != TokenKind::Eof)
        .map(|token| token.span.start)
        .unwrap_or(source.len());
    Some(TextSpan::new(prefix_span.start, end))
}

fn completion_insert_text_authors_call_suffix(item: &LspCompletionItem) -> bool {
    item.text_edit
        .new_text
        .strip_prefix(&item.label)
        .is_some_and(|suffix| suffix.trim_start().starts_with('('))
}

fn completion_insert_text_authors_named_argument_separator(item: &LspCompletionItem) -> bool {
    item.text_edit
        .new_text
        .strip_prefix(&item.label)
        .is_some_and(|suffix| suffix.starts_with(':'))
}

/// Returns the current-revision lexical/top-level completion contract while a
/// whole-file analysis is still converging.  It intentionally supplies no
/// stale local scope or syntax facts: the lightweight analysis has current
/// lexer tokens plus an empty local index, so candidates are limited to
/// independently valid workspace/game-data and language facts.
pub(crate) fn completion_report_for_lexical_source_with_external_indexes(
    source: &str,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspCompletionReport {
    let Some(offset) = offset_for_position(source, position) else {
        return unavailable_completion_report(empty_completion_report(0), "invalid-position");
    };
    completion_report_for_lexical_source_at_offset_with_external_indexes(
        source,
        offset,
        workspace_index,
        game_data_index,
    )
}

pub(crate) fn completion_report_for_lexical_source_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspCompletionReport {
    if current_snapshot_cursor_is_in_comment_or_string(source, offset) {
        return empty_completion_report(0);
    }
    if let Some(context) = completion_context_at_offset(source, offset) {
        let empty_local_index = SymbolIndex::default();
        return preprocessor_completion_report(
            source,
            0,
            context,
            &empty_local_index,
            workspace_index,
            game_data_index,
            Duration::ZERO,
            Instant::now(),
        );
    }
    if let Some(report) =
        collection_declaration_tail_completion_report(source, 0, offset, Instant::now())
    {
        return report;
    }
    let Some(region) = BoundedCompletionRegion::new(source, offset) else {
        if current_snapshot_cursor_is_in_comment_or_string(source, offset) {
            return empty_completion_report(0);
        }
        // The fixed window can end inside a valid multi-line comment or
        // string.  It is not safe to recover local or receiver facts without
        // lexical context, but the cursor-local identifier prefix is enough
        // to retain independently indexed top-level suggestions.
        let report = top_level_fallback_report_for_prefix_span(
            source,
            raw_completion_prefix_span(source, offset),
            workspace_index,
            game_data_index,
            UnavailableCompletionContext::TopLevel,
        );
        return unavailable_completion_report(report, "invalid-or-unterminated-lexical-window");
    };
    if completion_cursor_is_in_comment_or_string(&region.tokens, offset) {
        return empty_completion_report(0);
    }
    let context = unavailable_completion_context(&region.tokens, offset);
    let report = lexical_top_level_fallback_report(
        source,
        &region.tokens,
        offset,
        workspace_index,
        game_data_index,
        context,
    );
    unavailable_completion_report(report, context.reason())
}

fn collection_declaration_tail_completion_report(
    source: &str,
    parse_diagnostics: usize,
    offset: usize,
    total_start: Instant,
) -> Option<LspCompletionReport> {
    let declaration = collection_declaration_before_cursor(source, offset, true)?;
    let whitespace = source.get(declaration.name_span.end..offset)?;
    if whitespace.is_empty() || !whitespace.chars().all(char::is_whitespace) {
        return None;
    }
    let type_text = source.get(declaration.type_span.start..declaration.type_span.end)?;
    let range = range_for_span(source, TextSpan::new(declaration.name_span.end, offset));
    let item =
        |label: &str, detail: &str, new_text: String, order: usize, command: Option<LspCommand>| {
            let label = if label.starts_with("Custom initializer") {
                "Custom initializer\u{2026}"
            } else {
                label
            };
            LspCompletionItem {
                label: label.to_string(),
                label_details: None,
                kind: 15,
                detail: Some(detail.to_string()),
                documentation: None,
                sort_text: Some(format!("000:collection-tail:{order:03}:{label}")),
                filter_text: Some(label.to_string()),
                preselect: None,
                insert_text_format: new_text.contains("${").then_some(2),
                commit_characters: None,
                command,
                text_edit: LspTextEdit {
                    range: range.clone(),
                    new_text,
                    replace_range: None,
                },
                required_parameter_count: 0,
                optional_parameter_count: 0,
            }
        };
    let mut items = Vec::new();
    if declaration.collection == "array" {
        items.push(item(
            "Initialize with empty literal",
            "= {};",
            " = {};".to_string(),
            0,
            None,
        ));
        items.push(item(
            "Initialize with new array",
            "= new array<T>;",
            format!(" = new {type_text};"),
            1,
            None,
        ));
    } else {
        items.push(item(
            "Initialize with new collection",
            "= new collection<T>;",
            format!(" = new {type_text};"),
            0,
            None,
        ));
    }
    let order = items.len();
    items.push(item(
        "Declare without initializer",
        ";",
        ";".to_string(),
        order,
        None,
    ));
    items.push(item(
        "Custom initializer…",
        "= ",
        " = ${1:Expression}".to_string(),
        order + 1,
        Some(trigger_suggest_at_snippet_placeholder_command(vec![
            "Expression".to_string(),
        ])),
    ));
    Some(LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete: false,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: "collection-declaration-tail".to_string(),
        receiver_text: None,
        owner_type: None,
        prefix: String::new(),
        source_kind_counts: BTreeMap::new(),
        origin_counts: BTreeMap::new(),
        failure_reason: None,
        timings: LspCompletionTimings {
            total: total_start.elapsed(),
            ..LspCompletionTimings::default()
        },
    })
}

/// Handles the source forms that are complete from the current snapshot alone
/// before pending-analysis queries attempt ordinary local, receiver, or
/// argument completion. This keeps directive completion independent of the
/// document-analysis publication race.
pub(crate) fn completion_report_for_current_preprocessor_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    if current_snapshot_cursor_is_in_comment_or_string(source, offset) {
        return None;
    }
    let context = completion_context_at_offset(source, offset)?;
    let empty_local_index = SymbolIndex::default();
    Some(preprocessor_completion_report(
        source,
        0,
        context,
        &empty_local_index,
        workspace_index,
        game_data_index,
        Duration::ZERO,
        Instant::now(),
    ))
}

/// Recovers an exact declaration-backed `new` operand from the bounded current
/// snapshot. Collection construction is complete from source alone; indexed
/// class candidates remain refreshable because a pending local index can add
/// same-file subclasses.
pub(crate) fn completion_report_for_current_contextual_constructor_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    let total_start = Instant::now();
    let region = BoundedCompletionRegion::new(source, offset)?;
    (!completion_cursor_is_in_comment_or_string(&region.tokens, offset)).then_some(())?;
    let operand = new_operand_before_offset(source, &region.tokens, offset)?;
    let context = lexical_construction_context_at_operand(source, &region.tokens, operand.span)?;
    let empty_local_index = SymbolIndex::default();
    let candidates = compatible_construction_candidates(
        &context,
        &empty_local_index,
        ExternalIndexes::new(workspace_index, game_data_index).ordered(),
    );
    let owner = callable_type_owner(&context.type_text)?;
    let is_collection = matches!(owner.as_str(), "array" | "set" | "map");
    if !is_collection && candidates.is_empty() {
        return None;
    }
    let replacement_span = if operand.replaces_typed_prefix {
        operand.span
    } else {
        TextSpan::new(offset, offset)
    };
    let report = contextual_constructor_completion_report(
        source,
        0,
        context.type_text,
        candidates,
        replacement_span,
        operand.replaces_typed_prefix,
        &empty_local_index,
        workspace_index,
        game_data_index,
        Duration::ZERO,
        total_start,
    );
    Some(if is_collection {
        report
    } else {
        unavailable_completion_report(report, "current-local-index-pending")
    })
}

/// Recovers a prospective method parameter type before the pending-analysis
/// fast path asks for argument labels. This is the current-snapshot counterpart
/// to the complete-analysis recovery in `completion_report_for_offset`: without
/// it, typing the first letter in `int Method(a)` can surface the plain external
/// `array` class instead of the generic collection snippet.
pub(crate) fn completion_report_for_current_incomplete_callable_parameter_type_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    if current_snapshot_cursor_is_in_comment_or_string(source, offset)
        || !incomplete_callable_parameter_type_before_offset(source, offset)
    {
        return None;
    }
    let prefix_span = raw_completion_prefix_span(source, offset);
    Some(top_level_fallback_report_for_prefix_span_with_mode(
        source,
        prefix_span,
        workspace_index,
        game_data_index,
        UnavailableCompletionContext::TopLevel,
        EditorTopLevelCompletionMode::Type,
    ))
}

/// Runs `LocalScopeQuery` over a fixed lexical/syntax window from the current
/// revision. It supplies unqualified values both at ordinary value positions
/// and inside an argument expression, without parsing, indexing, or projecting
/// the file. When the callable block cannot be proved inside that window,
/// callers use the documented unavailable fallback.
pub(crate) fn completion_report_for_current_local_scope_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    let start = Instant::now();
    // Preserve the preceding line at the bounded-window edge so the current
    // class header remains available to unqualified member recovery.
    let region = BoundedCompletionRegion::for_current_override(source, offset)?;
    (!completion_cursor_is_in_comment_or_string(&region.tokens, offset)).then_some(())?;
    if !matches!(
        unavailable_completion_context(&region.tokens, offset),
        UnavailableCompletionContext::TopLevel | UnavailableCompletionContext::Argument
    ) {
        return None;
    }
    let facts = BoundedCompletionFacts::recover(source, &region, offset)?;
    if start.elapsed() > LOCAL_SCOPE_QUERY_DEADLINE {
        return None;
    }
    Some(facts.local_completion_report(
        source,
        &region,
        offset,
        workspace_index,
        game_data_index,
        start,
    ))
}

/// Runs a bounded current-revision query for an override declaration. The
/// current lexer window must prove both the enclosing class body and its direct
/// base; method facts always come from immutable external indexes.
pub(crate) fn completion_report_for_current_override_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    let start = Instant::now();
    let region = BoundedCompletionRegion::for_current_override(source, offset)?;
    (!completion_cursor_is_in_comment_or_string(&region.tokens, offset)).then_some(())?;
    let context = region.current_override_context(source, offset)?;
    if start.elapsed() > LOCAL_SCOPE_QUERY_DEADLINE {
        return None;
    }

    let mut candidates = Vec::new();
    for owner in external_completion_owners(&context.base_type, workspace_index, game_data_index) {
        if let Some(index) = workspace_index {
            candidates.extend(prefixed_candidates(
                if context.modded {
                    override_candidates_for_predecessor_owner(index, &owner)
                } else {
                    override_candidates_for_owner(index, &owner)
                },
                &context.prefix,
            ));
        }
        if let Some(index) = game_data_index {
            candidates.extend(prefixed_candidates(
                if context.modded {
                    override_candidates_for_predecessor_owner(index, &owner)
                } else {
                    override_candidates_for_owner(index, &owner)
                },
                &context.prefix,
            ));
        }
    }
    let candidates = combine_completion_candidates(candidates);
    if start.elapsed() > LOCAL_SCOPE_QUERY_DEADLINE {
        return None;
    }

    let existing_body = override_completion_has_existing_body(&region.tokens, context.prefix_span);
    let (mut items, mut source_kind_counts, mut origin_counts) =
        completion_items_for_override_candidates(
            &candidates,
            range_for_span(source, context.prefix_span),
            &context.typed_modifiers,
            &context.prefix,
            existing_body,
        );
    let empty_local_index = SymbolIndex::default();
    let source_candidates = top_level_source_completion_candidates(
        &context.prefix,
        EditorTopLevelCompletionMode::Type,
        &empty_local_index,
        workspace_index,
        game_data_index,
        MAX_COMPLETION_ITEMS + 1,
    );
    let insert_context = completion_insert_context(
        source,
        context.prefix_span.start,
        EditorTopLevelCompletionMode::Type,
    );
    let (source_items, source_counts, source_origins) = completion_items_for_candidates(
        &source_candidates,
        range_for_span(source, context.prefix_span),
        Some("TopLevel"),
        insert_context,
        Some(&context.prefix),
        CompletionRenderContext::new(&empty_local_index, workspace_index, game_data_index),
    );
    merge_count_maps(&mut source_kind_counts, source_counts);
    merge_count_maps(&mut origin_counts, source_origins);
    let mut keyword_items = keyword_completion_items(
        &context.prefix,
        range_for_span(source, context.prefix_span),
        EditorTopLevelCompletionMode::Type,
        true,
    );
    if !keyword_items.is_empty() {
        prioritize_keyword_item(&mut keyword_items, "override");
        remove_items_shadowed_by_keywords(&mut items, &keyword_items);
        keyword_items.extend(items);
        items = keyword_items;
    }
    items.extend(source_items);
    // This bounded query is only a contextual specialization. When neither an
    // inherited member nor a declaration keyword matches, preserve the normal
    // local/top-level completion path instead of claiming an empty override
    // response for an ordinary identifier at class scope.
    (!items.is_empty()).then_some(())?;
    let (items, is_incomplete) = cap_completion_items(items);

    Some(LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics: 0,
        completion_context: "override".to_string(),
        receiver_text: None,
        owner_type: Some(context.base_type),
        prefix: context.prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings: LspCompletionTimings {
            total: start.elapsed(),
            ..LspCompletionTimings::default()
        },
    })
}

/// Runs the current-revision `ReceiverResolutionQuery` for one bare local,
/// parameter, or field receiver. It also accepts a zero-argument call chain
/// whose callable facts are fully supplied by the captured external indexes.
/// A bare static owner is also admitted when bounded current facts prove that
/// no visible local binding shadows its externally indexed name. Other chained,
/// call, index, malformed, and over-budget expressions use the lexical fallback
/// rather than joining current text to older local analysis.
pub(crate) fn completion_report_for_current_receiver_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    let start = Instant::now();
    let region = BoundedCompletionRegion::new(source, offset)?;
    (!completion_cursor_is_in_comment_or_string(&region.tokens, offset)).then_some(())?;
    if unavailable_completion_context(&region.tokens, offset)
        != UnavailableCompletionContext::Member
    {
        return None;
    }
    let external_chain =
        region.external_call_chain_member_context(source, offset, workspace_index, game_data_index);
    let mut class_region = None;
    let (receiver, receiver_span, owner, prefix, prefix_span, receiver_is_static, facts) =
        if let Some((receiver, receiver_span, owner, prefix, prefix_span)) = external_chain {
            (
                receiver,
                receiver_span,
                owner,
                prefix,
                prefix_span,
                false,
                None,
            )
        } else {
            if region.has_completed_call_receiver(source, offset) {
                let report = lexical_top_level_fallback_report(
                    source,
                    &region.tokens,
                    offset,
                    workspace_index,
                    game_data_index,
                    UnavailableCompletionContext::Member,
                );
                return Some(unavailable_completion_report(
                    report,
                    "current-revision-external-call-chain-unresolved",
                ));
            }
            let facts = BoundedCompletionFacts::recover(source, &region, offset)?;
            let (receiver, receiver_span, prefix, prefix_span) =
                region.simple_member_context(source, offset)?;
            if let Some(owner) = facts.visible_type(&receiver) {
                (
                    receiver,
                    receiver_span,
                    owner,
                    prefix,
                    prefix_span,
                    false,
                    Some(facts),
                )
            } else if external_static_owner_has_members(&receiver, workspace_index, game_data_index)
            {
                // Static enum completion also needs the enclosing class
                // header to recover current and inherited value candidates.
                // Reuse the bounded override window rather than reading any
                // previous analysis or delaying the request.
                class_region = BoundedCompletionRegion::for_current_override(source, offset);
                let class_facts =
                    BoundedCompletionFacts::recover(source, class_region.as_ref()?, offset)?;
                if class_facts.visible_type(&receiver).is_some() {
                    return None;
                }
                // The current bounded facts prove no local/parameter/field
                // binding shadows this name. Static candidates therefore come
                // solely from immutable external indexes and remain safe while
                // whole-file analysis is pending.
                (
                    receiver.clone(),
                    receiver_span,
                    receiver,
                    prefix,
                    prefix_span,
                    true,
                    Some(class_facts),
                )
            } else {
                return None;
            }
        };
    if start.elapsed() > LOCAL_SCOPE_QUERY_DEADLINE {
        return None;
    }
    let empty_local_index = SymbolIndex::default();
    let mut report = member_completion_report_for_indexes(
        source,
        0,
        &owner,
        Some(receiver),
        receiver_span,
        Some(owner.clone()),
        prefix.clone(),
        prefix_span,
        None,
        receiver_is_static,
        MemberVisibilityContext::ExternalReceiver,
        &empty_local_index,
        None,
        workspace_index,
        game_data_index,
        LspCompletionTimings::default(),
        start,
    );
    if let Some(facts) = facts {
        let facts_region = class_region.as_ref().unwrap_or(&region);
        if receiver_is_static {
            if CompletionRenderContext::new(&empty_local_index, workspace_index, game_data_index)
                .is_enum_owner(&owner)
            {
                facts.append_current_enum_value_items(
                    source,
                    facts_region,
                    offset,
                    &owner,
                    receiver_span,
                    prefix_span,
                    &prefix,
                    workspace_index,
                    game_data_index,
                    &mut report,
                );
            }
            // Static-owner completion is authoritative from the captured
            // indexes. The bounded current-source member recovery has no
            // modifier model, so applying it here would leak instance methods
            // into `Type.` while foreground analysis is pending.
        } else {
            facts.append_current_member_items(
                source,
                facts_region,
                &owner,
                &prefix,
                prefix_span,
                &mut report,
            );
        }
    }
    Some(report)
}

fn external_static_owner_has_members(
    owner: &str,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> bool {
    ExternalIndexes::new(workspace_index, game_data_index)
        .ordered()
        .into_iter()
        .any(|index| {
            !IndexQuery::new(index)
                .completion_static_members_for_type(owner)
                .is_empty()
        })
}

/// Produces the one base-call completion that corresponds to the callable the
/// user is currently overriding. This is a bounded current-snapshot recovery:
/// it reads only the enclosing class/method shape while the full document
/// analysis is pending, then takes the callable signature from immutable
/// external indexes.
pub(crate) fn completion_report_for_current_super_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    let start = Instant::now();
    let region = BoundedCompletionRegion::for_current_override(source, offset)?;
    (!completion_cursor_is_in_comment_or_string(&region.tokens, offset)).then_some(())?;
    let context = region.current_super_call_context(source, offset)?;
    if start.elapsed() > LOCAL_SCOPE_QUERY_DEADLINE {
        return None;
    }

    let mut candidates = Vec::new();
    for index in ExternalIndexes::new(workspace_index, game_data_index).ordered() {
        candidates.extend(
            if context.modded {
                completion_candidates_for_predecessor_owner(index, &context.base_type, false)
            } else {
                completion_candidates_for_owner(index, &context.base_type, false)
            }
            .into_iter()
            .filter(is_overridable_method_candidate)
            .filter(|candidate| candidate.name.as_deref() == Some(&context.method_name)),
        );
    }
    let candidates = combine_completion_candidates(candidates);
    let empty_local_index = SymbolIndex::default();
    let (mut items, source_kind_counts, origin_counts) = completion_items_for_candidates(
        &candidates,
        range_for_span(source, context.prefix_span),
        Some("BaseCall"),
        CompletionInsertContext::Normal,
        Some("super"),
        CompletionRenderContext::new(&empty_local_index, workspace_index, game_data_index),
    );
    for (order, item) in items.iter_mut().enumerate() {
        item.label = format!("super.{}", item.label);
        item.filter_text = Some("super".to_string());
        item.sort_text = Some(format!("000:base-call:{order:03}:{}", item.label));
        item.text_edit.new_text = format!("super.{}", item.text_edit.new_text);
    }
    let (items, is_incomplete) = cap_completion_items(items);
    Some(LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics: 0,
        completion_context: "super".to_string(),
        receiver_text: Some("super".to_string()),
        owner_type: Some(context.base_type),
        prefix: context.prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings: LspCompletionTimings {
            total: start.elapsed(),
            ..LspCompletionTimings::default()
        },
    })
}

fn super_call_completion_report_for_indexes(
    source: &str,
    parse_diagnostics: usize,
    offset: usize,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    total_start: Instant,
) -> Option<LspCompletionReport> {
    let region = BoundedCompletionRegion::for_current_override(source, offset)?;
    let context = region.current_super_call_context(source, offset)?;
    let mut candidates = if context.modded {
        Vec::new()
    } else {
        completion_candidates_for_owner(local_index, &context.base_type, false)
    };
    if let Some(index) = workspace_index {
        candidates.extend(if context.modded {
            completion_candidates_for_predecessor_owner(index, &context.base_type, false)
        } else {
            completion_candidates_for_owner(index, &context.base_type, false)
        });
    }
    if let Some(index) = game_data_index {
        candidates.extend(if context.modded {
            completion_candidates_for_predecessor_owner(index, &context.base_type, false)
        } else {
            completion_candidates_for_owner(index, &context.base_type, false)
        });
    }
    let candidates = combine_completion_candidates(
        candidates
            .into_iter()
            .filter(is_overridable_method_candidate)
            .filter(|candidate| candidate.name.as_deref() == Some(&context.method_name))
            .collect(),
    );
    let (mut items, source_kind_counts, origin_counts) = completion_items_for_candidates(
        &candidates,
        range_for_span(source, context.prefix_span),
        Some("BaseCall"),
        CompletionInsertContext::Normal,
        Some("super"),
        CompletionRenderContext::new(local_index, workspace_index, game_data_index),
    );
    for (order, item) in items.iter_mut().enumerate() {
        item.label = format!("super.{}", item.label);
        item.filter_text = Some("super".to_string());
        item.sort_text = Some(format!("000:base-call:{order:03}:{}", item.label));
        item.text_edit.new_text = format!("super.{}", item.text_edit.new_text);
    }
    let (items, is_incomplete) = cap_completion_items(items);
    (!items.is_empty()).then_some(LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: "super".to_string(),
        receiver_text: Some("super".to_string()),
        owner_type: Some(context.base_type),
        prefix: context.prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings: LspCompletionTimings {
            total: total_start.elapsed(),
            ..LspCompletionTimings::default()
        },
    })
}

/// Runs the bounded current-revision argument-label query for one bare,
/// already-resolved callable. The query admits only a valid current CST and a
/// simple identifier callee, so it cannot combine current argument text with
/// a prior document's callable facts. Member/delegate calls, constructors,
/// malformed text, values after a label, and over-budget work intentionally
/// remain on the documented unavailable fallback.
pub(crate) fn completion_report_for_current_argument_labels_at_offset_with_external_indexes(
    source: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<LspCompletionReport> {
    let start = Instant::now();
    let region = BoundedCompletionRegion::new(source, offset)?;
    (!completion_cursor_is_in_comment_or_string(&region.tokens, offset)).then_some(())?;
    if unavailable_completion_context(&region.tokens, offset)
        != UnavailableCompletionContext::Argument
    {
        return None;
    }
    let context = region.bare_argument_context(source, offset)?;
    let mut callable_candidates = Vec::new();
    for index in ExternalIndexes::new(workspace_index, game_data_index).ordered() {
        callable_candidates.extend(
            exact_top_level_candidates(index, &context.callee)
                .into_iter()
                .filter(|candidate| {
                    matches!(candidate.kind, SymbolKind::Function | SymbolKind::Method)
                }),
        );
    }
    let callable_candidates = combine_completion_candidates(callable_candidates);
    let builtin_parts = builtin_callable_fact(&context.callee).map(|fact| fact.signature_parts());
    if start.elapsed() > LOCAL_SCOPE_QUERY_DEADLINE {
        return None;
    }
    if callable_candidates.is_empty() && builtin_parts.is_none() {
        let labels =
            BoundedCompletionFacts::callable_parameter_names(source, &region, &context.callee)?;
        return (start.elapsed() <= LOCAL_SCOPE_QUERY_DEADLINE).then_some(
            bounded_argument_label_report(source, context, labels, start),
        );
    }
    let indexed_candidates = if builtin_parts.is_some() {
        &[][..]
    } else {
        callable_candidates.as_slice()
    };
    let parameter_candidates = parameter_label_candidates_for_callables(
        indexed_candidates,
        builtin_parts.as_ref(),
        &context,
    );
    let edit_range = range_for_span(source, context.prefix_span);
    let empty_local_index = SymbolIndex::default();
    let (items, source_kind_counts, origin_counts) = completion_items_for_parameter_labels(
        &parameter_candidates,
        edit_range,
        CompletionRenderContext::new(&empty_local_index, workspace_index, game_data_index),
    );
    let (items, is_incomplete) = cap_completion_items(items);
    (start.elapsed() <= LOCAL_SCOPE_QUERY_DEADLINE).then_some(LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics: 0,
        completion_context: "argument-label".to_string(),
        receiver_text: None,
        owner_type: None,
        prefix: context.prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings: LspCompletionTimings {
            total: start.elapsed(),
            ..LspCompletionTimings::default()
        },
    })
}

fn is_simple_current_receiver(receiver: &str) -> bool {
    let mut chars = receiver.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// A source-free foreground view.  The window starts at a line boundary, so
/// lexer spans can be translated back to the immutable snapshot without ever
/// traversing its root syntax tree.
struct BoundedCompletionRegion {
    tokens: Vec<Token>,
}

struct CurrentOverrideCompletionContext {
    base_type: String,
    modded: bool,
    prefix: String,
    prefix_span: TextSpan,
    typed_modifiers: BTreeSet<String>,
}

struct CurrentSuperCallCompletionContext {
    base_type: String,
    modded: bool,
    method_name: String,
    prefix: String,
    prefix_span: TextSpan,
}

#[derive(Clone)]
struct ClassSuperContext {
    owner: String,
    modded: bool,
}

impl BoundedCompletionRegion {
    fn new(source: &str, offset: usize) -> Option<Self> {
        let (start, end) = Self::window_bounds(source, offset)?;
        Self::from_window(source, start, end)
            .or_else(|| Self::from_current_snapshot_context(source, start, end))
    }

    fn for_current_override(source: &str, offset: usize) -> Option<Self> {
        let (_, end) = Self::window_bounds(source, offset)?;
        let mut start = offset.saturating_sub(LOCAL_SCOPE_QUERY_MAX_WINDOW_BYTES);
        while start < offset && !source.is_char_boundary(start) {
            start += 1;
        }
        // The normal local query begins after this line because it only needs
        // callable-local facts. Override recovery must retain the class header,
        // so it extends to the preceding line boundary while preserving the
        // same fixed before-cursor budget.
        start = source
            .get(..start)?
            .rfind('\n')
            .map(|newline| newline + 1)
            .unwrap_or(0);
        (offset.saturating_sub(start) <= OVERRIDE_QUERY_MAX_WINDOW_BYTES).then_some(())?;
        Self::from_window(source, start, end)
    }

    fn window_bounds(source: &str, offset: usize) -> Option<(usize, usize)> {
        if offset > source.len() || !source.is_char_boundary(offset) {
            return None;
        }
        let mut start = offset.saturating_sub(LOCAL_SCOPE_QUERY_MAX_WINDOW_BYTES);
        while start < offset && !source.is_char_boundary(start) {
            start += 1;
        }
        if let Some(newline) = source.get(start..offset)?.find('\n') {
            start += newline + 1;
        }
        let mut end = (offset + LOCAL_SCOPE_QUERY_TRAILING_BYTES).min(source.len());
        while end > offset && !source.is_char_boundary(end) {
            end -= 1;
        }
        Some((start, end))
    }

    fn from_window(source: &str, start: usize, end: usize) -> Option<Self> {
        let mut tokens = lex(source.get(start..end)?);
        for token in &mut tokens {
            token.span.start += start;
            token.span.end += start;
        }
        (!tokens.iter().any(|token| token.kind.is_error())).then_some(Self { tokens })
    }

    fn from_current_snapshot_context(source: &str, start: usize, end: usize) -> Option<Self> {
        (source.len() <= LEXICAL_CONTEXT_RECOVERY_MAX_SOURCE_BYTES).then_some(())?;
        let mut tokens = lex(source);
        tokens.retain(|token| token.span.start < end && token.span.end > start);
        (!tokens.iter().any(|token| token.kind.is_error())).then_some(Self { tokens })
    }

    fn significant_before(&self, offset: usize) -> Vec<Token> {
        self.tokens
            .iter()
            .copied()
            .filter(|token| {
                token.span.end <= offset && !token.kind.is_trivia() && token.kind != TokenKind::Eof
            })
            .collect()
    }

    fn simple_member_context(
        &self,
        source: &str,
        offset: usize,
    ) -> Option<(String, TextSpan, String, TextSpan)> {
        let prefix_span = lexical_completion_prefix_span(&self.tokens, offset);
        let prefix = source.get(prefix_span.start..prefix_span.end)?.to_string();
        let tokens = self.significant_before(prefix_span.start);
        let dot = tokens.last()?;
        (dot.kind == TokenKind::Dot).then_some(())?;
        let receiver = tokens.get(tokens.len().checked_sub(2)?)?;
        (receiver.kind == TokenKind::Identifier).then_some(())?;
        let receiver_text = source
            .get(receiver.span.start..receiver.span.end)?
            .to_string();
        is_simple_current_receiver(&receiver_text).then_some((
            receiver_text,
            receiver.span,
            prefix,
            prefix_span,
        ))
    }

    fn external_call_chain_member_context(
        &self,
        source: &str,
        offset: usize,
        workspace_index: Option<&SymbolIndex>,
        game_data_index: Option<&SymbolIndex>,
    ) -> Option<(String, TextSpan, String, String, TextSpan)> {
        let prefix_span = lexical_completion_prefix_span(&self.tokens, offset);
        let prefix = source.get(prefix_span.start..prefix_span.end)?.to_string();
        let tokens = self.significant_before(prefix_span.start);
        let dot = tokens.last()?;
        (dot.kind == TokenKind::Dot).then_some(())?;

        let mut cursor = tokens.len().checked_sub(1)?;
        let mut calls = Vec::new();
        let receiver_end = tokens.get(cursor.checked_sub(1)?)?.span.end;
        loop {
            cursor = cursor.checked_sub(1)?;
            (tokens.get(cursor)?.kind == TokenKind::RightParen).then_some(())?;
            cursor = cursor.checked_sub(1)?;
            (tokens.get(cursor)?.kind == TokenKind::LeftParen).then_some(())?;
            cursor = cursor.checked_sub(1)?;
            let name = tokens.get(cursor)?;
            (name.kind == TokenKind::Identifier).then_some(())?;
            calls.push(source.get(name.span.start..name.span.end)?.to_string());

            let Some(previous) = cursor.checked_sub(1) else {
                break;
            };
            if tokens.get(previous)?.kind != TokenKind::Dot {
                break;
            }
            cursor = previous;
        }
        calls.reverse();
        // A completed external call is already a member receiver path:
        // `GetGame().GetPlayer...` must not wait for whole-file analysis.
        // Each additional `.Method()` is another proven external step.
        (!calls.is_empty()).then_some(())?;
        let receiver_start = tokens.get(cursor)?.span.start;
        let receiver_span = TextSpan::new(receiver_start, receiver_end);
        let receiver = source
            .get(receiver_span.start..receiver_span.end)?
            .to_string();
        let owner = external_call_chain_owner(
            &calls,
            [workspace_index, game_data_index].into_iter().flatten(),
        )?;
        Some((receiver, receiver_span, owner, prefix, prefix_span))
    }

    fn has_completed_call_receiver(&self, source: &str, offset: usize) -> bool {
        let prefix_span = lexical_completion_prefix_span(&self.tokens, offset);
        let tokens = self.significant_before(prefix_span.start);
        tokens
            .last()
            .is_some_and(|token| token.kind == TokenKind::Dot)
            && tokens
                .get(tokens.len().saturating_sub(2))
                .is_some_and(|token| token.kind == TokenKind::RightParen)
            && source.get(prefix_span.start..prefix_span.end).is_some()
    }

    fn bare_argument_context(
        &self,
        source: &str,
        offset: usize,
    ) -> Option<ArgumentLabelCompletionContext> {
        let (prefix, prefix_span) =
            completion_identifier_prefix_for_argument_label(source, &self.tokens, offset)?;
        let tokens = self.significant_before(prefix_span.start);
        let left_paren_index = tokens
            .iter()
            .rposition(|token| token.kind == TokenKind::LeftParen)?;
        let callee = tokens.get(left_paren_index.checked_sub(1)?)?;
        (callee.kind == TokenKind::Identifier).then_some(())?;
        let callee = source.get(callee.span.start..callee.span.end)?.to_string();
        is_simple_current_receiver(&callee).then_some(())?;
        let mut supplied_labels = BTreeSet::new();
        for pair in tokens[left_paren_index + 1..].windows(2) {
            if pair[0].kind == TokenKind::Identifier && pair[1].kind == TokenKind::Colon {
                supplied_labels.insert(
                    source
                        .get(pair[0].span.start..pair[0].span.end)?
                        .to_string(),
                );
            }
        }
        Some(ArgumentLabelCompletionContext {
            prefix,
            prefix_span,
            target: CallableTarget::Call {
                callee_span: TextSpan::new(0, 0),
            },
            argument_index: tokens[left_paren_index + 1..]
                .iter()
                .filter(|token| token.kind == TokenKind::Comma)
                .count(),
            supplied_labels,
            callee,
        })
    }

    fn current_override_context(
        &self,
        source: &str,
        offset: usize,
    ) -> Option<CurrentOverrideCompletionContext> {
        let prefix_span = lexical_completion_prefix_span(&self.tokens, offset);
        let prefix = source.get(prefix_span.start..prefix_span.end)?.to_string();
        (!prefix.is_empty()).then_some(())?;
        let tokens = self.significant_before(prefix_span.start);
        let previous = tokens.last().map(|token| token.kind);
        is_declaration_boundary(previous).then_some(())?;

        let mut class_bases = class_super_contexts(source, &tokens)?;

        let mut scopes = Vec::new();
        for token in tokens {
            match token.kind {
                TokenKind::LeftBrace => scopes.push(class_bases.remove(&token.span.start)),
                TokenKind::RightBrace => {
                    scopes.pop()?;
                }
                _ => {}
            }
        }
        let class_context = scopes.last().and_then(|base| base.clone())?;
        Some(CurrentOverrideCompletionContext {
            base_type: class_context.owner,
            modded: class_context.modded,
            prefix,
            prefix_span,
            typed_modifiers: bounded_typed_declaration_modifiers(&self.tokens, prefix_span),
        })
    }

    fn current_super_call_context(
        &self,
        source: &str,
        offset: usize,
    ) -> Option<CurrentSuperCallCompletionContext> {
        let prefix_span = lexical_completion_prefix_span(&self.tokens, offset);
        let prefix = source.get(prefix_span.start..prefix_span.end)?.to_string();
        completion_name_match_rank("super", &prefix)?;
        let tokens = self.significant_before(prefix_span.start);

        let body_open = tokens
            .iter()
            .rposition(|token| token.kind == TokenKind::LeftBrace)?;
        let before_body = &tokens[..body_open];
        (before_body.last()?.kind == TokenKind::RightParen).then_some(())?;
        let mut depth = 0usize;
        let parameters_open = before_body
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, token)| match token.kind {
                TokenKind::RightParen => {
                    depth += 1;
                    None
                }
                TokenKind::LeftParen if depth == 1 => Some(index),
                TokenKind::LeftParen => {
                    depth = depth.checked_sub(1)?;
                    None
                }
                _ => None,
            })?;
        let method = before_body.get(parameters_open.checked_sub(1)?)?;
        (method.kind == TokenKind::Identifier).then_some(())?;
        let method_name = source.get(method.span.start..method.span.end)?.to_string();

        let mut class_bases = class_super_contexts(source, &tokens)?;
        let mut scopes = Vec::new();
        for token in tokens {
            match token.kind {
                TokenKind::LeftBrace => scopes.push(class_bases.remove(&token.span.start)),
                TokenKind::RightBrace => {
                    scopes.pop()?;
                }
                _ => {}
            }
        }
        let class_context = scopes.into_iter().rev().flatten().next()?;
        Some(CurrentSuperCallCompletionContext {
            base_type: class_context.owner,
            modded: class_context.modded,
            method_name,
            prefix,
            prefix_span,
        })
    }
}

fn class_super_contexts(
    source: &str,
    tokens: &[Token],
) -> Option<BTreeMap<usize, ClassSuperContext>> {
    let mut contexts = BTreeMap::new();
    for (index, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, TokenKind::Keyword(Keyword::Class)) {
            continue;
        }
        let name = tokens.get(index + 1)?;
        if name.kind != TokenKind::Identifier {
            continue;
        }
        let header = &tokens[index + 2..];
        let body_index = header
            .iter()
            .position(|token| token.kind == TokenKind::LeftBrace)?;
        let explicit_base = header[..body_index]
            .windows(2)
            .find(|window| {
                matches!(
                    window[0].kind,
                    TokenKind::Colon | TokenKind::Keyword(Keyword::Extends)
                ) && window[1].kind == TokenKind::Identifier
            })
            .and_then(|window| source.get(window[1].span.start..window[1].span.end))
            .map(str::to_string);
        let is_modded = index.checked_sub(1).is_some_and(|previous| {
            matches!(tokens[previous].kind, TokenKind::Keyword(Keyword::Modded))
        });
        let owner = if is_modded {
            source
                .get(name.span.start..name.span.end)
                .map(str::to_string)
        } else {
            explicit_base
        };
        if let Some(owner) = owner {
            contexts.insert(
                header[body_index].span.start,
                ClassSuperContext {
                    owner,
                    modded: is_modded,
                },
            );
        }
    }
    Some(contexts)
}

fn external_call_chain_owner<'a>(
    calls: &[String],
    indexes: impl IntoIterator<Item = &'a SymbolIndex>,
) -> Option<String> {
    let indexes = indexes.into_iter().collect::<Vec<_>>();
    let first = calls.first()?;
    let mut owner = None;
    for index in &indexes {
        for id in index.preferred_from_symbols(index.functions_by_name(first)) {
            if !callable_accepts_zero_arguments(index, id) {
                continue;
            }
            let Some(symbol) = index.symbol(id) else {
                continue;
            };
            if let Some(result) = expression_type_from_index_symbol(index, symbol) {
                owner = Some(result.owner_type);
                break;
            }
        }
        if owner.is_some() {
            break;
        }
    }
    let mut owner = owner?;

    for call in calls.iter().skip(1) {
        let mut next = None;
        for index in &indexes {
            let candidates = IndexQuery::new(index).completion_members_for_class(&owner);
            for candidate in candidates.candidates {
                if candidate.name.as_deref() != Some(call.as_str()) {
                    continue;
                }
                if !callable_accepts_zero_arguments(index, candidate.id) {
                    continue;
                }
                let Some(symbol) = index.symbol(candidate.id) else {
                    continue;
                };
                if let Some(result) = expression_type_from_index_symbol(index, symbol) {
                    next = Some(result.owner_type);
                    break;
                }
            }
            if next.is_some() {
                break;
            }
        }
        owner = next?;
    }

    Some(owner)
}

fn callable_accepts_zero_arguments(index: &SymbolIndex, id: crate::index::GlobalSymbolId) -> bool {
    index.children(id).iter().all(|parameter| {
        index.symbol(*parameter).is_some_and(|symbol| {
            symbol.kind != SymbolKind::Parameter || symbol.detail.default_text.is_some()
        })
    })
}

#[derive(Clone)]
struct BoundedDeclaration {
    name: String,
    owner_type: String,
    scope: Vec<usize>,
    declared_at: usize,
}

struct BoundedCompletionFacts {
    declarations: Vec<BoundedDeclaration>,
    cursor_scope: Vec<usize>,
}

impl BoundedCompletionFacts {
    fn recover(source: &str, region: &BoundedCompletionRegion, offset: usize) -> Option<Self> {
        let tokens = region.significant_before(offset);
        let mut scopes = Vec::new();
        let mut declarations = Vec::new();
        for (index, token) in tokens.iter().enumerate() {
            match token.kind {
                TokenKind::LeftBrace => scopes.push(token.span.start),
                TokenKind::RightBrace => {
                    scopes.pop()?;
                }
                _ => {}
            }
            let Some(name) = tokens.get(index + 1) else {
                continue;
            };
            if !is_bounded_type_token(token.kind) || name.kind != TokenKind::Identifier {
                continue;
            }
            let Some(next) = tokens.get(index + 2) else {
                continue;
            };
            if next.kind == TokenKind::LeftParen
                || !matches!(
                    next.kind,
                    TokenKind::Semicolon
                        | TokenKind::Comma
                        | TokenKind::Operator(_)
                        | TokenKind::RightParen
                )
            {
                continue;
            }
            let owner_type = source.get(token.span.start..token.span.end)?.to_string();
            let name_text = source.get(name.span.start..name.span.end)?.to_string();
            let mut declaration_scope = scopes.clone();
            // A parameter belongs to the following callable body, not the
            // enclosing class.  Requiring that body inside the window avoids
            // leaking parameters into sibling methods.
            if next.kind == TokenKind::Comma || next.kind == TokenKind::RightParen {
                let body = tokens[index + 2..]
                    .iter()
                    .find(|candidate| candidate.kind == TokenKind::LeftBrace)?;
                declaration_scope.push(body.span.start);
            }
            declarations.push(BoundedDeclaration {
                name: name_text,
                owner_type,
                scope: declaration_scope,
                declared_at: name.span.start,
            });
        }
        // A callable-local query is exact only with a proven enclosing body
        // and its closing delimiter inside the same bounded region.  This
        // deliberately declines an in-progress unterminated body rather than
        // guessing from a partial file projection.
        (!scopes.is_empty()
            && region
                .tokens
                .iter()
                .any(|token| token.span.start >= offset && token.kind == TokenKind::RightBrace))
        .then_some(Self {
            declarations,
            cursor_scope: scopes,
        })
    }

    fn visible_declarations(&self) -> Vec<&BoundedDeclaration> {
        let mut latest = BTreeMap::<String, &BoundedDeclaration>::new();
        for declaration in &self.declarations {
            if declaration.scope.len() <= self.cursor_scope.len()
                && self.cursor_scope.starts_with(&declaration.scope)
            {
                latest
                    .entry(declaration.name.clone())
                    .and_modify(|current| {
                        if declaration.scope.len() > current.scope.len()
                            || (declaration.scope.len() == current.scope.len()
                                && declaration.declared_at > current.declared_at)
                        {
                            *current = declaration;
                        }
                    })
                    .or_insert(declaration);
            }
        }
        latest.into_values().collect()
    }

    fn visible_type(&self, name: &str) -> Option<String> {
        self.visible_declarations()
            .into_iter()
            .find(|declaration| declaration.name == name)
            .map(|declaration| declaration.owner_type.clone())
    }

    fn local_completion_report(
        &self,
        source: &str,
        region: &BoundedCompletionRegion,
        offset: usize,
        workspace_index: Option<&SymbolIndex>,
        game_data_index: Option<&SymbolIndex>,
        start: Instant,
    ) -> LspCompletionReport {
        let prefix_span = lexical_completion_prefix_span(&region.tokens, offset);
        let prefix = source[prefix_span.start..prefix_span.end].to_string();
        let mut items = self
            .visible_declarations()
            .into_iter()
            .filter_map(|declaration| {
                candidate_matches_prefix_name(&declaration.name, &prefix).then_some(
                    LspCompletionItem {
                        label: declaration.name.clone(),
                        label_details: Some(LspCompletionItemLabelDetails {
                            detail: Some(format!(": {}", declaration.owner_type)),
                            description: None,
                        }),
                        kind: 6,
                        detail: Some(declaration.owner_type.clone()),
                        documentation: None,
                        sort_text: Some(format!("000:local:{}", declaration.name)),
                        filter_text: Some(declaration.name.clone()),
                        preselect: None,
                        insert_text_format: None,
                        commit_characters: None,
                        command: None,
                        text_edit: LspTextEdit {
                            range: range_for_span(source, prefix_span),
                            new_text: declaration.name.clone(),
                            replace_range: None,
                        },
                        required_parameter_count: 0,
                        optional_parameter_count: 0,
                    },
                )
            })
            .collect::<Vec<_>>();
        // Local facts are exact, but ordinary completion also retains the
        // independently valid external/keyword candidates from this same
        // lexical window.  This is a merge of current bounded facts, never a
        // projection of the document snapshot.
        let fallback = lexical_top_level_fallback_report(
            source,
            &region.tokens,
            offset,
            workspace_index,
            game_data_index,
            UnavailableCompletionContext::TopLevel,
        );
        let mut fallback = fallback;
        let fallback_is_incomplete = fallback.list.is_incomplete;
        // An argument expression is still an unqualified value position. The
        // current snapshot proves the containing class/base and its directly
        // declared members; immutable indexes prove inherited members. Keep
        // those facts while analysis is pending instead of dropping them to
        // the global-only fallback.
        self.append_current_unqualified_value_items(
            source,
            region,
            offset,
            &prefix,
            prefix_span,
            workspace_index,
            game_data_index,
            &mut fallback,
        );
        items.extend(fallback.list.items);
        let mut seen = BTreeSet::new();
        items.retain(|item| {
            seen.insert((
                item.label.to_ascii_lowercase(),
                item.text_edit.new_text.clone(),
            ))
        });
        add_return_separator_to_completion_items(source, prefix_span, &mut items);
        let (items, is_incomplete) =
            cap_completion_items_with_upstream_incomplete(items, fallback_is_incomplete);
        LspCompletionReport {
            candidate_count: items.len(),
            list: LspCompletionList {
                is_incomplete,
                items,
            },
            query_quality: QueryQuality::Exact,
            recovery_reason: None,
            parse_diagnostics: 0,
            completion_context: "local".to_string(),
            receiver_text: None,
            owner_type: None,
            prefix,
            source_kind_counts: fallback.source_kind_counts,
            origin_counts: fallback.origin_counts,
            failure_reason: None,
            timings: LspCompletionTimings {
                total: start.elapsed(),
                ..LspCompletionTimings::default()
            },
        }
    }

    fn append_current_member_items(
        &self,
        source: &str,
        region: &BoundedCompletionRegion,
        owner: &str,
        prefix: &str,
        prefix_span: TextSpan,
        report: &mut LspCompletionReport,
    ) {
        let mut items = Self::class_member_names(source, region, owner)
            .into_iter()
            .filter(|name| candidate_matches_prefix_name(name, prefix))
            .map(|name| {
                bounded_completion_item(source, prefix_span, name, Some(owner.to_string()), 2)
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            return;
        }
        let report_is_incomplete = report.list.is_incomplete;
        items.extend(std::mem::take(&mut report.list.items));
        let (items, is_incomplete) =
            cap_completion_items_with_upstream_incomplete(items, report_is_incomplete);
        report.candidate_count = items.len();
        report.list = LspCompletionList {
            is_incomplete,
            items,
        };
    }

    /// Adds members that are provable from the current snapshot's containing
    /// class and from immutable external base-class indexes. This is valid for
    /// any unqualified value expression, including a function argument, and
    /// deliberately never reads a previous document analysis.
    #[allow(clippy::too_many_arguments)]
    fn append_current_unqualified_value_items(
        &self,
        source: &str,
        region: &BoundedCompletionRegion,
        offset: usize,
        prefix: &str,
        prefix_span: TextSpan,
        workspace_index: Option<&SymbolIndex>,
        game_data_index: Option<&SymbolIndex>,
        report: &mut LspCompletionReport,
    ) {
        let Some((class_name, base_type)) = self.enclosing_class_context(source, region, offset)
        else {
            return;
        };

        let mut items = Self::class_member_names(source, region, &class_name)
            .into_iter()
            .filter(|name| candidate_matches_prefix_name(name, prefix))
            .map(|name| {
                bounded_completion_item(source, prefix_span, name, Some(class_name.clone()), 2)
            })
            .collect::<Vec<_>>();

        if let Some(base_type) = base_type {
            let mut inherited = Vec::new();
            for owner in external_completion_owners(&base_type, workspace_index, game_data_index) {
                if let Some(index) = workspace_index {
                    inherited.extend(prefixed_candidates(
                        completion_candidates_for_owner(index, &owner, false),
                        prefix,
                    ));
                }
                if let Some(index) = game_data_index {
                    inherited.extend(prefixed_candidates(
                        completion_candidates_for_owner(index, &owner, false),
                        prefix,
                    ));
                }
            }
            let inherited = combine_completion_candidates(inherited);
            let empty_local_index = SymbolIndex::default();
            let (inherited_items, source_counts, origin_counts) = completion_items_for_candidates(
                &inherited,
                range_for_span(source, prefix_span),
                None,
                CompletionInsertContext::Normal,
                Some(prefix),
                CompletionRenderContext::new(&empty_local_index, workspace_index, game_data_index),
            );
            merge_count_maps(&mut report.source_kind_counts, source_counts);
            merge_count_maps(&mut report.origin_counts, origin_counts);
            items.extend(inherited_items);
        }

        if items.is_empty() {
            return;
        }
        let report_is_incomplete = report.list.is_incomplete;
        items.extend(std::mem::take(&mut report.list.items));
        let mut seen = BTreeSet::new();
        items.retain(|item| {
            seen.insert((
                item.label.to_ascii_lowercase(),
                item.text_edit.new_text.clone(),
            ))
        });
        let (items, is_incomplete) =
            cap_completion_items_with_upstream_incomplete(items, report_is_incomplete);
        report.candidate_count = items.len();
        report.list = LspCompletionList {
            is_incomplete,
            items,
        };
    }

    /// Adds only facts proved from the current bounded snapshot to a pending
    /// static-enum expression. This keeps the snippet-triggered request
    /// useful before whole-file analysis finishes without joining the current
    /// text to local declarations from an older revision.
    #[allow(clippy::too_many_arguments)]
    fn append_current_enum_value_items(
        &self,
        source: &str,
        region: &BoundedCompletionRegion,
        offset: usize,
        enum_owner: &str,
        receiver_span: TextSpan,
        prefix_span: TextSpan,
        prefix: &str,
        workspace_index: Option<&SymbolIndex>,
        game_data_index: Option<&SymbolIndex>,
        report: &mut LspCompletionReport,
    ) {
        let full_expression_span = TextSpan::new(receiver_span.start, prefix_span.end);
        let full_expression_range = range_for_span(source, full_expression_span);
        let full_expression_filter = format!("{enum_owner}.{prefix}");
        let mut items = self
            .visible_declarations()
            .into_iter()
            .filter(|declaration| candidate_matches_prefix_name(&declaration.name, ""))
            .map(|declaration| {
                bounded_completion_item(
                    source,
                    full_expression_span,
                    declaration.name.clone(),
                    Some(declaration.owner_type.clone()),
                    6,
                )
            })
            .collect::<Vec<_>>();

        if let Some((class_name, base_type)) = self.enclosing_class_context(source, region, offset)
        {
            items.extend(
                Self::class_member_names(source, region, &class_name)
                    .into_iter()
                    .map(|name| {
                        bounded_completion_item(
                            source,
                            full_expression_span,
                            name,
                            Some(class_name.clone()),
                            2,
                        )
                    }),
            );

            if let Some(base_type) = base_type {
                let empty_local_index = SymbolIndex::default();
                let render_context = CompletionRenderContext::new(
                    &empty_local_index,
                    workspace_index,
                    game_data_index,
                );
                let mut inherited = Vec::new();
                for owner in
                    external_completion_owners(&base_type, workspace_index, game_data_index)
                {
                    if let Some(index) = workspace_index {
                        inherited.extend(completion_candidates_for_owner(index, &owner, false));
                    }
                    if let Some(index) = game_data_index {
                        inherited.extend(completion_candidates_for_owner(index, &owner, false));
                    }
                }
                let inherited = combine_completion_candidates(inherited);
                let (inherited_items, source_counts, origin_counts) =
                    completion_items_for_candidates(
                        &inherited,
                        full_expression_range,
                        None,
                        CompletionInsertContext::Normal,
                        Some(""),
                        render_context,
                    );
                merge_count_maps(&mut report.source_kind_counts, source_counts);
                merge_count_maps(&mut report.origin_counts, origin_counts);
                items.extend(inherited_items);
            }
        }

        if items.is_empty() {
            return;
        }
        for (index, item) in items.iter_mut().enumerate() {
            item.filter_text = Some(full_expression_filter.clone());
            item.sort_text = Some(format!("100:enum-fallback:{index:03}:{}", item.label));
        }
        let report_is_incomplete = report.list.is_incomplete;
        items.extend(std::mem::take(&mut report.list.items));
        for (index, item) in items.iter_mut().enumerate() {
            if item.kind == 14 {
                // The initial static-enum report already contains keywords.
                // They are valid fallbacks, but the immediate current-snapshot
                // values recovered above must outrank them.
                item.sort_text = Some(format!("900:enum-keyword:{index:03}:{}", item.label));
            }
        }
        let mut seen = BTreeSet::new();
        items.retain(|item| {
            seen.insert((
                item.label.to_ascii_lowercase(),
                item.text_edit.new_text.clone(),
            ))
        });
        add_return_separator_to_completion_items(source, prefix_span, &mut items);
        let (items, is_incomplete) =
            cap_completion_items_with_upstream_incomplete(items, report_is_incomplete);
        report.candidate_count = items.len();
        report.list = LspCompletionList {
            is_incomplete,
            items,
        };
    }

    fn enclosing_class_context(
        &self,
        source: &str,
        region: &BoundedCompletionRegion,
        offset: usize,
    ) -> Option<(String, Option<String>)> {
        let tokens = region.significant_before(offset);
        let mut class_bodies = BTreeMap::new();
        for (index, token) in tokens.iter().enumerate() {
            if !matches!(token.kind, TokenKind::Keyword(Keyword::Class))
                || tokens.get(index + 1)?.kind != TokenKind::Identifier
            {
                continue;
            }
            let name = source
                .get(tokens[index + 1].span.start..tokens[index + 1].span.end)?
                .to_string();
            let header = &tokens[index + 2..];
            let body_index = header
                .iter()
                .position(|token| token.kind == TokenKind::LeftBrace)?;
            let base_type = header[..body_index]
                .windows(2)
                .find(|window| {
                    window[0].kind == TokenKind::Colon && window[1].kind == TokenKind::Identifier
                })
                .and_then(|window| source.get(window[1].span.start..window[1].span.end))
                .map(str::to_string);
            class_bodies.insert(header[body_index].span.start, (name, base_type));
        }
        self.cursor_scope
            .iter()
            .rev()
            .find_map(|scope_start| class_bodies.remove(scope_start))
    }

    fn class_member_names(
        source: &str,
        region: &BoundedCompletionRegion,
        owner: &str,
    ) -> Vec<String> {
        let tokens = region
            .tokens
            .iter()
            .copied()
            .filter(|token| !token.kind.is_trivia() && token.kind != TokenKind::Eof)
            .collect::<Vec<_>>();
        let class_index = tokens.windows(2).position(|window| {
            matches!(window[0].kind, TokenKind::Keyword(Keyword::Class))
                && source.get(window[1].span.start..window[1].span.end) == Some(owner)
        });
        let Some(class_index) = class_index else {
            return Vec::new();
        };
        let Some(body_offset) = tokens[class_index + 2..]
            .iter()
            .position(|token| token.kind == TokenKind::LeftBrace)
        else {
            return Vec::new();
        };
        let body_index = class_index + 2 + body_offset;
        let mut depth = 1usize;
        let mut names = Vec::new();
        for window in tokens[body_index + 1..].windows(3) {
            match window[0].kind {
                TokenKind::LeftBrace => depth += 1,
                TokenKind::RightBrace => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                _ if depth == 1
                    && is_bounded_type_token(window[0].kind)
                    && window[1].kind == TokenKind::Identifier
                    && window[2].kind == TokenKind::LeftParen =>
                {
                    if let Some(name) = source.get(window[1].span.start..window[1].span.end) {
                        names.push(name.to_string());
                    }
                }
                _ => {}
            }
        }
        names
    }

    fn callable_parameter_names(
        source: &str,
        region: &BoundedCompletionRegion,
        callee: &str,
    ) -> Option<Vec<String>> {
        let tokens = region.significant_before(source.len());
        let callable = tokens.windows(3).find(|window| {
            is_bounded_type_token(window[0].kind)
                && window[1].kind == TokenKind::Identifier
                && source.get(window[1].span.start..window[1].span.end) == Some(callee)
                && window[2].kind == TokenKind::LeftParen
        })?;
        let open = tokens
            .iter()
            .position(|token| token.span == callable[2].span)?;
        let close = tokens[open + 1..]
            .iter()
            .position(|token| token.kind == TokenKind::RightParen)?
            + open
            + 1;
        let mut names = Vec::new();
        for window in tokens[open + 1..close].windows(2) {
            if is_bounded_type_token(window[0].kind) && window[1].kind == TokenKind::Identifier {
                names.push(
                    source
                        .get(window[1].span.start..window[1].span.end)?
                        .to_string(),
                );
            }
        }
        Some(names)
    }
}

fn bounded_completion_item(
    source: &str,
    prefix_span: TextSpan,
    label: String,
    detail: Option<String>,
    kind: u32,
) -> LspCompletionItem {
    LspCompletionItem {
        label: label.clone(),
        label_details: detail.as_ref().map(|detail| LspCompletionItemLabelDetails {
            detail: Some(format!(": {detail}")),
            description: None,
        }),
        kind,
        detail,
        documentation: None,
        sort_text: Some(format!("000:bounded:{label}")),
        filter_text: Some(label.clone()),
        preselect: None,
        insert_text_format: None,
        commit_characters: None,
        command: None,
        text_edit: LspTextEdit {
            range: range_for_span(source, prefix_span),
            new_text: label,
            replace_range: None,
        },
        required_parameter_count: 0,
        optional_parameter_count: 0,
    }
}

fn bounded_argument_label_report(
    source: &str,
    context: ArgumentLabelCompletionContext,
    labels: Vec<String>,
    start: Instant,
) -> LspCompletionReport {
    let mut items = labels
        .into_iter()
        .filter(|label| starts_with_ignore_ascii_case(label, &context.prefix))
        .filter(|label| {
            !context
                .supplied_labels
                .iter()
                .any(|supplied| supplied.eq_ignore_ascii_case(label))
        })
        .map(|label| bounded_completion_item(source, context.prefix_span, label, None, 5))
        .collect::<Vec<_>>();
    let (items, is_incomplete) = cap_completion_items(std::mem::take(&mut items));
    LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics: 0,
        completion_context: "argument-label".to_string(),
        receiver_text: None,
        owner_type: None,
        prefix: context.prefix,
        source_kind_counts: BTreeMap::new(),
        origin_counts: BTreeMap::new(),
        failure_reason: None,
        timings: LspCompletionTimings {
            total: start.elapsed(),
            ..LspCompletionTimings::default()
        },
    }
}

fn is_bounded_type_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(
                Keyword::Int
                    | Keyword::Void
                    | Keyword::Float
                    | Keyword::Bool
                    | Keyword::String
                    | Keyword::Vector
                    | Keyword::Typename
                    | Keyword::Auto
            )
    )
}

fn candidate_matches_prefix_name(name: &str, prefix: &str) -> bool {
    completion_name_match_rank(name, prefix).is_some()
}

fn is_declaration_boundary(previous: Option<TokenKind>) -> bool {
    previous.is_none_or(|kind| match kind {
        TokenKind::LeftBrace | TokenKind::RightBrace | TokenKind::Semicolon => true,
        TokenKind::Keyword(keyword) => matches!(
            keyword,
            Keyword::Class
                | Keyword::Modded
                | Keyword::Sealed
                | Keyword::Typedef
                | Keyword::Proto
                | Keyword::External
                | Keyword::Native
                | Keyword::Private
                | Keyword::Protected
                | Keyword::Static
                | Keyword::Override
                | Keyword::Const
                | Keyword::Ref
                | Keyword::Out
                | Keyword::Inout
                | Keyword::Notnull
                | Keyword::Autoptr
                | Keyword::Owned
                | Keyword::Event
        ),
        _ => false,
    })
}

fn bounded_typed_declaration_modifiers(
    tokens: &[Token],
    prefix_span: TextSpan,
) -> BTreeSet<String> {
    let mut modifiers = BTreeSet::new();
    for token in tokens.iter().rev().filter(|token| {
        token.span.end <= prefix_span.start
            && !token.kind.is_trivia()
            && token.kind != TokenKind::Eof
    }) {
        match token.kind {
            TokenKind::LeftBrace | TokenKind::RightBrace | TokenKind::Semicolon => break,
            TokenKind::Keyword(_) => {
                // The bounded query only needs to suppress duplicate modifiers;
                // all other modifier interpretation remains in the shared
                // override renderer.
                let text = match token.kind {
                    TokenKind::Keyword(Keyword::Override) => "override",
                    TokenKind::Keyword(Keyword::Protected) => "protected",
                    TokenKind::Keyword(Keyword::Const) => "const",
                    TokenKind::Keyword(Keyword::Notnull) => "notnull",
                    _ => break,
                };
                modifiers.insert(text.to_string());
            }
            _ => break,
        }
    }
    modifiers
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnavailableCompletionContext {
    TopLevel,
    Member,
    Argument,
}

impl UnavailableCompletionContext {
    const fn reason(self) -> &'static str {
        match self {
            Self::TopLevel => "current-revision-local-facts-pending",
            Self::Member => "current-revision-receiver-facts-pending",
            Self::Argument => "current-revision-argument-facts-pending",
        }
    }
}

fn unavailable_completion_report(
    mut report: LspCompletionReport,
    reason: impl Into<String>,
) -> LspCompletionReport {
    // This response is safe to show while current-document analysis catches
    // up, but it can lack the signature used to render a callable snippet.
    // It must therefore stay refreshable even when it is below the item cap.
    report.list.is_incomplete = true;
    report.query_quality = QueryQuality::Unavailable;
    report.recovery_reason = Some(reason.into());
    report
}

fn unavailable_completion_context(
    tokens: &[crate::lexer::Token],
    offset: usize,
) -> UnavailableCompletionContext {
    let prefix_span = lexical_completion_prefix_span(tokens, offset);
    let previous = previous_significant_completion_token_before_span(tokens, prefix_span);
    if previous.is_some_and(|token| token.kind == TokenKind::Dot) {
        return UnavailableCompletionContext::Member;
    }
    if previous.is_some_and(|token| matches!(token.kind, TokenKind::LeftParen | TokenKind::Comma)) {
        return UnavailableCompletionContext::Argument;
    }
    UnavailableCompletionContext::TopLevel
}

fn completion_cursor_is_in_comment_or_string(tokens: &[Token], offset: usize) -> bool {
    tokens.iter().any(|token| {
        token.span.start < offset
            && offset <= token.span.end
            && matches!(
                token.kind,
                TokenKind::LineComment
                    | TokenKind::DocLineComment
                    | TokenKind::BlockComment
                    | TokenKind::DocBlockComment
                    | TokenKind::UnterminatedBlockComment
                    | TokenKind::String
                    | TokenKind::UnterminatedString
            )
    })
}

/// The normal bounded region intentionally rejects lexical errors. For an
/// unfinished block comment in a bounded-size snapshot, use the same current
/// snapshot recovery budget solely to suppress completion rather than emit a
/// top-level fallback from comment text.
fn current_snapshot_cursor_is_in_comment_or_string(source: &str, offset: usize) -> bool {
    source.len() <= LEXICAL_CONTEXT_RECOVERY_MAX_SOURCE_BYTES
        && completion_cursor_is_in_comment_or_string(&lex(source), offset)
}

fn lexical_completion_prefix_span(tokens: &[crate::lexer::Token], offset: usize) -> TextSpan {
    tokens
        .iter()
        .find(|token| {
            token.kind == TokenKind::Identifier
                && token.span.start <= offset
                && offset <= token.span.end
        })
        .map(|token| TextSpan::new(token.span.start, offset))
        .unwrap_or_else(|| TextSpan::new(offset, offset))
}

/// Returns the identifier immediately before the current cursor without
/// attempting to derive lexical context. This is only used after the bounded
/// lexer has rejected its arbitrary source window, so it must not be used to
/// infer local scope, member ownership, or argument facts.
fn raw_completion_prefix_span(source: &str, offset: usize) -> TextSpan {
    let Some(before_cursor) = source.get(..offset) else {
        return TextSpan::new(offset, offset);
    };
    let start = before_cursor
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            (!matches!(character, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_'))
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    let Some(prefix) = source.get(start..offset) else {
        return TextSpan::new(offset, offset);
    };
    prefix
        .chars()
        .next()
        .is_some_and(|character| matches!(character, 'a'..='z' | 'A'..='Z' | '_'))
        .then_some(TextSpan::new(start, offset))
        .unwrap_or_else(|| TextSpan::new(offset, offset))
}

fn lexical_top_level_fallback_report(
    source: &str,
    tokens: &[Token],
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    context: UnavailableCompletionContext,
) -> LspCompletionReport {
    top_level_fallback_report_for_prefix_span(
        source,
        lexical_completion_prefix_span(tokens, offset),
        workspace_index,
        game_data_index,
        context,
    )
}

fn top_level_fallback_report_for_prefix_span(
    source: &str,
    prefix_span: TextSpan,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    context: UnavailableCompletionContext,
) -> LspCompletionReport {
    let declaration_context = declaration_keyword_context(source, prefix_span.start);
    let mode = if declaration_context
        || empty_generic_type_slot_before_offset_with_indexes(
            source,
            prefix_span.end,
            None,
            workspace_index,
            game_data_index,
        ) {
        EditorTopLevelCompletionMode::Type
    } else {
        EditorTopLevelCompletionMode::Value
    };
    top_level_fallback_report_for_prefix_span_with_mode(
        source,
        prefix_span,
        workspace_index,
        game_data_index,
        context,
        mode,
    )
}

fn top_level_fallback_report_for_prefix_span_with_mode(
    source: &str,
    prefix_span: TextSpan,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    context: UnavailableCompletionContext,
    mode: EditorTopLevelCompletionMode,
) -> LspCompletionReport {
    let total_start = Instant::now();
    let declaration_context = declaration_keyword_context(source, prefix_span.start);
    let prefix = source
        .get(prefix_span.start..prefix_span.end)
        .unwrap_or_default()
        .to_string();
    let empty_local_index = SymbolIndex::default();
    let candidates = top_level_source_completion_candidates(
        &prefix,
        mode,
        &empty_local_index,
        workspace_index,
        game_data_index,
        // The fallback is the list VS Code receives while current-document
        // analysis is pending. It must retain the same useful candidate
        // horizon as a normal response, otherwise VS Code treats a smaller
        // list as final and cannot discover a later prefix match as the user
        // keeps typing.
        MAX_COMPLETION_ITEMS + 1,
    );
    let render_context =
        CompletionRenderContext::new(&empty_local_index, workspace_index, game_data_index);
    let insert_context = completion_insert_context(source, prefix_span.start, mode);
    let (mut items, source_kind_counts, origin_counts) = completion_items_for_candidates(
        &candidates,
        range_for_span(source, prefix_span),
        None,
        insert_context,
        Some(&prefix),
        render_context,
    );
    if mode == EditorTopLevelCompletionMode::Type || declaration_context {
        apply_tuple_type_snippets(&mut items);
    }
    if mode == EditorTopLevelCompletionMode::Type {
        rank_indexed_type_completion_items(&mut items, &prefix);
    }
    let mut keyword_items = keyword_completion_items(
        &prefix,
        range_for_span(source, prefix_span),
        mode,
        declaration_context,
    );
    if mode == EditorTopLevelCompletionMode::Type {
        keyword_items.retain(|item| item.label != "void");
    }
    if !keyword_items.is_empty() {
        remove_items_shadowed_by_keywords(&mut items, &keyword_items);
        items.extend(keyword_items);
    }
    if mode == EditorTopLevelCompletionMode::Value {
        add_return_separator_to_completion_items(source, prefix_span, &mut items);
    }
    let (items, is_incomplete) = cap_completion_items(items);
    let mut report = LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics: 0,
        completion_context: if mode == EditorTopLevelCompletionMode::Type {
            "type".to_string()
        } else {
            "top-level".to_string()
        },
        receiver_text: None,
        owner_type: None,
        prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings: LspCompletionTimings {
            total: total_start.elapsed(),
            ..LspCompletionTimings::default()
        },
    };
    report.completion_context = if mode == EditorTopLevelCompletionMode::Type {
        "type".to_string()
    } else {
        match context {
            UnavailableCompletionContext::Member => "member-unavailable-top-level-fallback",
            UnavailableCompletionContext::Argument => "argument-unavailable-top-level-fallback",
            UnavailableCompletionContext::TopLevel => "top-level",
        }
        .to_string()
    };
    report.receiver_text = None;
    report.owner_type = None;
    report.failure_reason = Some(context.reason().to_string());
    report
}

pub(crate) fn completion_debug_markdown(
    report: &LspCompletionReport,
    uri: &str,
    bytes: usize,
    revision: u64,
    external_index_status: &str,
) -> String {
    let mut output = String::new();
    output.push_str("# Reforger Completion Debug\n\n");
    output.push_str("## Request\n\n");
    output.push_str(&format!("- URI: `{}`\n", escape_markdown_cell(uri)));
    output.push_str(&format!("- Bytes: `{bytes}`\n"));
    output.push_str(&format!("- Revision: `{revision}`\n"));
    output.push_str("- Cached Analysis: `true`\n");
    output.push_str(&format!("- Query Quality: `{:?}`\n", report.query_quality));
    output.push_str(&format!(
        "- Recovery Reason: `{}`\n",
        escape_markdown_cell(report.recovery_reason.as_deref().unwrap_or("<none>"))
    ));
    output.push_str(&format!(
        "- External Index Status: `{}`\n",
        escape_markdown_cell(external_index_status)
    ));
    output.push_str(&format!(
        "- Parse Diagnostics: `{}`\n\n",
        report.parse_diagnostics
    ));

    output.push_str("## Completion Context\n\n");
    output.push_str(&format!(
        "- Context: `{}`\n",
        escape_markdown_cell(&report.completion_context)
    ));
    output.push_str(&format!(
        "- Receiver: `{}`\n",
        escape_markdown_cell(report.receiver_text.as_deref().unwrap_or("<none>"))
    ));
    output.push_str(&format!(
        "- Owner Type: `{}`\n",
        escape_markdown_cell(report.owner_type.as_deref().unwrap_or("<none>"))
    ));
    output.push_str(&format!(
        "- Prefix: `{}`\n",
        escape_markdown_cell(&report.prefix)
    ));
    output.push_str(&format!(
        "- Candidate Count: `{}`\n",
        report.candidate_count
    ));
    output.push_str(&format!(
        "- Failure Reason: `{}`\n\n",
        escape_markdown_cell(report.failure_reason.as_deref().unwrap_or("<none>"))
    ));

    output.push_str("## Candidate Sources\n\n");
    if report.source_kind_counts.is_empty() {
        output.push_str("None.\n\n");
    } else {
        output.push_str("| Source Kind | Count |\n");
        output.push_str("| --- | ---: |\n");
        for (kind, count) in &report.source_kind_counts {
            output.push_str(&format!("| `{:?}` | {} |\n", kind, count));
        }
        output.push('\n');
    }

    output.push_str("## Candidate Origins\n\n");
    if report.origin_counts.is_empty() {
        output.push_str("None.\n\n");
    } else {
        output.push_str("| Origin | Count |\n");
        output.push_str("| --- | ---: |\n");
        for (origin, count) in &report.origin_counts {
            output.push_str(&format!(
                "| `{}` | {} |\n",
                escape_markdown_cell(origin),
                count
            ));
        }
        output.push('\n');
    }

    output.push_str("## Timings\n\n");
    output.push_str("| Phase | Milliseconds |\n");
    output.push_str("| --- | ---: |\n");
    output.push_str(&format!(
        "| Context detection | {} |\n",
        report.timings.context_detection.as_millis()
    ));
    output.push_str(&format!(
        "| Receiver inference | {} |\n",
        report.timings.receiver_inference.as_millis()
    ));
    output.push_str(&format!(
        "| Candidate lookup | {} |\n",
        report.timings.candidate_lookup.as_millis()
    ));
    output.push_str(&format!(
        "| Item rendering | {} |\n",
        report.timings.item_rendering.as_millis()
    ));
    output.push_str(&format!(
        "| Total | {} |\n\n",
        report.timings.total.as_millis()
    ));

    output.push_str("## Completion Items\n\n");
    if report.list.items.is_empty() {
        output.push_str("None.\n");
        return output;
    }

    output.push_str(
        "| # | Label | Kind | Detail | Label Details | Required | Optional | Insert Text | Command | Sort Text | Docs Preview |\n",
    );
    output.push_str("| ---: | --- | --- | --- | --- | ---: | ---: | --- | --- | --- | --- |\n");
    for (index, item) in report.list.items.iter().take(50).enumerate() {
        output.push_str(&format!(
            "| {} | `{}` | `{}` | `{}` | `{}` | {} | {} | `{}` | `{}` | `{}` | {} |\n",
            index + 1,
            escape_markdown_cell(&item.label),
            completion_lsp_kind_label(item.kind),
            escape_markdown_cell(item.detail.as_deref().unwrap_or("")),
            escape_markdown_cell(&format_label_details(item.label_details.as_ref())),
            item.required_parameter_count,
            item.optional_parameter_count,
            escape_markdown_cell(&item.text_edit.new_text),
            escape_markdown_cell(
                item.command
                    .as_ref()
                    .map(|command| command.command.as_str())
                    .unwrap_or("")
            ),
            escape_markdown_cell(item.sort_text.as_deref().unwrap_or("")),
            markdown_table_text(
                item.documentation
                    .as_ref()
                    .map(|documentation| documentation.value.as_str())
                    .unwrap_or("")
            )
        ));
    }
    if report.list.items.len() > 50 {
        output.push_str(&format!(
            "|  |  |  |  |  |  |  |  |  |  | +{} more |\n",
            report.list.items.len() - 50
        ));
    }

    output
}

fn completion_report_for_offset(
    source: &str,
    analysis: &FileIndexAnalysis,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspCompletionReport {
    if completion_cursor_is_in_comment_or_string(&analysis.syntax.lexer_tokens, offset) {
        return empty_completion_report(analysis.parse_diagnostics);
    }
    let total_start = Instant::now();
    let context_start = Instant::now();
    if let Some(context) = completion_context_at_offset(source, offset) {
        return preprocessor_completion_report(
            source,
            analysis.parse_diagnostics,
            context,
            &analysis.index,
            workspace_index,
            game_data_index,
            context_start.elapsed(),
            total_start,
        );
    }
    if let Some(new_operand) =
        new_operand_before_offset(source, &analysis.syntax.lexer_tokens, offset)
    {
        let query = ConstructionQuery::new(
            source,
            &analysis.syntax.parse,
            &analysis.syntax.lexer_tokens,
            &analysis.index,
            &analysis.scope,
            ExternalIndexes::new(workspace_index, game_data_index).ordered(),
        );
        if let Some(context) = query.context_at_new(new_operand.span) {
            let candidates = query.compatible_candidates(&context);
            let completes_typed_keyword = new_operand.replaces_typed_prefix;
            let replacement_span = if completes_typed_keyword {
                new_operand.span
            } else {
                TextSpan::new(offset, offset)
            };
            return contextual_constructor_completion_report(
                source,
                analysis.parse_diagnostics,
                context.type_text,
                candidates,
                replacement_span,
                completes_typed_keyword,
                &analysis.index,
                workspace_index,
                game_data_index,
                context_start.elapsed(),
                total_start,
            );
        }
    }
    if let Some(report) = collection_declaration_tail_completion_report(
        source,
        analysis.parse_diagnostics,
        offset,
        total_start,
    ) {
        return report;
    }
    // Once a generic argument has been completed, the parser sees the whole
    // `array<int>` type and no longer exposes a completion context before its
    // closing delimiter. Recover that exact caret shape so retyping a type
    // argument can use the normal type-ranking pipeline again.
    if let Some(prefix_span) = retyped_generic_type_argument_prefix_before_offset(source, offset) {
        let context_elapsed = context_start.elapsed();
        let prefix = source
            .get(prefix_span.start..prefix_span.end)
            .unwrap_or_default()
            .to_string();
        return top_level_completion_report_for_indexes(
            source,
            analysis.parse_diagnostics,
            "type",
            prefix,
            prefix_span,
            offset,
            EditorTopLevelCompletionMode::Type,
            analysis,
            &analysis.index,
            workspace_index,
            game_data_index,
            LspCompletionTimings {
                context_detection: context_elapsed,
                ..LspCompletionTimings::default()
            },
            total_start,
        );
    }
    // A freshly accepted supported generic snippet has an intentionally empty
    // type slot (`array<>`, `map<, >`, or `Tuple2<, >`). The incomplete parser
    // has no type node there yet, so recover that lexical shape directly and
    // use the normal type-ranking pipeline while the user chooses the argument.
    if empty_generic_type_slot_before_offset_with_indexes(
        source,
        offset,
        Some(&analysis.index),
        workspace_index,
        game_data_index,
    ) {
        let context_elapsed = context_start.elapsed();
        return top_level_completion_report_for_indexes(
            source,
            analysis.parse_diagnostics,
            "type",
            String::new(),
            TextSpan::new(offset, offset),
            offset,
            EditorTopLevelCompletionMode::Type,
            analysis,
            &analysis.index,
            workspace_index,
            game_data_index,
            LspCompletionTimings {
                context_detection: context_elapsed,
                ..LspCompletionTimings::default()
            },
            total_start,
        );
    }
    if incomplete_callable_parameter_type_before_offset(source, offset) {
        let context_elapsed = context_start.elapsed();
        let prefix_span = raw_completion_prefix_span(source, offset);
        let prefix = source
            .get(prefix_span.start..prefix_span.end)
            .unwrap_or_default()
            .to_string();
        return top_level_completion_report_for_indexes(
            source,
            analysis.parse_diagnostics,
            "type",
            prefix,
            prefix_span,
            offset,
            EditorTopLevelCompletionMode::Type,
            analysis,
            &analysis.index,
            workspace_index,
            game_data_index,
            LspCompletionTimings {
                context_detection: context_elapsed,
                ..LspCompletionTimings::default()
            },
            total_start,
        );
    }
    let resolver = ReferenceResolver::new_with_parse_scope_and_external_indexes(
        source,
        &analysis.index,
        &analysis.syntax.parse,
        &analysis.scope,
        ExternalIndexes::new(workspace_index, game_data_index).ordered(),
    );
    let mut argument_label_fallback = None;
    if let Some(context) =
        argument_label_completion_context(source, &analysis.syntax.parse.root, offset)
    {
        let context_elapsed = context_start.elapsed();
        let argument_label_report = argument_label_completion_report_for_indexes(
            source,
            analysis.parse_diagnostics,
            context,
            &resolver,
            &analysis.index,
            workspace_index,
            game_data_index,
            LspCompletionTimings {
                context_detection: context_elapsed,
                ..LspCompletionTimings::default()
            },
            total_start,
        );
        if !argument_label_report.list.items.is_empty() {
            argument_label_fallback = Some(argument_label_report);
        }
    }

    if let Some(context) = resolver
        .member_completion_context_at_offset_with_tokens(offset, &analysis.syntax.lexer_tokens)
    {
        let context_elapsed = context_start.elapsed();
        let receiver_text = Some(context.receiver.receiver_text.clone());
        let receiver_span = context.receiver.receiver_span;
        let owner_type = context.receiver.owner_type.clone();
        let receiver_is_static = context.receiver.is_static;
        let prefix = context.prefix.clone();
        let failure_reason = context.receiver.failure_reason.clone();
        let Some(owner) = owner_type.clone() else {
            return argument_label_fallback.unwrap_or_else(|| LspCompletionReport {
                list: empty_completion_list(),
                query_quality: QueryQuality::Exact,
                recovery_reason: None,
                parse_diagnostics: analysis.parse_diagnostics,
                completion_context: "member".to_string(),
                receiver_text,
                owner_type,
                prefix,
                candidate_count: 0,
                source_kind_counts: BTreeMap::new(),
                origin_counts: BTreeMap::new(),
                failure_reason,
                timings: LspCompletionTimings {
                    context_detection: context_elapsed,
                    receiver_inference: context_elapsed,
                    total: total_start.elapsed(),
                    ..LspCompletionTimings::default()
                },
            });
        };
        let visibility = member_visibility_context(
            receiver_text.as_deref(),
            &owner,
            containing_class_name(&analysis.index, offset).as_deref(),
        );
        let member_report = member_completion_report_for_indexes(
            source,
            analysis.parse_diagnostics,
            &owner,
            receiver_text,
            receiver_span,
            owner_type,
            prefix,
            context.prefix_span,
            failure_reason,
            receiver_is_static,
            visibility,
            &analysis.index,
            Some((analysis, offset)),
            workspace_index,
            game_data_index,
            LspCompletionTimings {
                context_detection: context_elapsed,
                receiver_inference: context_elapsed,
                ..LspCompletionTimings::default()
            },
            total_start,
        );
        if !member_report.list.items.is_empty() {
            return member_report;
        }
        if let Some(fallback) = argument_label_fallback {
            return fallback;
        }
        return member_report;
    }

    if let Some(report) = super_call_completion_report_for_indexes(
        source,
        analysis.parse_diagnostics,
        offset,
        &analysis.index,
        workspace_index,
        game_data_index,
        total_start,
    ) {
        return report;
    }

    let top_level_context = resolver
        .top_level_completion_context_at_offset_with_tokens(offset, &analysis.syntax.lexer_tokens);
    let context_elapsed = context_start.elapsed();
    let Some(context) = top_level_context else {
        if let Some(fallback) = argument_label_fallback {
            return fallback;
        }
        let mut report = empty_completion_report(analysis.parse_diagnostics);
        report.timings.context_detection = context_elapsed;
        report.timings.total = total_start.elapsed();
        return report;
    };
    let mode = if context.identifier_context == IdentifierContext::TypePosition {
        EditorTopLevelCompletionMode::Type
    } else {
        EditorTopLevelCompletionMode::Value
    };
    let completion_context = match mode {
        EditorTopLevelCompletionMode::Type => "type",
        EditorTopLevelCompletionMode::Value => "top-level",
    };
    // Older snippets can still select these labels before asking VS Code to
    // trigger completion. They are UI placeholders, not an intended filter:
    // retain their edit span but query types as though the slot were empty.
    let prefix = if (mode == EditorTopLevelCompletionMode::Type
        && matches!(context.prefix.as_str(), "Type" | "KeyType" | "ValueType"))
        || (mode == EditorTopLevelCompletionMode::Value && context.prefix == "Expression")
    {
        String::new()
    } else {
        context.prefix
    };

    let top_level_report = top_level_completion_report_for_indexes(
        source,
        analysis.parse_diagnostics,
        completion_context,
        prefix,
        context.prefix_span,
        offset,
        mode,
        analysis,
        &analysis.index,
        workspace_index,
        game_data_index,
        LspCompletionTimings {
            context_detection: context_elapsed,
            ..LspCompletionTimings::default()
        },
        total_start,
    );
    if let Some(fallback) = argument_label_fallback {
        return merge_argument_label_and_value_reports(fallback, top_level_report, total_start);
    }
    if !top_level_report.list.items.is_empty() {
        return top_level_report;
    }
    top_level_report
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NewOperand {
    span: TextSpan,
    replaces_typed_prefix: bool,
}

fn new_operand_before_offset(source: &str, tokens: &[Token], offset: usize) -> Option<NewOperand> {
    let operand = tokens
        .iter()
        .rev()
        .find(|token| {
            token.span.end <= offset && !token.kind.is_trivia() && token.kind != TokenKind::Eof
        })
        .copied()?;

    if operand.kind == TokenKind::Identifier && operand.span.end == offset {
        let prefix = source.get(operand.span.start..operand.span.end)?;
        return matches!(prefix, "n" | "ne").then_some(NewOperand {
            span: operand.span,
            replaces_typed_prefix: true,
        });
    }
    if operand.kind != TokenKind::Keyword(Keyword::New) || operand.span.end > offset {
        return None;
    }
    let operand_space = source.get(operand.span.end..offset)?;
    (!operand_space.contains(['\n', '\r']) && operand_space.chars().all(char::is_whitespace))
        .then_some(NewOperand {
            span: operand.span,
            replaces_typed_prefix: operand.span.end == offset,
        })
}

#[allow(clippy::too_many_arguments)]
fn contextual_constructor_completion_report(
    source: &str,
    parse_diagnostics: usize,
    contextual_type: String,
    candidates: Vec<EditorCompletionCandidate>,
    replacement_span: TextSpan,
    completes_typed_keyword: bool,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    context_detection: Duration,
    total_start: Instant,
) -> LspCompletionReport {
    let Some(owner) = callable_type_owner(&contextual_type) else {
        return empty_completion_report(parse_diagnostics);
    };
    if matches!(owner.as_str(), "array" | "set" | "map") {
        let spelling = strip_all_type_prefixes(&contextual_type).trim();
        let constructor_text = format!(
            "{}{spelling}()",
            if completes_typed_keyword { "new " } else { "" }
        );
        let item = LspCompletionItem {
            label: constructor_text.clone(),
            label_details: None,
            kind: 4,
            detail: Some("contextual collection constructor".to_string()),
            documentation: None,
            sort_text: Some(format!("000:contextual-constructor:{constructor_text}")),
            filter_text: Some(if completes_typed_keyword {
                constructor_text.clone()
            } else {
                spelling.to_string()
            }),
            preselect: Some(true),
            insert_text_format: None,
            commit_characters: None,
            command: None,
            text_edit: LspTextEdit {
                range: range_for_span(source, replacement_span),
                new_text: constructor_text,
                replace_range: None,
            },
            required_parameter_count: 0,
            optional_parameter_count: 0,
        };
        return LspCompletionReport {
            list: LspCompletionList {
                is_incomplete: false,
                items: vec![item],
            },
            query_quality: QueryQuality::Exact,
            recovery_reason: None,
            parse_diagnostics,
            completion_context: "constructor".to_string(),
            receiver_text: None,
            owner_type: Some(contextual_type),
            prefix: String::new(),
            candidate_count: 1,
            source_kind_counts: BTreeMap::new(),
            origin_counts: BTreeMap::from([("ContextualCollection".to_string(), 1)]),
            failure_reason: None,
            timings: LspCompletionTimings {
                context_detection,
                total: total_start.elapsed(),
                ..LspCompletionTimings::default()
            },
        };
    }
    let render_start = Instant::now();
    let render_context =
        CompletionRenderContext::new(local_index, workspace_index, game_data_index);
    let (mut items, source_kind_counts, origin_counts) = completion_items_for_candidates(
        &candidates,
        range_for_span(source, replacement_span),
        Some("ContextualConstructor"),
        CompletionInsertContext::ContextualConstructorCall,
        None,
        render_context,
    );
    let has_exact = items.iter().any(|item| item.label == owner);
    for (index, item) in items.iter_mut().enumerate() {
        let exact = item.label == owner;
        if completes_typed_keyword {
            item.label = format!("new {}", item.label);
            item.filter_text = Some(item.label.clone());
            item.text_edit.new_text = format!("new {}", item.text_edit.new_text);
        }
        if let Some(preview) =
            constructor_completion_preview_suffix(&item.label, &item.text_edit.new_text)
        {
            let description = item
                .label_details
                .as_ref()
                .and_then(|details| details.description.clone());
            item.label_details = Some(LspCompletionItemLabelDetails {
                detail: Some(preview),
                description,
            });
        }
        item.preselect = (exact && has_exact).then_some(true);
        item.sort_text = Some(format!(
            "{}:contextual-constructor:{index:05}:{}",
            if exact { "000" } else { "100" },
            item.label
        ));
    }
    let render_elapsed = render_start.elapsed();

    LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete: false,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: "constructor".to_string(),
        receiver_text: None,
        owner_type: Some(contextual_type),
        prefix: String::new(),
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings: LspCompletionTimings {
            context_detection,
            item_rendering: render_elapsed,
            total: total_start.elapsed(),
            ..LspCompletionTimings::default()
        },
    }
}

fn constructor_completion_preview_suffix(label: &str, insert_text: &str) -> Option<String> {
    let suffix = insert_text.strip_prefix(label)?;
    let mut preview = String::new();
    let mut rest = suffix;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("$0") {
            rest = after;
            continue;
        }
        if let Some(after_open) = rest.strip_prefix("${") {
            let colon = after_open.find(':')?;
            let after_colon = &after_open[colon + 1..];
            let close = after_colon.find('}')?;
            preview.push_str(&after_colon[..close]);
            rest = &after_colon[close + 1..];
            continue;
        }
        let ch = rest.chars().next()?;
        preview.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    Some(preview)
}

#[allow(clippy::too_many_arguments)]
fn preprocessor_completion_report(
    source: &str,
    parse_diagnostics: usize,
    context: CompletionContext,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    context_detection: Duration,
    total_start: Instant,
) -> LspCompletionReport {
    let lookup_start = Instant::now();
    let (prefix, items, source_kind_counts, origin_counts) = match context {
        CompletionContext::Directive { prefix, span } => {
            let range = range_for_span(source, span);
            let items = COMPLETION_DIRECTIVES
                .iter()
                .filter(|directive| completion_name_match_rank(directive, &prefix).is_some())
                .enumerate()
                .map(|(order, directive)| {
                    preprocessor_directive_completion_item(directive, range.clone(), &prefix, order)
                })
                .collect();
            (prefix, items, BTreeMap::new(), BTreeMap::new())
        }
        CompletionContext::Macro { prefix, span } => {
            let mut candidates =
                IndexQuery::new(local_index).completion_preprocessor_macros(&prefix);
            if let Some(index) = workspace_index {
                candidates.extend(IndexQuery::new(index).completion_preprocessor_macros(&prefix));
            }
            if let Some(index) = game_data_index {
                candidates.extend(IndexQuery::new(index).completion_preprocessor_macros(&prefix));
            }
            let (mut items, source_kind_counts, origin_counts) = completion_items_for_candidates(
                &candidates,
                range_for_span(source, span),
                Some("PreprocessorMacro"),
                CompletionInsertContext::Normal,
                Some(&prefix),
                CompletionRenderContext::new(local_index, workspace_index, game_data_index),
            );
            for (item, candidate) in items.iter_mut().zip(&candidates) {
                item.detail = Some(format!(
                    "#define {} ({})",
                    candidate.display.label,
                    candidate.source_kind.as_str()
                ));
            }
            (prefix, items, source_kind_counts, origin_counts)
        }
    };
    let is_incomplete = false;
    LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: "preprocessor".to_string(),
        receiver_text: None,
        owner_type: None,
        prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings: LspCompletionTimings {
            context_detection,
            candidate_lookup: lookup_start.elapsed(),
            total: total_start.elapsed(),
            ..LspCompletionTimings::default()
        },
    }
}

fn preprocessor_directive_completion_item(
    directive: &str,
    range: LspRange,
    prefix: &str,
    order: usize,
) -> LspCompletionItem {
    let takes_operand = matches!(directive, "define" | "ifdef" | "ifndef");
    LspCompletionItem {
        label: format!("#{directive}"),
        label_details: None,
        kind: 14,
        detail: Some("Preprocessor Directive".to_string()),
        documentation: None,
        sort_text: Some(format!(
            "{:03}:{}",
            completion_name_match_rank(directive, prefix).unwrap_or(u16::MAX),
            order
        )),
        filter_text: Some(directive.to_string()),
        preselect: None,
        insert_text_format: None,
        commit_characters: None,
        command: takes_operand.then(|| LspCommand {
            title: "Apply directive separator".to_string(),
            command: COMMAND_APPLY_DIRECTIVE_SEPARATOR.to_string(),
            arguments: Some(vec![Value::String(format!("#{directive}"))]),
        }),
        text_edit: LspTextEdit {
            range,
            new_text: directive.to_string(),
            replace_range: None,
        },
        required_parameter_count: 0,
        optional_parameter_count: 0,
    }
}

pub(crate) fn merge_argument_label_and_value_reports(
    mut argument_report: LspCompletionReport,
    value_report: LspCompletionReport,
    total_start: Instant,
) -> LspCompletionReport {
    if value_report.list.items.is_empty() {
        argument_report.timings.total = total_start.elapsed();
        return argument_report;
    }

    let has_exact_value = value_report.list.items.iter().any(|item| {
        completion_item_is_argument_value(item)
            && item.label.eq_ignore_ascii_case(&value_report.prefix)
    });
    let argument_sort_prefix = if has_exact_value { "001" } else { "000" };
    for (index, item) in argument_report.list.items.iter_mut().enumerate() {
        item.sort_text = Some(format!(
            "{argument_sort_prefix}:argument:{index:03}:{}",
            item.label
        ));
    }

    let value_is_incomplete = value_report.list.is_incomplete;
    let argument_items = argument_report.list.items;
    let value_items = value_report.list.items;
    let combined_items: Box<dyn Iterator<Item = LspCompletionItem>> = if has_exact_value {
        Box::new(value_items.into_iter().chain(argument_items))
    } else {
        Box::new(argument_items.into_iter().chain(value_items))
    };

    let mut seen = BTreeSet::new();
    let mut items = Vec::new();
    for item in combined_items {
        let key = format!(
            "{}:{}",
            item.label.to_ascii_lowercase(),
            item.text_edit.new_text
        );
        if seen.insert(key) {
            items.push(item);
        }
    }
    let (items, is_incomplete) = cap_completion_items(items);

    merge_count_maps(
        &mut argument_report.source_kind_counts,
        value_report.source_kind_counts,
    );
    merge_count_maps(
        &mut argument_report.origin_counts,
        value_report.origin_counts,
    );
    argument_report.candidate_count = items.len();
    argument_report.list = LspCompletionList {
        is_incomplete: is_incomplete || value_is_incomplete,
        items,
    };
    argument_report.timings.candidate_lookup += value_report.timings.candidate_lookup;
    argument_report.timings.item_rendering += value_report.timings.item_rendering;
    argument_report.timings.total = total_start.elapsed();
    argument_report
}

fn completion_item_is_argument_value(item: &LspCompletionItem) -> bool {
    matches!(item.kind, 2 | 3 | 5 | 6 | 12 | 14 | 21 | 22)
}

#[allow(clippy::too_many_arguments)]
fn member_completion_report_for_indexes(
    source: &str,
    parse_diagnostics: usize,
    owner: &str,
    receiver_text: Option<String>,
    receiver_span: TextSpan,
    owner_type: Option<String>,
    prefix: String,
    prefix_span: TextSpan,
    failure_reason: Option<String>,
    receiver_is_static: bool,
    visibility: MemberVisibilityContext,
    local_index: &SymbolIndex,
    value_fallback_context: Option<(&FileIndexAnalysis, usize)>,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    mut timings: LspCompletionTimings,
    total_start: Instant,
) -> LspCompletionReport {
    let lookup_start = Instant::now();
    let modded_super =
        receiver_text.as_deref() == Some("super") && preferred_class_is_modded(local_index, owner);
    let mut candidates = if modded_super {
        Vec::new()
    } else {
        completion_candidates_for_owner(local_index, owner, receiver_is_static)
    };
    if let Some(external_index) = workspace_index {
        candidates.extend(if modded_super {
            completion_candidates_for_predecessor_owner(external_index, owner, receiver_is_static)
        } else {
            completion_candidates_for_owner(external_index, owner, receiver_is_static)
        });
    }
    if let Some(external_index) = game_data_index {
        candidates.extend(if modded_super {
            completion_candidates_for_predecessor_owner(external_index, owner, receiver_is_static)
        } else {
            completion_candidates_for_owner(external_index, owner, receiver_is_static)
        });
    }
    let candidates = filter_member_candidates_by_visibility(candidates, visibility);
    let candidates = combine_completion_candidates(candidates);
    timings.candidate_lookup = lookup_start.elapsed();

    let edit_range = range_for_span(source, prefix_span);
    let render_start = Instant::now();
    let render_context =
        CompletionRenderContext::new(local_index, workspace_index, game_data_index);
    let (items, source_kind_counts, origin_counts) =
        if receiver_is_static && render_context.is_enum_owner(owner) {
            // An enum field is still an expression position. Reuse the same
            // contextual value collection as ordinary unqualified completion so
            // enum-first ranking never turns into enum-only filtering. Pending
            // receiver recovery deliberately has no whole-file analysis here and
            // therefore retains its safe external/top-level fallback only.
            let value_fallback_candidates = value_fallback_context.map_or_else(
                || {
                    top_level_source_completion_candidates(
                        "",
                        EditorTopLevelCompletionMode::Value,
                        local_index,
                        workspace_index,
                        game_data_index,
                        MAX_COMPLETION_ITEMS + 1,
                    )
                },
                |(analysis, offset)| {
                    contextual_value_completion_candidates(
                        analysis,
                        "",
                        offset,
                        local_index,
                        workspace_index,
                        game_data_index,
                    )
                },
            );
            enum_static_completion_items(
                source,
                owner,
                receiver_span,
                prefix_span,
                &prefix,
                &candidates,
                &value_fallback_candidates,
                render_context,
            )
        } else {
            completion_items_for_candidates(
                &candidates,
                edit_range,
                None,
                CompletionInsertContext::Normal,
                Some(&prefix),
                render_context,
            )
        };
    let (items, is_incomplete) = cap_completion_items(items);
    timings.item_rendering = render_start.elapsed();
    timings.total = total_start.elapsed();

    LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: "member".to_string(),
        receiver_text,
        owner_type,
        prefix,
        source_kind_counts,
        origin_counts,
        failure_reason,
        timings,
    }
}

/// Static enum values rank before normal value fallbacks, and every item
/// replaces the complete `Owner.Member` expression. A completion edit's
/// filter range and replacement range must describe the same valid VS Code
/// range contract; an insert range cannot begin inside a replace range that
/// starts earlier.
#[allow(clippy::too_many_arguments)]
fn enum_static_completion_items(
    source: &str,
    owner: &str,
    receiver_span: TextSpan,
    prefix_span: TextSpan,
    prefix: &str,
    enum_candidates: &[EditorCompletionCandidate],
    value_fallback_candidates: &[EditorCompletionCandidate],
    render_context: CompletionRenderContext<'_>,
) -> (
    Vec<LspCompletionItem>,
    BTreeMap<SourceKind, usize>,
    BTreeMap<String, usize>,
) {
    let full_expression_span = TextSpan::new(receiver_span.start, prefix_span.end);
    let full_expression_range = range_for_span(source, full_expression_span);
    let (mut enum_items, mut source_kind_counts, mut origin_counts) =
        completion_items_for_candidates(
            enum_candidates,
            full_expression_range,
            None,
            CompletionInsertContext::Normal,
            Some(prefix),
            render_context,
        );
    let full_expression_filter = format!("{owner}.{prefix}");
    for (index, item) in enum_items.iter_mut().enumerate() {
        item.label = format!("{owner}.{}", item.label);
        // The selected snippet field contains the complete expression. Give
        // all enum values the same current-expression filter text so VS Code
        // presents the enum choice set before the user types a replacement.
        item.filter_text = Some(full_expression_filter.clone());
        item.text_edit.new_text = format!("{owner}.{}", item.text_edit.new_text);
        item.sort_text = Some(format!("000:enum:{index:03}:{}", item.label));
    }

    let (mut value_items, value_source_counts, value_origins) = completion_items_for_candidates(
        value_fallback_candidates,
        range_for_span(source, full_expression_span),
        None,
        CompletionInsertContext::Normal,
        Some(""),
        render_context,
    );
    merge_count_maps(&mut source_kind_counts, value_source_counts);
    merge_count_maps(&mut origin_counts, value_origins);
    value_items.extend(keyword_completion_items(
        "",
        range_for_span(source, full_expression_span),
        EditorTopLevelCompletionMode::Value,
        false,
    ));
    for (index, item) in value_items.iter_mut().enumerate() {
        item.filter_text = Some(full_expression_filter.clone());
        item.sort_text = Some(format!("100:enum-fallback:{index:03}:{}", item.label));
    }

    enum_items.extend(value_items);
    (enum_items, source_kind_counts, origin_counts)
}

fn completion_candidates_for_owner(
    index: &SymbolIndex,
    owner: &str,
    receiver_is_static: bool,
) -> Vec<EditorCompletionCandidate> {
    let query = IndexQuery::new(index);
    if receiver_is_static {
        query.completion_static_members_for_type(owner)
    } else {
        query.completion_members_for_class(owner).candidates
    }
}

fn completion_candidates_for_predecessor_owner(
    index: &SymbolIndex,
    owner: &str,
    receiver_is_static: bool,
) -> Vec<EditorCompletionCandidate> {
    let query = IndexQuery::new(index);
    let candidates = query
        .completion_members_for_modded_predecessor(owner)
        .candidates;
    if !receiver_is_static {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate
                .display
                .modifiers
                .iter()
                .any(|modifier| modifier == "static")
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn argument_label_completion_report_for_indexes(
    source: &str,
    parse_diagnostics: usize,
    context: ArgumentLabelCompletionContext,
    resolver: &ReferenceResolver<'_, '_>,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    mut timings: LspCompletionTimings,
    total_start: Instant,
) -> LspCompletionReport {
    let lookup_start = Instant::now();
    let callable_candidates = callable_candidates_for_argument_label_context(
        &context,
        resolver,
        local_index,
        workspace_index,
        game_data_index,
    );
    let builtin_parts = match &context.target {
        CallableTarget::Attribute { name } => {
            builtin_callable_fact(name).map(|fact| fact.signature_parts())
        }
        _ => None,
    };
    let indexed_candidates = if builtin_parts.is_some() {
        &[][..]
    } else {
        callable_candidates.as_slice()
    };
    let parameter_candidates = parameter_label_candidates_for_callables(
        indexed_candidates,
        builtin_parts.as_ref(),
        &context,
    );
    timings.candidate_lookup = lookup_start.elapsed();

    let render_start = Instant::now();
    let edit_range = range_for_span(source, context.prefix_span);
    let render_context =
        CompletionRenderContext::new(local_index, workspace_index, game_data_index);
    let (items, source_kind_counts, origin_counts) =
        completion_items_for_parameter_labels(&parameter_candidates, edit_range, render_context);
    let (items, is_incomplete) = cap_completion_items(items);
    timings.item_rendering = render_start.elapsed();
    timings.total = total_start.elapsed();

    LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: "argument-label".to_string(),
        receiver_text: None,
        owner_type: None,
        prefix: context.prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: if callable_candidates.is_empty() && builtin_parts.is_none() {
            Some("callable target was not resolved".to_string())
        } else {
            None
        },
        timings,
    }
}

fn callable_candidates_for_argument_label_context(
    context: &ArgumentLabelCompletionContext,
    resolver: &ReferenceResolver<'_, '_>,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Vec<EditorCompletionCandidate> {
    match &context.target {
        CallableTarget::Attribute { name } | CallableTarget::New { type_name: name } => {
            let mut candidates = exact_top_level_candidates(local_index, name);
            if let Some(index) = workspace_index {
                candidates.extend(exact_top_level_candidates(index, name));
            }
            if let Some(index) = game_data_index {
                candidates.extend(exact_top_level_candidates(index, name));
            }
            combine_completion_candidates(candidates)
        }
        CallableTarget::Call { callee_span } => resolver
            .resolve_at_offset(callee_span.start)
            .and_then(|resolution| resolution.selected)
            .and_then(|selected| {
                completion_candidate_for_reference(
                    &selected,
                    local_index,
                    workspace_index,
                    game_data_index,
                )
            })
            .into_iter()
            .collect(),
    }
}

fn exact_top_level_candidates(index: &SymbolIndex, name: &str) -> Vec<EditorCompletionCandidate> {
    IndexQuery::new(index)
        .completion_top_level_limited(name, EditorTopLevelCompletionMode::Type, 32)
        .into_iter()
        .chain(IndexQuery::new(index).completion_top_level_limited(
            name,
            EditorTopLevelCompletionMode::Value,
            32,
        ))
        .filter(|candidate| {
            candidate
                .name
                .as_deref()
                .unwrap_or(candidate.display.label.as_str())
                == name
        })
        .collect()
}

fn completion_candidate_for_reference(
    reference: &ReferenceCandidate,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<EditorCompletionCandidate> {
    let indexes = match reference.source {
        CandidateSource::FileLocal => vec![local_index],
        CandidateSource::External => workspace_index.into_iter().chain(game_data_index).collect(),
    };

    for index in indexes {
        let Some(symbol) = index.symbol(reference.id) else {
            continue;
        };
        if symbol.kind != reference.kind || symbol.name != reference.name {
            continue;
        }
        if let Some(expected_path) = reference.absolute_path.as_ref() {
            let Some(actual_path) = index
                .file(reference.id.file_id)
                .and_then(|file| file.metadata.absolute_path.as_ref())
            else {
                continue;
            };
            if actual_path != expected_path {
                continue;
            }
        }
        return IndexQuery::new(index)
            .completion_symbols([reference.id], EditorCompletionOrigin::Direct)
            .into_iter()
            .next();
    }

    None
}

#[derive(Debug, Clone)]
struct ParameterLabelCandidate {
    parameter: CallableParameter,
    required: bool,
    active_positional: bool,
    source_kind: SourceKind,
    origin: EditorCompletionOrigin,
    sort_index: usize,
}

fn parameter_label_candidates_for_callables(
    callables: &[EditorCompletionCandidate],
    builtin_parts: Option<&CallableSignatureParts>,
    context: &ArgumentLabelCompletionContext,
) -> Vec<ParameterLabelCandidate> {
    let mut by_name = BTreeMap::<String, ParameterLabelCandidate>::new();
    let mut order = 0usize;

    let callable_parts = callables
        .iter()
        .filter_map(|callable| {
            callable
                .callable_signature_parts
                .as_ref()
                .map(|parts| (parts, callable.source_kind, callable.origin))
        })
        .chain(
            builtin_parts
                .map(|parts| (parts, SourceKind::GameData, EditorCompletionOrigin::Direct)),
        );

    for (parts, source_kind, origin) in callable_parts {
        for (parameter_index, parameter) in parts.parameters_info.iter().cloned().enumerate() {
            // The active positional `func` parameter describes the callable
            // reference slot. It is not itself a callable value, so it must
            // not enter the value list ahead of ordinary scoped completion.
            if parameter_index == context.argument_index
                && parameter.type_and_modifiers.trim() == "func"
            {
                continue;
            }
            if !starts_with_ignore_ascii_case(&parameter.name, &context.prefix) {
                continue;
            }
            if context
                .supplied_labels
                .contains(&parameter.name.to_ascii_lowercase())
            {
                continue;
            }
            let key = parameter.name.to_ascii_lowercase();
            by_name.entry(key).or_insert_with(|| {
                let required = parameter.default_text.is_none();
                let candidate = ParameterLabelCandidate {
                    parameter,
                    required,
                    active_positional: parameter_index == context.argument_index,
                    source_kind,
                    origin,
                    sort_index: order,
                };
                order += 1;
                candidate
            });
        }
    }

    let mut candidates = by_name.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        (!left.active_positional)
            .cmp(&(!right.active_positional))
            .then_with(|| (!left.required).cmp(&(!right.required)))
            .then_with(|| left.sort_index.cmp(&right.sort_index))
            .then_with(|| left.parameter.name.cmp(&right.parameter.name))
    });
    candidates
}

fn completion_items_for_parameter_labels(
    candidates: &[ParameterLabelCandidate],
    edit_range: LspRange,
    render_context: CompletionRenderContext<'_>,
) -> (
    Vec<LspCompletionItem>,
    BTreeMap<SourceKind, usize>,
    BTreeMap<String, usize>,
) {
    let mut source_kind_counts = BTreeMap::new();
    let mut origin_counts = BTreeMap::new();
    let items = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            *source_kind_counts.entry(candidate.source_kind).or_default() += 1;
            *origin_counts
                .entry(format!("{:?}", candidate.origin))
                .or_default() += 1;
            completion_item_for_parameter_label(candidate, edit_range, index, render_context)
        })
        .collect();
    (items, source_kind_counts, origin_counts)
}

fn completion_item_for_parameter_label(
    candidate: &ParameterLabelCandidate,
    edit_range: LspRange,
    index: usize,
    render_context: CompletionRenderContext<'_>,
) -> LspCompletionItem {
    let parameter = &candidate.parameter;
    let optionality = if candidate.required {
        "required"
    } else {
        "optional"
    };
    let detail = parameter_label_detail(parameter, optionality);
    let documentation = Some(LspMarkupContent {
        kind: "markdown".to_string(),
        value: parameter_label_documentation(parameter, optionality),
    });
    let insert_value = parameter_label_insert_value(parameter, Some(render_context));
    let command = Some(trigger_parameter_hints_command());
    let new_text = if candidate.active_positional && insert_value.contains('.') {
        insert_value.clone()
    } else if candidate.active_positional {
        parameter.name.clone()
    } else {
        format!("{}: {}", parameter.name, insert_value)
    };

    LspCompletionItem {
        label: parameter.name.clone(),
        label_details: Some(LspCompletionItemLabelDetails {
            detail: Some(format!(": {}", parameter.type_and_modifiers)),
            description: parameter
                .default_text
                .as_ref()
                .map(|default| format!("= {default}"))
                .or_else(|| Some(optionality.to_string())),
        }),
        kind: 10,
        detail: Some(detail),
        documentation,
        sort_text: Some(format!(
            "00:00:{:03}:{:03}:{:03}:{}",
            if candidate.active_positional { 0 } else { 1 },
            if candidate.required { 0 } else { 1 },
            index,
            parameter.name
        )),
        filter_text: Some(parameter.name.clone()),
        preselect: None,
        insert_text_format: Some(2),
        commit_characters: None,
        command,
        text_edit: LspTextEdit {
            range: edit_range,
            new_text,
            replace_range: None,
        },
        required_parameter_count: usize::from(candidate.required),
        optional_parameter_count: usize::from(!candidate.required),
    }
}

fn parameter_label_detail(parameter: &CallableParameter, optionality: &str) -> String {
    let mut detail = optionality.to_string();
    if !parameter.type_and_modifiers.is_empty() {
        detail.push(' ');
        detail.push_str(&parameter.type_and_modifiers);
    }
    if let Some(default) = parameter.default_text.as_ref() {
        detail.push_str(" = ");
        detail.push_str(default);
    }
    detail
}

fn parameter_label_documentation(parameter: &CallableParameter, optionality: &str) -> String {
    let mut output = format!("**{} parameter**", optionality);
    if !parameter.type_and_modifiers.is_empty() {
        output.push_str(&format!(
            "\n\nType: `{}`",
            escape_markdown_inline_code(&parameter.type_and_modifiers)
        ));
    }
    if let Some(default) = parameter.default_text.as_ref() {
        output.push_str(&format!(
            "\n\nDefault: `{}`",
            escape_markdown_inline_code(default)
        ));
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberVisibilityContext {
    UnqualifiedOrSelf,
    ExternalReceiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionInsertContext {
    Normal,
    Type,
    ConstructorCall,
    ContextualConstructorCall,
}

#[derive(Debug, Clone)]
struct ArgumentLabelCompletionContext {
    prefix: String,
    prefix_span: TextSpan,
    target: CallableTarget,
    argument_index: usize,
    supplied_labels: BTreeSet<String>,
    /// Present only for the bounded lexical query.  Parser-backed callers
    /// retain `target` as their source of callee identity.
    callee: String,
}

fn member_visibility_context(
    receiver_text: Option<&str>,
    owner: &str,
    containing_class: Option<&str>,
) -> MemberVisibilityContext {
    let Some(receiver) = receiver_text.map(str::trim) else {
        return MemberVisibilityContext::UnqualifiedOrSelf;
    };
    if matches!(receiver, "this" | "super") || containing_class == Some(owner) {
        MemberVisibilityContext::UnqualifiedOrSelf
    } else {
        MemberVisibilityContext::ExternalReceiver
    }
}

fn filter_member_candidates_by_visibility(
    candidates: Vec<EditorCompletionCandidate>,
    visibility: MemberVisibilityContext,
) -> Vec<EditorCompletionCandidate> {
    if visibility == MemberVisibilityContext::UnqualifiedOrSelf {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|candidate| !is_restricted_member_candidate(candidate))
        .collect()
}

fn is_restricted_member_candidate(candidate: &EditorCompletionCandidate) -> bool {
    candidate
        .display
        .modifiers
        .iter()
        .any(|modifier| matches!(modifier.as_str(), "private" | "protected"))
}

fn completion_items_for_candidates(
    candidates: &[EditorCompletionCandidate],
    edit_range: LspRange,
    origin_override: Option<&str>,
    insert_context: CompletionInsertContext,
    match_prefix: Option<&str>,
    render_context: CompletionRenderContext<'_>,
) -> (
    Vec<LspCompletionItem>,
    BTreeMap<SourceKind, usize>,
    BTreeMap<String, usize>,
) {
    let mut source_kind_counts = BTreeMap::new();
    let mut origin_counts = BTreeMap::new();
    let items = candidates
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            *source_kind_counts.entry(candidate.source_kind).or_default() += 1;
            let origin = origin_override
                .map(str::to_string)
                .unwrap_or_else(|| format!("{:?}", candidate.origin));
            *origin_counts.entry(origin).or_default() += 1;
            completion_item_for_candidate(
                candidate,
                edit_range,
                insert_context,
                match_prefix,
                order,
                render_context,
            )
        })
        .collect::<Vec<_>>();

    (items, source_kind_counts, origin_counts)
}

#[allow(clippy::too_many_arguments)]
fn top_level_completion_report_for_indexes(
    source: &str,
    parse_diagnostics: usize,
    completion_context: &str,
    prefix: String,
    prefix_span: TextSpan,
    offset: usize,
    mode: EditorTopLevelCompletionMode,
    analysis: &FileIndexAnalysis,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    mut timings: LspCompletionTimings,
    total_start: Instant,
) -> LspCompletionReport {
    let lookup_start = Instant::now();
    let declaration_context = declaration_keyword_context(source, prefix_span.start);
    let mut candidates = Vec::new();
    let override_candidates = override_completion_candidates_for_context(
        &prefix,
        offset,
        declaration_context,
        local_index,
        workspace_index,
        game_data_index,
    );
    if !override_candidates.is_empty() {
        let source_candidates = top_level_source_completion_candidates(
            &prefix,
            mode,
            local_index,
            workspace_index,
            game_data_index,
            MAX_COMPLETION_ITEMS + 1,
        );
        timings.candidate_lookup = lookup_start.elapsed();
        let edit_range = range_for_span(source, prefix_span);
        let typed_modifiers = typed_declaration_modifiers_before(source, prefix_span.start);
        let render_start = Instant::now();
        let render_context =
            CompletionRenderContext::new(local_index, workspace_index, game_data_index);
        let (mut items, mut source_kind_counts, mut origin_counts) =
            completion_items_for_override_candidates(
                &override_candidates,
                edit_range,
                &typed_modifiers,
                &prefix,
                override_completion_has_existing_body(&analysis.syntax.lexer_tokens, prefix_span),
            );
        let insert_context = completion_insert_context(source, prefix_span.start, mode);
        let (source_items, source_counts, source_origins) = completion_items_for_candidates(
            &source_candidates,
            edit_range,
            Some("TopLevel"),
            insert_context,
            Some(&prefix),
            render_context,
        );
        merge_count_maps(&mut source_kind_counts, source_counts);
        merge_count_maps(&mut origin_counts, source_origins);
        let mut keyword_items =
            keyword_completion_items(&prefix, edit_range, mode, declaration_context);
        if !keyword_items.is_empty() {
            *origin_counts.entry("Keyword".to_string()).or_default() += keyword_items.len();
            prioritize_keyword_item(&mut keyword_items, "override");
            remove_items_shadowed_by_keywords(&mut items, &keyword_items);
            keyword_items.extend(items);
            items = keyword_items;
        }
        items.extend(source_items);
        let (items, is_incomplete) = cap_completion_items(items);
        timings.item_rendering = render_start.elapsed();
        timings.total = total_start.elapsed();

        return LspCompletionReport {
            candidate_count: items.len(),
            list: LspCompletionList {
                is_incomplete,
                items,
            },
            query_quality: QueryQuality::Exact,
            recovery_reason: None,
            parse_diagnostics,
            completion_context: "override".to_string(),
            receiver_text: None,
            owner_type: None,
            prefix,
            source_kind_counts,
            origin_counts,
            failure_reason: None,
            timings,
        };
    }

    let candidates = if mode == EditorTopLevelCompletionMode::Value {
        contextual_value_completion_candidates(
            analysis,
            &prefix,
            offset,
            local_index,
            workspace_index,
            game_data_index,
        )
    } else {
        candidates.extend(top_level_source_completion_candidates(
            &prefix,
            mode,
            local_index,
            workspace_index,
            game_data_index,
            MAX_COMPLETION_ITEMS + 1,
        ));
        combine_completion_candidates(candidates)
    };
    timings.candidate_lookup = lookup_start.elapsed();

    let edit_range = range_for_span(source, prefix_span);
    let render_start = Instant::now();
    let insert_context = completion_insert_context(source, prefix_span.start, mode);
    let render_context =
        CompletionRenderContext::new(local_index, workspace_index, game_data_index);
    let (mut items, source_kind_counts, mut origin_counts) = completion_items_for_candidates(
        &candidates,
        edit_range,
        Some("TopLevel"),
        insert_context,
        Some(&prefix),
        render_context,
    );
    if mode == EditorTopLevelCompletionMode::Type || declaration_context {
        apply_tuple_type_snippets(&mut items);
    }
    if mode == EditorTopLevelCompletionMode::Type {
        rank_indexed_type_completion_items(&mut items, &prefix);
    }
    let mut keyword_items =
        keyword_completion_items(&prefix, edit_range, mode, declaration_context);
    if mode == EditorTopLevelCompletionMode::Type
        && generic_type_argument_before_offset(source, prefix_span.start)
    {
        keyword_items.retain(|item| item.label != "void");
    }
    if !keyword_items.is_empty() {
        *origin_counts.entry("Keyword".to_string()).or_default() += keyword_items.len();
        remove_items_shadowed_by_keywords(&mut items, &keyword_items);
        keyword_items.extend(items);
        items = keyword_items;
    }
    if mode == EditorTopLevelCompletionMode::Value {
        add_return_separator_to_completion_items(source, prefix_span, &mut items);
    }
    if mode == EditorTopLevelCompletionMode::Type {
        apply_generic_type_argument_final_caret_edit(source, prefix_span, &mut items);
    }
    let (items, is_incomplete) = cap_completion_items(items);
    timings.item_rendering = render_start.elapsed();
    timings.total = total_start.elapsed();

    LspCompletionReport {
        candidate_count: items.len(),
        list: LspCompletionList {
            is_incomplete,
            items,
        },
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: completion_context.to_string(),
        receiver_text: None,
        owner_type: None,
        prefix,
        source_kind_counts,
        origin_counts,
        failure_reason: None,
        timings,
    }
}

fn retyped_generic_type_argument_prefix_before_offset(
    source: &str,
    offset: usize,
) -> Option<TextSpan> {
    let prefix_span = raw_completion_prefix_span(source, offset);
    (!prefix_span.is_empty()
        && prefix_span.end == offset
        && generic_type_argument_before_offset(source, prefix_span.start)
        && generic_type_argument_closing_span_after_prefix(source, prefix_span).is_some())
    .then_some(prefix_span)
}

fn apply_generic_type_argument_final_caret_edit(
    source: &str,
    prefix_span: TextSpan,
    items: &mut [LspCompletionItem],
) {
    let Some(replace_span) = generic_type_argument_closing_span_after_prefix(source, prefix_span)
    else {
        return;
    };
    let Some(closing_text) = source.get(prefix_span.end..replace_span.end) else {
        return;
    };
    let replace_range = range_for_span(source, replace_span);
    for item in items {
        if item.insert_text_format.is_none()
            && item.command.is_none()
            && item.text_edit.replace_range.is_none()
        {
            item.text_edit.new_text.push_str(closing_text);
            item.text_edit.new_text.push_str("$0");
            // VS Code chooses the insert side of an InsertReplaceEdit at this
            // caret shape, which would leave the existing `>` behind. Use one
            // ordinary edit spanning the closer so acceptance always consumes
            // it before placing the final tabstop after the rebuilt delimiter.
            item.text_edit.range = replace_range.clone();
            item.insert_text_format = Some(2);
        }
    }
}

fn generic_type_argument_closing_span_after_prefix(
    source: &str,
    prefix_span: TextSpan,
) -> Option<TextSpan> {
    if prefix_span.is_empty() || !generic_type_argument_before_offset(source, prefix_span.start) {
        return None;
    }
    let closing = lex(source).into_iter().find(|token| {
        !token.kind.is_trivia()
            && token.kind != TokenKind::Eof
            && token.span.start >= prefix_span.end
    })?;
    let closing_end = match closing.kind {
        TokenKind::Operator(Operator::Greater) => closing.span.end,
        // Nested generic closers are lexed as one `>>` token. Replace only
        // the inner closer so its final tabstop remains inside the outer
        // generic argument rather than consuming both delimiters.
        TokenKind::Operator(Operator::GreaterGreater) => closing.span.start + 1,
        _ => return None,
    };
    if !source
        .get(prefix_span.end..closing.span.start)?
        .chars()
        .all(char::is_whitespace)
    {
        return None;
    }
    Some(TextSpan::new(prefix_span.start, closing_end))
}

fn generic_type_argument_before_offset(source: &str, offset: usize) -> bool {
    let tokens = lex(source)
        .into_iter()
        .filter(|token| {
            !token.kind.is_trivia() && token.kind != TokenKind::Eof && token.span.end <= offset
        })
        .collect::<Vec<_>>();
    let mut depth = 0usize;
    for token in tokens.iter().rev() {
        match token.kind {
            TokenKind::Operator(Operator::Greater) => depth += 1,
            TokenKind::Operator(Operator::GreaterGreater) => depth += 2,
            TokenKind::Operator(Operator::Less) => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            TokenKind::Semicolon | TokenKind::LeftBrace | TokenKind::RightBrace => return false,
            _ => {}
        }
    }
    false
}

fn empty_generic_type_slot_before_offset(source: &str, offset: usize) -> bool {
    generic_type_slot_owner_before_offset(source, offset).is_some_and(|owner| {
        matches!(owner, "array" | "map" | "set") || tuple_type_argument_count(owner).is_some()
    })
}

fn empty_generic_type_slot_before_offset_with_indexes(
    source: &str,
    offset: usize,
    local_index: Option<&SymbolIndex>,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> bool {
    empty_generic_type_slot_before_offset(source, offset)
        || generic_type_slot_owner_before_offset(source, offset).is_some_and(|owner| {
            [local_index, workspace_index, game_data_index]
                .into_iter()
                .flatten()
                .any(|index| index_has_generic_class(index, owner))
        })
}

fn index_has_generic_class(index: &SymbolIndex, owner: &str) -> bool {
    index
        .preferred_classes_by_name(owner)
        .into_iter()
        .any(|id| {
            index.children(id).iter().any(|child| {
                index
                    .symbol(*child)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::TypeParameter)
            })
        })
}

fn generic_type_slot_owner_before_offset(source: &str, offset: usize) -> Option<&str> {
    let tokens = lex(source)
        .into_iter()
        .filter(|token| {
            !token.kind.is_trivia() && token.kind != TokenKind::Eof && token.span.end <= offset
        })
        .collect::<Vec<_>>();
    let Some(last) = tokens.last() else {
        return None;
    };
    let opener_index = match last.kind {
        TokenKind::Operator(Operator::Less) => tokens.len().checked_sub(1),
        TokenKind::Comma => {
            let mut nested_generic_depth = 0usize;
            tokens[..tokens.len() - 1]
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, token)| match token.kind {
                    TokenKind::Operator(Operator::Greater) => {
                        nested_generic_depth += 1;
                        None
                    }
                    TokenKind::Operator(Operator::GreaterGreater) => {
                        nested_generic_depth += 2;
                        None
                    }
                    TokenKind::Operator(Operator::Less) if nested_generic_depth == 0 => Some(index),
                    TokenKind::Operator(Operator::Less) => {
                        nested_generic_depth -= 1;
                        None
                    }
                    _ => None,
                })
        }
        _ => None,
    };
    let Some(opener_index) = opener_index else {
        return None;
    };
    let generic_type = opener_index
        .checked_sub(1)
        .and_then(|index| tokens.get(index))?;
    (generic_type.kind == TokenKind::Identifier
        || matches!(generic_type.kind, TokenKind::Keyword(_)))
    .then(|| &source[generic_type.span.start..generic_type.span.end])
}

/// Recovers a lone prospective parameter type while a callable declaration is
/// incomplete. Without a variable name, Enfusion cannot have a parameter
/// declaration yet; treating the token as one would suppress type snippets.
fn incomplete_callable_parameter_type_before_offset(source: &str, offset: usize) -> bool {
    let tokens = lex(source)
        .into_iter()
        .filter(|token| {
            !token.kind.is_trivia() && token.kind != TokenKind::Eof && token.span.end <= offset
        })
        .collect::<Vec<_>>();
    let Some(last) = tokens.last() else {
        return false;
    };
    if !matches!(last.kind, TokenKind::Identifier | TokenKind::Keyword(_))
        || last.span.end != offset
    {
        return false;
    }
    let mut parenthesis_depth = 0usize;
    let Some(open_index) =
        tokens
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, token)| match token.kind {
                TokenKind::RightParen => {
                    parenthesis_depth += 1;
                    None
                }
                TokenKind::LeftParen if parenthesis_depth == 0 => Some(index),
                TokenKind::LeftParen => {
                    parenthesis_depth -= 1;
                    None
                }
                _ => None,
            })
    else {
        return false;
    };
    let Some(method_name_index) = open_index.checked_sub(1) else {
        return false;
    };
    if tokens[method_name_index].kind != TokenKind::Identifier
        || method_name_index == 0
        || !is_callable_return_type_token(tokens[method_name_index - 1].kind)
    {
        return false;
    }
    let parameter_start = tokens[open_index + 1..]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, token)| {
            (token.kind == TokenKind::Comma).then_some(open_index + index + 2)
        })
        .unwrap_or(open_index + 1);
    let parameter = &tokens[parameter_start..];
    let Some((type_token, modifiers)) = parameter.split_last() else {
        return false;
    };
    matches!(
        type_token.kind,
        TokenKind::Identifier | TokenKind::Keyword(_)
    ) && modifiers
        .iter()
        .all(|token| is_incomplete_parameter_type_modifier(token.kind))
}

fn is_incomplete_parameter_type_modifier(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Keyword(
            Keyword::Const | Keyword::Ref | Keyword::Out | Keyword::Inout | Keyword::Notnull
        )
    )
}

fn is_callable_return_type_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(
                Keyword::Void
                    | Keyword::Bool
                    | Keyword::Int
                    | Keyword::Float
                    | Keyword::String
                    | Keyword::Vector
                    | Keyword::Typename
                    | Keyword::Auto
            )
    )
}

fn rank_indexed_type_completion_items(items: &mut [LspCompletionItem], prefix: &str) {
    for item in items {
        let match_rank = completion_name_match_rank(&item.label, prefix).unwrap_or(u16::MAX);
        let inherited_sort = item.sort_text.take().unwrap_or_else(|| item.label.clone());
        // Indexed enums/classes follow builtins only once their match quality
        // ties; an exact user class must beat a weaker builtin prefix.
        item.sort_text = Some(format!("{match_rank:03}:03:{inherited_sort}"));
    }
}

fn top_level_source_completion_candidates(
    prefix: &str,
    mode: EditorTopLevelCompletionMode,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    limit: usize,
) -> Vec<EditorCompletionCandidate> {
    let mut candidates = Vec::new();
    candidates
        .extend(IndexQuery::new(local_index).completion_top_level_limited(prefix, mode, limit));
    if let Some(external_index) = workspace_index {
        candidates.extend(
            IndexQuery::new(external_index).completion_top_level_limited(prefix, mode, limit),
        );
    }
    if let Some(external_index) = game_data_index {
        candidates.extend(
            IndexQuery::new(external_index).completion_top_level_limited(prefix, mode, limit),
        );
    }
    candidates
}

/// A value completion immediately after `return` must supply the required
/// separator because its zero-width edit cannot otherwise distinguish
/// `return owner` from `returnowner`.
fn completion_immediately_follows_return(source: &str, prefix_span: TextSpan) -> bool {
    let prefix_start = prefix_span.start;
    let Some(previous) = lex(&source[..prefix_start])
        .into_iter()
        .filter(|token| !token.kind.is_trivia() && token.kind != TokenKind::Eof)
        .last()
    else {
        return false;
    };
    previous.kind == TokenKind::Keyword(Keyword::Return) && previous.span.end == prefix_start
}

fn add_return_separator_to_completion_items(
    source: &str,
    prefix_span: TextSpan,
    items: &mut [LspCompletionItem],
) {
    if completion_immediately_follows_return(source, prefix_span) {
        for item in items {
            item.text_edit.new_text.insert(0, ' ');
        }
    }
}

/// Collects the normal value-completion universe for a matching-revision
/// analysis. The enum member renderer reuses this exact collector so enum
/// ranking cannot discard locals or containing-class members.
fn contextual_value_completion_candidates(
    analysis: &FileIndexAnalysis,
    prefix: &str,
    offset: usize,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Vec<EditorCompletionCandidate> {
    let mut candidates = scoped_value_completion_candidates(analysis, prefix, offset);
    if let Some(class_name) = containing_class_name(local_index, offset) {
        for owner in containing_class_completion_owners(
            local_index,
            workspace_index,
            game_data_index,
            &class_name,
        ) {
            candidates.extend(prefixed_candidates(
                completion_candidates_for_owner(local_index, &owner, false),
                prefix,
            ));
            if let Some(external_index) = workspace_index {
                candidates.extend(prefixed_candidates(
                    completion_candidates_for_owner(external_index, &owner, false),
                    prefix,
                ));
            }
            if let Some(external_index) = game_data_index {
                candidates.extend(prefixed_candidates(
                    completion_candidates_for_owner(external_index, &owner, false),
                    prefix,
                ));
            }
        }
    }
    candidates.extend(top_level_source_completion_candidates(
        prefix,
        EditorTopLevelCompletionMode::Value,
        local_index,
        workspace_index,
        game_data_index,
        MAX_COMPLETION_ITEMS + 1,
    ));
    combine_completion_candidates(
        candidates
            .into_iter()
            .filter(|candidate| !is_current_prefix_self_candidate(candidate, prefix, offset))
            .collect(),
    )
}

fn merge_count_maps<K: Ord>(target: &mut BTreeMap<K, usize>, source: BTreeMap<K, usize>) {
    for (key, count) in source {
        *target.entry(key).or_default() += count;
    }
}

fn is_current_prefix_self_candidate(
    candidate: &EditorCompletionCandidate,
    prefix: &str,
    offset: usize,
) -> bool {
    candidate
        .name
        .as_deref()
        .unwrap_or(candidate.display.label.as_str())
        == prefix
        && span_contains(candidate.selection_span, offset)
}

fn prioritize_keyword_item(items: &mut Vec<LspCompletionItem>, keyword: &str) {
    if let Some(position) = items.iter().position(|item| item.label == keyword) {
        let mut item = items.remove(position);
        item.sort_text = Some(format!("00:00:000:00:{keyword}"));
        items.insert(0, item);
    }
}

fn remove_items_shadowed_by_keywords(
    items: &mut Vec<LspCompletionItem>,
    keyword_items: &[LspCompletionItem],
) {
    let keyword_labels = keyword_items
        .iter()
        .map(|item| item.label.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    items.retain(|item| !keyword_labels.contains(&item.label.to_ascii_lowercase()));
}

fn scoped_value_completion_candidates(
    analysis: &FileIndexAnalysis,
    prefix: &str,
    offset: usize,
) -> Vec<EditorCompletionCandidate> {
    let ids = analysis
        .scope
        .visible_symbols_with_prefix(&analysis.index, prefix, offset);
    IndexQuery::new(&analysis.index).completion_symbols(ids, EditorCompletionOrigin::Direct)
}

fn override_completion_candidates_for_context(
    prefix: &str,
    offset: usize,
    declaration_context: bool,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Vec<EditorCompletionCandidate> {
    if prefix.is_empty() || !declaration_context || inside_callable_after_name(local_index, offset)
    {
        return Vec::new();
    }
    let Some(class_name) = containing_class_name(local_index, offset) else {
        return Vec::new();
    };

    let owners = containing_class_completion_owners(
        local_index,
        workspace_index,
        game_data_index,
        &class_name,
    );
    let modded = preferred_class_is_modded(local_index, &class_name);
    let mut candidates = Vec::new();
    for owner in owners.into_iter().skip(usize::from(!modded)) {
        let modded_owner = modded && owner == class_name;
        if !modded_owner {
            candidates.extend(prefixed_candidates(
                override_candidates_for_owner(local_index, &owner),
                prefix,
            ));
        }
        if let Some(external_index) = workspace_index {
            candidates.extend(prefixed_candidates(
                if modded {
                    override_candidates_for_predecessor_owner(external_index, &owner)
                } else {
                    override_candidates_for_owner(external_index, &owner)
                },
                prefix,
            ));
        }
        if let Some(external_index) = game_data_index {
            candidates.extend(prefixed_candidates(
                if modded {
                    override_candidates_for_predecessor_owner(external_index, &owner)
                } else {
                    override_candidates_for_owner(external_index, &owner)
                },
                prefix,
            ));
        }
    }

    combine_completion_candidates(candidates)
}

fn override_candidates_for_owner(
    index: &SymbolIndex,
    owner: &str,
) -> Vec<EditorCompletionCandidate> {
    IndexQuery::new(index)
        .completion_members_for_class(owner)
        .candidates
        .into_iter()
        .filter(is_overridable_method_candidate)
        .collect()
}

fn override_candidates_for_predecessor_owner(
    index: &SymbolIndex,
    owner: &str,
) -> Vec<EditorCompletionCandidate> {
    completion_candidates_for_predecessor_owner(index, owner, false)
        .into_iter()
        .filter(is_overridable_method_candidate)
        .collect()
}

fn is_overridable_method_candidate(candidate: &EditorCompletionCandidate) -> bool {
    candidate.kind == SymbolKind::Method
        && !candidate.display.modifiers.iter().any(|modifier| {
            matches!(
                modifier.as_str(),
                "private" | "static" | "sealed" | "proto" | "external" | "native"
            )
        })
}

fn completion_items_for_override_candidates(
    candidates: &[EditorCompletionCandidate],
    edit_range: LspRange,
    typed_modifiers: &BTreeSet<String>,
    match_prefix: &str,
    existing_body: bool,
) -> (
    Vec<LspCompletionItem>,
    BTreeMap<SourceKind, usize>,
    BTreeMap<String, usize>,
) {
    let mut source_kind_counts = BTreeMap::new();
    let mut origin_counts = BTreeMap::new();
    let items = candidates
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            *source_kind_counts.entry(candidate.source_kind).or_default() += 1;
            *origin_counts.entry("Override".to_string()).or_default() += 1;
            completion_item_for_override_candidate(
                candidate,
                edit_range,
                typed_modifiers,
                match_prefix,
                order,
                existing_body,
            )
        })
        .collect::<Vec<_>>();

    (items, source_kind_counts, origin_counts)
}

/// A completion edit replaces only the in-progress declaration text. When a
/// following block already belongs to that declaration, preserve it and render
/// only the resolved signature instead of inserting a second method body.
fn override_completion_has_existing_body(tokens: &[Token], prefix_span: TextSpan) -> bool {
    tokens
        .iter()
        .find(|token| token.span.start >= prefix_span.end && !token.kind.is_trivia())
        .is_some_and(|token| token.kind == TokenKind::LeftBrace)
}

fn completion_item_for_override_candidate(
    candidate: &EditorCompletionCandidate,
    edit_range: LspRange,
    typed_modifiers: &BTreeSet<String>,
    match_prefix: &str,
    order: usize,
    existing_body: bool,
) -> Option<LspCompletionItem> {
    let label = candidate
        .name
        .clone()
        .or_else(|| Some(candidate.display.label.clone()))?;
    let call = candidate.callable_signature_parts.clone()?;
    let return_type = call
        .result
        .as_deref()
        .and_then(|result| result.strip_prefix("->"))
        .map(str::trim)
        .filter(|result| !result.is_empty())
        .unwrap_or("void");
    let modifiers = override_completion_modifiers(candidate, typed_modifiers);
    let declaration_prefix = if modifiers.is_empty() {
        return_type.to_string()
    } else {
        format!("{} {return_type}", modifiers.join(" "))
    };
    let declaration = format!("{declaration_prefix} {label}{}", call.parameters);
    let new_text = if existing_body {
        declaration
    } else {
        format!("{declaration}\n{{\n\t$0\n}}")
    };

    Some(LspCompletionItem {
        label: label.clone(),
        label_details: Some(LspCompletionItemLabelDetails {
            detail: Some(call.parameters.clone()),
            description: call.result.clone(),
        }),
        kind: 2,
        detail: Some(declaration_prefix),
        documentation: candidate
            .display
            .documentation_preview
            .as_ref()
            .map(|preview| LspMarkupContent {
                kind: "markdown".to_string(),
                value: preview.clone(),
            }),
        sort_text: Some(completion_sort_text(
            candidate,
            &label,
            Some(match_prefix),
            order,
        )),
        filter_text: Some(label),
        preselect: None,
        insert_text_format: Some(2),
        commit_characters: None,
        command: None,
        text_edit: LspTextEdit {
            range: edit_range,
            new_text,
            replace_range: None,
        },
        required_parameter_count: call.required_parameter_count(),
        optional_parameter_count: call.optional_parameter_count(),
    })
}

fn override_completion_modifiers(
    candidate: &EditorCompletionCandidate,
    typed_modifiers: &BTreeSet<String>,
) -> Vec<String> {
    let mut modifiers = vec!["override".to_string()];
    for modifier in &candidate.display.modifiers {
        if matches!(modifier.as_str(), "protected" | "const" | "notnull") {
            modifiers.push(modifier.clone());
        }
    }
    modifiers.retain(|modifier| !typed_modifiers.contains(modifier.as_str()));
    modifiers
}

fn typed_declaration_modifiers_before(source: &str, offset: usize) -> BTreeSet<String> {
    let mut modifiers = BTreeSet::new();
    for token in lex(source)
        .into_iter()
        .take_while(|token| token.span.end <= offset)
        .filter(|token| !token.kind.is_trivia())
    {
        match token.kind {
            TokenKind::LeftBrace | TokenKind::RightBrace | TokenKind::Semicolon => {
                modifiers.clear();
            }
            TokenKind::Keyword(_) => {
                if let Some(text) = source.get(token.span.start..token.span.end) {
                    if is_override_completion_modifier_text(text) {
                        modifiers.insert(text.to_ascii_lowercase());
                    }
                }
            }
            _ => {}
        }
    }
    modifiers
}

fn is_override_completion_modifier_text(text: &str) -> bool {
    matches!(text, "override" | "protected" | "const" | "notnull")
}

fn keyword_completion_items(
    prefix: &str,
    edit_range: LspRange,
    mode: EditorTopLevelCompletionMode,
    declaration_context: bool,
) -> Vec<LspCompletionItem> {
    let mut keywords = match mode {
        EditorTopLevelCompletionMode::Type => TYPE_COMPLETION_KEYWORDS.to_vec(),
        EditorTopLevelCompletionMode::Value => STATEMENT_COMPLETION_KEYWORDS.to_vec(),
    };
    if declaration_context {
        keywords.extend(DECLARATION_COMPLETION_KEYWORDS);
        if mode == EditorTopLevelCompletionMode::Value {
            keywords.extend(TYPE_COMPLETION_KEYWORDS);
        }
    }
    keywords.sort_by(|left, right| {
        (
            keyword_completion_match_rank(left, prefix),
            type_keyword_category_rank(left, mode),
            left.chars().count(),
            *left,
        )
            .cmp(&(
                keyword_completion_match_rank(right, prefix),
                type_keyword_category_rank(right, mode),
                right.chars().count(),
                *right,
            ))
    });
    keywords.dedup();
    keywords
        .into_iter()
        .filter(|keyword| starts_with_ignore_ascii_case(keyword, prefix))
        .map(|keyword| {
            // These control headers have one unambiguous next construct: a
            // parenthesized condition/header. Keep that choice inside the accepted
            // Rust-authored completion edit so VS Code applies it atomically,
            // rather than observing a later Space/Enter edit from the client.
            let is_control_header = matches!(keyword, "if" | "for" | "foreach" | "while" | "switch");
            let type_snippet = type_keyword_snippet(keyword, mode, declaration_context);
            let (new_text, insert_text_format, commit_characters) = if is_control_header {
                (
                    format!("{keyword} ($0)"),
                    Some(2),
                    // Space accepts the selected `if` suggestion as the same
                    // atomic completion transaction as Tab. This is not an
                    // observed post-keypress rewrite, so it cannot race the
                    // document, caret, or native Enter indentation.
                    Some(vec![" ".to_string()]),
                )
            } else if let Some(type_snippet) = type_snippet.as_ref() {
                (
                    type_snippet.snippet(keyword),
                    Some(2),
                    None,
                )
            } else {
                (keyword.to_string(), None, None)
            };
            LspCompletionItem {
                label: keyword.to_string(),
                label_details: None,
                kind: 14,
                detail: Some("keyword".to_string()),
                documentation: None,
                sort_text: Some(format!(
                    "{:03}:{:02}:{:03}:{}",
                    keyword_completion_match_rank(keyword, prefix),
                    type_keyword_category_rank(keyword, mode),
                    keyword.chars().count(),
                    keyword
                )),
                filter_text: Some(keyword.to_string()),
                preselect: None,
                insert_text_format,
                commit_characters,
                command: if is_control_header {
                    Some(LspCommand {
                        title: "Normalize control header Space commit".to_string(),
                        command: COMMAND_NORMALIZE_IF_SPACE_COMMIT.to_string(),
                        arguments: Some(vec![json!({
                        "expectedCommit": " ",
                        "deletion": {
                            "start": { "line": edit_range.start.line, "character": edit_range.start.character + keyword.len() as u32 + 2 },
                            "end": { "line": edit_range.start.line, "character": edit_range.start.character + keyword.len() as u32 + 3 },
                        },
                        "trailingDeletion": {
                            "start": { "line": edit_range.start.line, "character": edit_range.start.character + keyword.len() as u32 + 3 },
                            "end": { "line": edit_range.start.line, "character": edit_range.start.character + keyword.len() as u32 + 4 },
                        },
                        "caret": { "line": edit_range.start.line, "character": edit_range.start.character + keyword.len() as u32 + 2 },
                        })]),
                    })
                } else {
                    type_snippet.map(TypeKeywordSnippet::follow_up_command)
                },
                text_edit: LspTextEdit {
                    range: edit_range,
                    new_text,
                    replace_range: None,
                },
                required_parameter_count: 0,
                optional_parameter_count: 0,
            }
        })
        .collect()
}

fn type_keyword_category_rank(keyword: &str, mode: EditorTopLevelCompletionMode) -> u8 {
    if mode != EditorTopLevelCompletionMode::Type {
        return 0;
    }
    match keyword {
        "bool" | "int" | "float" | "string" | "vector" | "typename" | "auto" | "void" => 0,
        "ref" => 1,
        "array" | "set" | "map" => 2,
        _ => 3,
    }
}

#[derive(Clone, Copy)]
struct GenericTypeSnippet {
    type_argument_count: usize,
}

impl GenericTypeSnippet {
    fn snippet(self, label: &str) -> String {
        let slots = (1..=self.type_argument_count)
            .map(|index| format!("${{{index}}}"))
            .collect::<Vec<_>>()
            .join(", ");
        // The final tabstop is deliberately outside the closing delimiter.
        format!("{label}<{slots}>$0")
    }

    fn follow_up_command(self) -> LspCommand {
        trigger_suggest_at_generic_type_placeholder_command(
            std::iter::repeat_n(String::new(), self.type_argument_count).collect(),
        )
    }
}

#[derive(Clone, Copy)]
struct CollectionTypeSnippet {
    generic: GenericTypeSnippet,
}

impl CollectionTypeSnippet {
    fn for_keyword(
        keyword: &str,
        mode: EditorTopLevelCompletionMode,
        declaration_context: bool,
    ) -> Option<Self> {
        if mode != EditorTopLevelCompletionMode::Type && !declaration_context {
            return None;
        }
        let type_argument_count = match keyword {
            "array" | "set" => 1,
            "map" => 2,
            _ => return None,
        };
        Some(Self {
            generic: GenericTypeSnippet {
                type_argument_count,
            },
        })
    }

    fn snippet(self, keyword: &str) -> String {
        self.generic.snippet(keyword)
    }

    fn follow_up_command(self) -> LspCommand {
        self.generic.follow_up_command()
    }
}

enum TypeKeywordSnippet {
    Collection(CollectionTypeSnippet),
    Ref,
}

impl TypeKeywordSnippet {
    fn snippet(&self, keyword: &str) -> String {
        match self {
            Self::Collection(collection) => collection.snippet(keyword),
            Self::Ref => "ref ${1}".to_string(),
        }
    }

    fn follow_up_command(self) -> LspCommand {
        match self {
            Self::Collection(collection) => collection.follow_up_command(),
            Self::Ref => trigger_suggest_at_snippet_placeholder_command(vec![String::new()]),
        }
    }
}

fn type_keyword_snippet(
    keyword: &str,
    mode: EditorTopLevelCompletionMode,
    declaration_context: bool,
) -> Option<TypeKeywordSnippet> {
    CollectionTypeSnippet::for_keyword(keyword, mode, declaration_context)
        .map(TypeKeywordSnippet::Collection)
        .or_else(|| {
            ((mode == EditorTopLevelCompletionMode::Type || declaration_context)
                && keyword == "ref")
                .then_some(TypeKeywordSnippet::Ref)
        })
}

fn tuple_type_argument_count(label: &str) -> Option<usize> {
    let type_argument_count = label.strip_prefix("Tuple")?.parse::<usize>().ok()?;
    (1..=6)
        .contains(&type_argument_count)
        .then_some(type_argument_count)
}

fn tuple_type_snippet(label: &str) -> Option<GenericTypeSnippet> {
    Some(GenericTypeSnippet {
        type_argument_count: tuple_type_argument_count(label)?,
    })
}

fn apply_tuple_type_snippets(items: &mut [LspCompletionItem]) {
    for item in items {
        // Tuple completion belongs only to class candidates, never to an
        // unrelated callable or value that happens to share the name.
        if item.kind != 7 {
            continue;
        }
        let Some(tuple) = tuple_type_snippet(&item.label) else {
            continue;
        };
        item.text_edit.new_text = tuple.snippet(&item.label);
        item.insert_text_format = Some(2);
        item.command = Some(tuple.follow_up_command());
    }
}

/// The generated first switch arm is deliberately a tiny completion surface.
/// `default` remains a normal replacement; accepting `case` creates the only
/// structurally valid alternative and selects a value field for ordinary value
/// completion. This recognizes syntax, never editor text heuristics.
#[cfg(test)]
fn switch_arm_placeholder_completion_items(
    source: &str,
    prefix_span: TextSpan,
) -> Option<Vec<LspCompletionItem>> {
    let prefix = source.get(prefix_span.start..prefix_span.end)?;
    if prefix.is_empty()
        || !starts_with_ignore_ascii_case("default", prefix)
            && !starts_with_ignore_ascii_case("case", prefix)
    {
        return None;
    }
    let tokens = lex(source)
        .into_iter()
        .filter(|token| !token.kind.is_trivia() && token.kind != TokenKind::Eof)
        .collect::<Vec<_>>();
    let index = tokens.iter().position(|token| token.span == prefix_span)?;
    if tokens.get(index + 1).map(|token| token.kind) != Some(TokenKind::Colon)
        || index < 2
        || tokens[index - 1].kind != TokenKind::LeftBrace
        || tokens[index - 2].kind != TokenKind::RightParen
        || !tokens[..index - 2]
            .iter()
            .rev()
            .any(|token| token.kind == TokenKind::Keyword(Keyword::Switch))
    {
        return None;
    }
    let range = range_for_span(source, prefix_span);
    let keyword_item = |label: &str,
                        new_text: String,
                        format: Option<u32>,
                        command: Option<LspCommand>| LspCompletionItem {
        label: label.to_string(),
        label_details: None,
        kind: 14,
        detail: Some("switch arm".to_string()),
        documentation: None,
        sort_text: Some(format!("00:00:000:{label}")),
        filter_text: Some(label.to_string()),
        preselect: None,
        insert_text_format: format,
        commit_characters: None,
        command,
        text_edit: LspTextEdit {
            range,
            new_text,
            replace_range: None,
        },
        required_parameter_count: 0,
        optional_parameter_count: 0,
    };
    Some(vec![
        keyword_item("default", "default".to_string(), None, None),
        keyword_item(
            "case",
            "case ${1:value}".to_string(),
            Some(2),
            Some(trigger_suggest_at_snippet_placeholder_command(vec![
                "value".to_string(),
            ])),
        ),
    ])
}

fn keyword_completion_match_rank(keyword: &str, prefix: &str) -> u16 {
    completion_name_match_rank(keyword, prefix).unwrap_or(u16::MAX)
}

const STATEMENT_COMPLETION_KEYWORDS: &[&str] = &[
    "return", "if", "else", "for", "foreach", "while", "do", "switch", "case", "default", "break",
    "continue", "true", "false", "null", "new", "delete", "thread", "this", "super",
];

const DECLARATION_COMPLETION_KEYWORDS: &[&str] = &[
    "static",
    "protected",
    "private",
    "const",
    "ref",
    "out",
    "inout",
    "notnull",
    "override",
    "event",
    "proto",
    "external",
    "native",
    "class",
    "modded",
    "sealed",
    "enum",
    "typedef",
];

const TYPE_COMPLETION_KEYWORDS: &[&str] = &[
    "void", "int", "float", "bool", "string", "vector", "typename", "auto", "ref", "array", "set",
    "map",
];

fn declaration_keyword_context(source: &str, offset: usize) -> bool {
    // Completion must remain useful while the preceding line is unfinished.
    // At an indentation-only physical line, do not let an earlier assignment,
    // type, or other incomplete token suppress the next declaration/type
    // skeleton. The same recovery boundary serves every declaration keyword;
    // collection types do not get a separate exception.
    if source
        .get(..offset)
        .and_then(|before_cursor| before_cursor.rsplit(['\n', '\r']).next())
        .is_some_and(|line| line.trim().is_empty())
    {
        return true;
    }
    previous_significant_token_kind(source, offset).is_none_or(|kind| match kind {
        TokenKind::LeftBrace | TokenKind::RightBrace | TokenKind::Semicolon => true,
        TokenKind::Keyword(keyword) => matches!(
            keyword,
            crate::lexer::Keyword::Class
                | crate::lexer::Keyword::Modded
                | crate::lexer::Keyword::Sealed
                | crate::lexer::Keyword::Typedef
                | crate::lexer::Keyword::Proto
                | crate::lexer::Keyword::External
                | crate::lexer::Keyword::Native
                | crate::lexer::Keyword::Private
                | crate::lexer::Keyword::Protected
                | crate::lexer::Keyword::Static
                | crate::lexer::Keyword::Override
                | crate::lexer::Keyword::Const
                | crate::lexer::Keyword::Ref
                | crate::lexer::Keyword::Out
                | crate::lexer::Keyword::Inout
                | crate::lexer::Keyword::Notnull
                | crate::lexer::Keyword::Autoptr
                | crate::lexer::Keyword::Owned
                | crate::lexer::Keyword::Event
        ),
        _ => false,
    })
}

fn previous_significant_token_kind(source: &str, offset: usize) -> Option<TokenKind> {
    lex(source)
        .into_iter()
        .take_while(|token| token.span.end <= offset)
        .filter(|token| !token.kind.is_trivia())
        .last()
        .map(|token| token.kind)
}

fn previous_significant_completion_token_before_span(
    tokens: &[crate::lexer::Token],
    span: TextSpan,
) -> Option<crate::lexer::Token> {
    tokens
        .iter()
        .rev()
        .find(|token| {
            token.span.end <= span.start && !token.kind.is_trivia() && token.kind != TokenKind::Eof
        })
        .copied()
}

fn token_blocks_argument_label_completion(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::LineComment
            | TokenKind::DocLineComment
            | TokenKind::BlockComment
            | TokenKind::DocBlockComment
            | TokenKind::UnterminatedBlockComment
            | TokenKind::String
            | TokenKind::UnterminatedString
    )
}

fn completion_insert_context(
    source: &str,
    offset: usize,
    mode: EditorTopLevelCompletionMode,
) -> CompletionInsertContext {
    match previous_significant_token_kind(source, offset) {
        Some(TokenKind::LeftBracket) | Some(TokenKind::Keyword(crate::lexer::Keyword::New)) => {
            CompletionInsertContext::ConstructorCall
        }
        _ if mode == EditorTopLevelCompletionMode::Type => CompletionInsertContext::Type,
        _ => CompletionInsertContext::Normal,
    }
}

fn containing_class_name(index: &SymbolIndex, offset: usize) -> Option<String> {
    index
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Class && span_contains(symbol.span, offset))
        .min_by_key(|symbol| symbol.span.len())
        .and_then(|symbol| symbol.name.clone())
}

fn inside_callable_after_name(index: &SymbolIndex, offset: usize) -> bool {
    index.symbols().iter().any(|symbol| {
        matches!(
            symbol.kind,
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::Constructor
                | SymbolKind::Destructor
        ) && span_contains(symbol.span, offset)
            && offset > symbol.selection_span.end
    })
}

fn containing_class_completion_owners(
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    class_name: &str,
) -> Vec<String> {
    let indexes = ExternalIndexes::new(workspace_index, game_data_index).ordered();
    let mut owners = Vec::new();
    let mut pending = vec![class_name.to_string()];
    let mut seen = BTreeSet::new();

    while let Some(owner) = pending.pop() {
        if owners.len() >= 32 || !seen.insert(owner.clone()) {
            continue;
        }

        let base = class_base_type(local_index, &owner).or_else(|| {
            indexes
                .iter()
                .find_map(|external_index| class_base_type(external_index, &owner))
        });
        owners.push(owner);

        if let Some(base) = base {
            pending.push(base);
        }
    }

    owners
}

fn external_completion_owners(
    root_owner: &str,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Vec<String> {
    let indexes = ExternalIndexes::new(workspace_index, game_data_index).ordered();
    let mut owners = Vec::new();
    let mut pending = vec![root_owner.to_string()];
    let mut seen = BTreeSet::new();

    while let Some(owner) = pending.pop() {
        if owners.len() >= 32 || !seen.insert(owner.clone()) {
            continue;
        }
        let base = indexes
            .iter()
            .find_map(|index| class_base_type(index, &owner));
        owners.push(owner);
        if let Some(base) = base {
            pending.push(base);
        }
    }

    owners
}

fn class_base_type(index: &SymbolIndex, class_name: &str) -> Option<String> {
    index
        .preferred_classes_by_name(class_name)
        .into_iter()
        .find_map(|id| {
            index
                .symbol(id)
                .and_then(|symbol| symbol.detail.base_type.as_deref())
                .map(str::trim)
                .filter(|base| !base.is_empty())
                .map(str::to_string)
        })
}

fn prefixed_candidates(
    candidates: Vec<EditorCompletionCandidate>,
    prefix: &str,
) -> Vec<EditorCompletionCandidate> {
    candidates
        .into_iter()
        .filter(|candidate| candidate_matches_prefix(candidate, prefix))
        .collect()
}

fn candidate_matches_prefix(candidate: &EditorCompletionCandidate, prefix: &str) -> bool {
    let name = candidate
        .name
        .as_deref()
        .unwrap_or(candidate.display.label.as_str());
    completion_name_match_rank(name, prefix).is_some()
}

fn span_contains(span: TextSpan, offset: usize) -> bool {
    offset >= span.start && offset <= span.end
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn argument_label_completion_context(
    source: &str,
    root: &SyntaxNode,
    offset: usize,
) -> Option<ArgumentLabelCompletionContext> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }

    let tokens = lex(source);
    let (prefix, prefix_span) =
        completion_identifier_prefix_for_argument_label(source, &tokens, offset)?;
    if previous_significant_completion_token_before_span(&tokens, prefix_span)
        .is_some_and(|token| token.kind == TokenKind::Dot)
    {
        return None;
    }
    if !is_argument_label_position(source, prefix_span) {
        return None;
    }

    let context = callable_argument_context_at_offset(source, root, prefix_span.start)?;
    Some(ArgumentLabelCompletionContext {
        prefix,
        prefix_span,
        target: context.target,
        argument_index: context.argument_index,
        supplied_labels: context.supplied_labels,
        callee: String::new(),
    })
}

fn completion_identifier_prefix_for_argument_label(
    source: &str,
    tokens: &[crate::lexer::Token],
    offset: usize,
) -> Option<(String, TextSpan)> {
    if tokens.iter().any(|token| {
        token.span.start < offset
            && offset <= token.span.end
            && token_blocks_argument_label_completion(token.kind)
    }) {
        return None;
    }
    if let Some(token) = tokens.iter().find(|token| {
        token.kind == TokenKind::Identifier
            && token.span.start <= offset
            && offset <= token.span.end
    }) {
        return Some((
            source[token.span.start..offset].to_string(),
            TextSpan::new(token.span.start, offset),
        ));
    }
    let prefix_span = TextSpan::new(offset, offset);
    previous_significant_completion_token_before_span(tokens, prefix_span)
        .is_some_and(|token| matches!(token.kind, TokenKind::LeftParen | TokenKind::Comma))
        .then_some((String::new(), prefix_span))
}

fn is_argument_label_position(source: &str, prefix_span: TextSpan) -> bool {
    let tokens = lex(source);
    let previous = tokens
        .into_iter()
        .take_while(|token| token.span.end <= prefix_span.start)
        .filter(|token| !token.kind.is_trivia())
        .last();
    matches!(
        previous.map(|token| token.kind),
        Some(TokenKind::LeftParen | TokenKind::Comma)
    )
}

fn combine_completion_candidates(
    candidates: Vec<EditorCompletionCandidate>,
) -> Vec<EditorCompletionCandidate> {
    let mut by_key = BTreeMap::<String, EditorCompletionCandidate>::new();
    let mut order = Vec::<String>::new();
    for candidate in candidates {
        let key = completion_candidate_key(&candidate);
        if !by_key.contains_key(&key) {
            order.push(key.clone());
            by_key.insert(key, candidate);
        }
    }
    order
        .into_iter()
        .filter_map(|key| by_key.remove(&key))
        .collect()
}

fn cap_completion_items(mut items: Vec<LspCompletionItem>) -> (Vec<LspCompletionItem>, bool) {
    items.sort_by(|left, right| {
        left.sort_text
            .as_deref()
            .unwrap_or(left.label.as_str())
            .cmp(right.sort_text.as_deref().unwrap_or(right.label.as_str()))
            .then_with(|| left.label.cmp(&right.label))
    });
    // A query-derived empty list must remain refreshable. VS Code otherwise
    // treats it as final, so erasing an unmatched prefix never asks Rust for
    // the now-matching candidates. Deliberately suppressed contexts bypass
    // this helper through `empty_completion_list` and stay complete.
    let is_incomplete = items.is_empty() || items.len() >= MAX_COMPLETION_ITEMS;
    if is_incomplete {
        items.truncate(MAX_COMPLETION_ITEMS);
    }
    (items, is_incomplete)
}

/// A response remains incomplete when a merged upstream response had already
/// omitted candidates. De-duplication or context filtering can reduce the
/// visible item count below the cap without making those omitted candidates
/// available again.
fn cap_completion_items_with_upstream_incomplete(
    items: Vec<LspCompletionItem>,
    upstream_is_incomplete: bool,
) -> (Vec<LspCompletionItem>, bool) {
    let (items, is_incomplete) = cap_completion_items(items);
    (items, is_incomplete || upstream_is_incomplete)
}

fn completion_candidate_key(candidate: &EditorCompletionCandidate) -> String {
    let name = candidate
        .name
        .as_deref()
        .unwrap_or(candidate.display.label.as_str());
    let signature = candidate.signature.as_deref().unwrap_or("");
    format!("{:?}:{name}:{signature}", candidate.kind)
}

pub(crate) fn empty_completion_list() -> LspCompletionList {
    LspCompletionList {
        is_incomplete: false,
        items: Vec::new(),
    }
}

pub(crate) fn apply_automatic_trigger_policy(
    report: LspCompletionReport,
    trigger_character: Option<&str>,
) -> LspCompletionReport {
    if trigger_character != Some(" ") || report.completion_context == "constructor" {
        return report;
    }
    let mut empty = empty_completion_report(report.parse_diagnostics);
    empty.timings = report.timings;
    empty
}

fn empty_completion_report(parse_diagnostics: usize) -> LspCompletionReport {
    LspCompletionReport {
        list: empty_completion_list(),
        query_quality: QueryQuality::Exact,
        recovery_reason: None,
        parse_diagnostics,
        completion_context: "none".to_string(),
        receiver_text: None,
        owner_type: None,
        prefix: String::new(),
        candidate_count: 0,
        source_kind_counts: BTreeMap::new(),
        origin_counts: BTreeMap::new(),
        failure_reason: None,
        timings: LspCompletionTimings::default(),
    }
}

fn completion_item_for_candidate(
    candidate: &EditorCompletionCandidate,
    edit_range: LspRange,
    insert_context: CompletionInsertContext,
    match_prefix: Option<&str>,
    order: usize,
    render_context: CompletionRenderContext<'_>,
) -> Option<LspCompletionItem> {
    let label = candidate
        .name
        .clone()
        .or_else(|| Some(candidate.display.label.clone()))?;
    let callable = callable_completion_render(&label, candidate, insert_context, render_context);
    let detail = candidate.signature.clone().or(candidate.detail.clone());
    let label_details = callable
        .as_ref()
        .map(|render| LspCompletionItemLabelDetails {
            detail: Some(render.call.parameters.clone()),
            description: render.call.result.clone(),
        })
        .or_else(|| completion_label_details(candidate));
    let generic_type_snippet = (candidate.kind == SymbolKind::Class
        && insert_context == CompletionInsertContext::Type
        && candidate.generic_type_parameter_count > 0)
        .then_some(GenericTypeSnippet {
            type_argument_count: candidate.generic_type_parameter_count,
        });
    let (new_text, insert_text_format) = callable
        .as_ref()
        .map(|render| (render.insert_text.clone(), Some(2)))
        .or_else(|| generic_type_snippet.map(|snippet| (snippet.snippet(&label), Some(2))))
        .unwrap_or_else(|| (label.clone(), None));
    let command = callable
        .as_ref()
        .map(|render| render.follow_up_command.clone())
        .or_else(|| generic_type_snippet.map(GenericTypeSnippet::follow_up_command));
    let documentation = completion_documentation(candidate, callable.as_ref());
    let required_parameter_count = callable
        .as_ref()
        .map(|render| render.call.required_parameter_count())
        .unwrap_or(0);
    let optional_parameter_count = callable
        .as_ref()
        .map(|render| render.call.optional_parameter_count())
        .unwrap_or(0);

    let text_edit = LspTextEdit {
        range: edit_range,
        new_text,
        replace_range: None,
    };
    let filter_text = label.clone();
    Some(LspCompletionItem {
        label: label.clone(),
        label_details,
        kind: completion_item_kind(candidate),
        detail,
        documentation,
        sort_text: Some(completion_sort_text(candidate, &label, match_prefix, order)),
        filter_text: Some(filter_text),
        preselect: None,
        insert_text_format,
        commit_characters: None,
        command,
        text_edit,
        required_parameter_count,
        optional_parameter_count,
    })
}

fn completion_label_details(
    candidate: &EditorCompletionCandidate,
) -> Option<LspCompletionItemLabelDetails> {
    let call = candidate.callable_signature_parts.clone()?;
    Some(LspCompletionItemLabelDetails {
        detail: Some(call.parameters),
        description: call.result,
    })
}

fn callable_completion_render(
    label: &str,
    candidate: &EditorCompletionCandidate,
    insert_context: CompletionInsertContext,
    render_context: CompletionRenderContext<'_>,
) -> Option<CallableCompletionRender> {
    let call = match candidate.kind {
        SymbolKind::Function
        | SymbolKind::Method
        | SymbolKind::Constructor
        | SymbolKind::Destructor => candidate.callable_signature_parts.clone()?,
        SymbolKind::Class
            if matches!(
                insert_context,
                CompletionInsertContext::ConstructorCall
                    | CompletionInsertContext::ContextualConstructorCall
            ) =>
        {
            let call = candidate.callable_signature_parts.clone().or_else(|| {
                (insert_context == CompletionInsertContext::ContextualConstructorCall).then(|| {
                    CallableSignatureParts {
                        parameters: "()".to_string(),
                        parameters_info: Vec::new(),
                        result: None,
                    }
                })
            })?;
            if let Some(insert_text) = rpl_rpc_attribute_template(label, &call, false) {
                return Some(CallableCompletionRender::from_insert(
                    call,
                    CallableInsertText {
                        text: insert_text,
                        enum_placeholder_defaults: rpl_rpc_enum_placeholder_defaults(),
                    },
                ));
            }
            let insert = callable_insert_text_with_context(label, &call, Some(render_context));
            return Some(CallableCompletionRender::from_insert(call, insert));
        }
        SymbolKind::Class
            if insert_context == CompletionInsertContext::Type
                && is_attribute_like_completion_candidate(candidate) =>
        {
            let call = candidate.callable_signature_parts.clone()?;
            if let Some(insert_text) = rpl_rpc_attribute_template(label, &call, true) {
                return Some(CallableCompletionRender::from_insert(
                    call,
                    CallableInsertText {
                        text: insert_text,
                        enum_placeholder_defaults: rpl_rpc_enum_placeholder_defaults(),
                    },
                ));
            }
            let mut insert = callable_insert_text_with_context(label, &call, Some(render_context));
            insert.text = format!("[{}]", insert.text);
            return Some(CallableCompletionRender::from_insert(call, insert));
        }
        _ => return None,
    };
    let insert = callable_insert_text_with_context(label, &call, Some(render_context));
    Some(CallableCompletionRender::from_insert(call, insert))
}

fn is_attribute_like_completion_candidate(candidate: &EditorCompletionCandidate) -> bool {
    candidate.kind == SymbolKind::Class && candidate.is_attribute_like
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallableCompletionRender {
    call: CallableSignatureParts,
    insert_text: String,
    follow_up_command: LspCommand,
}

impl CallableCompletionRender {
    fn from_insert(call: CallableSignatureParts, insert: CallableInsertText) -> Self {
        if !insert.enum_placeholder_defaults.is_empty() {
            Self::trigger_suggest_at_snippet_placeholders(
                call,
                insert.text,
                insert.enum_placeholder_defaults,
            )
        } else {
            Self::trigger_parameter_hints(call, insert.text)
        }
    }

    fn trigger_parameter_hints(call: CallableSignatureParts, insert_text: String) -> Self {
        Self {
            call,
            insert_text,
            follow_up_command: trigger_parameter_hints_command(),
        }
    }

    fn trigger_suggest_at_snippet_placeholders(
        call: CallableSignatureParts,
        insert_text: String,
        placeholder_defaults: Vec<String>,
    ) -> Self {
        Self {
            call,
            insert_text,
            follow_up_command: trigger_suggest_at_snippet_placeholder_command(placeholder_defaults),
        }
    }
}

#[cfg(test)]
fn callable_insert_text(label: &str, call: &CallableSignatureParts) -> String {
    callable_insert_text_with_context(label, call, None).text
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallableInsertText {
    text: String,
    /// Complete selected snippet-field text for every required enum parameter,
    /// in source signature order. Rust determines this from indexed type facts;
    /// the TypeScript bridge only observes the selected fields and opens Suggest.
    enum_placeholder_defaults: Vec<String>,
}

/// `RplRpc` has two required enum arguments but the engine's canonical RPC
/// examples use the safe request-to-server shape. Keep this a narrow,
/// signature-checked template rather than treating an enum's declaration order
/// as a default for arbitrary attribute constructors.
fn rpl_rpc_attribute_template(
    label: &str,
    call: &CallableSignatureParts,
    include_brackets: bool,
) -> Option<String> {
    let required = call.required_parameters().collect::<Vec<_>>();
    (label == "RplRpc"
        && required.len() == 2
        && callable_type_owner(&required[0].type_and_modifiers).as_deref() == Some("RplChannel")
        && callable_type_owner(&required[1].type_and_modifiers).as_deref() == Some("RplRcver"))
    .then(|| {
        // The complete default enum expressions are snippet fields. Typing
        // replaces the whole expression; the shared enum completion policy
        // ranks enum values first and retains normal value choices below.
        let body = format!(
            "RplRpc(${{1:{}}}, ${{2:{}}})",
            RPL_RPC_ENUM_PLACEHOLDER_DEFAULTS[0], RPL_RPC_ENUM_PLACEHOLDER_DEFAULTS[1],
        );
        if include_brackets {
            format!("[{body}]")
        } else {
            body
        }
    })
}

const RPL_RPC_ENUM_PLACEHOLDER_DEFAULTS: [&str; 2] = ["RplChannel.Reliable", "RplRcver.Server"];

fn rpl_rpc_enum_placeholder_defaults() -> Vec<String> {
    RPL_RPC_ENUM_PLACEHOLDER_DEFAULTS
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn callable_insert_text_with_context(
    label: &str,
    call: &CallableSignatureParts,
    render_context: Option<CompletionRenderContext<'_>>,
) -> CallableInsertText {
    let required = call.required_parameters().collect::<Vec<_>>();
    if call.parameters_info.is_empty() {
        return CallableInsertText {
            text: format!("{label}()"),
            enum_placeholder_defaults: Vec::new(),
        };
    }
    if required.is_empty() {
        return CallableInsertText {
            text: format!("{label}($0)"),
            enum_placeholder_defaults: Vec::new(),
        };
    }

    let mut enum_placeholder_defaults = Vec::new();
    let mut arguments = Vec::new();
    for (index, parameter) in required.iter().enumerate() {
        let argument = if let Some(owner) = enum_parameter_owner(parameter, render_context) {
            let placeholder = format!("{owner}.");
            enum_placeholder_defaults.push(placeholder.clone());
            format!(
                "${{{}:{}}}",
                index + 1,
                snippet_placeholder_text(&placeholder)
            )
        } else {
            let placeholder = parameter.name.clone();
            format!(
                "${{{}:{}}}",
                index + 1,
                snippet_placeholder_text(&placeholder)
            )
        };
        arguments.push(argument);
    }
    let arguments = arguments.join(", ");
    CallableInsertText {
        text: format!("{label}({arguments})"),
        enum_placeholder_defaults,
    }
}

fn trigger_parameter_hints_command() -> LspCommand {
    LspCommand {
        title: "Trigger Parameter Hints".to_string(),
        command: COMMAND_TRIGGER_PARAMETER_HINTS.to_string(),
        arguments: None,
    }
}

fn trigger_suggest_at_snippet_placeholder_command(placeholder_defaults: Vec<String>) -> LspCommand {
    trigger_suggest_at_snippet_placeholder_command_with_options(placeholder_defaults, false)
}

fn trigger_suggest_at_generic_type_placeholder_command(
    placeholder_defaults: Vec<String>,
) -> LspCommand {
    trigger_suggest_at_snippet_placeholder_command_with_options(placeholder_defaults, true)
}

fn trigger_suggest_at_snippet_placeholder_command_with_options(
    placeholder_defaults: Vec<String>,
    final_tabstop: bool,
) -> LspCommand {
    let mut arguments = placeholder_defaults
        .into_iter()
        .map(Value::String)
        .collect::<Vec<_>>();
    if final_tabstop {
        arguments.push(json!({ "finalTabstop": true }));
    }
    LspCommand {
        title: "Trigger enum suggestions".to_string(),
        command: COMMAND_TRIGGER_SUGGEST_AT_SNIPPET_PLACEHOLDER.to_string(),
        // The extension uses this ordered sequence only to recognize VS
        // Code's selected snippet fields. It never parses or classifies
        // source text, symbols, or parameters.
        arguments: Some(arguments),
    }
}

fn parameter_label_insert_value(
    parameter: &CallableParameter,
    render_context: Option<CompletionRenderContext<'_>>,
) -> String {
    enum_parameter_owner(parameter, render_context)
        .map(|owner| format!("${{0:{}}}", snippet_placeholder_text(&format!("{owner}."))))
        .unwrap_or_else(|| "$0".to_string())
}

fn enum_parameter_owner(
    parameter: &CallableParameter,
    render_context: Option<CompletionRenderContext<'_>>,
) -> Option<String> {
    let owner = callable_type_owner(&parameter.type_and_modifiers)?;
    render_context?.is_enum_owner(&owner).then_some(owner)
}

fn index_has_enum_owner(index: &SymbolIndex, owner: &str) -> bool {
    index.top_level_symbols_for_name(owner).iter().any(|id| {
        index
            .symbol(*id)
            .is_some_and(|symbol| symbol.kind == SymbolKind::Enum)
    })
}

fn snippet_placeholder_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('}', "\\}")
}

fn completion_documentation(
    candidate: &EditorCompletionCandidate,
    callable: Option<&CallableCompletionRender>,
) -> Option<LspMarkupContent> {
    let mut sections = Vec::new();
    if let Some(render) = callable {
        let parameter_docs = callable_parameter_documentation(&render.call);
        if !parameter_docs.is_empty() {
            sections.push(parameter_docs);
        }
    }
    if let Some(preview) = candidate.display.documentation_preview.as_ref() {
        if !preview.trim().is_empty() {
            sections.push(preview.clone());
        }
    }
    if sections.is_empty() {
        None
    } else {
        Some(LspMarkupContent {
            kind: "markdown".to_string(),
            value: sections.join("\n\n"),
        })
    }
}

fn callable_parameter_documentation(call: &CallableSignatureParts) -> String {
    let mut output = String::new();
    let required = call.required_parameters().collect::<Vec<_>>();
    let optional = call.optional_parameters().collect::<Vec<_>>();

    if !required.is_empty() {
        output.push_str("**Required**\n");
        for parameter in required {
            output.push_str(&format!(
                "- `{}`{}\n",
                parameter.name,
                parameter_type_suffix(parameter)
            ));
        }
    }
    if !optional.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("**Optional**\n");
        for parameter in optional {
            output.push_str(&format!(
                "- `{}`{} = `{}`\n",
                parameter.name,
                parameter_type_suffix(parameter),
                parameter
                    .default_text
                    .as_deref()
                    .map(escape_markdown_inline_code)
                    .unwrap_or_default()
            ));
        }
    }

    output
}

fn parameter_type_suffix(parameter: &CallableParameter) -> String {
    if parameter.type_and_modifiers.is_empty() {
        String::new()
    } else {
        format!(
            ": `{}`",
            escape_markdown_inline_code(&parameter.type_and_modifiers)
        )
    }
}

fn escape_markdown_inline_code(value: &str) -> String {
    value.replace('`', "\\`")
}

fn completion_sort_text(
    candidate: &EditorCompletionCandidate,
    label: &str,
    match_prefix: Option<&str>,
    order: usize,
) -> String {
    let match_rank = match_prefix
        .and_then(|prefix| completion_name_match_rank(label, prefix))
        .unwrap_or(u16::MAX);
    format!(
        "{:03}:{:02}:{:02}:{:05}:{:03}:{}",
        match_rank,
        completion_source_rank(candidate),
        completion_origin_sort_rank(candidate.origin),
        order,
        completion_item_kind_rank(candidate.kind),
        label
    )
}

fn completion_source_rank(candidate: &EditorCompletionCandidate) -> u8 {
    match candidate.source_kind {
        SourceKind::Unknown => 0,
        SourceKind::Workspace => 1,
        SourceKind::GameData => 2,
        SourceKind::Fixture => 3,
    }
}

fn completion_origin_sort_rank(origin: EditorCompletionOrigin) -> u8 {
    match origin {
        EditorCompletionOrigin::Direct => 0,
        EditorCompletionOrigin::Overlay => 1,
        EditorCompletionOrigin::Inherited => 2,
        EditorCompletionOrigin::Unknown => 3,
    }
}

fn completion_item_kind_rank(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Class => 0,
        SymbolKind::Enum => 1,
        SymbolKind::Typedef => 2,
        SymbolKind::Function => 3,
        SymbolKind::Method => 4,
        SymbolKind::Constructor => 5,
        SymbolKind::Destructor => 6,
        SymbolKind::Field | SymbolKind::GlobalField => 7,
        SymbolKind::EnumMember => 8,
        SymbolKind::Parameter | SymbolKind::LocalVariable | SymbolKind::PreprocessorMacro => 9,
        _ => 99,
    }
}

fn completion_item_kind(candidate: &EditorCompletionCandidate) -> u32 {
    match candidate.kind {
        SymbolKind::Method | SymbolKind::Destructor => 2,
        SymbolKind::Function => 3,
        SymbolKind::Constructor => 4,
        SymbolKind::Field if is_constant_completion_candidate(candidate) => 21,
        SymbolKind::Field => 5,
        SymbolKind::GlobalField if is_constant_completion_candidate(candidate) => 21,
        SymbolKind::GlobalField | SymbolKind::LocalVariable | SymbolKind::Parameter => 6,
        SymbolKind::Class => 7,
        SymbolKind::Enum => 13,
        SymbolKind::EnumMember => 20,
        SymbolKind::PreprocessorMacro => 21,
        SymbolKind::Typedef | SymbolKind::TypeParameter => 25,
    }
}

fn is_constant_completion_candidate(candidate: &EditorCompletionCandidate) -> bool {
    candidate
        .display
        .modifiers
        .iter()
        .any(|modifier| matches!(modifier.as_str(), "const"))
}

fn completion_lsp_kind_label(kind: u32) -> &'static str {
    match kind {
        2 => "Method",
        3 => "Function",
        4 => "Constructor",
        5 => "Field",
        6 => "Variable",
        7 => "Class",
        13 => "Enum",
        14 => "Keyword",
        20 => "EnumMember",
        21 => "Constant",
        25 => "TypeParameter",
        _ => "Property",
    }
}

fn format_label_details(details: Option<&LspCompletionItemLabelDetails>) -> String {
    let Some(details) = details else {
        return String::new();
    };
    match (details.detail.as_deref(), details.description.as_deref()) {
        (Some(detail), Some(description)) => format!("{detail} {description}"),
        (Some(detail), None) => detail.to_string(),
        (None, Some(description)) => description.to_string(),
        (None, None) => String::new(),
    }
}

fn escape_markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('|', "\\|")
        .replace('\r', "")
        .replace('\n', " ")
}

fn markdown_table_text(value: &str) -> String {
    escape_markdown_cell(value.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_range() -> LspRange {
        LspRange {
            start: LspPosition {
                line: 0,
                character: 0,
            },
            end: LspPosition {
                line: 0,
                character: 5,
            },
        }
    }

    #[test]
    fn current_snapshot_partial_new_completion_uses_the_declared_collection_type() {
        let source = "class Example\n{\n\tvoid Run()\n\t{\n\t\tarray<int> tesyArray = n\n\t}\n}\n";
        let offset = source.find("tesyArray = n").unwrap() + "tesyArray = n".len();
        let report =
            completion_report_for_current_contextual_constructor_at_offset_with_external_indexes(
                source, offset, None, None,
            )
            .expect("the bounded current snapshot proves the declaration type");

        assert_eq!(report.completion_context, "constructor");
        assert_eq!(report.owner_type.as_deref(), Some("array<int>"));
        assert_eq!(report.list.items.len(), 1);
        assert_eq!(report.list.items[0].label, "new array<int>()");
        assert_eq!(
            report.list.items[0].filter_text.as_deref(),
            Some("new array<int>()")
        );
        assert_eq!(report.list.items[0].text_edit.new_text, "new array<int>()");
        assert_eq!(
            report.list.items[0].text_edit.range,
            range_for_span(source, TextSpan::new(offset - "n".len(), offset),)
        );
    }

    #[test]
    fn current_snapshot_partial_new_completion_uses_an_indexed_class_constructor() {
        let source = "class Example\n{\n\tvoid Run()\n\t{\n\t\tManaged value = n\n\t}\n}\n";
        let offset = source.find("value = n").unwrap() + "value = n".len();
        let external =
            file_index_for_source("class Managed\n{\n\tvoid Managed(int id);\n}\n").index;
        let report =
            completion_report_for_current_contextual_constructor_at_offset_with_external_indexes(
                source,
                offset,
                Some(&external),
                None,
            )
            .expect("the indexed class proves a constructor candidate");

        assert_eq!(report.completion_context, "constructor");
        assert_eq!(report.owner_type.as_deref(), Some("Managed"));
        assert!(report.list.is_incomplete);
        assert_eq!(report.list.items.len(), 1);
        assert_eq!(report.list.items[0].label, "new Managed");
        assert_eq!(
            report.list.items[0].text_edit.new_text,
            "new Managed(${1:id})"
        );
    }

    #[test]
    fn lexical_pending_completion_uses_current_prefix_and_external_top_level_symbols() {
        let external = file_index_for_source("class GetGameMode {}").index;
        let report = completion_report_for_lexical_source_with_external_indexes(
            "getga",
            LspPosition {
                line: 0,
                character: 5,
            },
            Some(&external),
            None,
        );

        assert_eq!(report.prefix, "getga");
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "GetGameMode"));
        assert_eq!(report.query_quality, QueryQuality::Unavailable);
        assert_eq!(
            report.recovery_reason.as_deref(),
            Some("current-revision-local-facts-pending")
        );
    }

    #[test]
    fn lexical_pending_completion_keeps_later_prefix_matches_for_incremental_typing() {
        let mut external_source = String::new();
        for index in 0..32 {
            external_source.push_str(&format!("class R{index:02} {{}}\n"));
        }
        external_source.push_str("class Resource {}\nclass ResourceName {}\n");
        let external = file_index_for_source(&external_source).index;
        let report = completion_report_for_lexical_source_with_external_indexes(
            "r",
            LspPosition {
                line: 0,
                character: 1,
            },
            Some(&external),
            None,
        );

        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "ResourceName"));
    }

    #[test]
    fn pending_override_completion_keeps_later_top_level_matches_for_incremental_typing() {
        let source = "class Child : ScriptComponent { r }";
        let mut external_source = String::from("class ScriptComponent {}\n");
        for index in 0..32 {
            external_source.push_str(&format!("class R{index:02} {{}}\n"));
        }
        external_source.push_str("class Resource {}\nclass ResourceName {}\n");
        let external = file_index_for_source(&external_source).index;
        let offset = source.find(" r }").unwrap() + 2;
        let report = completion_report_for_current_override_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("the pending override response should retain top-level candidates");

        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "ResourceName"));
    }

    #[test]
    fn pending_super_completion_inserts_the_matching_base_call() {
        let source = r#"class Child : Base
{
	override void OnPostInit(IEntity owner)
	{
		sup
	}
}
"#;
        let external =
            file_index_for_source("class Base { void OnPostInit(IEntity owner); }").index;
        let offset = source.find("sup\n").unwrap() + 3;

        let report = completion_report_for_current_super_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("the pending super response should be available");
        let item = report
            .list
            .items
            .iter()
            .find(|item| item.label == "super.OnPostInit")
            .expect("expected a matching base-call completion");
        assert_eq!(item.text_edit.new_text, "super.OnPostInit(${1:owner})");
        assert_eq!(item.insert_text_format, Some(2));
    }

    #[test]
    fn lexical_pending_completion_keeps_collection_snippets_at_declaration_boundaries() {
        let source = "class Example { arr";
        let external = file_index_for_source("int array;").index;
        let report = completion_report_for_lexical_source_with_external_indexes(
            source,
            LspPosition {
                line: 0,
                character: source.len() as u32,
            },
            Some(&external),
            None,
        );
        let array = report
            .list
            .items
            .iter()
            .find(|item| item.label == "array")
            .expect("array completion");

        assert_eq!(array.kind, 14);
        assert_eq!(array.text_edit.new_text, "array<${1}>$0");
        assert_eq!(array.insert_text_format, Some(2));
        assert_eq!(
            report
                .list
                .items
                .iter()
                .filter(|item| item.label == "array")
                .count(),
            1
        );
    }

    #[test]
    fn lexical_pending_empty_generic_slot_keeps_indexed_type_candidates() {
        let source = "class Example { void Run(Tuple2<int, > value) {} }";
        let external = file_index_for_source("class GameCollectionElement {}").index;
        assert!(top_level_source_completion_candidates(
            "",
            EditorTopLevelCompletionMode::Type,
            &SymbolIndex::default(),
            Some(&external),
            None,
            32,
        )
        .iter()
        .any(|candidate| candidate.name.as_deref() == Some("GameCollectionElement")));
        let offset = source.find("Tuple2<int, ").unwrap() + "Tuple2<int, ".len();
        assert!(empty_generic_type_slot_before_offset(source, offset));
        let report = completion_report_for_lexical_source_with_external_indexes(
            source,
            range_for_span(source, TextSpan::new(0, offset)).end,
            Some(&external),
            None,
        );

        assert_eq!(report.completion_context, "type");
        assert!(report.list.items.iter().any(|item| item.label == "int"));
        assert!(
            report
                .list
                .items
                .iter()
                .any(|item| item.label == "GameCollectionElement"),
            "{report:?}"
        );
    }

    #[test]
    fn nested_generic_closers_preserve_the_outer_empty_type_slot() {
        let source = "class Example { map<array<array<int>>, > values; }";
        let offset = source.find(">, ").unwrap() + 3;
        assert!(empty_generic_type_slot_before_offset(source, offset));
    }

    #[test]
    fn pending_completion_keeps_top_level_suggestions_when_a_large_valid_file_window_ends_in_a_comment(
    ) {
        let external = file_index_for_source("class GetGameMode {}").index;
        let mut source = "void Padding() {}\n".repeat(1_100);
        source.push_str("getga\n/*");
        source.push_str(&"comment ".repeat(400));
        source.push_str("*/\n");
        let offset = source.find("getga").unwrap() + "getga".len();

        let report = completion_report_for_lexical_source_at_offset_with_external_indexes(
            &source,
            offset,
            Some(&external),
            None,
        );

        assert!(BoundedCompletionRegion::new(&source, offset).is_some());
        assert_eq!(report.prefix, "getga");
        assert_eq!(report.query_quality, QueryQuality::Unavailable);
        assert_eq!(
            report.recovery_reason.as_deref(),
            Some("current-revision-local-facts-pending")
        );
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "GetGameMode"));
    }

    #[test]
    fn completion_suppresses_items_inside_line_and_block_comments() {
        let external = file_index_for_source("class GetGameMode {}").index;
        for source in ["// GetGameMode", "/* GetGameMode */", "/* GetGameMode"] {
            let offset = source.len();
            let lexical = completion_report_for_lexical_source_at_offset_with_external_indexes(
                source,
                offset,
                Some(&external),
                None,
            );
            assert!(
                lexical.list.items.is_empty(),
                "pending completion: {source}"
            );

            let analysis = file_index_for_source(source);
            let cached =
                completion_report_for_offset(source, &analysis, offset, Some(&external), None);
            assert!(cached.list.items.is_empty(), "cached completion: {source}");
        }
    }

    #[test]
    fn pending_member_completion_uses_top_level_fallback_without_receiver_facts() {
        let external = file_index_for_source("class GetGameMode {}").index;
        let source = "instance.getga";
        let report = completion_report_for_lexical_source_with_external_indexes(
            source,
            LspPosition {
                line: 0,
                character: source.len() as u32,
            },
            Some(&external),
            None,
        );

        assert_eq!(report.query_quality, QueryQuality::Unavailable);
        assert_eq!(
            report.completion_context,
            "member-unavailable-top-level-fallback"
        );
        assert_eq!(
            report.recovery_reason.as_deref(),
            Some("current-revision-receiver-facts-pending")
        );
        assert!(report.receiver_text.is_none());
        assert!(report.owner_type.is_none());
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "GetGameMode"));
    }

    #[test]
    fn pending_member_completion_remains_incomplete_until_current_snapshot_facts_are_available() {
        let external = file_index_for_source("class GetGameMode {}").index;
        let source = "instance.getga";
        let report = completion_report_for_lexical_source_with_external_indexes(
            source,
            LspPosition {
                line: 0,
                character: source.len() as u32,
            },
            Some(&external),
            None,
        );

        assert_eq!(report.query_quality, QueryQuality::Unavailable);
        assert!(
            report.list.is_incomplete,
            "a fallback without current receiver facts must stay refreshable"
        );
    }

    #[test]
    fn pending_argument_completion_uses_top_level_fallback_without_argument_facts() {
        let external = file_index_for_source("class GetGameMode {}").index;
        let source = "Run(getga";
        let report = completion_report_for_lexical_source_with_external_indexes(
            source,
            LspPosition {
                line: 0,
                character: source.len() as u32,
            },
            Some(&external),
            None,
        );

        assert_eq!(report.query_quality, QueryQuality::Unavailable);
        assert_eq!(
            report.completion_context,
            "argument-unavailable-top-level-fallback"
        );
        assert_eq!(
            report.recovery_reason.as_deref(),
            Some("current-revision-argument-facts-pending")
        );
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "GetGameMode"));
    }

    #[test]
    fn current_argument_query_returns_labels_for_a_resolved_bare_callable() {
        let external = file_index_for_source(
            "void Run(int firstValue, string secondValue, bool optionalValue = true) {}",
        )
        .index;
        let source = "class Example { void Test() { Run(sec); } }";
        let offset = source.find("Run(sec").unwrap() + "Run(sec".len();

        let report = completion_report_for_current_argument_labels_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("a valid bare callable with source-backed external facts should be exact");

        assert_eq!(report.query_quality, QueryQuality::Exact);
        assert_eq!(report.completion_context, "argument-label");
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "secondValue"));
        assert!(!report
            .list
            .items
            .iter()
            .any(|item| item.label == "firstValue"));
    }

    #[test]
    fn current_argument_query_uses_the_shared_builtin_attribute_fact() {
        let source = "class Example { [Attribute(uiw)] int m_Value; }";
        let offset = source.find("Attribute(uiw").unwrap() + "Attribute(uiw".len();

        let report = completion_report_for_current_argument_labels_at_offset_with_external_indexes(
            source, offset, None, None,
        )
        .expect("the built-in Attribute fact should be available before rich analysis");

        assert_eq!(report.query_quality, QueryQuality::Exact);
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "uiwidget"));
    }

    #[test]
    fn current_local_scope_query_returns_current_locals_without_ready_analysis() {
        let source = r#"class Example
{
	void Run(int parameter)
	{
		string localValue;
		loc
	}
}"#;
        let offset = source.find("loc\n").unwrap() + 3;

        let report = completion_report_for_current_local_scope_at_offset_with_external_indexes(
            source, offset, None, None,
        )
        .expect("valid callable-local source should use the current local query");

        assert_eq!(report.query_quality, QueryQuality::Exact);
        assert_eq!(report.completion_context, "local");
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "localValue"));
    }

    #[test]
    fn current_argument_value_query_keeps_inherited_members_without_ready_analysis() {
        let external = file_index_for_source(
            r#"class ScriptComponent
{
	proto external GenericEntity GetOwner();
}
"#,
        )
        .index;
        let source = r#"class Example : ScriptComponent
{
	void Consume(GenericEntity value) {}
	void Run()
	{
		Consume(GetOwn);
	}
}
"#;
        let offset = source.find("GetOwn").unwrap() + "GetOwn".len();

        let report = completion_report_for_current_local_scope_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("a current argument expression should retain proven inherited values");

        assert_eq!(report.query_quality, QueryQuality::Exact);
        assert_eq!(report.completion_context, "local");
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "GetOwner"));
    }

    #[test]
    fn current_override_query_returns_script_component_events_without_ready_analysis() {
        let source = r#"class Child : ScriptComponent
{
	on
}
"#;
        let external = file_index_for_source(
            r#"class ScriptComponent
{
	event protected void OnPostInit(IEntity owner);
}
"#,
        )
        .index;
        let offset = source.find("\ton").unwrap() + 3;
        let report = completion_report_for_current_override_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("current class header should prove an override context");

        assert_eq!(report.completion_context, "override");
        assert_eq!(report.prefix, "on");
        assert!(report.list.items.iter().any(|item| {
            item.label == "OnPostInit"
                && item
                    .text_edit
                    .new_text
                    .contains("override protected void OnPostInit(IEntity owner)")
        }));
    }

    #[test]
    fn override_completion_preserves_an_existing_following_body() {
        let source = r#"class Child : ScriptComponent
{
	override protected on
	// Keep this body while completing the declaration.
	{
		int value = 1;
	}
}
"#;
        let external = file_index_for_source(
            r#"class ScriptComponent
{
	event protected void OnPostInit(IEntity owner);
}
"#,
        )
        .index;
        let offset = source.find("protected on").unwrap() + "protected on".len();

        let pending = completion_report_for_current_override_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("pending query should recognize the incomplete override");
        let analysis = file_index_for_source(source);
        let cached = completion_report_for_offset(source, &analysis, offset, Some(&external), None);

        for report in [&pending, &cached] {
            let item = report
                .list
                .items
                .iter()
                .find(|item| item.label == "OnPostInit")
                .expect("expected OnPostInit override completion");
            assert_eq!(item.text_edit.new_text, "void OnPostInit(IEntity owner)");
        }
    }

    #[test]
    fn current_override_query_offers_the_override_keyword_without_generic_fallback_items() {
        let source = r#"class Child : ScriptComponent
{
	overr
}
"#;
        let external = file_index_for_source(
            r#"class ScriptComponent
{
	event protected void OnPostInit(IEntity owner);
}
"#,
        )
        .index;
        let offset = source.find("\toverr").unwrap() + "\toverr".len();
        let report = completion_report_for_current_override_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("current class header should prove an override context");

        assert_eq!(report.completion_context, "override");
        assert_eq!(report.prefix, "overr");
        assert_eq!(
            report.list.items.first().map(|item| item.label.as_str()),
            Some("override")
        );
        assert!(report
            .list
            .items
            .iter()
            .all(|item| item.label == "override"));
    }

    #[test]
    fn current_override_query_falls_through_when_the_prefix_has_no_override_match() {
        let source = r#"class Child : ScriptComponent
{
	SCR_
}
"#;
        let external = file_index_for_source(
            r#"class ScriptComponent
{
	event protected void OnPostInit(IEntity owner);
}
"#,
        )
        .index;
        let offset = source.find("\tSCR_").unwrap() + "\tSCR_".len();

        assert!(
            completion_report_for_current_override_at_offset_with_external_indexes(
                source,
                offset,
                Some(&external),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn current_override_query_merges_attribute_completion_with_override_members() {
        let source = r#"class Child : ScriptComponent
{
	rpl
}
"#;
        let external = file_index_for_source(
            r#"enum RplChannel { Reliable }
enum RplRcver { Server }
class UniqueAttribute {}
class RplRpc : UniqueAttribute
{
	void RplRpc(RplChannel channel, RplRcver rcver);
}
class ScriptComponent
{
	protected void RplLoad();
	protected void RplSave();
}
"#,
        )
        .index;
        let offset = source.find("\trpl").unwrap() + "\trpl".len();
        let report = completion_report_for_current_override_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("matching override and attribute candidates should share one pending response");

        assert!(report.list.items.iter().any(|item| item.label == "RplLoad"));
        let rpc = report
            .list
            .items
            .iter()
            .find(|item| item.label == "RplRpc")
            .expect("expected RplRpc attribute completion");
        assert_eq!(
            rpc.text_edit.new_text,
            "[RplRpc(${1:RplChannel.Reliable}, ${2:RplRcver.Server})]"
        );
        assert_eq!(
            rpc.command.as_ref().map(|command| command.command.as_str()),
            Some("reforger-sript-tools.completion.triggerSuggestAtSnippetPlaceholder")
        );
        assert_eq!(
            rpc.command
                .as_ref()
                .and_then(|command| command.arguments.as_ref())
                .cloned(),
            Some(vec![json!("RplChannel.Reliable"), json!("RplRcver.Server")])
        );
    }

    #[test]
    fn current_override_query_keeps_a_nearby_class_header_in_a_large_file() {
        let source = format!(
            "{}class Child : ScriptComponent\n{{\n\ton\n}}\n",
            "int padding;\n".repeat(2_000)
        );
        let external = file_index_for_source(
            r#"class ScriptComponent
{
	event protected void OnPostInit(IEntity owner);
}
"#,
        )
        .index;
        let offset = source.rfind("\ton").unwrap() + 3;

        let report = completion_report_for_current_override_at_offset_with_external_indexes(
            &source,
            offset,
            Some(&external),
            None,
        )
        .expect("nearby class header should stay within the fixed override window");

        assert_eq!(report.completion_context, "override");
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "OnPostInit"));
    }

    #[test]
    fn current_override_query_declines_callable_bodies() {
        let source = r#"class Child : ScriptComponent
{
	void Run()
	{
		on
	}
}
"#;
        let external = file_index_for_source(
            r#"class ScriptComponent
{
	event protected void OnPostInit(IEntity owner);
}
"#,
        )
        .index;
        let offset = source.rfind("on").unwrap() + 2;

        assert!(
            completion_report_for_current_override_at_offset_with_external_indexes(
                source,
                offset,
                Some(&external),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn current_local_scope_query_declines_malformed_or_member_source() {
        let malformed = "void Run() { int localValue; loc";
        assert!(
            completion_report_for_current_local_scope_at_offset_with_external_indexes(
                malformed,
                malformed.len(),
                None,
                None,
            )
            .is_none()
        );

        let member = "void Run() { localValue.loc }";
        let offset = member.find("loc }").unwrap() + 3;
        assert!(
            completion_report_for_current_local_scope_at_offset_with_external_indexes(
                member, offset, None, None,
            )
            .is_none()
        );
    }

    #[test]
    fn current_receiver_query_resolves_simple_current_locals_parameters_and_fields() {
        let external =
            file_index_for_source("class Widget { void GetName() {} void GetSize() {} }").index;
        let source = r#"class Example
{
	Widget m_widget;
	void Run(Widget parameter)
	{
		Widget localValue;
		localValue.Get
		parameter.Get
		m_widget.Get
	}
}"#;

        for receiver in ["localValue.Get", "parameter.Get", "m_widget.Get"] {
            let offset = source.find(receiver).unwrap() + receiver.len();
            let report = completion_report_for_current_receiver_at_offset_with_external_indexes(
                source,
                offset,
                Some(&external),
                None,
            )
            .unwrap_or_else(|| panic!("{receiver} should use the current receiver query"));

            assert_eq!(report.query_quality, QueryQuality::Exact);
            assert_eq!(report.completion_context, "member");
            assert_eq!(
                report.receiver_text.as_deref(),
                Some(&receiver[..receiver.len() - 4])
            );
            assert_eq!(report.owner_type.as_deref(), Some("Widget"));
            assert!(report.list.items.iter().any(|item| item.label == "GetName"));
        }
    }

    #[test]
    fn current_receiver_query_uses_valid_full_expression_edits_for_enum_values() {
        let external = file_index_for_source(
            r#"enum RplChannel
{
	Reliable,
	Unreliable
}
class OtherChoice {}
"#,
        )
        .index;
        let source = r#"class Example
{
	void Run()
	{
		RplChannel.Reliable
	}
}
"#;
        let offset = source.find("RplChannel.Reliable").unwrap() + "RplChannel.Reliable".len();

        let report = completion_report_for_current_receiver_at_offset_with_external_indexes(
            source,
            offset,
            None,
            Some(&external),
        )
        .expect("an externally indexed enum owner should complete before foreground analysis");

        assert_eq!(report.query_quality, QueryQuality::Exact);
        assert_eq!(report.completion_context, "member");
        assert_eq!(report.receiver_text.as_deref(), Some("RplChannel"));
        assert_eq!(report.owner_type.as_deref(), Some("RplChannel"));
        let labels = report
            .list
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels[..2],
            ["RplChannel.Reliable", "RplChannel.Unreliable"]
        );
        assert!(
            labels.len() > 2,
            "expected normal value fallbacks: {labels:?}"
        );
        assert_eq!(
            report.list.items[0].text_edit.new_text,
            "RplChannel.Reliable"
        );
        for item in &report.list.items {
            assert_eq!(item.filter_text.as_deref(), Some("RplChannel.Reliable"));
            assert_eq!(item.text_edit.range.start.character, 2);
            assert_eq!(item.text_edit.range.end.character, 21);
            assert!(
                item.text_edit.replace_range.is_none(),
                "the full-expression edit must not serialize an invalid InsertReplaceEdit"
            );
        }
        assert!(
            !report.list.items[2]
                .text_edit
                .new_text
                .starts_with("RplChannel."),
            "a normal fallback must replace rather than extend the enum expression"
        );
    }

    #[test]
    fn static_enum_completion_keeps_value_fallbacks_in_attribute_call_and_constructor_contexts() {
        let external = file_index_for_source(
            r#"enum Channel
{
	Reliable,
	Unreliable
}
class AttributeTarget : UniqueAttribute
{
	void AttributeTarget(Channel channel);
}
class Receiver
{
	void Receive(Channel channel);
}
class Constructed
{
	void Constructed(Channel channel);
}
"#,
        )
        .index;
        let contexts = [
            "[AttributeTarget(Channel.Reliable)] class Example {}",
            "class Example { void Run() { Receiver.Receive(Channel.Reliable); } }",
            "class Example { void Run() { new Constructed(Channel.Reliable); } }",
        ];

        for source in contexts {
            let needle = "Channel.Reliable";
            let offset = source.find(needle).unwrap() + needle.len();
            let position = range_for_span(source, TextSpan::new(0, offset)).end;
            let report = completion_report_for_source_position_with_external(
                source,
                position,
                Some(&external),
            );
            let labels = report
                .list
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>();
            assert_eq!(labels[..2], ["Channel.Reliable", "Channel.Unreliable"]);
            assert!(
                labels.len() > 2,
                "expected normal fallbacks for {source}: {labels:?}"
            );
            assert!(report
                .list
                .items
                .iter()
                .all(|item| item.text_edit.replace_range.is_none()));
        }
    }

    #[test]
    fn filtered_empty_completion_list_requests_refresh_after_erasing() {
        let (items, is_incomplete) = cap_completion_items(Vec::new());

        assert!(items.is_empty());
        assert!(is_incomplete);
        assert!(!empty_completion_list().is_incomplete);
    }

    #[test]
    fn merged_completion_list_keeps_the_upstream_incomplete_signal() {
        let (items, is_incomplete) = cap_completion_items_with_upstream_incomplete(
            vec![bounded_completion_item(
                "x",
                TextSpan::new(0, 1),
                "x".to_string(),
                None,
                0,
            )],
            true,
        );

        assert_eq!(items.len(), 1);
        assert!(is_incomplete);
    }

    #[test]
    fn current_receiver_query_ranks_current_values_for_a_static_enum_without_analysis() {
        let external = file_index_for_source(
            r#"enum RplChannel
{
	Reliable,
	Unreliable
}
class ScriptComponent
{
	void OnPostInit(IEntity owner);
}
"#,
        )
        .index;
        let source = r#"class Example : ScriptComponent
{
	int m_Field;
	void CurrentMethod();
	void Run(IEntity owner)
	{
		int localValue;
		RplChannel.Reliable
	}
}
"#;
        let needle = "RplChannel.Reliable";
        let offset = source.find(needle).unwrap() + needle.len();

        let report = completion_report_for_current_receiver_at_offset_with_external_indexes(
            source,
            offset,
            Some(&external),
            None,
        )
        .expect("the current bounded enum query should complete");
        let labels = report
            .list
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            labels[..2],
            ["RplChannel.Reliable", "RplChannel.Unreliable"]
        );
        for label in [
            "localValue",
            "owner",
            "m_Field",
            "CurrentMethod",
            "OnPostInit",
        ] {
            assert!(
                labels.contains(&label),
                "pending enum fallback must retain {label}: {labels:?}"
            );
        }
        let first_keyword = report
            .list
            .items
            .iter()
            .position(|item| item.kind == 14)
            .expect("the normal keyword fallback remains available");
        for label in [
            "localValue",
            "owner",
            "m_Field",
            "CurrentMethod",
            "OnPostInit",
        ] {
            assert!(
                labels
                    .iter()
                    .position(|candidate| *candidate == label)
                    .unwrap()
                    < first_keyword,
                "current value {label} must rank before keywords: {labels:?}"
            );
        }
        assert!(report.list.items.iter().all(|item| {
            item.filter_text.as_deref() == Some("RplChannel.Reliable")
                && item.text_edit.replace_range.is_none()
        }));
    }

    #[test]
    fn static_enum_completion_ranks_enum_members_without_hiding_contextual_values() {
        let source = r#"enum Channel
{
	Reliable,
	Unreliable
}

class Example
{
	int m_ClassValue;

	void Helper();

	void Run(int parameter)
	{
		int localValue;
		Channel.Reliable
	}
}
"#;
        let needle = "Channel.Reliable";
        let offset = source.find(needle).unwrap() + needle.len();
        let position = range_for_span(source, TextSpan::new(0, offset)).end;

        let report = completion_report_for_source_position_with_external(source, position, None);
        let labels = report
            .list
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(labels[..2], ["Channel.Reliable", "Channel.Unreliable"]);
        for label in ["localValue", "parameter", "m_ClassValue", "Helper"] {
            assert!(
                labels.contains(&label),
                "enum fallback must retain {label}: {labels:?}"
            );
        }
        for item in &report.list.items {
            assert_eq!(item.filter_text.as_deref(), Some("Channel.Reliable"));
            assert!(
                item.text_edit.replace_range.is_none(),
                "the full-expression edit must remain an ordinary text edit"
            );
        }
    }

    #[test]
    fn insert_replace_edits_require_a_shared_start_and_prefix_insert_range() {
        let insert = LspRange {
            start: LspPosition {
                line: 0,
                character: 4,
            },
            end: LspPosition {
                line: 0,
                character: 7,
            },
        };
        let replace = LspRange {
            start: LspPosition {
                line: 0,
                character: 4,
            },
            end: LspPosition {
                line: 0,
                character: 12,
            },
        };
        assert!(insert_replace_ranges_are_valid(&insert, &replace));
        let invalid_insert = LspRange {
            start: LspPosition {
                line: 0,
                character: 8,
            },
            end: LspPosition {
                line: 0,
                character: 12,
            },
        };
        assert!(!insert_replace_ranges_are_valid(&invalid_insert, &replace));
    }

    #[test]
    fn current_receiver_query_resolves_externally_indexed_static_class_members() {
        let external = file_index_for_source(
            r#"class ScriptApi
{
	static int s_Value;
	static void StaticRun();
	int m_Value;
	void InstanceRun();
}
"#,
        )
        .index;
        let source = r#"class ScriptApi
{
	static int s_Value;
	static void StaticRun();
	int m_Value;
	void InstanceRun();
}

class Example
{
	void Run()
	{
		ScriptApi.
	}
}
"#;
        let offset = source.find("ScriptApi.").unwrap() + "ScriptApi.".len();

        let report = completion_report_for_current_receiver_at_offset_with_external_indexes(
            source,
            offset,
            None,
            Some(&external),
        )
        .expect("an externally indexed static class should complete before foreground analysis");

        assert_eq!(report.owner_type.as_deref(), Some("ScriptApi"));
        assert!(report.list.items.iter().any(|item| item.label == "s_Value"));
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "StaticRun"));
        assert!(
            !report.list.items.iter().any(|item| item.label == "m_Value"),
            "a static owner must not expose instance fields"
        );
        assert!(
            !report
                .list
                .items
                .iter()
                .any(|item| item.label == "InstanceRun"),
            "a static owner must not expose instance methods"
        );
    }

    #[test]
    fn current_receiver_query_does_not_treat_a_visible_binding_as_a_static_owner() {
        let external = file_index_for_source(
            r#"enum RplChannel
{
	Reliable,
	Unreliable
}
"#,
        )
        .index;
        let source = r#"class Example
{
	void Run(LocalReceiver RplChannel)
	{
		RplChannel.
	}
}
"#;
        let offset = source.rfind("RplChannel.").unwrap() + "RplChannel.".len();

        let report = completion_report_for_current_receiver_at_offset_with_external_indexes(
            source,
            offset,
            None,
            Some(&external),
        );

        let report = report.expect("the visible local binding should use its own type");
        assert_eq!(report.owner_type.as_deref(), Some("LocalReceiver"));
        assert!(!report
            .list
            .items
            .iter()
            .any(|item| item.label == "Reliable"));
    }

    #[test]
    fn current_receiver_query_resolves_external_zero_argument_call_paths() {
        let external = file_index_for_source(
            r#"class ChimeraGame
{
	proto external PlayerController GetPlayerController();
}

class ArmaReforgerScripted : ChimeraGame {}

ArmaReforgerScripted GetGame();

class PlayerController
{
	proto external IEntity GetControlledEntity();
}
"#,
        )
        .index;
        let source = r#"class Example
{
	void Run()
	{
		GetGame().GetPlayer
		GetGame().GetPlayerController().
	}
}
"#;
        let first_member_offset = source.find("GetPlayer\n").unwrap() + "GetPlayer".len();
        let first_member = completion_report_for_current_receiver_at_offset_with_external_indexes(
            source,
            first_member_offset,
            None,
            Some(&external),
        )
        .expect("an external root call should use the bounded receiver query");

        assert_eq!(first_member.query_quality, QueryQuality::Exact);
        assert_eq!(first_member.completion_context, "member");
        assert_eq!(first_member.receiver_text.as_deref(), Some("GetGame()"));
        assert_eq!(
            first_member.owner_type.as_deref(),
            Some("ArmaReforgerScripted")
        );
        assert!(first_member
            .list
            .items
            .iter()
            .any(|item| item.label == "GetPlayerController"));

        let chained_member_offset =
            source.find("GetPlayerController().").unwrap() + "GetPlayerController().".len();
        let chained_member =
            completion_report_for_current_receiver_at_offset_with_external_indexes(
                source,
                chained_member_offset,
                None,
                Some(&external),
            )
            .expect("an external call chain should use the bounded receiver query");

        assert_eq!(chained_member.query_quality, QueryQuality::Exact);
        assert_eq!(chained_member.completion_context, "member");
        assert_eq!(
            chained_member.receiver_text.as_deref(),
            Some("GetGame().GetPlayerController()")
        );
        assert_eq!(
            chained_member.owner_type.as_deref(),
            Some("PlayerController")
        );
        assert!(chained_member
            .list
            .items
            .iter()
            .any(|item| item.label == "GetControlledEntity"));
    }

    #[test]
    fn current_receiver_query_resolves_the_real_get_game_definition_shape() {
        let game_data = file_index_for_source(
            r#"class ChimeraGame
{
	proto external PlayerController GetPlayerController();
}

class ArmaReforgerScripted : ChimeraGame {}

ArmaReforgerScripted g_ARGame;

ArmaReforgerScripted GetGame()
{
	return g_ARGame;
}

class PlayerController
{
	proto external IEntity GetControlledEntity();
}
"#,
        )
        .index;
        let source = r#"class Example
{
	void Run()
	{
		GetGame().GetPlayer
	}
}
"#;
        let offset = source.find("GetPlayer\n").unwrap() + "GetPlayer".len();

        let report = completion_report_for_current_receiver_at_offset_with_external_indexes(
            source,
            offset,
            None,
            Some(&game_data),
        )
        .expect("a later externally indexed overload can prove the member chain");

        assert_eq!(report.owner_type.as_deref(), Some("ArmaReforgerScripted"));
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "GetPlayerController"));
    }

    #[test]
    fn current_receiver_query_recovers_external_root_calls_across_multiline_lexical_context() {
        let external = file_index_for_source(
            r#"class ChimeraGame
{
	proto external PlayerController GetPlayerController();
}

class ArmaReforgerScripted : ChimeraGame {}
ArmaReforgerScripted GetGame();
"#,
        )
        .index;
        let mut source = "void Padding() {}\n".repeat(1_100);
        source.push_str("class Example { void Run() { GetGame().GetPlayer\n/*");
        source.push_str(&"comment ".repeat(400));
        source.push_str("*/\n} }\n");
        let offset = source.find("GetPlayer\n").unwrap() + "GetPlayer".len();

        let report = completion_report_for_current_receiver_at_offset_with_external_indexes(
            &source,
            offset,
            None,
            Some(&external),
        )
        .expect("current lexical context should recover an externally proven root call");

        assert_eq!(report.query_quality, QueryQuality::Exact);
        assert_eq!(report.receiver_text.as_deref(), Some("GetGame()"));
        assert_eq!(report.owner_type.as_deref(), Some("ArmaReforgerScripted"));
        assert!(report
            .list
            .items
            .iter()
            .any(|item| item.label == "GetPlayerController"));
    }

    #[test]
    fn oversized_ambiguous_lexical_windows_keep_the_safe_fallback() {
        let mut source = "void Padding() {}\n".repeat(15_000);
        source.push_str("GetGame().GetPlayer\n/*");
        source.push_str(&"comment ".repeat(400));
        source.push_str("*/\n");
        let offset = source.find("GetPlayer\n").unwrap() + "GetPlayer".len();

        assert!(source.len() > LEXICAL_CONTEXT_RECOVERY_MAX_SOURCE_BYTES);
        assert!(BoundedCompletionRegion::new(&source, offset).is_none());
    }

    #[test]
    fn current_receiver_query_labels_unresolved_completed_call_receivers() {
        let chained = "class Example { void Run(Widget value) { value.GetWidget().Get } }";
        let chained_offset = chained.find(".Get }").unwrap() + 4;
        let chained_report =
            completion_report_for_current_receiver_at_offset_with_external_indexes(
                chained,
                chained_offset,
                None,
                None,
            )
            .expect("a completed call receiver should retain its bounded fallback reason");
        assert_eq!(
            chained_report.recovery_reason.as_deref(),
            Some("current-revision-external-call-chain-unresolved")
        );

        let static_receiver = "class Example { void Run() { Widget.Get } }";
        assert!(
            completion_report_for_current_receiver_at_offset_with_external_indexes(
                static_receiver,
                static_receiver.find("Get }").unwrap() + 3,
                None,
                None,
            )
            .is_none()
        );

        let malformed = "class Example { void Run(Widget value) { value.Get";
        assert!(
            completion_report_for_current_receiver_at_offset_with_external_indexes(
                malformed,
                malformed.len(),
                None,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn bounded_pending_queries_admit_large_snapshots_without_file_projection() {
        // The query window is anchored at the cursor, so a large unrelated
        // prefix cannot force parse/catalog/index/scope construction on the
        // request thread.  The old implementation rejected this source at
        // 64KiB before it could answer any of these current-revision facts.
        let padding = "// unrelated background text\n".repeat(5_000);
        let external = file_index_for_source(
            "class Widget { void GetName() {} } void Run(int firstValue, string secondValue) {}",
        )
        .index;
        let source = format!(
            "{padding}class Example {{ Widget m_widget; void Test(Widget parameter) {{ Widget localValue; localValue.Get; Run(sec); loc }} }}"
        );

        let local_offset = source.rfind("loc }").unwrap() + 3;
        assert!(
            completion_report_for_current_local_scope_at_offset_with_external_indexes(
                &source,
                local_offset,
                Some(&external),
                None,
            )
            .is_some()
        );

        let member_offset = source.rfind("localValue.Get").unwrap() + "localValue.Get".len();
        let member = completion_report_for_current_receiver_at_offset_with_external_indexes(
            &source,
            member_offset,
            Some(&external),
            None,
        )
        .expect("the bounded region contains the local declaration and receiver");
        assert!(member.list.items.iter().any(|item| item.label == "GetName"));

        let argument_offset = source.rfind("Run(sec").unwrap() + "Run(sec".len();
        let argument =
            completion_report_for_current_argument_labels_at_offset_with_external_indexes(
                &source,
                argument_offset,
                Some(&external),
                None,
            )
            .expect("the bounded region resolves the captured external callable");
        assert!(argument
            .list
            .items
            .iter()
            .any(|item| item.label == "secondValue"));
    }

    #[test]
    fn declaration_keywords_are_available_in_type_mode_at_declaration_boundaries() {
        let items = keyword_completion_items(
            "overr",
            test_range(),
            EditorTopLevelCompletionMode::Type,
            true,
        );

        let first = items.first().unwrap();
        assert_eq!(first.label, "override");
        assert_eq!(first.kind, 14);
        assert_eq!(first.text_edit.new_text, "override");
    }

    #[test]
    fn declaration_keywords_stay_out_of_type_mode_without_declaration_boundary() {
        let items = keyword_completion_items(
            "overr",
            test_range(),
            EditorTopLevelCompletionMode::Type,
            false,
        );

        assert!(items.is_empty());
    }

    #[test]
    fn control_header_keyword_completions_place_the_cursor_inside_empty_parentheses() {
        for keyword in ["if", "for", "foreach", "while", "switch"] {
            let item = keyword_completion_items(
                keyword,
                test_range(),
                EditorTopLevelCompletionMode::Value,
                false,
            )
            .into_iter()
            .find(|item| item.label == keyword)
            .expect("control keyword should remain a value-context keyword");
            let caret = keyword.len() as u32 + 2;

            assert_eq!(item.text_edit.new_text, format!("{keyword} ($0)"));
            assert_eq!(item.insert_text_format, Some(2));
            assert_eq!(item.commit_characters, Some(vec![" ".to_string()]));
            assert_eq!(
                item.command
                    .as_ref()
                    .map(|command| command.command.as_str()),
                Some(COMMAND_NORMALIZE_IF_SPACE_COMMIT),
            );
            assert_eq!(
                item.command
                    .as_ref()
                    .and_then(|command| command.arguments.as_ref()),
                Some(&vec![json!({
                    "expectedCommit": " ",
                    "deletion": { "start": { "line": 0, "character": caret }, "end": { "line": 0, "character": caret + 1 } },
                    "trailingDeletion": { "start": { "line": 0, "character": caret + 1 }, "end": { "line": 0, "character": caret + 2 } },
                    "caret": { "line": 0, "character": caret },
                })]),
            );
            let wire_item = serde_json::to_value(item).expect("keyword item should serialize");
            assert_eq!(wire_item["commitCharacters"], serde_json::json!([" "]));
        }
    }

    #[test]
    fn switch_arm_placeholder_offers_default_and_a_value_case_snippet() {
        let source = "switch (kind) {\n\tdefault:\n\t\t\n}";
        let start = source.find("default").unwrap();
        let items =
            switch_arm_placeholder_completion_items(source, TextSpan::new(start, start + 7))
                .expect("generated switch arm should be recognized");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "default");
        assert_eq!(items[1].label, "case");
        assert_eq!(items[1].text_edit.new_text, "case ${1:value}");
        assert_eq!(items[1].insert_text_format, Some(2));
        assert_eq!(
            items[1]
                .command
                .as_ref()
                .and_then(|command| command.arguments.as_ref()),
            Some(&vec![json!("value")]),
        );
    }

    #[test]
    fn value_completion_after_return_inserts_the_required_separator() {
        let source = "return";
        let prefix_span = TextSpan::new(source.len(), source.len());
        assert!(completion_immediately_follows_return(source, prefix_span));
        let mut items = keyword_completion_items(
            "",
            range_for_span(source, prefix_span),
            EditorTopLevelCompletionMode::Value,
            false,
        );
        add_return_separator_to_completion_items(source, prefix_span, &mut items);
        assert!(items
            .iter()
            .all(|item| item.text_edit.new_text.starts_with(' ')));
        assert!(!completion_immediately_follows_return(
            "return ",
            TextSpan::new("return ".len(), "return ".len()),
        ));
    }

    #[test]
    fn no_arg_callable_inserts_empty_call() {
        let call = callable_signature_parts("Run", "Example.Run() -> void").unwrap();

        assert_eq!(callable_insert_text("Run", &call), "Run()");
        assert_eq!(call.required_parameter_count(), 0);
        assert_eq!(call.optional_parameter_count(), 0);
    }

    #[test]
    fn required_callable_parameters_insert_name_placeholders() {
        let call = callable_signature_parts(
            "GetComponentsByType",
            "Example.GetComponentsByType(typename componentType, out int foundCount) -> void",
        )
        .unwrap();

        assert_eq!(
            callable_insert_text("GetComponentsByType", &call),
            "GetComponentsByType(${1:componentType}, ${2:foundCount})"
        );
        assert_eq!(call.required_parameter_count(), 2);
        assert_eq!(call.optional_parameter_count(), 0);
    }

    #[test]
    fn callable_snippets_preserve_every_required_enum_parameter_in_signature_order() {
        let enum_index = file_index_for_source(
            r#"enum FirstChoice { First }
enum SecondChoice { Second }"#,
        )
        .index;
        let render_context = CompletionRenderContext::new(&enum_index, None, None);
        let call = callable_signature_parts(
            "Run",
            "Example.Run(FirstChoice first, SecondChoice second, int count) -> void",
        )
        .unwrap();

        let insert = callable_insert_text_with_context("Run", &call, Some(render_context));

        assert_eq!(
            insert.text,
            "Run(${1:FirstChoice.}, ${2:SecondChoice.}, ${3:count})"
        );
        assert_eq!(
            insert.enum_placeholder_defaults,
            vec!["FirstChoice.".to_string(), "SecondChoice.".to_string()]
        );
    }

    #[test]
    fn optional_callable_parameters_are_not_inserted() {
        let call = callable_signature_parts(
            "SendToEveryone",
            "SCR_NotificationsComponent.SendToEveryone(ENotification notificationID, int param1 = 0, string label = \"ok\") -> bool",
        )
        .unwrap();

        assert_eq!(
            callable_insert_text("SendToEveryone", &call),
            "SendToEveryone(${1:notificationID})"
        );
        assert_eq!(call.required_parameter_count(), 1);
        assert_eq!(call.optional_parameter_count(), 2);
    }

    #[test]
    fn all_optional_callable_parameters_leave_cursor_inside_call() {
        let call = callable_signature_parts(
            "Attribute",
            "Attribute(string defvalue = \"\", string uiwidget = \"auto\", int precision = 3)",
        )
        .unwrap();

        assert_eq!(callable_insert_text("Attribute", &call), "Attribute($0)");
        assert_eq!(call.required_parameter_count(), 0);
        assert_eq!(call.optional_parameter_count(), 3);
    }

    #[test]
    fn generic_parameter_types_split_at_top_level_commas_only() {
        let call = callable_signature_parts(
            "UseValues",
            "Example.UseValues(map<string, ref array<IEntity>> values, inout array<int> outValues) -> void",
        )
        .unwrap();

        assert_eq!(call.parameters_info.len(), 2);
        assert_eq!(call.parameters_info[0].name, "values");
        assert_eq!(
            call.parameters_info[0].type_and_modifiers,
            "map<string, ref array<IEntity>>"
        );
        assert_eq!(call.parameters_info[1].name, "outValues");
        assert_eq!(
            call.parameters_info[1].type_and_modifiers,
            "inout array<int>"
        );
        assert_eq!(
            callable_insert_text("UseValues", &call),
            "UseValues(${1:values}, ${2:outValues})"
        );
    }

    #[test]
    fn completion_type_owner_strips_parameter_modifiers() {
        assert_eq!(
            callable_type_owner("notnull SCR_InstigatorContextData").as_deref(),
            Some("SCR_InstigatorContextData")
        );
        assert_eq!(
            callable_type_owner("out RplChannel").as_deref(),
            Some("RplChannel")
        );
        assert_eq!(
            callable_type_owner("ref array<IEntity>").as_deref(),
            Some("array")
        );
        assert_eq!(callable_type_owner("int").as_deref(), Some("int"));
    }

    #[test]
    fn preprocessor_completion_offers_directives_and_active_local_macros() {
        let directives = completion_report_for_source_position_with_external(
            "\t#defi",
            LspPosition {
                line: 0,
                character: 6,
            },
            None,
        );
        assert_eq!(directives.completion_context, "preprocessor");
        let define = directives
            .list
            .items
            .iter()
            .find(|item| item.label == "#define")
            .expect("expected #define directive completion");
        assert_eq!(define.text_edit.new_text, "define");
        assert_eq!(
            define
                .command
                .as_ref()
                .map(|command| command.command.as_str()),
            Some(COMMAND_APPLY_DIRECTIVE_SEPARATOR)
        );

        let all_directives = completion_report_for_source_position_with_external(
            "#",
            LspPosition {
                line: 0,
                character: 1,
            },
            None,
        );
        assert_eq!(all_directives.list.items.len(), COMPLETION_DIRECTIVES.len());

        let string = completion_report_for_source_position_with_external(
            "\"#define",
            LspPosition {
                line: 0,
                character: 8,
            },
            None,
        );
        assert!(string.list.items.is_empty());

        let macros = completion_report_for_source_position_with_external(
            "#define ACTIVE_FLAG\n//#define COMMENTED_FLAG\n#ifdef ACT",
            LspPosition {
                line: 2,
                character: 10,
            },
            None,
        );
        let active = macros
            .list
            .items
            .iter()
            .find(|item| item.label == "ACTIVE_FLAG")
            .expect("expected active Macro completion");
        assert_eq!(active.text_edit.new_text, "ACTIVE_FLAG");
        assert_eq!(
            active.detail.as_deref(),
            Some("#define ACTIVE_FLAG (Workspace)")
        );
        assert!(!macros
            .list
            .items
            .iter()
            .any(|item| item.label == "COMMENTED_FLAG"));

        let empty_operand = completion_report_for_source_position_with_external(
            "#define ACTIVE_FLAG\n#ifdef ",
            LspPosition {
                line: 1,
                character: 7,
            },
            None,
        );
        assert!(empty_operand
            .list
            .items
            .iter()
            .any(|item| item.label == "ACTIVE_FLAG"));
    }
}
