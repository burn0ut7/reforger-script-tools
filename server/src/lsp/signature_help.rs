use crate::analysis_runtime::DocumentSnapshot;
use crate::callable::{
    builtin_callable_fact, callable_argument_context_at_offset, callable_signature_parts,
    BuiltinCallableFact, CallableArgumentContext, CallableParameter, CallableSignatureParts,
    CallableTarget,
};
use crate::index::SymbolIndex;
use crate::index_query::{
    EditorCompletionCandidate, EditorCompletionOrigin, EditorTopLevelCompletionMode, IndexQuery,
};
use crate::lexer::{Token, TokenKind};
use crate::lsp::open_documents::ForegroundQuerySnapshot;
use crate::lsp::{
    file_index_for_source, offset_for_position, FileIndexAnalysis, LspMarkupContent, LspPosition,
};
use crate::resolver::{CandidateSource, ReferenceCandidate, ReferenceResolver};
use crate::symbol_display::documentation_display;
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspSignatureHelp {
    pub signatures: Vec<LspSignatureInformation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspSignatureInformation {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<LspMarkupContent>,
    pub parameters: Vec<LspParameterInformation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspParameterInformation {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<LspMarkupContent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSignatureHelpReport {
    pub help: Option<LspSignatureHelp>,
    pub parse_diagnostics: usize,
    pub context: Option<String>,
    pub active_parameter: Option<usize>,
    pub candidate_count: usize,
    pub selected_label: Option<String>,
    pub failure_reason: Option<String>,
    pub timings: LspSignatureHelpTimings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LspSignatureHelpTimings {
    pub context_detection: Duration,
    pub candidate_lookup: Duration,
    pub item_rendering: Duration,
    pub total: Duration,
}

pub fn signature_help_report_for_source_position(
    source: &str,
    position: LspPosition,
) -> LspSignatureHelpReport {
    let analysis = file_index_for_source(source);
    signature_help_report_for_cached_analysis_with_external_indexes(
        source, &analysis, position, None, None,
    )
}

pub(crate) fn signature_help_report_for_cached_analysis_with_external_indexes(
    source: &str,
    analysis: &FileIndexAnalysis,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspSignatureHelpReport {
    let total_start = Instant::now();
    let Some(offset) = offset_for_position(source, position) else {
        return empty_signature_help_report(analysis.parse_diagnostics, total_start);
    };

    let context_start = Instant::now();
    let Some(context) =
        callable_argument_context_at_offset(source, &analysis.syntax.parse.root, offset)
    else {
        let mut report = empty_signature_help_report(analysis.parse_diagnostics, total_start);
        report.timings.context_detection = context_start.elapsed();
        report.failure_reason = Some("not in callable argument list".to_string());
        return report;
    };
    let context_elapsed = context_start.elapsed();

    let resolver = ReferenceResolver::new_with_parse_scope_and_external_indexes(
        source,
        &analysis.index,
        &analysis.syntax.parse,
        &analysis.scope,
        workspace_index.into_iter().chain(game_data_index),
    );

    let lookup_start = Instant::now();
    let candidates = callable_candidates_for_context(
        &context,
        &resolver,
        &analysis.index,
        workspace_index,
        game_data_index,
    );
    let builtin = match &context.target {
        CallableTarget::Attribute { name } => builtin_callable_fact(name),
        _ => None,
    };
    let lookup_elapsed = lookup_start.elapsed();
    if candidates.is_empty() && builtin.is_none() {
        let mut report = empty_signature_help_report(analysis.parse_diagnostics, total_start);
        report.timings.context_detection = context_elapsed;
        report.timings.candidate_lookup = lookup_elapsed;
        report.context = Some(context_label(&context));
        report.active_parameter = Some(context.argument_index);
        report.failure_reason = Some("callable target unresolved".to_string());
        return report;
    }

    let render_start = Instant::now();
    let mut signatures = Vec::new();
    let mut selected_label = None;
    let mut selected_active_parameter = None;
    if let Some(fact) = builtin {
        let parts = fact.signature_parts();
        let active_parameter = active_parameter_for_candidate(&context, &parts);
        selected_label = Some(fact.name.to_string());
        selected_active_parameter = active_parameter;
        signatures.push(signature_information_for_builtin(
            fact,
            &parts,
            active_parameter,
        ));
    } else {
        for candidate in &candidates {
            let label = candidate
                .name
                .as_deref()
                .unwrap_or(candidate.display.label.as_str());
            let Some(parts) = candidate.callable_signature_parts.as_ref() else {
                continue;
            };
            let active_parameter = active_parameter_for_candidate(&context, parts);
            if selected_label.is_none() {
                selected_label = Some(label.to_string());
                selected_active_parameter = active_parameter;
            }
            signatures.push(signature_information_for_candidate(
                candidate,
                label,
                parts,
                active_parameter,
            ));
        }
    }
    let render_elapsed = render_start.elapsed();

    if signatures.is_empty() {
        let mut report = empty_signature_help_report(analysis.parse_diagnostics, total_start);
        report.timings.context_detection = context_elapsed;
        report.timings.candidate_lookup = lookup_elapsed;
        report.timings.item_rendering = render_elapsed;
        report.context = Some(context_label(&context));
        report.active_parameter = Some(context.argument_index);
        report.failure_reason = Some("callable signature unavailable".to_string());
        return report;
    }

    LspSignatureHelpReport {
        help: Some(LspSignatureHelp {
            signatures,
            active_signature: Some(0),
            active_parameter: selected_active_parameter.map(|parameter| parameter as u32),
        }),
        parse_diagnostics: analysis.parse_diagnostics,
        context: Some(context_label(&context)),
        active_parameter: selected_active_parameter,
        candidate_count: if builtin.is_some() {
            1
        } else {
            candidates.len()
        },
        selected_label,
        failure_reason: None,
        timings: LspSignatureHelpTimings {
            context_detection: context_elapsed,
            candidate_lookup: lookup_elapsed,
            item_rendering: render_elapsed,
            total: total_start.elapsed(),
        },
    }
}

/// Returns the narrow signature-help projection that is safe before the
/// current snapshot's semantic analysis is available.  It deliberately avoids
/// resolution: only an unqualified call whose name has exactly one complete
/// callable declaration in this current source snapshot can be shown.
/// Receiver calls, constructors, attributes, malformed declarations, and
/// ambiguous names require revision-matching semantic facts and return none.
pub(crate) fn signature_help_report_for_pending_snapshot(
    snapshot: &DocumentSnapshot,
    foreground: &ForegroundQuerySnapshot,
    parse_diagnostics: usize,
    position: LspPosition,
) -> LspSignatureHelpReport {
    let total_start = Instant::now();
    let Some(offset) = snapshot.positions().and_then(|positions| {
        positions.offset_for_position(crate::analysis_runtime::Position {
            line: position.line,
            character: position.character,
        })
    }) else {
        return empty_signature_help_report(parse_diagnostics, total_start);
    };
    let source = snapshot.text();
    let context_start = Instant::now();
    let Some(context) = pending_unqualified_call_context(source, foreground.tokens(), offset)
    else {
        let mut report = empty_signature_help_report(parse_diagnostics, total_start);
        report.timings.context_detection = context_start.elapsed();
        report.failure_reason = Some("not in callable argument list".to_string());
        return report;
    };
    let context_elapsed = context_start.elapsed();
    let lookup_start = Instant::now();
    let candidates = foreground
        .callable_declarations_named(&context.callee)
        .collect::<Vec<_>>();
    let lookup_elapsed = lookup_start.elapsed();
    let [candidate] = candidates.as_slice() else {
        let mut report = empty_signature_help_report(parse_diagnostics, total_start);
        report.timings.context_detection = context_elapsed;
        report.timings.candidate_lookup = lookup_elapsed;
        report.context = Some("call".to_string());
        report.active_parameter = Some(context.argument_index);
        report.failure_reason = Some("pending snapshot callable target unavailable".to_string());
        return report;
    };

    let render_start = Instant::now();
    let Some(parts) = callable_signature_parts(&candidate.name, &candidate.signature) else {
        let mut report = empty_signature_help_report(parse_diagnostics, total_start);
        report.timings.context_detection = context_elapsed;
        report.timings.candidate_lookup = lookup_elapsed;
        report.timings.item_rendering = render_start.elapsed();
        report.context = Some("call".to_string());
        report.active_parameter = Some(context.argument_index);
        report.failure_reason = Some("pending snapshot callable signature unavailable".to_string());
        return report;
    };
    let active_parameter = (!parts.parameters_info.is_empty()).then(|| {
        context
            .argument_index
            .min(parts.parameters_info.len().saturating_sub(1))
    });
    let signature = LspSignatureInformation {
        label: candidate.signature.clone(),
        documentation: None,
        parameters: parts
            .parameters_info
            .iter()
            .map(|parameter| LspParameterInformation {
                label: parameter.raw.clone(),
                documentation: None,
            })
            .collect(),
        active_parameter: active_parameter.map(|parameter| parameter as u32),
    };
    let render_elapsed = render_start.elapsed();
    LspSignatureHelpReport {
        help: Some(LspSignatureHelp {
            signatures: vec![signature],
            active_signature: Some(0),
            active_parameter: active_parameter.map(|parameter| parameter as u32),
        }),
        parse_diagnostics,
        context: Some("call".to_string()),
        active_parameter,
        candidate_count: 1,
        selected_label: Some(candidate.name.clone()),
        failure_reason: None,
        timings: LspSignatureHelpTimings {
            context_detection: context_elapsed,
            candidate_lookup: lookup_elapsed,
            item_rendering: render_elapsed,
            total: total_start.elapsed(),
        },
    }
}

/// A bounded token-only cursor query. It deliberately declines every shape
/// that needs CST traversal (member calls, attributes, constructors, named
/// arguments, or nested expression structure) while semantic analysis is
/// pending. The foreground worker already owns the whole-file lexer/parser
/// cost; this handler only examines the call's immediate token window.
#[derive(Debug, Clone)]
struct PendingUnqualifiedCallContext {
    callee: String,
    argument_index: usize,
}

fn pending_unqualified_call_context(
    source: &str,
    tokens: &[Token],
    offset: usize,
) -> Option<PendingUnqualifiedCallContext> {
    const MAX_PENDING_CALL_TOKENS: usize = 256;
    let cursor = tokens.partition_point(|token| token.span.end <= offset);
    let mut depth = 0usize;
    let mut argument_index = 0usize;
    let mut opening = None;

    for index in (0..cursor).rev().take(MAX_PENDING_CALL_TOKENS) {
        let token = tokens[index];
        if token.kind.is_trivia() {
            continue;
        }
        match token.kind {
            TokenKind::RightParen => depth += 1,
            TokenKind::LeftParen if depth > 0 => depth -= 1,
            TokenKind::LeftParen => {
                opening = Some(index);
                break;
            }
            TokenKind::Comma if depth == 0 => argument_index += 1,
            TokenKind::Semicolon | TokenKind::LeftBrace | TokenKind::RightBrace if depth == 0 => {
                return None;
            }
            _ => {}
        }
    }
    let opening = opening?;
    let callee_index = (0..opening)
        .rev()
        .map(|index| (index, tokens[index]))
        .find(|(_, token)| !token.kind.is_trivia())?;
    let (callee_index, callee_token) = callee_index;
    if callee_token.kind != TokenKind::Identifier {
        return None;
    }
    let before_callee = (0..callee_index)
        .rev()
        .map(|index| tokens[index])
        .find(|token| !token.kind.is_trivia());
    if before_callee.is_some_and(|token| token.kind == TokenKind::Dot) {
        return None;
    }
    let callee = source.get(callee_token.span.start..callee_token.span.end)?;
    Some(PendingUnqualifiedCallContext {
        callee: callee.to_string(),
        argument_index,
    })
}

fn empty_signature_help_report(
    parse_diagnostics: usize,
    total_start: Instant,
) -> LspSignatureHelpReport {
    LspSignatureHelpReport {
        help: None,
        parse_diagnostics,
        context: None,
        active_parameter: None,
        candidate_count: 0,
        selected_label: None,
        failure_reason: None,
        timings: LspSignatureHelpTimings {
            total: total_start.elapsed(),
            ..Default::default()
        },
    }
}

fn callable_candidates_for_context(
    context: &CallableArgumentContext,
    resolver: &ReferenceResolver<'_, '_>,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Vec<EditorCompletionCandidate> {
    match &context.target {
        CallableTarget::Attribute { name } | CallableTarget::New { type_name: name } => {
            callable_type_candidates(name, local_index, workspace_index, game_data_index)
        }
        CallableTarget::Call { callee_span } => resolver
            .resolve_at_offset(callee_span.start)
            .map(|resolution| {
                let mut references = Vec::new();
                if let Some(selected) = resolution.selected {
                    references.push(selected.clone());
                    references.extend(
                        resolution
                            .candidates
                            .into_iter()
                            .filter(|candidate| candidate.id != selected.id),
                    );
                } else {
                    references = resolution.candidates;
                }
                references
                    .into_iter()
                    .filter_map(|reference| {
                        completion_candidate_for_reference(
                            &reference,
                            local_index,
                            workspace_index,
                            game_data_index,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn callable_type_candidates(
    name: &str,
    local_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Vec<EditorCompletionCandidate> {
    let mut candidates = Vec::new();
    candidates.extend(exact_type_candidates(name, local_index));
    if let Some(index) = workspace_index {
        candidates.extend(exact_type_candidates(name, index));
    }
    if let Some(index) = game_data_index {
        candidates.extend(exact_type_candidates(name, index));
    }
    candidates
        .into_iter()
        .filter(|candidate| candidate.constructor_signature.is_some())
        .collect()
}

fn exact_type_candidates(name: &str, index: &SymbolIndex) -> Vec<EditorCompletionCandidate> {
    IndexQuery::new(index)
        .completion_top_level_limited(name, EditorTopLevelCompletionMode::Type, 32)
        .into_iter()
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

fn signature_information_for_candidate(
    candidate: &EditorCompletionCandidate,
    label: &str,
    parts: &CallableSignatureParts,
    active_parameter: Option<usize>,
) -> LspSignatureInformation {
    let signature = candidate
        .signature
        .as_deref()
        .or(candidate.constructor_signature.as_deref())
        .unwrap_or(label);
    let documentation = callable_documentation(candidate, parts);
    let parameters = parts
        .parameters_info
        .iter()
        .map(|parameter| parameter_information(parameter, candidate))
        .collect();
    LspSignatureInformation {
        label: signature.to_string(),
        documentation,
        parameters,
        active_parameter: active_parameter.map(|parameter| parameter as u32),
    }
}

fn signature_information_for_builtin(
    fact: &BuiltinCallableFact,
    parts: &CallableSignatureParts,
    active_parameter: Option<usize>,
) -> LspSignatureInformation {
    LspSignatureInformation {
        label: fact.signature(),
        documentation: None,
        parameters: parts
            .parameters_info
            .iter()
            .map(|parameter| LspParameterInformation {
                label: parameter.raw.clone(),
                documentation: None,
            })
            .collect(),
        active_parameter: active_parameter.map(|parameter| parameter as u32),
    }
}

fn active_parameter_for_candidate(
    context: &CallableArgumentContext,
    parts: &CallableSignatureParts,
) -> Option<usize> {
    let parameter_count = parts.parameters_info.len();
    if parameter_count == 0 {
        return None;
    }
    context
        .active_label
        .as_ref()
        .and_then(|label| {
            parts
                .parameters_info
                .iter()
                .position(|parameter| parameter.name.eq_ignore_ascii_case(label))
        })
        .or_else(|| Some(context.argument_index.min(parameter_count - 1)))
}

fn callable_documentation(
    candidate: &EditorCompletionCandidate,
    parts: &CallableSignatureParts,
) -> Option<LspMarkupContent> {
    let mut sections = Vec::new();
    let parameter_summary = signature_parameter_summary(parts, candidate);
    if !parameter_summary.is_empty() {
        sections.push(parameter_summary);
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

fn signature_parameter_summary(
    parts: &CallableSignatureParts,
    candidate: &EditorCompletionCandidate,
) -> String {
    let docs = documentation_display(&candidate.display.doc_comments);
    let mut output = String::new();
    for parameter in &parts.parameters_info {
        let optional = parameter.default_text.is_some();
        let direction = parameter_direction(parameter);
        let doc = docs
            .parameters
            .iter()
            .find(|doc| doc.name == parameter.name)
            .map(|doc| doc.description.as_str())
            .unwrap_or("");
        output.push_str(&format!(
            "- `{}`{}{}{}\n",
            parameter.name,
            parameter_type_suffix(parameter),
            if optional { " optional" } else { "" },
            parameter
                .default_text
                .as_ref()
                .map(|default| format!(" = `{}`", escape_markdown_inline_code(default)))
                .unwrap_or_default()
        ));
        if let Some(direction) = direction {
            output.push_str(&format!("  - direction: `{direction}`\n"));
        }
        if !doc.is_empty() {
            output.push_str(&format!("  - {}\n", escape_markdown_text(doc)));
        }
    }
    if let Some(returns) = docs.returns {
        if !returns.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("Returns: {}\n", escape_markdown_text(&returns)));
        }
    }
    output
}

fn parameter_information(
    parameter: &CallableParameter,
    candidate: &EditorCompletionCandidate,
) -> LspParameterInformation {
    let docs = documentation_display(&candidate.display.doc_comments);
    let doc = docs
        .parameters
        .iter()
        .find(|doc| doc.name == parameter.name)
        .map(|doc| doc.description.clone());
    let mut documentation = Vec::new();
    if let Some(doc) = doc {
        if !doc.trim().is_empty() {
            documentation.push(doc);
        }
    }
    if let Some(default) = parameter.default_text.as_ref() {
        documentation.push(format!(
            "Optional. Default: `{}`",
            escape_markdown_inline_code(default)
        ));
    }
    LspParameterInformation {
        label: parameter.raw.clone(),
        documentation: (!documentation.is_empty()).then(|| LspMarkupContent {
            kind: "markdown".to_string(),
            value: documentation.join("\n\n"),
        }),
    }
}

fn parameter_direction(parameter: &CallableParameter) -> Option<&'static str> {
    let text = parameter.type_and_modifiers.as_str();
    if text.split_whitespace().any(|part| part == "inout") {
        Some("inout")
    } else if text.split_whitespace().any(|part| part == "out") {
        Some("out")
    } else {
        None
    }
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

pub(crate) fn signature_help_debug_markdown(report: &LspSignatureHelpReport) -> String {
    let mut output = String::new();
    output.push_str("\n---\n\n");
    output.push_str("## Signature Help Context\n\n");
    output.push_str(&format!(
        "- Context: `{}`\n",
        escape_markdown_cell(report.context.as_deref().unwrap_or("<none>"))
    ));
    output.push_str(&format!(
        "- Active Parameter: `{}`\n",
        report
            .active_parameter
            .map(|index| index.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    ));
    output.push_str(&format!(
        "- Candidate Count: `{}`\n",
        report.candidate_count
    ));
    output.push_str(&format!(
        "- Selected Callable: `{}`\n",
        escape_markdown_cell(report.selected_label.as_deref().unwrap_or("<none>"))
    ));
    output.push_str(&format!(
        "- Failure Reason: `{}`\n",
        escape_markdown_cell(report.failure_reason.as_deref().unwrap_or("<none>"))
    ));
    output.push_str(&format!(
        "- Parse Diagnostics: `{}`\n\n",
        report.parse_diagnostics
    ));

    output.push_str("## Signature Help Timings\n\n");
    output.push_str("| Phase | Milliseconds |\n");
    output.push_str("| --- | ---: |\n");
    output.push_str(&format!(
        "| Context detection | {} |\n",
        report.timings.context_detection.as_millis()
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

    output.push_str("## Signature Candidates\n\n");
    let Some(help) = report.help.as_ref() else {
        output.push_str("None.\n");
        return output;
    };
    if help.signatures.is_empty() {
        output.push_str("None.\n");
        return output;
    }

    output.push_str("| # | Signature | Active Param | Parameters | Documentation |\n");
    output.push_str("| ---: | --- | ---: | --- | --- |\n");
    for (index, signature) in help.signatures.iter().take(20).enumerate() {
        output.push_str(&format!(
            "| {} | `{}` | {} | {} | {} |\n",
            index + 1,
            escape_markdown_cell(&signature.label),
            signature
                .active_parameter
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            signature_parameters_cell(signature),
            markdown_table_text(
                signature
                    .documentation
                    .as_ref()
                    .map(|documentation| documentation.value.as_str())
                    .unwrap_or("")
            )
        ));
    }
    if help.signatures.len() > 20 {
        output.push_str(&format!(
            "|  |  |  |  | +{} more signatures |\n",
            help.signatures.len() - 20
        ));
    }
    output.push('\n');

    if let Some(signature) = help.signatures.first() {
        output.push_str("## Active Signature Parameters\n\n");
        output.push_str("| # | Parameter | Active | Documentation |\n");
        output.push_str("| ---: | --- | --- | --- |\n");
        let active = help
            .active_parameter
            .or(signature.active_parameter)
            .map(|value| value as usize);
        for (index, parameter) in signature.parameters.iter().enumerate() {
            output.push_str(&format!(
                "| {} | `{}` | {} | {} |\n",
                index + 1,
                escape_markdown_cell(&parameter.label),
                if Some(index) == active { "yes" } else { "no" },
                markdown_table_text(
                    parameter
                        .documentation
                        .as_ref()
                        .map(|documentation| documentation.value.as_str())
                        .unwrap_or("")
                )
            ));
        }
    }

    output
}

fn context_label(context: &CallableArgumentContext) -> String {
    match &context.target {
        CallableTarget::Attribute { name } => format!("attribute {name}"),
        CallableTarget::Call { .. } => "call".to_string(),
        CallableTarget::New { type_name } => format!("new {type_name}"),
    }
}

fn escape_markdown_inline_code(value: &str) -> String {
    value.replace('`', "\\`")
}

fn escape_markdown_text(value: &str) -> String {
    value.replace('\\', "\\\\")
}

fn signature_parameters_cell(signature: &LspSignatureInformation) -> String {
    let parameters = signature
        .parameters
        .iter()
        .map(|parameter| format!("`{}`", escape_markdown_cell(&parameter.label)))
        .collect::<Vec<_>>()
        .join("<br>");
    if parameters.is_empty() {
        String::new()
    } else {
        parameters
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
    use crate::lsp::LspPosition;

    #[test]
    fn function_call_reports_active_parameter_after_comma() {
        let source = r#"class Example
{
	void SendToEveryone(ENotification notificationID, int param1 = 0);
	void Run()
	{
		SendToEveryone(ENotification.PLAYER_JOINED, );
	}
}
enum ENotification
{
	PLAYER_JOINED
}"#;
        let position = position_after(source, "PLAYER_JOINED, ");
        let report = signature_help_report_for_source_position(source, position);
        assert_eq!(report.active_parameter, Some(1));
        let help = report.help.unwrap();
        assert_eq!(help.signatures[0].parameters.len(), 2);
    }

    #[test]
    fn named_argument_selects_matching_parameter() {
        let source = r#"class Attribute
{
	void Attribute(string defvalue = "", string uiwidget = "auto", string desc = "");
}
class Example
{
	[Attribute(desc: )]
	int m_Value;
}"#;
        let position = position_after(source, "desc: ");
        let report = signature_help_report_for_source_position(source, position);
        assert_eq!(report.active_parameter, Some(2));
    }

    #[test]
    fn builtin_attribute_signature_uses_the_shared_compiler_fact() {
        let source = "class Example { [Attribute(uiwidget: )] int m_Value; }";
        let report =
            signature_help_report_for_source_position(source, position_after(source, "uiwidget: "));
        let help = report
            .help
            .expect("expected built-in Attribute signature help");

        assert_eq!(report.selected_label.as_deref(), Some("Attribute"));
        assert_eq!(help.active_parameter, Some(1));
        assert_eq!(help.signatures.len(), 1);
        assert_eq!(
            help.signatures[0].parameters[0].label,
            "string defvalue = \"\""
        );
        assert!(help.signatures[0].label.contains("bool prefabbed = false"));
    }

    #[test]
    fn non_call_position_returns_no_help() {
        let source = "class Example {}";
        let report = signature_help_report_for_source_position(
            source,
            LspPosition {
                line: 0,
                character: 2,
            },
        );
        assert!(report.help.is_none());
    }

    #[test]
    fn foreground_callable_declarations_extract_complete_current_method() {
        let source =
            "class Example { void Current(string currentValue) {} void Test() { Current(\"\", ); } }";
        let syntax = std::sync::Arc::new(crate::lsp::open_documents::DocumentSyntax::new(source));
        let foreground = ForegroundQuerySnapshot::build(source, syntax.clone());
        let candidates = foreground
            .callable_declarations_named("Current")
            .collect::<Vec<_>>();
        assert_eq!(
            candidates.len(),
            1,
            "diagnostics={:?}",
            syntax.parse.diagnostics
        );
        assert_eq!(candidates[0].signature, "void Current(string currentValue)");
    }

    #[test]
    fn pending_callable_context_selects_simple_call_name_without_a_syntax_walk() {
        let source =
            "class Example { void Current(string currentValue) {} void Test() { Current(\"\", ); } }";
        let offset = source.find("Current(\"\", ").unwrap() + "Current(\"\", ".len();
        let context = pending_unqualified_call_context(source, &crate::lexer::lex(source), offset)
            .expect("expected call context");
        assert_eq!(context.callee, "Current");
        assert_eq!(context.argument_index, 1);
    }

    #[test]
    fn active_parameters_are_clamped_per_signature_candidate() {
        let context = CallableArgumentContext {
            target: CallableTarget::New {
                type_name: "Example".to_string(),
            },
            argument_span: crate::lexer::TextSpan::new(0, 0),
            argument_index: 3,
            active_label: None,
            supplied_labels: Default::default(),
        };
        let one_parameter =
            callable_signature_parts("Run", "Example.Run(int first) -> void").unwrap();
        let two_parameters =
            callable_signature_parts("Run", "Example.Run(int first, int second) -> void").unwrap();

        assert_eq!(
            active_parameter_for_candidate(&context, &one_parameter),
            Some(0)
        );
        assert_eq!(
            active_parameter_for_candidate(&context, &two_parameters),
            Some(1)
        );
    }

    fn position_after(source: &str, needle: &str) -> LspPosition {
        let offset = source.find(needle).unwrap() + needle.len();
        let mut line = 0u32;
        let mut character = 0u32;
        for ch in source[..offset].chars() {
            if ch == '\n' {
                line += 1;
                character = 0;
            } else {
                character += 1;
            }
        }
        LspPosition { line, character }
    }
}
