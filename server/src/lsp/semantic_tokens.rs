use crate::index::SymbolIndex;
use crate::lexer::{lex, TextSpan, Token, TokenKind};
use crate::lsp::external_indexes::ExternalIndexes;
use crate::lsp::{
    file_index_for_source, range_for_span, span_text, FileIndexAnalysis, LspPositionIndex, LspRange,
};
use crate::model::SymbolKind;
use crate::resolver::{
    CandidateSource, IdentifierContext, ReferenceCandidate, ReferenceResolver,
    ReferenceResolverTimings, ResolutionReason,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "class",
    "enum",
    "type",
    "function",
    "reforgerField",
    "variable",
    "parameter",
    "enumMember",
    "keyword",
    "comment",
    "string",
    "number",
    "operator",
    "reforgerPunctuation",
    "reforgerPreprocessor",
    "typeParameter",
];

pub(crate) const SEMANTIC_TOKEN_MODIFIERS: &[&str] = &[
    "declaration",
    "static",
    "readonly",
    "deprecated",
    "abstract",
    "modification",
];

const SEMANTIC_MOD_DECLARATION: u32 = 1 << 0;
const SEMANTIC_MOD_STATIC: u32 = 1 << 1;
const SEMANTIC_MOD_READONLY: u32 = 1 << 2;
const SEMANTIC_MOD_MODIFICATION: u32 = 1 << 5;
const RESOLVER_REFERENCE_PRIORITY: u8 = 60;
const RESOLVER_TYPE_REFERENCE_PRIORITY: u8 = 80;
const MAX_RAW_SEMANTIC_TOKENS: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspSemanticTokens {
    pub data: Vec<u32>,
}

/// Full-response wire form for `textDocument/semanticTokens/full`.
///
/// The current server intentionally does not advertise delta tokens. `resultId`
/// still identifies the exact lexical baseline or rich overlay that produced a
/// full response, so a later delta-capable implementation has an explicit,
/// revision-safe lifecycle to extend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LspSemanticTokensFull {
    pub(crate) result_id: String,
    pub(crate) data: Vec<u32>,
}

