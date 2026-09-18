use crate::analysis_runtime::DocumentSnapshot;
use crate::index::{GlobalSymbolId, SymbolIndex};
use crate::index_query::IndexQuery;
use crate::lexer::TokenKind;
use crate::lsp::external_indexes::ExternalIndexes;
use crate::lsp::hover_render::{
    render_hover_markdown as render_hover_markdown_with_context, HoverLinkContext,
    HoverRenderContext,
};
use crate::lsp::open_documents::ForegroundQuerySnapshot;
use crate::lsp::{
    file_index_for_source, offset_for_position, range_for_span, FileIndexAnalysis,
    LspMarkupContent, LspPosition, LspRange,
};
use crate::model::SymbolKind;
use crate::resolver::{
    CandidateSource, HoverResolution, IdentifierContext, ReceiverResolution, ReferenceResolver,
    ResolutionReason,
};
use serde::Serialize;

const SYNTHETIC_HOVER_URI: &str = "file:///hover-source.c";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspHover {
    pub contents: LspMarkupContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<LspRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspHoverReport {
    pub hover: Option<LspHover>,
    pub parse_diagnostics: usize,
    pub selected_label: Option<String>,
    pub selected_kind: Option<SymbolKind>,
    pub selected_source: Option<CandidateSource>,
    pub selection_source: HoverSelectionSource,
    pub resolver_reason: Option<ResolutionReason>,
    pub identifier_context: Option<IdentifierContext>,
    pub resolver_candidate_count: usize,
    pub receiver_resolution: Option<ReceiverResolution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverSelectionSource {
    /// A current-revision lexer fact. This never attempts name resolution.
    Lexical,
    ResolverIdentifier,
    ResolverSyntaxSpan,
    None,
}

impl HoverSelectionSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lexical => "lexical",
            Self::ResolverIdentifier => "resolver-identifier",
            Self::ResolverSyntaxSpan => "resolver-syntax-span",
            Self::None => "none",
        }
    }
}

impl LspHoverReport {
    pub fn is_hit(&self) -> bool {
        self.hover.is_some()
    }

    fn with_parse_diagnostics(mut self, parse_diagnostics: usize) -> Self {
        self.parse_diagnostics = parse_diagnostics;
        self
    }
}

pub fn hover_report_for_source_position(source: &str, position: LspPosition) -> LspHoverReport {
    hover_report_for_source_position_with_external(source, position, None)
}

pub fn hover_report_for_source_position_with_external(
    source: &str,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
) -> LspHoverReport {
    let analysis = file_index_for_source(source);
    hover_report_for_cached_analysis_with_external(source, &analysis, position, external_index)
}

pub(crate) fn hover_report_for_cached_analysis_with_external(
    source: &str,
    analysis: &FileIndexAnalysis,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
) -> LspHoverReport {
    hover_report_for_cached_analysis_with_external_indexes(
        source,
        analysis,
        SYNTHETIC_HOVER_URI,
        position,
        None,
        external_index,
    )
}

/// Produces the deliberately narrow hover result allowed while full local
/// analysis for an accepted snapshot is still pending.  In particular,
/// identifiers return no hover: a name can only be interpreted by the
/// revision-matching semantic query and must never borrow an older index.
pub(crate) fn hover_report_for_pending_snapshot(
    snapshot: &DocumentSnapshot,
    foreground: &ForegroundQuerySnapshot,
    position: LspPosition,
    parse_diagnostics: usize,
) -> LspHoverReport {
    let Some(offset) = snapshot.positions().and_then(|positions| {
        positions.offset_for_position(crate::analysis_runtime::Position {
            line: position.line,
            character: position.character,
        })
    }) else {
        return empty_hover_report(parse_diagnostics);
    };
    let source = snapshot.text();
    let Some(token) = foreground.token_at_offset(offset) else {
        return empty_hover_report(parse_diagnostics);
    };

    let category = match token.kind {
        TokenKind::Keyword(_) => "Keyword",
        TokenKind::Number | TokenKind::InvalidNumber => "Number literal",
        TokenKind::String | TokenKind::UnterminatedString => "String literal",
        TokenKind::LineComment
        | TokenKind::DocLineComment
        | TokenKind::BlockComment
        | TokenKind::DocBlockComment
        | TokenKind::UnterminatedBlockComment => "Comment",
        _ => return empty_hover_report(parse_diagnostics),
    };
    let text = source
        .get(token.span.start..token.span.end)
        .unwrap_or_default();
    LspHoverReport {
        hover: Some(LspHover {
            contents: LspMarkupContent {
                kind: "markdown".to_string(),
                value: format!("**{category}**\n\n```enforce\n{text}\n```"),
            },
            range: Some(range_for_span(source, token.span)),
        }),
        parse_diagnostics,
        selected_label: Some(text.to_string()),
        selected_kind: None,
        selected_source: None,
        selection_source: HoverSelectionSource::Lexical,
        resolver_reason: None,
        identifier_context: None,
        resolver_candidate_count: 0,
        receiver_resolution: None,
    }
}

