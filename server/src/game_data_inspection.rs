use crate::game_data_search::{
    compact_signature, decode_symbol_ref, documentation_summary, encode_symbol_ref, kind_name,
    logical_path, owner_name, qualify, GameDataAddonMap, ReadSourceInput, SourceLineRange,
    SourceLineStarts,
};
use crate::index::{GlobalSymbolId, SourceFileId, SymbolIndex};
use crate::index_build::IndexBuildControl;
use crate::lexer::{lex_with_control, TokenKind};
use crate::symbol_display::documentation_display;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const MAX_SYMBOL_REF_BYTES: usize = 2048;
const MAX_MEMBERS: usize = 50;
const MAX_RAW_DOCUMENTATION_BYTES: usize = 16 * 1024;
const DEFAULT_LINES: usize = 200;
const MAX_LINES: usize = 500;
const MAX_SOURCE_BYTES: usize = 128 * 1024;

#[derive(Debug)]
pub enum GameDataInspectionError {
    Initialization(String),
    Unavailable,
    SourceEvidenceUnavailable,
    InvalidSymbolRef,
    StaleSymbolRef,
    InvalidSource(String),
    SourceReadFailed(String),
    GameDataChanged,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct GameDataSourceReadRequest {
    pub catalogue_revision: String,
    pub addon_guid: Option<String>,
    pub relative_path: String,
    pub start_line: Option<usize>,
    pub line_count: Option<usize>,
    pub include_preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GameDataInspectionOutput {
    pub catalogue_revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addon_guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addon_label: Option<String>,
    pub symbol_ref: String,
    pub name: Option<String>,
    pub kind: String,
    pub qualified_name: String,
    pub container: Option<String>,
    pub signature: String,
    #[serde(rename = "type")]
    pub type_text: Option<String>,
    pub return_type: Option<String>,
    pub base_type: Option<String>,
    pub default_value: Option<String>,
    pub enum_value: Option<String>,
    pub modifiers: Vec<String>,
    pub attributes: Vec<String>,
    pub callable_form: Option<String>,
    pub documentation: InspectionDocumentation,
    pub raw_documentation: String,
    pub raw_truncated: bool,
    pub conditional_context: Vec<InspectionConditionalContext>,
    pub source_category: String,
    pub relative_path: String,
    pub declaration_range: SourceLineRange,
    pub selection_range: SourceLineRange,
    pub parent_symbol_ref: Option<String>,
    pub read_source_input: ReadSourceInput,
    pub members: Vec<InspectionMember>,
    pub members_returned: usize,
    pub members_total: usize,
    pub members_truncated: bool,
    pub members_truncation_guidance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InspectionDocumentation {
    pub summary: Option<String>,
    pub parameters: Vec<InspectionDocumentationParameter>,
    pub returns: Option<String>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InspectionDocumentationParameter {
    pub name: String,
    pub direction: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InspectionConditionalContext {
    pub kind: String,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InspectionMember {
    pub symbol_ref: String,
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub documentation_summary: Option<String>,
    pub selection_range: SourceLineRange,
}

pub fn inspect(
    index: &SymbolIndex,
    starts: &BTreeMap<SourceFileId, SourceLineStarts>,
    addon_map: &GameDataAddonMap,
    control: &IndexBuildControl,
    revision: &str,
    symbol_ref: &str,
) -> Result<GameDataInspectionOutput, GameDataInspectionError> {
    let id = resolve_symbol_ref(index, addon_map, control, revision, symbol_ref)?;
    let symbol = index.symbol(id).expect("located symbol");
    let qualified_name = qualify(
        owner_name(index, symbol).as_deref(),
        symbol.name.as_deref().unwrap_or_default(),
    );
    let file = index.file(id.file_id).expect("located file");
    let addon = addon_map.get(&id.file_id);
    let lines = starts.get(&id.file_id).cloned().unwrap_or_default();
    let raw = symbol
        .doc_comments
        .iter()
        .map(|comment| comment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let (raw, raw_truncated) = bounded(raw, MAX_RAW_DOCUMENTATION_BYTES);
    let members = index
        .children(id)
        .iter()
        .filter_map(|child| index.symbol(*child).map(|member| (*child, member)))
        .filter(|(_, member)| member.kind != crate::model::SymbolKind::Parameter)
        .collect::<Vec<_>>();
    let member_values = members
        .iter()
        .take(MAX_MEMBERS)
        .map(|(member_id, member)| {
            let name = member.name.clone().unwrap_or_default();
            let owner = owner_name(index, member);
            let qualified = qualify(owner.as_deref(), &name);
            InspectionMember {
                symbol_ref: encode_symbol_ref(
                    revision,
                    addon.map(|identity| identity.guid.as_str()),
                    &logical_path(file),
                    kind_name(member.kind),
                    &qualified,
                    member.selection_span.start,
                ),
                name,
                kind: kind_name(member.kind).to_string(),
                signature: index
                    .callable_signature(*member_id)
                    .unwrap_or_else(|| compact_signature(member, &qualified)),
                documentation_summary: documentation_summary(member),
                selection_range: lines
                    .range(member.selection_span.start, member.selection_span.end),
            }
        })
        .collect::<Vec<_>>();
    let parent_ref = symbol
        .parent
        .and_then(|parent| index.symbol(parent))
        .and_then(|parent| {
            let parent_file = index.file(parent.id.file_id)?;
            let name = parent.name.as_deref()?;
            let owner = owner_name(index, parent);
            let q = qualify(owner.as_deref(), name);
            Some(encode_symbol_ref(
                revision,
                addon_map
                    .get(&parent.id.file_id)
                    .map(|identity| identity.guid.as_str()),
                &logical_path(parent_file),
                kind_name(parent.kind),
                &q,
                parent.selection_span.start,
            ))
        });
    let members_returned = member_values.len();
    let documentation = documentation_display(&symbol.doc_comments);
    Ok(GameDataInspectionOutput {
        catalogue_revision: revision.to_string(),
        addon_guid: addon.map(|identity| identity.guid.clone()),
        addon_label: addon.map(|identity| identity.label.clone()),
        symbol_ref: symbol_ref.to_string(),
        name: symbol.name.clone(),
        kind: kind_name(symbol.kind).to_string(),
        qualified_name: qualified_name.clone(),
        container: owner_name(index, symbol),
        signature: index.callable_signature(id).unwrap_or_else(|| compact_signature(symbol, &qualified_name)),
        type_text: symbol.detail.type_text.clone(),
        return_type: symbol.detail.return_type_text.clone(),
        base_type: symbol.detail.base_type.clone(),
        default_value: symbol.detail.default_text.clone(),
        enum_value: symbol.detail.enum_value_text.clone(),
        modifiers: symbol.modifiers.clone(),
        attributes: symbol.attributes.iter().map(|attribute| attribute.text.clone()).collect(),
        callable_form: symbol.callable_form.map(|form| form.as_str().to_string()),
        documentation: InspectionDocumentation {
            summary: documentation.summary,
            parameters: documentation.parameters.into_iter().map(|parameter| InspectionDocumentationParameter {
                name: parameter.name,
                direction: parameter.direction,
                description: parameter.description,
            }).collect(),
            returns: documentation.returns,
            warnings: documentation.warnings,
            notes: documentation.notes,
        },
        raw_documentation: raw,
        raw_truncated,
        conditional_context: symbol.conditional_context.iter().map(|context| InspectionConditionalContext {
            kind: context.kind.as_str().to_string(),
            condition: context.condition.clone(),
        }).collect(),
        source_category: file.metadata.category.as_str().to_string(),
        relative_path: logical_path(file),
        declaration_range: lines.range(symbol.span.start, symbol.span.end),
        selection_range: lines.range(symbol.selection_span.start, symbol.selection_span.end),
        parent_symbol_ref: parent_ref,
        read_source_input: ReadSourceInput {
            catalogue_revision: revision.to_string(),
            addon_guid: addon.map(|identity| identity.guid.clone()),
            relative_path: logical_path(file),
            start_line: lines.range(symbol.span.start, symbol.span.end).start_line,
        },
        members: member_values,
        members_returned,
        members_total: members.len(),
        members_truncated: members.len() > MAX_MEMBERS,
        members_truncation_guidance: (members.len() > MAX_MEMBERS).then(|| "Call list_game_data_symbol_members with this symbolRef and an optional kinds filter.".to_string()),
    })
}

pub(crate) fn resolve_symbol_ref(
    index: &SymbolIndex,
    addon_map: &GameDataAddonMap,
    control: &IndexBuildControl,
    revision: &str,
    symbol_ref: &str,
) -> Result<GlobalSymbolId, GameDataInspectionError> {
    if symbol_ref.len() > MAX_SYMBOL_REF_BYTES {
        return Err(GameDataInspectionError::InvalidSymbolRef);
    }
    let reference =
        decode_symbol_ref(symbol_ref).ok_or(GameDataInspectionError::InvalidSymbolRef)?;
    if reference.catalogue_revision != revision {
        return Err(GameDataInspectionError::StaleSymbolRef);
    }
    for symbol in index.symbol_iter() {
        control
            .check()
            .map_err(|_| GameDataInspectionError::Cancelled)?;
        let Some(file) = index.file(symbol.id.file_id) else {
            continue;
        };
        let Some(name) = symbol.name.as_deref() else {
            continue;
        };
        let qualified = qualify(owner_name(index, symbol).as_deref(), name);
        if logical_path(file) == reference.path
            && addon_map
                .get(&symbol.id.file_id)
                .map(|identity| identity.guid.as_str())
                == reference.addon_guid.as_deref()
            && kind_name(symbol.kind) == reference.kind
            && qualified == reference.qualified_name
            && symbol.selection_span.start == reference.selection_start
        {
            return Ok(symbol.id);
        }
    }
    Err(GameDataInspectionError::StaleSymbolRef)
}

pub fn read_source(
    index: &SymbolIndex,
    addon_map: &GameDataAddonMap,
    control: &IndexBuildControl,
    revision: &str,
    source_texts: &BTreeMap<SourceFileId, Arc<str>>,
    request: GameDataSourceReadRequest,
) -> Result<Value, GameDataInspectionError> {
    control
        .check()
        .map_err(|_| GameDataInspectionError::Cancelled)?;
    if request.catalogue_revision != revision {
        return Err(GameDataInspectionError::StaleSymbolRef);
    }
    if request.relative_path.is_empty()
        || request.relative_path.contains("\\")
        || request.relative_path.starts_with('/')
        || request.relative_path.contains("..")
    {
        return Err(GameDataInspectionError::InvalidSource(
            "relativePath must be an exact logical catalogue path".to_string(),
        ));
    }
    let file = index
        .files()
        .iter()
        .find(|file| {
            logical_path(file) == request.relative_path
                && addon_map
                    .get(&file.id)
                    .map(|identity| identity.guid.as_str())
                    == request.addon_guid.as_deref()
        })
        .ok_or_else(|| {
            GameDataInspectionError::InvalidSource(
                "relativePath is not in the catalogue".to_string(),
            )
        })?;
    let source = source_texts.get(&file.id).ok_or_else(|| {
        GameDataInspectionError::InvalidSource("catalogue source is unavailable".to_string())
    })?;
    let start = request.start_line.unwrap_or(1);
    if start == 0 {
        return Err(GameDataInspectionError::InvalidSource(
            "startLine must be one-based".to_string(),
        ));
    }
    let count = request
        .line_count
        .unwrap_or(DEFAULT_LINES)
        .clamp(1, MAX_LINES);
    let all = source.split_inclusive('\n').collect::<Vec<_>>();
    if start > all.len().saturating_add(1) {
        return Err(GameDataInspectionError::InvalidSource(
            "startLine is outside the source file".to_string(),
        ));
    }
    let mut content = String::new();
    let mut taken = 0;
    for text in all.iter().skip(start - 1).take(count) {
        if content.len() + text.len() > MAX_SOURCE_BYTES {
            break;
        }
        content.push_str(text);
        taken += 1;
    }
    if taken == 0 && start <= all.len() {
        return Err(GameDataInspectionError::InvalidSource(
            "The requested source line exceeds the 128 KiB response limit.".to_string(),
        ));
    }
    let end = if taken == 0 {
        start.saturating_sub(1)
    } else {
        start + taken - 1
    };
    let truncated = end < all.len();
    let preview = if request.include_preview {
        let byte_start = all.iter().take(start - 1).map(|line| line.len()).sum();
        Some(source_preview_content(
            source,
            byte_start,
            content.len(),
            control,
        )?)
    } else {
        None
    };
    let mut result = json!({"catalogueRevision": revision, "addonGuid": request.addon_guid, "relativePath": request.relative_path, "startLine": start, "endLine": end, "content": content, "truncated": truncated, "nextStartLine": truncated.then_some(end + 1)});
    if let Some(preview) = preview {
        result["previewContent"] = Value::String(preview);
    }
    Ok(result)
}

// Lex only through the requested range, retaining comment state from earlier
// lines. Masking preserves UTF-16 columns and line breaks for editor tokens.
fn source_preview_content(
    source: &str,
    start: usize,
    length: usize,
    control: &IndexBuildControl,
) -> Result<String, GameDataInspectionError> {
    let end = start + length;
    let tokens = lex_with_control(&source[..end], || control.check())
        .map_err(|_| GameDataInspectionError::Cancelled)?;
    let mut output = String::with_capacity(length);
    let mut offset = start;
    for token in tokens {
        if !matches!(
            token.kind,
            TokenKind::LineComment
                | TokenKind::DocLineComment
                | TokenKind::BlockComment
                | TokenKind::DocBlockComment
                | TokenKind::UnterminatedBlockComment
        ) || token.span.end <= start
        {
            continue;
        }
        let comment_start = token.span.start.max(start);
        output.push_str(&source[offset..comment_start]);
        for character in source[comment_start..token.span.end].chars() {
            match character {
                '\r' | '\n' | '\t' => output.push(character),
                _ => output.extend(std::iter::repeat_n(' ', character.len_utf16())),
            }
        }
        offset = token.span.end;
    }
    output.push_str(&source[offset..end]);
    Ok(output)
}

fn bounded(value: String, max: usize) -> (String, bool) {
    if value.len() <= max {
        return (value, false);
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1
    }
    (value[..end].to_string(), true)
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn bounded_preview_keeps_comment_state_from_before_the_window() {
        let source = "/* comment begins\r\n😀 continued */ int value; // trailing\r\nstring url = \"https://example.test/*path*/\";\r\n";
        let start = source.find('😀').unwrap();
        let result = source_preview_content(
            source,
            start,
            source.len() - start,
            &IndexBuildControl::default(),
        )
        .unwrap();
        let lines = result.lines().collect::<Vec<_>>();
        assert_eq!(lines[0].trim(), "int value;");
        assert_eq!(lines[1], "string url = \"https://example.test/*path*/\";");
        assert_eq!(
            result.encode_utf16().count(),
            source[start..].encode_utf16().count()
        );
        assert!(result.contains("\r\n"));
    }

    #[test]
    fn previews_preserve_escaped_strings_and_mask_unterminated_comments() {
        let source = "string value = \"quote: \\\" // string\"; /* unterminated\n still a comment";
        let result =
            source_preview_content(source, 0, source.len(), &IndexBuildControl::default()).unwrap();
        assert_eq!(
            result.lines().next().unwrap().trim_end(),
            "string value = \"quote: \\\" // string\";"
        );
        assert!(result.lines().nth(1).unwrap().trim().is_empty());
        assert_eq!(result.encode_utf16().count(), source.encode_utf16().count());
    }

    #[test]
    fn preview_lexing_stops_at_the_bounded_read_and_honors_cancellation() {
        let source = "int value;\n/* unfinished beyond the returned window";
        assert_eq!(
            source_preview_content(source, 0, 11, &IndexBuildControl::default()).unwrap(),
            "int value;\n"
        );
        let control = IndexBuildControl::default();
        control.cancel();
        assert!(matches!(
            source_preview_content(source, 0, 11, &control),
            Err(GameDataInspectionError::Cancelled)
        ));
    }

    #[test]
    #[ignore = "manual bounded-preview timing profile"]
    fn profile_bounded_preview_projection() {
        let source =
            "class Example { string url = \"https://example.test\"; /* note */ int value; }\n"
                .repeat(4000);
        for start in [0, source.len() - 75] {
            let mut samples = (0..8)
                .map(|_| {
                    let begin = std::time::Instant::now();
                    let result =
                        source_preview_content(&source, start, 75, &IndexBuildControl::default())
                            .unwrap();
                    std::hint::black_box(result);
                    begin.elapsed().as_micros()
                })
                .collect::<Vec<_>>();
            samples.remove(0);
            samples.sort_unstable();
            println!(
                "preview prefix_bytes={} returned_bytes=75 median_us={}",
                start + 75,
                samples[3]
            );
        }
    }
}
