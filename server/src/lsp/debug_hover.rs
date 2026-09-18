use crate::index::{GlobalSymbolId, SymbolIndex};
use crate::index_query::IndexQuery;
use crate::lexer::{lex, TextSpan, Token};
use crate::lsp::external_indexes::ExternalIndexes;
use crate::lsp::hover_render::{render_hover_markdown, HoverLinkContext, HoverRenderContext};
use crate::lsp::semantic_tokens::semantic_tokens_report_for_cached_analysis_with_external_indexes;
use crate::lsp::{
    file_index_for_source, offset_for_position, position_for_offset, span_text, symbol_kind_label,
    ExternalIndexStatusSummary, FileIndexAnalysis, LspPosition, LspRange,
};
use crate::model::SymbolKind;
use crate::resolver::{CandidateSource, HoverResolution, ReferenceResolver};
use crate::syntax::ParseDiagnostic;
use std::collections::BTreeMap;
use std::time::Instant;

const DEBUG_TOKEN_CONTEXT: usize = 8;
const DEBUG_CANDIDATE_LIMIT: usize = 20;
const DEBUG_CHILD_LIMIT: usize = 20;

pub fn debug_hover_report_for_source_position(source: &str, position: LspPosition) -> String {
    debug_hover_report_for_source_position_with_external(source, position, None, None)
}

fn debug_hover_report_for_source_position_with_external(
    source: &str,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
    external_status: Option<&ExternalIndexStatusSummary>,
) -> String {
    let analysis = file_index_for_source(source);
    debug_hover_report_for_cached_analysis_with_external(
        source,
        &analysis,
        position,
        external_index,
        external_status,
    )
}

pub(crate) fn debug_hover_report_for_cached_analysis_with_external(
    source: &str,
    analysis: &FileIndexAnalysis,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
    external_status: Option<&ExternalIndexStatusSummary>,
) -> String {
    debug_hover_report_for_cached_analysis_with_external_indexes(
        source,
        analysis,
        "file:///hover-debug-source.c",
        position,
        None,
        external_index,
        external_status,
    )
}

pub(crate) fn debug_hover_report_for_cached_analysis_with_external_indexes(
    source: &str,
    analysis: &FileIndexAnalysis,
    current_uri: &str,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    external_status: Option<&ExternalIndexStatusSummary>,
) -> String {
    debug_hover_report_for_cached_analysis_with_external_layers(
        source,
        analysis,
        current_uri,
        position,
        workspace_index,
        game_data_index,
        external_status,
    )
}

