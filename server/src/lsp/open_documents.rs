use crate::analysis_runtime::{DocumentSnapshot, PositionIndex};
use crate::ast::{ClassMember, Declaration, MethodDecl};
use crate::index::SymbolIndex;
use crate::lexer::{lex, Keyword, TextSpan, Token, TokenKind};
use crate::model::{SourceFileMetadata, SymbolKind};
use crate::parser::parse_lexed_source;
use crate::scope::LexicalScopeModel;
use crate::semantic_file::SemanticFile;
use crate::syntax::Parse;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;

use super::scope_delimiters::{
    scope_delimiters_for_syntax, ScopeDelimiter, MAX_ACTIVE_SCOPE_DELIMITER_SOURCE_BYTES,
};
use super::semantic_tokens::{LspSemanticTokenProjection, RichSemanticProjectionCache};
use super::LspDocumentSymbol;

pub(crate) struct OpenDocument {
    /// The runtime-owned immutable source identity.  Analysis and caches below
    /// are derived state only and may never outlive this revision.
    pub(crate) snapshot: DocumentSnapshot,
    /// Foreground-only query facts for this exact revision. Semantic work is
    /// deliberately not allowed to manufacture or replace this state.
    foreground: Option<ForegroundQuerySnapshot>,
    analysis: Option<FileIndexAnalysis>,
    analysis_timings: Option<FileIndexAnalysisTimings>,
    analysis_rejected: bool,
    document_symbols: Vec<LspDocumentSymbol>,
    document_symbols_ready: bool,
    pub(crate) semantic_tokens: SemanticTokenCache,
    rich_projection_cache: Option<Arc<RichSemanticProjectionCache>>,
}

impl OpenDocument {
    pub(crate) fn new(snapshot: DocumentSnapshot) -> Self {
        let revision = snapshot.revision();
        let mut document = Self::pending(snapshot);
        // Deterministic in-process fixtures construct ready documents without
        // the production executor. The transport path never calls this: it
        // installs the same state through a `TaskClass::Foreground` worker.
        let positions = PositionIndex::new(document.snapshot.text());
        let syntax = Arc::new(DocumentSyntax::new(document.snapshot.text()));
        assert!(document.install_foreground(revision, positions, syntax.clone()));
        let (analysis, analysis_timings) = file_index_for_syntax(document.snapshot.text(), syntax);
        assert!(document.install_analysis(revision, analysis, analysis_timings));
        document
    }

    /// Creates a cache whose source snapshot is authoritative but whose
    /// compiler analysis has not run yet. Feature dispatch must therefore use
    /// only a foreground-safe projection until the worker installs this
    /// revision; no legacy empty-file analysis exists in this state.
    pub(crate) fn pending(snapshot: DocumentSnapshot) -> Self {
        Self {
            snapshot,
            foreground: None,
            analysis: None,
            analysis_timings: None,
            analysis_rejected: false,
            document_symbols: Vec::new(),
            document_symbols_ready: false,
            semantic_tokens: SemanticTokenCache::default(),
            rich_projection_cache: None,
        }
    }

    pub(crate) fn replace(&mut self, snapshot: DocumentSnapshot) {
        self.snapshot = snapshot;
        self.foreground = None;
        self.analysis = None;
        self.analysis_timings = None;
        self.analysis_rejected = false;
        self.document_symbols.clear();
        self.document_symbols_ready = false;
        self.semantic_tokens.cancel_pending();
        // The current rich worker may replace the cache before VS Code asks
        // for this revision. If the request wins that race, it installs the
        // lexical fallback internally and still waits for rich publication.
    }

    pub(crate) fn analysis_ready(&self) -> bool {
        self.analysis.is_some()
    }

    pub(crate) fn syntax(&self) -> Option<&Parse> {
        self.syntax_snapshot().map(|syntax| &syntax.parse)
    }

    pub(crate) fn syntax_snapshot(&self) -> Option<&Arc<DocumentSyntax>> {
        self.foreground
            .as_ref()
            .map(|foreground| &foreground.syntax)
    }

