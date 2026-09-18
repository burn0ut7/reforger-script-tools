use crate::index::SymbolIndex;
use crate::resolver::ReferenceCandidate;

#[derive(Clone, Copy)]
pub(crate) struct ExternalIndexes<'a> {
    workspace: Option<&'a SymbolIndex>,
    game_data: Option<&'a SymbolIndex>,
}

impl<'a> ExternalIndexes<'a> {
    pub(crate) const fn new(
        workspace: Option<&'a SymbolIndex>,
        game_data: Option<&'a SymbolIndex>,
    ) -> Self {
        Self {
            workspace,
            game_data,
        }
    }

    pub(crate) fn ordered(self) -> Vec<&'a SymbolIndex> {
        self.workspace.into_iter().chain(self.game_data).collect()
    }

    pub(crate) fn for_candidate(self, candidate: &ReferenceCandidate) -> Option<&'a SymbolIndex> {
        self.workspace
            .into_iter()
            .chain(self.game_data)
            .find(|index| index.identity() == candidate.index_id)
    }
}

#[cfg(test)]
mod tests {
    use super::ExternalIndexes;
    use crate::index::SymbolIndex;
    use crate::lsp::open_documents::file_index_for_source;
    use crate::model::{SourceFileMetadata, SourceKind};
    use crate::parser::parse_source;
    use crate::resolver::ReferenceResolver;
    use crate::semantic_file::SemanticFile;

    fn index(source: &str, kind: SourceKind) -> SymbolIndex {
        let parse = parse_source(source);
        assert!(parse.diagnostics.is_empty());
        let semantic = SemanticFile::build(source, &parse);
        SymbolIndex::from_semantic_files([(
            &semantic,
            SourceFileMetadata {
                kind,
                ..SourceFileMetadata::unknown()
            },
        )])
    }

    #[test]
    fn candidate_ownership_survives_metadata_and_rejects_other_snapshots() {
        let source = "Widget value;";
        let local = file_index_for_source(source);
        for kind in [
            SourceKind::Unknown,
            SourceKind::Fixture,
            SourceKind::Workspace,
            SourceKind::GameData,
        ] {
            let owner = index("class Widget {}", kind);
            let unrelated = index("class Other {}", kind);
            let candidate = ReferenceResolver::new(source, &local.index, Some(&owner))
                .resolve_at_offset(1)
                .unwrap()
                .selected
                .unwrap();
            assert_eq!(candidate.id, unrelated.symbols()[0].id);
            let selected = ExternalIndexes::new(Some(&unrelated), Some(&owner))
                .for_candidate(&candidate)
                .unwrap();
            assert!(std::ptr::eq(selected, &owner));
            assert!(ExternalIndexes::new(Some(&unrelated), None)
                .for_candidate(&candidate)
                .is_none());
            assert!(ExternalIndexes::new(None, None)
                .for_candidate(&candidate)
                .is_none());
            let cloned = owner.clone();
            assert!(ExternalIndexes::new(None, Some(&cloned))
                .for_candidate(&candidate)
                .is_none());
            let encoded = serde_json::to_vec(&owner).unwrap();
            let decoded: SymbolIndex = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(encoded, serde_json::to_vec(&decoded).unwrap());
            assert!(ExternalIndexes::new(None, Some(&decoded))
                .for_candidate(&candidate)
                .is_none());
        }
    }

    #[test]
    fn colliding_ids_do_not_suppress_distinct_index_candidates() {
        let source = "Widget value;";
        let local = file_index_for_source(source);
        let first = index("class Widget {}", SourceKind::Fixture);
        let second = index("class Widget {}", SourceKind::Fixture);
        let resolution =
            ReferenceResolver::new_with_external_indexes(source, &local.index, [&first, &second])
                .resolve_at_offset(1)
                .unwrap();
        assert_eq!(resolution.candidates.len(), 2);
        assert_eq!(resolution.candidates[0].id, resolution.candidates[1].id);
        assert_ne!(
            resolution.candidates[0].index_id,
            resolution.candidates[1].index_id
        );
    }

    #[test]
    fn fixture_and_layered_owners_drive_hover_coloring_and_debug_details() {
        let source = "class Example { void Run() { int count = Widget.COUNT; } }";
        let local = file_index_for_source(source);
        for layered in [false, true] {
            let owner = index(
                "class Widget { static const int COUNT = 4; }",
                SourceKind::Fixture,
            );
            let unrelated = index("class Other { int WRONG = 9; }", SourceKind::Fixture);
            let owner = if layered {
                SymbolIndex::layered([owner])
            } else {
                owner
            };
            let offset = source.find("COUNT").unwrap();
            let position = crate::lsp::position_for_offset(source, offset);
            let hover = crate::lsp::hover::hover_report_for_cached_analysis_with_external_indexes(
                source,
                &local,
                "file:///example.c",
                position,
                Some(&unrelated),
                Some(&owner),
            );
            assert_eq!(hover.selected_label.as_deref(), Some("COUNT"));
            let tokens = crate::lsp::semantic_tokens::semantic_tokens_report_for_cached_analysis_with_external_indexes(
                source, &local, Some(&unrelated), Some(&owner));
            assert!(tokens
                .decoded
                .iter()
                .any(|token| token.text == "COUNT" && token.token_type == "enumMember"));
            let debug = crate::lsp::debug_hover::debug_hover_report_for_cached_analysis_with_external_indexes(
                source, &local, "file:///example.c", position, Some(&unrelated), Some(&owner), None);
            assert!(debug.contains("COUNT"));
            assert!(!debug.contains("WRONG"), "{debug}");
        }
    }

    #[test]
    fn orders_workspace_ahead_of_game_data_for_same_symbol_lookup() {
        let workspace = SymbolIndex::default();
        let game_data = SymbolIndex::default();

        let ordered = ExternalIndexes::new(Some(&workspace), Some(&game_data)).ordered();

        assert_eq!(ordered.len(), 2);
        assert!(std::ptr::eq(ordered[0], &workspace));
        assert!(std::ptr::eq(ordered[1], &game_data));
    }
}