impl LspSemanticTokensFull {
    pub(crate) fn from_tokens(result_id: String, tokens: &LspSemanticTokens) -> Self {
        Self {
            result_id,
            data: tokens.data.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSemanticTokenProjection {
    pub tokens: LspSemanticTokens,
    pub token_count: usize,
    pub parse_diagnostics: usize,
    pub timings: LspSemanticTokenTimings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSemanticTokenReport {
    pub tokens: LspSemanticTokens,
    pub decoded: Vec<SemanticTokenDebug>,
    pub parse_diagnostics: usize,
    pub timings: LspSemanticTokenTimings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LspSemanticTokenTimings {
    pub lex_ms: u128,
    pub token_loop_ms: u128,
    pub resolver_ms: u128,
    pub resolver_context_ms: u128,
    pub resolver_declaration_ms: u128,
    pub resolver_scope_ms: u128,
    pub resolver_member_ms: u128,
    pub resolver_top_level_ms: u128,
    pub resolver_external_ms: u128,
    pub resolver_selection_ms: u128,
    pub declaration_overlay_ms: u128,
    pub symbol_declaration_overlay_ms: u128,
    pub delimiter_overlay_ms: u128,
    pub sort_filter_split_ms: u128,
    pub encode_ms: u128,
    pub decode_debug_ms: u128,
    pub identifier_resolver_calls: usize,
    pub identifier_results_reused: usize,
    pub identifier_regions_reused: usize,
    pub identifier_regions_invalidated: usize,
    pub identifier_regions_recomputed: usize,
    pub delimiter_resolver_calls: usize,
    pub delimiter_owners_reused: usize,
    pub delimiter_owners_invalidated: usize,
    pub delimiter_owners_recomputed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticTokenDebug {
    pub text: String,
    pub range: LspRange,
    pub token_type: &'static str,
    pub modifiers: Vec<&'static str>,
}

pub fn semantic_tokens_report_for_source(source: &str) -> LspSemanticTokenReport {
    let analysis = file_index_for_source(source);
    semantic_tokens_report_for_cached_analysis(source, &analysis)
}

pub fn semantic_tokens_report_for_source_with_bracket_coloring(
    source: &str,
    bracket_coloring: BracketColoringMode,
) -> LspSemanticTokenReport {
    let analysis = file_index_for_source(source);
    semantic_tokens_report_for_cached_analysis_with_bracket_coloring(
        source,
        &analysis,
        None,
        None,
        bracket_coloring,
    )
}

pub fn semantic_tokens_report_for_source_with_external(
    source: &str,
    external_index: Option<&SymbolIndex>,
) -> LspSemanticTokenReport {
    let analysis = file_index_for_source(source);
    semantic_tokens_report_for_cached_analysis_with_external(source, &analysis, external_index)
}

pub fn semantic_tokens_for_source_with_external(
    source: &str,
    external_index: Option<&SymbolIndex>,
) -> LspSemanticTokenProjection {
    let analysis = file_index_for_source(source);
    semantic_tokens_for_cached_analysis_with_external(source, &analysis, external_index)
}

/// Projects only facts available from the current lexer input.
///
/// This entrypoint deliberately does not construct a `FileIndexAnalysis`: it
/// is safe to use while a newer document revision is awaiting syntax and
/// semantic analysis. It preserves the shared semantic-token encoding rules,
/// including UTF-16 positions and CRLF-aware multiline token splitting.
#[cfg(test)]
pub(crate) fn lexical_semantic_tokens_for_source(source: &str) -> LspSemanticTokenProjection {
    lexical_semantic_tokens_for_source_with_bracket_coloring(
        source,
        BracketColoringMode::Semantic,
        &BTreeSet::new(),
    )
}

pub(crate) fn lexical_semantic_tokens_for_source_with_bracket_coloring(
    source: &str,
    bracket_coloring: BracketColoringMode,
    generic_angle_offsets: &BTreeSet<usize>,
) -> LspSemanticTokenProjection {
    let lex_start = Instant::now();
    let lexer_tokens = lex(source);
    let lex_elapsed = lex_start.elapsed();
    let raw_projection = lexical_raw_tokens(
        source,
        &lexer_tokens,
        lex_elapsed,
        bracket_coloring,
        generic_angle_offsets,
        None,
    )
    .expect("lexical semantic token projection is not cancellable through this entrypoint");
    encode_lexical_projection(source, raw_projection, None)
        .expect("lexical semantic token projection is not cancellable through this entrypoint")
}

fn semantic_tokens_report_for_cached_analysis(
    source: &str,
    analysis: &FileIndexAnalysis,
) -> LspSemanticTokenReport {
    semantic_tokens_report_for_cached_analysis_with_external(source, analysis, None)
}

pub(crate) fn semantic_tokens_report_for_cached_analysis_with_external(
    source: &str,
    analysis: &FileIndexAnalysis,
    external_index: Option<&SymbolIndex>,
) -> LspSemanticTokenReport {
    semantic_tokens_report_for_cached_analysis_with_external_indexes(
        source,
        analysis,
        None,
        external_index,
    )
}

pub(crate) fn semantic_tokens_report_for_cached_analysis_with_external_indexes(
    source: &str,
    analysis: &FileIndexAnalysis,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspSemanticTokenReport {
    semantic_tokens_report_for_cached_analysis_with_bracket_coloring(
        source,
        analysis,
        workspace_index,
        game_data_index,
        BracketColoringMode::Semantic,
    )
}

fn semantic_tokens_report_for_cached_analysis_with_bracket_coloring(
    source: &str,
    analysis: &FileIndexAnalysis,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    bracket_coloring: BracketColoringMode,
) -> LspSemanticTokenReport {
    let raw_projection = semantic_raw_tokens(
        source,
        analysis,
        workspace_index,
        game_data_index,
        bracket_coloring,
        None,
        None,
    )
    .expect("semantic token reports are not cancellable");
    let decode_start = Instant::now();
    let decoded = raw_projection
        .tokens
        .iter()
        .map(|token| SemanticTokenDebug {
            text: span_text(source, token.span).to_string(),
            range: range_for_span(source, token.span),
            token_type: semantic_token_type_name(token.token_type),
            modifiers: semantic_modifier_names(token.modifiers),
        })
        .collect::<Vec<_>>();
    let encode_start = Instant::now();
    let data = encode_semantic_tokens(source, &raw_projection.tokens, None)
        .expect("semantic token reports are not cancellable");
    let mut timings = raw_projection.timings;
    timings.decode_debug_ms = decode_start.elapsed().as_millis();
    timings.encode_ms = encode_start.elapsed().as_millis();

    LspSemanticTokenReport {
        tokens: LspSemanticTokens { data },
        decoded,
        parse_diagnostics: analysis.parse_diagnostics,
        timings,
    }
}

pub(crate) fn semantic_tokens_for_cached_analysis_with_external(
    source: &str,
    analysis: &FileIndexAnalysis,
    external_index: Option<&SymbolIndex>,
) -> LspSemanticTokenProjection {
    semantic_tokens_for_cached_analysis_with_external_indexes(
        source,
        analysis,
        None,
        external_index,
    )
}

pub(crate) fn semantic_tokens_for_cached_analysis_with_external_indexes(
    source: &str,
    analysis: &FileIndexAnalysis,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspSemanticTokenProjection {
    semantic_tokens_for_cached_analysis_with_external_indexes_and_bracket_coloring(
        source,
        analysis,
        workspace_index,
        game_data_index,
        BracketColoringMode::Semantic,
    )
}

pub(crate) fn semantic_tokens_for_cached_analysis_with_external_indexes_and_bracket_coloring(
    source: &str,
    analysis: &FileIndexAnalysis,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    bracket_coloring: BracketColoringMode,
) -> LspSemanticTokenProjection {
    let raw_projection = semantic_raw_tokens(
        source,
        analysis,
        workspace_index,
        game_data_index,
        bracket_coloring,
        None,
        None,
    )
    .expect("rich semantic token projection is not cancellable through this entrypoint");
    encode_projection(source, analysis, raw_projection, None)
        .expect("rich semantic token projection is not cancellable through this entrypoint")
}

pub(crate) struct IncrementalSemanticTokenProjection {
    pub(crate) projection: LspSemanticTokenProjection,
    pub(crate) cache: RichSemanticProjectionCache,
}

#[derive(Clone)]
pub(crate) struct RichSemanticProjectionCache {
    revision: u64,
    external_generation: u64,
    delimiter_owner_cache: super::scope_delimiters::DelimiterOwnerProjectionCache,
    identifier_cache: IdentifierProjectionCache,
}

impl RichSemanticProjectionCache {
    pub(crate) fn rebind_external_generation(
        &mut self,
        revision: u64,
        previous_generation: u64,
        external_generation: u64,
    ) -> bool {
        if self.revision != revision || self.external_generation != previous_generation {
            return false;
        }
        if !self.delimiter_owner_cache.rebind_external_generation(
            revision,
            previous_generation,
            external_generation,
        ) {
            return false;
        }
        self.external_generation = external_generation;
        true
    }
}

#[derive(Clone)]
struct IdentifierProjectionCache {
    source: Arc<str>,
    structure: Vec<super::scope_delimiters::SemanticStructureFact>,
    regions: Vec<CachedIdentifierRegion>,
}

#[derive(Clone)]
struct CachedIdentifierRegion {
    span: TextSpan,
    results: Vec<CachedIdentifierResult>,
}

#[derive(Clone, Copy)]
struct CachedIdentifierResult {
    relative_span: TextSpan,
    kind: Option<SymbolKind>,
    token_type: Option<u32>,
    priority: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct RichSemanticProjectionCacheContext<'cache> {
    revision: u64,
    external_generation: u64,
    previous_cache: Option<&'cache RichSemanticProjectionCache>,
}

impl<'cache> RichSemanticProjectionCacheContext<'cache> {
    pub(crate) fn new(
        revision: u64,
        external_generation: u64,
        previous_cache: Option<&'cache RichSemanticProjectionCache>,
    ) -> Self {
        Self {
            revision,
            external_generation,
            previous_cache,
        }
    }
}

pub(crate) fn semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled(
    source: &str,
    analysis: &FileIndexAnalysis,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    bracket_coloring: BracketColoringMode,
    cache_context: RichSemanticProjectionCacheContext<'_>,
    should_cancel: &dyn Fn() -> bool,
) -> Option<IncrementalSemanticTokenProjection> {
    let mut raw_projection = semantic_raw_tokens(
        source,
        analysis,
        workspace_index,
        game_data_index,
        bracket_coloring,
        Some(cache_context),
        Some(should_cancel),
    )?;
    let delimiter_owner_cache = raw_projection
        .delimiter_owner_cache
        .take()
        .expect("incremental rich projection produces a delimiter-owner cache");
    let identifier_cache = raw_projection
        .identifier_cache
        .take()
        .expect("incremental rich projection produces an identifier cache");
    let projection = encode_projection(source, analysis, raw_projection, Some(should_cancel))?;
    Some(IncrementalSemanticTokenProjection {
        projection,
        cache: RichSemanticProjectionCache {
            revision: cache_context.revision,
            external_generation: cache_context.external_generation,
            delimiter_owner_cache,
            identifier_cache,
        },
    })
}

fn encode_projection(
    source: &str,
    analysis: &FileIndexAnalysis,
    raw_projection: RawSemanticTokenProjection,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<LspSemanticTokenProjection> {
    let token_count = raw_projection.tokens.len();
    let encode_start = Instant::now();
    let data = encode_semantic_tokens(source, &raw_projection.tokens, should_cancel)?;
    let mut timings = raw_projection.timings;
    timings.encode_ms = encode_start.elapsed().as_millis();
    Some(LspSemanticTokenProjection {
        tokens: LspSemanticTokens { data },
        token_count,
        parse_diagnostics: analysis.parse_diagnostics,
        timings,
    })
}

fn encode_lexical_projection(
    source: &str,
    raw_projection: RawSemanticTokenProjection,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<LspSemanticTokenProjection> {
    let token_count = raw_projection.tokens.len();
    let encode_start = Instant::now();
    let data = encode_semantic_tokens(source, &raw_projection.tokens, should_cancel)?;
    let mut timings = raw_projection.timings;
    timings.encode_ms = encode_start.elapsed().as_millis();
    Some(LspSemanticTokenProjection {
        tokens: LspSemanticTokens { data },
        token_count,
        // Parsing has intentionally not run on this path.
        parse_diagnostics: 0,
        timings,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawSemanticToken {
    span: TextSpan,
    token_type: u32,
    modifiers: u32,
    priority: u8,
}

#[derive(Clone)]
struct RawSemanticTokenProjection {
    tokens: Vec<RawSemanticToken>,
    timings: LspSemanticTokenTimings,
    delimiter_owner_cache: Option<super::scope_delimiters::DelimiterOwnerProjectionCache>,
    identifier_cache: Option<IdentifierProjectionCache>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BracketColoringMode {
    #[default]
    Semantic,
    Punctuation,
    VsCode,
}

fn semantic_raw_tokens(
    source: &str,
    analysis: &FileIndexAnalysis,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
    bracket_coloring: BracketColoringMode,
    cache_context: Option<RichSemanticProjectionCacheContext<'_>>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<RawSemanticTokenProjection> {
    let lex_elapsed = Duration::default();
    let lexer_tokens = &analysis.syntax.lexer_tokens;
    if should_cancel.is_some_and(|should_cancel| should_cancel()) {
        return None;
    }
    let mut tokens = Vec::new();
    let declaration_spans = analysis
        .index
        .symbols()
        .iter()
        .filter(|symbol| {
            !symbol.selection_span.is_empty() && symbol.selection_span.end <= source.len()
        })
        .map(|symbol| (symbol.selection_span.start, symbol.selection_span.end))
        .collect::<std::collections::BTreeSet<_>>();
    let resolver = ReferenceResolver::new_with_parse_scope_and_external_indexes(
        source,
        &analysis.index,
        &analysis.syntax.parse,
        &analysis.scope,
        ExternalIndexes::new(workspace_index, game_data_index).ordered(),
    );
    let identifier_structure = if cache_context.is_some() {
        Some(super::scope_delimiters::semantic_structure_facts(
            &analysis.index,
            should_cancel,
        )?)
    } else {
        None
    };
    let identifier_regions = if cache_context.is_some() {
        identifier_reuse_regions(source, &analysis.index)
    } else {
        Vec::new()
    };
    let reusable_identifier_regions = if let (Some(context), Some(structure)) =
        (cache_context, identifier_structure.as_deref())
    {
        reusable_identifier_regions(
            source,
            context,
            structure,
            &identifier_regions,
            should_cancel,
        )?
    } else {
        vec![None; identifier_regions.len()]
    };
    let identifier_regions_reused = reusable_identifier_regions
        .iter()
        .filter(|region| region.is_some())
        .count();
    let identifier_regions_invalidated = cache_context
        .and_then(|context| context.previous_cache)
        .map_or(0, |cache| {
            cache
                .identifier_cache
                .regions
                .len()
                .saturating_sub(identifier_regions_reused)
        });
    let identifier_regions_recomputed = identifier_regions
        .len()
        .saturating_sub(identifier_regions_reused);
    let mut identifier_region_results = identifier_regions
        .iter()
        .map(|_| Vec::new())
        .collect::<Vec<Vec<CachedIdentifierResult>>>();

    let mut resolver_elapsed = Duration::default();
    let mut resolver_timings = ReferenceResolverTimings::default();
    let mut identifier_resolver_calls = 0usize;
    let mut identifier_results_reused = 0usize;
    let mut resolved_identifier_kinds = BTreeMap::new();
    let preprocessor_lines = preprocessor_line_spans(source);
    let mut preprocessor_line_index = 0usize;
    let token_loop_start = Instant::now();
    for (token_index, token) in lexer_tokens.iter().enumerate() {
        if token_index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        if token.kind == TokenKind::Whitespace || token.kind == TokenKind::Eof {
            continue;
        }
        if token_is_on_preprocessor_line(*token, &preprocessor_lines, &mut preprocessor_line_index)
        {
            if let Some(token_type) = preprocessor_line_semantic_type(source, *token) {
                push_raw_semantic_token(&mut tokens, raw_semantic(*token, token_type, 0, 20));
            }
            continue;
        }
        if bracket_coloring == BracketColoringMode::VsCode
            && is_standard_bracket_token_kind(token.kind)
        {
            continue;
        }
        if let Some(token_type) = lexical_semantic_type(token.kind) {
            let priority = if is_comment_token_kind(token.kind) {
                200
            } else {
                10
            };
            push_raw_semantic_token(&mut tokens, raw_semantic(*token, token_type, 0, priority));
        }
        if token.kind == TokenKind::Identifier {
            if declaration_spans.contains(&(token.span.start, token.span.end)) {
                continue;
            }
            let region_index = identifier_region_index(&identifier_regions, token.span);
            let cached = region_index.and_then(|region_index| {
                let region = reusable_identifier_regions[region_index]?;
                let relative_span = relative_span(token.span, identifier_regions[region_index])?;
                cached_identifier_result(region, relative_span)
            });
            let result = if let Some(cached) = cached {
                identifier_results_reused += 1;
                cached
            } else {
                identifier_resolver_calls += 1;
                let resolver_start = Instant::now();
                let (resolution, token_timings) =
                    resolver.resolve_identifier_token_profiled(token.span);
                resolver_elapsed += resolver_start.elapsed();
                resolver_timings.context += token_timings.context;
                resolver_timings.declaration += token_timings.declaration;
                resolver_timings.scope += token_timings.scope;
                resolver_timings.member += token_timings.member;
                resolver_timings.top_level += token_timings.top_level;
                resolver_timings.external += token_timings.external;
                resolver_timings.selection += token_timings.selection;
                let candidate = resolution
                    .as_ref()
                    .and_then(|resolution| resolution.selected.as_ref());
                let builtin_collection_type = resolution
                    .as_ref()
                    .and_then(builtin_collection_type_semantic_type);
                CachedIdentifierResult {
                    relative_span: region_index
                        .and_then(|index| relative_span(token.span, identifier_regions[index]))
                        .unwrap_or(token.span),
                    kind: candidate.map(|candidate| candidate.kind),
                    token_type: candidate
                        .and_then(|candidate| {
                            candidate_semantic_type(
                                candidate,
                                &analysis.index,
                                workspace_index,
                                game_data_index,
                            )
                        })
                        .or(builtin_collection_type),
                    priority: candidate
                        .map(|candidate| resolver_reference_priority(candidate.kind))
                        .or_else(|| {
                            builtin_collection_type.map(|_| RESOLVER_TYPE_REFERENCE_PRIORITY)
                        })
                        .unwrap_or(0),
                }
            };
            resolved_identifier_kinds.insert((token.span.start, token.span.end), result.kind);
            if let Some(region_index) = region_index {
                identifier_region_results[region_index].push(result);
            }
            if let Some(token_type) = result.token_type {
                push_raw_semantic_token(
                    &mut tokens,
                    RawSemanticToken {
                        span: token.span,
                        token_type,
                        modifiers: 0,
                        priority: result.priority,
                    },
                );
            }
        }
    }
    if should_cancel.is_some_and(|should_cancel| should_cancel()) {
        return None;
    }
    let token_loop_elapsed = token_loop_start.elapsed();

    let declaration_overlay_start = Instant::now();
    for symbol in analysis.index.symbols() {
        if should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        if symbol.selection_span.is_empty() || symbol.selection_span.end > source.len() {
            continue;
        }
        let Some(token_type) = symbol_semantic_type(symbol.kind) else {
            continue;
        };
        push_raw_semantic_token(
            &mut tokens,
            RawSemanticToken {
                span: symbol.selection_span,
                token_type,
                modifiers: symbol_semantic_modifiers(symbol),
                priority: 100,
            },
        );
    }
    let declaration_overlay_elapsed = declaration_overlay_start.elapsed();

    let delimiter_overlay_start = Instant::now();
    // Reuse selected kinds resolved earlier in this same immutable snapshot.
    // Dynamic delimiter proof otherwise repeats the identical resolver query.
    let delimiter_projection = if let Some(context) = cache_context {
        super::scope_delimiters::semantic_scope_delimiters_for_analysis_incremental(
            source,
            analysis,
            ExternalIndexes::new(workspace_index, game_data_index),
            &resolved_identifier_kinds,
            true,
            super::scope_delimiters::DelimiterProjectionCacheContext::new(
                context.revision,
                context.external_generation,
                context
                    .previous_cache
                    .map(|cache| &cache.delimiter_owner_cache),
            ),
            should_cancel,
        )?
    } else {
        super::scope_delimiters::semantic_scope_delimiters_for_analysis(
            source,
            analysis,
            ExternalIndexes::new(workspace_index, game_data_index),
            &resolved_identifier_kinds,
            true,
            should_cancel,
        )?
    };
    let super::scope_delimiters::ScopeDelimiterProjection {
        delimiters,
        dynamic_owner_resolver_calls: delimiter_resolver_calls,
        dynamic_owners_reused: delimiter_owners_reused,
        dynamic_owners_invalidated: delimiter_owners_invalidated,
        dynamic_owners_recomputed: delimiter_owners_recomputed,
        owner_cache: delimiter_owner_cache,
    } = delimiter_projection;
    let generic_angle_offsets = generic_angle_offsets_for_delimiters(source, &delimiters);
    let operator_type = semantic_type_index("operator");
    tokens = tokens
        .into_iter()
        .flat_map(|token| {
            if token.token_type == operator_type {
                split_raw_token_around_offsets(token, &generic_angle_offsets)
            } else {
                vec![token]
            }
        })
        .collect();
    if bracket_coloring != BracketColoringMode::VsCode {
        let mut owner_types = BTreeMap::new();
        for token in &tokens {
            let entry = owner_types
                .entry((token.span.start, token.span.end))
                .or_insert((token.priority, token.token_type));
            if token.priority > entry.0 {
                *entry = (token.priority, token.token_type);
            }
        }
        for (index, delimiter) in delimiters.into_iter().enumerate() {
            if index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
                return None;
            }
            let token_type = if bracket_coloring == BracketColoringMode::Punctuation
                || delimiter.anchor_kind
                    == super::scope_delimiters::ScopeDelimiterAnchorKind::Punctuation
            {
                semantic_type_index("reforgerPunctuation")
            } else if let Some((_, token_type)) =
                owner_types.get(&(delimiter.anchor.start, delimiter.anchor.end))
            {
                *token_type
            } else {
                continue;
            };
            for span in [Some(delimiter.opener), delimiter.closer]
                .into_iter()
                .flatten()
            {
                push_raw_semantic_token(
                    &mut tokens,
                    RawSemanticToken {
                        span,
                        token_type,
                        modifiers: 0,
                        priority: 90,
                    },
                );
            }
        }
    }
    let delimiter_overlay_elapsed = delimiter_overlay_start.elapsed();

    let sort_filter_split_start = Instant::now();
    tokens.sort_by_key(|token| {
        (
            token.span.start,
            std::cmp::Reverse(token.priority),
            std::cmp::Reverse(token.span.len()),
        )
    });

    let mut filtered = Vec::new();
    for token in tokens {
        if filtered.len() % 1024 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel())
        {
            return None;
        }
        if filtered
            .last()
            .is_some_and(|last: &RawSemanticToken| token.span.start < last.span.end)
        {
            continue;
        }
        filtered.push(token);
    }
    filtered.sort_by_key(|token| (token.span.start, token.span.end));
    let tokens = split_multiline_semantic_tokens(source, filtered, should_cancel)?
        .into_iter()
        .take(MAX_RAW_SEMANTIC_TOKENS)
        .collect();
    Some(RawSemanticTokenProjection {
        tokens,
        timings: LspSemanticTokenTimings {
            lex_ms: lex_elapsed.as_millis(),
            token_loop_ms: token_loop_elapsed.as_millis(),
            resolver_ms: resolver_elapsed.as_millis(),
            resolver_context_ms: resolver_timings.context.as_millis(),
            resolver_declaration_ms: resolver_timings.declaration.as_millis(),
            resolver_scope_ms: resolver_timings.scope.as_millis(),
            resolver_member_ms: resolver_timings.member.as_millis(),
            resolver_top_level_ms: resolver_timings.top_level.as_millis(),
            resolver_external_ms: resolver_timings.external.as_millis(),
            resolver_selection_ms: resolver_timings.selection.as_millis(),
            declaration_overlay_ms: declaration_overlay_elapsed.as_millis()
                + delimiter_overlay_elapsed.as_millis(),
            symbol_declaration_overlay_ms: declaration_overlay_elapsed.as_millis(),
            delimiter_overlay_ms: delimiter_overlay_elapsed.as_millis(),
            sort_filter_split_ms: sort_filter_split_start.elapsed().as_millis(),
            encode_ms: 0,
            decode_debug_ms: 0,
            identifier_resolver_calls,
            identifier_results_reused,
            identifier_regions_reused,
            identifier_regions_invalidated,
            identifier_regions_recomputed,
            delimiter_resolver_calls,
            delimiter_owners_reused,
            delimiter_owners_invalidated,
            delimiter_owners_recomputed,
        },
        delimiter_owner_cache,
        identifier_cache: cache_context.map(|_| IdentifierProjectionCache {
            source: Arc::from(source),
            structure: identifier_structure.unwrap_or_default(),
            regions: identifier_regions
                .into_iter()
                .zip(identifier_region_results)
                .map(|(span, results)| CachedIdentifierRegion { span, results })
                .collect(),
        }),
    })
}

fn identifier_reuse_regions(source: &str, index: &SymbolIndex) -> Vec<TextSpan> {
    let mut regions = index
        .symbols()
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Constructor
                    | SymbolKind::Destructor
            ) && !symbol.span.is_empty()
                && symbol.span.end <= source.len()
        })
        .map(|symbol| symbol.span)
        .collect::<Vec<_>>();
    regions.sort_by_key(|span| (span.start, span.end));
    regions
}

fn reusable_identifier_regions<'cache>(
    source: &str,
    context: RichSemanticProjectionCacheContext<'cache>,
    structure: &[super::scope_delimiters::SemanticStructureFact],
    regions: &[TextSpan],
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<Vec<Option<&'cache CachedIdentifierRegion>>> {
    let Some(cache) = context.previous_cache.filter(|cache| {
        cache.revision <= context.revision
            && cache.external_generation == context.external_generation
            && cache.identifier_cache.structure == structure
            && cache.identifier_cache.regions.len() == regions.len()
    }) else {
        return Some(vec![None; regions.len()]);
    };
    let mut reusable = Vec::with_capacity(regions.len());
    for (index, current_span) in regions.iter().copied().enumerate() {
        if index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        let cached = cache.identifier_cache.regions.get(index).filter(|cached| {
            span_text(source, current_span)
                == span_text(&cache.identifier_cache.source, cached.span)
        });
        reusable.push(cached);
    }
    Some(reusable)
}

fn identifier_region_index(regions: &[TextSpan], token_span: TextSpan) -> Option<usize> {
    let insertion = regions.partition_point(|region| region.start <= token_span.start);
    insertion
        .checked_sub(1)
        .filter(|index| token_span.end <= regions[*index].end)
}

fn relative_span(span: TextSpan, region: TextSpan) -> Option<TextSpan> {
    (region.start <= span.start && span.end <= region.end).then(|| {
        TextSpan::new(
            span.start.saturating_sub(region.start),
            span.end.saturating_sub(region.start),
        )
    })
}

fn cached_identifier_result(
    region: &CachedIdentifierRegion,
    relative_span: TextSpan,
) -> Option<CachedIdentifierResult> {
    region
        .results
        .binary_search_by_key(&(relative_span.start, relative_span.end), |result| {
            (result.relative_span.start, result.relative_span.end)
        })
        .ok()
        .and_then(|index| region.results.get(index))
        .copied()
}

fn lexical_raw_tokens(
    source: &str,
    lexer_tokens: &[Token],
    lex_elapsed: Duration,
    bracket_coloring: BracketColoringMode,
    generic_angle_offsets: &BTreeSet<usize>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<RawSemanticTokenProjection> {
    if should_cancel.is_some_and(|should_cancel| should_cancel()) {
        return None;
    }

    let mut tokens = Vec::new();
    let preprocessor_lines = preprocessor_line_spans(source);
    let mut preprocessor_line_index = 0usize;
    let token_loop_start = Instant::now();

    for (token_index, token) in lexer_tokens.iter().enumerate() {
        if token_index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        if token.kind == TokenKind::Whitespace || token.kind == TokenKind::Eof {
            continue;
        }
        if token_is_on_preprocessor_line(*token, &preprocessor_lines, &mut preprocessor_line_index)
        {
            if let Some(token_type) = preprocessor_line_semantic_type(source, *token) {
                push_raw_semantic_token(&mut tokens, raw_semantic(*token, token_type, 0, 20));
            }
            continue;
        }
        if bracket_coloring == BracketColoringMode::VsCode
            && is_standard_bracket_token_kind(token.kind)
        {
            continue;
        }
        let generic_angles = generic_angle_offsets
            .range(token.span.start..token.span.end)
            .copied()
            .collect::<Vec<_>>();
        if bracket_coloring != BracketColoringMode::Semantic && !generic_angles.is_empty() {
            let operator_type = semantic_type_index("operator");
            if lexical_semantic_type(token.kind) == Some(operator_type) {
                for residual in split_raw_token_around_offsets(
                    raw_semantic(*token, operator_type, 0, 10),
                    generic_angle_offsets,
                ) {
                    push_raw_semantic_token(&mut tokens, residual);
                }
            }
            if bracket_coloring == BracketColoringMode::Punctuation {
                for offset in generic_angles {
                    push_raw_semantic_token(
                        &mut tokens,
                        RawSemanticToken {
                            span: TextSpan::new(offset, offset + 1),
                            token_type: semantic_type_index("reforgerPunctuation"),
                            modifiers: 0,
                            priority: 10,
                        },
                    );
                }
            }
            continue;
        }
        if let Some(token_type) = lexical_semantic_type(token.kind) {
            let priority = if is_comment_token_kind(token.kind) {
                200
            } else {
                10
            };
            push_raw_semantic_token(&mut tokens, raw_semantic(*token, token_type, 0, priority));
        }
    }

    let sort_filter_split_start = Instant::now();
    tokens.sort_by_key(|token| {
        (
            token.span.start,
            std::cmp::Reverse(token.priority),
            std::cmp::Reverse(token.span.len()),
        )
    });
    let mut filtered = Vec::new();
    for token in tokens {
        if filtered.len() % 1024 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel())
        {
            return None;
        }
        if filtered
            .last()
            .is_some_and(|last: &RawSemanticToken| token.span.start < last.span.end)
        {
            continue;
        }
        filtered.push(token);
    }
    filtered.sort_by_key(|token| (token.span.start, token.span.end));
    let tokens = split_multiline_semantic_tokens(source, filtered, should_cancel)?
        .into_iter()
        .take(MAX_RAW_SEMANTIC_TOKENS)
        .collect();
    Some(RawSemanticTokenProjection {
        tokens,
        timings: LspSemanticTokenTimings {
            lex_ms: lex_elapsed.as_millis(),
            token_loop_ms: token_loop_start.elapsed().as_millis(),
            resolver_ms: 0,
            resolver_context_ms: 0,
            resolver_declaration_ms: 0,
            resolver_scope_ms: 0,
            resolver_member_ms: 0,
            resolver_top_level_ms: 0,
            resolver_external_ms: 0,
            resolver_selection_ms: 0,
            declaration_overlay_ms: 0,
            symbol_declaration_overlay_ms: 0,
            delimiter_overlay_ms: 0,
            sort_filter_split_ms: sort_filter_split_start.elapsed().as_millis(),
            encode_ms: 0,
            decode_debug_ms: 0,
            identifier_resolver_calls: 0,
            identifier_results_reused: 0,
            identifier_regions_reused: 0,
            identifier_regions_invalidated: 0,
            identifier_regions_recomputed: 0,
            delimiter_resolver_calls: 0,
            delimiter_owners_reused: 0,
            delimiter_owners_invalidated: 0,
            delimiter_owners_recomputed: 0,
        },
        delimiter_owner_cache: None,
        identifier_cache: None,
    })
}

pub(crate) fn generic_angle_offsets_for_delimiters(
    source: &str,
    delimiters: &[super::scope_delimiters::ScopeDelimiter],
) -> BTreeSet<usize> {
    delimiters
        .iter()
        .flat_map(|delimiter| [Some(delimiter.opener), delimiter.closer])
        .flatten()
        .filter_map(|span| {
            source
                .as_bytes()
                .get(span.start)
                .is_some_and(|byte| matches!(byte, b'<' | b'>'))
                .then_some(span.start)
        })
        .collect()
}

fn split_raw_token_around_offsets(
    token: RawSemanticToken,
    excluded_offsets: &BTreeSet<usize>,
) -> Vec<RawSemanticToken> {
    let mut residual = Vec::new();
    let mut start = token.span.start;
    for offset in excluded_offsets
        .range(token.span.start..token.span.end)
        .copied()
    {
        if start < offset {
            residual.push(RawSemanticToken {
                span: TextSpan::new(start, offset),
                ..token
            });
        }
        start = offset + 1;
    }
    if start < token.span.end {
        residual.push(RawSemanticToken {
            span: TextSpan::new(start, token.span.end),
            ..token
        });
    }
    residual
}

fn is_standard_bracket_token_kind(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::LeftBrace
            | TokenKind::RightBrace
            | TokenKind::LeftParen
            | TokenKind::RightParen
            | TokenKind::LeftBracket
            | TokenKind::RightBracket
    )
}

fn raw_semantic(token: Token, token_type: u32, modifiers: u32, priority: u8) -> RawSemanticToken {
    RawSemanticToken {
        span: token.span,
        token_type,
        modifiers,
        priority,
    }
}

fn push_raw_semantic_token(tokens: &mut Vec<RawSemanticToken>, token: RawSemanticToken) {
    if tokens.len() < MAX_RAW_SEMANTIC_TOKENS {
        tokens.push(token);
    }
}

fn lexical_semantic_type(kind: TokenKind) -> Option<u32> {
    match kind {
        TokenKind::LineComment
        | TokenKind::DocLineComment
        | TokenKind::BlockComment
        | TokenKind::DocBlockComment
        | TokenKind::UnterminatedBlockComment => Some(semantic_type_index("comment")),
        TokenKind::String | TokenKind::UnterminatedString => Some(semantic_type_index("string")),
        TokenKind::Number | TokenKind::InvalidNumber => Some(semantic_type_index("number")),
        TokenKind::Keyword(keyword) if keyword.is_class_like_type() => {
            Some(semantic_type_index("class"))
        }
        TokenKind::Keyword(_) => Some(semantic_type_index("keyword")),
        TokenKind::Operator(_) => Some(semantic_type_index("operator")),
        TokenKind::LeftBrace
        | TokenKind::RightBrace
        | TokenKind::LeftParen
        | TokenKind::RightParen
        | TokenKind::LeftBracket
        | TokenKind::RightBracket
        | TokenKind::Semicolon
        | TokenKind::Colon
        | TokenKind::Comma
        | TokenKind::Dot
        | TokenKind::Question => Some(semantic_type_index("reforgerPunctuation")),
        TokenKind::Hash => Some(semantic_type_index("reforgerPreprocessor")),
        _ => None,
    }
}

fn is_comment_token_kind(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::LineComment
            | TokenKind::DocLineComment
            | TokenKind::BlockComment
            | TokenKind::DocBlockComment
            | TokenKind::UnterminatedBlockComment
    )
}

fn symbol_semantic_type(kind: SymbolKind) -> Option<u32> {
    match kind {
        SymbolKind::Class => Some(semantic_type_index("class")),
        SymbolKind::TypeParameter => Some(semantic_type_index("typeParameter")),
        SymbolKind::Enum => Some(semantic_type_index("enum")),
        SymbolKind::EnumMember => Some(semantic_type_index("enumMember")),
        SymbolKind::Typedef => Some(semantic_type_index("type")),
        SymbolKind::Function => Some(semantic_type_index("function")),
        SymbolKind::GlobalField | SymbolKind::Field => Some(semantic_type_index("reforgerField")),
        SymbolKind::Constructor | SymbolKind::Destructor => Some(semantic_type_index("class")),
        SymbolKind::Method => Some(semantic_type_index("function")),
        SymbolKind::Parameter => Some(semantic_type_index("parameter")),
        SymbolKind::LocalVariable | SymbolKind::PreprocessorMacro => {
            Some(semantic_type_index("variable"))
        }
    }
}

fn candidate_semantic_type(
    candidate: &ReferenceCandidate,
    file_index: &SymbolIndex,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> Option<u32> {
    if matches!(candidate.kind, SymbolKind::GlobalField | SymbolKind::Field)
        && candidate.reason == ResolutionReason::StaticMember
    {
        let index = match candidate.source {
            CandidateSource::FileLocal => Some(file_index),
            CandidateSource::External => {
                ExternalIndexes::new(workspace_index, game_data_index).for_candidate(candidate)
            }
        };
        if index
            .and_then(|index| index.symbol(candidate.id))
            .is_some_and(is_static_const_symbol)
        {
            return Some(semantic_type_index("enumMember"));
        }
    }

    symbol_semantic_type(candidate.kind)
}

fn builtin_collection_type_semantic_type(
    resolution: &crate::resolver::ReferenceResolution,
) -> Option<u32> {
    if !matches!(
        resolution.identifier_context,
        IdentifierContext::TypePosition
            | IdentifierContext::ConstructedType
            | IdentifierContext::AttributeType
    ) {
        return None;
    }
    matches!(resolution.token_text.as_str(), "array" | "set" | "map")
        .then_some(semantic_type_index("class"))
}

fn is_static_const_symbol(symbol: &crate::index::IndexedSymbol) -> bool {
    let has_static = symbol.modifiers.iter().any(|modifier| modifier == "static");
    let has_const = symbol.modifiers.iter().any(|modifier| modifier == "const");
    has_static && has_const
}

fn resolver_reference_priority(kind: SymbolKind) -> u8 {
    if matches!(
        kind,
        SymbolKind::Class | SymbolKind::Enum | SymbolKind::Typedef | SymbolKind::TypeParameter
    ) {
        RESOLVER_TYPE_REFERENCE_PRIORITY
    } else {
        RESOLVER_REFERENCE_PRIORITY
    }
}

fn symbol_semantic_modifiers(symbol: &crate::index::IndexedSymbol) -> u32 {
    let mut modifiers = SEMANTIC_MOD_DECLARATION;
    for modifier in &symbol.modifiers {
        match modifier.as_str() {
            "static" => modifiers |= SEMANTIC_MOD_STATIC,
            "const" => modifiers |= SEMANTIC_MOD_READONLY,
            "modded" | "override" | "vanilla" => modifiers |= SEMANTIC_MOD_MODIFICATION,
            _ => {}
        }
    }
    modifiers
}

fn split_multiline_semantic_tokens(
    source: &str,
    tokens: Vec<RawSemanticToken>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<Vec<RawSemanticToken>> {
    let mut result = Vec::new();
    for (token_index, token) in tokens.into_iter().enumerate() {
        if token_index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        let mut segment_start = token.span.start;
        let mut segment_index = 0usize;
        while segment_start < token.span.end {
            if segment_index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel())
            {
                return None;
            }
            segment_index += 1;
            let line_end = source[segment_start..token.span.end]
                .find(['\r', '\n'])
                .map(|offset| segment_start + offset)
                .unwrap_or(token.span.end);
            if segment_start < line_end {
                push_raw_semantic_token(
                    &mut result,
                    RawSemanticToken {
                        span: TextSpan::new(segment_start, line_end),
                        ..token
                    },
                );
            }
            if line_end == token.span.end {
                break;
            }
            segment_start = line_end
                + if source.as_bytes()[line_end] == b'\r'
                    && source.as_bytes().get(line_end + 1) == Some(&b'\n')
                {
                    2
                } else {
                    1
                };
        }
    }
    Some(result)
}

fn encode_semantic_tokens(
    source: &str,
    tokens: &[RawSemanticToken],
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<Vec<u32>> {
    let encoded_capacity = tokens.len().min(MAX_RAW_SEMANTIC_TOKENS).saturating_mul(5);
    let mut data = Vec::with_capacity(encoded_capacity);
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;
    let line_index = LspPositionIndex::new_cancellable(source, should_cancel)?;

    for (token_index, token) in tokens.iter().take(MAX_RAW_SEMANTIC_TOKENS).enumerate() {
        if token_index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        let start = line_index.position_for_offset(token.span.start);
        let end = line_index.position_for_offset(token.span.end);
        if start.line != end.line || end.character <= start.character {
            continue;
        }
        let delta_line = start.line.saturating_sub(previous_line);
        let delta_start = if delta_line == 0 {
            start.character.saturating_sub(previous_start)
        } else {
            start.character
        };
        data.extend([
            delta_line,
            delta_start,
            end.character - start.character,
            token.token_type,
            token.modifiers,
        ]);
        previous_line = start.line;
        previous_start = start.character;
    }

    Some(data)
}

fn semantic_type_index(name: &str) -> u32 {
    SEMANTIC_TOKEN_TYPES
        .iter()
        .position(|token_type| *token_type == name)
        .expect("semantic token type should be in legend") as u32
}

fn semantic_token_type_name(index: u32) -> &'static str {
    SEMANTIC_TOKEN_TYPES
        .get(index as usize)
        .copied()
        .unwrap_or("<unknown>")
}

fn semantic_modifier_names(modifiers: u32) -> Vec<&'static str> {
    SEMANTIC_TOKEN_MODIFIERS
        .iter()
        .enumerate()
        .filter_map(|(index, name)| ((modifiers & (1 << index)) != 0).then_some(*name))
        .collect()
}

fn preprocessor_line_spans(source: &str) -> Vec<TextSpan> {
    let mut spans = Vec::new();
    let mut line_start = 0usize;
    for line in source.split_inclusive('\n') {
        let line_end = line_start + line.len();
        if line.trim_start().starts_with('#') {
            spans.push(TextSpan::new(line_start, line_end));
        }
        line_start = line_end;
    }
    spans
}

fn token_is_on_preprocessor_line(token: Token, lines: &[TextSpan], line_index: &mut usize) -> bool {
    while lines
        .get(*line_index)
        .is_some_and(|line| line.end <= token.span.start)
    {
        *line_index += 1;
    }
    lines
        .get(*line_index)
        .is_some_and(|line| line.start <= token.span.start && token.span.start < line.end)
}

fn preprocessor_line_semantic_type(source: &str, token: Token) -> Option<u32> {
    if token.kind == TokenKind::Hash || is_preprocessor_directive_token(source, token) {
        return Some(semantic_type_index("reforgerPreprocessor"));
    }
    if token.kind == TokenKind::Identifier {
        return Some(semantic_type_index("variable"));
    }
    lexical_semantic_type(token.kind)
}

fn is_preprocessor_directive_token(source: &str, token: Token) -> bool {
    if token.kind != TokenKind::Identifier {
        return false;
    }
    let line_start = source[..token.span.start]
        .rfind(['\r', '\n'])
        .map(|index| index + 1)
        .unwrap_or(0);
    let before_token = &source[line_start..token.span.start];
    if !before_token.trim_start().starts_with('#') {
        return false;
    }
    let after_hash = before_token
        .trim_start()
        .strip_prefix('#')
        .unwrap_or_default();
    if !after_hash.trim().is_empty() {
        return false;
    }
    matches!(
        &source[token.span.start..token.span.end],
        "if" | "ifdef" | "ifndef" | "elif" | "else" | "endif" | "define" | "undef" | "include"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_source;
    use std::cell::Cell;

    #[test]
    fn lexical_projection_colours_current_malformed_source_without_analysis() {
        let source = "class Broken {\n// current edit";

        let projection = lexical_semantic_tokens_for_source(source);

        assert_eq!(projection.parse_diagnostics, 0);
        assert_eq!(projection.timings.identifier_resolver_calls, 0);
        assert_eq!(projection.tokens.data.len() % 5, 0);
        assert!(projection.token_count >= 3);
    }

    #[test]
    fn incremental_projection_reuses_exact_unchanged_callable_identifiers() {
        let original = "void Known() {}\nclass Example\n{\n\tvoid First() { Known(); }\n\tvoid Second() { Known(); }\n}\n";
        let edited = "void Known() {}\nclass Example\n{\n\tvoid First() { Known(); }\n\tvoid Second() { int value = 1; Known(); }\n}\n";
        let original_analysis = file_index_for_source(original);
        let initial =
            semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled(
                original,
                &original_analysis,
                None,
                None,
                BracketColoringMode::Semantic,
                RichSemanticProjectionCacheContext::new(1, 7, None),
                &|| false,
            )
            .expect("initial projection");
        let edited_analysis = file_index_for_source(edited);
        let incremental =
            semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled(
                edited,
                &edited_analysis,
                None,
                None,
                BracketColoringMode::Semantic,
                RichSemanticProjectionCacheContext::new(2, 7, Some(&initial.cache)),
                &|| false,
            )
            .expect("edited projection");
        let full = semantic_tokens_for_cached_analysis_with_external_indexes(
            edited,
            &edited_analysis,
            None,
            None,
        );

        assert_eq!(incremental.projection.tokens, full.tokens);
        assert_eq!(incremental.projection.timings.identifier_results_reused, 1);
        assert_eq!(incremental.projection.timings.identifier_resolver_calls, 1);
        assert_eq!(incremental.projection.timings.identifier_regions_reused, 2);
        assert_eq!(
            incremental
                .projection
                .timings
                .identifier_regions_invalidated,
            1
        );
    }

    #[test]
    fn incremental_projection_invalidates_identifier_regions_for_external_changes() {
        let source =
            "void Known() {}\nclass Example { void First() { Known(); } void Second() { Known(); } }\n";
        let analysis = file_index_for_source(source);
        let initial =
            semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled(
                source,
                &analysis,
                None,
                None,
                BracketColoringMode::Semantic,
                RichSemanticProjectionCacheContext::new(1, 7, None),
                &|| false,
            )
            .expect("initial projection");
        let changed_generation =
            semantic_tokens_for_cached_analysis_with_external_indexes_incremental_cancelled(
                source,
                &analysis,
                None,
                None,
                BracketColoringMode::Semantic,
                RichSemanticProjectionCacheContext::new(1, 8, Some(&initial.cache)),
                &|| false,
            )
            .expect("generation projection");

        assert_eq!(
            changed_generation
                .projection
                .timings
                .identifier_results_reused,
            0
        );
        assert_eq!(
            changed_generation
                .projection
                .timings
                .identifier_regions_reused,
            0
        );
        assert_eq!(
            changed_generation
                .projection
                .timings
                .identifier_regions_invalidated,
            3
        );
        assert_eq!(
            changed_generation
                .projection
                .timings
                .identifier_resolver_calls,
            2
        );
    }

    #[test]
    fn lexical_projection_encodes_utf16_positions_across_crlf_multiline_comments() {
        let source = "/* 😀\r\nstill";

        let projection = lexical_semantic_tokens_for_source(source);

        assert_eq!(
            projection.tokens.data,
            vec![
                0,
                0,
                5,
                semantic_type_index("comment"),
                0,
                1,
                0,
                5,
                semantic_type_index("comment"),
                0,
            ]
        );
    }

    #[test]
    fn unresolved_statement_identifier_keeps_the_default_editor_foreground() {
        let before_control_statement = r#"class Example
{
	void Run()
	{
		asdasdsadasd
		if (true)
			return;
	}
}
"#;
        let before_call_statement = r#"class Example
{
	void Run()
	{
		asdasdsadasd
		DoThing();
	}
}
"#;
        let before_postfix_statement = r#"class Example
{
	void Run()
	{
		int testnum = 5;
		asdasdsadasd // Still showing as class green

		testnum++;
	}
}
"#;

        for source in [
            before_control_statement,
            before_call_statement,
            before_postfix_statement,
        ] {
            let report = semantic_tokens_report_for_source(source);

            assert!(
                report
                    .decoded
                    .iter()
                    .all(|token| token.text != "asdasdsadasd"),
                "an unresolved statement identifier must not receive a semantic token"
            );
        }
    }

    #[test]
    fn unresolved_map_type_argument_keeps_the_default_editor_foreground() {
        let source = r#"class Example
{
	void Run()
	{
		map<int, sadasdasdasd> testmap = new map<int, int>();
	}
}
"#;
        let report = semantic_tokens_report_for_source(source);

        assert!(
            report
                .decoded
                .iter()
                .all(|token| token.text != "sadasdasdasd"),
            "an unresolved map type argument must not receive a semantic token"
        );
    }

    #[test]
    fn unresolved_identifier_shapes_keep_the_default_editor_foreground() {
        let source = r#"[MissingAttribute(MissingArgument: MissingAttributeValue, MissingAttributeOwner.MissingAttributeMember)]
class Example : MissingBase
{
	MissingField m_Field;

	MissingReturn Run(MissingParameter parameter)
	{
		MissingLocal local;
		array<MissingArrayItem> arrayValues;
		set<MissingSetItem> setValues;
		map<MissingMapKey, MissingMapValue> mapValues;
		MissingConstructed created = new MissingConstructed();
		MissingStaticOwner.MissingStaticMember;
		MissingCall();
		local.MissingMemberCall();
	}
}

typedef MissingAliasTarget MissingAlias;
"#;
        let expected_default = [
            "MissingAttribute",
            "MissingArgument",
            "MissingAttributeValue",
            "MissingAttributeOwner",
            "MissingAttributeMember",
            "MissingBase",
            "MissingField",
            "MissingReturn",
            "MissingParameter",
            "MissingLocal",
            "MissingArrayItem",
            "MissingSetItem",
            "MissingMapKey",
            "MissingMapValue",
            "MissingConstructed",
            "MissingStaticOwner",
            "MissingStaticMember",
            "MissingCall",
            "MissingMemberCall",
            "MissingAliasTarget",
        ];
        let report = semantic_tokens_report_for_source(source);
        let wrongly_classified = report
            .decoded
            .iter()
            .filter(|token| expected_default.contains(&token.text.as_str()))
            .collect::<Vec<_>>();

        assert!(
            wrongly_classified.is_empty(),
            "unresolved identifier shapes must not receive semantic tokens: {wrongly_classified:#?}"
        );
    }

    #[test]
    fn value_only_symbol_in_a_type_position_keeps_the_default_editor_foreground() {
        let source = r#"class Example
{
	int ValueOnly;

	void Run()
	{
		ValueOnly invalidTypeUse;
	}
}
"#;
        let report = semantic_tokens_report_for_source(source);
        let classifications = report
            .decoded
            .iter()
            .filter(|token| token.text == "ValueOnly")
            .collect::<Vec<_>>();

        assert_eq!(
            classifications.len(),
            1,
            "only the actual field declaration should be classified: {classifications:#?}"
        );
        assert_eq!(classifications[0].token_type, "reforgerField");
        assert!(classifications[0].modifiers.contains(&"declaration"));
    }

    #[test]
    fn local_and_parameter_collisions_in_type_positions_remain_unclassified() {
        let source = r#"class Example
{
	void Run(int CollisionParameter)
	{
		int CollisionLocal;
		map<CollisionParameter, CollisionLocal> invalidTypes;
	}
}
"#;
        let report = semantic_tokens_report_for_source(source);

        for (name, expected_type) in [
            ("CollisionParameter", "parameter"),
            ("CollisionLocal", "variable"),
        ] {
            let classifications = report
                .decoded
                .iter()
                .filter(|token| token.text == name)
                .collect::<Vec<_>>();
            assert_eq!(
                classifications.len(),
                1,
                "only the value declaration should be classified for {name}: {classifications:#?}"
            );
            assert_eq!(classifications[0].token_type, expected_type);
            assert!(classifications[0].modifiers.contains(&"declaration"));
        }
    }

    #[test]
    fn incompatible_same_name_symbols_do_not_prove_syntax_roles() {
        let source = r#"void WrongAttribute();
void WrongStaticOwner();

class Owner
{
	int WrongMemberCall;
}

[WrongAttribute()]
class Example
{
	int WrongCall;
	int WrongConstructed;

	void Run(Owner owner)
	{
		WrongCall();
		WrongConstructed invalidType = new WrongConstructed();
		WrongStaticOwner.Value;
		owner.WrongMemberCall();
	}
}
"#;
        let report = semantic_tokens_report_for_source(source);

        for name in [
            "WrongAttribute",
            "WrongStaticOwner",
            "WrongMemberCall",
            "WrongCall",
            "WrongConstructed",
        ] {
            let classifications = report
                .decoded
                .iter()
                .filter(|token| token.text == name)
                .collect::<Vec<_>>();
            assert_eq!(
                classifications.len(),
                1,
                "only the incompatible symbol's declaration should be classified for {name}: {classifications:#?}"
            );
            assert!(classifications[0].modifiers.contains(&"declaration"));
        }
    }

    #[test]
    fn lexical_bracket_modes_classify_only_parser_proven_generic_angles() {
        let source = "class Example { array<int> values; array<int>> broken; void Run() { bool less = 1 < 2; int shifted = 8 >> 1; } }";
        let generic_open = source.find("array<int>").unwrap() + "array".len();
        let generic_close = generic_open + "<int".len();
        let mixed_closers = source.find("array<int>> broken").unwrap() + "array<int".len();
        let comparison = source.find("1 < 2").unwrap() + "1 ".len();
        let shift = source.find("8 >> 1").unwrap() + "8 ".len();
        let lexer_tokens = lex(source);
        let generic_angle_offsets = generic_angle_offsets_for_delimiters(
            source,
            &super::super::scope_delimiters::scope_delimiters_for_syntax(
                &parse_source(source),
                &lexer_tokens,
            ),
        );

        let punctuation = lexical_semantic_tokens_for_source_with_bracket_coloring(
            source,
            BracketColoringMode::Punctuation,
            &generic_angle_offsets,
        );
        assert_eq!(
            lexical_token_type_at_offset(&punctuation, generic_open),
            Some(semantic_type_index("reforgerPunctuation")),
        );
        assert_eq!(
            lexical_token_type_at_offset(&punctuation, generic_close),
            Some(semantic_type_index("reforgerPunctuation")),
        );
        assert_eq!(
            lexical_token_type_at_offset(&punctuation, mixed_closers),
            Some(semantic_type_index("reforgerPunctuation")),
        );
        assert_eq!(
            lexical_token_type_at_offset(&punctuation, mixed_closers + 1),
            Some(semantic_type_index("operator")),
        );
        assert_eq!(
            lexical_token_type_at_offset(&punctuation, comparison),
            Some(semantic_type_index("operator")),
        );
        assert_eq!(
            lexical_token_type_at_offset(&punctuation, shift),
            Some(semantic_type_index("operator")),
        );

        let vscode = lexical_semantic_tokens_for_source_with_bracket_coloring(
            source,
            BracketColoringMode::VsCode,
            &generic_angle_offsets,
        );
        assert_eq!(lexical_token_type_at_offset(&vscode, generic_open), None);
        assert_eq!(lexical_token_type_at_offset(&vscode, generic_close), None);
        assert_eq!(lexical_token_type_at_offset(&vscode, mixed_closers), None);
        assert_eq!(
            lexical_token_type_at_offset(&vscode, mixed_closers + 1),
            Some(semantic_type_index("operator")),
        );
        assert_eq!(
            lexical_token_type_at_offset(&vscode, comparison),
            Some(semantic_type_index("operator")),
        );
        assert_eq!(
            lexical_token_type_at_offset(&vscode, shift),
            Some(semantic_type_index("operator")),
        );
    }

    fn lexical_token_type_at_offset(
        projection: &LspSemanticTokenProjection,
        offset: usize,
    ) -> Option<u32> {
        let mut line = 0usize;
        let mut character = 0usize;
        for token in projection.tokens.data.chunks_exact(5) {
            line += token[0] as usize;
            character = if token[0] == 0 {
                character + token[1] as usize
            } else {
                token[1] as usize
            };
            if line == 0 && character <= offset && offset < character + token[2] as usize {
                return Some(token[3]);
            }
        }
        None
    }

    #[test]
    fn rich_resolution_reaches_resolved_identifiers_after_the_former_call_budget() {
        const REFERENCE_COUNT: usize = 128;
        let mut source = String::from("class Known {}\nclass Example\n{\n\tvoid Run()\n\t{\n");
        for index in 0..REFERENCE_COUNT {
            source.push_str(&format!("\t\tKnown value{index};\n"));
        }
        source.push_str("\t}\n}\n");

        let report = semantic_tokens_report_for_source(&source);
        let known_tokens = report
            .decoded
            .iter()
            .filter(|token| token.text == "Known" && token.token_type == "class")
            .count();

        assert_eq!(known_tokens, REFERENCE_COUNT + 1);
        assert!(report.timings.identifier_resolver_calls >= REFERENCE_COUNT);
    }

    #[test]
    fn preprocessor_line_projection_handles_indentation_crlf_and_final_lines() {
        let source = "  #define VALUE 1\r\nint value;\n\t#ifdef TEST\nVALUE\n#endif";
        let lines = preprocessor_line_spans(source);
        let mut line_index = 0usize;
        let classified = lex(source)
            .into_iter()
            .filter(|token| !matches!(token.kind, TokenKind::Whitespace | TokenKind::Eof))
            .map(|token| {
                (
                    span_text(source, token.span).to_string(),
                    token_is_on_preprocessor_line(token, &lines, &mut line_index),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            classified,
            vec![
                ("#".to_string(), true),
                ("define".to_string(), true),
                ("VALUE".to_string(), true),
                ("1".to_string(), true),
                ("int".to_string(), false),
                ("value".to_string(), false),
                (";".to_string(), false),
                ("#".to_string(), true),
                ("ifdef".to_string(), true),
                ("TEST".to_string(), true),
                ("VALUE".to_string(), false),
                ("#".to_string(), true),
                ("endif".to_string(), true),
            ]
        );
    }

    #[test]
    fn multiline_token_expansion_respects_the_final_output_cap() {
        let source = format!("/*\n{}", "x\n".repeat(MAX_RAW_SEMANTIC_TOKENS + 1));

        let projection = semantic_tokens_for_source_with_external(&source, None);

        assert_eq!(projection.tokens.data.len() % 5, 0);
        assert!(projection.tokens.data.len() / 5 <= MAX_RAW_SEMANTIC_TOKENS);
    }

    #[test]
    fn encoding_stops_when_cancellation_arrives_mid_projection() {
        let source = "value ".repeat(128);
        let tokens = (0..128)
            .map(|index| {
                let start = index * 6;
                RawSemanticToken {
                    span: TextSpan::new(start, start + 5),
                    token_type: semantic_type_index("variable"),
                    modifiers: 0,
                    priority: 0,
                }
            })
            .collect::<Vec<_>>();
        let checks = Cell::new(0usize);

        let result = encode_semantic_tokens(
            &source,
            &tokens,
            Some(&|| {
                checks.set(checks.get() + 1);
                checks.get() >= 2
            }),
        );

        assert!(result.is_none());
    }

    #[test]
    fn multiline_split_stops_within_one_large_token() {
        let source = format!("/*{}*/", "line\n".repeat(256));
        let token = RawSemanticToken {
            span: TextSpan::new(0, source.len()),
            token_type: semantic_type_index("comment"),
            modifiers: 0,
            priority: 0,
        };
        let checks = Cell::new(0usize);

        let result = split_multiline_semantic_tokens(
            &source,
            vec![token],
            Some(&|| {
                checks.set(checks.get() + 1);
                checks.get() >= 3
            }),
        );

        assert!(result.is_none());
    }
}