pub(crate) fn hover_report_for_cached_analysis_with_external_indexes(
    source: &str,
    analysis: &FileIndexAnalysis,
    current_uri: &str,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspHoverReport {
    hover_report_for_cached_analysis_with_external_layers(
        source,
        analysis,
        current_uri,
        position,
        workspace_index,
        game_data_index,
    )
}

fn hover_report_for_cached_analysis_with_external_layers(
    source: &str,
    analysis: &FileIndexAnalysis,
    current_uri: &str,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspHoverReport {
    let Some(offset) = offset_for_position(source, position) else {
        return empty_hover_report(analysis.parse_diagnostics);
    };
    hover_report_for_offset(
        source,
        analysis,
        current_uri,
        offset,
        workspace_index,
        game_data_index,
    )
}

pub fn hover_reports_for_source_positions(
    source: &str,
    positions: &[LspPosition],
) -> Vec<LspHoverReport> {
    hover_reports_for_source_positions_with_external(source, positions, None)
}

pub fn hover_reports_for_source_positions_with_external(
    source: &str,
    positions: &[LspPosition],
    external_index: Option<&SymbolIndex>,
) -> Vec<LspHoverReport> {
    let analysis = file_index_for_source(source);
    positions
        .iter()
        .map(|position| {
            offset_for_position(source, *position)
                .map(|offset| {
                    hover_report_for_offset(
                        source,
                        &analysis,
                        SYNTHETIC_HOVER_URI,
                        offset,
                        None,
                        external_index,
                    )
                })
                .unwrap_or_else(|| empty_hover_report(analysis.parse_diagnostics))
        })
        .collect()
}

fn hover_report_for_offset(
    source: &str,
    analysis: &FileIndexAnalysis,
    current_uri: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspHoverReport {
    let query = IndexQuery::new(&analysis.index);
    let resolver = ReferenceResolver::new_with_parse_scope_and_external_indexes(
        source,
        &analysis.index,
        &analysis.syntax.parse,
        &analysis.scope,
        ExternalIndexes::new(workspace_index, game_data_index).ordered(),
    );
    match resolver.resolve_hover_at_offset(offset) {
        Some(HoverResolution::Identifier(resolution)) => {
            let candidate_count = resolution.candidates.len();
            let reason = resolution.reason;
            let identifier_context = resolution.identifier_context;
            if let Some(selected) = resolution.selected.as_ref() {
                match selected.source {
                    CandidateSource::FileLocal => {
                        let member_summary_query =
                            workspace_index.or(game_data_index).map(IndexQuery::new);
                        let game_data_query = game_data_index.map(IndexQuery::new);
                        if let Some(mut report) = hover_report_for_symbol(
                            source,
                            &analysis.index,
                            &query,
                            member_summary_query.as_ref(),
                            game_data_query.as_ref(),
                            current_uri,
                            selected.id,
                            Some(range_for_span(source, resolution.token_span)),
                            HoverSelectionSource::ResolverIdentifier,
                            Some(CandidateSource::FileLocal),
                            Some(reason),
                            Some(identifier_context),
                            candidate_count,
                        ) {
                            report.receiver_resolution = resolution.receiver.clone();
                            return report.with_parse_diagnostics(analysis.parse_diagnostics);
                        }
                    }
                    CandidateSource::External => {
                        if let Some(external_index) =
                            ExternalIndexes::new(workspace_index, game_data_index)
                                .for_candidate(selected)
                        {
                            let external_query = IndexQuery::new(external_index);
                            if let Some(mut report) = hover_report_for_symbol(
                                source,
                                external_index,
                                &external_query,
                                None,
                                None,
                                current_uri,
                                selected.id,
                                Some(range_for_span(source, resolution.token_span)),
                                HoverSelectionSource::ResolverIdentifier,
                                Some(CandidateSource::External),
                                Some(reason),
                                Some(identifier_context),
                                candidate_count,
                            ) {
                                report.receiver_resolution = resolution.receiver.clone();
                                return report.with_parse_diagnostics(analysis.parse_diagnostics);
                            }
                        }
                    }
                }
            }
            LspHoverReport {
                hover: None,
                parse_diagnostics: analysis.parse_diagnostics,
                selected_label: None,
                selected_kind: None,
                selected_source: None,
                selection_source: HoverSelectionSource::None,
                resolver_reason: Some(reason),
                identifier_context: Some(identifier_context),
                resolver_candidate_count: candidate_count,
                receiver_resolution: resolution.receiver.clone(),
            }
        }
        Some(HoverResolution::SyntaxSpan(resolution)) => {
            let Some(selected) = resolution.selected.as_ref() else {
                return empty_hover_report(analysis.parse_diagnostics);
            };
            let member_summary_query = workspace_index.or(game_data_index).map(IndexQuery::new);
            let game_data_query = game_data_index.map(IndexQuery::new);
            hover_report_for_symbol(
                source,
                &analysis.index,
                &query,
                member_summary_query.as_ref(),
                game_data_query.as_ref(),
                current_uri,
                selected.id,
                None,
                HoverSelectionSource::ResolverSyntaxSpan,
                Some(CandidateSource::FileLocal),
                Some(resolution.reason),
                None,
                resolution.candidates.len(),
            )
            .map(|report| report.with_parse_diagnostics(analysis.parse_diagnostics))
            .unwrap_or_else(|| empty_hover_report(analysis.parse_diagnostics))
        }
        None => empty_hover_report(analysis.parse_diagnostics),
    }
}

fn hover_report_for_symbol(
    source: &str,
    index: &SymbolIndex,
    query: &IndexQuery<'_>,
    member_summary_query: Option<&IndexQuery<'_>>,
    game_data_query: Option<&IndexQuery<'_>>,
    current_uri: &str,
    id: GlobalSymbolId,
    range_override: Option<LspRange>,
    selection_source: HoverSelectionSource,
    selected_source: Option<CandidateSource>,
    resolver_reason: Option<ResolutionReason>,
    identifier_context: Option<IdentifierContext>,
    resolver_candidate_count: usize,
) -> Option<LspHoverReport> {
    let display = query.symbol_display(id)?;
    let selected_kind = display.kind;
    let selected_label = display.label.clone();
    let symbol = index.symbol(id);
    let range = range_override
        .or_else(|| symbol.map(|symbol| range_for_span(source, symbol.selection_span)));
    Some(LspHoverReport {
        hover: Some(LspHover {
            contents: LspMarkupContent {
                kind: "markdown".to_string(),
                value: render_hover_markdown_with_context(
                    &display,
                    Some(HoverRenderContext {
                        query,
                        member_summary_query,
                        links: Some(HoverLinkContext {
                            current_uri,
                            external_query: member_summary_query,
                            game_data_query,
                        }),
                    }),
                ),
            },
            range,
        }),
        parse_diagnostics: 0,
        selected_label: Some(selected_label),
        selected_kind: Some(selected_kind),
        selected_source,
        selection_source,
        resolver_reason,
        identifier_context,
        resolver_candidate_count,
        receiver_resolution: None,
    })
}

fn empty_hover_report(parse_diagnostics: usize) -> LspHoverReport {
    LspHoverReport {
        hover: None,
        parse_diagnostics,
        selected_label: None,
        selected_kind: None,
        selected_source: None,
        selection_source: HoverSelectionSource::None,
        resolver_reason: None,
        identifier_context: None,
        resolver_candidate_count: 0,
        receiver_resolution: None,
    }
}