fn debug_hover_report_for_cached_analysis_with_external_layers(
    source: &str,
    analysis: &FileIndexAnalysis,
    current_uri: &str,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    external_status: Option<&ExternalIndexStatusSummary>,
) -> String {
    let start = Instant::now();
    let index = &analysis.index;
    let query = IndexQuery::new(index);
    let offset = offset_for_position(source, position);
    let tokens = lex(source);
    let resolver = ReferenceResolver::new_with_parse_scope_and_external_indexes(
        source,
        index,
        &analysis.syntax.parse,
        &analysis.scope,
        ExternalIndexes::new(workspace_index, game_data_index).ordered(),
    );
    let resolver_resolution = offset.and_then(|offset| resolver.resolve_at_offset(offset));
    let hover_resolution = resolver_resolution
        .clone()
        .map(HoverResolution::Identifier)
        .or_else(|| offset.and_then(|offset| resolver.resolve_hover_at_offset(offset)));
    let candidates = offset
        .map(|offset| resolver.syntax_span_candidates_at_offset(offset))
        .unwrap_or_default();
    let selected_candidate = match hover_resolution.as_ref() {
        Some(HoverResolution::Identifier(resolution)) => resolution.selected.as_ref(),
        Some(HoverResolution::SyntaxSpan(resolution)) => resolution.selected.as_ref(),
        None => None,
    };
    let selected_id = selected_candidate
        .filter(|candidate| candidate.source == CandidateSource::FileLocal)
        .map(|candidate| candidate.id);
    let selected_external_id = selected_candidate
        .filter(|candidate| candidate.source == CandidateSource::External)
        .map(|candidate| candidate.id);

    let mut report = String::new();
    report.push_str("# Reforger Hover Debug\n\n");
    report.push_str(&format!(
        "- Position: line {} character {} (UTF-16, zero-based)\n",
        position.line, position.character
    ));
    report.push_str(&format!(
        "- Byte offset: {}\n",
        format_optional_usize(offset)
    ));
    report.push_str(&format!("- Source bytes: {}\n", source.len()));
    report.push_str("- Pipeline: lexer -> parser -> AST -> model -> index -> display\n");
    report.push_str(&format!(
        "- Parse diagnostics: {}\n",
        analysis.parse_diagnostics
    ));
    report.push_str(&format!("- Indexed symbols: {}\n", index.symbols().len()));
    report.push_str(&format!(
        "- Selected Symbol: {}\n",
        if selected_id.is_some() || selected_external_id.is_some() {
            "yes"
        } else {
            "no"
        }
    ));
    report.push_str(&format!("- Elapsed: {} ms\n", start.elapsed().as_millis()));

    report.push_str("\n## Source Line\n\n");
    append_source_line(&mut report, source, position);

    report.push_str("\n## Tokens Around Cursor\n\n");
    append_token_context(&mut report, source, &tokens, offset);

    report.push_str("\n## Semantic Token Coloring Context\n\n");
    append_semantic_token_context(
        &mut report,
        source,
        analysis,
        workspace_index,
        game_data_index,
        offset,
    );

    report.push_str("\n## Parse Diagnostics\n\n");
    append_parse_diagnostics(&mut report, source, &analysis.syntax.parse.diagnostics);

    report.push_str("\n## Resolver Resolution\n\n");
    append_resolver_resolution(
        &mut report,
        &query,
        workspace_index,
        game_data_index,
        resolver_resolution.as_ref(),
    );

    report.push_str("\n## External Index\n\n");
    append_external_index_status(&mut report, external_status);

    report.push_str("\n## Hover Selection\n\n");
    if let Some(id) = selected_id {
        append_display_details(&mut report, source, index, &query, id);
        if let Some(display) = query.symbol_display(id) {
            let external_query = workspace_index.or(game_data_index).map(IndexQuery::new);
            let game_data_query = game_data_index.map(IndexQuery::new);
            report.push_str("\n### Hover Markdown\n\n```markdown\n");
            report.push_str(&escape_fence_text(&render_hover_markdown(
                &display,
                Some(HoverRenderContext {
                    query: &query,
                    member_summary_query: external_query.as_ref(),
                    links: Some(HoverLinkContext {
                        current_uri,
                        external_query: external_query.as_ref(),
                        game_data_query: game_data_query.as_ref(),
                    }),
                }),
            )));
            report.push_str("\n```\n");
        }
    } else if let Some(selected_candidate) = selected_candidate {
        if let Some(external_index) =
            ExternalIndexes::new(workspace_index, game_data_index).for_candidate(selected_candidate)
        {
            let id = selected_candidate.id;
            let external_query = IndexQuery::new(external_index);
            append_external_display_details(&mut report, external_index, &external_query, id);
            if let Some(display) = external_query.symbol_display(id) {
                report.push_str("\n### Hover Markdown\n\n```markdown\n");
                report.push_str(&escape_fence_text(&render_hover_markdown(
                    &display,
                    Some(HoverRenderContext {
                        query: &external_query,
                        member_summary_query: None,
                        links: Some(HoverLinkContext {
                            current_uri,
                            external_query: None,
                            game_data_query: None,
                        }),
                    }),
                )));
                report.push_str("\n```\n");
            }
        }
    } else {
        report.push_str("No symbol matched the cursor position.\n");
    }

    report.push_str("\n## Candidate Symbols At Cursor\n\n");
    append_hover_candidates(&mut report, index, &query, &candidates);

    if let Some(id) = selected_id {
        report.push_str("\n## Parent Chain\n\n");
        append_parent_chain(&mut report, source, index, &query, id);

        report.push_str("\n## Immediate Children\n\n");
        append_children(&mut report, source, index, &query, id);
    } else if let Some(selected_candidate) = selected_candidate {
        if let Some(external_index) =
            ExternalIndexes::new(workspace_index, game_data_index).for_candidate(selected_candidate)
        {
            let id = selected_candidate.id;
            let external_query = IndexQuery::new(external_index);
            report.push_str("\n## Parent Chain\n\n");
            append_external_parent_chain(&mut report, external_index, &external_query, id);

            report.push_str("\n## Immediate Children\n\n");
            append_external_children(&mut report, external_index, &external_query, id);
        }
    }

    report.push_str("\n## Symbol Kind Counts\n\n");
    append_symbol_kind_counts(&mut report, index);

    report
}

