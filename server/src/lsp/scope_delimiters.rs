use super::external_indexes::ExternalIndexes;
use super::FileIndexAnalysis;
use crate::index::{IndexedSymbol, SymbolIndex};
use crate::lexer::{Keyword, Operator, TextSpan, Token, TokenKind};
use crate::model::SymbolKind;
use crate::resolver::ReferenceResolver;
use crate::syntax::{Parse, SyntaxElement, SyntaxKind, SyntaxNode};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const MAX_ACTIVE_SCOPE_DELIMITER_SOURCE_BYTES: usize = 128 * 1024;
const MAX_SCOPE_DELIMITERS: usize = 200_000;
const MAX_RESOLVED_SCOPE_DELIMITER_OWNERS: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeDelimiterAnchorKind {
    SemanticToken,
    Punctuation,
    ResolvedCall,
    ResolvedConstructor,
    ResolvedIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScopeDelimiterAnchor {
    span: TextSpan,
    kind: ScopeDelimiterAnchorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeDelimiter {
    pub(crate) opener: TextSpan,
    pub(crate) closer: Option<TextSpan>,
    pub(crate) anchor: TextSpan,
    pub(crate) anchor_kind: ScopeDelimiterAnchorKind,
}

pub(crate) struct ScopeDelimiterProjection {
    pub(crate) delimiters: Vec<ScopeDelimiter>,
    pub(crate) dynamic_owner_resolver_calls: usize,
    pub(crate) dynamic_owners_reused: usize,
    pub(crate) dynamic_owners_invalidated: usize,
    pub(crate) dynamic_owners_recomputed: usize,
    pub(crate) owner_cache: Option<DelimiterOwnerProjectionCache>,
}

/// Previous-revision proof results for resolver-dependent delimiter owners.
///
/// Cached entries never carry current ranges into a new revision. The current
/// parse collects every delimiter again, then an exact callable-region match
/// may reuse only the boolean proof for the corresponding owner.
#[derive(Clone)]
pub(crate) struct DelimiterOwnerProjectionCache {
    revision: u64,
    external_generation: u64,
    source: Arc<str>,
    structure: Vec<SemanticStructureFact>,
    regions: Vec<CachedDelimiterRegion>,
    dynamic_owner_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SemanticStructureFact {
    parent_path: Vec<(SymbolKind, Option<String>)>,
    kind: SymbolKind,
    name: Option<String>,
    type_text: Option<String>,
    return_type_text: Option<String>,
    base_type: Option<String>,
    default_text: Option<String>,
    enum_value_text: Option<String>,
    modifiers: Vec<String>,
    callable_form: Option<&'static str>,
    conditional_context: Vec<(&'static str, Option<String>)>,
}

#[derive(Clone)]
struct CachedDelimiterRegion {
    identity: DelimiterRegionIdentity,
    span: TextSpan,
    dynamic_owner_proofs: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DelimiterRegionIdentity {
    kind: SyntaxKind,
    declaration_path: Vec<(SyntaxKind, String)>,
    header: String,
}

struct CurrentDelimiterRegion {
    identity: DelimiterRegionIdentity,
    span: TextSpan,
}

impl DelimiterOwnerProjectionCache {
    pub(crate) fn rebind_external_generation(
        &mut self,
        revision: u64,
        previous_generation: u64,
        external_generation: u64,
    ) -> bool {
        if self.revision != revision || self.external_generation != previous_generation {
            return false;
        }
        self.external_generation = external_generation;
        true
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DelimiterProjectionCacheContext<'cache> {
    revision: u64,
    external_generation: u64,
    previous_cache: Option<&'cache DelimiterOwnerProjectionCache>,
}

impl<'cache> DelimiterProjectionCacheContext<'cache> {
    pub(crate) fn new(
        revision: u64,
        external_generation: u64,
        previous_cache: Option<&'cache DelimiterOwnerProjectionCache>,
    ) -> Self {
        Self {
            revision,
            external_generation,
            previous_cache,
        }
    }
}

pub(crate) fn semantic_scope_delimiters_for_analysis(
    source: &str,
    analysis: &FileIndexAnalysis,
    external_indexes: ExternalIndexes<'_>,
    pre_resolved_identifier_kinds: &BTreeMap<(usize, usize), Option<SymbolKind>>,
    resolve_dynamic_owners: bool,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<ScopeDelimiterProjection> {
    semantic_scope_delimiters_for_analysis_internal(
        source,
        analysis,
        external_indexes,
        pre_resolved_identifier_kinds,
        resolve_dynamic_owners,
        None,
        should_cancel,
    )
}

pub(crate) fn semantic_scope_delimiters_for_analysis_incremental(
    source: &str,
    analysis: &FileIndexAnalysis,
    external_indexes: ExternalIndexes<'_>,
    pre_resolved_identifier_kinds: &BTreeMap<(usize, usize), Option<SymbolKind>>,
    resolve_dynamic_owners: bool,
    cache_context: DelimiterProjectionCacheContext<'_>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<ScopeDelimiterProjection> {
    semantic_scope_delimiters_for_analysis_internal(
        source,
        analysis,
        external_indexes,
        pre_resolved_identifier_kinds,
        resolve_dynamic_owners,
        Some(cache_context),
        should_cancel,
    )
}

fn semantic_scope_delimiters_for_analysis_internal(
    source: &str,
    analysis: &FileIndexAnalysis,
    external_indexes: ExternalIndexes<'_>,
    pre_resolved_identifier_kinds: &BTreeMap<(usize, usize), Option<SymbolKind>>,
    resolve_dynamic_owners: bool,
    cache_context: Option<DelimiterProjectionCacheContext<'_>>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<ScopeDelimiterProjection> {
    let mut delimiters = collect_scope_delimiters(
        &analysis.syntax.parse,
        &analysis.syntax.lexer_tokens,
        Some(&analysis.index),
        should_cancel,
    )?;
    let structure = if cache_context.is_some() {
        Some(semantic_structure_facts(&analysis.index, should_cancel)?)
    } else {
        None
    };
    let regions = if cache_context.is_some() {
        delimiter_reuse_regions(source, &analysis.syntax.parse, should_cancel)?
    } else {
        Vec::new()
    };
    if !resolve_dynamic_owners {
        delimiters.retain(delimiter_anchor_is_structurally_proven);
        let owner_cache = cache_context.map(|context| DelimiterOwnerProjectionCache {
            revision: context.revision,
            external_generation: context.external_generation,
            source: Arc::from(source),
            structure: structure.unwrap_or_default(),
            regions: regions
                .into_iter()
                .map(|region| CachedDelimiterRegion {
                    identity: region.identity,
                    span: region.span,
                    dynamic_owner_proofs: Vec::new(),
                })
                .collect(),
            dynamic_owner_count: 0,
        });
        return Some(ScopeDelimiterProjection {
            delimiters,
            dynamic_owner_resolver_calls: 0,
            dynamic_owners_reused: 0,
            dynamic_owners_invalidated: 0,
            dynamic_owners_recomputed: 0,
            owner_cache,
        });
    }
    let resolver = ReferenceResolver::new_with_parse_scope_and_external_indexes(
        source,
        &analysis.index,
        &analysis.syntax.parse,
        &analysis.scope,
        external_indexes.ordered(),
    );
    let reusable_regions = if let (Some(context), Some(structure)) = (cache_context, &structure) {
        reusable_delimiter_regions(
            source,
            context.revision,
            context.external_generation,
            structure,
            &regions,
            context.previous_cache,
            should_cancel,
        )?
    } else {
        vec![None; regions.len()]
    };
    let reused_owner_capacity = reusable_regions
        .iter()
        .flatten()
        .map(|region| region.dynamic_owner_proofs.len())
        .sum::<usize>();
    let invalidated_owners = cache_context
        .and_then(|context| context.previous_cache)
        .map_or(0, |cache| {
            cache
                .dynamic_owner_count
                .saturating_sub(reused_owner_capacity)
        });
    let mut region_proofs = regions
        .iter()
        .map(|_| Vec::<bool>::new())
        .collect::<Vec<_>>();
    let mut proven = Vec::with_capacity(delimiters.len());
    let mut dynamic_owner_attempts = 0usize;
    let mut dynamic_owner_resolver_calls = 0usize;
    let mut dynamic_owners_reused = 0usize;
    for (index, delimiter) in delimiters.into_iter().enumerate() {
        if index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        if delimiter_anchor_is_structurally_proven(&delimiter) {
            proven.push(delimiter);
            continue;
        }
        if dynamic_owner_attempts >= MAX_RESOLVED_SCOPE_DELIMITER_OWNERS {
            continue;
        }
        dynamic_owner_attempts += 1;
        let region_index = delimiter_region_index(&regions, delimiter.opener);
        let cached_proof = region_index.and_then(|region_index| {
            reusable_regions[region_index].and_then(|region| {
                region
                    .dynamic_owner_proofs
                    .get(region_proofs[region_index].len())
                    .copied()
            })
        });
        let is_proven = if let Some(is_proven) = cached_proof {
            dynamic_owners_reused += 1;
            is_proven
        } else if let Some(candidate_kind) =
            pre_resolved_identifier_kinds.get(&(delimiter.anchor.start, delimiter.anchor.end))
        {
            candidate_kind.is_some_and(|candidate_kind| {
                delimiter_anchor_kind_is_proven(&delimiter, candidate_kind)
            })
        } else {
            dynamic_owner_resolver_calls += 1;
            delimiter_anchor_is_proven(&delimiter, &analysis.syntax.lexer_tokens, &resolver)
        };
        if let Some(region_index) = region_index {
            region_proofs[region_index].push(is_proven);
        }
        if is_proven {
            proven.push(delimiter);
        }
    }
    if should_cancel.is_some_and(|should_cancel| should_cancel()) {
        return None;
    }
    let owner_cache = cache_context.map(|context| DelimiterOwnerProjectionCache {
        revision: context.revision,
        external_generation: context.external_generation,
        source: Arc::from(source),
        structure: structure.unwrap_or_default(),
        regions: regions
            .into_iter()
            .zip(region_proofs)
            .map(|(region, dynamic_owner_proofs)| CachedDelimiterRegion {
                identity: region.identity,
                span: region.span,
                dynamic_owner_proofs,
            })
            .collect(),
        dynamic_owner_count: dynamic_owner_attempts,
    });
    Some(ScopeDelimiterProjection {
        delimiters: proven,
        dynamic_owner_resolver_calls,
        dynamic_owners_reused,
        dynamic_owners_invalidated: invalidated_owners,
        dynamic_owners_recomputed: dynamic_owner_attempts.saturating_sub(dynamic_owners_reused),
        owner_cache,
    })
}

pub(crate) fn semantic_structure_facts(
    index: &SymbolIndex,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<Vec<SemanticStructureFact>> {
    // File-visible declarations can change resolution inside an otherwise
    // byte-identical callable, so they form one exact cache dependency.
    // Locals are excluded because their containing callable source already
    // invalidates that callable's owner proofs.
    let mut facts = Vec::new();
    for (symbol_index, symbol) in index.symbols().iter().enumerate() {
        if symbol_index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        if symbol.kind == SymbolKind::LocalVariable {
            continue;
        }
        facts.push(SemanticStructureFact {
            parent_path: semantic_symbol_parent_path(index, symbol),
            kind: symbol.kind,
            name: symbol.name.clone(),
            type_text: symbol.detail.type_text.clone(),
            return_type_text: symbol.detail.return_type_text.clone(),
            base_type: symbol.detail.base_type.clone(),
            default_text: symbol.detail.default_text.clone(),
            enum_value_text: symbol.detail.enum_value_text.clone(),
            modifiers: symbol.modifiers.clone(),
            callable_form: symbol.callable_form.map(|form| form.as_str()),
            conditional_context: symbol
                .conditional_context
                .iter()
                .map(|branch| (branch.kind.as_str(), branch.condition.clone()))
                .collect(),
        });
    }
    Some(facts)
}

pub(crate) fn semantic_symbol_parent_path(
    index: &SymbolIndex,
    symbol: &IndexedSymbol,
) -> Vec<(SymbolKind, Option<String>)> {
    let mut path = Vec::new();
    let mut parent = symbol.parent;
    while let Some(parent_id) = parent {
        let Some(parent_symbol) = index.symbol(parent_id) else {
            break;
        };
        path.push((parent_symbol.kind, parent_symbol.name.clone()));
        parent = parent_symbol.parent;
    }
    path.reverse();
    path
}

fn delimiter_reuse_regions(
    source: &str,
    parse: &Parse,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<Vec<CurrentDelimiterRegion>> {
    let mut regions = vec![CurrentDelimiterRegion {
        identity: DelimiterRegionIdentity {
            kind: SyntaxKind::SourceFile,
            declaration_path: Vec::new(),
            header: String::new(),
        },
        span: TextSpan::new(0, source.len()),
    }];
    collect_delimiter_reuse_regions(
        source,
        &parse.root,
        &mut Vec::new(),
        &mut regions,
        should_cancel,
        &mut 0,
    )?;
    Some(regions)
}

fn collect_delimiter_reuse_regions(
    source: &str,
    node: &SyntaxNode,
    declaration_path: &mut Vec<(SyntaxKind, String)>,
    regions: &mut Vec<CurrentDelimiterRegion>,
    should_cancel: Option<&dyn Fn() -> bool>,
    visited_nodes: &mut usize,
) -> Option<()> {
    *visited_nodes += 1;
    if *visited_nodes % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
        return None;
    }
    let declaration_name = match node.kind {
        SyntaxKind::ClassDecl => name_after_keyword(node, Keyword::Class),
        SyntaxKind::EnumDecl => name_after_keyword(node, Keyword::Enum),
        _ => None,
    }
    .and_then(|token| source.get(token.span.start..token.span.end))
    .map(str::to_string);
    let entered_declaration = declaration_name.is_some();
    if let Some(name) = declaration_name {
        declaration_path.push((node.kind, name));
    }

    if matches!(node.kind, SyntaxKind::FunctionDecl | SyntaxKind::MethodDecl) {
        let header_end = first_child(node, SyntaxKind::Block)
            .map_or(node.span.end, |body| body.span.start)
            .min(source.len());
        let header = source
            .get(node.span.start.min(header_end)..header_end)
            .unwrap_or_default()
            .to_string();
        regions.push(CurrentDelimiterRegion {
            identity: DelimiterRegionIdentity {
                kind: node.kind,
                declaration_path: declaration_path.clone(),
                header,
            },
            span: node.span,
        });
    } else {
        for child in &node.children {
            if let SyntaxElement::Node(child) = child {
                collect_delimiter_reuse_regions(
                    source,
                    child,
                    declaration_path,
                    regions,
                    should_cancel,
                    visited_nodes,
                )?;
            }
        }
    }

    if entered_declaration {
        declaration_path.pop();
    }
    Some(())
}

fn reusable_delimiter_regions<'cache>(
    source: &str,
    revision: u64,
    external_generation: u64,
    structure: &[SemanticStructureFact],
    regions: &[CurrentDelimiterRegion],
    previous_cache: Option<&'cache DelimiterOwnerProjectionCache>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<Vec<Option<&'cache CachedDelimiterRegion>>> {
    let Some(cache) = previous_cache.filter(|cache| {
        cache.revision <= revision
            && cache.external_generation == external_generation
            && cache.structure == structure
    }) else {
        return Some(vec![None; regions.len()]);
    };
    let mut current_identity_counts = BTreeMap::new();
    for (index, region) in regions.iter().enumerate() {
        if index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        *current_identity_counts
            .entry(&region.identity)
            .or_insert(0usize) += 1;
    }
    let mut cached_unique_regions = BTreeMap::new();
    for (index, region) in cache.regions.iter().enumerate() {
        if index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        cached_unique_regions
            .entry(&region.identity)
            .and_modify(|unique_index| *unique_index = None)
            .or_insert(Some(index));
    }
    let mut reusable = Vec::with_capacity(regions.len());
    for (index, region) in regions.iter().enumerate() {
        if index % 64 == 0 && should_cancel.is_some_and(|should_cancel| should_cancel()) {
            return None;
        }
        let cached = (|| {
            // Duplicate declaration paths and headers are valid in conditional
            // or modded source. Their semantic contexts may differ, so an
            // ambiguous syntax identity is never reusable even when each body
            // has a distinct exact-source match.
            if current_identity_counts.get(&region.identity) != Some(&1) {
                return None;
            }
            let current_text = span_text(source, region.span)?;
            let cached_index = cached_unique_regions
                .get(&region.identity)
                .copied()
                .flatten()?;
            let cached = cache.regions.get(cached_index)?;
            (span_text(&cache.source, cached.span) == Some(current_text)).then_some(cached)
        })();
        reusable.push(cached);
    }
    Some(reusable)
}

fn delimiter_region_index(
    regions: &[CurrentDelimiterRegion],
    delimiter_span: TextSpan,
) -> Option<usize> {
    // Region zero is the file root; callable regions are collected in source
    // order and never nest because collection stops at a callable boundary.
    let callable_regions = regions.get(1..).unwrap_or_default();
    let insertion =
        callable_regions.partition_point(|region| region.span.start <= delimiter_span.start);
    if let Some((index, region)) = insertion.checked_sub(1).and_then(|index| {
        callable_regions
            .get(index)
            .map(|region| (index + 1, region))
    }) {
        if delimiter_span.end <= region.span.end {
            return Some(index);
        }
    }
    regions.first().and_then(|root| {
        (root.span.start <= delimiter_span.start && delimiter_span.end <= root.span.end)
            .then_some(0)
    })
}

fn span_text(source: &str, span: TextSpan) -> Option<&str> {
    source.get(span.start..span.end)
}

pub(crate) fn scope_delimiters_for_syntax(
    parse: &Parse,
    lexer_tokens: &[Token],
) -> Vec<ScopeDelimiter> {
    collect_scope_delimiters(parse, lexer_tokens, None, None).unwrap_or_default()
}

fn collect_scope_delimiters(
    parse: &Parse,
    lexer_tokens: &[Token],
    index: Option<&SymbolIndex>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Option<Vec<ScopeDelimiter>> {
    let mut collector = DelimiterCollector {
        lexer_tokens,
        index,
        delimiters: BTreeMap::new(),
        should_cancel,
        visited_nodes: 0,
    };
    collector.collect_node(&parse.root, None)?;
    collector.collect_indexed_type_angles()?;
    Some(collector.delimiters.into_values().collect())
}

pub(crate) fn active_scope_delimiters(
    delimiters: &[ScopeDelimiter],
    offsets: &[usize],
) -> Vec<ScopeDelimiter> {
    let mut active = BTreeMap::new();
    for offset in offsets {
        let selected = delimiters
            .iter()
            .filter(|delimiter| {
                delimiter.closer.is_some_and(|closer| {
                    delimiter.opener.end <= *offset && *offset <= closer.start
                })
            })
            .min_by_key(|delimiter| {
                let closer = delimiter.closer.expect("matched delimiter");
                (
                    closer.end - delimiter.opener.start,
                    usize::MAX - delimiter.opener.start,
                )
            });
        if let Some(delimiter) = selected {
            active.insert(
                (
                    delimiter.opener.start,
                    delimiter.closer.expect("active delimiter is matched").start,
                ),
                *delimiter,
            );
        }
    }
    active.into_values().collect()
}

struct DelimiterCollector<'analysis> {
    lexer_tokens: &'analysis [Token],
    index: Option<&'analysis SymbolIndex>,
    delimiters: BTreeMap<usize, ScopeDelimiter>,
    should_cancel: Option<&'analysis dyn Fn() -> bool>,
    visited_nodes: usize,
}

impl DelimiterCollector<'_> {
    fn collect_node(
        &mut self,
        node: &SyntaxNode,
        inherited_anchor: Option<ScopeDelimiterAnchor>,
    ) -> Option<()> {
        if self.visited_nodes % 64 == 0
            && self
                .should_cancel
                .is_some_and(|should_cancel| should_cancel())
        {
            return None;
        }
        self.visited_nodes += 1;
        if node.kind == SyntaxKind::PreprocessorDirective {
            return Some(());
        }

        let anchor = self.node_anchor(node).or(inherited_anchor);
        if standard_delimiter_node(node.kind) {
            let tokens = direct_tokens(node);
            self.collect_standard_pairs(node, &tokens, anchor);
        }
        if matches!(node.kind, SyntaxKind::GenericArgList | SyntaxKind::TypeRef) {
            let tokens = direct_tokens(node);
            self.collect_angle_pairs(&tokens, anchor);
        }

        for child in &node.children {
            if let SyntaxElement::Node(child) = child {
                self.collect_node(child, anchor)?;
            }
        }
        Some(())
    }

    fn node_anchor(&self, node: &SyntaxNode) -> Option<ScopeDelimiterAnchor> {
        let (span, kind) = match node.kind {
            SyntaxKind::ClassDecl => (
                name_after_keyword(node, Keyword::Class)?.span,
                ScopeDelimiterAnchorKind::SemanticToken,
            ),
            SyntaxKind::EnumDecl => (
                name_after_keyword(node, Keyword::Enum)?.span,
                ScopeDelimiterAnchorKind::SemanticToken,
            ),
            SyntaxKind::FunctionDecl | SyntaxKind::MethodDecl => {
                let parameter_start = first_child(node, SyntaxKind::ParameterList)
                    .map_or(node.span.end, |parameters| parameters.span.start);
                (
                    last_name_token_before(node, parameter_start)?.span,
                    ScopeDelimiterAnchorKind::SemanticToken,
                )
            }
            SyntaxKind::IfStatement
            | SyntaxKind::ForStatement
            | SyntaxKind::ForeachStatement
            | SyntaxKind::WhileStatement
            | SyntaxKind::DoWhileStatement
            | SyntaxKind::SwitchStatement
            | SyntaxKind::ElseClause => (
                first_direct_keyword(node)?.span,
                ScopeDelimiterAnchorKind::SemanticToken,
            ),
            SyntaxKind::CallExpression => {
                let arguments = first_child(node, SyntaxKind::ArgumentList)?;
                (
                    last_name_token_before(node, arguments.span.start)?.span,
                    ScopeDelimiterAnchorKind::ResolvedCall,
                )
            }
            SyntaxKind::NewExpression => (
                name_after_keyword(node, Keyword::New)?.span,
                ScopeDelimiterAnchorKind::ResolvedConstructor,
            ),
            SyntaxKind::IndexExpression => {
                let opener = direct_tokens(node)
                    .into_iter()
                    .find(|token| token.kind == TokenKind::LeftBracket)?;
                (
                    last_name_token_before(node, opener.span.start)?.span,
                    ScopeDelimiterAnchorKind::ResolvedIndex,
                )
            }
            SyntaxKind::AttributeList | SyntaxKind::Attribute => (
                first_name_token(node)?.span,
                ScopeDelimiterAnchorKind::SemanticToken,
            ),
            SyntaxKind::InitializerExpression => (
                direct_tokens(node)
                    .into_iter()
                    .find(|token| token.kind == TokenKind::LeftBrace)?
                    .span,
                ScopeDelimiterAnchorKind::Punctuation,
            ),
            SyntaxKind::NameExpression | SyntaxKind::TypeRef => (
                first_name_token(node)?.span,
                ScopeDelimiterAnchorKind::SemanticToken,
            ),
            _ => return None,
        };
        Some(ScopeDelimiterAnchor { span, kind })
    }

    fn collect_standard_pairs(
        &mut self,
        node: &SyntaxNode,
        tokens: &[Token],
        inherited_anchor: Option<ScopeDelimiterAnchor>,
    ) {
        let mut stack: Vec<(Token, Option<ScopeDelimiterAnchor>)> = Vec::new();
        for token in tokens {
            if is_standard_opener(token.kind) {
                let anchor = if matches!(node.kind, SyntaxKind::Declarator | SyntaxKind::Parameter)
                {
                    previous_name_token(tokens, token.span.start)
                        .map(|previous| ScopeDelimiterAnchor {
                            span: previous.span,
                            kind: ScopeDelimiterAnchorKind::SemanticToken,
                        })
                        .or(inherited_anchor)
                } else {
                    inherited_anchor
                };
                stack.push((*token, anchor));
                continue;
            }
            if !is_standard_closer(token.kind) {
                continue;
            }
            let Some((opener, anchor)) = stack.last().copied() else {
                continue;
            };
            if !matching_delimiters(opener.kind, token.kind) {
                continue;
            }
            stack.pop();
            if let Some(anchor) = anchor {
                self.insert(ScopeDelimiter {
                    opener: opener.span,
                    closer: Some(token.span),
                    anchor: anchor.span,
                    anchor_kind: anchor.kind,
                });
            }
        }
        for (opener, anchor) in stack {
            if let Some(anchor) = anchor {
                self.insert(ScopeDelimiter {
                    opener: opener.span,
                    closer: None,
                    anchor: anchor.span,
                    anchor_kind: anchor.kind,
                });
            }
        }
    }

    fn collect_angle_pairs(
        &mut self,
        tokens: &[Token],
        inherited_anchor: Option<ScopeDelimiterAnchor>,
    ) {
        let mut stack: Vec<(TextSpan, Option<ScopeDelimiterAnchor>)> = Vec::new();
        for token in tokens {
            match token.kind {
                TokenKind::Operator(Operator::Less) => {
                    let anchor = previous_name_token(tokens, token.span.start)
                        .map(|previous| ScopeDelimiterAnchor {
                            span: previous.span,
                            kind: ScopeDelimiterAnchorKind::SemanticToken,
                        })
                        .or(inherited_anchor);
                    stack.push((token.span, anchor));
                }
                TokenKind::Operator(Operator::Greater) => {
                    self.close_angle(&mut stack, token.span);
                }
                TokenKind::Operator(Operator::GreaterGreater) if token.span.len() == 2 => {
                    self.close_angle(
                        &mut stack,
                        TextSpan::new(token.span.start, token.span.start + 1),
                    );
                    self.close_angle(
                        &mut stack,
                        TextSpan::new(token.span.start + 1, token.span.end),
                    );
                }
                _ => {}
            }
        }
        for (opener, anchor) in stack {
            if let Some(anchor) = anchor {
                self.insert(ScopeDelimiter {
                    opener,
                    closer: None,
                    anchor: anchor.span,
                    anchor_kind: anchor.kind,
                });
            }
        }
    }

    fn close_angle(
        &mut self,
        stack: &mut Vec<(TextSpan, Option<ScopeDelimiterAnchor>)>,
        closer: TextSpan,
    ) {
        let Some((opener, anchor)) = stack.pop() else {
            return;
        };
        if let Some(anchor) = anchor {
            self.insert(ScopeDelimiter {
                opener,
                closer: Some(closer),
                anchor: anchor.span,
                anchor_kind: anchor.kind,
            });
        }
    }

    fn collect_indexed_type_angles(&mut self) -> Option<()> {
        let Some(index) = self.index else {
            return Some(());
        };
        for (symbol_index, symbol) in index.symbols().iter().enumerate() {
            if symbol_index % 64 == 0
                && self
                    .should_cancel
                    .is_some_and(|should_cancel| should_cancel())
            {
                return None;
            }
            for span in [
                symbol.detail.type_text_span,
                symbol.detail.return_type_text_span,
                symbol.detail.base_type_span,
            ]
            .into_iter()
            .flatten()
            {
                let start = self
                    .lexer_tokens
                    .partition_point(|token| token.span.end <= span.start);
                let end = self
                    .lexer_tokens
                    .partition_point(|token| token.span.start < span.end);
                let tokens = self.lexer_tokens[start..end]
                    .iter()
                    .copied()
                    .filter(|token| span.start <= token.span.start && token.span.end <= span.end)
                    .collect::<Vec<_>>();
                let anchor = tokens
                    .iter()
                    .copied()
                    .find(|token| is_name_token(token.kind))
                    .map(|token| ScopeDelimiterAnchor {
                        span: token.span,
                        kind: ScopeDelimiterAnchorKind::SemanticToken,
                    });
                self.collect_angle_pairs(&tokens, anchor);
            }
        }
        Some(())
    }

    fn insert(&mut self, delimiter: ScopeDelimiter) {
        if self.delimiters.len() < MAX_SCOPE_DELIMITERS {
            self.delimiters.insert(delimiter.opener.start, delimiter);
        }
    }
}

fn delimiter_anchor_is_structurally_proven(delimiter: &ScopeDelimiter) -> bool {
    matches!(
        delimiter.anchor_kind,
        ScopeDelimiterAnchorKind::SemanticToken | ScopeDelimiterAnchorKind::Punctuation
    )
}

fn delimiter_anchor_is_proven(
    delimiter: &ScopeDelimiter,
    lexer_tokens: &[Token],
    resolver: &ReferenceResolver<'_, '_>,
) -> bool {
    if delimiter_anchor_is_structurally_proven(delimiter) {
        return true;
    }
    let token_index =
        lexer_tokens.partition_point(|token| token.span.start < delimiter.anchor.start);
    let Some(token) = lexer_tokens
        .get(token_index)
        .copied()
        .filter(|token| token.span == delimiter.anchor)
    else {
        return false;
    };
    if delimiter.anchor_kind == ScopeDelimiterAnchorKind::ResolvedConstructor {
        if let TokenKind::Keyword(keyword) = token.kind {
            return keyword.is_class_like_type();
        }
    }
    if token.kind != TokenKind::Identifier {
        return false;
    }
    let Some(candidate) = resolver
        .resolve_identifier_token(token.span)
        .and_then(|resolution| resolution.selected)
    else {
        return false;
    };
    delimiter_anchor_kind_is_proven(delimiter, candidate.kind)
}

fn delimiter_anchor_kind_is_proven(delimiter: &ScopeDelimiter, candidate_kind: SymbolKind) -> bool {
    match delimiter.anchor_kind {
        ScopeDelimiterAnchorKind::SemanticToken | ScopeDelimiterAnchorKind::Punctuation => true,
        ScopeDelimiterAnchorKind::ResolvedCall => matches!(
            candidate_kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
        ),
        ScopeDelimiterAnchorKind::ResolvedConstructor => {
            matches!(candidate_kind, SymbolKind::Class | SymbolKind::Typedef)
        }
        ScopeDelimiterAnchorKind::ResolvedIndex => !matches!(
            candidate_kind,
            SymbolKind::Class
                | SymbolKind::TypeParameter
                | SymbolKind::Enum
                | SymbolKind::Typedef
                | SymbolKind::PreprocessorMacro
        ),
    }
}

fn standard_delimiter_node(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::AttributeList
            | SyntaxKind::AttributeArgs
            | SyntaxKind::EnumDecl
            | SyntaxKind::ParameterList
            | SyntaxKind::Declarator
            | SyntaxKind::Parameter
            | SyntaxKind::Block
            | SyntaxKind::Condition
            | SyntaxKind::ForHeader
            | SyntaxKind::ForeachHeader
            | SyntaxKind::SwitchStatement
            | SyntaxKind::SwitchHeader
            | SyntaxKind::ParenthesizedExpression
            | SyntaxKind::CastExpression
            | SyntaxKind::ArgumentList
            | SyntaxKind::IndexExpression
            | SyntaxKind::InitializerExpression
    )
}

fn direct_tokens(node: &SyntaxNode) -> Vec<Token> {
    node.children
        .iter()
        .filter_map(|child| match child {
            SyntaxElement::Token(token) => Some(*token),
            SyntaxElement::Node(_) => None,
        })
        .collect()
}

fn first_child(node: &SyntaxNode, kind: SyntaxKind) -> Option<&SyntaxNode> {
    node.children.iter().find_map(|child| match child {
        SyntaxElement::Node(child) if child.kind == kind => Some(&**child),
        _ => None,
    })
}

fn first_direct_keyword(node: &SyntaxNode) -> Option<Token> {
    direct_tokens(node)
        .into_iter()
        .find(|token| matches!(token.kind, TokenKind::Keyword(_)))
}

fn name_after_keyword(node: &SyntaxNode, keyword: Keyword) -> Option<Token> {
    let mut saw_keyword = false;
    descendant_tokens(node).into_iter().find(|token| {
        if saw_keyword && is_name_token(token.kind) {
            return true;
        }
        if token.kind == TokenKind::Keyword(keyword) {
            saw_keyword = true;
        }
        false
    })
}

fn first_name_token(node: &SyntaxNode) -> Option<Token> {
    descendant_tokens(node)
        .into_iter()
        .find(|token| is_name_token(token.kind))
}

fn last_name_token_before(node: &SyntaxNode, offset: usize) -> Option<Token> {
    descendant_tokens(node)
        .into_iter()
        .filter(|token| token.span.end <= offset && is_name_token(token.kind))
        .next_back()
}

fn previous_name_token(tokens: &[Token], offset: usize) -> Option<Token> {
    tokens
        .iter()
        .copied()
        .filter(|token| token.span.end <= offset && is_name_token(token.kind))
        .next_back()
}

fn descendant_tokens(node: &SyntaxNode) -> Vec<Token> {
    let mut result = Vec::new();
    collect_descendant_tokens(node, &mut result);
    result
}

fn collect_descendant_tokens(node: &SyntaxNode, result: &mut Vec<Token>) {
    for child in &node.children {
        match child {
            SyntaxElement::Token(token) => result.push(*token),
            SyntaxElement::Node(child) => collect_descendant_tokens(child, result),
        }
    }
}

fn is_name_token(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Identifier | TokenKind::Keyword(_))
}

fn is_standard_opener(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::LeftBrace | TokenKind::LeftParen | TokenKind::LeftBracket
    )
}

fn is_standard_closer(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::RightBrace | TokenKind::RightParen | TokenKind::RightBracket
    )
}

fn matching_delimiters(opener: TokenKind, closer: TokenKind) -> bool {
    matches!(
        (opener, closer),
        (TokenKind::LeftBrace, TokenKind::RightBrace)
            | (TokenKind::LeftParen, TokenKind::RightParen)
            | (TokenKind::LeftBracket, TokenKind::RightBracket)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        active_scope_delimiters, scope_delimiters_for_syntax,
        semantic_scope_delimiters_for_analysis, semantic_scope_delimiters_for_analysis_incremental,
        DelimiterProjectionCacheContext, ScopeDelimiterAnchorKind,
    };
    use crate::{
        lexer::lex,
        lsp::{external_indexes::ExternalIndexes, file_index_for_source},
        model::SymbolKind,
        parser::parse_source,
    };
    use std::collections::BTreeMap;

    #[test]
    fn initializer_braces_keep_active_pair_matching_with_punctuation_color() {
        let source = "class Example\n{\n\tvoid Run()\n\t{\n\t\tarray<string> extra = {};\n\t}\n}\n";
        let opener = source.find("extra = {").expect("initializer") + "extra = ".len();
        let closer = opener + 1;
        let delimiters = scope_delimiters_for_syntax(&parse_source(source), &lex(source));
        let initializer = delimiters
            .iter()
            .find(|delimiter| delimiter.opener.start == opener)
            .expect("initializer delimiter");

        assert_eq!(
            initializer.anchor_kind,
            ScopeDelimiterAnchorKind::Punctuation
        );
        assert_eq!(
            initializer.closer.expect("matched initializer").start,
            closer
        );
        assert_eq!(
            active_scope_delimiters(&delimiters, &[closer]),
            vec![*initializer]
        );
    }

    #[test]
    fn reuses_pre_resolved_identifier_kinds_without_changing_delimiters() {
        let source = "void Known() {}\nvoid Run() { Known(); Missing(); }\n";
        let analysis = file_index_for_source(source);
        let without_reuse = semantic_scope_delimiters_for_analysis(
            source,
            &analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            None,
        )
        .expect("delimiter projection");
        let known_start = source.find("Known();").expect("known call");
        let missing_start = source.find("Missing();").expect("missing call");
        let pre_resolved = BTreeMap::from([
            (
                (known_start, known_start + "Known".len()),
                Some(SymbolKind::Function),
            ),
            ((missing_start, missing_start + "Missing".len()), None),
        ]);

        let with_reuse = semantic_scope_delimiters_for_analysis(
            source,
            &analysis,
            ExternalIndexes::new(None, None),
            &pre_resolved,
            true,
            None,
        )
        .expect("delimiter projection");

        assert_eq!(with_reuse.delimiters, without_reuse.delimiters);
        assert_eq!(
            without_reuse.dynamic_owner_resolver_calls - with_reuse.dynamic_owner_resolver_calls,
            2
        );
    }

    #[test]
    fn reuses_unchanged_callable_delimiter_owners_across_document_revisions() {
        let original = "void Known() {}\nclass Example\n{\n\tvoid First() { Known(); }\n\tvoid Second() { Known(); }\n}\n";
        let edited = "void Known() {}\nclass Example\n{\n\tvoid First() { Known(); }\n\tvoid Second() { int value = 1; Known(); }\n}\n";
        let original_analysis = file_index_for_source(original);
        let original_projection = semantic_scope_delimiters_for_analysis_incremental(
            original,
            &original_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(1, 7, None),
            None,
        )
        .expect("original delimiter projection");
        let edited_analysis = file_index_for_source(edited);
        let edited_projection = semantic_scope_delimiters_for_analysis_incremental(
            edited,
            &edited_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(2, 7, original_projection.owner_cache.as_ref()),
            None,
        )
        .expect("edited delimiter projection");
        let cold_edited_projection = semantic_scope_delimiters_for_analysis(
            edited,
            &edited_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            None,
        )
        .expect("cold edited delimiter projection");

        assert_eq!(original_projection.dynamic_owners_recomputed, 2);
        assert_eq!(edited_projection.dynamic_owners_reused, 1);
        assert_eq!(edited_projection.dynamic_owners_invalidated, 1);
        assert_eq!(edited_projection.dynamic_owners_recomputed, 1);
        assert_eq!(edited_projection.dynamic_owner_resolver_calls, 1);
        assert_eq!(
            edited_projection.delimiters,
            cold_edited_projection.delimiters
        );
    }

    #[test]
    fn reuses_callable_owners_after_an_edit_shifts_their_absolute_offsets() {
        let original =
            "void Known() {}\nclass Example\n{\n\tvoid First() { Known(); }\n\tvoid Second() { Known(); }\n}\n";
        let shifted = "// shifted\nvoid Known() {}\nclass Example\n{\n\tvoid First() { Known(); }\n\tvoid Second() { Known(); }\n}\n";
        let original_analysis = file_index_for_source(original);
        let original_projection = semantic_scope_delimiters_for_analysis_incremental(
            original,
            &original_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(1, 7, None),
            None,
        )
        .expect("original delimiter projection");
        let shifted_analysis = file_index_for_source(shifted);
        let shifted_projection = semantic_scope_delimiters_for_analysis_incremental(
            shifted,
            &shifted_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(2, 7, original_projection.owner_cache.as_ref()),
            None,
        )
        .expect("shifted delimiter projection");

        assert_eq!(shifted_projection.dynamic_owners_reused, 2);
        assert_eq!(shifted_projection.dynamic_owners_invalidated, 0);
        assert_eq!(shifted_projection.dynamic_owners_recomputed, 0);
        assert_eq!(shifted_projection.dynamic_owner_resolver_calls, 0);
    }

    #[test]
    fn structural_declaration_changes_invalidate_unchanged_callable_owners() {
        let original =
            "class Example\n{\n\tvoid First() { Added(); }\n\tvoid Second() { Added(); }\n}\n";
        let edited = "void Added() {}\nclass Example\n{\n\tvoid First() { Added(); }\n\tvoid Second() { Added(); }\n}\n";
        let original_analysis = file_index_for_source(original);
        let original_projection = semantic_scope_delimiters_for_analysis_incremental(
            original,
            &original_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(1, 7, None),
            None,
        )
        .expect("original delimiter projection");
        let edited_analysis = file_index_for_source(edited);
        let edited_projection = semantic_scope_delimiters_for_analysis_incremental(
            edited,
            &edited_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(2, 7, original_projection.owner_cache.as_ref()),
            None,
        )
        .expect("edited delimiter projection");

        assert_eq!(edited_projection.dynamic_owners_reused, 0);
        assert_eq!(edited_projection.dynamic_owners_invalidated, 2);
        assert_eq!(edited_projection.dynamic_owners_recomputed, 2);
        assert_eq!(edited_projection.dynamic_owner_resolver_calls, 2);
    }

    #[test]
    fn declaration_reordering_invalidates_unchanged_callable_owners() {
        let original = "void Alpha() {}\nvoid Beta() {}\nclass Example\n{\n\tvoid First() { Alpha(); }\n\tvoid Second() { Beta(); }\n}\n";
        let edited = "void Beta() {}\nvoid Alpha() {}\nclass Example\n{\n\tvoid First() { Alpha(); }\n\tvoid Second() { Beta(); }\n}\n";
        let original_analysis = file_index_for_source(original);
        let original_projection = semantic_scope_delimiters_for_analysis_incremental(
            original,
            &original_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(1, 7, None),
            None,
        )
        .expect("original delimiter projection");
        let edited_analysis = file_index_for_source(edited);
        let edited_projection = semantic_scope_delimiters_for_analysis_incremental(
            edited,
            &edited_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(2, 7, original_projection.owner_cache.as_ref()),
            None,
        )
        .expect("edited delimiter projection");

        assert_eq!(edited_projection.dynamic_owners_reused, 0);
        assert_eq!(edited_projection.dynamic_owners_invalidated, 2);
        assert_eq!(edited_projection.dynamic_owners_recomputed, 2);
        assert_eq!(edited_projection.dynamic_owner_resolver_calls, 2);
    }

    #[test]
    fn duplicate_callable_identities_are_never_reused() {
        let original = "void Known() {}\nclass Example { void Run() { Known(); } }\nclass Example { void Run() { Missing(); } }\n";
        let edited = "void Known() {}\nclass Example { void Run() { Missing(); } }\nclass Example { void Run() { Known(); } }\n";
        let original_analysis = file_index_for_source(original);
        let original_projection = semantic_scope_delimiters_for_analysis_incremental(
            original,
            &original_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(1, 7, None),
            None,
        )
        .expect("original delimiter projection");
        let edited_analysis = file_index_for_source(edited);
        let edited_projection = semantic_scope_delimiters_for_analysis_incremental(
            edited,
            &edited_analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(2, 7, original_projection.owner_cache.as_ref()),
            None,
        )
        .expect("edited delimiter projection");

        assert_eq!(edited_projection.dynamic_owners_reused, 0);
        assert_eq!(edited_projection.dynamic_owners_invalidated, 2);
        assert_eq!(edited_projection.dynamic_owners_recomputed, 2);
    }

    #[test]
    fn external_generation_changes_invalidate_all_cached_callable_owners() {
        let source =
            "void Known() {}\nclass Example\n{\n\tvoid First() { Known(); }\n\tvoid Second() { Known(); }\n}\n";
        let analysis = file_index_for_source(source);
        let original_projection = semantic_scope_delimiters_for_analysis_incremental(
            source,
            &analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(1, 7, None),
            None,
        )
        .expect("original delimiter projection");
        let next_projection = semantic_scope_delimiters_for_analysis_incremental(
            source,
            &analysis,
            ExternalIndexes::new(None, None),
            &BTreeMap::new(),
            true,
            DelimiterProjectionCacheContext::new(2, 8, original_projection.owner_cache.as_ref()),
            None,
        )
        .expect("next delimiter projection");

        assert_eq!(next_projection.dynamic_owners_reused, 0);
        assert_eq!(next_projection.dynamic_owners_invalidated, 2);
        assert_eq!(next_projection.dynamic_owners_recomputed, 2);
        assert_eq!(next_projection.dynamic_owner_resolver_calls, 2);
    }
}