    pub(crate) fn parse_diagnostic_count(&self) -> usize {
        self.syntax().map_or(0, |parse| parse.diagnostics.len())
    }

    pub(crate) fn install_foreground(
        &mut self,
        revision: u64,
        positions: PositionIndex,
        syntax: Arc<DocumentSyntax>,
    ) -> bool {
        if revision != self.snapshot.revision() {
            return false;
        }
        // A duplicate foreground event is harmless only when the exact
        // revision already owns its immutable position table.
        if !self.snapshot.install_positions(positions) && self.snapshot.positions().is_none() {
            return false;
        }
        self.foreground = Some(ForegroundQuerySnapshot::build(self.snapshot.text(), syntax));
        true
    }

    pub(crate) fn foreground_ready(&self) -> bool {
        self.foreground.is_some() && self.snapshot.positions().is_some()
    }

    pub(crate) fn foreground(&self) -> Option<&ForegroundQuerySnapshot> {
        self.foreground.as_ref()
    }

    pub(crate) fn analysis(&self) -> &FileIndexAnalysis {
        self.analysis
            .as_ref()
            .expect("ready analysis is required by this feature path")
    }

    pub(crate) fn analysis_timings(&self) -> FileIndexAnalysisTimings {
        self.analysis_timings
            .expect("ready analysis timings are required by this feature path")
    }

    pub(crate) fn mark_analysis_pending(&mut self) {
        self.analysis_rejected = false;
    }

    /// Marks the matching revision unavailable after deterministic runtime
    /// overload. Request dispatch must respond rather than retaining an
    /// unbounded deferred request that can never be replayed.
    pub(crate) fn reject_pending_analysis(&mut self) {
        self.analysis_rejected = true;
    }

    pub(crate) fn analysis_rejected(&self) -> bool {
        self.analysis_rejected
    }

    pub(crate) fn install_analysis(
        &mut self,
        revision: u64,
        analysis: FileIndexAnalysis,
        analysis_timings: FileIndexAnalysisTimings,
    ) -> bool {
        if revision != self.snapshot.revision() {
            return false;
        }
        self.analysis = Some(analysis);
        self.analysis_timings = Some(analysis_timings);
        self.analysis_rejected = false;
        true
    }

    pub(crate) fn set_document_symbols(&mut self, symbols: Vec<LspDocumentSymbol>) {
        self.document_symbols = symbols;
        self.document_symbols_ready = true;
    }

    pub(crate) fn document_symbols(&self) -> &[LspDocumentSymbol] {
        &self.document_symbols
    }

    pub(crate) fn document_symbols_ready(&self) -> bool {
        self.document_symbols_ready
    }

    pub(crate) fn rich_projection_cache(&self) -> Option<Arc<RichSemanticProjectionCache>> {
        self.rich_projection_cache.clone()
    }

    pub(crate) fn install_rich_projection_cache(
        &mut self,
        revision: u64,
        cache: RichSemanticProjectionCache,
    ) -> bool {
        if revision != self.snapshot.revision() {
            return false;
        }
        self.rich_projection_cache = Some(Arc::new(cache));
        true
    }

    pub(crate) fn rebind_rich_projection_cache_external_generation(
        &mut self,
        previous_generation: u64,
        external_generation: u64,
    ) -> bool {
        self.rich_projection_cache.as_mut().is_some_and(|cache| {
            Arc::make_mut(cache).rebind_external_generation(
                self.snapshot.revision(),
                previous_generation,
                external_generation,
            )
        })
    }
}

/// The complete token state that may be published for one document revision.
///
/// A lexical baseline is materialized only when an editor request outruns the
/// current rich worker. It is derived solely from the current snapshot text
/// and remains an internal fallback while that request waits. The server only
/// supports full semantic-token responses today, so a changed overlay receives
/// a new opaque result id rather than attempting an unsafe delta against a
/// former result.
pub(crate) struct TokenSnapshot {
    revision: u64,
    lexical_baseline: Option<LspSemanticTokenProjection>,
    rich_overlay: Option<RichTokenOverlay>,
}