fn append_source_line(report: &mut String, source: &str, position: LspPosition) {
    let Some((start, end)) = line_bounds(source, position.line) else {
        report.push_str("Position is outside the document.\n");
        return;
    };
    let line = &source[start..end];
    report.push_str("```text\n");
    report.push_str(&escape_debug_text(line));
    report.push('\n');
    report.push_str(&" ".repeat(position.character as usize));
    report.push_str("^\n");
    report.push_str("```\n");
}

fn append_token_context(
    report: &mut String,
    source: &str,
    tokens: &[Token],
    offset: Option<usize>,
) {
    if tokens.is_empty() {
        report.push_str("No tokens.\n");
        return;
    }
    let center = offset
        .and_then(|offset| {
            tokens
                .iter()
                .position(|token| span_contains_or_touches_offset(token.span, offset))
        })
        .unwrap_or(0);
    let start = center.saturating_sub(DEBUG_TOKEN_CONTEXT);
    let end = (center + DEBUG_TOKEN_CONTEXT + 1).min(tokens.len());
    report.push_str("| Hit | Kind | Span | Text |\n");
    report.push_str("| --- | --- | --- | --- |\n");
    for token in &tokens[start..end] {
        let hit = offset.is_some_and(|offset| span_contains_or_touches_offset(token.span, offset));
        report.push_str(&format!(
            "| {} | `{:?}` | `{}` | `{}` |\n",
            if hit { "*" } else { "" },
            token.kind,
            format_span(token.span),
            escape_table_text(span_text(source, token.span))
        ));
    }
}

fn append_semantic_token_context(
    report: &mut String,
    source: &str,
    analysis: &FileIndexAnalysis,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    offset: Option<usize>,
) {
    report.push_str("Semantic token types are produced by the Rust language server. VS Code applies the extension's Enforce palette and any user semantic-token customizations; TextMate scopes are not used for Enforce coloring.\n\n");
    let semantic = semantic_tokens_report_for_cached_analysis_with_external_indexes(
        source,
        analysis,
        workspace_index,
        game_data_index,
    );
    if semantic.decoded.is_empty() {
        report.push_str("No semantic tokens.\n");
        return;
    }

    let center = offset
        .and_then(|offset| {
            semantic.decoded.iter().position(|token| {
                let span = TextSpan::new(
                    offset_for_position(source, token.range.start).unwrap_or(0),
                    offset_for_position(source, token.range.end).unwrap_or(0),
                );
                span_contains_or_touches_offset(span, offset)
            })
        })
        .unwrap_or(0);
    let start = center.saturating_sub(DEBUG_TOKEN_CONTEXT);
    let end = (center + DEBUG_TOKEN_CONTEXT + 1).min(semantic.decoded.len());

    report.push_str("| Hit | Text | Range | Semantic type | Modifiers |\n");
    report.push_str("| --- | --- | --- | --- | --- |\n");
    for index in start..end {
        let token = &semantic.decoded[index];
        let span = TextSpan::new(
            offset_for_position(source, token.range.start).unwrap_or(0),
            offset_for_position(source, token.range.end).unwrap_or(0),
        );
        let hit = offset.is_some_and(|offset| span_contains_or_touches_offset(span, offset));
        report.push_str(&format!(
            "| {} | `{}` | `{}` | `{}` | `{}` |\n",
            if hit { "*" } else { "" },
            escape_table_text(&token.text),
            format_range_from_lsp(token.range),
            token.token_type,
            token.modifiers.join(", "),
        ));
    }
}

