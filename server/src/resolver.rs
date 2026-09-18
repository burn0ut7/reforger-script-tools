use crate::ast::{
    member_access_for_member_name_at_offset, named_argument_label_at_offset, Expression,
};
use crate::expression_type::{
    base_owner_type_from_symbol, enum_member_ids_for_owner, has_modifier,
    is_pseudo_class_member_name, matching_members_for_exact_owner, member_lookup_owners,
    ExpressionType, ExpressionTypeEnvironment,
};
use crate::index::{GlobalSymbolId, IndexedFile, IndexedSymbol, SymbolIndex};
use crate::lexer::{lex, Keyword, Operator, TextSpan, Token, TokenKind};
use crate::model::{SourceCategory, SourceKind, SymbolKind, VirtualSourceIdentity};
use crate::parser::parse_source;
use crate::scope::LexicalScopeModel;
use crate::syntax::{Parse, SyntaxElement, SyntaxKind, SyntaxNode};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ReferenceResolver<'source, 'index> {
    source: &'source str,
    file_index: &'index SymbolIndex,
    external_indexes: Vec<&'index SymbolIndex>,
    parse: Option<&'index Parse>,
    scope: Option<&'index LexicalScopeModel>,
    owned_parse: Option<Parse>,
    owned_scope: Option<LexicalScopeModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceResolution {
    pub token_text: String,
    pub token_span: TextSpan,
    pub identifier_context: IdentifierContext,
    pub candidates: Vec<ReferenceCandidate>,
    pub selected: Option<ReferenceCandidate>,
    pub reason: ResolutionReason,
    pub receiver: Option<ReceiverResolution>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReferenceResolverTimings {
    pub(crate) context: Duration,
    pub(crate) declaration: Duration,
    pub(crate) scope: Duration,
    pub(crate) member: Duration,
    pub(crate) top_level: Duration,
    pub(crate) external: Duration,
    pub(crate) selection: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverResolution {
    Identifier(ReferenceResolution),
    SyntaxSpan(SyntaxSpanResolution),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxSpanResolution {
    pub candidates: Vec<ReferenceCandidate>,
    pub selected: Option<ReferenceCandidate>,
    pub reason: ResolutionReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceCandidate {
    pub(crate) index_id: crate::index::SymbolIndexId,
    pub source: CandidateSource,
    pub id: GlobalSymbolId,
    pub reason: ResolutionReason,
    pub kind: SymbolKind,
    pub name: Option<String>,
    pub span: TextSpan,
    pub selection_span: TextSpan,
    pub source_kind: SourceKind,
    pub source_category: SourceCategory,
    pub source_priority: u16,
    pub relative_path: Option<PathBuf>,
    pub absolute_path: Option<PathBuf>,
    pub virtual_source: Option<VirtualSourceIdentity>,
    /// Normalized callable shape used by definition navigation to distinguish
    /// an override's inherited declaration from unrelated overloads.
    pub callable_override_key: Option<String>,
    pub is_override: bool,
    pub is_modded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverResolution {
    pub receiver_text: String,
    pub receiver_span: TextSpan,
    pub receiver_expression_kind: String,
    pub owner_type: Option<String>,
    pub is_static: bool,
    pub lookup_path: Vec<String>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberCompletionContext {
    pub receiver: ReceiverResolution,
    pub prefix: String,
    pub prefix_span: TextSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelCompletionContext {
    pub prefix: String,
    pub prefix_span: TextSpan,
    pub identifier_context: IdentifierContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
    FileLocal,
    External,
}

impl CandidateSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileLocal => "file-local",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionReason {
    DeclarationHit,
    LocalInCallable,
    ParameterInCallable,
    ClassMember,
    TypeParameter,
    ReceiverMember,
    StaticMember,
    EngineClassCast,
    PseudoClassMember,
    SyntaxSpan,
    ReceiverUnresolved,
    AttributeNamedArgument,
    NamedArgumentLabel,
    PreprocessorDirective,
    PreprocessorMacro,
    TopLevel,
    ExternalPreferred,
    Unresolved,
}

impl ResolutionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeclarationHit => "declaration-hit",
            Self::LocalInCallable => "local-in-callable",
            Self::ParameterInCallable => "parameter-in-callable",
            Self::ClassMember => "class-member",
            Self::TypeParameter => "type-parameter",
            Self::ReceiverMember => "receiver-member",
            Self::StaticMember => "static-member",
            Self::EngineClassCast => "engine-class-cast",
            Self::PseudoClassMember => "pseudo-class-member",
            Self::SyntaxSpan => "syntax-span",
            Self::ReceiverUnresolved => "receiver-unresolved",
            Self::AttributeNamedArgument => "attribute-named-argument",
            Self::NamedArgumentLabel => "named-argument-label",
            Self::PreprocessorDirective => "preprocessor-directive",
            Self::PreprocessorMacro => "preprocessor-macro",
            Self::TopLevel => "top-level",
            Self::ExternalPreferred => "external-preferred",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierContext {
    DeclarationName,
    MemberAccess,
    MemberCallable,
    MemberOwner,
    TypePosition,
    ConstructedType,
    Callable,
    AttributeType,
    ValueOrCallable,
}

impl IdentifierContext {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeclarationName => "declaration-name",
            Self::MemberAccess => "member-access",
            Self::MemberCallable => "member-callable",
            Self::MemberOwner => "member-owner",
            Self::TypePosition => "type-position",
            Self::ConstructedType => "constructed-type",
            Self::Callable => "callable",
            Self::AttributeType => "attribute-type",
            Self::ValueOrCallable => "value-or-callable",
        }
    }
}

impl<'source, 'index> ReferenceResolver<'source, 'index> {
    pub fn new(
        source: &'source str,
        file_index: &'index SymbolIndex,
        external_index: Option<&'index SymbolIndex>,
    ) -> Self {
        Self::new_with_external_indexes(source, file_index, external_index)
    }

    pub fn new_with_external_indexes(
        source: &'source str,
        file_index: &'index SymbolIndex,
        external_indexes: impl IntoIterator<Item = &'index SymbolIndex>,
    ) -> Self {
        let parse = parse_source(source);
        let scope = LexicalScopeModel::from_parse_and_index(&parse, file_index);
        Self {
            source,
            file_index,
            external_indexes: external_indexes.into_iter().collect(),
            parse: None,
            scope: None,
            owned_parse: Some(parse),
            owned_scope: Some(scope),
        }
    }

    pub fn new_with_parse(
        source: &'source str,
        file_index: &'index SymbolIndex,
        parse: &'index Parse,
        external_index: Option<&'index SymbolIndex>,
    ) -> Self {
        Self::new_with_parse_and_external_indexes(source, file_index, parse, external_index)
    }

    pub fn new_with_parse_and_external_indexes(
        source: &'source str,
        file_index: &'index SymbolIndex,
        parse: &'index Parse,
        external_indexes: impl IntoIterator<Item = &'index SymbolIndex>,
    ) -> Self {
        let scope = LexicalScopeModel::from_parse_and_index(parse, file_index);
        Self {
            source,
            file_index,
            external_indexes: external_indexes.into_iter().collect(),
            parse: Some(parse),
            scope: None,
            owned_parse: None,
            owned_scope: Some(scope),
        }
    }

    pub fn new_with_parse_and_scope(
        source: &'source str,
        file_index: &'index SymbolIndex,
        parse: &'index Parse,
        scope: &'index LexicalScopeModel,
        external_index: Option<&'index SymbolIndex>,
    ) -> Self {
        Self::new_with_parse_scope_and_external_indexes(
            source,
            file_index,
            parse,
            scope,
            external_index,
        )
    }

    pub fn new_with_parse_scope_and_external_indexes(
        source: &'source str,
        file_index: &'index SymbolIndex,
        parse: &'index Parse,
        scope: &'index LexicalScopeModel,
        external_indexes: impl IntoIterator<Item = &'index SymbolIndex>,
    ) -> Self {
        Self {
            source,
            file_index,
            external_indexes: external_indexes.into_iter().collect(),
            parse: Some(parse),
            scope: Some(scope),
            owned_parse: None,
            owned_scope: None,
        }
    }

    pub fn resolve_at_offset(&self, offset: usize) -> Option<ReferenceResolution> {
        let token = token_at_offset(self.source, offset)?;
        match token.kind {
            TokenKind::Identifier => self.resolve_identifier_token(token.span),
            TokenKind::Keyword(keyword) if is_resolvable_type_keyword(keyword) => {
                self.resolve_type_keyword_token(token.span)
            }
            _ => None,
        }
    }

    pub fn resolve_identifier_token(&self, token_span: TextSpan) -> Option<ReferenceResolution> {
        self.resolve_identifier_token_inner(token_span, None)
    }

    pub(crate) fn resolve_identifier_token_profiled(
        &self,
        token_span: TextSpan,
    ) -> (Option<ReferenceResolution>, ReferenceResolverTimings) {
        let mut timings = ReferenceResolverTimings::default();
        let resolution = self.resolve_identifier_token_inner(token_span, Some(&mut timings));
        (resolution, timings)
    }

    fn resolve_identifier_token_inner(
        &self,
        token_span: TextSpan,
        mut timings: Option<&mut ReferenceResolverTimings>,
    ) -> Option<ReferenceResolution> {
        if token_span.start >= token_span.end
            || token_span.end > self.source.len()
            || !self.source.is_char_boundary(token_span.start)
            || !self.source.is_char_boundary(token_span.end)
        {
            return None;
        }

        let context_start = timings.is_some().then(Instant::now);
        let token_text = self.source[token_span.start..token_span.end].to_string();
        if let Some(reason) = preprocessor_reason_for_token(self.source, token_span, &token_text) {
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), context_start) {
                timings.context += start.elapsed();
            }
            if reason == ResolutionReason::PreprocessorMacro {
                return Some(self.resolve_preprocessor_macro_token(token_text, token_span));
            }
            return Some(ReferenceResolution {
                token_text,
                token_span,
                identifier_context: IdentifierContext::ValueOrCallable,
                candidates: Vec::new(),
                selected: None,
                reason,
                receiver: None,
            });
        }
        if is_attribute_named_argument_token(self.source, token_span) {
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), context_start) {
                timings.context += start.elapsed();
            }
            return Some(ReferenceResolution {
                token_text,
                token_span,
                identifier_context: IdentifierContext::ValueOrCallable,
                candidates: Vec::new(),
                selected: None,
                reason: ResolutionReason::AttributeNamedArgument,
                receiver: None,
            });
        }
        if next_significant_char_after_span(self.source, token_span) == Some(':')
            && named_argument_label_at_offset(self.source, &self.parse().root, token_span).is_some()
        {
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), context_start) {
                timings.context += start.elapsed();
            }
            return Some(ReferenceResolution {
                token_text,
                token_span,
                identifier_context: IdentifierContext::ValueOrCallable,
                candidates: Vec::new(),
                selected: None,
                reason: ResolutionReason::NamedArgumentLabel,
                receiver: None,
            });
        }
        let member_access =
            if previous_significant_char_before_span(self.source, token_span) == Some('.') {
                self.member_access_context(token_span)
            } else {
                None
            };
        let syntax_context = self.identifier_context(token_span);
        let identifier_context = if member_access.is_some() {
            if syntax_context == IdentifierContext::MemberCallable {
                IdentifierContext::MemberCallable
            } else {
                IdentifierContext::MemberAccess
            }
        } else {
            syntax_context
        };
        if let (Some(timings), Some(start)) = (timings.as_deref_mut(), context_start) {
            timings.context += start.elapsed();
        }
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();

        let declaration_start = timings.is_some().then(Instant::now);
        self.push_declaration_hits(&token_text, token_span, &mut candidates, &mut seen);
        if let (Some(timings), Some(start)) = (timings.as_deref_mut(), declaration_start) {
            timings.declaration += start.elapsed();
        }

        let receiver = if let Some(member_access) = member_access {
            let member_start = timings.is_some().then(Instant::now);
            let receiver = self.push_receiver_member_candidates(
                &member_access,
                &token_text,
                token_span.start,
                &mut candidates,
                &mut seen,
            );
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), member_start) {
                timings.member += start.elapsed();
            }
            Some(receiver)
        } else if identifier_context == IdentifierContext::TypePosition {
            let scope_start = timings.is_some().then(Instant::now);
            self.push_class_type_parameters(
                &token_text,
                token_span.start,
                &mut candidates,
                &mut seen,
            );
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), scope_start) {
                timings.scope += start.elapsed();
            }
            let top_level_start = timings.is_some().then(Instant::now);
            self.push_type_like_top_level(&token_text, &mut candidates, &mut seen);
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), top_level_start) {
                timings.top_level += start.elapsed();
            }
            let external_start = timings.is_some().then(Instant::now);
            self.push_external_type_like(&token_text, &mut candidates, &mut seen);
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), external_start) {
                timings.external += start.elapsed();
            }
            None
        } else {
            let scope_start = timings.is_some().then(Instant::now);
            self.push_callable_locals_and_parameters(
                &token_text,
                token_span.start,
                &mut candidates,
                &mut seen,
            );
            self.push_class_type_parameters(
                &token_text,
                token_span.start,
                &mut candidates,
                &mut seen,
            );
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), scope_start) {
                timings.scope += start.elapsed();
            }
            let member_start = timings.is_some().then(Instant::now);
            self.push_class_members(&token_text, token_span.start, &mut candidates, &mut seen);
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), member_start) {
                timings.member += start.elapsed();
            }
            let top_level_start = timings.is_some().then(Instant::now);
            self.push_top_level(&token_text, &mut candidates, &mut seen);
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), top_level_start) {
                timings.top_level += start.elapsed();
            }
            let external_start = timings.is_some().then(Instant::now);
            self.push_external(&token_text, &mut candidates, &mut seen);
            if let (Some(timings), Some(start)) = (timings.as_deref_mut(), external_start) {
                timings.external += start.elapsed();
            }
            None
        };

        let selection_start = timings.is_some().then(Instant::now);
        candidates.retain(|candidate| {
            identifier_context_accepts_kind(identifier_context, candidate.kind)
        });
        let selected = candidates.first().cloned();
        let reason = selected
            .as_ref()
            .map(|candidate| candidate.reason)
            .or_else(|| {
                receiver
                    .as_ref()
                    .map(|_| ResolutionReason::ReceiverUnresolved)
            })
            .unwrap_or(ResolutionReason::Unresolved);
        if let (Some(timings), Some(start)) = (timings.as_deref_mut(), selection_start) {
            timings.selection += start.elapsed();
        }

        Some(ReferenceResolution {
            token_text,
            token_span,
            identifier_context,
            candidates,
            selected,
            reason,
            receiver,
        })
    }

    fn resolve_preprocessor_macro_token(
        &self,
        token_text: String,
        token_span: TextSpan,
    ) -> ReferenceResolution {
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();
        self.push_preprocessor_macros(&token_text, &mut candidates, &mut seen);
        let selected = candidates.first().cloned();
        let reason = selected
            .as_ref()
            .map(|candidate| candidate.reason)
            .unwrap_or(ResolutionReason::PreprocessorMacro);

        ReferenceResolution {
            token_text,
            token_span,
            identifier_context: IdentifierContext::ValueOrCallable,
            candidates,
            selected,
            reason,
            receiver: None,
        }
    }

    fn resolve_type_keyword_token(&self, token_span: TextSpan) -> Option<ReferenceResolution> {
        let token = token_at_offset(self.source, token_span.start)?;
        let TokenKind::Keyword(keyword) = token.kind else {
            return None;
        };
        let identifier_context = self.identifier_context(token_span);
        if identifier_context != IdentifierContext::TypePosition && !keyword.is_class_like_type() {
            return None;
        }

        let token_text = self.source[token_span.start..token_span.end].to_string();
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();
        self.push_type_like_top_level(&token_text, &mut candidates, &mut seen);
        self.push_external_type_like(&token_text, &mut candidates, &mut seen);
        let selected = candidates.first().cloned();
        let reason = selected
            .as_ref()
            .map(|candidate| candidate.reason)
            .unwrap_or(ResolutionReason::Unresolved);

        Some(ReferenceResolution {
            token_text,
            token_span,
            identifier_context,
            candidates,
            selected,
            reason,
            receiver: None,
        })
    }

    fn parse(&self) -> &Parse {
        self.parse
            .or(self.owned_parse.as_ref())
            .expect("resolver should always have parse context")
    }

    fn scope(&self) -> &LexicalScopeModel {
        self.scope
            .or(self.owned_scope.as_ref())
            .expect("resolver should always have scope context")
    }

    fn type_environment(&self) -> ExpressionTypeEnvironment<'source, '_> {
        ExpressionTypeEnvironment::new_with_external_indexes(
            self.source,
            self.file_index,
            self.parse(),
            self.scope(),
            self.external_indexes(),
        )
    }

    fn external_indexes(&self) -> impl Iterator<Item = &'index SymbolIndex> + '_ {
        self.external_indexes.iter().copied()
    }

    pub fn resolve_hover_at_offset(&self, offset: usize) -> Option<HoverResolution> {
        if let Some(reference) = self.resolve_at_offset(offset) {
            return Some(HoverResolution::Identifier(reference));
        }

        let token = token_at_offset(self.source, offset)?;
        if !is_syntax_span_hover_token(token.kind) {
            return None;
        }

        let span_resolution = self.resolve_syntax_span_hover_at_offset(offset)?;
        Some(HoverResolution::SyntaxSpan(span_resolution))
    }

    pub fn member_completion_context_at_offset(
        &self,
        offset: usize,
    ) -> Option<MemberCompletionContext> {
        let tokens = lex(self.source);
        self.member_completion_context_at_offset_with_tokens(offset, &tokens)
    }

    pub fn member_completion_context_at_offset_with_tokens(
        &self,
        offset: usize,
        tokens: &[Token],
    ) -> Option<MemberCompletionContext> {
        if offset > self.source.len() || !self.source.is_char_boundary(offset) {
            return None;
        }

        let (dot, prefix, prefix_span) = completion_dot_and_prefix(self.source, tokens, offset)?;
        if dot.span.start == 0 {
            return None;
        }

        let receiver = receiver_expression_before_dot(self.source, &self.parse().root, dot.span)?;

        let mut lookup_path = Vec::new();
        let inferred =
            self.type_environment()
                .infer_expression_type(receiver, offset, &mut lookup_path);
        let Some(inferred) = inferred else {
            return Some(MemberCompletionContext {
                receiver: ReceiverResolution {
                    receiver_text: receiver.source_text().trim().to_string(),
                    receiver_span: receiver.span(),
                    receiver_expression_kind: format!("{:?}", receiver.kind()),
                    owner_type: None,
                    is_static: false,
                    lookup_path,
                    failure_reason: Some("receiver type was not inferred".to_string()),
                },
                prefix,
                prefix_span,
            });
        };

        Some(MemberCompletionContext {
            receiver: ReceiverResolution {
                receiver_text: receiver.source_text().trim().to_string(),
                receiver_span: receiver.span(),
                receiver_expression_kind: format!("{:?}", receiver.kind()),
                owner_type: Some(inferred.owner_type),
                is_static: inferred.is_static,
                lookup_path,
                failure_reason: None,
            },
            prefix,
            prefix_span,
        })
    }

    pub fn top_level_completion_context_at_offset(
        &self,
        offset: usize,
    ) -> Option<TopLevelCompletionContext> {
        let tokens = lex(self.source);
        self.top_level_completion_context_at_offset_with_tokens(offset, &tokens)
    }

    pub fn top_level_completion_context_at_offset_with_tokens(
        &self,
        offset: usize,
        tokens: &[Token],
    ) -> Option<TopLevelCompletionContext> {
        if offset > self.source.len() || !self.source.is_char_boundary(offset) {
            return None;
        }

        let (prefix, prefix_span) = completion_identifier_prefix(self.source, tokens, offset)?;
        if prefix.is_empty() {
            return None;
        }
        if completion_dot_and_prefix(self.source, tokens, offset).is_some() {
            return None;
        }
        if previous_significant_token_before_span(tokens, prefix_span)
            .is_some_and(|token| token.kind == TokenKind::Dot)
        {
            return None;
        }
        if named_argument_label_at_offset(self.source, &self.parse().root, prefix_span).is_some() {
            return None;
        }

        let identifier_context = self.identifier_context(prefix_span);
        let identifier_context = if identifier_context == IdentifierContext::TypePosition {
            identifier_context
        } else {
            completion_recovery_type_context(self.source, tokens, prefix_span)
        };
        Some(TopLevelCompletionContext {
            prefix,
            prefix_span,
            identifier_context,
        })
    }

    pub fn syntax_span_candidates_at_offset(&self, offset: usize) -> Vec<ReferenceCandidate> {
        let mut candidates = self
            .file_index
            .symbols()
            .iter()
            .filter_map(|symbol| {
                if !symbol_detail_span_contains_offset(symbol, offset) {
                    return None;
                }
                let selection_hit = span_contains(symbol.selection_span, offset);
                let span_hit = span_contains(symbol.span, offset);
                if !selection_hit && !span_hit {
                    return None;
                }
                let matched_span = if selection_hit {
                    symbol.selection_span
                } else {
                    symbol.span
                };
                let file = self.file_index.file(symbol.id.file_id)?;
                Some((
                    !selection_hit,
                    matched_span.end.saturating_sub(matched_span.start),
                    symbol.id.file_id,
                    symbol.id.symbol_id,
                    candidate_from_symbol(
                        self.file_index,
                        CandidateSource::FileLocal,
                        symbol.id,
                        ResolutionReason::SyntaxSpan,
                        symbol,
                        file,
                    ),
                ))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| (candidate.0, candidate.1, candidate.2, candidate.3));
        candidates
            .into_iter()
            .map(|(_, _, _, _, candidate)| candidate)
            .collect()
    }

    fn resolve_syntax_span_hover_at_offset(&self, offset: usize) -> Option<SyntaxSpanResolution> {
        let candidates = self.syntax_span_candidates_at_offset(offset);
        let selected = candidates.first().cloned();
        selected.as_ref()?;
        Some(SyntaxSpanResolution {
            candidates,
            selected,
            reason: ResolutionReason::SyntaxSpan,
        })
    }

    fn identifier_context(&self, token_span: TextSpan) -> IdentifierContext {
        if self
            .file_index
            .symbols()
            .iter()
            .any(|symbol| symbol.selection_span == token_span)
        {
            return IdentifierContext::DeclarationName;
        }

        if let Some(context) = syntax_identifier_context(&self.parse().root, token_span) {
            return context;
        }

        if self.file_index.symbols().iter().any(|symbol| {
            [
                symbol.detail.type_text_span,
                symbol.detail.return_type_text_span,
                symbol.detail.base_type_span,
            ]
            .into_iter()
            .flatten()
            .any(|span| {
                span_contains_span(span, token_span)
                    && type_position_span_is_reliable(self.source, symbol, span, token_span)
            })
        }) {
            return IdentifierContext::TypePosition;
        }

        IdentifierContext::ValueOrCallable
    }

    fn push_declaration_hits(
        &self,
        token_text: &str,
        token_span: TextSpan,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for id in self.file_index.symbols_for_name(token_text) {
            let Some(symbol) = self.file_index.symbol(*id) else {
                continue;
            };
            if symbol.selection_span == token_span {
                self.push_candidate(
                    candidates,
                    seen,
                    CandidateSource::FileLocal,
                    *id,
                    ResolutionReason::DeclarationHit,
                );
            }
        }
    }

    fn push_callable_locals_and_parameters(
        &self,
        token_text: &str,
        offset: usize,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for id in self
            .scope()
            .visible_symbols_named(self.file_index, token_text, offset)
        {
            let Some(symbol) = self.file_index.symbol(id) else {
                continue;
            };
            let reason = match symbol.kind {
                SymbolKind::LocalVariable => ResolutionReason::LocalInCallable,
                SymbolKind::Parameter => ResolutionReason::ParameterInCallable,
                _ => continue,
            };
            self.push_candidate(candidates, seen, CandidateSource::FileLocal, id, reason);
        }
    }

    fn push_class_members(
        &self,
        token_text: &str,
        offset: usize,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        let Some(class) = self.containing_class(offset) else {
            return;
        };
        let Some(class_name) = self
            .file_index
            .symbol(class)
            .and_then(|symbol| symbol.name.as_deref())
        else {
            return;
        };

        let before = candidates.len();
        push_class_member_candidates_from_index(
            self.file_index,
            CandidateSource::FileLocal,
            class_name,
            token_text,
            candidates,
            seen,
        );
        if let Some(base_type) = base_owner_type_from_symbol(self.file_index, class) {
            push_class_member_candidates_from_index(
                self.file_index,
                CandidateSource::FileLocal,
                &base_type,
                token_text,
                candidates,
                seen,
            );
        }
        for external_index in self.external_indexes() {
            push_class_member_candidates_from_index(
                external_index,
                CandidateSource::External,
                class_name,
                token_text,
                candidates,
                seen,
            );
            if let Some(base_type) = base_owner_type_from_symbol(self.file_index, class) {
                push_class_member_candidates_from_index(
                    external_index,
                    CandidateSource::External,
                    &base_type,
                    token_text,
                    candidates,
                    seen,
                );
            }
        }

        if candidates.len() == before && is_pseudo_class_member_name(token_text) {
            self.push_pseudo_class_member_rule(token_text, candidates, seen);
        }
    }

    fn push_class_type_parameters(
        &self,
        token_text: &str,
        offset: usize,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        let Some(class_id) = self.containing_class(offset) else {
            return;
        };
        for child in self.file_index.children(class_id) {
            let Some(symbol) = self.file_index.symbol(*child) else {
                continue;
            };
            if symbol.kind == SymbolKind::TypeParameter
                && symbol.name.as_deref() == Some(token_text)
            {
                self.push_candidate(
                    candidates,
                    seen,
                    CandidateSource::FileLocal,
                    *child,
                    ResolutionReason::TypeParameter,
                );
            }
        }
    }

    fn push_top_level(
        &self,
        token_text: &str,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for id in self.file_index.top_level_symbols_for_name(token_text) {
            self.push_candidate(
                candidates,
                seen,
                CandidateSource::FileLocal,
                *id,
                ResolutionReason::TopLevel,
            );
        }
    }

    fn push_type_like_top_level(
        &self,
        token_text: &str,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for id in self.file_index.top_level_symbols_for_name(token_text) {
            let Some(symbol) = self.file_index.symbol(*id) else {
                continue;
            };
            if is_type_like_kind(symbol.kind) {
                self.push_candidate(
                    candidates,
                    seen,
                    CandidateSource::FileLocal,
                    *id,
                    ResolutionReason::TopLevel,
                );
            }
        }
    }

    fn push_external(
        &self,
        token_text: &str,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for external_index in self.external_indexes() {
            let mut external = Vec::new();
            for id in external_index.preferred_classes_by_name(token_text) {
                push_unique_id(&mut external, id);
            }
            for id in external_index.preferred_typedefs_by_name(token_text) {
                push_unique_id(&mut external, id);
            }
            for id in external_index.preferred_functions_by_name(token_text) {
                push_unique_id(&mut external, id);
            }
            for id in external_index.preferred_top_level_symbols_for_name(token_text) {
                push_unique_id(&mut external, id);
            }

            for id in external {
                push_index_candidate(
                    external_index,
                    candidates,
                    seen,
                    CandidateSource::External,
                    id,
                    ResolutionReason::ExternalPreferred,
                );
            }
        }
    }

    fn push_external_type_like(
        &self,
        token_text: &str,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for external_index in self.external_indexes() {
            let external = external_index
                .top_level_symbols_for_name(token_text)
                .iter()
                .copied()
                .filter(|id| {
                    external_index
                        .symbol(*id)
                        .is_some_and(|symbol| is_type_like_kind(symbol.kind))
                })
                .collect::<Vec<_>>();

            for id in external_index.preferred_from_symbols(&external) {
                push_index_candidate(
                    external_index,
                    candidates,
                    seen,
                    CandidateSource::External,
                    id,
                    ResolutionReason::ExternalPreferred,
                );
            }
        }
    }

    fn push_preprocessor_macros(
        &self,
        token_text: &str,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for id in self.file_index.symbols_for_name(token_text) {
            if self
                .file_index
                .symbol(*id)
                .is_some_and(|symbol| symbol.kind == SymbolKind::PreprocessorMacro)
            {
                self.push_candidate(
                    candidates,
                    seen,
                    CandidateSource::FileLocal,
                    *id,
                    ResolutionReason::PreprocessorMacro,
                );
            }
        }

        for external_index in self.external_indexes() {
            let external = external_index
                .symbols_for_name(token_text)
                .iter()
                .copied()
                .filter(|id| {
                    external_index
                        .symbol(*id)
                        .is_some_and(|symbol| symbol.kind == SymbolKind::PreprocessorMacro)
                })
                .collect::<Vec<_>>();
            for id in external_index.preferred_from_symbols(&external) {
                push_index_candidate(
                    external_index,
                    candidates,
                    seen,
                    CandidateSource::External,
                    id,
                    ResolutionReason::PreprocessorMacro,
                );
            }
        }
    }

    fn push_receiver_member_candidates(
        &self,
        member_access: &MemberAccessContext<'source, '_>,
        member_name: &str,
        offset: usize,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) -> ReceiverResolution {
        let mut lookup_path = Vec::new();
        let environment = self.type_environment();
        let inferred =
            environment.infer_expression_type(member_access.receiver, offset, &mut lookup_path);
        let Some(mut inferred) = inferred else {
            return ReceiverResolution {
                receiver_text: member_access.receiver.source_text().trim().to_string(),
                receiver_span: member_access.receiver_span,
                receiver_expression_kind: format!("{:?}", member_access.receiver.kind()),
                owner_type: None,
                is_static: false,
                lookup_path,
                failure_reason: Some("receiver type was not inferred".to_string()),
            };
        };
        if member_name == "Cast" {
            if let Some(static_owner) =
                environment.static_type_name_from_expression(member_access.receiver, offset)
            {
                inferred = InferredReceiverType::static_type(static_owner);
            }
        }

        let reason = if inferred.is_static {
            ResolutionReason::StaticMember
        } else {
            ResolutionReason::ReceiverMember
        };
        let modded_super = inferred.is_modded_super();
        let before = candidates.len();
        if !modded_super {
            self.push_members_for_owner(
                self.file_index,
                CandidateSource::FileLocal,
                &inferred.owner_type,
                member_name,
                inferred.is_static,
                false,
                reason,
                candidates,
                seen,
            );
        }
        for external_index in self.external_indexes() {
            self.push_members_for_owner(
                external_index,
                CandidateSource::External,
                &inferred.owner_type,
                member_name,
                inferred.is_static,
                modded_super,
                reason,
                candidates,
                seen,
            );
        }

        if candidates.len() == before && inferred.is_static {
            self.push_engine_class_cast_rule(member_name, candidates, seen);
        }

        ReceiverResolution {
            receiver_text: member_access.receiver.source_text().trim().to_string(),
            receiver_span: member_access.receiver_span,
            receiver_expression_kind: format!("{:?}", member_access.receiver.kind()),
            owner_type: Some(inferred.owner_type),
            is_static: inferred.is_static,
            lookup_path,
            failure_reason: (candidates.len() == before)
                .then(|| format!("no member named `{member_name}` was found for inferred owner")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_members_for_owner(
        &self,
        index: &SymbolIndex,
        source: CandidateSource,
        owner: &str,
        member_name: &str,
        static_only: bool,
        modded_predecessor: bool,
        mut reason: ResolutionReason,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        let mut matching = Vec::new();
        if modded_predecessor {
            matching
                .extend(index.preferred_members_named_for_modded_predecessor(owner, member_name));
        } else {
            for owner in member_lookup_owners(index, owner) {
                for id in index.preferred_members_named_for_class(&owner, member_name) {
                    if index.symbol(id).is_some_and(|symbol| {
                        is_member_lookup_kind(symbol.kind)
                            && symbol.name.as_deref() == Some(member_name)
                    }) {
                        push_unique_id(&mut matching, id);
                    }
                }
            }
        }
        matching.retain(|id| {
            index.symbol(*id).is_some_and(|symbol| {
                is_member_lookup_kind(symbol.kind) && symbol.name.as_deref() == Some(member_name)
            })
        });

        if static_only {
            let static_matching = matching
                .iter()
                .copied()
                .filter(|id| {
                    index
                        .symbol(*id)
                        .is_some_and(|symbol| has_modifier(symbol, "static"))
                })
                .collect::<Vec<_>>();
            if !static_matching.is_empty() {
                matching = static_matching;
            }
        }

        if !modded_predecessor
            && matching.is_empty()
            && !static_only
            && is_pseudo_class_member_name(member_name)
        {
            for id in matching_members_for_exact_owner(index, "Class", member_name) {
                push_unique_id(&mut matching, id);
            }
            reason = ResolutionReason::PseudoClassMember;
        }

        for id in matching {
            push_index_candidate(index, candidates, seen, source, id, reason);
        }

        if !modded_predecessor {
            self.push_enum_members_for_owner(
                index,
                source,
                owner,
                member_name,
                reason,
                candidates,
                seen,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_enum_members_for_owner(
        &self,
        index: &SymbolIndex,
        source: CandidateSource,
        owner: &str,
        member_name: &str,
        reason: ResolutionReason,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        for member in enum_member_ids_for_owner(index, owner, member_name) {
            push_index_candidate(index, candidates, seen, source, member, reason);
        }
    }

    fn push_engine_class_cast_rule(
        &self,
        member_name: &str,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        if member_name != "Cast" {
            return;
        }

        self.push_members_for_owner(
            self.file_index,
            CandidateSource::FileLocal,
            "Class",
            member_name,
            true,
            false,
            ResolutionReason::EngineClassCast,
            candidates,
            seen,
        );
        for external_index in self.external_indexes() {
            self.push_members_for_owner(
                external_index,
                CandidateSource::External,
                "Class",
                member_name,
                true,
                false,
                ResolutionReason::EngineClassCast,
                candidates,
                seen,
            );
        }
    }

    fn push_pseudo_class_member_rule(
        &self,
        member_name: &str,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
    ) {
        push_class_member_candidates_from_index_with_reason(
            self.file_index,
            CandidateSource::FileLocal,
            "Class",
            member_name,
            ResolutionReason::PseudoClassMember,
            candidates,
            seen,
        );
        for external_index in self.external_indexes() {
            push_class_member_candidates_from_index_with_reason(
                external_index,
                CandidateSource::External,
                "Class",
                member_name,
                ResolutionReason::PseudoClassMember,
                candidates,
                seen,
            );
        }
    }

    fn member_access_context(
        &self,
        token_span: TextSpan,
    ) -> Option<MemberAccessContext<'source, '_>> {
        let member_access =
            member_access_for_member_name_at_offset(self.source, &self.parse().root, token_span)?;
        Some(MemberAccessContext {
            receiver: member_access.receiver,
            receiver_span: member_access.receiver.span(),
        })
    }

    fn push_candidate(
        &self,
        candidates: &mut Vec<ReferenceCandidate>,
        seen: &mut BTreeSet<CandidateKey>,
        source: CandidateSource,
        id: GlobalSymbolId,
        reason: ResolutionReason,
    ) {
        push_index_candidate(self.file_index, candidates, seen, source, id, reason);
    }

    fn containing_class(&self, offset: usize) -> Option<GlobalSymbolId> {
        self.file_index
            .symbols()
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Class && span_contains(symbol.span, offset))
            .min_by_key(|symbol| symbol.span.len())
            .map(|symbol| symbol.id)
    }
}

/// Completion prefixes are commonly incomplete and therefore absent from the
/// indexed declaration/type spans. Recover only two unambiguous grammar slots
/// here: the operand of `new` and a builtin collection's type argument.
fn completion_recovery_type_context(
    source: &str,
    tokens: &[Token],
    prefix_span: TextSpan,
) -> IdentifierContext {
    let Some(previous_index) = previous_non_trivia_token_index(tokens, prefix_span.start) else {
        return IdentifierContext::ValueOrCallable;
    };
    let previous = tokens[previous_index];
    if previous.kind == TokenKind::Keyword(Keyword::New) {
        return IdentifierContext::TypePosition;
    }
    if previous.kind != TokenKind::Operator(Operator::Less) || previous_index == 0 {
        return IdentifierContext::ValueOrCallable;
    }
    let collection = tokens[previous_index - 1];
    (collection.kind == TokenKind::Identifier
        && source
            .get(collection.span.start..collection.span.end)
            .is_some_and(|name| matches!(name, "array" | "set" | "map")))
    .then_some(IdentifierContext::TypePosition)
    .unwrap_or(IdentifierContext::ValueOrCallable)
}

#[derive(Debug, Clone)]
struct MemberAccessContext<'source, 'tree> {
    receiver: Expression<'source, 'tree>,
    receiver_span: TextSpan,
}

type InferredReceiverType = ExpressionType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CandidateKey {
    index_id: crate::index::SymbolIndexId,
    source: CandidateSource,
    id: GlobalSymbolId,
}

impl PartialOrd for CandidateSource {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CandidateSource {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

fn push_index_candidate(
    index: &SymbolIndex,
    candidates: &mut Vec<ReferenceCandidate>,
    seen: &mut BTreeSet<CandidateKey>,
    source: CandidateSource,
    id: GlobalSymbolId,
    reason: ResolutionReason,
) {
    let key = CandidateKey {
        index_id: index.identity(),
        source,
        id,
    };
    if !seen.insert(key) {
        return;
    }
    let Some(symbol) = index.symbol(id) else {
        return;
    };
    let Some(file) = index.file(id.file_id) else {
        return;
    };
    candidates.push(candidate_from_symbol(
        index, source, id, reason, symbol, file,
    ));
}

fn push_class_member_candidates_from_index(
    index: &SymbolIndex,
    source: CandidateSource,
    class_name: &str,
    token_text: &str,
    candidates: &mut Vec<ReferenceCandidate>,
    seen: &mut BTreeSet<CandidateKey>,
) {
    push_class_member_candidates_from_index_with_reason(
        index,
        source,
        class_name,
        token_text,
        ResolutionReason::ClassMember,
        candidates,
        seen,
    );
}

fn push_class_member_candidates_from_index_with_reason(
    index: &SymbolIndex,
    source: CandidateSource,
    class_name: &str,
    token_text: &str,
    reason: ResolutionReason,
    candidates: &mut Vec<ReferenceCandidate>,
    seen: &mut BTreeSet<CandidateKey>,
) {
    for member in index.preferred_members_named_for_class(class_name, token_text) {
        push_index_candidate(index, candidates, seen, source, member, reason);
    }
}

fn candidate_from_symbol(
    index: &SymbolIndex,
    source: CandidateSource,
    id: GlobalSymbolId,
    reason: ResolutionReason,
    symbol: &IndexedSymbol,
    file: &IndexedFile,
) -> ReferenceCandidate {
    ReferenceCandidate {
        index_id: index.identity(),
        source,
        id,
        reason,
        kind: symbol.kind,
        name: symbol.name.clone(),
        span: symbol.span,
        selection_span: symbol.selection_span,
        source_kind: file.metadata.kind,
        source_category: file.metadata.category,
        source_priority: file.metadata.priority,
        relative_path: file.metadata.relative_path.clone(),
        absolute_path: file.metadata.absolute_path.clone(),
        virtual_source: file.metadata.virtual_source.clone(),
        callable_override_key: callable_override_key(index, id),
        is_override: has_modifier(symbol, "override"),
        is_modded: has_modifier(symbol, "modded"),
    }
}

pub(crate) fn callable_override_key(index: &SymbolIndex, id: GlobalSymbolId) -> Option<String> {
    let symbol = index.symbol(id)?;
    (symbol.kind == SymbolKind::Method).then_some(())?;
    let return_type = symbol.detail.return_type_text.as_deref().unwrap_or("");
    let parameters = index
        .children(id)
        .iter()
        .filter_map(|child_id| index.symbol(*child_id))
        .filter(|child| child.kind == SymbolKind::Parameter)
        .map(|child| {
            let modifiers = child.modifiers.join(" ");
            let type_text = child.detail.type_text.as_deref().unwrap_or("");
            format!("{modifiers}|{type_text}")
        })
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("{return_type}({parameters})"))
}

fn token_at_offset(source: &str, offset: usize) -> Option<crate::lexer::Token> {
    lex(source)
        .into_iter()
        .find(|token| token.span.start <= offset && offset < token.span.end)
}

fn is_syntax_span_hover_token(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Keyword(_))
}

fn is_resolvable_type_keyword(keyword: Keyword) -> bool {
    matches!(
        keyword,
        Keyword::Bool
            | Keyword::Int
            | Keyword::Float
            | Keyword::String
            | Keyword::Vector
            | Keyword::Typename
    )
}

fn symbol_detail_span_contains_offset(symbol: &IndexedSymbol, offset: usize) -> bool {
    [
        symbol.detail.type_text_span,
        symbol.detail.return_type_text_span,
        symbol.detail.base_type_span,
    ]
    .into_iter()
    .flatten()
    .any(|span| span_contains(span, offset))
}

fn preprocessor_reason_for_token(
    source: &str,
    token_span: TextSpan,
    token_text: &str,
) -> Option<ResolutionReason> {
    let line_start = source[..token_span.start]
        .rfind(['\r', '\n'])
        .map_or(0, |index| index + 1);
    let before = &source[line_start..token_span.start];
    if !before.trim_start().starts_with('#') {
        return None;
    }

    if matches!(
        token_text,
        "if" | "ifdef" | "ifndef" | "elif" | "else" | "endif" | "define" | "undef" | "include"
    ) {
        Some(ResolutionReason::PreprocessorDirective)
    } else {
        Some(ResolutionReason::PreprocessorMacro)
    }
}

fn is_attribute_named_argument_token(source: &str, token_span: TextSpan) -> bool {
    let line_start = source[..token_span.start]
        .rfind(['\r', '\n'])
        .map_or(0, |index| index + 1);
    let line_end = source[token_span.end..]
        .find(['\r', '\n'])
        .map_or(source.len(), |index| token_span.end + index);
    let before = &source[line_start..token_span.start];
    let after = &source[token_span.end..line_end];
    before.rfind('[').is_some_and(|open| {
        before[open..].find(']').is_none() && after.trim_start().starts_with(':')
    })
}

fn completion_dot_and_prefix(
    source: &str,
    tokens: &[crate::lexer::Token],
    offset: usize,
) -> Option<(crate::lexer::Token, String, TextSpan)> {
    let mut token_index = completion_token_index(tokens, offset)?;
    let mut prefix = String::new();
    let mut prefix_span = TextSpan::new(offset, offset);

    let token = tokens[token_index];
    if token.kind == TokenKind::Identifier && token.span.start < offset && offset <= token.span.end
    {
        prefix = source[token.span.start..offset].to_string();
        prefix_span = TextSpan::new(token.span.start, offset);
        token_index = previous_non_trivia_token_index(tokens, token.span.start)?;
    }

    let dot = tokens[token_index];
    if dot.kind != TokenKind::Dot {
        return None;
    }

    Some((dot, prefix, prefix_span))
}

fn completion_identifier_prefix(
    source: &str,
    tokens: &[crate::lexer::Token],
    offset: usize,
) -> Option<(String, TextSpan)> {
    let token_index = completion_token_index(tokens, offset)?;
    let token = tokens[token_index];
    if token.kind != TokenKind::Identifier || token.span.start > offset || offset > token.span.end {
        return None;
    }
    let prefix = source[token.span.start..offset].to_string();
    Some((prefix, TextSpan::new(token.span.start, offset)))
}

fn completion_token_index(tokens: &[crate::lexer::Token], offset: usize) -> Option<usize> {
    if tokens.iter().any(|token| {
        token.span.start < offset && offset <= token.span.end && token_blocks_completion(token.kind)
    }) {
        return None;
    }

    tokens
        .iter()
        .enumerate()
        .find(|(_, token)| {
            token.span.start < offset
                && offset <= token.span.end
                && !token.kind.is_trivia()
                && token.kind != TokenKind::Eof
        })
        .map(|(index, _)| index)
        .or_else(|| previous_non_trivia_token_index(tokens, offset))
}

fn token_blocks_completion(kind: TokenKind) -> bool {
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

fn previous_non_trivia_token_index(tokens: &[crate::lexer::Token], offset: usize) -> Option<usize> {
    tokens
        .iter()
        .enumerate()
        .rev()
        .find(|(_, token)| {
            token.span.end <= offset && !token.kind.is_trivia() && token.kind != TokenKind::Eof
        })
        .map(|(index, _)| index)
}

fn previous_significant_token_before_span(
    tokens: &[crate::lexer::Token],
    span: TextSpan,
) -> Option<crate::lexer::Token> {
    previous_non_trivia_token_index(tokens, span.start).map(|index| tokens[index])
}

fn previous_significant_char_before_span(source: &str, span: TextSpan) -> Option<char> {
    source
        .get(..span.start)?
        .chars()
        .rev()
        .find(|character| !character.is_whitespace())
}

fn next_significant_char_after_span(source: &str, span: TextSpan) -> Option<char> {
    source
        .get(span.end..)?
        .chars()
        .find(|character| !character.is_whitespace())
}

fn receiver_expression_before_dot<'source, 'tree>(
    source: &'source str,
    root: &'tree SyntaxNode,
    dot_span: TextSpan,
) -> Option<Expression<'source, 'tree>> {
    let mut best = None;
    collect_receiver_expression_before_dot(source, root, dot_span.start, &mut best);
    best
}

fn collect_receiver_expression_before_dot<'source, 'tree>(
    source: &'source str,
    node: &'tree SyntaxNode,
    dot_start: usize,
    best: &mut Option<Expression<'source, 'tree>>,
) {
    if node.span.start > dot_start || node.span.end < dot_start {
        return;
    }

    if let Some(expression) = Expression::from_node(source, node) {
        let span = expression.span();
        if span.end <= dot_start && source[span.end..dot_start].trim().is_empty() {
            let replace = best
                .as_ref()
                .map(|best| expression.span().len() > best.span().len())
                .unwrap_or(true);
            if replace {
                *best = Some(expression);
            }
        }
    }

    for child in &node.children {
        if let SyntaxElement::Node(child) = child {
            collect_receiver_expression_before_dot(source, child, dot_start, best);
        }
    }
}

fn is_member_lookup_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Field | SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Destructor
    )
}

fn span_contains(span: TextSpan, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

fn span_contains_span(outer: TextSpan, inner: TextSpan) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn syntax_identifier_context(node: &SyntaxNode, token_span: TextSpan) -> Option<IdentifierContext> {
    if !span_contains_span(node.span, token_span) {
        return None;
    }
    if node.kind == SyntaxKind::GenericArgList {
        return Some(IdentifierContext::TypePosition);
    }

    let direct_name_span = |node: &SyntaxNode| {
        (node.kind == SyntaxKind::NameExpression)
            .then(|| {
                node.children.iter().find_map(|child| match child {
                    SyntaxElement::Token(token) if !token.kind.is_trivia() => Some(token.span),
                    _ => None,
                })
            })
            .flatten()
    };

    match node.kind {
        SyntaxKind::Attribute => {
            if node.children.iter().any(
                |child| matches!(child, SyntaxElement::Token(token) if token.span == token_span),
            ) {
                return Some(IdentifierContext::AttributeType);
            }
        }
        SyntaxKind::NewExpression => {
            if node.children.iter().any(|child| {
                matches!(
                    child,
                    SyntaxElement::Node(name)
                        if direct_name_span(name) == Some(token_span)
                )
            }) {
                return Some(IdentifierContext::ConstructedType);
            }
        }
        SyntaxKind::CallExpression => {
            let callee = node.children.iter().find_map(|child| match child {
                SyntaxElement::Node(child) if child.kind != SyntaxKind::ArgumentList => {
                    Some(child.as_ref())
                }
                _ => None,
            });
            if let Some(callee) = callee {
                if direct_name_span(callee) == Some(token_span) {
                    return Some(IdentifierContext::Callable);
                }
                if callee.kind == SyntaxKind::MemberAccessExpression
                    && callee.children.iter().rev().find_map(|child| match child {
                        SyntaxElement::Node(name) => direct_name_span(name),
                        SyntaxElement::Token(_) => None,
                    }) == Some(token_span)
                {
                    return Some(IdentifierContext::MemberCallable);
                }
            }
        }
        SyntaxKind::MemberAccessExpression => {
            if node.children.iter().find_map(|child| match child {
                SyntaxElement::Node(receiver) => direct_name_span(receiver),
                SyntaxElement::Token(_) => None,
            }) == Some(token_span)
            {
                return Some(IdentifierContext::MemberOwner);
            }
        }
        _ => {}
    }

    node.children.iter().find_map(|child| match child {
        SyntaxElement::Node(child) => syntax_identifier_context(child, token_span),
        SyntaxElement::Token(_) => None,
    })
}

fn identifier_context_accepts_kind(context: IdentifierContext, kind: SymbolKind) -> bool {
    match context {
        IdentifierContext::TypePosition => matches!(
            kind,
            SymbolKind::Class | SymbolKind::Enum | SymbolKind::Typedef | SymbolKind::TypeParameter
        ),
        IdentifierContext::ConstructedType => {
            matches!(kind, SymbolKind::Class | SymbolKind::Constructor)
        }
        IdentifierContext::AttributeType => kind == SymbolKind::Class,
        IdentifierContext::Callable | IdentifierContext::MemberCallable => matches!(
            kind,
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::Constructor
                | SymbolKind::Destructor
        ),
        IdentifierContext::MemberOwner => matches!(
            kind,
            SymbolKind::Class
                | SymbolKind::Enum
                | SymbolKind::Typedef
                | SymbolKind::TypeParameter
                | SymbolKind::GlobalField
                | SymbolKind::Field
                | SymbolKind::Parameter
                | SymbolKind::LocalVariable
        ),
        IdentifierContext::DeclarationName
        | IdentifierContext::MemberAccess
        | IdentifierContext::ValueOrCallable => true,
    }
}

fn type_position_span_is_reliable(
    source: &str,
    symbol: &IndexedSymbol,
    detail_span: TextSpan,
    token_span: TextSpan,
) -> bool {
    if symbol.kind != SymbolKind::LocalVariable {
        return true;
    }

    let boundary_end = symbol.selection_span.start.min(detail_span.end);
    if token_span.end >= boundary_end || boundary_end > source.len() {
        return true;
    }

    let between = &source[token_span.end..boundary_end];
    // A recovered local declaration must not make an unfinished statement on
    // the preceding line type-only. The parser can legally join
    // `GetGam\nWidget root` as one malformed declaration while the user is
    // typing, but that interpretation would hide global callables such as
    // `GetGame()`. Same-line declaration prefixes remain authoritative.
    !between.contains(['\r', '\n'])
}

fn is_type_like_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Enum | SymbolKind::Typedef
    )
}

fn push_unique_id(ids: &mut Vec<GlobalSymbolId>, id: GlobalSymbolId) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        source_category_for_path, SourceFileMetadata, SOURCE_PRIORITY_GAME_DATA,
        SOURCE_PRIORITY_WORKSPACE,
    };
    use crate::parser::parse_source;
    use crate::semantic_file::SemanticFile;

    #[test]
    fn declaration_identifier_resolves_to_itself() {
        let source = "class Example {}";
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve(&index, source, "Example");

        assert_eq!(resolution.token_text, "Example");
        assert_eq!(resolution.reason, ResolutionReason::DeclarationHit);
        assert_eq!(resolution.selected.unwrap().kind, SymbolKind::Class);
    }

    #[test]
    fn local_variable_use_resolves_before_parameter_member_and_top_level() {
        let source = r#"int value;
class Example
{
	int value;
	void Run(int value)
	{
		int value = 4;
		value = value + 1;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "value = value + 1", "value");

        assert_eq!(resolution.reason, ResolutionReason::LocalInCallable);
        let candidates = candidate_kinds_reasons(&resolution);
        assert_eq!(
            candidates[..4],
            [
                (SymbolKind::LocalVariable, ResolutionReason::LocalInCallable),
                (SymbolKind::Parameter, ResolutionReason::ParameterInCallable),
                (SymbolKind::Field, ResolutionReason::ClassMember),
                (SymbolKind::GlobalField, ResolutionReason::TopLevel),
            ]
        );
    }

    #[test]
    fn nearest_preceding_local_shadows_earlier_local_with_same_name() {
        let source = r#"class FirstTarget
{
	int FirstOnly();
}

class SecondTarget
{
	int SecondOnly();
}

class Example
{
	void Run()
	{
		FirstTarget target;
		target.FirstOnly();

		SecondTarget target;
		target.SecondOnly();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "target.SecondOnly", "SecondOnly");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("SecondTarget")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().name.as_deref(),
            Some("SecondOnly")
        );
    }

    #[test]
    fn parameter_use_resolves_before_member_and_top_level_when_no_local_exists() {
        let source = r#"int value;
class Example
{
	int value;
	void Run(int value)
	{
		Print(value);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "Print(value)", "value");

        assert_eq!(resolution.reason, ResolutionReason::ParameterInCallable);
        assert_eq!(resolution.selected.unwrap().kind, SymbolKind::Parameter);
    }

    #[test]
    fn class_member_use_resolves_from_containing_class() {
        let source = r#"class Example
{
	int m_Value;
	void Run()
	{
		m_Value = 4;
		Run();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let field = resolve_at_needle(&index, source, "m_Value = 4", "m_Value");
        let method = resolve_at_needle(&index, source, "Run();", "Run");

        assert_eq!(field.reason, ResolutionReason::ClassMember);
        assert_eq!(field.selected.unwrap().kind, SymbolKind::Field);
        assert_eq!(method.reason, ResolutionReason::ClassMember);
        assert_eq!(method.selected.unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn class_member_use_resolves_field_without_semicolon_before_constructor() {
        let source = r#"class Example
{
	string m_Tag
	void Example(string tag)
	{
		m_Tag = tag;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let field = resolve_at_needle(&index, source, "m_Tag = tag", "m_Tag");

        assert_eq!(field.reason, ResolutionReason::ClassMember);
        assert_eq!(field.selected.unwrap().kind, SymbolKind::Field);
    }

    #[test]
    fn class_member_use_resolves_static_array_field_without_semicolon_before_method() {
        let source = r#"class Example
{
	protected vector m_Target[4]

	//-------------------------------------------------------------------------
	//! Calculates the position.
	protected void Run()
	{
		m_Target[0].ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class vector { string ToString(); }",
            game_metadata("Core/generated/Types/vector.c"),
        );

        let field = resolve_at_needle(&file_index, source, "m_Target[0]", "m_Target");
        assert_eq!(field.reason, ResolutionReason::ClassMember);
        assert_eq!(field.selected.unwrap().kind, SymbolKind::Field);

        let member = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "m_Target[0].ToString",
            "ToString",
        );
        assert_eq!(member.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            member.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("vector")
        );
    }

    #[test]
    fn class_member_use_resolves_field_without_semicolon_before_another_field() {
        let source = r#"class Provider
{
}

class Example
{
	protected Provider m_ProviderComponent
	protected bool m_bEnabled;

	void Run()
	{
		if (m_ProviderComponent)
			m_bEnabled = true;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let provider = resolve_at_needle(
            &index,
            source,
            "if (m_ProviderComponent)",
            "m_ProviderComponent",
        );
        let enabled = resolve_at_needle(&index, source, "m_bEnabled = true", "m_bEnabled");

        assert_eq!(provider.reason, ResolutionReason::ClassMember);
        assert_eq!(provider.selected.unwrap().kind, SymbolKind::Field);
        assert_eq!(enabled.reason, ResolutionReason::ClassMember);
        assert_eq!(enabled.selected.unwrap().kind, SymbolKind::Field);
    }

    #[test]
    fn file_local_top_level_declarations_resolve_by_name() {
        let source = r#"Game g_Game;
typedef string FactionKey;
enum EExample { One = 1 }
void GlobalFn();
class Example
{
	void Run()
	{
		Example local;
		FactionKey key;
		EExample flag;
		GlobalFn();
		g_Game = null;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        assert_eq!(
            resolve_at_needle(&index, source, "Example local", "Example").reason,
            ResolutionReason::TopLevel
        );
        assert_eq!(
            resolve_at_needle(&index, source, "FactionKey key", "FactionKey")
                .selected
                .unwrap()
                .kind,
            SymbolKind::Typedef
        );
        assert_eq!(
            resolve_at_needle(&index, source, "EExample flag", "EExample")
                .selected
                .unwrap()
                .kind,
            SymbolKind::Enum
        );
        assert_eq!(
            resolve_at_needle(&index, source, "GlobalFn();", "GlobalFn")
                .selected
                .unwrap()
                .kind,
            SymbolKind::Function
        );
        assert_eq!(
            resolve_at_needle(&index, source, "g_Game = null", "g_Game")
                .selected
                .unwrap()
                .kind,
            SymbolKind::GlobalField
        );
    }

    #[test]
    fn type_position_prefers_type_declarations_over_constructor_members() {
        let source = r#"class Example
{
	void Example();
	static Example Make()
	{
		Example value;
		array<Example> values;
		Example constructed = new Example();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let return_type = resolve_at_needle(&index, source, "static Example Make", "Example");
        let local_type = resolve_at_needle(&index, source, "Example value", "Example");
        let generic_arg = resolve_at_needle(&index, source, "array<Example> values", "Example");
        let constructor_call = resolve_at_needle(&index, source, "new Example()", "Example");

        assert_eq!(
            return_type.identifier_context,
            IdentifierContext::TypePosition
        );
        assert_eq!(return_type.reason, ResolutionReason::TopLevel);
        assert_eq!(return_type.selected.unwrap().kind, SymbolKind::Class);

        assert_eq!(
            local_type.identifier_context,
            IdentifierContext::TypePosition
        );
        assert_eq!(local_type.selected.unwrap().kind, SymbolKind::Class);

        assert_eq!(
            generic_arg.identifier_context,
            IdentifierContext::TypePosition
        );
        assert_eq!(generic_arg.selected.unwrap().kind, SymbolKind::Class);

        assert_eq!(
            constructor_call.identifier_context,
            IdentifierContext::ConstructedType
        );
        assert_eq!(constructor_call.reason, ResolutionReason::ClassMember);
        assert_eq!(
            constructor_call.selected.unwrap().kind,
            SymbolKind::Constructor
        );
    }

    #[test]
    fn generic_argument_in_new_expression_is_a_type_position() {
        let source = r#"typedef int Callback;
class Box<Class T>
{
	void Box();
}
class Example
{
	void Run()
	{
		Box<Callback> value = new Box<Callback>();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "new Box<Callback>()", "Callback");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::TypePosition
        );
        assert_eq!(resolution.selected.unwrap().kind, SymbolKind::Typedef);
    }

    #[test]
    fn incompatible_symbol_kinds_do_not_resolve_for_syntax_roles() {
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

	void Run(Owner owner, int CollisionParameter)
	{
		int CollisionLocal;
		map<CollisionParameter, CollisionLocal> invalidTypes;
		WrongCall();
		WrongConstructed invalidType = new WrongConstructed();
		WrongStaticOwner.Value;
		owner.WrongMemberCall();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));
        let cases = [
            (
                "[WrongAttribute()]",
                "WrongAttribute",
                IdentifierContext::AttributeType,
            ),
            (
                "map<CollisionParameter",
                "CollisionParameter",
                IdentifierContext::TypePosition,
            ),
            (
                "CollisionLocal> invalidTypes",
                "CollisionLocal",
                IdentifierContext::TypePosition,
            ),
            ("WrongCall();", "WrongCall", IdentifierContext::Callable),
            (
                "new WrongConstructed()",
                "WrongConstructed",
                IdentifierContext::ConstructedType,
            ),
            (
                "WrongStaticOwner.Value",
                "WrongStaticOwner",
                IdentifierContext::MemberOwner,
            ),
            (
                "owner.WrongMemberCall()",
                "WrongMemberCall",
                IdentifierContext::MemberCallable,
            ),
        ];

        for (needle, cursor, expected_context) in cases {
            let resolution = resolve_at_needle(&index, source, needle, cursor);
            assert_eq!(resolution.identifier_context, expected_context);
            assert!(
                resolution.selected.is_none(),
                "an incompatible same-name symbol resolved for {cursor}: {resolution:#?}"
            );
            assert!(
                resolution.candidates.is_empty(),
                "incompatible candidates survived for {cursor}: {resolution:#?}"
            );
        }
    }

    #[test]
    fn base_type_position_prefers_file_local_class() {
        let source = r#"class Base {}
class Child : Base {}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "class Child : Base", "Base");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::TypePosition
        );
        assert_eq!(resolution.reason, ResolutionReason::TopLevel);
        assert_eq!(resolution.selected.unwrap().kind, SymbolKind::Class);
    }

    #[test]
    fn external_index_resolves_missing_type_name() {
        let source = r#"class Example
{
	void Run()
	{
		ExternalType value;
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_source = "class ExternalType {}";
        let external_index =
            index_for_source(external_source, game_metadata("Game/ExternalType.c"));

        let offset = offset_for_needle(source, "ExternalType value", "ExternalType");
        let resolution = ReferenceResolver::new(source, &file_index, Some(&external_index))
            .resolve_at_offset(offset)
            .unwrap();

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::TypePosition
        );
        assert_eq!(resolution.reason, ResolutionReason::ExternalPreferred);
        let selected = resolution.selected.unwrap();
        assert_eq!(selected.source, CandidateSource::External);
        assert_eq!(selected.kind, SymbolKind::Class);
        assert_eq!(selected.source_kind, SourceKind::GameData);
    }

    #[test]
    fn local_variable_receiver_resolves_external_member() {
        let source = r#"class Example
{
	void Run()
	{
		IEntity ent;
		ent.GetOrigin();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class IEntity { vector GetOrigin(); }",
            game_metadata("Game/IEntity.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "ent.GetOrigin",
            "GetOrigin",
        );

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().source,
            CandidateSource::External
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("IEntity")
        );
    }

    #[test]
    fn auto_local_call_default_infers_receiver_type_from_method_return() {
        let source = r#"class Physics
{
	float GetMass();
}

class IEntity
{
	Physics GetPhysics();
}

class Example
{
	void Run(IEntity owner)
	{
		auto physics = owner.GetPhysics();
		physics.GetMass();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Physics.c"));

        let resolution = resolve_at_needle(&index, source, "physics.GetMass", "GetMass");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("Physics")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn auto_local_external_function_chain_infers_receiver_type() {
        let source = r#"class Example
{
	void Run()
	{
		auto game = GetGame();
		auto menuManager = game.GetMenuManager();
		menuManager.OpenMenu();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Menu.c"));
        let external_index = index_for_source(
            r#"class MenuManager
{
	void OpenMenu();
}

class Game
{
	MenuManager GetMenuManager();
}

class ArmaReforgerScripted : Game
{
}

ArmaReforgerScripted GetGame();
"#,
            game_metadata("Game/generated/Game.c"),
        );

        let game_member = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "game.GetMenuManager",
            "GetMenuManager",
        );
        assert_eq!(game_member.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            game_member.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("ArmaReforgerScripted")
        );

        let menu_member = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "menuManager.OpenMenu",
            "OpenMenu",
        );
        assert_eq!(menu_member.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            menu_member.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("MenuManager")
        );
    }

    #[test]
    fn parameter_receiver_resolves_file_local_member() {
        let source = r#"class Component
{
	int GetResourceValue();
}

class Example
{
	void Run(Component comp)
	{
		comp.GetResourceValue();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution =
            resolve_at_needle(&index, source, "comp.GetResourceValue", "GetResourceValue");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().source,
            CandidateSource::FileLocal
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn field_receiver_strips_collection_type_to_owner() {
        let source = r#"class Example
{
	ref array<int> m_Values;
	void Run()
	{
		m_Values.Insert(4);
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class array { void Insert(int value); }",
            game_metadata("Core/proto/Types.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "m_Values.Insert",
            "Insert",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("array")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().source,
            CandidateSource::External
        );
    }

    #[test]
    fn this_receiver_resolves_containing_class_members() {
        let source = r#"class Example
{
	void DoThing();
	void Run()
	{
		this.DoThing();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "this.DoThing", "DoThing");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn super_receiver_resolves_base_class_members() {
        let source = r#"class Base
{
	void OnInit();
}

class Example : Base
{
	void Run()
	{
		super.OnInit();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "super.OnInit", "OnInit");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("Base")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn static_receiver_prefers_static_member_when_available() {
        let source = r#"class IEntity {}
class SCR_BaseGameMode
{
	static SCR_BaseGameMode Cast(IEntity entity);
	void Cast();
}

class Example
{
	void Run(IEntity entity)
	{
		SCR_BaseGameMode.Cast(entity);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "SCR_BaseGameMode.Cast", "Cast");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::StaticMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("SCR_BaseGameMode")
        );
    }

    #[test]
    fn static_enum_member_resolves_to_enum_member_child() {
        let source = r#"enum RplChannel
{
	Reliable = 0,
	Unreliable = 1
}

class Example
{
	void Run()
	{
		RplChannel.Reliable;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "RplChannel.Reliable", "Reliable");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberAccess
        );
        assert_eq!(resolution.reason, ResolutionReason::StaticMember);
        let selected = resolution.selected.as_ref().unwrap();
        assert_eq!(selected.kind, SymbolKind::EnumMember);
        assert_eq!(selected.name.as_deref(), Some("Reliable"));
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("RplChannel")
        );
    }

    #[test]
    fn static_enum_member_resolves_from_base_enum() {
        let source = r#"enum EHitZoneGroup
{
	VIRTUAL,
}

enum ECharacterHitZoneGroup : EHitZoneGroup
{
	HEAD,
}

class Example
{
	void Run()
	{
		ECharacterHitZoneGroup.VIRTUAL;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution =
            resolve_at_needle(&index, source, "ECharacterHitZoneGroup.VIRTUAL", "VIRTUAL");

        assert_eq!(resolution.reason, ResolutionReason::StaticMember);
        let selected = resolution.selected.as_ref().unwrap();
        assert_eq!(selected.kind, SymbolKind::EnumMember);
        assert_eq!(selected.name.as_deref(), Some("VIRTUAL"));
    }

    #[test]
    fn static_enum_member_resolves_through_typedef_alias() {
        let source = r#"enum WorldSystemPoint
{
	Frame,
	FixedFrame,
}

typedef WorldSystemPoint ESystemPoint;

class Example
{
	void Run()
	{
		ESystemPoint.Frame;
		ESystemPoint.FixedFrame;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("WorldSystemPoint.c"));

        let frame = resolve_at_needle(&index, source, "ESystemPoint.Frame", "Frame");
        assert_eq!(frame.reason, ResolutionReason::StaticMember);
        let selected = frame.selected.as_ref().unwrap();
        assert_eq!(selected.kind, SymbolKind::EnumMember);
        assert_eq!(selected.name.as_deref(), Some("Frame"));

        let fixed_frame =
            resolve_at_needle(&index, source, "ESystemPoint.FixedFrame", "FixedFrame");
        assert_eq!(fixed_frame.reason, ResolutionReason::StaticMember);
        let selected = fixed_frame.selected.as_ref().unwrap();
        assert_eq!(selected.kind, SymbolKind::EnumMember);
        assert_eq!(selected.name.as_deref(), Some("FixedFrame"));
    }

    #[test]
    fn type_cast_uses_engine_class_cast_rule() {
        let source = r#"class IEntity {}
class Class
{
	static Class Cast(Class from);
}
class SCR_Foo {}

class Example
{
	void Run(IEntity entity)
	{
		SCR_Foo.Cast(entity);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "SCR_Foo.Cast", "Cast");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::EngineClassCast);
        let selected = resolution.selected.as_ref().unwrap();
        assert_eq!(selected.kind, SymbolKind::Method);
        assert_eq!(selected.name.as_deref(), Some("Cast"));
        assert_eq!(resolution.receiver.as_ref().unwrap().failure_reason, None);
    }

    #[test]
    fn pseudo_class_member_rule_resolves_instance_members() {
        let source = r#"class ExampleType {}

class Example
{
	void Run(ExampleType test)
	{
		test.ClassName();
		test.Type();
		test.IsInherited(ExampleType);
		test.ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"class Class
{
	proto native external bool IsInherited(typename type);
	proto native owned external string ClassName();
	proto native external typename Type();
	proto external string ToString();
}
"#,
            game_metadata("Core/proto/Types.c"),
        );

        for (needle, cursor) in [
            ("test.ClassName", "ClassName"),
            ("test.Type", "Type"),
            ("test.IsInherited", "IsInherited"),
            ("test.ToString", "ToString"),
        ] {
            let resolution = resolve_at_needle_with_external(
                &file_index,
                &external_index,
                source,
                needle,
                cursor,
            );

            assert_eq!(resolution.reason, ResolutionReason::PseudoClassMember);
            assert_eq!(
                resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
                Some("ExampleType")
            );
            let selected = resolution.selected.as_ref().unwrap();
            assert_eq!(selected.source, CandidateSource::External);
            assert_eq!(selected.kind, SymbolKind::Method);
            assert_eq!(selected.name.as_deref(), Some(cursor));
        }
    }

    #[test]
    fn concrete_member_beats_pseudo_class_member_rule() {
        let source = r#"class ExampleType
{
	string ToString();
}

class Example
{
	void Run(ExampleType test)
	{
		test.ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"class Class
{
	proto external string ToString();
}
"#,
            game_metadata("Core/proto/Types.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "test.ToString",
            "ToString",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        let selected = resolution.selected.as_ref().unwrap();
        assert_eq!(selected.source, CandidateSource::FileLocal);
        assert_eq!(selected.name.as_deref(), Some("ToString"));
    }

    #[test]
    fn unqualified_pseudo_call_resolves_to_class_member() {
        let source = r#"class Example
{
	void Run()
	{
		Type().ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"class Class
{
	proto native external typename Type();
	proto external string ToString();
}
"#,
            game_metadata("Core/proto/Types.c"),
        );

        let type_resolution =
            resolve_at_needle_with_external(&file_index, &external_index, source, "Type()", "Type");
        assert_eq!(type_resolution.reason, ResolutionReason::PseudoClassMember);
        assert_eq!(
            type_resolution.selected.as_ref().unwrap().source,
            CandidateSource::External
        );

        let to_string_resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "Type().ToString",
            "ToString",
        );
        assert_eq!(
            to_string_resolution.reason,
            ResolutionReason::PseudoClassMember
        );
        assert_eq!(
            to_string_resolution
                .receiver
                .as_ref()
                .unwrap()
                .owner_type
                .as_deref(),
            Some("typename")
        );
        assert_eq!(
            to_string_resolution.selected.as_ref().unwrap().source,
            CandidateSource::External
        );
    }

    #[test]
    fn enum_member_receiver_can_use_pseudo_class_members() {
        let source = r#"enum ExampleKind
{
	One,
}

class Example
{
	void Run()
	{
		ExampleKind.One.ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"class Class
{
	proto external string ToString();
}
"#,
            game_metadata("Core/proto/Types.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "ExampleKind.One.ToString",
            "ToString",
        );

        assert_eq!(resolution.reason, ResolutionReason::PseudoClassMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("ExampleKind")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().source,
            CandidateSource::External
        );
    }

    #[test]
    fn cast_receiver_infers_cast_target_for_next_member() {
        let source = r#"class IEntity {}
class SCR_Foo
{
	static SCR_Foo Cast(IEntity entity);
	void Run();
}

class Example
{
	void Test(IEntity entity)
	{
		SCR_Foo.Cast(entity).Run();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "SCR_Foo.Cast(entity).Run", "Run");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("SCR_Foo")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn function_call_receiver_uses_return_type() {
        let source = r#"class World {}
class Game
{
	World GetWorld();
}
Game GetGame();

class Example
{
	void Run()
	{
		GetGame().GetWorld();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "GetGame().GetWorld", "GetWorld");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("Game")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn external_function_call_receiver_uses_inherited_return_type_chain() {
        let source = r#"class Example
{
	void Run()
	{
		GetGame().GetWorld().GetWorldTime();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"class BaseWorld
{
	float GetWorldTime();
}

class World : BaseWorld
{
}

class Game
{
	World GetWorld();
}

class ChimeraGame : Game
{
}

class ArmaReforgerScripted : ChimeraGame
{
}

ArmaReforgerScripted GetGame();
"#,
            game_metadata("Game/game.c"),
        );

        let get_world = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "GetGame().GetWorld",
            "GetWorld",
        );
        assert_eq!(get_world.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            get_world.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("ArmaReforgerScripted")
        );
        assert_eq!(
            get_world.selected.as_ref().unwrap().name.as_deref(),
            Some("GetWorld")
        );

        let get_world_time = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "GetWorld().GetWorldTime",
            "GetWorldTime",
        );
        assert_eq!(get_world_time.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            get_world_time
                .receiver
                .as_ref()
                .unwrap()
                .owner_type
                .as_deref(),
            Some("World")
        );
        assert_eq!(
            get_world_time.selected.as_ref().unwrap().name.as_deref(),
            Some("GetWorldTime")
        );
    }

    #[test]
    fn external_function_call_receiver_chain_works_in_local_initializer() {
        let source = r#"class Example
{
	void Run()
	{
		float current = GetGame().GetWorld().GetWorldTime();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"class BaseWorld
{
	float GetWorldTime();
}

class World : BaseWorld
{
}

class Game
{
	World GetWorld();
}

class ChimeraGame : Game
{
}

class ArmaReforgerScripted : ChimeraGame
{
}

ArmaReforgerScripted GetGame();
"#,
            game_metadata("Game/game.c"),
        );

        let get_world = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "GetGame().GetWorld",
            "GetWorld",
        );
        assert_eq!(get_world.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            get_world.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("ArmaReforgerScripted")
        );

        let get_world_time = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "GetWorld().GetWorldTime",
            "GetWorldTime",
        );
        assert_eq!(get_world_time.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            get_world_time
                .receiver
                .as_ref()
                .unwrap()
                .owner_type
                .as_deref(),
            Some("World")
        );
    }

    #[test]
    fn receiver_span_stops_at_control_statement_condition() {
        let source = r#"class Widget
{
	void RemoveFromHierarchy();
}

class Example
{
	Widget m_wHint;
	void Run()
	{
		if (m_wHint)
			m_wHint.RemoveFromHierarchy();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(
            &index,
            source,
            "m_wHint.RemoveFromHierarchy",
            "RemoveFromHierarchy",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        let receiver = resolution.receiver.as_ref().unwrap();
        assert_eq!(receiver.receiver_text, "m_wHint");
        assert_eq!(receiver.owner_type.as_deref(), Some("Widget"));
        assert_eq!(receiver.failure_reason, None);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn receiver_span_stops_at_return_keyword_for_static_enum_members() {
        let source = r#"enum ENodeResult
{
	FAIL,
	SUCCESS,
}

class Example
{
	ENodeResult Run()
	{
		return ENodeResult.FAIL;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "return ENodeResult.FAIL", "FAIL");

        assert_eq!(resolution.reason, ResolutionReason::StaticMember);
        let receiver = resolution.receiver.as_ref().unwrap();
        assert_eq!(receiver.receiver_text, "ENodeResult");
        assert_eq!(receiver.owner_type.as_deref(), Some("ENodeResult"));
        assert_eq!(receiver.failure_reason, None);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::EnumMember
        );
    }

    #[test]
    fn receiver_span_stops_at_binary_operators() {
        let source = r#"enum TraceFlags
{
	WORLD,
	ENTS,
}

class Example
{
	void Run()
	{
		int flags = TraceFlags.WORLD | TraceFlags.ENTS;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let world = resolve_at_needle(&index, source, "TraceFlags.WORLD", "WORLD");
        let ents = resolve_at_needle(&index, source, "TraceFlags.ENTS", "ENTS");

        assert_eq!(world.reason, ResolutionReason::StaticMember);
        assert_eq!(world.receiver.as_ref().unwrap().receiver_text, "TraceFlags");
        assert_eq!(
            world.selected.as_ref().unwrap().kind,
            SymbolKind::EnumMember
        );
        assert_eq!(ents.reason, ResolutionReason::StaticMember);
        assert_eq!(ents.receiver.as_ref().unwrap().receiver_text, "TraceFlags");
        assert_eq!(ents.selected.as_ref().unwrap().kind, SymbolKind::EnumMember);
    }

    #[test]
    fn receiver_span_stops_at_call_argument_boundaries() {
        let source = r#"class Math
{
	static float RandomFloatInclusive(float min, float max);
	static float Max(float first, float second);
}

class Example
{
	void Run(float value)
	{
		float random = Math.RandomFloatInclusive(Math.Max(0, value), value);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(
            &index,
            source,
            "Math.RandomFloatInclusive",
            "RandomFloatInclusive",
        );

        assert_eq!(resolution.reason, ResolutionReason::StaticMember);
        let receiver = resolution.receiver.as_ref().unwrap();
        assert_eq!(receiver.receiver_text, "Math");
        assert_eq!(receiver.owner_type.as_deref(), Some("Math"));
        assert_eq!(receiver.failure_reason, None);
    }

    #[test]
    fn receiver_span_preserves_valid_call_chains() {
        let source = r#"class World {}
class Game
{
	World GetWorld();
}
Game GetGame();

class Example
{
	void Run()
	{
		GetGame().GetWorld();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "GetGame().GetWorld", "GetWorld");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        let receiver = resolution.receiver.as_ref().unwrap();
        assert_eq!(receiver.receiver_text, "GetGame()");
        assert_eq!(receiver.owner_type.as_deref(), Some("Game"));
        assert_eq!(receiver.failure_reason, None);
    }

    #[test]
    fn unqualified_call_resolves_inherited_member() {
        let source = r#"class Base
{
	protected void SendCancelMessagesToAllAgents();
}

class Example : Base
{
	void Run()
	{
		SendCancelMessagesToAllAgents();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(
            &index,
            source,
            "\t\tSendCancelMessagesToAllAgents();",
            "SendCancelMessagesToAllAgents",
        );

        assert_eq!(resolution.reason, ResolutionReason::ClassMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn unqualified_call_resolves_external_inherited_member() {
        let source = r#"class Example : Node
{
	void Run()
	{
		GetVariableIn("Value", value);
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class Node { proto bool GetVariableIn(string name, out void val); }",
            game_metadata("Game/generated/AI/Node.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "GetVariableIn(",
            "GetVariableIn",
        );

        assert_eq!(resolution.reason, ResolutionReason::ClassMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().source,
            CandidateSource::External
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn script_invoker_typedef_receiver_resolves_base_members() {
        let source = r#"class Example
{
	void Run(ScriptInvoker inv, func fn)
	{
		inv.Insert(fn);
		inv.Remove(fn);
		inv.Invoke();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"class ScriptInvokerBase<Class T>: Managed
{
	proto void Invoke();
	proto void Insert(T fn);
	proto void Remove(T fn);
}
typedef ScriptInvokerBase<func> ScriptInvoker;
"#,
            game_metadata("GameLib/tools.c"),
        );

        for (needle, cursor) in [
            ("inv.Insert", "Insert"),
            ("inv.Remove", "Remove"),
            ("inv.Invoke", "Invoke"),
        ] {
            let resolution = resolve_at_needle_with_external(
                &file_index,
                &external_index,
                source,
                needle,
                cursor,
            );
            assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
            assert_eq!(
                resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
                Some("ScriptInvoker")
            );
            assert_eq!(
                resolution.selected.as_ref().unwrap().source,
                CandidateSource::External
            );
            assert_eq!(
                resolution.selected.as_ref().unwrap().kind,
                SymbolKind::Method
            );
        }
    }

    #[test]
    fn array_index_receiver_uses_element_type() {
        let source = r#"class Example
{
	void Run(array<IEntity> items)
	{
		items[0].GetOrigin();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class IEntity { vector GetOrigin(); }",
            game_metadata("Game/generated/Entities/IEntity.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "items[0].GetOrigin",
            "GetOrigin",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("IEntity")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn foreach_auto_variable_uses_iterable_element_type() {
        let source = r#"class Example
{
	ref array<ref FilterEntry> m_aFilters;
	void Run()
	{
		foreach (auto f : m_aFilters)
		{
			f.GetSelected();
		}
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class FilterEntry { bool GetSelected(); }",
            game_metadata("Game/UI/Menu/Common/SCR_FilterSet.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "f.GetSelected",
            "GetSelected",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("FilterEntry")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn map_index_receiver_uses_value_type() {
        let source = r#"class Example
{
	void Run(map<string, Widget> widgets, string key)
	{
		widgets[key].SetVisible(true);
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class Widget { void SetVisible(bool visible); }",
            game_metadata("Core/generated/UI/Widget.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "widgets[key].SetVisible",
            "SetVisible",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("Widget")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn map_get_return_substitutes_value_type_for_receiver_chain() {
        let source = r#"class map<Class TKey, Class TValue>
{
	TValue Get(TKey key);
}

class TextWidget
{
	void SetText(string value);
}

enum EStats
{
	Distance
}

class Example
{
	void Run(map<EStats, TextWidget> widgets)
	{
		widgets.Get(EStats.Distance).SetText("ok");
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("MapGet.c"));

        let resolution =
            resolve_at_needle(&index, source, "Get(EStats.Distance).SetText", "SetText");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("TextWidget")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn map_get_return_substitutes_nested_set_type_for_receiver_chain() {
        let source = r#"class map<Class TKey, Class TValue>
{
	TValue Get(TKey key);
}

class set<Class T>
{
	void Insert(T value);
}

class Example
{
	void Run(map<string, ref set<string>> changes, string category, string action)
	{
		changes.Get(category).Insert(action);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("MapSet.c"));

        let resolution = resolve_at_needle(&index, source, "Get(category).Insert", "Insert");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("set")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn generic_field_chain_substitutes_wrapped_field_type() {
        let source = r#"class AIWaypoint
{
	string ToString();
}

class SCR_BTParam<Class T>
{
	T m_Value;
}

class SCR_AIDefendBehavior
{
	ref SCR_BTParam<AIWaypoint> m_RelatedWaypoint;

	void Run()
	{
		m_RelatedWaypoint.m_Value.ToString();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("BtParamField.c"));

        let resolution = resolve_at_needle(&index, source, "m_Value.ToString", "ToString");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("AIWaypoint")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn generic_static_cast_preserves_raw_type_for_field_chain_substitution() {
        let source = r#"class Class
{
	static Class Cast(Class from);
}

class Tuple2<Class T1, Class T2>
{
	T1 param1;
	T2 param2;
}

class UserActionEvent
{
	ref array<IEntity> m_aUserEntities;
}

class array<Class T>
{
	void Insert(T value);
}

class IEntity {}

class Example
{
	void Run(Class context, IEntity entity)
	{
		auto entityContext = Tuple2<UserActionEvent, bool>.Cast(context);
		entityContext.param1.m_aUserEntities.Insert(entity);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("TupleCast.c"));

        let param1 = resolve_at_needle(&index, source, "entityContext.param1", "param1");
        assert_eq!(param1.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            param1.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("Tuple2")
        );
        let selected = param1.selected.as_ref().unwrap();
        assert_eq!(selected.kind, SymbolKind::Field);
        assert_eq!(selected.name.as_deref(), Some("param1"));

        let insert = resolve_at_needle(&index, source, "m_aUserEntities.Insert", "Insert");
        assert_eq!(insert.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            insert.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("array")
        );
        let selected = insert.selected.as_ref().unwrap();
        assert_eq!(selected.kind, SymbolKind::Method);
        assert_eq!(selected.name.as_deref(), Some("Insert"));
    }

    #[test]
    fn vector_index_receiver_uses_float_type() {
        let source = r#"class Example
{
	void Run(vector matrix)
	{
		matrix[0].ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class float { string ToString(); }",
            game_metadata("Core/generated/Types/float.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "matrix[0].ToString",
            "ToString",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("float")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn string_index_receiver_keeps_string_type() {
        let source = r#"class Example
{
	void Run(string time)
	{
		time[0].ToInt();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class string { int ToInt(); }",
            game_metadata("Core/generated/Types/string.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "time[0].ToInt",
            "ToInt",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("string")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn static_array_parameter_index_receiver_uses_element_type() {
        let source = r#"class Example
{
	void Run(float quat[4])
	{
		quat[2].ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class float { string ToString(); }",
            game_metadata("Core/generated/Types/float.c"),
        );

        let resolution = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "quat[2].ToString",
            "ToString",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("float")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn auto_local_new_expression_infers_receiver_type() {
        let source = r#"class SCR_AICharacterStanceSetting_Range
{
	void Init();
	void VerifyStanceValues();
}

class Example
{
	void Run()
	{
		auto s = new SCR_AICharacterStanceSetting_Range();
		s.Init();
		s.VerifyStanceValues();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        for (needle, member_name) in [
            ("s.Init", "Init"),
            ("s.VerifyStanceValues", "VerifyStanceValues"),
        ] {
            let resolution = resolve_at_needle(&index, source, needle, member_name);
            assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
            assert_eq!(
                resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
                Some("SCR_AICharacterStanceSetting_Range")
            );
            assert_eq!(
                resolution.selected.as_ref().unwrap().kind,
                SymbolKind::Method
            );
        }
    }

    #[test]
    fn direct_new_expression_receiver_infers_receiver_type() {
        let source = r#"class SCR_AIAnimateBehavior
{
	array<string> GetPortNames();
}

class Example
{
	void Run()
	{
		(new SCR_AIAnimateBehavior()).GetPortNames();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("NewReceiver.c"));

        let resolution = resolve_at_needle(&index, source, ")).GetPortNames", "GetPortNames");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("SCR_AIAnimateBehavior")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn auto_local_cast_expression_infers_receiver_type() {
        let source = r#"class GenericComponent {}

class SCR_CharacterCameraHandlerComponent
{
	void SetThirdPerson(bool value);
	static SCR_CharacterCameraHandlerComponent Cast(GenericComponent component);
}

class Example
{
	void Run(GenericComponent component)
	{
		auto cameraHandler = SCR_CharacterCameraHandlerComponent.Cast(component);
		cameraHandler.SetThirdPerson(true);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("CameraHandler.c"));

        let resolution = resolve_at_needle(
            &index,
            source,
            "cameraHandler.SetThirdPerson",
            "SetThirdPerson",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("SCR_CharacterCameraHandlerComponent")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn local_without_semicolon_before_expression_statement_is_extracted() {
        let source = r#"enum ETreeSoundTypes
{
	Leafy
}

class Example
{
	string GetTreeSoundEventName(float height, ETreeSoundTypes treeType);
	void GetTreeProperties(out ETreeSoundTypes treeType, out float height);

	void Run()
	{
		float foliageHeight;
		ETreeSoundTypes treeSoundType
		GetTreeProperties(treeSoundType, foliageHeight);
		string eventName = GetTreeSoundEventName(foliageHeight, treeSoundType);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let local = resolve_at_needle(
            &index,
            source,
            "GetTreeSoundEventName(foliageHeight, treeSoundType)",
            "treeSoundType",
        );

        assert_eq!(local.reason, ResolutionReason::LocalInCallable);
        assert_eq!(local.selected.unwrap().kind, SymbolKind::LocalVariable);
    }

    #[test]
    fn comma_locals_without_semicolon_before_expression_statement_are_extracted() {
        let source = r#"class FuelManager
{
	void GetTotalValuesOfFuelNodes(out float totalFuel, out float totalMaxFuel, out float totalFuelPercentage);
}

class Example
{
	FuelManager m_FuelManager;

	void Run()
	{
		float totalFuel, totalMaxFuel, totalFuelPercentage
		m_FuelManager.GetTotalValuesOfFuelNodes(totalFuel, totalMaxFuel, totalFuelPercentage);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Fuel.c"));

        for name in ["totalFuel", "totalMaxFuel", "totalFuelPercentage"] {
            let resolution = resolve_at_needle(
                &index,
                source,
                "GetTotalValuesOfFuelNodes(totalFuel, totalMaxFuel, totalFuelPercentage)",
                name,
            );

            assert_eq!(resolution.reason, ResolutionReason::LocalInCallable);
            assert_eq!(
                resolution.selected.as_ref().unwrap().kind,
                SymbolKind::LocalVariable
            );
        }
    }

    #[test]
    fn parenthesized_numeric_receiver_uses_primitive_type() {
        let source = r#"class Example
{
	int m_iKickTimeout;
	void Run()
	{
		(m_iKickTimeout / 60).ToString();
		(10 / 2).ToString();
	}
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            "class int { string ToString(); }",
            game_metadata("Core/generated/Types/int.c"),
        );

        for needle in ["(m_iKickTimeout / 60).ToString", "(10 / 2).ToString"] {
            let resolution = resolve_at_needle_with_external(
                &file_index,
                &external_index,
                source,
                needle,
                "ToString",
            );

            assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
            assert_eq!(
                resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
                Some("int")
            );
            assert_eq!(
                resolution.selected.as_ref().unwrap().kind,
                SymbolKind::Method
            );
        }
    }

    #[test]
    fn chained_field_receiver_uses_each_field_type() {
        let source = r#"class TextWidget
{
	void SetText(string value);
}

class WidgetBundle
{
	TextWidget m_wTitle;
}

class Example
{
	WidgetBundle m_Widgets;
	void Run()
	{
		m_Widgets.m_wTitle.SetText("ok");
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "m_wTitle.SetText", "SetText");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("TextWidget")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn unresolved_receiver_does_not_fall_back_to_containing_symbol() {
        let source = r#"class Example
{
	void Run()
	{
		missing.GetWorld();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(&index, source, "missing.GetWorld", "GetWorld");

        assert_eq!(
            resolution.identifier_context,
            IdentifierContext::MemberCallable
        );
        assert_eq!(resolution.reason, ResolutionReason::ReceiverUnresolved);
        assert!(resolution.selected.is_none());
        assert!(resolution.candidates.is_empty());
        assert!(resolution
            .receiver
            .as_ref()
            .unwrap()
            .failure_reason
            .is_some());
    }

    #[test]
    fn non_identifier_tokens_do_not_resolve() {
        let source = r#"class Example
{
	void Run()
	{
		// comment value
		string text = "value";
		if (true) {}
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        assert_no_resolution(&index, source, "class Example", "class");
        assert_no_resolution(&index, source, "// comment value", "value");
        assert_no_resolution(&index, source, "\"value\"", "value");
        assert_no_resolution(&index, source, "(true)", "(");
    }

    #[test]
    fn named_argument_labels_are_not_resolved_as_symbols() {
        let source = r#"class LogLevel
{
	static LogLevel WARNING;
}

void Print(string text, LogLevel level);

class Example
{
	void Run()
	{
		Print("hello", level: LogLevel.WARNING);
		PrintFormat("Invalid resource path for autotest config: %1", path, level: LogLevel.WARNING);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let label = resolve_at_needle(&index, source, "level: LogLevel", "level");
        assert_eq!(label.reason, ResolutionReason::NamedArgumentLabel);
        assert_eq!(label.selected, None);
        assert!(label.candidates.is_empty());

        let nested_label = resolve_at_needle(&index, source, "path, level: LogLevel", "level");
        assert_eq!(nested_label.reason, ResolutionReason::NamedArgumentLabel);
        assert_eq!(nested_label.selected, None);
        assert!(nested_label.candidates.is_empty());

        let value = resolve_at_needle(&index, source, "level: LogLevel", "LogLevel");
        assert_eq!(value.reason, ResolutionReason::TopLevel);
        assert_eq!(value.selected.as_ref().unwrap().kind, SymbolKind::Class);
    }

    #[test]
    fn preprocessor_tokens_are_classified_as_non_symbol_targets() {
        let source = r#"#ifdef ENABLE_DIAG
class Example
{
}
#endif
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let directive = resolve_at_needle(&index, source, "#ifdef ENABLE_DIAG", "ifdef");
        assert_eq!(directive.reason, ResolutionReason::PreprocessorDirective);
        assert_eq!(directive.selected, None);
        assert!(directive.candidates.is_empty());

        let macro_name = resolve_at_needle(&index, source, "#ifdef ENABLE_DIAG", "ENABLE_DIAG");
        assert_eq!(macro_name.reason, ResolutionReason::PreprocessorMacro);
        assert_eq!(macro_name.selected, None);
        assert!(macro_name.candidates.is_empty());

        let endif = resolve_at_needle(&index, source, "#endif", "endif");
        assert_eq!(endif.reason, ResolutionReason::PreprocessorDirective);
        assert_eq!(endif.selected, None);
        assert!(endif.candidates.is_empty());
    }

    #[test]
    fn attribute_named_arguments_are_classified_as_non_symbol_targets() {
        let source = r#"class UIWidgets
{
	static UIWidgets ComboBox;
}

class Example
{
	[Attribute("", UIWidgets.ComboBox, desc: "Display text", params: "et")]
	int value;
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let desc = resolve_at_needle(&index, source, "desc: \"Display text\"", "desc");
        assert_eq!(desc.reason, ResolutionReason::AttributeNamedArgument);
        assert_eq!(desc.selected, None);
        assert!(desc.candidates.is_empty());

        let params = resolve_at_needle(&index, source, "params: \"et\"", "params");
        assert_eq!(params.reason, ResolutionReason::AttributeNamedArgument);
        assert_eq!(params.selected, None);
        assert!(params.candidates.is_empty());

        let value = resolve_at_needle(&index, source, "UIWidgets.ComboBox", "ComboBox");
        assert_eq!(value.reason, ResolutionReason::StaticMember);
        assert_eq!(value.selected.as_ref().unwrap().kind, SymbolKind::Field);
    }

    #[test]
    fn parameter_default_member_expressions_resolve() {
        let source = r#"class Example
{
	void Run(string prefix = string.Empty, NamingConvention namingConvention = NamingConvention.NC_MUST_HAVE_GUID);
}
"#;
        let file_index = index_for_source(source, workspace_metadata("Example.c"));
        let external_index = index_for_source(
            r#"sealed class string
{
	static const string Empty;
}

enum NamingConvention
{
	NC_MUST_HAVE_GUID
}
"#,
            game_metadata("Core/generated/Types.c"),
        );

        let empty = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "string.Empty",
            "Empty",
        );
        assert_eq!(empty.reason, ResolutionReason::StaticMember);
        assert_eq!(empty.selected.as_ref().unwrap().kind, SymbolKind::Field);

        let enum_member = resolve_at_needle_with_external(
            &file_index,
            &external_index,
            source,
            "NamingConvention.NC_MUST_HAVE_GUID",
            "NC_MUST_HAVE_GUID",
        );
        assert_eq!(enum_member.reason, ResolutionReason::StaticMember);
        assert_eq!(
            enum_member.selected.as_ref().unwrap().kind,
            SymbolKind::EnumMember
        );
    }

    #[test]
    fn field_initializer_member_expressions_resolve() {
        let source = r#"class SCR_AIActionTask
{
	static const string WAYPOINT_RELATED_PORT;
}

class Example
{
	ref SCR_BTParam<bool> m_bIsWaypointRelated = new SCR_BTParam<bool>(SCR_AIActionTask.WAYPOINT_RELATED_PORT);
}
"#;
        let index = index_for_source(source, workspace_metadata("Example.c"));

        let resolution = resolve_at_needle(
            &index,
            source,
            "SCR_AIActionTask.WAYPOINT_RELATED_PORT",
            "WAYPOINT_RELATED_PORT",
        );

        assert_eq!(resolution.reason, ResolutionReason::StaticMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Field
        );
    }

    #[test]
    fn class_type_parameters_resolve_in_type_positions() {
        let source = r#"class map<Class TKey,Class TValue>: Managed
{
	proto TValue Get(TKey key);
}
"#;
        let index = index_for_source(source, workspace_metadata("Core/proto/Types.c"));

        let value = resolve_at_needle(&index, source, "TValue Get", "TValue");
        assert_eq!(value.reason, ResolutionReason::TypeParameter);
        assert_eq!(
            value.selected.as_ref().unwrap().kind,
            SymbolKind::TypeParameter
        );

        let key = resolve_at_needle(&index, source, "TKey key", "TKey");
        assert_eq!(key.reason, ResolutionReason::TypeParameter);
        assert_eq!(
            key.selected.as_ref().unwrap().kind,
            SymbolKind::TypeParameter
        );
    }

    #[test]
    fn class_type_parameters_resolve_as_value_identifiers() {
        let source = r#"class WeightedArray<Class TValue>
{
	void Run()
	{
		PrintFormat("%1", TValue);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("WeightedArray.c"));

        let resolution = resolve_at_needle(&index, source, "TValue);", "TValue");

        assert_eq!(resolution.reason, ResolutionReason::TypeParameter);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::TypeParameter
        );
    }

    #[test]
    fn class_type_parameter_can_be_static_cast_receiver() {
        let source = r#"class Class
{
	proto external static void Cast(Managed object);
}

class Widget: Managed {}

class SCR_SpinningWidgetAnimation<Widget TWidget>
{
	TWidget m_wTarget;
	void Run(Widget w)
	{
		m_wTarget = TWidget.Cast(w);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("GenericWidget.c"));

        let resolution = resolve_at_needle(&index, source, "TWidget.Cast(w)", "Cast");

        assert_eq!(resolution.reason, ResolutionReason::EngineClassCast);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("TWidget")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn enum_member_value_static_member_expressions_resolve() {
        let source = r#"enum EPlatform
{
	XBOX_ONE_X,
}

enum SCR_EPlatform
{
	XBOX_ONE_X = 1 << EPlatform.XBOX_ONE_X,
}
"#;
        let index = index_for_source(source, workspace_metadata("Enums.c"));

        let resolution = resolve_at_needle(&index, source, "EPlatform.XBOX_ONE_X", "XBOX_ONE_X");

        assert_eq!(resolution.reason, ResolutionReason::StaticMember);
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::EnumMember
        );
    }

    #[test]
    fn foreach_generic_variable_preserves_type_for_tuple_member_lookup() {
        let source = r#"class Tuple2<Class T1, Class T2>
{
	T1 param1;
	T2 param2;
}

class Example
{
	void Run(array<ref Tuple2<vector, vector>> areas)
	{
		foreach (Tuple2<vector, vector> area: areas)
		{
			RequestNavmeshRebuild(area.param1, area.param2);
		}
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("TupleUse.c"));

        let resolution = resolve_at_needle(&index, source, "area.param1", "param1");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("Tuple2")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Field
        );
    }

    #[test]
    fn generic_base_owner_members_are_visible_for_receiver_lookup() {
        let source = r#"class array<Class T>
{
	void InsertAt(T value, int index);
}

class ParamEnum {}
class ParamEnumArray: array<ref ParamEnum> {}

class Example
{
	void Run(ParamEnumArray params, ParamEnum value)
	{
		params.InsertAt(value, 0);
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("ParamEnumArray.c"));

        let resolution = resolve_at_needle(&index, source, "params.InsertAt", "InsertAt");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverMember);
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("ParamEnumArray")
        );
        assert_eq!(
            resolution.selected.as_ref().unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn unconstrained_generic_receiver_does_not_guess_member_by_name() {
        let source = r#"class Class {}

class SCR_ResourcePlayerControllerInventoryComponent
{
	void RequestUnsubscription();
}

class Example<Class OWNER_TYPE>
{
	OWNER_TYPE m_Owner;

	void Run()
	{
		m_Owner.RequestUnsubscription();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("GenericOwner.c"));

        let resolution = resolve_at_needle(
            &index,
            source,
            "m_Owner.RequestUnsubscription",
            "RequestUnsubscription",
        );

        assert_eq!(resolution.reason, ResolutionReason::ReceiverUnresolved);
        assert!(resolution.selected.is_none());
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("OWNER_TYPE")
        );
    }

    #[test]
    fn unconstrained_generic_field_chain_does_not_guess_member_by_name() {
        let source = r#"class Class {}

class Wrapper<Class T>
{
	T m_Value;
}

class SmartActionComponent
{
	void ReleaseAction();
}

class Example<Class T>
{
	Wrapper<T> m_SmartActionComponent;

	void Run()
	{
		m_SmartActionComponent.m_Value.ReleaseAction();
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("GenericValue.c"));

        let resolution =
            resolve_at_needle(&index, source, "m_Value.ReleaseAction", "ReleaseAction");

        assert_eq!(resolution.reason, ResolutionReason::ReceiverUnresolved);
        assert!(resolution.selected.is_none());
        assert_eq!(
            resolution.receiver.as_ref().unwrap().owner_type.as_deref(),
            Some("T")
        );
    }

    #[test]
    fn ambiguous_same_name_candidates_are_preserved_with_selected_first() {
        let source = r#"class Shared {}
typedef string Shared;
void Shared();
class Example
{
	void Run()
	{
		Shared value;
	}
}
"#;
        let index = index_for_source(source, workspace_metadata("Shared.c"));

        let resolution = resolve_at_needle(&index, source, "Shared value", "Shared");

        assert_eq!(resolution.reason, ResolutionReason::TopLevel);
        assert_eq!(resolution.candidates.len(), 2);
        assert_eq!(resolution.selected.unwrap().kind, SymbolKind::Class);
        assert!(resolution
            .candidates
            .iter()
            .any(|candidate| candidate.kind == SymbolKind::Typedef));
        assert!(!resolution
            .candidates
            .iter()
            .any(|candidate| candidate.kind == SymbolKind::Function));
    }

    fn resolve(index: &SymbolIndex, source: &str, needle: &str) -> ReferenceResolution {
        let offset = source.find(needle).unwrap();
        ReferenceResolver::new(source, index, None)
            .resolve_at_offset(offset)
            .unwrap()
    }

    fn resolve_at_needle(
        index: &SymbolIndex,
        source: &str,
        needle: &str,
        cursor: &str,
    ) -> ReferenceResolution {
        let offset = offset_for_needle(source, needle, cursor);
        ReferenceResolver::new(source, index, None)
            .resolve_at_offset(offset)
            .unwrap()
    }

    fn resolve_at_needle_with_external(
        index: &SymbolIndex,
        external_index: &SymbolIndex,
        source: &str,
        needle: &str,
        cursor: &str,
    ) -> ReferenceResolution {
        let offset = offset_for_needle(source, needle, cursor);
        ReferenceResolver::new(source, index, Some(external_index))
            .resolve_at_offset(offset)
            .unwrap()
    }

    fn assert_no_resolution(index: &SymbolIndex, source: &str, needle: &str, cursor: &str) {
        let offset = offset_for_needle(source, needle, cursor);
        assert_eq!(
            ReferenceResolver::new(source, index, None).resolve_at_offset(offset),
            None
        );
    }

    fn candidate_kinds_reasons(
        resolution: &ReferenceResolution,
    ) -> Vec<(SymbolKind, ResolutionReason)> {
        resolution
            .candidates
            .iter()
            .map(|candidate| (candidate.kind, candidate.reason))
            .collect()
    }

    fn offset_for_needle(source: &str, needle: &str, cursor: &str) -> usize {
        let start = source.find(needle).unwrap();
        let cursor_start = needle.find(cursor).unwrap();
        start + cursor_start
    }

    fn index_for_source(source: &str, metadata: SourceFileMetadata) -> SymbolIndex {
        let parse = parse_source(source);
        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        let semantic_file = SemanticFile::build(source, &parse);
        SymbolIndex::from_semantic_files([(&semantic_file, metadata)])
    }

    fn workspace_metadata(path: &str) -> SourceFileMetadata {
        SourceFileMetadata {
            kind: SourceKind::Workspace,
            category: SourceCategory::Workspace,
            absolute_path: Some(PathBuf::from("C:/workspace").join(path)),
            virtual_source: None,
            root_path: Some(PathBuf::from("C:/workspace")),
            relative_path: Some(PathBuf::from(path)),
            priority: SOURCE_PRIORITY_WORKSPACE,
        }
    }

    fn game_metadata(path: &str) -> SourceFileMetadata {
        let relative_path = PathBuf::from(path);
        SourceFileMetadata {
            kind: SourceKind::GameData,
            category: source_category_for_path(SourceKind::GameData, Some(&relative_path)),
            absolute_path: Some(PathBuf::from("C:/game").join(path)),
            virtual_source: None,
            root_path: Some(PathBuf::from("C:/game")),
            relative_path: Some(relative_path),
            priority: SOURCE_PRIORITY_GAME_DATA,
        }
    }
}