struct RichTokenOverlay {
    external_generation: u64,
    workspace_excludes_document: bool,
    rich_elapsed_ms: u128,
    projection: LspSemanticTokenProjection,
}

pub(crate) enum SelfSaveRichPreservation {
    Ready { reference_elapsed_ms: u128 },
    Pending,
}

pub(crate) struct RichProjectionPublication {
    pub(crate) external_generation: u64,
    pub(crate) self_save_retargeted: bool,
}

impl SelfSaveRichPreservation {
    #[cfg(test)]
    pub(crate) const fn state(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::Pending => "pending",
        }
    }

    #[cfg(test)]
    pub(crate) const fn reference_elapsed_ms(&self) -> u128 {
        match self {
            Self::Ready {
                reference_elapsed_ms,
            } => *reference_elapsed_ms,
            Self::Pending => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenResultDisposition {
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenProjectionKind {
    LexicalBaseline,
    RichOverlay,
}

pub(crate) struct TokenSelection<'a> {
    pub(crate) projection: &'a LspSemanticTokenProjection,
    pub(crate) kind: TokenProjectionKind,
    pub(crate) result_id: String,
    pub(crate) disposition: TokenResultDisposition,
}

impl TokenSnapshot {
    fn new(revision: u64) -> Self {
        Self {
            revision,
            lexical_baseline: None,
            rich_overlay: None,
        }
    }

    fn select(&self, external_generation: u64) -> TokenSelection<'_> {
        if let Some(overlay) = self
            .rich_overlay
            .as_ref()
            .filter(|overlay| overlay.external_generation == external_generation)
        {
            return TokenSelection {
                projection: &overlay.projection,
                kind: TokenProjectionKind::RichOverlay,
                result_id: format!("reforger:{}:rich:{}", self.revision, external_generation),
                disposition: TokenResultDisposition::Full,
            };
        }
        TokenSelection {
            projection: self
                .lexical_baseline
                .as_ref()
                .expect("selection requires a lexical or rich projection"),
            kind: TokenProjectionKind::LexicalBaseline,
            result_id: format!("reforger:{}:lexical", self.revision),
            disposition: TokenResultDisposition::Full,
        }
    }

    fn set_rich(
        &mut self,
        external_generation: u64,
        workspace_excludes_document: bool,
        rich_elapsed_ms: u128,
        projection: LspSemanticTokenProjection,
    ) {
        self.rich_overlay = Some(RichTokenOverlay {
            external_generation,
            workspace_excludes_document,
            rich_elapsed_ms,
            projection,
        });
    }
}

#[derive(Default)]
pub(crate) struct SemanticTokenCache {
    snapshot: Option<TokenSnapshot>,
    pending: Option<PendingRichProjection>,
}

struct PendingRichProjection {
    revision: u64,
    task_external_generation: u64,
    publish_external_generation: u64,
    workspace_excludes_document: bool,
    cancel: Arc<AtomicBool>,
}

impl SemanticTokenCache {
    #[cfg(test)]
    pub(crate) fn has_rich_for_revision(&self, revision: u64) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.revision == revision && snapshot.rich_overlay.is_some()
        })
    }

    pub(crate) fn select_or_insert_lexical(
        &mut self,
        revision: u64,
        external_generation: u64,
        lexical_baseline: impl FnOnce() -> LspSemanticTokenProjection,
    ) -> TokenSelection<'_> {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.revision != revision)
        {
            self.snapshot = Some(TokenSnapshot::new(revision));
        }
        self.discard_rich_for_other_external_generation(external_generation);
        let snapshot = self
            .snapshot
            .as_mut()
            .expect("semantic token snapshot was just installed");
        if snapshot.rich_overlay.is_none() && snapshot.lexical_baseline.is_none() {
            snapshot.lexical_baseline = Some(lexical_baseline());
        }
        self.snapshot
            .as_ref()
            .expect("semantic token snapshot was just installed")
            .select(external_generation)
    }

    #[cfg(test)]
    pub(crate) fn rich_for_revision_and_external_generation(
        &self,
        revision: u64,
        external_generation: u64,
    ) -> Option<&LspSemanticTokenProjection> {
        self.snapshot
            .as_ref()
            .filter(|snapshot| snapshot.revision == revision)
            .and_then(|snapshot| snapshot.rich_overlay.as_ref())
            .filter(|overlay| overlay.external_generation == external_generation)
            .map(|overlay| &overlay.projection)
    }

    pub(crate) fn set_rich(
        &mut self,
        revision: u64,
        external_generation: u64,
        workspace_excludes_document: bool,
        rich_elapsed_ms: u128,
        projection: LspSemanticTokenProjection,
    ) {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.revision != revision)
        {
            self.snapshot = Some(TokenSnapshot::new(revision));
        }
        self.snapshot
            .as_mut()
            .expect("semantic token snapshot was just installed")
            .set_rich(
                external_generation,
                workspace_excludes_document,
                rich_elapsed_ms,
                projection,
            );
        self.pending = None;
    }

    pub(crate) fn pending_for_revision_and_external_generation(
        &self,
        revision: u64,
        external_generation: u64,
    ) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.revision == revision
                && pending.publish_external_generation == external_generation
        })
    }

    pub(crate) fn needs_rich_projection(&self, revision: u64, external_generation: u64) -> bool {
        self.snapshot.as_ref().is_none_or(|snapshot| {
            snapshot.revision != revision
                || snapshot
                    .rich_overlay
                    .as_ref()
                    .is_none_or(|overlay| overlay.external_generation != external_generation)
        }) && !self.pending_for_revision_and_external_generation(revision, external_generation)
    }

    pub(crate) fn mark_pending(
        &mut self,
        revision: u64,
        external_generation: u64,
        workspace_excludes_document: bool,
        cancel: Arc<AtomicBool>,
    ) {
        self.cancel_pending();
        self.pending = Some(PendingRichProjection {
            revision,
            task_external_generation: external_generation,
            publish_external_generation: external_generation,
            workspace_excludes_document,
            cancel,
        });
    }

    pub(crate) fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn cancel_pending_task(
        &mut self,
        revision: u64,
        task_external_generation: u64,
    ) -> Option<u64> {
        let publish_external_generation = self.pending.as_ref().and_then(|pending| {
            (pending.revision == revision
                && pending.task_external_generation == task_external_generation)
                .then_some(pending.publish_external_generation)
        })?;
        self.cancel_pending();
        Some(publish_external_generation)
    }

    pub(crate) fn cancel_pending_for_other_external_generation(
        &mut self,
        external_generation: u64,
    ) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.publish_external_generation != external_generation)
        {
            self.cancel_pending();
        }
    }

    pub(crate) fn publish_generation_for_ready_task(
        &self,
        revision: u64,
        task_external_generation: u64,
        current_external_generation: u64,
        workspace_excludes_document: bool,
    ) -> Option<RichProjectionPublication> {
        if task_external_generation == current_external_generation {
            return Some(RichProjectionPublication {
                external_generation: current_external_generation,
                self_save_retargeted: false,
            });
        }
        self.pending.as_ref().and_then(|pending| {
            (pending.revision == revision
                && pending.task_external_generation == task_external_generation
                && pending.publish_external_generation == current_external_generation
                && pending.workspace_excludes_document
                && workspace_excludes_document)
                .then_some(RichProjectionPublication {
                    external_generation: current_external_generation,
                    self_save_retargeted: true,
                })
        })
    }

    /// An external-index generation is part of a rich overlay's identity.
    /// Keeping an unmatched overlay would be harmless at selection time, but
    /// dropping it here makes the cache's retained state match its publishable
    /// state and prevents an old overlay from becoming eligible again.
    pub(crate) fn discard_rich_for_other_external_generation(&mut self, external_generation: u64) {
        if self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .rich_overlay
                .as_ref()
                .is_some_and(|overlay| overlay.external_generation != external_generation)
        }) {
            self.snapshot
                .as_mut()
                .expect("snapshot was checked above")
                .rich_overlay = None;
        }
    }

    /// Carries a rich projection across the one workspace generation created
    /// by saving its own source file. This is valid only when the projection
    /// was built from a workspace view that excluded that file, so the
    /// generation change cannot alter any of its semantic inputs.
    pub(crate) fn rebind_self_save_rich_generation(
        &mut self,
        revision: u64,
        previous_external_generation: u64,
        external_generation: u64,
    ) -> Option<SelfSaveRichPreservation> {
        if let Some(overlay) = self
            .snapshot
            .as_mut()
            .filter(|snapshot| snapshot.revision == revision)
            .and_then(|snapshot| snapshot.rich_overlay.as_mut())
            .filter(|overlay| {
                overlay.external_generation == previous_external_generation
                    && overlay.workspace_excludes_document
            })
        {
            overlay.external_generation = external_generation;
            return Some(SelfSaveRichPreservation::Ready {
                reference_elapsed_ms: overlay.rich_elapsed_ms,
            });
        }
        let Some(pending) = self.pending.as_mut().filter(|pending| {
            pending.revision == revision
                && pending.publish_external_generation == previous_external_generation
                && pending.workspace_excludes_document
        }) else {
            return None;
        };
        pending.publish_external_generation = external_generation;
        Some(SelfSaveRichPreservation::Pending)
    }
}