fn append_parse_diagnostics(report: &mut String, source: &str, diagnostics: &[ParseDiagnostic]) {
    if diagnostics.is_empty() {
        report.push_str("None.\n");
        return;
    }
    for diagnostic in diagnostics.iter().take(10) {
        report.push_str(&format!(
            "- {} at `{}` {}\n",
            diagnostic.message,
            format_span(diagnostic.span),
            format_range(source, diagnostic.span)
        ));
    }
    if diagnostics.len() > 10 {
        report.push_str(&format!(
            "- ... {} more diagnostics\n",
            diagnostics.len() - 10
        ));
    }
}

fn append_display_details(
    report: &mut String,
    source: &str,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let Some(symbol) = index.symbol(id) else {
        report.push_str("Selected symbol is missing from the index.\n");
        return;
    };
    let Some(display) = query.symbol_display(id) else {
        report.push_str("Selected symbol has no display information.\n");
        return;
    };

    report.push_str(&format!("- Kind: `{}`\n", symbol_kind_label(display.kind)));
    report.push_str(&format!(
        "- Label: `{}`\n",
        escape_debug_text(&display.label)
    ));
    if let Some(signature) = &display.signature {
        report.push_str(&format!(
            "- Signature: `{}`\n",
            escape_debug_text(signature)
        ));
    }
    if let Some(detail) = &display.detail {
        report.push_str(&format!("- Detail: `{}`\n", escape_debug_text(detail)));
    }
    report.push_str(&format!(
        "- Span: `{}` {}\n",
        format_span(symbol.span),
        format_range(source, symbol.span)
    ));
    report.push_str(&format!(
        "- Selection span: `{}` {}\n",
        format_span(symbol.selection_span),
        format_range(source, symbol.selection_span)
    ));
    if !display.modifiers.is_empty() {
        report.push_str(&format!(
            "- Modifiers: `{}`\n",
            display.modifiers.join(", ")
        ));
    }
    if !display.attributes.is_empty() {
        report.push_str(&format!(
            "- Attributes: `{}`\n",
            display
                .attributes
                .iter()
                .map(|attribute| attribute.name.as_deref().unwrap_or(attribute.text.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(preview) = &display.documentation_preview {
        report.push_str(&format!(
            "- Doc preview: `{}`\n",
            escape_debug_text(preview)
        ));
    }
    if !display.doc_comments.is_empty() {
        report.push_str(&format!(
            "- Raw doc comments: `{}`\n",
            display.doc_comments.len()
        ));
    }
    if let Some(form) = display.callable_form {
        report.push_str(&format!("- Callable form: `{}`\n", form.as_str()));
    }
    if !display.conditional_context.is_empty() {
        report.push_str(&format!(
            "- Conditional context: `{}`\n",
            display
                .conditional_context
                .iter()
                .map(|branch| format!(
                    "{} {}",
                    branch.kind.as_str(),
                    branch.condition.as_deref().unwrap_or("")
                ))
                .collect::<Vec<_>>()
                .join(" > ")
        ));
    }
}

fn append_external_display_details(
    report: &mut String,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let Some(symbol) = index.symbol(id) else {
        report.push_str("Selected external symbol is missing from the index.\n");
        return;
    };
    let Some(display) = query.symbol_display(id) else {
        report.push_str("Selected external symbol has no display information.\n");
        return;
    };
    let file = index.file(id.file_id);

    report.push_str("- Source: `external`\n");
    report.push_str(&format!("- Kind: `{}`\n", symbol_kind_label(display.kind)));
    report.push_str(&format!(
        "- Label: `{}`\n",
        escape_debug_text(&display.label)
    ));
    if let Some(signature) = &display.signature {
        report.push_str(&format!(
            "- Signature: `{}`\n",
            escape_debug_text(signature)
        ));
    }
    if let Some(detail) = &display.detail {
        report.push_str(&format!("- Detail: `{}`\n", escape_debug_text(detail)));
    }
    report.push_str(&format!("- Span: `{}`\n", format_span(symbol.span)));
    report.push_str(&format!(
        "- Selection span: `{}`\n",
        format_span(symbol.selection_span)
    ));
    if let Some(file) = file {
        report.push_str(&format!(
            "- Source kind: `{}`\n",
            file.metadata.kind.as_str()
        ));
        report.push_str(&format!(
            "- Source category: `{}`\n",
            file.metadata.category.as_str()
        ));
        report.push_str(&format!("- Priority: `{}`\n", file.metadata.priority));
        if let Some(path) = &file.metadata.relative_path {
            report.push_str(&format!(
                "- Relative path: `{}`\n",
                escape_debug_text(&path.display().to_string())
            ));
        }
        if let Some(path) = &file.metadata.absolute_path {
            report.push_str(&format!(
                "- Absolute path: `{}`\n",
                escape_debug_text(&path.display().to_string())
            ));
        }
    }
    if !display.modifiers.is_empty() {
        report.push_str(&format!(
            "- Modifiers: `{}`\n",
            display.modifiers.join(", ")
        ));
    }
    if !display.attributes.is_empty() {
        report.push_str(&format!(
            "- Attributes: `{}`\n",
            display
                .attributes
                .iter()
                .map(|attribute| attribute.name.as_deref().unwrap_or(attribute.text.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(preview) = &display.documentation_preview {
        report.push_str(&format!(
            "- Doc preview: `{}`\n",
            escape_debug_text(preview)
        ));
    }
    if let Some(form) = display.callable_form {
        report.push_str(&format!("- Callable form: `{}`\n", form.as_str()));
    }
}

fn append_hover_candidates(
    report: &mut String,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    candidates: &[crate::resolver::ReferenceCandidate],
) {
    if candidates.is_empty() {
        report.push_str("None.\n");
        return;
    }
    report.push_str("| Rank | Match | Kind | Label | Span | Detail |\n");
    report.push_str("| ---: | --- | --- | --- | --- | --- |\n");
    for (index_in_list, candidate) in candidates.iter().take(DEBUG_CANDIDATE_LIMIT).enumerate() {
        let display = query.symbol_display(candidate.id);
        let symbol = index.symbol(candidate.id);
        report.push_str(&format!(
            "| {} | {} | `{}` | `{}` | `{}` | `{}` |\n",
            index_in_list + 1,
            candidate.reason.as_str(),
            display
                .as_ref()
                .map(|display| symbol_kind_label(display.kind))
                .unwrap_or("<missing>"),
            display
                .as_ref()
                .map(|display| escape_table_text(&display.label))
                .unwrap_or_else(|| "<missing>".to_string()),
            symbol
                .map(|symbol| format_span(symbol.selection_span))
                .unwrap_or_else(|| "<missing>".to_string()),
            display
                .as_ref()
                .and_then(|display| display.signature.as_ref().or(display.detail.as_ref()))
                .map(|value| escape_table_text(value))
                .unwrap_or_default()
        ));
    }
    if candidates.len() > DEBUG_CANDIDATE_LIMIT {
        report.push_str(&format!(
            "\n{} additional candidates omitted.\n",
            candidates.len() - DEBUG_CANDIDATE_LIMIT
        ));
    }
}

fn append_resolver_resolution(
    report: &mut String,
    query: &IndexQuery<'_>,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    resolution: Option<&crate::resolver::ReferenceResolution>,
) {
    let Some(resolution) = resolution else {
        report.push_str("Cursor is not on an identifier token; resolver syntax-span hover will be used if a symbol span contains the cursor.\n");
        return;
    };

    report.push_str(&format!(
        "- Token: `{}` at `{}`\n",
        escape_debug_text(&resolution.token_text),
        format_span(resolution.token_span)
    ));
    report.push_str(&format!("- Reason: `{}`\n", resolution.reason.as_str()));
    report.push_str(&format!(
        "- Identifier context: `{}`\n",
        resolution.identifier_context.as_str()
    ));
    if let Some(receiver) = &resolution.receiver {
        report.push_str("\n### Receiver\n\n");
        report.push_str(&format!(
            "- Receiver text: `{}` at `{}`\n",
            escape_debug_text(&receiver.receiver_text),
            format_span(receiver.receiver_span)
        ));
        report.push_str(&format!(
            "- Receiver expression kind: `{}`\n",
            receiver.receiver_expression_kind
        ));
        report.push_str(&format!(
            "- Inferred owner type: `{}`\n",
            receiver
                .owner_type
                .as_deref()
                .map(escape_debug_text)
                .unwrap_or_else(|| "<none>".to_string())
        ));
        report.push_str(&format!(
            "- Static-looking receiver: `{}`\n",
            receiver.is_static
        ));
        if let Some(failure) = &receiver.failure_reason {
            report.push_str(&format!("- Failure: `{}`\n", escape_debug_text(failure)));
        }
        if !receiver.lookup_path.is_empty() {
            report.push_str("- Lookup path:\n");
            for step in &receiver.lookup_path {
                report.push_str(&format!("  - `{}`\n", escape_debug_text(step)));
            }
        }
        report.push('\n');
    }
    report.push_str(&format!("- Candidates: {}\n", resolution.candidates.len()));
    if let Some(selected) = &resolution.selected {
        let selected_label = selected
            .name
            .as_deref()
            .map(escape_debug_text)
            .unwrap_or_else(|| "<unknown>".to_string());
        report.push_str(&format!(
            "- Selected: `{}` `{}` from `{}`\n",
            symbol_kind_label(selected.kind),
            selected_label,
            selected.source.as_str()
        ));
    } else {
        report.push_str("- Selected: none\n");
    }

    if resolution.candidates.is_empty() {
        return;
    }

    report.push_str("\n| Rank | Source | Reason | Kind | Label | Span | Detail |\n");
    report.push_str("| ---: | --- | --- | --- | --- | --- | --- |\n");
    for (index_in_list, candidate) in resolution
        .candidates
        .iter()
        .take(DEBUG_CANDIDATE_LIMIT)
        .enumerate()
    {
        let display = match candidate.source {
            CandidateSource::FileLocal => query.symbol_display(candidate.id),
            CandidateSource::External => ExternalIndexes::new(workspace_index, game_data_index)
                .for_candidate(candidate)
                .map(IndexQuery::new)
                .and_then(|query| query.symbol_display(candidate.id)),
        };
        report.push_str(&format!(
            "| {} | `{}` | `{}` | `{}` | `{}` | `{}` | `{}` |\n",
            index_in_list + 1,
            candidate.source.as_str(),
            candidate.reason.as_str(),
            symbol_kind_label(candidate.kind),
            candidate
                .name
                .as_deref()
                .map(escape_table_text)
                .unwrap_or_else(|| "<unknown>".to_string()),
            format_span(candidate.selection_span),
            display
                .as_ref()
                .and_then(|display| display.signature.as_ref().or(display.detail.as_ref()))
                .map(|value| escape_table_text(value))
                .unwrap_or_default()
        ));
    }
    if resolution.candidates.len() > DEBUG_CANDIDATE_LIMIT {
        report.push_str(&format!(
            "\n{} additional resolver candidates omitted.\n",
            resolution.candidates.len() - DEBUG_CANDIDATE_LIMIT
        ));
    }
}

fn append_external_index_status(report: &mut String, status: Option<&ExternalIndexStatusSummary>) {
    let Some(status) = status else {
        report.push_str("No runtime external index status was provided.\n");
        return;
    };

    report.push_str(&format!("- Status: `{}`\n", status.status));
    report.push_str(&format!("- Generation: `{}`\n", status.generation));
    report.push_str(&format!("- Files: `{}`\n", status.files));
    report.push_str(&format!("- Symbols: `{}`\n", status.symbols));
    report.push_str(&format!(
        "- Parse diagnostics: `{}`\n",
        status.parse_diagnostics
    ));
    report.push_str(&format!(
        "- Workspace files/symbols/diagnostics: `{}` / `{}` / `{}`\n",
        status.workspace_files, status.workspace_symbols, status.workspace_parse_diagnostics
    ));
    report.push_str(&format!(
        "- Game-data files/symbols/diagnostics: `{}` / `{}` / `{}`\n",
        status.game_data_files, status.game_data_symbols, status.game_data_parse_diagnostics
    ));
    if let Some(cache_status) = &status.cache_status {
        report.push_str(&format!(
            "- Cache status: `{}`\n",
            escape_debug_text(cache_status)
        ));
    }
    if let Some(detail) = &status.cache_detail {
        report.push_str(&format!(
            "- Cache detail: `{}`\n",
            escape_debug_text(detail)
        ));
    }
    if let Some(fingerprint) = &status.fingerprint {
        report.push_str(&format!(
            "- Fingerprint: `{}`\n",
            escape_debug_text(fingerprint)
        ));
    }
    if let Some(error) = &status.error {
        report.push_str(&format!("- Error: `{}`\n", escape_debug_text(error)));
    }
}

fn append_parent_chain(
    report: &mut String,
    source: &str,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let mut current = index.symbol(id).and_then(|symbol| symbol.parent);
    if current.is_none() {
        report.push_str("None.\n");
        return;
    }
    while let Some(parent_id) = current {
        append_symbol_bullet(report, source, index, query, parent_id);
        current = index.symbol(parent_id).and_then(|symbol| symbol.parent);
    }
}

fn append_children(
    report: &mut String,
    source: &str,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let children = index.children(id);
    if children.is_empty() {
        report.push_str("None.\n");
        return;
    }
    for child in children.iter().take(DEBUG_CHILD_LIMIT) {
        append_symbol_bullet(report, source, index, query, *child);
    }
    if children.len() > DEBUG_CHILD_LIMIT {
        report.push_str(&format!(
            "- ... {} more children\n",
            children.len() - DEBUG_CHILD_LIMIT
        ));
    }
}

fn append_external_parent_chain(
    report: &mut String,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let mut current = index.symbol(id).and_then(|symbol| symbol.parent);
    if current.is_none() {
        report.push_str("None.\n");
        return;
    }
    while let Some(parent_id) = current {
        append_external_symbol_bullet(report, index, query, parent_id);
        current = index.symbol(parent_id).and_then(|symbol| symbol.parent);
    }
}

fn append_external_children(
    report: &mut String,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let children = index.children(id);
    if children.is_empty() {
        report.push_str("None.\n");
        return;
    }
    for child in children.iter().take(DEBUG_CHILD_LIMIT) {
        append_external_symbol_bullet(report, index, query, *child);
    }
    if children.len() > DEBUG_CHILD_LIMIT {
        report.push_str(&format!(
            "- ... {} more children\n",
            children.len() - DEBUG_CHILD_LIMIT
        ));
    }
}

fn append_external_symbol_bullet(
    report: &mut String,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let Some(symbol) = index.symbol(id) else {
        return;
    };
    let Some(display) = query.symbol_display(id) else {
        return;
    };
    let detail = display
        .signature
        .as_ref()
        .or(display.detail.as_ref())
        .map(|value| format!(" - `{}`", escape_debug_text(value)))
        .unwrap_or_default();
    let path = index
        .file(id.file_id)
        .and_then(|file| file.metadata.relative_path.as_ref())
        .map(|path| format!(" `{}`", escape_debug_text(&path.display().to_string())))
        .unwrap_or_default();
    report.push_str(&format!(
        "- `{}` `{}`{} at `{}`{}\n",
        symbol_kind_label(display.kind),
        escape_debug_text(&display.label),
        detail,
        format_span(symbol.selection_span),
        path
    ));
}

fn append_symbol_bullet(
    report: &mut String,
    source: &str,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    id: GlobalSymbolId,
) {
    let Some(symbol) = index.symbol(id) else {
        return;
    };
    let Some(display) = query.symbol_display(id) else {
        return;
    };
    let detail = display
        .signature
        .as_ref()
        .or(display.detail.as_ref())
        .map(|value| format!(" - `{}`", escape_debug_text(value)))
        .unwrap_or_default();
    report.push_str(&format!(
        "- `{}` `{}`{} at `{}` {}\n",
        symbol_kind_label(display.kind),
        escape_debug_text(&display.label),
        detail,
        format_span(symbol.selection_span),
        format_range(source, symbol.selection_span)
    ));
}

fn append_symbol_kind_counts(report: &mut String, index: &SymbolIndex) {
    let mut counts = BTreeMap::<SymbolKind, usize>::new();
    for symbol in index.symbols() {
        *counts.entry(symbol.kind).or_default() += 1;
    }
    report.push_str("| Kind | Count |\n");
    report.push_str("| --- | ---: |\n");
    for (kind, count) in counts {
        report.push_str(&format!("| `{}` | {} |\n", symbol_kind_label(kind), count));
    }
}

fn line_bounds(source: &str, target_line: u32) -> Option<(usize, usize)> {
    let mut current_line = 0u32;
    let mut start = 0usize;
    for (index, value) in source.char_indices() {
        if current_line == target_line && value == '\n' {
            return Some((start, index));
        }
        if value == '\n' {
            current_line += 1;
            start = index + value.len_utf8();
        }
    }
    (current_line == target_line).then_some((start, source.len()))
}

fn format_optional_usize(value: Option<usize>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "<invalid>".to_string())
}

fn format_span(span: TextSpan) -> String {
    format!("{}..{}", span.start, span.end)
}

fn format_range(source: &str, span: TextSpan) -> String {
    let start = position_for_offset(source, span.start);
    let end = position_for_offset(source, span.end);
    format_range_from_lsp(LspRange { start, end })
}

fn format_range_from_lsp(range: LspRange) -> String {
    format!(
        "L{}:C{}-L{}:C{}",
        range.start.line, range.start.character, range.end.line, range.end.character
    )
}

fn span_contains_or_touches_offset(span: TextSpan, offset: usize) -> bool {
    if span.is_empty() {
        return span.start == offset;
    }
    span.start <= offset && offset < span.end
}

fn escape_table_text(value: &str) -> String {
    escape_debug_text(value).replace('|', "\\|")
}

fn escape_debug_text(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

fn escape_fence_text(value: &str) -> String {
    value.replace("```", "`\u{200b}``")
}

pub(crate) fn selected_label_from_debug_report(report: &str) -> Option<String> {
    report
        .lines()
        .find_map(|line| line.strip_prefix("- Label: `"))
        .and_then(|line| line.strip_suffix('`'))
        .map(str::to_string)
}
