use crate::analysis_runtime::{DocumentSnapshot, Position};
use crate::index::SymbolIndex;
use crate::lsp::external_indexes::ExternalIndexes;
use crate::lsp::open_documents::ForegroundQuerySnapshot;
use crate::lsp::{
    file_index_for_source, offset_for_position, range_for_span, FileIndexAnalysis, LspPosition,
    LspRange,
};
use crate::model::SymbolKind;
use crate::resolver::{
    CandidateSource, IdentifierContext, ReferenceCandidate, ReferenceResolver, ResolutionReason,
};
use serde::Serialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspLocation {
    pub uri: String,
    pub range: LspRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspLocationLink {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_selection_range: Option<LspRange>,
    pub target_uri: String,
    pub target_range: LspRange,
    pub target_selection_range: LspRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDefinitionReport {
    pub locations: Vec<LspLocation>,
    pub links: Vec<LspLocationLink>,
    pub parse_diagnostics: usize,
    pub selected_label: Option<String>,
    pub selected_kind: Option<SymbolKind>,
    pub selected_source: Option<CandidateSource>,
    pub resolver_reason: Option<ResolutionReason>,
    pub identifier_context: Option<IdentifierContext>,
    pub resolver_candidate_count: usize,
}

impl LspDefinitionReport {
    pub fn is_hit(&self) -> bool {
        !self.links.is_empty()
    }
}

pub fn definition_report_for_source_position(
    source: &str,
    uri: &str,
    position: LspPosition,
) -> LspDefinitionReport {
    definition_report_for_source_position_with_external(source, uri, position, None)
}

pub fn definition_report_for_source_position_with_external(
    source: &str,
    uri: &str,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
) -> LspDefinitionReport {
    let analysis = file_index_for_source(source);
    definition_report_for_cached_analysis_with_external(
        source,
        &analysis,
        uri,
        position,
        external_index,
    )
}

pub fn definition_report_for_cached_analysis_with_external(
    source: &str,
    analysis: &FileIndexAnalysis,
    uri: &str,
    position: LspPosition,
    external_index: Option<&SymbolIndex>,
) -> LspDefinitionReport {
    definition_report_for_cached_analysis_with_external_indexes(
        source,
        analysis,
        uri,
        position,
        None,
        external_index,
    )
}

/// The pending-analysis definition contract is intentionally narrower than
/// resolver-backed navigation: only a cursor already on a syntactically proven
/// top-level declaration may navigate to that declaration's current-snapshot
/// name.  References, members, locals, and recovered declarations return no
/// target rather than combining current text with an older analysis.
pub(crate) fn definition_report_for_pending_snapshot(
    snapshot: &DocumentSnapshot,
    foreground: &ForegroundQuerySnapshot,
    uri: &str,
    position: LspPosition,
    parse_diagnostics: usize,
) -> LspDefinitionReport {
    let Some(offset) = snapshot.positions().and_then(|positions| {
        positions.offset_for_position(Position {
            line: position.line,
            character: position.character,
        })
    }) else {
        return empty_definition_report(parse_diagnostics);
    };

    let source = snapshot.text();
    let Some(declaration) = foreground.top_level_declaration_at_offset(offset) else {
        return empty_definition_report(parse_diagnostics);
    };
    let range = range_for_span(source, declaration.name_span);
    let link = LspLocationLink {
        origin_selection_range: Some(range.clone()),
        target_uri: uri.to_string(),
        target_range: range.clone(),
        target_selection_range: range,
    };
    LspDefinitionReport {
        locations: vec![location_from_link(&link)],
        links: vec![link],
        parse_diagnostics,
        selected_label: Some(declaration.name.clone()),
        selected_kind: Some(declaration.kind),
        selected_source: Some(CandidateSource::FileLocal),
        resolver_reason: None,
        identifier_context: None,
        resolver_candidate_count: 1,
    }
}

pub(crate) fn definition_report_for_cached_analysis_with_external_indexes(
    source: &str,
    analysis: &FileIndexAnalysis,
    uri: &str,
    position: LspPosition,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspDefinitionReport {
    let Some(offset) = offset_for_position(source, position) else {
        return empty_definition_report(analysis.parse_diagnostics);
    };
    definition_report_for_offset(
        source,
        analysis,
        uri,
        offset,
        workspace_index,
        game_data_index,
    )
}

fn definition_report_for_offset(
    source: &str,
    analysis: &FileIndexAnalysis,
    uri: &str,
    offset: usize,
    workspace_index: Option<&SymbolIndex>,
    game_data_index: Option<&SymbolIndex>,
) -> LspDefinitionReport {
    let resolver = ReferenceResolver::new_with_parse_scope_and_external_indexes(
        source,
        &analysis.index,
        &analysis.syntax.parse,
        &analysis.scope,
        ExternalIndexes::new(workspace_index, game_data_index).ordered(),
    );
    let Some(resolution) = resolver.resolve_at_offset(offset) else {
        return empty_definition_report(analysis.parse_diagnostics);
    };
    let candidate_count = resolution.candidates.len();
    let reason = resolution.reason;
    let identifier_context = resolution.identifier_context;
    let Some(selected) = definition_target_for_resolution(&resolution) else {
        return LspDefinitionReport {
            locations: Vec::new(),
            links: Vec::new(),
            parse_diagnostics: analysis.parse_diagnostics,
            selected_label: None,
            selected_kind: None,
            selected_source: None,
            resolver_reason: Some(reason),
            identifier_context: Some(identifier_context),
            resolver_candidate_count: candidate_count,
        };
    };

    let link = definition_link_for_candidate(uri, source, resolution.token_span, selected);
    let location = link.as_ref().map(location_from_link);
    LspDefinitionReport {
        links: link.into_iter().collect(),
        locations: location.into_iter().collect(),
        parse_diagnostics: analysis.parse_diagnostics,
        selected_label: selected.name.clone(),
        selected_kind: Some(selected.kind),
        selected_source: Some(selected.source),
        resolver_reason: Some(reason),
        identifier_context: Some(identifier_context),
        resolver_candidate_count: candidate_count,
    }
}

/// A definition request on an override declaration should lead to the inherited
/// contract, while the shared resolver keeps its usual local-declaration
/// selection for hover and all other consumers.
fn definition_target_for_resolution(
    resolution: &crate::resolver::ReferenceResolution,
) -> Option<&ReferenceCandidate> {
    let selected = resolution.selected.as_ref()?;
    if selected.kind == SymbolKind::Class && selected.is_modded {
        return resolution
            .candidates
            .iter()
            .find(|candidate| {
                candidate.kind == SymbolKind::Class
                    && candidate.name == selected.name
                    && !candidate.is_modded
            })
            .or(Some(selected));
    }
    if !(selected.source == CandidateSource::FileLocal
        && selected.reason == ResolutionReason::DeclarationHit
        && selected.kind == SymbolKind::Method
        && selected.is_override)
    {
        return Some(selected);
    }
    let override_key = selected.callable_override_key.as_deref()?;
    resolution
        .candidates
        .iter()
        .skip(1)
        .find(|candidate| {
            candidate.kind == SymbolKind::Method
                && candidate.selection_span != resolution.token_span
                && candidate.callable_override_key.as_deref() == Some(override_key)
        })
        .or(Some(selected))
}

fn empty_definition_report(parse_diagnostics: usize) -> LspDefinitionReport {
    LspDefinitionReport {
        locations: Vec::new(),
        links: Vec::new(),
        parse_diagnostics,
        selected_label: None,
        selected_kind: None,
        selected_source: None,
        resolver_reason: None,
        identifier_context: None,
        resolver_candidate_count: 0,
    }
}

fn definition_link_for_candidate(
    current_uri: &str,
    current_source: &str,
    origin_span: crate::lexer::TextSpan,
    candidate: &ReferenceCandidate,
) -> Option<LspLocationLink> {
    let origin_selection_range = Some(range_for_span(current_source, origin_span));
    match candidate.source {
        CandidateSource::FileLocal => Some(LspLocationLink {
            origin_selection_range,
            target_uri: current_uri.to_string(),
            target_range: range_for_span(current_source, candidate.span),
            target_selection_range: range_for_span(current_source, candidate.selection_span),
        }),
        CandidateSource::External => {
            let (target_uri, source) = if let Some(identity) = &candidate.virtual_source {
                (
                    identity.uri.clone(),
                    crate::addon_sources::read_virtual_source(&identity.uri).ok()?,
                )
            } else {
                let path = candidate.absolute_path.as_ref()?;
                (file_uri_for_path(path)?, fs::read_to_string(path).ok()?)
            };
            Some(LspLocationLink {
                origin_selection_range,
                target_uri,
                target_range: range_for_span(&source, candidate.span),
                target_selection_range: range_for_span(&source, candidate.selection_span),
            })
        }
    }
}

fn location_from_link(link: &LspLocationLink) -> LspLocation {
    LspLocation {
        uri: link.target_uri.clone(),
        range: link.target_selection_range,
    }
}

pub(crate) fn file_uri_for_path(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = path.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = normalized.strip_prefix("//?/UNC/") {
        normalized = format!("//{stripped}");
    }
    if let Some(stripped) = normalized.strip_prefix("//?/") {
        normalized = stripped.to_string();
    }
    if let Some(unc) = normalized.strip_prefix("//") {
        let (host, share_path) = unc.split_once('/')?;
        return Some(format!(
            "file://{}/{}",
            percent_encode_uri_path(host),
            percent_encode_uri_path(share_path)
        ));
    }
    if normalized.starts_with('/') {
        Some(format!("file://{}", percent_encode_uri_path(&normalized)))
    } else {
        Some(format!("file:///{}", percent_encode_uri_path(&normalized)))
    }
}

fn percent_encode_uri_path(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep =
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~');
        if keep {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::file_index_for_source;

    fn override_method_position(source: &str) -> LspPosition {
        crate::lsp::position_for_offset(source, source.rfind("OnPostInit").unwrap())
    }

    #[test]
    fn override_definition_selects_a_matching_file_local_base_method() {
        let source = r#"class Base
{
	event protected void OnPostInit(IEntity owner);
}

class Child : Base
{
	override protected void OnPostInit(IEntity owner) {}
}
"#;
        let report = definition_report_for_source_position(
            source,
            "file:///Scripts/Child.c",
            override_method_position(source),
        );

        assert_eq!(report.selected_source, Some(CandidateSource::FileLocal));
        assert_eq!(report.selected_label.as_deref(), Some("OnPostInit"));
        assert_eq!(report.links.len(), 1);
        assert_eq!(report.links[0].target_uri, "file:///Scripts/Child.c");
        assert!(report.links[0].target_selection_range.start.line < 5);
    }

    #[test]
    fn override_definition_selects_a_matching_external_base_method() {
        let source = r#"class Child : ScriptComponent
{
	override protected void OnPostInit(IEntity owner) {}
}
"#;
        let external = file_index_for_source(
            "class ScriptComponent { event protected void OnPostInit(IEntity owner); }",
        )
        .index;
        let report = definition_report_for_source_position_with_external(
            source,
            "file:///Scripts/Child.c",
            override_method_position(source),
            Some(&external),
        );

        assert_eq!(report.selected_source, Some(CandidateSource::External));
        assert_eq!(report.selected_label.as_deref(), Some("OnPostInit"));
    }

    #[test]
    fn override_definition_does_not_select_a_different_overload() {
        let source = r#"class Base
{
	event protected void OnPostInit(int value);
	event protected void OnPostInit(IEntity owner);
}

class Child : Base
{
	override protected void OnPostInit(IEntity owner) {}
}
"#;
        let report = definition_report_for_source_position(
            source,
            "file:///Scripts/Child.c",
            override_method_position(source),
        );

        assert_eq!(report.links.len(), 1);
        assert_eq!(report.links[0].target_selection_range.start.line, 3);
    }

    #[test]
    fn definition_keeps_a_non_override_method_on_its_own_declaration() {
        let source = r#"class Base
{
	event protected void OnPostInit(IEntity owner);
}

class Child : Base
{
	protected void OnPostInit(IEntity owner) {}
}
"#;
        let report = definition_report_for_source_position(
            source,
            "file:///Scripts/Child.c",
            override_method_position(source),
        );

        assert_eq!(report.selected_source, Some(CandidateSource::FileLocal));
        assert_eq!(report.links.len(), 1);
        assert_eq!(report.links[0].target_selection_range.start.line, 7);
    }
}