/// Immutable, current-revision facts built exclusively by the foreground
/// worker. Pending request handlers may query these facts, but must not lex,
/// parse, or walk the CST/AST themselves.
#[derive(Clone)]
pub(crate) struct ForegroundQuerySnapshot {
    syntax: Arc<DocumentSyntax>,
    scope_delimiters: Vec<ScopeDelimiter>,
    top_level_declarations: Vec<ForegroundTopLevelDeclaration>,
    callable_declarations: Vec<ForegroundCallableDeclaration>,
}

#[derive(Clone, Debug)]
pub(crate) struct ForegroundTopLevelDeclaration {
    pub(crate) name: String,
    pub(crate) name_span: TextSpan,
    pub(crate) kind: SymbolKind,
}

#[derive(Clone, Debug)]
pub(crate) struct ForegroundCallableDeclaration {
    pub(crate) name: String,
    pub(crate) signature: String,
}

impl ForegroundQuerySnapshot {
    pub(crate) fn build(source: &str, syntax: Arc<DocumentSyntax>) -> Self {
        let tokens = &syntax.lexer_tokens;
        let parse = &syntax.parse;
        let scope_delimiters = if source.len() <= MAX_ACTIVE_SCOPE_DELIMITER_SOURCE_BYTES {
            scope_delimiters_for_syntax(parse, tokens)
        } else {
            Vec::new()
        };
        Self {
            top_level_declarations: foreground_top_level_declarations(source, tokens),
            callable_declarations: foreground_callable_declarations(source, parse),
            syntax,
            scope_delimiters,
        }
    }

    pub(crate) fn token_at_offset(&self, offset: usize) -> Option<Token> {
        self.syntax
            .lexer_tokens
            .binary_search_by(|token| {
                if token.span.end <= offset {
                    std::cmp::Ordering::Less
                } else if token.span.start > offset {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()
            .and_then(|index| self.syntax.lexer_tokens.get(index).copied())
    }

    pub(crate) fn top_level_declaration_at_offset(
        &self,
        offset: usize,
    ) -> Option<&ForegroundTopLevelDeclaration> {
        self.top_level_declarations.iter().find(|declaration| {
            declaration.name_span.start <= offset && offset <= declaration.name_span.end
        })
    }

    pub(crate) fn callable_declarations_named<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a ForegroundCallableDeclaration> + 'a {
        self.callable_declarations
            .iter()
            .filter(move |candidate| candidate.name == name)
    }

    pub(crate) fn tokens(&self) -> &[Token] {
        &self.syntax.lexer_tokens
    }

    pub(crate) fn scope_delimiters(&self) -> &[ScopeDelimiter] {
        &self.scope_delimiters
    }
}

fn foreground_top_level_declarations(
    source: &str,
    tokens: &[Token],
) -> Vec<ForegroundTopLevelDeclaration> {
    let mut declarations = Vec::new();
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
                if let Some((name, name_span, next_index)) =
                    foreground_top_level_declaration(source, tokens, index, declaration_kind)
                {
                    let kind = match declaration_kind {
                        TokenKind::Keyword(Keyword::Class) => SymbolKind::Class,
                        TokenKind::Keyword(Keyword::Enum) => SymbolKind::Enum,
                        TokenKind::Keyword(Keyword::Typedef) => SymbolKind::Typedef,
                        _ => unreachable!("only declaration keywords reach this branch"),
                    };
                    declarations.push(ForegroundTopLevelDeclaration {
                        name,
                        name_span,
                        kind,
                    });
                    index = next_index;
                    continue;
                }
            }
            _ => {}
        }
        index += 1;
    }
    declarations
}

fn foreground_top_level_declaration(
    source: &str,
    tokens: &[Token],
    keyword_index: usize,
    declaration_kind: TokenKind,
) -> Option<(String, TextSpan, usize)> {
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
                        token.span,
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
                            name.span,
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

fn foreground_callable_declarations(
    source: &str,
    parse: &Parse,
) -> Vec<ForegroundCallableDeclaration> {
    // The parser owns the canonical typed declaration traversal.  Keep the
    // foreground snapshot on that CST path rather than reinstating an
    // `AstSourceFile` facade solely for pending signature help.
    parse
        .declaration_iter(source)
        .flat_map(|declaration| match declaration {
            Declaration::Function(method) => vec![method],
            Declaration::Class(class) => class
                .members()
                .into_iter()
                .filter_map(|member| match member {
                    ClassMember::Method(method) => Some(method),
                    ClassMember::Field(_) | ClassMember::Empty(_) => None,
                })
                .collect(),
            Declaration::Enum(_) | Declaration::Typedef(_) | Declaration::Field(_) => Vec::new(),
        })
        .filter_map(foreground_callable_declaration)
        .collect()
}

fn foreground_callable_declaration(
    method: MethodDecl<'_, '_>,
) -> Option<ForegroundCallableDeclaration> {
    if method.is_destructor() || !method.parameter_fragments().is_empty() {
        return None;
    }
    let name = method.name()?.text().to_string();
    let return_type = method.return_type_text()?.text().trim().to_string();
    if return_type.is_empty() {
        return None;
    }
    let parameters = method
        .parameters()
        .into_iter()
        .map(|parameter| {
            parameter
                .text()
                .map(|text| text.text().trim().to_string())
                .filter(|text| !text.is_empty())
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ForegroundCallableDeclaration {
        signature: format!("{return_type} {name}({})", parameters.join(", ")),
        name,
    })
}

/// One immutable lexical/syntax allocation shared by the foreground and
/// semantic stages for a captured document revision.
pub(crate) struct DocumentSyntax {
    pub(crate) parse: Parse,
    pub(crate) lexer_tokens: Vec<Token>,
}

impl DocumentSyntax {
    pub(crate) fn new(source: &str) -> Self {
        Self::from_tokens(source, lex(source))
    }

    pub(crate) fn from_tokens(source: &str, lexer_tokens: Vec<Token>) -> Self {
        Self {
            parse: parse_lexed_source(source, &lexer_tokens),
            lexer_tokens,
        }
    }
}

#[derive(Clone)]
pub struct FileIndexAnalysis {
    pub(crate) syntax: Arc<DocumentSyntax>,
    /// Immutable compiler-owned declaration and local-binding facts. The
    /// index and lexical scope below are feature compatibility projections of
    /// this semantic authority, never independent declaration discovery.
    pub(crate) semantic: SemanticFile,
    pub(crate) index: SymbolIndex,
    pub(crate) scope: LexicalScopeModel,
    pub(crate) parse_diagnostics: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FileIndexAnalysisTimings {
    pub(crate) parse_ms: u128,
    pub(crate) catalog_ms: u128,
    pub(crate) index_ms: u128,
    pub(crate) scope_ms: u128,
    pub(crate) total_ms: u128,
}

pub fn file_index_for_source(source: &str) -> FileIndexAnalysis {
    file_index_for_source_with_timings(source).0
}

fn file_index_for_source_with_timings(
    source: &str,
) -> (FileIndexAnalysis, FileIndexAnalysisTimings) {
    let total_start = Instant::now();
    let lexer_tokens = lex(source);
    let parse_start = Instant::now();
    let syntax = Arc::new(DocumentSyntax::from_tokens(source, lexer_tokens));
    let parse_ms = parse_start.elapsed().as_millis();
    let (analysis, mut timings) = file_index_for_syntax(source, syntax);
    timings.parse_ms = parse_ms;
    timings.total_ms = total_start.elapsed().as_millis();
    (analysis, timings)
}

pub(crate) fn file_index_for_syntax(
    source: &str,
    syntax: Arc<DocumentSyntax>,
) -> (FileIndexAnalysis, FileIndexAnalysisTimings) {
    let total_start = Instant::now();
    let parse = &syntax.parse;
    let parse_diagnostics = parse.diagnostics.len();
    let catalog_start = Instant::now();
    let semantic_file = SemanticFile::build(source, parse);
    let catalog_ms = catalog_start.elapsed().as_millis();
    let index_start = Instant::now();
    let mut index = SymbolIndex::default();
    let local_file_id = index.add_semantic_file(
        &semantic_file,
        SourceFileMetadata {
            kind: crate::model::SourceKind::Workspace,
            category: crate::model::SourceCategory::Workspace,
            absolute_path: None,
            virtual_source: None,
            root_path: None,
            relative_path: None,
            priority: crate::model::SOURCE_PRIORITY_WORKSPACE,
        },
    );
    let index_ms = index_start.elapsed().as_millis();
    let scope_start = Instant::now();
    let scope =
        LexicalScopeModel::from_parse_and_semantics(parse, &semantic_file, &index, local_file_id);
    let scope_ms = scope_start.elapsed().as_millis();
    (
        FileIndexAnalysis {
            syntax,
            semantic: semantic_file,
            index,
            scope,
            parse_diagnostics,
        },
        FileIndexAnalysisTimings {
            parse_ms: 0,
            catalog_ms,
            index_ms,
            scope_ms,
            total_ms: total_start.elapsed().as_millis(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_runtime::DocumentStore;
    use crate::lsp::definition::definition_report_for_pending_snapshot;
    use crate::lsp::hover::hover_report_for_pending_snapshot;
    use crate::lsp::signature_help::signature_help_report_for_pending_snapshot;
    use crate::lsp::LspPosition;
    use crate::parser::parse_source;

    #[test]
    fn file_analysis_tokenizes_source_once() {
        let source = "class Example { void Run() { int value = 1; } }";
        let before = crate::lexer::test_lex_call_count();
        let analysis = file_index_for_source(source);
        assert_eq!(crate::lexer::test_lex_call_count() - before, 1);
        assert_eq!(analysis.syntax.parse, parse_source(source));
        assert_eq!(analysis.syntax.lexer_tokens, lex(source));
    }

    #[test]
    fn foreground_and_semantics_share_one_revision_and_release_old_syntax() {
        let mut store = DocumentStore::new();
        let uri = "file:///Shared.c";
        for source in ["class Valid {}", "class Broken { void Run( {"] {
            store.upsert(uri, 1, source);
            let snapshot = store.latest(uri).unwrap();
            let revision = snapshot.revision();
            let before = crate::lexer::test_lex_call_count();
            let mut document = OpenDocument::new(snapshot);
            assert_eq!(crate::lexer::test_lex_call_count() - before, 1);
            assert!(Arc::ptr_eq(
                document.syntax_snapshot().unwrap(),
                &document.analysis().syntax
            ));
            assert_eq!(
                document.parse_diagnostic_count(),
                document.analysis().parse_diagnostics
            );
            let old_syntax = document.syntax_snapshot().unwrap().clone();
            let old_weak = Arc::downgrade(&old_syntax);
            store.upsert(uri, 2, "class Current {}");
            document.replace(store.latest(uri).unwrap());
            assert!(!document.install_foreground(
                revision,
                PositionIndex::new(source),
                old_syntax.clone()
            ));
            let (old_analysis, timings) = file_index_for_syntax(source, old_syntax.clone());
            assert!(!document.install_analysis(revision, old_analysis, timings));
            assert!(!document.foreground_ready());
            drop(old_syntax);
            assert!(old_weak.upgrade().is_none());
            store = DocumentStore::new();
        }
    }

    #[test]
    #[ignore = "manual timing/allocation comparison; run with --ignored --nocapture"]
    fn profile_shared_document_syntax() {
        let unit =
            include_str!("../../../tools/fixtures/semantic/semantic_scale_declaration_unit.c");
        let source: String = (0..2_000)
            .map(|i| unit.replace("SemanticScaleFixture", &format!("Fixture{i}")))
            .collect();
        for shared in [false, true] {
            let mut samples = Vec::new();
            for _ in 0..8 {
                let ((foreground_us, total_us, positions, foreground, analysis), memory) =
                    crate::test_allocations::measure(|| {
                        let start = Instant::now();
                        let positions = PositionIndex::new(&source);
                        let syntax = Arc::new(DocumentSyntax::new(&source));
                        let foreground = ForegroundQuerySnapshot::build(&source, syntax.clone());
                        let foreground_us = start.elapsed().as_micros();
                        let analysis = if shared {
                            file_index_for_syntax(&source, syntax).0
                        } else {
                            file_index_for_source(&source)
                        };
                        (
                            foreground_us,
                            start.elapsed().as_micros(),
                            positions,
                            foreground,
                            analysis,
                        )
                    });
                assert_eq!(Arc::ptr_eq(&foreground.syntax, &analysis.syntax), shared);
                samples.push((foreground_us, total_us, memory));
                drop((positions, foreground, analysis));
            }
            samples.remove(0); // Warm allocator/code before comparing seven samples.
            samples.sort_by_key(|sample| sample.1);
            let sample = samples[3];
            println!("shared={shared} source_bytes={} foreground_us={} total_us={} allocations={} retained_bytes={} peak_bytes={}",
                source.len(), sample.0, sample.1, sample.2.allocation_calls, sample.2.retained_bytes, sample.2.peak_bytes);
        }
    }

    #[test]
    fn pending_request_projections_do_not_relex_or_rewalk_a_large_snapshot() {
        let mut source = String::from(
            "// pending lexical hover\nclass Pending { void Current(string value) {} void Run() { Current( ); } }\n",
        );
        source.push_str(&"// unrelated foreground payload\n".repeat(40_000));

        let mut store = DocumentStore::new();
        assert_eq!(
            store.upsert("file:///Scripts/Pending.c", 1, source.as_str()),
            crate::analysis_runtime::UpsertOutcome::Accepted
        );
        let snapshot = store.latest("file:///Scripts/Pending.c").unwrap();
        assert!(snapshot.install_positions(PositionIndex::new(snapshot.text())));
        let syntax = Arc::new(DocumentSyntax::new(snapshot.text()));
        let parse = &syntax.parse;
        let foreground = ForegroundQuerySnapshot::build(snapshot.text(), syntax.clone());
        let lex_calls_before_requests = crate::lexer::test_lex_call_count();

        let comment = hover_report_for_pending_snapshot(
            &snapshot,
            &foreground,
            LspPosition {
                line: 0,
                character: 4,
            },
            parse.diagnostics.len(),
        );
        assert!(comment.is_hit());

        let definition = definition_report_for_pending_snapshot(
            &snapshot,
            &foreground,
            snapshot.uri(),
            LspPosition {
                line: 1,
                character: 7,
            },
            parse.diagnostics.len(),
        );
        assert!(definition.is_hit());

        let signature = signature_help_report_for_pending_snapshot(
            &snapshot,
            &foreground,
            parse.diagnostics.len(),
            LspPosition {
                line: 1,
                character: 67,
            },
        );
        assert!(signature.help.is_some());
        assert_eq!(
            crate::lexer::test_lex_call_count(),
            lex_calls_before_requests
        );
    }
}
