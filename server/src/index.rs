use crate::ast::DocCommentKind;
use crate::lexer::TextSpan;
use crate::model::{
    CallableForm, PreprocessorBranchKind, SourceFileMetadata, SourceKind, SymbolId, SymbolKind,
};
use crate::semantic_file::{
    FileContribution, FileContributionValidationError, PublicSymbol, PublicText,
    SemanticCallableForm, SemanticConditionalBranchKind, SemanticDeclarationKind,
    SemanticDocCommentKind, SemanticFile,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Runtime ownership of symbol IDs; never persisted in an index cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SymbolIndexId(u64);

impl Default for SymbolIndexId {
    fn default() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceFileId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GlobalSymbolId {
    pub file_id: SourceFileId,
    pub symbol_id: SymbolId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFile {
    pub id: SourceFileId,
    pub metadata: SourceFileMetadata,
    pub symbol_start: usize,
    pub symbol_count: usize,
    pub non_declaration_callable_fragments: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSymbolDetail {
    pub type_text: Option<String>,
    pub type_text_span: Option<TextSpan>,
    pub return_type_text: Option<String>,
    pub return_type_text_span: Option<TextSpan>,
    pub base_type: Option<String>,
    pub base_type_span: Option<TextSpan>,
    pub default_text: Option<String>,
    pub default_text_span: Option<TextSpan>,
    pub enum_value_text: Option<String>,
    pub enum_value_text_span: Option<TextSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSymbol {
    pub id: GlobalSymbolId,
    pub parent: Option<GlobalSymbolId>,
    pub kind: SymbolKind,
    pub name: Option<String>,
    pub span: TextSpan,
    pub selection_span: TextSpan,
    pub detail: IndexedSymbolDetail,
    pub attributes: Vec<IndexedAttribute>,
    pub modifiers: Vec<String>,
    pub doc_comments: Vec<IndexedDocComment>,
    pub conditional_context: Vec<IndexedConditionalBranch>,
    pub callable_form: Option<CallableForm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedAttribute {
    pub name: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedDocComment {
    pub kind: DocCommentKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedConditionalBranch {
    pub kind: PreprocessorBranchKind,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionMemberLookup {
    pub raw_candidates: Vec<GlobalSymbolId>,
    pub members: Vec<GlobalSymbolId>,
    pub shadowed_groups: Vec<MemberShadowGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberShadowGroup {
    pub key: String,
    pub kept: GlobalSymbolId,
    pub shadowed: Vec<GlobalSymbolId>,
}

#[derive(Debug, Default)]
pub struct SymbolIndex {
    identity: SymbolIndexId,
    files: Vec<IndexedFile>,
    symbols: Vec<IndexedSymbol>,
    /// Immutable child indexes retained by a runtime-only layered projection.
    /// The parent owns shared lookup maps but never duplicates child symbols.
    layers: Vec<SymbolIndex>,
    file_id_base: usize,
    by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    top_level_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    top_level_by_folded_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    by_kind: BTreeMap<SymbolKind, Vec<GlobalSymbolId>>,
    children: BTreeMap<GlobalSymbolId, Vec<GlobalSymbolId>>,
    classes_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    typedefs_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    functions_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    methods_by_owner_name: BTreeMap<(String, String), Vec<GlobalSymbolId>>,
    fields_by_owner_name: BTreeMap<(String, String), Vec<GlobalSymbolId>>,
    members_by_owner: BTreeMap<String, Vec<GlobalSymbolId>>,
    #[cfg(test)]
    lookup_map_rebuild_count: usize,
}

impl Clone for SymbolIndex {
    fn clone(&self) -> Self {
        // A cloned index can subsequently be edited independently. Its numeric
        // symbol IDs must therefore belong to a different runtime owner.
        Self {
            identity: SymbolIndexId::default(),
            files: self.files.clone(),
            symbols: self.symbols.clone(),
            layers: self.layers.clone(),
            file_id_base: self.file_id_base,
            by_name: self.by_name.clone(),
            top_level_by_name: self.top_level_by_name.clone(),
            top_level_by_folded_name: self.top_level_by_folded_name.clone(),
            by_kind: self.by_kind.clone(),
            children: self.children.clone(),
            classes_by_name: self.classes_by_name.clone(),
            typedefs_by_name: self.typedefs_by_name.clone(),
            functions_by_name: self.functions_by_name.clone(),
            methods_by_owner_name: self.methods_by_owner_name.clone(),
            fields_by_owner_name: self.fields_by_owner_name.clone(),
            members_by_owner: self.members_by_owner.clone(),
            #[cfg(test)]
            lookup_map_rebuild_count: self.lookup_map_rebuild_count,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct GeneralLookupMaps {
    by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    top_level_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    top_level_by_folded_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    by_kind: BTreeMap<SymbolKind, Vec<GlobalSymbolId>>,
    children: BTreeMap<GlobalSymbolId, Vec<GlobalSymbolId>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct KindNameLookupMaps {
    classes_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    typedefs_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
    functions_by_name: BTreeMap<String, Vec<GlobalSymbolId>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct KindAndOwnerLookupMaps {
    kind_names: KindNameLookupMaps,
    members_by_owner: BTreeMap<String, Vec<GlobalSymbolId>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OwnerNameLookupMaps {
    methods_by_owner_name: BTreeMap<(String, String), Vec<GlobalSymbolId>>,
    fields_by_owner_name: BTreeMap<(String, String), Vec<GlobalSymbolId>>,
}

#[derive(Debug, PartialEq, Eq)]
struct LookupMaps {
    general: GeneralLookupMaps,
    kind_and_owner: KindAndOwnerLookupMaps,
    owner_name: OwnerNameLookupMaps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContributionProjection {
    Full,
    RuntimeCache,
}

impl LookupMaps {
    fn build(files: &[IndexedFile], symbols: &[IndexedSymbol]) -> Self {
        // Below this size, thread startup costs more than the independent map
        // scans save. Machines with fewer than seven workers use three coarse
        // families to avoid oversubscription; wider machines split all seven
        // independent families immediately.
        const PARALLEL_REBUILD_MIN_SYMBOLS: usize = 10_000;
        let parallelism = std::thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(1);
        if symbols.len() < PARALLEL_REBUILD_MIN_SYMBOLS || parallelism < 3 {
            return Self::build_sequential(files, symbols);
        }
        Self::build_parallel(files, symbols, parallelism)
    }

    fn build_sequential(files: &[IndexedFile], symbols: &[IndexedSymbol]) -> Self {
        Self {
            general: build_general_lookup_maps(symbols),
            kind_and_owner: build_kind_and_owner_lookup_maps(files, symbols),
            owner_name: build_owner_name_lookup_maps(files, symbols),
        }
    }

    fn build_parallel(
        files: &[IndexedFile],
        symbols: &[IndexedSymbol],
        parallelism: usize,
    ) -> Self {
        if parallelism >= 7 {
            return std::thread::scope(|scope| {
                let by_name = scope.spawn(|| build_name_lookup_map(symbols));
                let top_level = scope.spawn(|| build_top_level_lookup_maps(symbols));
                let structure = scope.spawn(|| build_structure_lookup_maps(symbols));
                let kind_names = scope.spawn(|| build_kind_name_lookup_maps(symbols));
                let members = scope.spawn(|| build_member_owner_lookup_map(files, symbols));
                let methods = scope.spawn(|| build_method_owner_name_lookup_map(files, symbols));
                let fields = scope.spawn(|| build_field_owner_name_lookup_map(files, symbols));
                let (top_level_by_name, top_level_by_folded_name) = top_level
                    .join()
                    .expect("top-level lookup-map construction should not panic");
                let (by_kind, children) = structure
                    .join()
                    .expect("structural lookup-map construction should not panic");
                let kind_names = kind_names
                    .join()
                    .expect("kind/name lookup-map construction should not panic");
                Self {
                    general: GeneralLookupMaps {
                        by_name: by_name
                            .join()
                            .expect("name lookup-map construction should not panic"),
                        top_level_by_name,
                        top_level_by_folded_name,
                        by_kind,
                        children,
                    },
                    kind_and_owner: KindAndOwnerLookupMaps {
                        kind_names,
                        members_by_owner: members
                            .join()
                            .expect("member/owner lookup-map construction should not panic"),
                    },
                    owner_name: OwnerNameLookupMaps {
                        methods_by_owner_name: methods
                            .join()
                            .expect("method owner/name lookup-map construction should not panic"),
                        fields_by_owner_name: fields
                            .join()
                            .expect("field owner/name lookup-map construction should not panic"),
                    },
                }
            });
        }

        std::thread::scope(|scope| {
            let general = scope.spawn(|| build_general_lookup_maps(symbols));
            let kind_and_owner = scope.spawn(|| build_kind_and_owner_lookup_maps(files, symbols));
            let owner_name = scope.spawn(|| build_owner_name_lookup_maps(files, symbols));
            Self {
                general: general
                    .join()
                    .expect("general lookup-map construction should not panic"),
                kind_and_owner: kind_and_owner
                    .join()
                    .expect("kind/owner lookup-map construction should not panic"),
                owner_name: owner_name
                    .join()
                    .expect("owner/name lookup-map construction should not panic"),
            }
        })
    }
}

#[derive(Serialize, Deserialize)]
struct SymbolIndexSnapshot {
    files: Vec<IndexedFile>,
    symbols: Vec<IndexedSymbol>,
    by_name: Vec<(String, Vec<GlobalSymbolId>)>,
    top_level_by_name: Vec<(String, Vec<GlobalSymbolId>)>,
    by_kind: Vec<(SymbolKind, Vec<GlobalSymbolId>)>,
    children: Vec<(GlobalSymbolId, Vec<GlobalSymbolId>)>,
    classes_by_name: Vec<(String, Vec<GlobalSymbolId>)>,
    typedefs_by_name: Vec<(String, Vec<GlobalSymbolId>)>,
    functions_by_name: Vec<(String, Vec<GlobalSymbolId>)>,
    methods_by_owner_name: Vec<((String, String), Vec<GlobalSymbolId>)>,
    fields_by_owner_name: Vec<((String, String), Vec<GlobalSymbolId>)>,
    members_by_owner: Vec<(String, Vec<GlobalSymbolId>)>,
}

impl From<&SymbolIndex> for SymbolIndexSnapshot {
    fn from(index: &SymbolIndex) -> Self {
        Self {
            files: index.files.clone(),
            symbols: index.symbols.clone(),
            by_name: index
                .by_name
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            top_level_by_name: index
                .top_level_by_name
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            by_kind: index
                .by_kind
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect(),
            children: index
                .children
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect(),
            classes_by_name: index
                .classes_by_name
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            typedefs_by_name: index
                .typedefs_by_name
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            functions_by_name: index
                .functions_by_name
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            methods_by_owner_name: index
                .methods_by_owner_name
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            fields_by_owner_name: index
                .fields_by_owner_name
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            members_by_owner: index
                .members_by_owner
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }
}

impl From<SymbolIndexSnapshot> for SymbolIndex {
    fn from(snapshot: SymbolIndexSnapshot) -> Self {
        let top_level_by_name = snapshot.top_level_by_name.into_iter().collect();
        let top_level_by_folded_name = folded_top_level_names(&top_level_by_name);
        Self {
            identity: SymbolIndexId::default(),
            files: snapshot.files,
            symbols: snapshot.symbols,
            layers: Vec::new(),
            file_id_base: 0,
            by_name: snapshot.by_name.into_iter().collect(),
            top_level_by_name,
            top_level_by_folded_name,
            by_kind: snapshot.by_kind.into_iter().collect(),
            children: snapshot.children.into_iter().collect(),
            classes_by_name: snapshot.classes_by_name.into_iter().collect(),
            typedefs_by_name: snapshot.typedefs_by_name.into_iter().collect(),
            functions_by_name: snapshot.functions_by_name.into_iter().collect(),
            methods_by_owner_name: snapshot.methods_by_owner_name.into_iter().collect(),
            fields_by_owner_name: snapshot.fields_by_owner_name.into_iter().collect(),
            members_by_owner: snapshot.members_by_owner.into_iter().collect(),
            #[cfg(test)]
            lookup_map_rebuild_count: 0,
        }
    }
}

impl Serialize for SymbolIndex {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SymbolIndexSnapshot::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SymbolIndex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        SymbolIndexSnapshot::deserialize(deserializer).map(Self::from)
    }
}

impl SymbolIndex {
    pub(crate) fn identity(&self) -> SymbolIndexId {
        self.identity
    }

    pub fn from_semantic_files<'a>(
        files: impl IntoIterator<Item = (&'a SemanticFile, SourceFileMetadata)>,
    ) -> Self {
        let mut index = Self::default();
        for (semantic_file, metadata) in files {
            index.append_semantic_file(semantic_file, metadata);
        }
        if !index.files.is_empty() {
            index.rebuild_lookup_maps();
        }
        index
    }

    pub fn from_indexed_parts(files: Vec<IndexedFile>, symbols: Vec<IndexedSymbol>) -> Self {
        Self::from_indexed_parts_with_map_timing(files, symbols).0
    }

    pub(crate) fn from_indexed_parts_with_map_timing(
        files: Vec<IndexedFile>,
        symbols: Vec<IndexedSymbol>,
    ) -> (Self, Duration) {
        let mut index = Self {
            files,
            symbols,
            ..Self::default()
        };
        let map_start = Instant::now();
        index.rebuild_lookup_maps();
        (index, map_start.elapsed())
    }

    pub fn merged<'a>(indexes: impl IntoIterator<Item = &'a SymbolIndex>) -> Self {
        let mut merged = Self::default();
        for index in indexes {
            for file in &index.files {
                let mut remapped_ids = BTreeMap::<GlobalSymbolId, GlobalSymbolId>::new();
                let new_file_id = SourceFileId(merged.files.len());
                let symbol_start = merged.symbols.len();

                for symbol in index.symbols_for_file(file) {
                    remapped_ids.insert(
                        symbol.id,
                        GlobalSymbolId {
                            file_id: new_file_id,
                            symbol_id: symbol.id.symbol_id,
                        },
                    );
                }

                merged.files.push(IndexedFile {
                    id: new_file_id,
                    metadata: file.metadata.clone(),
                    symbol_start,
                    symbol_count: file.symbol_count,
                    non_declaration_callable_fragments: file.non_declaration_callable_fragments,
                });

                for symbol in index.symbols_for_file(file) {
                    let mut remapped = symbol.clone();
                    remapped.id = remapped_ids
                        .get(&symbol.id)
                        .copied()
                        .expect("merged symbol id should be remapped");
                    remapped.parent = symbol
                        .parent
                        .and_then(|parent| remapped_ids.get(&parent).copied());
                    merged.symbols.push(remapped);
                }
            }
        }

        merged.rebuild_lookup_maps();
        merged
    }

    /// Composes immutable indexes without flattening their symbol records. The
    /// resulting index owns only combined lookup maps and shallow file metadata;
    /// symbol identity remains routed to the originating layer.
    pub fn layered(indexes: impl IntoIterator<Item = SymbolIndex>) -> Self {
        Self::layered_with_timings(indexes).0
    }

    pub fn layered_with_timings(
        indexes: impl IntoIterator<Item = SymbolIndex>,
    ) -> (Self, LayeredIndexTimings) {
        let total_start = Instant::now();
        let mut layered = Self::default();
        let mut next_file_id = 0;
        let mut timings = LayeredIndexTimings::default();
        for mut index in indexes {
            debug_assert!(index.layers.is_empty(), "layers must be composed once");
            let rebase_start = Instant::now();
            index.rebase_file_ids(next_file_id);
            timings.rebase += rebase_start.elapsed();
            next_file_id += index.files.len();
            let file_projection_start = Instant::now();
            layered.files.extend(index.files.iter().cloned());
            timings.file_projection += file_projection_start.elapsed();
            layered.layers.push(index);
        }
        let lookup_projection_start = Instant::now();
        layered.rebuild_layered_lookup_maps();
        timings.lookup_projection = lookup_projection_start.elapsed();
        timings.total = total_start.elapsed();
        (layered, timings)
    }

    fn rebase_file_ids(&mut self, file_id_base: usize) {
        if file_id_base == 0 {
            self.file_id_base = 0;
            return;
        }
        let rebase = |id: GlobalSymbolId| GlobalSymbolId {
            file_id: SourceFileId(id.file_id.0 + file_id_base),
            symbol_id: id.symbol_id,
        };
        for file in &mut self.files {
            file.id = SourceFileId(file.id.0 + file_id_base);
        }
        for symbol in &mut self.symbols {
            symbol.id = rebase(symbol.id);
            symbol.parent = symbol.parent.map(rebase);
        }
        rebase_lookup_map_ids(&mut self.by_name, rebase);
        rebase_lookup_map_ids(&mut self.top_level_by_name, rebase);
        rebase_lookup_map_ids(&mut self.top_level_by_folded_name, rebase);
        rebase_lookup_map_ids(&mut self.by_kind, rebase);
        let children = std::mem::take(&mut self.children);
        self.children = children
            .into_iter()
            .map(|(parent, mut children)| {
                for child in &mut children {
                    *child = rebase(*child);
                }
                (rebase(parent), children)
            })
            .collect();
        rebase_lookup_map_ids(&mut self.classes_by_name, rebase);
        rebase_lookup_map_ids(&mut self.typedefs_by_name, rebase);
        rebase_lookup_map_ids(&mut self.functions_by_name, rebase);
        rebase_lookup_map_ids(&mut self.methods_by_owner_name, rebase);
        rebase_lookup_map_ids(&mut self.fields_by_owner_name, rebase);
        rebase_lookup_map_ids(&mut self.members_by_owner, rebase);
        self.file_id_base = file_id_base;
    }

    fn rebuild_layered_lookup_maps(&mut self) {
        let layers = &self.layers;
        // Match normal index construction: a small graph or a narrow machine
        // is faster without fine-grained thread startup and contention.
        const PARALLEL_REBUILD_MIN_SYMBOLS: usize = 10_000;
        let symbol_count = layers
            .iter()
            .map(|layer| layer.symbols.len())
            .sum::<usize>();
        let parallelism = std::thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(1);
        if symbol_count < PARALLEL_REBUILD_MIN_SYMBOLS || parallelism < 3 {
            self.rebuild_layered_lookup_maps_sequential();
            return;
        }
        if parallelism < 7 {
            self.rebuild_layered_lookup_maps_coarse_parallel();
            return;
        }
        self.rebuild_layered_lookup_maps_wide_parallel();
    }

    fn rebuild_layered_lookup_maps_sequential(&mut self) {
        let layers = &self.layers;
        self.by_name = merge_layer_lookup_maps(layers, |index| &index.by_name);
        self.top_level_by_name = merge_layer_lookup_maps(layers, |index| &index.top_level_by_name);
        self.top_level_by_folded_name =
            merge_layer_lookup_maps(layers, |index| &index.top_level_by_folded_name);
        self.by_kind = merge_layer_lookup_maps(layers, |index| &index.by_kind);
        self.children = merge_layer_lookup_maps(layers, |index| &index.children);
        self.classes_by_name = merge_layer_lookup_maps(layers, |index| &index.classes_by_name);
        self.typedefs_by_name = merge_layer_lookup_maps(layers, |index| &index.typedefs_by_name);
        self.functions_by_name = merge_layer_lookup_maps(layers, |index| &index.functions_by_name);
        self.members_by_owner = merge_layer_lookup_maps(layers, |index| &index.members_by_owner);
        self.methods_by_owner_name =
            merge_layer_lookup_maps(layers, |index| &index.methods_by_owner_name);
        self.fields_by_owner_name =
            merge_layer_lookup_maps(layers, |index| &index.fields_by_owner_name);
    }

    fn rebuild_layered_lookup_maps_coarse_parallel(&mut self) {
        let layers = &self.layers;
        let (general, kind_and_owner, owner_name) = std::thread::scope(|scope| {
            let general = scope.spawn(|| build_layered_general_lookup_maps(layers));
            let kind_and_owner = scope.spawn(|| build_layered_kind_and_owner_lookup_maps(layers));
            let owner_name = scope.spawn(|| build_layered_owner_name_lookup_maps(layers));
            (
                general
                    .join()
                    .expect("layer general lookup projection should not panic"),
                kind_and_owner
                    .join()
                    .expect("layer kind/owner lookup projection should not panic"),
                owner_name
                    .join()
                    .expect("layer owner/name lookup projection should not panic"),
            )
        });
        self.by_name = general.0;
        self.top_level_by_name = general.1;
        self.top_level_by_folded_name = general.2;
        self.by_kind = general.3;
        self.children = general.4;
        self.classes_by_name = kind_and_owner.0;
        self.typedefs_by_name = kind_and_owner.1;
        self.functions_by_name = kind_and_owner.2;
        self.members_by_owner = kind_and_owner.3;
        self.methods_by_owner_name = owner_name.0;
        self.fields_by_owner_name = owner_name.1;
    }

    fn rebuild_layered_lookup_maps_wide_parallel(&mut self) {
        let layers = &self.layers;
        let (
            by_name,
            top_level,
            structure,
            kind_names,
            members_by_owner,
            methods_by_owner_name,
            fields_by_owner_name,
        ) = std::thread::scope(|scope| {
            let by_name = scope.spawn(|| merge_layer_lookup_maps(layers, |index| &index.by_name));
            let top_level = scope.spawn(|| {
                (
                    merge_layer_lookup_maps(layers, |index| &index.top_level_by_name),
                    merge_layer_lookup_maps(layers, |index| &index.top_level_by_folded_name),
                )
            });
            let structure = scope.spawn(|| {
                (
                    merge_layer_lookup_maps(layers, |index| &index.by_kind),
                    merge_layer_lookup_maps(layers, |index| &index.children),
                )
            });
            let kind_names = scope.spawn(|| {
                (
                    merge_layer_lookup_maps(layers, |index| &index.classes_by_name),
                    merge_layer_lookup_maps(layers, |index| &index.typedefs_by_name),
                    merge_layer_lookup_maps(layers, |index| &index.functions_by_name),
                )
            });
            let members_by_owner =
                scope.spawn(|| merge_layer_lookup_maps(layers, |index| &index.members_by_owner));
            let methods_by_owner_name = scope
                .spawn(|| merge_layer_lookup_maps(layers, |index| &index.methods_by_owner_name));
            let fields_by_owner_name = scope
                .spawn(|| merge_layer_lookup_maps(layers, |index| &index.fields_by_owner_name));
            (
                by_name
                    .join()
                    .expect("layer name lookup projection should not panic"),
                top_level
                    .join()
                    .expect("layer top-level lookup projection should not panic"),
                structure
                    .join()
                    .expect("layer structure lookup projection should not panic"),
                kind_names
                    .join()
                    .expect("layer kind/name lookup projection should not panic"),
                members_by_owner
                    .join()
                    .expect("layer member lookup projection should not panic"),
                methods_by_owner_name
                    .join()
                    .expect("layer method lookup projection should not panic"),
                fields_by_owner_name
                    .join()
                    .expect("layer field lookup projection should not panic"),
            )
        });
        self.by_name = by_name;
        self.top_level_by_name = top_level.0;
        self.top_level_by_folded_name = top_level.1;
        self.by_kind = structure.0;
        self.children = structure.1;
        self.classes_by_name = kind_names.0;
        self.typedefs_by_name = kind_names.1;
        self.functions_by_name = kind_names.2;
        self.members_by_owner = members_by_owner;
        self.methods_by_owner_name = methods_by_owner_name;
        self.fields_by_owner_name = fields_by_owner_name;
    }

    /// Adds compiler-owned declaration facts. This is intentionally an
    /// ingestion seam only: callers continue to choose when a file is parsed
    /// and when its immutable facts are published into an index.
    pub fn add_semantic_file(
        &mut self,
        semantic_file: &SemanticFile,
        metadata: SourceFileMetadata,
    ) -> SourceFileId {
        let file_id = self.append_semantic_file(semantic_file, metadata);
        self.rebuild_lookup_maps();
        file_id
    }

    fn append_semantic_file(
        &mut self,
        semantic_file: &SemanticFile,
        metadata: SourceFileMetadata,
    ) -> SourceFileId {
        let file_id = SourceFileId(self.files.len());
        let symbol_start = self.symbols.len();

        self.files.push(IndexedFile {
            id: file_id,
            metadata,
            symbol_start,
            symbol_count: semantic_file.declarations().len(),
            non_declaration_callable_fragments: semantic_file.non_declaration_callable_fragments(),
        });

        for declaration in semantic_file.declarations() {
            let id = GlobalSymbolId {
                file_id,
                symbol_id: SymbolId(declaration.id.0 as usize),
            };
            let parent = declaration.parent.map(|parent| GlobalSymbolId {
                file_id,
                symbol_id: SymbolId(parent.0 as usize),
            });
            self.symbols.push(IndexedSymbol {
                id,
                parent,
                kind: indexed_symbol_kind(declaration.kind),
                name: declaration.name.as_ref().map(|value| value.text.clone()),
                span: declaration.span,
                selection_span: declaration.selection_span,
                detail: IndexedSymbolDetail {
                    type_text: declaration
                        .detail
                        .type_text
                        .as_ref()
                        .map(|value| value.text.clone()),
                    type_text_span: declaration
                        .detail
                        .type_text
                        .as_ref()
                        .map(|value| value.span),
                    return_type_text: declaration
                        .detail
                        .return_type
                        .as_ref()
                        .map(|value| value.text.clone()),
                    return_type_text_span: declaration
                        .detail
                        .return_type
                        .as_ref()
                        .map(|value| value.span),
                    base_type: declaration
                        .detail
                        .base_type
                        .as_ref()
                        .map(|value| value.text.clone()),
                    base_type_span: declaration
                        .detail
                        .base_type
                        .as_ref()
                        .map(|value| value.span),
                    default_text: declaration
                        .detail
                        .default_value
                        .as_ref()
                        .map(|value| value.text.clone()),
                    default_text_span: declaration
                        .detail
                        .default_value
                        .as_ref()
                        .map(|value| value.span),
                    enum_value_text: declaration
                        .detail
                        .enum_value
                        .as_ref()
                        .map(|value| value.text.clone()),
                    enum_value_text_span: declaration
                        .detail
                        .enum_value
                        .as_ref()
                        .map(|value| value.span),
                },
                attributes: declaration
                    .attributes
                    .iter()
                    .map(|attribute| IndexedAttribute {
                        name: semantic_attribute_name(&attribute.text).map(str::to_owned),
                        text: attribute.text.clone(),
                    })
                    .collect(),
                modifiers: declaration
                    .modifiers
                    .iter()
                    .map(|modifier| modifier.text.clone())
                    .collect(),
                doc_comments: declaration
                    .doc_comments
                    .iter()
                    .map(|comment| IndexedDocComment {
                        kind: match comment.kind {
                            SemanticDocCommentKind::Line => DocCommentKind::Line,
                            SemanticDocCommentKind::Block => DocCommentKind::Block,
                        },
                        text: comment.text.clone(),
                    })
                    .collect(),
                conditional_context: semantic_file
                    .conditional_context(declaration.conditional_context)
                    .iter()
                    .map(|branch| IndexedConditionalBranch {
                        kind: indexed_conditional_kind(branch.kind),
                        condition: branch
                            .condition
                            .as_ref()
                            .map(|condition| condition.text.clone()),
                    })
                    .collect(),
                callable_form: declaration.callable_form.map(indexed_callable_form),
            });
        }

        file_id
    }

    /// Adds a validated, serialized compiler contribution. Production
    /// workspace and game-data ingestion uses this boundary so the index never
    /// reconstructs facts from the legacy catalog or source text.
    pub fn add_file_contribution(
        &mut self,
        contribution: &FileContribution,
        metadata: SourceFileMetadata,
    ) -> Result<SourceFileId, FileContributionValidationError> {
        let mut file_ids =
            self.add_file_contributions(std::iter::once((contribution, metadata)))?;
        Ok(file_ids
            .pop()
            .expect("one contribution must produce one source file id"))
    }

    /// Adds a complete group of validated compiler contributions and rebuilds
    /// global lookup maps once after the group is visible. This is the bulk
    /// construction boundary for a cold index build; per-file updates should
    /// continue to use [`Self::add_file_contribution`].
    pub fn add_file_contributions<'contribution>(
        &mut self,
        contributions: impl IntoIterator<Item = (&'contribution FileContribution, SourceFileMetadata)>,
    ) -> Result<Vec<SourceFileId>, FileContributionValidationError> {
        self.add_file_contributions_with_projection(contributions, ContributionProjection::Full)
    }

    /// Builds the persisted external-index shape directly from compiler
    /// contributions so cold indexing constructs lookup maps only once.
    pub(crate) fn add_runtime_cache_file_contributions<'contribution>(
        &mut self,
        contributions: impl IntoIterator<Item = (&'contribution FileContribution, SourceFileMetadata)>,
    ) -> Result<Vec<SourceFileId>, FileContributionValidationError> {
        self.add_file_contributions_with_projection(
            contributions,
            ContributionProjection::RuntimeCache,
        )
    }

    fn add_file_contributions_with_projection<'contribution>(
        &mut self,
        contributions: impl IntoIterator<Item = (&'contribution FileContribution, SourceFileMetadata)>,
        projection: ContributionProjection,
    ) -> Result<Vec<SourceFileId>, FileContributionValidationError> {
        let contributions = contributions.into_iter().collect::<Vec<_>>();
        for (contribution, _) in &contributions {
            contribution.validate()?;
        }

        self.files.reserve(contributions.len());
        self.symbols.reserve(
            contributions
                .iter()
                .map(|(contribution, _)| contribution.symbols.len())
                .sum(),
        );

        let mut file_ids = Vec::with_capacity(contributions.len());
        for (contribution, metadata) in contributions {
            file_ids.push(self.append_file_contribution(contribution, metadata, projection));
        }
        if !file_ids.is_empty() {
            self.rebuild_lookup_maps();
        }
        Ok(file_ids)
    }

    /// Consumes a complete group of already-validated compiler contributions.
    ///
    /// Cache loading owns its decoded canonical records, so this avoids cloning
    /// every public string into a short-lived `FileContribution` and then into
    /// the runtime index.  Keep the borrowed API above for live semantic files
    /// that remain owned by their caller.
    pub fn add_owned_file_contributions(
        &mut self,
        contributions: impl IntoIterator<Item = (FileContribution, SourceFileMetadata)>,
    ) -> Result<Vec<SourceFileId>, FileContributionValidationError> {
        let contributions = contributions.into_iter().collect::<Vec<_>>();
        for (contribution, _) in &contributions {
            contribution.validate()?;
        }

        self.files.reserve(contributions.len());
        self.symbols.reserve(
            contributions
                .iter()
                .map(|(contribution, _)| contribution.symbols.len())
                .sum(),
        );

        let mut file_ids = Vec::with_capacity(contributions.len());
        for (contribution, metadata) in contributions {
            file_ids.push(self.append_owned_file_contribution(contribution, metadata));
        }
        if !file_ids.is_empty() {
            self.rebuild_lookup_maps();
        }
        Ok(file_ids)
    }

    fn append_file_contribution(
        &mut self,
        contribution: &FileContribution,
        metadata: SourceFileMetadata,
        projection: ContributionProjection,
    ) -> SourceFileId {
        let file_id = SourceFileId(self.files.len());
        let symbol_start = self.symbols.len();
        let mut remapped_ids = vec![None; contribution.symbols.len()];
        let mut symbol_count = 0_usize;
        for declaration in &contribution.symbols {
            if projection == ContributionProjection::RuntimeCache
                && declaration.kind == SemanticDeclarationKind::LocalVariable
            {
                continue;
            }
            remapped_ids[declaration.id.0 as usize] = Some(SymbolId(symbol_count));
            symbol_count += 1;
        }
        self.files.push(IndexedFile {
            id: file_id,
            metadata,
            symbol_start,
            symbol_count,
            non_declaration_callable_fragments: contribution.non_declaration_callable_fragments,
        });

        for declaration in &contribution.symbols {
            if projection == ContributionProjection::RuntimeCache
                && declaration.kind == SemanticDeclarationKind::LocalVariable
            {
                continue;
            }
            let id = GlobalSymbolId {
                file_id,
                symbol_id: remapped_ids[declaration.id.0 as usize]
                    .expect("retained validated contribution id is remapped"),
            };
            let parent = declaration.parent.and_then(|parent| {
                remapped_ids[parent.0 as usize]
                    .map(|symbol_id| GlobalSymbolId { file_id, symbol_id })
            });
            self.symbols.push(IndexedSymbol {
                id,
                parent,
                kind: indexed_symbol_kind(declaration.kind),
                name: declaration.name.clone(),
                span: declaration.span,
                selection_span: declaration.selection_span,
                detail: IndexedSymbolDetail {
                    type_text: declaration
                        .detail
                        .type_text
                        .as_ref()
                        .map(|value| value.text.clone()),
                    type_text_span: declaration.detail.type_text.as_ref().and_then(|value| {
                        (projection == ContributionProjection::Full)
                            .then_some(value.span)
                            .flatten()
                    }),
                    return_type_text: declaration
                        .detail
                        .return_type
                        .as_ref()
                        .map(|value| value.text.clone()),
                    return_type_text_span: declaration.detail.return_type.as_ref().and_then(
                        |value| {
                            (projection == ContributionProjection::Full)
                                .then_some(value.span)
                                .flatten()
                        },
                    ),
                    base_type: declaration
                        .detail
                        .base_type
                        .as_ref()
                        .map(|value| value.text.clone()),
                    base_type_span: declaration.detail.base_type.as_ref().and_then(|value| {
                        (projection == ContributionProjection::Full)
                            .then_some(value.span)
                            .flatten()
                    }),
                    default_text: declaration
                        .detail
                        .default_value
                        .as_ref()
                        .map(|value| value.text.clone()),
                    default_text_span: declaration.detail.default_value.as_ref().and_then(
                        |value| {
                            (projection == ContributionProjection::Full)
                                .then_some(value.span)
                                .flatten()
                        },
                    ),
                    enum_value_text: declaration
                        .detail
                        .enum_value
                        .as_ref()
                        .map(|value| value.text.clone()),
                    enum_value_text_span: declaration.detail.enum_value.as_ref().and_then(
                        |value| {
                            (projection == ContributionProjection::Full)
                                .then_some(value.span)
                                .flatten()
                        },
                    ),
                },
                attributes: declaration
                    .attributes
                    .iter()
                    .map(|attribute| IndexedAttribute {
                        name: semantic_attribute_name(&attribute.text).map(str::to_owned),
                        text: attribute.text.clone(),
                    })
                    .collect(),
                modifiers: declaration
                    .modifiers
                    .iter()
                    .map(|value| value.text.clone())
                    .collect(),
                doc_comments: declaration
                    .doc_comments
                    .iter()
                    .map(|comment| IndexedDocComment {
                        kind: match comment.kind {
                            SemanticDocCommentKind::Line => DocCommentKind::Line,
                            SemanticDocCommentKind::Block => DocCommentKind::Block,
                        },
                        text: comment.text.clone(),
                    })
                    .collect(),
                conditional_context: declaration
                    .conditional_context
                    .iter()
                    .map(|branch| IndexedConditionalBranch {
                        kind: indexed_conditional_kind(branch.kind),
                        condition: branch.condition.as_ref().map(|value| value.text.clone()),
                    })
                    .collect(),
                callable_form: declaration.callable_form.map(indexed_callable_form),
            });
        }

        file_id
    }

    fn append_owned_file_contribution(
        &mut self,
        contribution: FileContribution,
        metadata: SourceFileMetadata,
    ) -> SourceFileId {
        let file_id = SourceFileId(self.files.len());
        let symbol_start = self.symbols.len();
        let FileContribution {
            non_declaration_callable_fragments,
            symbols,
            ..
        } = contribution;
        self.files.push(IndexedFile {
            id: file_id,
            metadata,
            symbol_start,
            symbol_count: symbols.len(),
            non_declaration_callable_fragments,
        });

        for declaration in symbols {
            self.symbols.push(indexed_symbol_from_owned_public_symbol(
                file_id,
                declaration,
            ));
        }

        file_id
    }

    #[cfg(test)]
    pub fn lookup_map_rebuild_count(&self) -> usize {
        self.lookup_map_rebuild_count
    }

    pub fn files(&self) -> &[IndexedFile] {
        &self.files
    }

    pub fn symbols(&self) -> &[IndexedSymbol] {
        &self.symbols
    }

    /// Iterates every symbol without requiring a layered runtime projection to
    /// flatten its immutable child records into a second allocation.
    pub fn symbol_iter(&self) -> Box<dyn Iterator<Item = &IndexedSymbol> + '_> {
        if self.layers.is_empty() {
            Box::new(self.symbols.iter())
        } else {
            Box::new(self.layers.iter().flat_map(|layer| layer.symbol_iter()))
        }
    }

    pub fn without_local_variables(&self) -> Self {
        self.without_symbol_kind(SymbolKind::LocalVariable)
    }

    pub fn compact_for_runtime_cache(&self) -> Self {
        let mut compact = self.without_local_variables();
        compact.strip_detail_spans();
        compact
    }

    /// Consumes a source-built index and projects it into the runtime cache
    /// shape without cloning every retained symbol and file record.
    pub(crate) fn into_runtime_cache(mut self) -> Result<Self, String> {
        if !self.layers.is_empty() || self.file_id_base != 0 {
            return Err(
                "runtime cache compaction requires one non-layered source index".to_string(),
            );
        }

        let files = std::mem::take(&mut self.files);
        let symbols = std::mem::take(&mut self.symbols);
        let mut remapped_ids = Vec::with_capacity(files.len());
        let mut retained_counts = Vec::with_capacity(files.len());
        for file in &files {
            let end = file
                .symbol_start
                .checked_add(file.symbol_count)
                .ok_or_else(|| "source index symbol range overflow".to_string())?;
            let file_symbols = symbols
                .get(file.symbol_start..end)
                .ok_or_else(|| "source index symbol range is invalid".to_string())?;
            let new_file_id = SourceFileId(remapped_ids.len());
            let mut file_remap = vec![None; file.symbol_count];
            let mut retained = 0_usize;
            for symbol in file_symbols {
                if symbol.kind == SymbolKind::LocalVariable {
                    continue;
                }
                let slot = file_remap
                    .get_mut(symbol.id.symbol_id.0)
                    .ok_or_else(|| "source index symbol id is outside its file".to_string())?;
                *slot = Some(GlobalSymbolId {
                    file_id: new_file_id,
                    symbol_id: SymbolId(retained),
                });
                retained += 1;
            }
            remapped_ids.push(file_remap);
            retained_counts.push(retained);
        }

        let mut compact = Self::default();
        compact.files.reserve(files.len());
        compact
            .symbols
            .reserve(retained_counts.iter().copied().sum());
        let mut next_symbol_start = 0_usize;
        for (file_index, (file, symbol_count)) in files.into_iter().zip(retained_counts).enumerate()
        {
            compact.files.push(IndexedFile {
                id: SourceFileId(file_index),
                metadata: file.metadata,
                symbol_start: next_symbol_start,
                symbol_count,
                non_declaration_callable_fragments: file.non_declaration_callable_fragments,
            });
            next_symbol_start += symbol_count;
        }

        let remapped_symbol_id = |id: GlobalSymbolId| {
            remapped_ids
                .get(id.file_id.0)
                .and_then(|file| file.get(id.symbol_id.0))
                .copied()
                .flatten()
        };
        for mut symbol in symbols {
            if symbol.kind == SymbolKind::LocalVariable {
                continue;
            }
            symbol.id = remapped_symbol_id(symbol.id)
                .ok_or_else(|| "source index retained symbol has no compact id".to_string())?;
            symbol.parent = symbol.parent.and_then(remapped_symbol_id);
            symbol.detail.type_text_span = None;
            symbol.detail.return_type_text_span = None;
            symbol.detail.base_type_span = None;
            symbol.detail.default_text_span = None;
            symbol.detail.enum_value_text_span = None;
            compact.symbols.push(symbol);
        }

        compact.rebuild_lookup_maps();
        Ok(compact)
    }

    fn strip_detail_spans(&mut self) {
        for symbol in &mut self.symbols {
            symbol.detail.type_text_span = None;
            symbol.detail.return_type_text_span = None;
            symbol.detail.base_type_span = None;
            symbol.detail.default_text_span = None;
            symbol.detail.enum_value_text_span = None;
        }
    }

    fn without_symbol_kind(&self, excluded_kind: SymbolKind) -> Self {
        let mut filtered = Self::default();
        let mut remapped_ids = BTreeMap::<GlobalSymbolId, GlobalSymbolId>::new();
        let mut next_symbol_start = 0;

        for file in &self.files {
            let new_file_id = SourceFileId(filtered.files.len());
            let symbol_start = next_symbol_start;
            let mut symbol_count = 0;

            for symbol in self.symbols_for_file(file) {
                if symbol.kind == excluded_kind {
                    continue;
                }

                let new_id = GlobalSymbolId {
                    file_id: new_file_id,
                    symbol_id: SymbolId(symbol_count),
                };
                remapped_ids.insert(symbol.id, new_id);
                symbol_count += 1;
            }
            next_symbol_start += symbol_count;

            filtered.files.push(IndexedFile {
                id: new_file_id,
                metadata: file.metadata.clone(),
                symbol_start,
                symbol_count,
                non_declaration_callable_fragments: file.non_declaration_callable_fragments,
            });
        }

        for file in &self.files {
            for symbol in self.symbols_for_file(file) {
                if symbol.kind == excluded_kind {
                    continue;
                }

                let Some(new_id) = remapped_ids.get(&symbol.id).copied() else {
                    continue;
                };
                let mut remapped = symbol.clone();
                remapped.id = new_id;
                remapped.parent = symbol
                    .parent
                    .and_then(|parent| remapped_ids.get(&parent).copied());
                filtered.symbols.push(remapped);
            }
        }

        filtered.rebuild_lookup_maps();
        filtered
    }

    pub fn file(&self, id: SourceFileId) -> Option<&IndexedFile> {
        if !self.layers.is_empty() {
            return self.layers.iter().find_map(|layer| layer.file(id));
        }
        id.0.checked_sub(self.file_id_base)
            .and_then(|local_id| self.files.get(local_id))
    }

    pub fn symbol(&self, id: GlobalSymbolId) -> Option<&IndexedSymbol> {
        if !self.layers.is_empty() {
            return self.layers.iter().find_map(|layer| layer.symbol(id));
        }
        let file = self.file(id.file_id)?;
        let local_index = id.symbol_id.0;
        if local_index >= file.symbol_count {
            return None;
        }
        self.symbols.get(file.symbol_start + local_index)
    }

    pub fn symbols_for_name(&self, name: &str) -> &[GlobalSymbolId] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn top_level_symbols_for_name(&self, name: &str) -> &[GlobalSymbolId] {
        self.top_level_by_name
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn preferred_symbols_for_name(&self, name: &str) -> Vec<GlobalSymbolId> {
        self.preferred_from_symbols(self.symbols_for_name(name))
    }

    pub fn preferred_top_level_symbols_for_name(&self, name: &str) -> Vec<GlobalSymbolId> {
        self.preferred_from_symbols(self.top_level_symbols_for_name(name))
    }

    pub fn preferred_classes_by_name(&self, name: &str) -> Vec<GlobalSymbolId> {
        self.preferred_from_symbols(self.classes_by_name(name))
    }

    pub fn preferred_typedefs_by_name(&self, name: &str) -> Vec<GlobalSymbolId> {
        self.preferred_from_symbols(self.typedefs_by_name(name))
    }

    pub fn preferred_functions_by_name(&self, name: &str) -> Vec<GlobalSymbolId> {
        self.preferred_from_symbols(self.functions_by_name(name))
    }

    pub fn preferred_from_symbols(&self, symbols: &[GlobalSymbolId]) -> Vec<GlobalSymbolId> {
        let mut symbols = symbols.to_vec();
        symbols.sort_by(|left, right| self.compare_symbol_preference(*left, *right));
        symbols
    }

    pub fn symbols_for_kind(&self, kind: SymbolKind) -> &[GlobalSymbolId] {
        self.by_kind.get(&kind).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn children(&self, parent: GlobalSymbolId) -> &[GlobalSymbolId] {
        self.children.get(&parent).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn classes_by_name(&self, name: &str) -> &[GlobalSymbolId] {
        self.classes_by_name
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn typedefs_by_name(&self, name: &str) -> &[GlobalSymbolId] {
        self.typedefs_by_name
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn functions_by_name(&self, name: &str) -> &[GlobalSymbolId] {
        self.functions_by_name
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn methods_by_owner_name(&self, owner: &str, name: &str) -> &[GlobalSymbolId] {
        self.methods_by_owner_name
            .get(&(owner.to_string(), name.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn method_owner_name_groups(&self) -> &BTreeMap<(String, String), Vec<GlobalSymbolId>> {
        &self.methods_by_owner_name
    }

    pub fn fields_by_owner_name(&self, owner: &str, name: &str) -> &[GlobalSymbolId] {
        self.fields_by_owner_name
            .get(&(owner.to_string(), name.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn members_by_owner(&self, owner: &str) -> &[GlobalSymbolId] {
        self.members_by_owner
            .get(owner)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn direct_members_by_owner(&self, owner: &str) -> &[GlobalSymbolId] {
        self.members_by_owner(owner)
    }

    pub fn raw_members_for_class_including_bases(&self, owner: &str) -> Vec<GlobalSymbolId> {
        self.member_segments_for_class_including_bases(owner)
            .into_iter()
            .flatten()
            .collect()
    }

    pub fn raw_completion_members_for_owner_name(&self, owner: &str) -> CompletionMemberLookup {
        let member_segments = self.member_segments_for_class_including_bases(owner);
        self.completion_from_member_segments(member_segments)
    }

    pub fn completion_members_for_preferred_class(&self, owner: &str) -> CompletionMemberLookup {
        let member_segments = self.preferred_class_member_segments_including_bases(owner, None);
        self.completion_from_member_segments(member_segments)
    }

    /// Resolves the member chain visible to `super` in a `modded` class. The
    /// active Workspace layer is removed before shadowing, then retained
    /// overlays are walked in reverse load order toward the original class.
    pub fn completion_members_for_modded_predecessor(&self, owner: &str) -> CompletionMemberLookup {
        let member_segments = self.modded_predecessor_member_segments_including_bases(owner, None);
        self.completion_from_member_segments(member_segments)
    }

    pub fn preferred_members_named_for_class(
        &self,
        owner: &str,
        member_name: &str,
    ) -> Vec<GlobalSymbolId> {
        let member_segments =
            self.preferred_class_member_segments_including_bases(owner, Some(member_name));
        self.completion_from_member_segments(member_segments)
            .members
    }

    pub fn preferred_members_named_for_modded_predecessor(
        &self,
        owner: &str,
        member_name: &str,
    ) -> Vec<GlobalSymbolId> {
        let member_segments =
            self.modded_predecessor_member_segments_including_bases(owner, Some(member_name));
        self.completion_from_member_segments(member_segments)
            .members
    }

    fn completion_from_member_segments(
        &self,
        member_segments: Vec<Vec<GlobalSymbolId>>,
    ) -> CompletionMemberLookup {
        let raw_candidates = member_segments
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        let mut members = Vec::new();
        let mut kept_by_key = BTreeMap::<String, GlobalSymbolId>::new();
        let mut shadow_group_by_key = BTreeMap::<String, usize>::new();
        let mut shadowed_groups = Vec::new();

        for segment in member_segments {
            let mut ids_by_key = BTreeMap::<String, Vec<GlobalSymbolId>>::new();
            let mut key_order = Vec::<String>::new();
            for id in segment {
                let key = self.completion_member_key(id);
                if !ids_by_key.contains_key(&key) {
                    key_order.push(key.clone());
                }
                ids_by_key.entry(key).or_default().push(id);
            }

            for key in key_order {
                let ids = ids_by_key.remove(&key).unwrap_or_default();
                let Some(preferred) = self.preferred_from_symbols(&ids).first().copied() else {
                    continue;
                };

                if let Some(kept) = kept_by_key.get(&key).copied() {
                    self.push_shadowed_members(
                        &mut shadowed_groups,
                        &mut shadow_group_by_key,
                        key,
                        kept,
                        ids,
                    );
                } else {
                    kept_by_key.insert(key.clone(), preferred);
                    members.push(preferred);
                    self.push_shadowed_members(
                        &mut shadowed_groups,
                        &mut shadow_group_by_key,
                        key,
                        preferred,
                        ids.into_iter().filter(|id| *id != preferred).collect(),
                    );
                }
            }
        }

        CompletionMemberLookup {
            raw_candidates,
            members,
            shadowed_groups,
        }
    }

    fn push_shadowed_members(
        &self,
        shadowed_groups: &mut Vec<MemberShadowGroup>,
        shadow_group_by_key: &mut BTreeMap<String, usize>,
        key: String,
        kept: GlobalSymbolId,
        shadowed: Vec<GlobalSymbolId>,
    ) {
        if shadowed.is_empty() {
            return;
        }

        let group_index = *shadow_group_by_key.entry(key.clone()).or_insert_with(|| {
            shadowed_groups.push(MemberShadowGroup {
                key,
                kept,
                shadowed: Vec::new(),
            });
            shadowed_groups.len() - 1
        });
        shadowed_groups[group_index].shadowed.extend(shadowed);
    }

    pub fn callable_signature(&self, id: GlobalSymbolId) -> Option<String> {
        let symbol = self.symbol(id)?;
        let name = symbol.name.as_deref()?;
        let parameters = self.callable_parameter_text(id);

        match symbol.kind {
            SymbolKind::Function => {
                let return_type = symbol
                    .detail
                    .return_type_text
                    .as_deref()
                    .unwrap_or("<unknown>");
                Some(format!("{name}({parameters}) -> {return_type}"))
            }
            SymbolKind::Method => {
                let owner = self.callable_owner_name(symbol)?;
                let return_type = symbol
                    .detail
                    .return_type_text
                    .as_deref()
                    .unwrap_or("<unknown>");
                Some(format!("{owner}.{name}({parameters}) -> {return_type}"))
            }
            SymbolKind::Constructor => {
                let owner = self.callable_owner_name(symbol)?;
                Some(format!("{owner}({parameters})"))
            }
            SymbolKind::Destructor => {
                let owner = self.callable_owner_name(symbol)?;
                Some(format!("~{owner}({parameters})"))
            }
            _ => None,
        }
    }

    fn callable_parameter_text(&self, id: GlobalSymbolId) -> String {
        self.children(id)
            .iter()
            .filter_map(|child_id| self.symbol(*child_id))
            .filter(|child| child.kind == SymbolKind::Parameter)
            .map(parameter_signature_text)
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn callable_owner_name<'a>(&'a self, symbol: &'a IndexedSymbol) -> Option<&'a str> {
        symbol
            .parent
            .and_then(|parent| self.symbol(parent))
            .and_then(|parent| parent.name.as_deref())
    }

    pub fn names(&self) -> &BTreeMap<String, Vec<GlobalSymbolId>> {
        &self.by_name
    }

    pub fn top_level_names(&self) -> &BTreeMap<String, Vec<GlobalSymbolId>> {
        &self.top_level_by_name
    }

    pub fn top_level_symbols_with_ascii_case_insensitive_prefix(
        &self,
        prefix: &str,
    ) -> impl Iterator<Item = GlobalSymbolId> + '_ {
        let folded_prefix = prefix.to_ascii_lowercase();
        self.top_level_by_folded_name
            .range(folded_prefix.clone()..)
            .take_while(move |(name, _)| name.starts_with(&folded_prefix))
            .flat_map(|(_, ids)| ids.iter().copied())
    }

    pub fn duplicate_names(&self) -> Vec<(&str, &[GlobalSymbolId])> {
        self.by_name
            .iter()
            .filter(|(_, symbols)| symbols.len() > 1)
            .map(|(name, symbols)| (name.as_str(), symbols.as_slice()))
            .collect()
    }

    pub fn duplicate_top_level_names(&self) -> Vec<(&str, &[GlobalSymbolId])> {
        self.top_level_by_name
            .iter()
            .filter(|(_, symbols)| symbols.len() > 1)
            .map(|(name, symbols)| (name.as_str(), symbols.as_slice()))
            .collect()
    }

    pub fn map_counts(&self) -> IndexMapCounts {
        IndexMapCounts {
            names: self.by_name.len(),
            name_entries: map_entry_count(&self.by_name),
            top_level_names: self.top_level_by_name.len(),
            top_level_name_entries: map_entry_count(&self.top_level_by_name),
            kinds: self.by_kind.len(),
            kind_entries: map_entry_count(&self.by_kind),
            class_names: self.classes_by_name.len(),
            class_name_entries: map_entry_count(&self.classes_by_name),
            typedef_names: self.typedefs_by_name.len(),
            typedef_name_entries: map_entry_count(&self.typedefs_by_name),
            function_names: self.functions_by_name.len(),
            function_name_entries: map_entry_count(&self.functions_by_name),
            method_owner_names: self.methods_by_owner_name.len(),
            method_owner_name_entries: map_entry_count(&self.methods_by_owner_name),
            field_owner_names: self.fields_by_owner_name.len(),
            field_owner_name_entries: map_entry_count(&self.fields_by_owner_name),
            member_owners: self.members_by_owner.len(),
            member_owner_entries: map_entry_count(&self.members_by_owner),
            parent_symbols: self.children.len(),
            child_entries: map_entry_count(&self.children),
        }
    }

    pub fn source_kind_counts(&self) -> BTreeMap<SourceKind, usize> {
        let mut counts = BTreeMap::new();
        for file in &self.files {
            *counts.entry(file.metadata.kind).or_default() += 1;
        }
        counts
    }

    pub(crate) fn symbols_for_file(&self, file: &IndexedFile) -> &[IndexedSymbol] {
        if !self.layers.is_empty() {
            return self
                .layers
                .iter()
                .find_map(|layer| {
                    layer
                        .file(file.id)
                        .map(|layer_file| layer.symbols_for_file(layer_file))
                })
                .unwrap_or(&[]);
        }
        &self.symbols[file.symbol_start..file.symbol_start + file.symbol_count]
    }

    fn rebuild_lookup_maps(&mut self) {
        #[cfg(test)]
        {
            self.lookup_map_rebuild_count += 1;
        }
        let LookupMaps {
            general,
            kind_and_owner,
            owner_name,
        } = LookupMaps::build(&self.files, &self.symbols);
        self.by_name = general.by_name;
        self.top_level_by_name = general.top_level_by_name;
        self.top_level_by_folded_name = general.top_level_by_folded_name;
        self.by_kind = general.by_kind;
        self.children = general.children;
        self.classes_by_name = kind_and_owner.kind_names.classes_by_name;
        self.typedefs_by_name = kind_and_owner.kind_names.typedefs_by_name;
        self.functions_by_name = kind_and_owner.kind_names.functions_by_name;
        self.members_by_owner = kind_and_owner.members_by_owner;
        self.methods_by_owner_name = owner_name.methods_by_owner_name;
        self.fields_by_owner_name = owner_name.fields_by_owner_name;
    }

    fn compare_symbol_preference(
        &self,
        left: GlobalSymbolId,
        right: GlobalSymbolId,
    ) -> std::cmp::Ordering {
        let left_file = self.file(left.file_id);
        let right_file = self.file(right.file_id);
        let left_priority = left_file
            .map(|file| file.metadata.priority)
            .unwrap_or_default();
        let right_priority = right_file
            .map(|file| file.metadata.priority)
            .unwrap_or_default();

        right_priority
            .cmp(&left_priority)
            .then_with(|| left.file_id.cmp(&right.file_id))
            .then_with(|| left.symbol_id.cmp(&right.symbol_id))
    }

    fn member_segments_for_class_including_bases(&self, owner: &str) -> Vec<Vec<GlobalSymbolId>> {
        let mut segments = Vec::new();
        let mut visited = BTreeSet::new();
        self.add_member_segments_for_class_including_bases(owner, &mut visited, &mut segments);
        segments
    }

    fn preferred_class_member_segments_including_bases(
        &self,
        owner: &str,
        member_name: Option<&str>,
    ) -> Vec<Vec<GlobalSymbolId>> {
        let mut segments = Vec::new();
        let mut visited = BTreeSet::new();
        self.add_preferred_class_member_segments_including_bases(
            owner,
            member_name,
            &mut visited,
            &mut segments,
        );
        segments
    }

    fn modded_predecessor_member_segments_including_bases(
        &self,
        owner: &str,
        member_name: Option<&str>,
    ) -> Vec<Vec<GlobalSymbolId>> {
        let mut segments = Vec::new();
        let mut visited = BTreeSet::new();
        self.add_modded_predecessor_member_segments_including_bases(
            owner,
            member_name,
            &mut visited,
            &mut segments,
        );
        segments
    }

    fn add_member_segments_for_class_including_bases(
        &self,
        owner: &str,
        visited: &mut BTreeSet<String>,
        segments: &mut Vec<Vec<GlobalSymbolId>>,
    ) {
        if !visited.insert(owner.to_string()) {
            return;
        }

        segments.push(self.members_by_owner(owner).to_vec());

        let Some(base_name) = self.preferred_class_base_name(owner) else {
            return;
        };
        self.add_member_segments_for_class_including_bases(&base_name, visited, segments);
    }

    fn add_preferred_class_member_segments_including_bases(
        &self,
        owner: &str,
        member_name: Option<&str>,
        visited: &mut BTreeSet<String>,
        segments: &mut Vec<Vec<GlobalSymbolId>>,
    ) {
        if !visited.insert(owner.to_string()) {
            return;
        }

        let classes = self.preferred_classes_by_name(owner);
        for class_id in &classes {
            let members = self
                .children(*class_id)
                .iter()
                .copied()
                .filter(|child_id| {
                    self.symbol(*child_id).is_some_and(|symbol| {
                        is_class_member_kind(symbol.kind)
                            && member_name.is_none_or(|member_name| {
                                symbol.name.as_deref() == Some(member_name)
                            })
                    })
                })
                .collect::<Vec<_>>();
            if member_name.is_none() || !members.is_empty() {
                segments.push(members);
            }
        }

        let Some(base_name) = self.first_class_base_name(&classes) else {
            return;
        };
        self.add_preferred_class_member_segments_including_bases(
            &base_name,
            member_name,
            visited,
            segments,
        );
    }

    fn add_modded_predecessor_member_segments_including_bases(
        &self,
        owner: &str,
        member_name: Option<&str>,
        visited: &mut BTreeSet<String>,
        segments: &mut Vec<Vec<GlobalSymbolId>>,
    ) {
        if !visited.insert(owner.to_string()) {
            return;
        }

        let mut classes = self
            .preferred_classes_by_name(owner)
            .into_iter()
            .filter(|class_id| {
                self.file(class_id.file_id)
                    .is_none_or(|file| file.metadata.kind != SourceKind::Workspace)
            })
            .collect::<Vec<_>>();
        let base_name = self.first_class_base_name(&classes);
        // Layered indexes retain canonical load order: the original class is
        // encountered before the overlays that modify it. `super` in the
        // active modded layer begins at the last retained overlay and walks
        // backward toward the original declaration.
        classes.reverse();
        for class_id in &classes {
            let members = self
                .children(*class_id)
                .iter()
                .copied()
                .filter(|child_id| {
                    self.symbol(*child_id).is_some_and(|symbol| {
                        is_class_member_kind(symbol.kind)
                            && member_name.is_none_or(|member_name| {
                                symbol.name.as_deref() == Some(member_name)
                            })
                    })
                })
                .collect::<Vec<_>>();
            if member_name.is_none() || !members.is_empty() {
                segments.push(members);
            }
        }

        let Some(base_name) = base_name else {
            return;
        };
        self.add_modded_predecessor_member_segments_including_bases(
            &base_name,
            member_name,
            visited,
            segments,
        );
    }

    fn preferred_class_base_name(&self, owner: &str) -> Option<String> {
        let class_id = self
            .preferred_from_symbols(self.classes_by_name(owner))
            .first()
            .copied()?;
        self.class_base_name(class_id)
    }

    fn first_class_base_name(&self, class_ids: &[GlobalSymbolId]) -> Option<String> {
        class_ids
            .iter()
            .find_map(|class_id| self.class_base_name(*class_id))
    }

    fn class_base_name(&self, class_id: GlobalSymbolId) -> Option<String> {
        let class = self.symbol(class_id)?;
        let base = class.detail.base_type.as_deref()?.trim();
        if base.is_empty() {
            None
        } else {
            Some(base.to_string())
        }
    }

    pub fn completion_member_key(&self, id: GlobalSymbolId) -> String {
        let Some(symbol) = self.symbol(id) else {
            return format!("Missing:{}:{}", id.file_id.0, id.symbol_id.0);
        };
        let name = symbol.name.as_deref().unwrap_or("<unknown>");

        match symbol.kind {
            SymbolKind::Field => format!("Field {name}"),
            SymbolKind::Method => format!(
                "Method {name}({}) -> {}",
                self.parameter_type_shape(id),
                symbol
                    .detail
                    .return_type_text
                    .as_deref()
                    .unwrap_or("<unknown>")
            ),
            SymbolKind::Constructor => {
                format!("Constructor {name}({})", self.parameter_type_shape(id))
            }
            SymbolKind::Destructor => {
                format!("Destructor {name}({})", self.parameter_type_shape(id))
            }
            _ => format!(
                "{} {name} #{}:{}",
                symbol_kind_key(symbol.kind),
                id.file_id.0,
                id.symbol_id.0
            ),
        }
    }

    fn parameter_type_shape(&self, id: GlobalSymbolId) -> String {
        self.children(id)
            .iter()
            .filter_map(|child_id| self.symbol(*child_id))
            .filter(|child| child.kind == SymbolKind::Parameter)
            .map(|parameter| {
                parameter
                    .detail
                    .type_text
                    .as_deref()
                    .unwrap_or("<unknown>")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(crate) fn indexed_symbol_from_owned_public_symbol(
    file_id: SourceFileId,
    declaration: PublicSymbol,
) -> IndexedSymbol {
    let PublicSymbol {
        id,
        parent,
        kind,
        name,
        detail,
        span,
        selection_span,
        modifiers,
        attributes,
        doc_comments,
        conditional_context,
        callable_form,
        ..
    } = declaration;
    let id = GlobalSymbolId {
        file_id,
        symbol_id: SymbolId(id.0 as usize),
    };
    let parent = parent.map(|parent| GlobalSymbolId {
        file_id,
        symbol_id: SymbolId(parent.0 as usize),
    });
    let crate::semantic_file::PublicSymbolDetail {
        type_text,
        return_type,
        base_type,
        default_value,
        enum_value,
    } = detail;
    let (type_text, type_text_span) = owned_public_text(type_text);
    let (return_type_text, return_type_text_span) = owned_public_text(return_type);
    let (base_type, base_type_span) = owned_public_text(base_type);
    let (default_text, default_text_span) = owned_public_text(default_value);
    let (enum_value_text, enum_value_text_span) = owned_public_text(enum_value);
    IndexedSymbol {
        id,
        parent,
        kind: indexed_symbol_kind(kind),
        name,
        span,
        selection_span,
        detail: IndexedSymbolDetail {
            type_text,
            type_text_span,
            return_type_text,
            return_type_text_span,
            base_type,
            base_type_span,
            default_text,
            default_text_span,
            enum_value_text,
            enum_value_text_span,
        },
        attributes: attributes
            .into_iter()
            .map(|attribute| IndexedAttribute {
                name: semantic_attribute_name(&attribute.text).map(str::to_owned),
                text: attribute.text,
            })
            .collect(),
        modifiers: modifiers.into_iter().map(|value| value.text).collect(),
        doc_comments: doc_comments
            .into_iter()
            .map(|comment| IndexedDocComment {
                kind: match comment.kind {
                    SemanticDocCommentKind::Line => DocCommentKind::Line,
                    SemanticDocCommentKind::Block => DocCommentKind::Block,
                },
                text: comment.text,
            })
            .collect(),
        conditional_context: conditional_context
            .into_iter()
            .map(|branch| IndexedConditionalBranch {
                kind: indexed_conditional_kind(branch.kind),
                condition: branch.condition.map(|value| value.text),
            })
            .collect(),
        callable_form: callable_form.map(indexed_callable_form),
    }
}

fn build_general_lookup_maps(symbols: &[IndexedSymbol]) -> GeneralLookupMaps {
    let (top_level_by_name, top_level_by_folded_name) = build_top_level_lookup_maps(symbols);
    let (by_kind, children) = build_structure_lookup_maps(symbols);
    GeneralLookupMaps {
        by_name: build_name_lookup_map(symbols),
        top_level_by_name,
        top_level_by_folded_name,
        by_kind,
        children,
    }
}

fn build_name_lookup_map(symbols: &[IndexedSymbol]) -> BTreeMap<String, Vec<GlobalSymbolId>> {
    group_borrowed_string_ids(
        symbols
            .iter()
            .filter_map(|symbol| symbol.name.as_deref().map(|name| (name, symbol.id))),
    )
}

fn build_top_level_lookup_maps(
    symbols: &[IndexedSymbol],
) -> (
    BTreeMap<String, Vec<GlobalSymbolId>>,
    BTreeMap<String, Vec<GlobalSymbolId>>,
) {
    let top_level = || {
        symbols.iter().filter_map(|symbol| {
            (symbol.parent.is_none())
                .then(|| symbol.name.as_deref().map(|name| (name, symbol.id)))
                .flatten()
        })
    };
    let top_level_by_name = group_borrowed_string_ids(top_level());
    let top_level_by_folded_name =
        group_owned_string_ids(top_level().map(|(name, id)| (name.to_ascii_lowercase(), id)));
    (top_level_by_name, top_level_by_folded_name)
}

fn build_structure_lookup_maps(
    symbols: &[IndexedSymbol],
) -> (
    BTreeMap<SymbolKind, Vec<GlobalSymbolId>>,
    BTreeMap<GlobalSymbolId, Vec<GlobalSymbolId>>,
) {
    let mut by_kind = BTreeMap::<SymbolKind, Vec<GlobalSymbolId>>::new();
    let mut children = BTreeMap::<GlobalSymbolId, Vec<GlobalSymbolId>>::new();
    for symbol in symbols {
        by_kind.entry(symbol.kind).or_default().push(symbol.id);
        if let Some(parent) = symbol.parent {
            children.entry(parent).or_default().push(symbol.id);
        }
    }
    (by_kind, children)
}

fn build_kind_and_owner_lookup_maps(
    files: &[IndexedFile],
    symbols: &[IndexedSymbol],
) -> KindAndOwnerLookupMaps {
    KindAndOwnerLookupMaps {
        kind_names: build_kind_name_lookup_maps(symbols),
        members_by_owner: build_member_owner_lookup_map(files, symbols),
    }
}

fn build_kind_name_lookup_maps(symbols: &[IndexedSymbol]) -> KindNameLookupMaps {
    let symbols_of_kind = |kind| {
        symbols.iter().filter_map(move |symbol| {
            (symbol.kind == kind)
                .then(|| symbol.name.as_deref().map(|name| (name, symbol.id)))
                .flatten()
        })
    };
    KindNameLookupMaps {
        classes_by_name: group_borrowed_string_ids(symbols_of_kind(SymbolKind::Class)),
        typedefs_by_name: group_borrowed_string_ids(symbols_of_kind(SymbolKind::Typedef)),
        functions_by_name: group_borrowed_string_ids(symbols_of_kind(SymbolKind::Function)),
    }
}

fn build_member_owner_lookup_map(
    files: &[IndexedFile],
    symbols: &[IndexedSymbol],
) -> BTreeMap<String, Vec<GlobalSymbolId>> {
    group_borrowed_string_ids(symbols.iter().filter_map(|symbol| {
        (symbol.name.is_some() && is_class_member_kind(symbol.kind))
            .then(|| {
                parent_class_name_from_parts(files, symbols, symbol).map(|owner| (owner, symbol.id))
            })
            .flatten()
    }))
}

fn build_owner_name_lookup_maps(
    files: &[IndexedFile],
    symbols: &[IndexedSymbol],
) -> OwnerNameLookupMaps {
    OwnerNameLookupMaps {
        methods_by_owner_name: build_method_owner_name_lookup_map(files, symbols),
        fields_by_owner_name: build_field_owner_name_lookup_map(files, symbols),
    }
}

fn build_method_owner_name_lookup_map(
    files: &[IndexedFile],
    symbols: &[IndexedSymbol],
) -> BTreeMap<(String, String), Vec<GlobalSymbolId>> {
    group_borrowed_string_pair_ids(symbols.iter().filter_map(|symbol| {
        (symbol.kind == SymbolKind::Method)
            .then(|| {
                Some((
                    parent_class_name_from_parts(files, symbols, symbol)?,
                    symbol.name.as_deref()?,
                    symbol.id,
                ))
            })
            .flatten()
    }))
}

fn build_field_owner_name_lookup_map(
    files: &[IndexedFile],
    symbols: &[IndexedSymbol],
) -> BTreeMap<(String, String), Vec<GlobalSymbolId>> {
    group_borrowed_string_pair_ids(symbols.iter().filter_map(|symbol| {
        (symbol.kind == SymbolKind::Field)
            .then(|| {
                Some((
                    parent_class_name_from_parts(files, symbols, symbol)?,
                    symbol.name.as_deref()?,
                    symbol.id,
                ))
            })
            .flatten()
    }))
}

fn group_borrowed_string_ids<'a>(
    entries: impl Iterator<Item = (&'a str, GlobalSymbolId)>,
) -> BTreeMap<String, Vec<GlobalSymbolId>> {
    let mut entries = entries.collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    let mut grouped = Vec::<(String, Vec<GlobalSymbolId>)>::new();
    for (key, id) in entries {
        if grouped.last().is_some_and(|(existing, _)| existing == key) {
            grouped.last_mut().expect("group exists").1.push(id);
        } else {
            grouped.push((key.to_string(), vec![id]));
        }
    }
    grouped.into_iter().collect()
}

fn group_owned_string_ids(
    entries: impl Iterator<Item = (String, GlobalSymbolId)>,
) -> BTreeMap<String, Vec<GlobalSymbolId>> {
    let mut entries = entries.collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut grouped = Vec::<(String, Vec<GlobalSymbolId>)>::new();
    for (key, id) in entries {
        if grouped.last().is_some_and(|(existing, _)| existing == &key) {
            grouped.last_mut().expect("group exists").1.push(id);
        } else {
            grouped.push((key, vec![id]));
        }
    }
    grouped.into_iter().collect()
}

fn group_borrowed_string_pair_ids<'a>(
    entries: impl Iterator<Item = (&'a str, &'a str, GlobalSymbolId)>,
) -> BTreeMap<(String, String), Vec<GlobalSymbolId>> {
    let mut entries = entries.collect::<Vec<_>>();
    entries.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    let mut grouped = Vec::<((String, String), Vec<GlobalSymbolId>)>::new();
    for (owner, name, id) in entries {
        if grouped
            .last()
            .is_some_and(|((existing_owner, existing_name), _)| {
                existing_owner == owner && existing_name == name
            })
        {
            grouped.last_mut().expect("group exists").1.push(id);
        } else {
            grouped.push(((owner.to_string(), name.to_string()), vec![id]));
        }
    }
    grouped.into_iter().collect()
}

fn parent_class_name_from_parts<'a>(
    files: &[IndexedFile],
    symbols: &'a [IndexedSymbol],
    symbol: &IndexedSymbol,
) -> Option<&'a str> {
    let parent = symbol.parent?;
    let file = files.get(parent.file_id.0)?;
    let local_index = parent.symbol_id.0;
    if local_index >= file.symbol_count {
        return None;
    }
    let parent_symbol = symbols.get(file.symbol_start + local_index)?;
    (parent_symbol.kind == SymbolKind::Class)
        .then_some(parent_symbol.name.as_deref())
        .flatten()
}

fn folded_top_level_names(
    top_level_by_name: &BTreeMap<String, Vec<GlobalSymbolId>>,
) -> BTreeMap<String, Vec<GlobalSymbolId>> {
    let mut folded = BTreeMap::<String, Vec<GlobalSymbolId>>::new();
    for (name, ids) in top_level_by_name {
        folded
            .entry(name.to_ascii_lowercase())
            .or_default()
            .extend(ids.iter().copied());
    }
    folded
}

/// Wall-clock phases of composing independently cached indexes into one
/// external query surface. This contains no source or path information.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayeredIndexTimings {
    pub rebase: Duration,
    pub file_projection: Duration,
    pub lookup_projection: Duration,
    pub total: Duration,
}

fn rebase_lookup_map_ids<K: Ord>(
    map: &mut BTreeMap<K, Vec<GlobalSymbolId>>,
    rebase: impl Fn(GlobalSymbolId) -> GlobalSymbolId + Copy,
) {
    for ids in map.values_mut() {
        for id in ids {
            *id = rebase(*id);
        }
    }
}

fn append_lookup_map<K: Ord + Clone>(
    target: &mut BTreeMap<K, Vec<GlobalSymbolId>>,
    source: &BTreeMap<K, Vec<GlobalSymbolId>>,
) {
    for (key, ids) in source {
        target
            .entry(key.clone())
            .or_default()
            .extend(ids.iter().copied());
    }
}

fn merge_layer_lookup_maps<K: Ord + Clone>(
    layers: &[SymbolIndex],
    select: impl Fn(&SymbolIndex) -> &BTreeMap<K, Vec<GlobalSymbolId>>,
) -> BTreeMap<K, Vec<GlobalSymbolId>> {
    let mut merged = BTreeMap::new();
    for layer in layers {
        append_lookup_map(&mut merged, select(layer));
    }
    merged
}

fn build_layered_general_lookup_maps(
    layers: &[SymbolIndex],
) -> (
    BTreeMap<String, Vec<GlobalSymbolId>>,
    BTreeMap<String, Vec<GlobalSymbolId>>,
    BTreeMap<String, Vec<GlobalSymbolId>>,
    BTreeMap<SymbolKind, Vec<GlobalSymbolId>>,
    BTreeMap<GlobalSymbolId, Vec<GlobalSymbolId>>,
) {
    (
        merge_layer_lookup_maps(layers, |index| &index.by_name),
        merge_layer_lookup_maps(layers, |index| &index.top_level_by_name),
        merge_layer_lookup_maps(layers, |index| &index.top_level_by_folded_name),
        merge_layer_lookup_maps(layers, |index| &index.by_kind),
        merge_layer_lookup_maps(layers, |index| &index.children),
    )
}

fn build_layered_kind_and_owner_lookup_maps(
    layers: &[SymbolIndex],
) -> (
    BTreeMap<String, Vec<GlobalSymbolId>>,
    BTreeMap<String, Vec<GlobalSymbolId>>,
    BTreeMap<String, Vec<GlobalSymbolId>>,
    BTreeMap<String, Vec<GlobalSymbolId>>,
) {
    (
        merge_layer_lookup_maps(layers, |index| &index.classes_by_name),
        merge_layer_lookup_maps(layers, |index| &index.typedefs_by_name),
        merge_layer_lookup_maps(layers, |index| &index.functions_by_name),
        merge_layer_lookup_maps(layers, |index| &index.members_by_owner),
    )
}

fn build_layered_owner_name_lookup_maps(
    layers: &[SymbolIndex],
) -> (
    BTreeMap<(String, String), Vec<GlobalSymbolId>>,
    BTreeMap<(String, String), Vec<GlobalSymbolId>>,
) {
    (
        merge_layer_lookup_maps(layers, |index| &index.methods_by_owner_name),
        merge_layer_lookup_maps(layers, |index| &index.fields_by_owner_name),
    )
}

fn map_entry_count<K>(map: &BTreeMap<K, Vec<GlobalSymbolId>>) -> usize {
    map.values().map(Vec::len).sum()
}

pub(crate) fn indexed_symbol_kind(kind: SemanticDeclarationKind) -> SymbolKind {
    match kind {
        SemanticDeclarationKind::Class => SymbolKind::Class,
        SemanticDeclarationKind::TypeParameter => SymbolKind::TypeParameter,
        SemanticDeclarationKind::Enum => SymbolKind::Enum,
        SemanticDeclarationKind::EnumMember => SymbolKind::EnumMember,
        SemanticDeclarationKind::Typedef => SymbolKind::Typedef,
        SemanticDeclarationKind::Function => SymbolKind::Function,
        SemanticDeclarationKind::GlobalField => SymbolKind::GlobalField,
        SemanticDeclarationKind::Field => SymbolKind::Field,
        SemanticDeclarationKind::Method => SymbolKind::Method,
        SemanticDeclarationKind::Constructor => SymbolKind::Constructor,
        SemanticDeclarationKind::Destructor => SymbolKind::Destructor,
        SemanticDeclarationKind::Parameter => SymbolKind::Parameter,
        SemanticDeclarationKind::LocalVariable => SymbolKind::LocalVariable,
        SemanticDeclarationKind::PreprocessorMacro => SymbolKind::PreprocessorMacro,
    }
}

pub(crate) fn indexed_callable_form(form: SemanticCallableForm) -> CallableForm {
    match form {
        SemanticCallableForm::Implementation => CallableForm::Implementation,
        SemanticCallableForm::Declaration => CallableForm::Declaration,
        SemanticCallableForm::Prototype => CallableForm::Prototype,
    }
}

pub(crate) fn indexed_conditional_kind(
    kind: SemanticConditionalBranchKind,
) -> PreprocessorBranchKind {
    match kind {
        SemanticConditionalBranchKind::If => PreprocessorBranchKind::If,
        SemanticConditionalBranchKind::Ifdef => PreprocessorBranchKind::Ifdef,
        SemanticConditionalBranchKind::Ifndef => PreprocessorBranchKind::Ifndef,
        SemanticConditionalBranchKind::Elif => PreprocessorBranchKind::Elif,
        SemanticConditionalBranchKind::Else => PreprocessorBranchKind::Else,
    }
}

pub(crate) fn semantic_attribute_name(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let trimmed = trimmed.strip_prefix('[').unwrap_or(trimmed).trim_start();
    let end = trimmed
        .char_indices()
        .take_while(|(_, value)| value.is_ascii_alphanumeric() || *value == '_')
        .map(|(index, value)| index + value.len_utf8())
        .last()?;
    Some(&trimmed[..end])
}

fn owned_public_text(value: Option<PublicText>) -> (Option<String>, Option<TextSpan>) {
    match value {
        Some(PublicText { span, text }) => (Some(text), span),
        None => (None, None),
    }
}

fn is_class_member_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Field | SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Destructor
    )
}

pub(crate) fn parameter_signature_text(symbol: &IndexedSymbol) -> String {
    let mut value = String::new();
    if !symbol.modifiers.is_empty() {
        value.push_str(&symbol.modifiers.join(" "));
    }
    if let Some(type_text) = &symbol.detail.type_text {
        if !value.is_empty() {
            value.push(' ');
        }
        value.push_str(type_text);
    }
    if let Some(name) = &symbol.name {
        if !value.is_empty() {
            value.push(' ');
        }
        value.push_str(name);
    }
    if value.is_empty() {
        value.push_str("<unknown>");
    }
    if let Some(default_text) = &symbol.detail.default_text {
        value.push_str(" = ");
        value.push_str(default_text);
    }
    value
}

fn symbol_kind_key(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "Class",
        SymbolKind::TypeParameter => "TypeParameter",
        SymbolKind::Enum => "Enum",
        SymbolKind::EnumMember => "EnumMember",
        SymbolKind::Typedef => "Typedef",
        SymbolKind::Function => "Function",
        SymbolKind::GlobalField => "GlobalField",
        SymbolKind::Field => "Field",
        SymbolKind::Method => "Method",
        SymbolKind::Constructor => "Constructor",
        SymbolKind::Destructor => "Destructor",
        SymbolKind::Parameter => "Parameter",
        SymbolKind::LocalVariable => "LocalVariable",
        SymbolKind::PreprocessorMacro => "PreprocessorMacro",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexMapCounts {
    pub names: usize,
    pub name_entries: usize,
    pub top_level_names: usize,
    pub top_level_name_entries: usize,
    pub kinds: usize,
    pub kind_entries: usize,
    pub class_names: usize,
    pub class_name_entries: usize,
    pub typedef_names: usize,
    pub typedef_name_entries: usize,
    pub function_names: usize,
    pub function_name_entries: usize,
    pub method_owner_names: usize,
    pub method_owner_name_entries: usize,
    pub field_owner_names: usize,
    pub field_owner_name_entries: usize,
    pub member_owners: usize,
    pub member_owner_entries: usize,
    pub parent_symbols: usize,
    pub child_entries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        source_category_for_path, SourceCategory, SourceFileMetadata, SOURCE_PRIORITY_GAME_DATA,
        SOURCE_PRIORITY_WORKSPACE,
    };
    use crate::parser::parse_source;
    use crate::semantic_file::SemanticFile;
    use std::path::PathBuf;

    #[test]
    fn validated_contribution_ingestion_matches_semantic_file_indexing() {
        let source = r#"typedef int Count;
class Example : Base
{
    int m_Value;
    void Run(string label = "x");
}
void Start();
"#;
        let metadata = SourceFileMetadata::unknown();
        let parse = parse_source(source);
        let semantic_file = SemanticFile::build(source, &parse);

        let mut from_semantic_file = SymbolIndex::default();
        from_semantic_file.add_semantic_file(&semantic_file, metadata.clone());

        let contribution = semantic_file.contribution();
        let mut from_contribution = SymbolIndex::default();
        from_contribution
            .add_file_contribution(&contribution, metadata)
            .unwrap();

        assert_eq!(from_contribution.files(), from_semantic_file.files());
        assert_eq!(from_contribution.symbols(), from_semantic_file.symbols());
        assert_eq!(
            from_contribution.methods_by_owner_name("Example", "Run"),
            from_semantic_file.methods_by_owner_name("Example", "Run")
        );
    }

    #[test]
    fn contribution_ingestion_preserves_public_symbols_after_local_ids() {
        let source =
            include_str!("../../tools/fixtures/index/contribution_public_ids_after_local.c");
        let parse = parse_source(source);
        let semantic_file = SemanticFile::build(source, &parse);
        let contribution = semantic_file.contribution();
        let mut index = SymbolIndex::default();
        index
            .add_file_contribution(&contribution, SourceFileMetadata::unknown())
            .unwrap();

        let later = index.classes_by_name("ContributionIdsAfterPublicFixture")[0];
        assert_eq!(later.symbol_id, SymbolId(2));
        assert_eq!(
            index
                .symbol(later)
                .and_then(|symbol| symbol.name.as_deref()),
            Some("ContributionIdsAfterPublicFixture")
        );
    }

    #[test]
    fn runtime_cache_contribution_projection_matches_full_compaction() {
        let source = r#"class Example : Base
{
	int Run(string name = "ok")
	{
		int localValue;
		return 0;
	}
}
"#;
        let parse = parse_source(source);
        let semantic_file = SemanticFile::build(source, &parse);
        let contribution = semantic_file.contribution();
        let metadata = SourceFileMetadata::unknown();

        let mut full = SymbolIndex::default();
        full.add_file_contributions([(&contribution, metadata.clone())])
            .unwrap();
        let expected = full.compact_for_runtime_cache();

        let mut projected = SymbolIndex::default();
        projected
            .add_runtime_cache_file_contributions([(&contribution, metadata)])
            .unwrap();

        assert_eq!(projected.files(), expected.files());
        assert_eq!(projected.symbols(), expected.symbols());
        assert_eq!(
            projected.callable_signature(projected.methods_by_owner_name("Example", "Run")[0]),
            expected.callable_signature(expected.methods_by_owner_name("Example", "Run")[0])
        );
        assert_no_dangling_symbol_references(&projected);
    }

    #[test]
    fn indexes_names_kinds_children_classes_typedefs_and_methods() {
        let source = r#"typedef string FactionKey;

class Example : Base
{
	int m_Value;
	void Run(int value);
}
"#;
        let catalog = catalog(
            source,
            SourceFileMetadata {
                kind: SourceKind::GameData,
                category: SourceCategory::Unknown,
                absolute_path: Some(PathBuf::from("C:/game/Example.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/game")),
                relative_path: Some(PathBuf::from("Example.c")),
                priority: SOURCE_PRIORITY_GAME_DATA,
            },
        );
        let index = index_from([&catalog]);

        assert_eq!(index.files().len(), 1);
        assert_eq!(index.symbols().len(), 5);
        assert_eq!(index.symbols_for_name("Example").len(), 1);
        assert_eq!(index.top_level_symbols_for_name("Example").len(), 1);
        assert_eq!(index.classes_by_name("Example").len(), 1);
        assert_eq!(index.typedefs_by_name("FactionKey").len(), 1);
        assert!(index.functions_by_name("FactionKey").is_empty());
        assert_eq!(index.methods_by_owner_name("Example", "Run").len(), 1);
        assert_eq!(index.fields_by_owner_name("Example", "m_Value").len(), 1);
        assert_eq!(index.members_by_owner("Example").len(), 2);
        assert_eq!(index.symbols_for_kind(SymbolKind::Parameter).len(), 1);

        let class_id = index.classes_by_name("Example")[0];
        let children = index.children(class_id);
        assert_eq!(children.len(), 2);
        assert!(children.iter().any(|id| index
            .symbol(*id)
            .is_some_and(|symbol| symbol.name.as_deref() == Some("Run"))));
    }

    #[test]
    fn parallel_lookup_map_builds_match_sequential_results() {
        let source = r#"typedef string FactionKey;

void GlobalFn(int value);

class Example : Base
{
	int m_Value;
	void Run(int value);
}
"#;
        let catalog = catalog(source, SourceFileMetadata::unknown());
        let index = index_from([&catalog]);
        let sequential = LookupMaps::build_sequential(index.files(), index.symbols());

        assert_eq!(
            LookupMaps::build_parallel(index.files(), index.symbols(), 3),
            sequential
        );
        assert_eq!(
            LookupMaps::build_parallel(index.files(), index.symbols(), 7),
            sequential
        );
    }

    #[test]
    fn top_level_prefix_lookup_is_ascii_case_insensitive_and_bounded() {
        let source = r#"class SCR_Alpha {}
class scr_Beta {}
class Other {}
"#;
        let catalog = catalog(source, SourceFileMetadata::unknown());
        let index = index_from([&catalog]);

        let names = index
            .top_level_symbols_with_ascii_case_insensitive_prefix("ScR_")
            .filter_map(|id| index.symbol(id).and_then(|symbol| symbol.name.as_deref()))
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["SCR_Alpha", "scr_Beta"]);
        assert!(index
            .top_level_symbols_with_ascii_case_insensitive_prefix("SCR_G")
            .next()
            .is_none());
    }

    #[test]
    fn global_ids_keep_file_id_and_file_local_symbol_id() {
        let game = catalog(
            "class Example {}",
            SourceFileMetadata {
                kind: SourceKind::GameData,
                category: SourceCategory::Unknown,
                absolute_path: Some(PathBuf::from("C:/game/Example.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/game")),
                relative_path: Some(PathBuf::from("Example.c")),
                priority: SOURCE_PRIORITY_GAME_DATA,
            },
        );
        let workspace = catalog(
            "class Example {}",
            SourceFileMetadata {
                kind: SourceKind::Workspace,
                category: SourceCategory::Workspace,
                absolute_path: Some(PathBuf::from("C:/workspace/Example.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/workspace")),
                relative_path: Some(PathBuf::from("Example.c")),
                priority: SOURCE_PRIORITY_WORKSPACE,
            },
        );
        let index = index_from([&game, &workspace]);

        let symbols = index.symbols_for_name("Example");
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].file_id, SourceFileId(0));
        assert_eq!(symbols[0].symbol_id, SymbolId(0));
        assert_eq!(symbols[1].file_id, SourceFileId(1));
        assert_eq!(symbols[1].symbol_id, SymbolId(0));

        let preferred = index.preferred_symbols_for_name("Example");
        assert_eq!(preferred[0].file_id, SourceFileId(1));
        assert_eq!(
            index.file(preferred[0].file_id).unwrap().metadata.kind,
            SourceKind::Workspace
        );
        assert_eq!(index.duplicate_top_level_names().len(), 1);
    }

    #[test]
    fn preferred_top_level_lookup_excludes_non_top_level_symbols() {
        let game = catalog(
            "class Example {}",
            SourceFileMetadata {
                kind: SourceKind::GameData,
                category: SourceCategory::Unknown,
                absolute_path: Some(PathBuf::from("C:/game/Example.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/game")),
                relative_path: Some(PathBuf::from("Example.c")),
                priority: SOURCE_PRIORITY_GAME_DATA,
            },
        );
        let workspace = catalog(
            r#"class Example
{
	void Run(int Example);
}
"#,
            SourceFileMetadata {
                kind: SourceKind::Workspace,
                category: SourceCategory::Workspace,
                absolute_path: Some(PathBuf::from("C:/workspace/Example.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/workspace")),
                relative_path: Some(PathBuf::from("Example.c")),
                priority: SOURCE_PRIORITY_WORKSPACE,
            },
        );
        let index = index_from([&game, &workspace]);

        let all = index.symbols_for_name("Example");
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|id| index
            .symbol(*id)
            .is_some_and(|symbol| symbol.kind == SymbolKind::Parameter)));

        let top_level = index.top_level_symbols_for_name("Example");
        assert_eq!(top_level.len(), 2);
        assert!(top_level.iter().all(|id| index
            .symbol(*id)
            .is_some_and(|symbol| symbol.parent.is_none())));

        let preferred_all = index.preferred_symbols_for_name("Example");
        assert_eq!(preferred_all.len(), 3);

        let preferred_top_level = index.preferred_top_level_symbols_for_name("Example");
        assert_eq!(preferred_top_level.len(), 2);
        assert_eq!(preferred_top_level[0].file_id, SourceFileId(1));
        assert_eq!(
            index.symbol(preferred_top_level[0]).unwrap().kind,
            SymbolKind::Class
        );
        assert_eq!(
            index
                .file(preferred_top_level[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );
    }

    #[test]
    fn stores_copied_lookup_details_without_requiring_source_text() {
        let catalog = catalog(
            r#"enum E
{
	One = 1,
}

class Example : Base
{
	void Run(int value = 4);
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let class = index.symbol(index.classes_by_name("Example")[0]).unwrap();
        assert_eq!(class.detail.base_type.as_deref(), Some("Base"));

        let method = index
            .symbol(index.methods_by_owner_name("Example", "Run")[0])
            .unwrap();
        assert_eq!(method.detail.return_type_text.as_deref(), Some("void"));

        let parameter = index.symbols_for_kind(SymbolKind::Parameter)[0];
        let parameter = index.symbol(parameter).unwrap();
        assert_eq!(parameter.detail.type_text.as_deref(), Some("int"));
        assert_eq!(parameter.detail.default_text.as_deref(), Some("4"));

        let enum_member = index.symbols_for_name("One")[0];
        let enum_member = index.symbol(enum_member).unwrap();
        assert_eq!(enum_member.detail.enum_value_text.as_deref(), Some("1"));
    }

    #[test]
    fn stores_copied_presentation_metadata_without_requiring_source_text() {
        let catalog = catalog(
            r#"//! Class docs
[BaseContainerProps()]
modded class Example
{
	/*! Field docs */
	protected int m_Value;

#ifdef ENABLE_RUN
	override void Run() {}
#endif
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let class = index.symbol(index.classes_by_name("Example")[0]).unwrap();
        assert_eq!(class.modifiers, vec!["modded"]);
        assert_eq!(class.attributes.len(), 1);
        assert_eq!(
            class.attributes[0].name.as_deref(),
            Some("BaseContainerProps")
        );
        assert_eq!(class.attributes[0].text, "[BaseContainerProps()]");
        assert_eq!(class.doc_comments.len(), 1);
        assert_eq!(class.doc_comments[0].kind, DocCommentKind::Line);
        assert_eq!(class.doc_comments[0].text, "//! Class docs");

        let field = index
            .symbol(index.fields_by_owner_name("Example", "m_Value")[0])
            .unwrap();
        assert_eq!(field.modifiers, vec!["protected"]);
        assert_eq!(field.doc_comments.len(), 1);
        assert_eq!(field.doc_comments[0].kind, DocCommentKind::Block);
        assert_eq!(field.doc_comments[0].text, "/*! Field docs */");

        let method = index
            .symbol(index.methods_by_owner_name("Example", "Run")[0])
            .unwrap();
        assert_eq!(method.modifiers, vec!["override"]);
        assert_eq!(method.callable_form, Some(CallableForm::Implementation));
        assert_eq!(method.conditional_context.len(), 1);
        assert_eq!(
            method.conditional_context[0].condition.as_deref(),
            Some("ENABLE_RUN")
        );
    }

    #[test]
    fn exposes_method_owner_name_groups_for_overload_review() {
        let catalog = catalog(
            r#"class SCR_AutotestHarness
{
	void Begin();
	void Begin(int value);
	int Count();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        assert_eq!(
            index
                .methods_by_owner_name("SCR_AutotestHarness", "Begin")
                .len(),
            2
        );
        assert_eq!(
            index
                .methods_by_owner_name("SCR_AutotestHarness", "Count")
                .len(),
            1
        );

        let begin_key = ("SCR_AutotestHarness".to_string(), "Begin".to_string());
        let count_key = ("SCR_AutotestHarness".to_string(), "Count".to_string());
        assert_eq!(index.method_owner_name_groups()[&begin_key].len(), 2);
        assert_eq!(index.method_owner_name_groups()[&count_key].len(), 1);
    }

    #[test]
    fn formats_regular_method_signatures_from_indexed_parameter_children() {
        let catalog = catalog(
            r#"class SCR_BaseGameMode
{
	void OnGameStart();
	void Begin(string suite, string test);
	void Run(int value = 4);
	array<SCR_BaseGameModeComponent> GetComponentsByType(typename componentType, out int foundCount);
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let on_game_start = index.methods_by_owner_name("SCR_BaseGameMode", "OnGameStart")[0];
        let begin = index.methods_by_owner_name("SCR_BaseGameMode", "Begin")[0];
        let run = index.methods_by_owner_name("SCR_BaseGameMode", "Run")[0];
        let get_components =
            index.methods_by_owner_name("SCR_BaseGameMode", "GetComponentsByType")[0];

        assert_eq!(
            index.callable_signature(on_game_start).as_deref(),
            Some("SCR_BaseGameMode.OnGameStart() -> void")
        );
        assert_eq!(
            index.callable_signature(begin).as_deref(),
            Some("SCR_BaseGameMode.Begin(string suite, string test) -> void")
        );
        assert_eq!(
            index.callable_signature(run).as_deref(),
            Some("SCR_BaseGameMode.Run(int value = 4) -> void")
        );
        assert_eq!(
            index.callable_signature(get_components).as_deref(),
            Some("SCR_BaseGameMode.GetComponentsByType(typename componentType, out int foundCount) -> array<SCR_BaseGameModeComponent>")
        );
    }

    #[test]
    fn formats_general_callable_signatures() {
        let catalog = catalog(
            r#"void GlobalFn(int value = 4);

class Example
{
	void Example(int value);
	void ~Example();
	void Run(notnull string name, inout int count);
	int Count();
	int m_Value;
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let global_fn = index.symbols_for_name("GlobalFn")[0];
        let run = index.methods_by_owner_name("Example", "Run")[0];
        let constructor = index
            .members_by_owner("Example")
            .iter()
            .copied()
            .find(|id| index.symbol(*id).unwrap().kind == SymbolKind::Constructor)
            .unwrap();
        let destructor = index
            .members_by_owner("Example")
            .iter()
            .copied()
            .find(|id| index.symbol(*id).unwrap().kind == SymbolKind::Destructor)
            .unwrap();
        let field = index.fields_by_owner_name("Example", "m_Value")[0];

        assert_eq!(
            index.callable_signature(global_fn).as_deref(),
            Some("GlobalFn(int value = 4) -> void")
        );
        assert_eq!(
            index.callable_signature(run).as_deref(),
            Some("Example.Run(notnull string name, inout int count) -> void")
        );
        assert_eq!(
            index.callable_signature(constructor).as_deref(),
            Some("Example(int value)")
        );
        assert_eq!(
            index.callable_signature(destructor).as_deref(),
            Some("~Example()")
        );
        assert_eq!(index.callable_signature(field), None);
    }

    #[test]
    fn indexes_direct_class_fields_and_members_by_owner() {
        let catalog = catalog(
            r#"int m_Value;

class Example
{
	int m_Value;
	void Example();
	void ~Example();
	void Run(int value);
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let fields = index.fields_by_owner_name("Example", "m_Value");
        assert_eq!(fields.len(), 1);
        assert_eq!(index.symbol(fields[0]).unwrap().kind, SymbolKind::Field);

        let members = index.members_by_owner("Example");
        assert_eq!(members.len(), 4);
        assert!(members
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Field));
        assert!(members
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Method));
        assert!(members
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Constructor));
        assert!(members
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Destructor));
        assert!(members
            .iter()
            .all(|id| index.symbol(*id).unwrap().kind != SymbolKind::Parameter));
    }

    #[test]
    fn walks_direct_members_then_exact_name_base_class_members() {
        let catalog = catalog(
            r#"class Base
{
	int m_Base;
	void Run();
}

class Child : Base
{
	int m_Child;
	void Run(int value);
}

class GrandChild : Child
{
	int m_GrandChild;
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let members = index.raw_members_for_class_including_bases("GrandChild");
        let member_names = member_names(&index, &members);

        assert_eq!(
            member_names,
            vec!["m_GrandChild", "m_Child", "Run", "m_Base", "Run"]
        );
        assert_eq!(index.direct_members_by_owner("GrandChild").len(), 1);
        assert_eq!(index.members_by_owner("GrandChild").len(), 1);
    }

    #[test]
    fn inherited_member_lookup_keeps_direct_members_when_base_is_missing() {
        let catalog = catalog(
            r#"class Child : MissingBase
{
	int m_Child;
	void Run();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let members = index.raw_members_for_class_including_bases("Child");

        assert_eq!(member_names(&index, &members), vec!["m_Child", "Run"]);
    }

    #[test]
    fn inherited_member_lookup_stops_on_cycles() {
        let catalog = catalog(
            r#"class A : B
{
	int m_A;
}

class B : A
{
	int m_B;
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let members = index.raw_members_for_class_including_bases("A");

        assert_eq!(member_names(&index, &members), vec!["m_A", "m_B"]);
    }

    #[test]
    fn completion_members_are_direct_first_and_hide_matching_base_members() {
        let catalog = catalog(
            r#"class Base
{
	int m_Value;
	int m_BaseOnly;
	void Run(int value);
	void Run(string value);
	void BaseOnly();
}

class Child : Base
{
	int m_Value;
	void Run(int other);
	void ChildOnly();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let raw = index.raw_members_for_class_including_bases("Child");
        assert_eq!(
            member_names(&index, &raw),
            vec![
                "m_Value",
                "Run",
                "ChildOnly",
                "m_Value",
                "m_BaseOnly",
                "Run",
                "Run",
                "BaseOnly"
            ]
        );

        let completion = index.raw_completion_members_for_owner_name("Child");
        assert_eq!(completion.raw_candidates, raw);
        assert_eq!(
            member_names(&index, &completion.members),
            vec![
                "m_Value",
                "Run",
                "ChildOnly",
                "m_BaseOnly",
                "Run",
                "BaseOnly"
            ]
        );
        assert_eq!(completion.shadowed_groups.len(), 2);
        assert!(completion
            .shadowed_groups
            .iter()
            .any(|group| group.key == "Field m_Value" && group.shadowed.len() == 1));
        assert!(completion
            .shadowed_groups
            .iter()
            .any(|group| group.key == "Method Run(int) -> void" && group.shadowed.len() == 1));
    }

    #[test]
    fn completion_members_do_not_shadow_static_array_fields_by_bound_name() {
        let catalog = catalog(
            r#"class Example
{
	static const int COUNT = 4;
	static const string TAGS[COUNT] = {};
	LocalizedString NAMES[COUNT];
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        assert_eq!(index.fields_by_owner_name("Example", "COUNT").len(), 1);
        assert_eq!(index.fields_by_owner_name("Example", "TAGS").len(), 1);
        assert_eq!(index.fields_by_owner_name("Example", "NAMES").len(), 1);
        assert!(index
            .fields_by_owner_name("Example", "COUNT")
            .iter()
            .all(|id| index.symbol(*id).unwrap().detail.type_text.as_deref() == Some("int")));

        let completion = index.raw_completion_members_for_owner_name("Example");

        assert_eq!(
            member_names(&index, &completion.members),
            vec!["COUNT", "TAGS", "NAMES"]
        );
        assert!(completion.shadowed_groups.is_empty());
    }

    #[test]
    fn indexes_comma_separated_field_declarators_for_lookup_and_completion() {
        let catalog = catalog(
            r#"class Example
{
	protected Widget m_ContentWidget, m_ButtonPrevWidget, m_ButtonNextWidget;
	protected int count, values[COUNT], other = 4;
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        for name in [
            "m_ContentWidget",
            "m_ButtonPrevWidget",
            "m_ButtonNextWidget",
            "count",
            "values",
            "other",
        ] {
            let fields = index.fields_by_owner_name("Example", name);
            assert_eq!(fields.len(), 1, "missing field {name}");
        }

        let content = index.symbol(index.fields_by_owner_name("Example", "m_ContentWidget")[0]);
        assert_eq!(content.unwrap().detail.type_text.as_deref(), Some("Widget"));
        let button_next =
            index.symbol(index.fields_by_owner_name("Example", "m_ButtonNextWidget")[0]);
        assert_eq!(
            button_next.unwrap().detail.type_text.as_deref(),
            Some("Widget")
        );
        let values = index.symbol(index.fields_by_owner_name("Example", "values")[0]);
        assert_eq!(values.unwrap().detail.type_text.as_deref(), Some("int"));

        let completion = index.raw_completion_members_for_owner_name("Example");
        assert_eq!(
            member_names(&index, &completion.members),
            vec![
                "m_ContentWidget",
                "m_ButtonPrevWidget",
                "m_ButtonNextWidget",
                "count",
                "values",
                "other"
            ]
        );
        assert!(completion.shadowed_groups.is_empty());
    }

    #[test]
    fn completion_members_prefer_workspace_within_same_owner_depth() {
        let game = catalog(
            r#"class SCR_BaseGameMode
{
	void OnGameStart();
	void GameOnly();
}
"#,
            game_metadata("SCR_BaseGameMode.c"),
        );
        let workspace = catalog(
            r#"modded class SCR_BaseGameMode
{
	override void OnGameStart();
	void WorkspaceOnly();
}
"#,
            workspace_metadata("SCR_BaseGameMode.c"),
        );
        let index = index_from([&game, &workspace]);

        let raw = index.raw_members_for_class_including_bases("SCR_BaseGameMode");
        assert_eq!(
            member_names(&index, &raw),
            vec!["OnGameStart", "GameOnly", "OnGameStart", "WorkspaceOnly"]
        );

        let completion = index.raw_completion_members_for_owner_name("SCR_BaseGameMode");
        assert_eq!(completion.raw_candidates, raw);
        assert_eq!(
            member_names(&index, &completion.members),
            vec!["OnGameStart", "GameOnly", "WorkspaceOnly"]
        );

        let kept_on_game_start = completion
            .members
            .iter()
            .copied()
            .find(|id| index.symbol(*id).unwrap().name.as_deref() == Some("OnGameStart"))
            .unwrap();
        assert_eq!(
            index
                .file(kept_on_game_start.file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );

        let shadow_group = completion
            .shadowed_groups
            .iter()
            .find(|group| group.key == "Method OnGameStart() -> void")
            .unwrap();
        assert_eq!(shadow_group.kept, kept_on_game_start);
        assert_eq!(shadow_group.shadowed.len(), 1);
        assert_eq!(
            index
                .file(shadow_group.shadowed[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::GameData
        );
    }

    #[test]
    fn preferred_class_completion_uses_preferred_declaration_then_overlay_then_bases() {
        let base = catalog(
            r#"class BaseGameMode
{
	void BaseOnly();
}
"#,
            game_metadata("BaseGameMode.c"),
        );
        let game = catalog(
            r#"class SCR_BaseGameMode : BaseGameMode
{
	void OnGameStart();
	void GameOnly();
}
"#,
            game_metadata("SCR_BaseGameMode.c"),
        );
        let workspace = catalog(
            r#"modded class SCR_BaseGameMode
{
	override void OnGameStart();
	void WorkspaceOnly();
}
"#,
            workspace_metadata("SCR_BaseGameMode.c"),
        );
        let index = index_from([&base, &game, &workspace]);

        let raw = index.raw_completion_members_for_owner_name("SCR_BaseGameMode");
        assert_eq!(
            member_names(&index, &raw.members),
            vec!["OnGameStart", "GameOnly", "WorkspaceOnly"]
        );

        let completion = index.completion_members_for_preferred_class("SCR_BaseGameMode");
        assert_eq!(
            member_names(&index, &completion.raw_candidates),
            vec![
                "OnGameStart",
                "WorkspaceOnly",
                "OnGameStart",
                "GameOnly",
                "BaseOnly"
            ]
        );
        assert_eq!(
            member_names(&index, &completion.members),
            vec!["OnGameStart", "WorkspaceOnly", "GameOnly", "BaseOnly"]
        );

        let kept_on_game_start = completion
            .members
            .iter()
            .copied()
            .find(|id| index.symbol(*id).unwrap().name.as_deref() == Some("OnGameStart"))
            .unwrap();
        assert_eq!(
            index
                .file(kept_on_game_start.file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );

        let shadow_group = completion
            .shadowed_groups
            .iter()
            .find(|group| group.key == "Method OnGameStart() -> void")
            .unwrap();
        assert_eq!(shadow_group.kept, kept_on_game_start);
        assert_eq!(shadow_group.shadowed.len(), 1);
        assert_eq!(
            index
                .file(shadow_group.shadowed[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::GameData
        );
    }

    #[test]
    fn preferred_named_members_match_the_full_completion_projection() {
        let base = catalog(
            r#"class BaseGameMode
{
	void BaseOnly();
	void OnGameStart();
}
"#,
            game_metadata("BaseGameMode.c"),
        );
        let game = catalog(
            r#"class SCR_BaseGameMode : BaseGameMode
{
	void OnGameStart();
	void GameOnly();
}
"#,
            game_metadata("SCR_BaseGameMode.c"),
        );
        let workspace = catalog(
            r#"modded class SCR_BaseGameMode
{
	override void OnGameStart();
	void WorkspaceOnly();
}
"#,
            workspace_metadata("SCR_BaseGameMode.c"),
        );
        let index = index_from([&base, &game, &workspace]);

        let full = index.completion_members_for_preferred_class("SCR_BaseGameMode");
        let named = index.preferred_members_named_for_class("SCR_BaseGameMode", "OnGameStart");
        let expected_members = full
            .members
            .iter()
            .copied()
            .filter(|id| index.symbol(*id).unwrap().name.as_deref() == Some("OnGameStart"))
            .collect::<Vec<_>>();

        assert_eq!(named, expected_members);
    }

    #[test]
    fn preferred_class_completion_keeps_direct_overlay_before_higher_priority_base_members() {
        let base = catalog(
            r#"class Base
{
	void Run();
}
"#,
            workspace_metadata("Base.c"),
        );
        let child = catalog(
            r#"class Child : Base
{
	void Run();
}
"#,
            game_metadata("Child.c"),
        );
        let index = index_from([&base, &child]);

        let completion = index.completion_members_for_preferred_class("Child");
        let run = completion.members[0];

        assert_eq!(index.symbol(run).unwrap().name.as_deref(), Some("Run"));
        assert_eq!(
            index.file(run.file_id).unwrap().metadata.kind,
            SourceKind::GameData
        );
        assert_eq!(completion.shadowed_groups.len(), 1);
        assert_eq!(completion.shadowed_groups[0].kept, run);
        assert_eq!(
            index
                .file(completion.shadowed_groups[0].shadowed[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );
    }

    #[test]
    fn preferred_class_completion_keeps_direct_members_when_base_is_missing() {
        let catalog = catalog(
            r#"class Child : MissingBase
{
	int m_Child;
	void Run();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let completion = index.completion_members_for_preferred_class("Child");

        assert_eq!(
            member_names(&index, &completion.members),
            vec!["m_Child", "Run"]
        );
        assert!(completion.shadowed_groups.is_empty());
    }

    #[test]
    fn preferred_class_completion_stops_on_cycles() {
        let catalog = catalog(
            r#"class A : B
{
	int m_A;
	void Run();
}

class B : A
{
	int m_B;
	void Run();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let completion = index.completion_members_for_preferred_class("A");

        assert_eq!(
            member_names(&index, &completion.members),
            vec!["m_A", "Run", "m_B"]
        );
        assert_eq!(completion.shadowed_groups.len(), 1);
        assert_eq!(completion.shadowed_groups[0].key, "Method Run() -> void");
    }

    #[test]
    fn completion_members_keep_direct_depth_before_higher_priority_base_members() {
        let game_base = catalog(
            r#"class Base
{
	void Run();
}
"#,
            workspace_metadata("Base.c"),
        );
        let game_child = catalog(
            r#"class Child : Base
{
	void Run();
}
"#,
            game_metadata("Child.c"),
        );
        let index = index_from([&game_base, &game_child]);

        let completion = index.raw_completion_members_for_owner_name("Child");
        let run = completion.members[0];

        assert_eq!(index.symbol(run).unwrap().name.as_deref(), Some("Run"));
        assert_eq!(
            index.file(run.file_id).unwrap().metadata.kind,
            SourceKind::GameData
        );
        assert_eq!(completion.shadowed_groups.len(), 1);
        assert_eq!(completion.shadowed_groups[0].kept, run);
        assert_eq!(
            index
                .file(completion.shadowed_groups[0].shadowed[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );
    }

    #[test]
    fn completion_members_keep_direct_members_when_base_is_missing() {
        let catalog = catalog(
            r#"class Child : MissingBase
{
	int m_Child;
	void Run();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let completion = index.raw_completion_members_for_owner_name("Child");

        assert_eq!(
            member_names(&index, &completion.members),
            vec!["m_Child", "Run"]
        );
        assert!(completion.shadowed_groups.is_empty());
    }

    #[test]
    fn completion_member_lookup_stops_on_cycles() {
        let catalog = catalog(
            r#"class A : B
{
	int m_A;
	void Run();
}

class B : A
{
	int m_B;
	void Run();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let completion = index.raw_completion_members_for_owner_name("A");

        assert_eq!(
            member_names(&index, &completion.members),
            vec!["m_A", "Run", "m_B"]
        );
        assert_eq!(completion.shadowed_groups.len(), 1);
        assert_eq!(completion.shadowed_groups[0].key, "Method Run() -> void");
    }

    #[test]
    fn completion_member_lookup_deduplicates_constructor_and_destructor_shapes() {
        let catalog = catalog(
            r#"class Base
{
	void Base(int value);
	void ~Base();
}

class Child : Base
{
	void Child(int value);
	void ~Child();
}
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        let completion = index.raw_completion_members_for_owner_name("Child");

        assert_eq!(
            member_names(&index, &completion.members),
            vec!["Child", "Child", "Base", "Base"]
        );
        assert!(completion.shadowed_groups.is_empty());
    }

    #[test]
    fn preferred_from_symbols_sorts_by_priority_then_stable_ids() {
        let first_game = catalog(
            "class Example {}",
            SourceFileMetadata {
                kind: SourceKind::GameData,
                category: SourceCategory::Unknown,
                absolute_path: Some(PathBuf::from("C:/game/First.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/game")),
                relative_path: Some(PathBuf::from("First.c")),
                priority: SOURCE_PRIORITY_GAME_DATA,
            },
        );
        let workspace = catalog(
            "class Example {}",
            SourceFileMetadata {
                kind: SourceKind::Workspace,
                category: SourceCategory::Workspace,
                absolute_path: Some(PathBuf::from("C:/workspace/Example.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/workspace")),
                relative_path: Some(PathBuf::from("Example.c")),
                priority: SOURCE_PRIORITY_WORKSPACE,
            },
        );
        let second_game = catalog(
            "class Example {}",
            SourceFileMetadata {
                kind: SourceKind::GameData,
                category: SourceCategory::Unknown,
                absolute_path: Some(PathBuf::from("C:/game/Second.c")),
                virtual_source: None,
                root_path: Some(PathBuf::from("C:/game")),
                relative_path: Some(PathBuf::from("Second.c")),
                priority: SOURCE_PRIORITY_GAME_DATA,
            },
        );
        let index = index_from([&first_game, &workspace, &second_game]);
        let unsorted = [
            GlobalSymbolId {
                file_id: SourceFileId(2),
                symbol_id: SymbolId(0),
            },
            GlobalSymbolId {
                file_id: SourceFileId(0),
                symbol_id: SymbolId(0),
            },
            GlobalSymbolId {
                file_id: SourceFileId(1),
                symbol_id: SymbolId(0),
            },
        ];

        let preferred = index.preferred_from_symbols(&unsorted);

        assert_eq!(preferred[0].file_id, SourceFileId(1));
        assert_eq!(preferred[1].file_id, SourceFileId(0));
        assert_eq!(preferred[2].file_id, SourceFileId(2));
    }

    #[test]
    fn workspace_modded_class_is_preferred_over_game_data_class() {
        let game = catalog(
            "class SCR_BaseGameMode {}",
            game_metadata("SCR_BaseGameMode.c"),
        );
        let workspace = catalog(
            "modded class SCR_BaseGameMode {}",
            workspace_metadata("SCR_BaseGameMode.c"),
        );
        let index = index_from([&game, &workspace]);

        let classes = index.classes_by_name("SCR_BaseGameMode");
        assert_eq!(classes.len(), 2);

        let preferred = index.preferred_from_symbols(classes);
        let preferred_symbol = index.symbol(preferred[0]).unwrap();
        let preferred_file = index.file(preferred[0].file_id).unwrap();

        assert_eq!(preferred_symbol.kind, SymbolKind::Class);
        assert_eq!(preferred_symbol.name.as_deref(), Some("SCR_BaseGameMode"));
        assert_eq!(preferred_file.metadata.kind, SourceKind::Workspace);
        assert_eq!(preferred_file.metadata.priority, SOURCE_PRIORITY_WORKSPACE);
    }

    #[test]
    fn top_level_lookup_ignores_fields_and_parameters_with_same_name() {
        let catalog = catalog(
            r#"class SharedName
{
	int SharedName;
	void Run(int SharedName);
}
"#,
            workspace_metadata("SharedName.c"),
        );
        let index = index_from([&catalog]);

        let all = index.symbols_for_name("SharedName");
        assert_eq!(all.len(), 3);
        assert!(all
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Class));
        assert!(all
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Field));
        assert!(all
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Parameter));

        let top_level = index.top_level_symbols_for_name("SharedName");
        assert_eq!(top_level.len(), 1);
        assert_eq!(index.symbol(top_level[0]).unwrap().kind, SymbolKind::Class);

        let preferred = index.preferred_top_level_symbols_for_name("SharedName");
        assert_eq!(preferred, top_level);
    }

    #[test]
    fn local_variables_are_indexed_for_name_lookup_but_not_member_completion() {
        let catalog = catalog(
            r#"class Example
{
	int value;
	void Run(int value)
	{
		int value = 4;
	}
}
"#,
            workspace_metadata("Example.c"),
        );
        let index = index_from([&catalog]);

        let all = index.symbols_for_name("value");
        assert_eq!(all.len(), 3);
        assert!(all
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Field));
        assert!(all
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::Parameter));
        assert!(all
            .iter()
            .any(|id| index.symbol(*id).unwrap().kind == SymbolKind::LocalVariable));

        assert!(index.top_level_symbols_for_name("value").is_empty());
        assert_eq!(index.fields_by_owner_name("Example", "value").len(), 1);
        assert!(index
            .members_by_owner("Example")
            .iter()
            .all(|id| index.symbol(*id).unwrap().kind != SymbolKind::LocalVariable));
        assert!(index
            .raw_completion_members_for_owner_name("Example")
            .members
            .iter()
            .all(|id| index.symbol(*id).unwrap().kind != SymbolKind::LocalVariable));
    }

    #[test]
    fn method_owner_lookup_aggregates_game_data_and_workspace_methods() {
        let game = catalog(
            r#"class SCR_BaseGameMode
{
	void OnGameStart();
}
"#,
            game_metadata("SCR_BaseGameMode.c"),
        );
        let workspace = catalog(
            r#"modded class SCR_BaseGameMode
{
	override void OnGameStart();
}
"#,
            workspace_metadata("SCR_BaseGameMode.c"),
        );
        let index = index_from([&game, &workspace]);

        let methods = index.methods_by_owner_name("SCR_BaseGameMode", "OnGameStart");
        assert_eq!(methods.len(), 2);

        let preferred = index.preferred_from_symbols(methods);
        let preferred_symbol = index.symbol(preferred[0]).unwrap();
        let preferred_file = index.file(preferred[0].file_id).unwrap();

        assert_eq!(preferred_symbol.kind, SymbolKind::Method);
        assert_eq!(preferred_symbol.name.as_deref(), Some("OnGameStart"));
        assert_eq!(preferred_file.metadata.kind, SourceKind::Workspace);
        assert_eq!(preferred_file.metadata.priority, SOURCE_PRIORITY_WORKSPACE);
    }

    #[test]
    fn duplicate_top_level_conflict_records_include_review_metadata() {
        let catalog = catalog(
            r#"typedef string FactionKey;
class FactionKey : string {}
"#,
            game_metadata("GameCode/Faction/FactionKey.c"),
        );
        let index = index_from([&catalog]);

        let duplicates = index.duplicate_top_level_names();
        let faction_key = duplicates
            .iter()
            .find(|(name, _)| *name == "FactionKey")
            .expect("FactionKey should be a duplicate top-level name");
        assert_eq!(faction_key.1.len(), 2);

        let kinds = faction_key
            .1
            .iter()
            .map(|id| index.symbol(*id).unwrap().kind)
            .collect::<Vec<_>>();
        assert!(kinds.contains(&SymbolKind::Typedef));
        assert!(kinds.contains(&SymbolKind::Class));

        for id in faction_key.1 {
            let file = index.file(id.file_id).unwrap();
            assert_eq!(file.metadata.kind, SourceKind::GameData);
            assert_eq!(file.metadata.priority, SOURCE_PRIORITY_GAME_DATA);
            assert_eq!(
                file.metadata.relative_path.as_deref(),
                Some(std::path::Path::new("GameCode/Faction/FactionKey.c"))
            );
        }
    }

    #[test]
    fn preferred_kind_specific_top_level_lookup_separates_conflict_kinds() {
        let catalog = catalog(
            r#"typedef string FactionKey;
class FactionKey : string {}
void FactionKey(int value);
"#,
            workspace_metadata("GameCode/Faction/FactionKey.c"),
        );
        let index = index_from([&catalog]);

        let generic = index.preferred_top_level_symbols_for_name("FactionKey");
        assert_eq!(generic.len(), 3);
        assert!(generic.iter().any(|id| index
            .symbol(*id)
            .is_some_and(|symbol| symbol.kind == SymbolKind::Class)));
        assert!(generic.iter().any(|id| index
            .symbol(*id)
            .is_some_and(|symbol| symbol.kind == SymbolKind::Typedef)));
        assert!(generic.iter().any(|id| index
            .symbol(*id)
            .is_some_and(|symbol| symbol.kind == SymbolKind::Function)));

        let preferred_class = index.preferred_classes_by_name("FactionKey");
        let preferred_typedef = index.preferred_typedefs_by_name("FactionKey");
        let preferred_function = index.preferred_functions_by_name("FactionKey");

        assert_eq!(preferred_class.len(), 1);
        assert_eq!(preferred_typedef.len(), 1);
        assert_eq!(preferred_function.len(), 1);
        assert_eq!(
            index.symbol(preferred_class[0]).unwrap().kind,
            SymbolKind::Class
        );
        assert_eq!(
            index.symbol(preferred_typedef[0]).unwrap().kind,
            SymbolKind::Typedef
        );
        assert_eq!(
            index.symbol(preferred_function[0]).unwrap().kind,
            SymbolKind::Function
        );
    }

    #[test]
    fn preferred_kind_specific_lookup_uses_workspace_priority_within_kind() {
        let game = catalog(
            r#"class Example {}
typedef int ExampleAlias;
void ExampleFn();
"#,
            game_metadata("Game.c"),
        );
        let workspace = catalog(
            r#"modded class Example {}
typedef float ExampleAlias;
void ExampleFn(int value);
"#,
            workspace_metadata("Workspace.c"),
        );
        let index = index_from([&game, &workspace]);

        let preferred_class = index.preferred_classes_by_name("Example");
        let preferred_typedef = index.preferred_typedefs_by_name("ExampleAlias");
        let preferred_function = index.preferred_functions_by_name("ExampleFn");

        for id in [
            preferred_class[0],
            preferred_typedef[0],
            preferred_function[0],
        ] {
            assert_eq!(
                index.file(id.file_id).unwrap().metadata.kind,
                SourceKind::Workspace
            );
        }
        assert_eq!(
            index.symbol(preferred_class[0]).unwrap().kind,
            SymbolKind::Class
        );
        assert_eq!(
            index.symbol(preferred_typedef[0]).unwrap().kind,
            SymbolKind::Typedef
        );
        assert_eq!(
            index.symbol(preferred_function[0]).unwrap().kind,
            SymbolKind::Function
        );
    }

    #[test]
    fn function_lookup_excludes_top_level_classes_and_typedefs_with_same_name() {
        let catalog = catalog(
            r#"typedef string Shared;
class Shared {}
void Shared();
"#,
            SourceFileMetadata::unknown(),
        );
        let index = index_from([&catalog]);

        assert_eq!(index.top_level_symbols_for_name("Shared").len(), 3);
        assert_eq!(index.functions_by_name("Shared").len(), 1);
        assert_eq!(
            index
                .symbol(index.functions_by_name("Shared")[0])
                .unwrap()
                .kind,
            SymbolKind::Function
        );
    }

    #[test]
    fn overlay_index_prefers_workspace_symbols_over_game_data() {
        let game = catalog(
            r#"class SCR_BaseGameMode
{
	void OnGameStart();
}

typedef string FactionKey;
"#,
            game_metadata("Game.c"),
        );
        let workspace = catalog(
            r#"modded class SCR_BaseGameMode
{
	override void OnGameStart();
}

class FactionKey : string
{
}
"#,
            workspace_metadata("Workspace.c"),
        );
        let index = index_from([&game, &workspace]);

        let source_counts = index.source_kind_counts();
        assert_eq!(source_counts.get(&SourceKind::GameData), Some(&1));
        assert_eq!(source_counts.get(&SourceKind::Workspace), Some(&1));

        let preferred_class =
            index.preferred_from_symbols(index.classes_by_name("SCR_BaseGameMode"));
        assert_eq!(
            index
                .file(preferred_class[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );

        let preferred_top_level = index.preferred_top_level_symbols_for_name("FactionKey");
        assert_eq!(
            index
                .file(preferred_top_level[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );
        assert_eq!(
            index.symbol(preferred_top_level[0]).unwrap().kind,
            SymbolKind::Class
        );

        let preferred_method = index
            .preferred_from_symbols(index.methods_by_owner_name("SCR_BaseGameMode", "OnGameStart"));
        assert_eq!(
            index
                .file(preferred_method[0].file_id)
                .unwrap()
                .metadata
                .kind,
            SourceKind::Workspace
        );
        assert_eq!(
            index.symbol(preferred_method[0]).unwrap().kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn merged_indexes_preserve_file_symbol_ranges_and_parent_links() {
        let first = index_from([&catalog(
            "class First { void FirstMethod(); }",
            game_metadata("Game/First.c"),
        )]);
        let second = index_from([&catalog(
            "class Second { void SecondMethod(); }",
            workspace_metadata("Scripts/Second.c"),
        )]);

        let merged = SymbolIndex::merged([&first, &second]);

        assert_eq!(
            member_names(
                &merged,
                &merged
                    .completion_members_for_preferred_class("First")
                    .members
            ),
            vec!["FirstMethod"]
        );
        assert_eq!(
            member_names(
                &merged,
                &merged
                    .completion_members_for_preferred_class("Second")
                    .members
            ),
            vec!["SecondMethod"]
        );
        assert_no_dangling_symbol_references(&merged);
    }

    #[test]
    fn layered_indexes_route_global_ids_without_copying_symbols() {
        let first = index_from([&catalog(
            "class First { void FirstMethod(); }",
            game_metadata("Game/First.c"),
        )]);
        let second = index_from([&catalog(
            "class Second { void SecondMethod(); }",
            game_metadata("Game/Second.c"),
        )]);
        let first_symbol_count = first.symbols.len();
        let second_symbol_count = second.symbols.len();

        let layered = SymbolIndex::layered([first, second]);

        assert_eq!(layered.symbols.len(), 0, "layers retain symbol records");
        assert_eq!(layered.files().len(), 2);
        assert_eq!(layered.symbols_for_name("First").len(), 1);
        assert_eq!(layered.symbols_for_name("Second").len(), 1);
        assert_eq!(
            layered
                .completion_members_for_preferred_class("Second")
                .members
                .len(),
            1
        );
        assert_eq!(
            layered
                .symbol(layered.symbols_for_name("Second")[0])
                .and_then(|symbol| symbol.name.as_deref()),
            Some("Second")
        );
        assert_eq!(
            layered
                .layers
                .iter()
                .map(|layer| layer.symbols.len())
                .sum::<usize>(),
            first_symbol_count + second_symbol_count
        );
    }

    #[test]
    fn parallel_layer_lookup_projection_matches_sequential_projection() {
        let indexes = vec![
            index_from([&catalog(
                "class First { int m_Value; void FirstMethod(); }",
                game_metadata("Game/First.c"),
            )]),
            index_from([&catalog(
                "typedef int SecondType; class Second { void SecondMethod(); }",
                game_metadata("Game/Second.c"),
            )]),
        ];
        let sequential = SymbolIndex::layered(indexes.clone());

        let mut coarse_parallel = SymbolIndex::layered(indexes.clone());
        coarse_parallel.rebuild_layered_lookup_maps_coarse_parallel();
        assert_layered_lookup_maps_match(&coarse_parallel, &sequential);

        let mut wide_parallel = SymbolIndex::layered(indexes);
        wide_parallel.rebuild_layered_lookup_maps_wide_parallel();
        assert_layered_lookup_maps_match(&wide_parallel, &sequential);
    }

    #[test]
    fn pruned_index_removes_local_variables_and_preserves_parameters() {
        let catalog = catalog(
            r#"class Example
{
	int m_Value;
	void Run(int value)
	{
		int localValue = value;
		string localName = "ok";
	}
}
"#,
            game_metadata("Game/Example.c"),
        );
        let index = index_from([&catalog]);
        let pruned = index.without_local_variables();

        assert_eq!(index.symbols_for_kind(SymbolKind::LocalVariable).len(), 2);
        assert!(pruned
            .symbols_for_kind(SymbolKind::LocalVariable)
            .is_empty());
        assert_eq!(pruned.symbols_for_kind(SymbolKind::Parameter).len(), 1);
        assert_eq!(pruned.symbols_for_name("localValue").len(), 0);
        assert_eq!(pruned.symbols_for_name("localName").len(), 0);
        assert_eq!(pruned.symbols_for_name("value").len(), 1);
        assert_eq!(pruned.classes_by_name("Example").len(), 1);
        assert_eq!(pruned.fields_by_owner_name("Example", "m_Value").len(), 1);
        assert_eq!(pruned.methods_by_owner_name("Example", "Run").len(), 1);
        assert_eq!(
            pruned
                .callable_signature(pruned.methods_by_owner_name("Example", "Run")[0])
                .as_deref(),
            Some("Example.Run(int value) -> void")
        );
        assert_no_dangling_symbol_references(&pruned);
    }

    #[test]
    fn runtime_cache_compaction_removes_locals_and_detail_spans_only() {
        let catalog = catalog(
            r#"class Example : BaseExample
{
	ref array<int> m_Values;

	int Run(string name = "ok")
	{
		int localValue;
		return 0;
	}
}
"#,
            game_metadata("Game/Example.c"),
        );
        let index = index_from([&catalog]);
        let reference = index.compact_for_runtime_cache();
        let compact = index.clone().into_runtime_cache().unwrap();
        assert_eq!(compact.files(), reference.files());
        assert_eq!(compact.symbols(), reference.symbols());

        assert_eq!(index.symbols_for_kind(SymbolKind::LocalVariable).len(), 1);
        assert!(compact
            .symbols_for_kind(SymbolKind::LocalVariable)
            .is_empty());
        assert_eq!(compact.symbols_for_kind(SymbolKind::Parameter).len(), 1);

        let class = compact
            .symbol(compact.classes_by_name("Example")[0])
            .unwrap();
        assert_eq!(class.detail.base_type.as_deref(), Some("BaseExample"));
        assert!(class.detail.base_type_span.is_none());

        let field = compact
            .symbol(compact.fields_by_owner_name("Example", "m_Values")[0])
            .unwrap();
        assert_eq!(field.detail.type_text.as_deref(), Some("ref array<int>"));
        assert!(field.detail.type_text_span.is_none());

        let method = compact
            .symbol(compact.methods_by_owner_name("Example", "Run")[0])
            .unwrap();
        assert_eq!(method.detail.return_type_text.as_deref(), Some("int"));
        assert!(method.detail.return_type_text_span.is_none());
        assert_eq!(
            compact.callable_signature(method.id).as_deref(),
            Some("Example.Run(string name = \"ok\") -> int")
        );

        let parameter = compact.symbols_for_name("name")[0];
        let parameter = compact.symbol(parameter).unwrap();
        assert_eq!(parameter.detail.type_text.as_deref(), Some("string"));
        assert_eq!(parameter.detail.default_text.as_deref(), Some("\"ok\""));
        assert!(parameter.detail.type_text_span.is_none());
        assert!(parameter.detail.default_text_span.is_none());

        assert_no_dangling_symbol_references(&compact);
    }

    #[test]
    fn runtime_cache_compaction_preserves_multi_file_symbol_ranges() {
        let first = catalog(
            r#"class First
{
	void Run()
	{
		int localValue;
	}
}
"#,
            game_metadata("Game/First.c"),
        );
        let second = catalog(
            r#"class SecondBase {}
class Second : SecondBase
{
	int m_Value;
}
"#,
            game_metadata("Game/Second.c"),
        );
        let index = index_from([&first, &second]);
        let reference = index.compact_for_runtime_cache();
        let compact = index.into_runtime_cache().unwrap();
        assert_eq!(compact.files(), reference.files());
        assert_eq!(compact.symbols(), reference.symbols());

        assert!(compact
            .symbols_for_kind(SymbolKind::LocalVariable)
            .is_empty());

        let second_id = compact.classes_by_name("Second")[0];
        let second_symbol = compact.symbol(second_id).unwrap();
        assert_eq!(second_symbol.name.as_deref(), Some("Second"));
        assert_eq!(second_symbol.kind, SymbolKind::Class);
        assert_eq!(
            second_symbol.detail.base_type.as_deref(),
            Some("SecondBase")
        );

        let field_id = compact.fields_by_owner_name("Second", "m_Value")[0];
        let field = compact.symbol(field_id).unwrap();
        assert_eq!(field.name.as_deref(), Some("m_Value"));
        assert_eq!(field.kind, SymbolKind::Field);

        let first_file = compact.file(SourceFileId(0)).unwrap();
        let second_file = compact.file(SourceFileId(1)).unwrap();
        assert_eq!(
            first_file.symbol_start + first_file.symbol_count,
            second_file.symbol_start
        );
        assert_no_dangling_symbol_references(&compact);
    }

    #[test]
    fn pruned_index_remaps_file_local_symbol_ids() {
        let catalog = catalog(
            r#"class Example
{
	void First()
	{
		int localValue;
	}

	void Second(string name);
}
"#,
            game_metadata("Game/Example.c"),
        );
        let pruned = index_from([&catalog]).without_local_variables();

        let first = pruned.methods_by_owner_name("Example", "First")[0];
        let second = pruned.methods_by_owner_name("Example", "Second")[0];
        assert_ne!(first.symbol_id, second.symbol_id);
        assert_eq!(
            pruned.callable_signature(second).as_deref(),
            Some("Example.Second(string name) -> void")
        );
        for file in pruned.files() {
            for local_id in 0..file.symbol_count {
                let id = GlobalSymbolId {
                    file_id: file.id,
                    symbol_id: SymbolId(local_id),
                };
                assert!(pruned.symbol(id).is_some());
            }
        }
        assert_no_dangling_symbol_references(&pruned);
    }

    struct TestSemanticFile {
        semantic: SemanticFile,
        metadata: SourceFileMetadata,
    }

    fn catalog(source: &str, metadata: SourceFileMetadata) -> TestSemanticFile {
        let parse = parse_source(source);
        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        TestSemanticFile {
            semantic: SemanticFile::build(source, &parse),
            metadata,
        }
    }

    fn index_from<'a>(files: impl IntoIterator<Item = &'a TestSemanticFile>) -> SymbolIndex {
        SymbolIndex::from_semantic_files(
            files
                .into_iter()
                .map(|file| (&file.semantic, file.metadata.clone())),
        )
    }

    fn game_metadata(path: &str) -> SourceFileMetadata {
        let relative_path = PathBuf::from(path);
        let mut category = source_category_for_path(SourceKind::GameData, Some(&relative_path));
        if category == SourceCategory::Unknown {
            category = SourceCategory::Game;
        }
        SourceFileMetadata {
            kind: SourceKind::GameData,
            category,
            absolute_path: Some(PathBuf::from("C:/game").join(path)),
            virtual_source: None,
            root_path: Some(PathBuf::from("C:/game")),
            relative_path: Some(relative_path),
            priority: SOURCE_PRIORITY_GAME_DATA,
        }
    }

    fn workspace_metadata(path: &str) -> SourceFileMetadata {
        let relative_path = PathBuf::from(path);
        SourceFileMetadata {
            kind: SourceKind::Workspace,
            category: SourceCategory::Workspace,
            absolute_path: Some(PathBuf::from("C:/workspace").join(path)),
            virtual_source: None,
            root_path: Some(PathBuf::from("C:/workspace")),
            relative_path: Some(relative_path),
            priority: SOURCE_PRIORITY_WORKSPACE,
        }
    }

    fn member_names(index: &SymbolIndex, members: &[GlobalSymbolId]) -> Vec<String> {
        members
            .iter()
            .filter_map(|id| index.symbol(*id))
            .filter_map(|symbol| symbol.name.clone())
            .collect()
    }

    fn assert_layered_lookup_maps_match(actual: &SymbolIndex, expected: &SymbolIndex) {
        assert_eq!(actual.by_name, expected.by_name);
        assert_eq!(actual.top_level_by_name, expected.top_level_by_name);
        assert_eq!(
            actual.top_level_by_folded_name,
            expected.top_level_by_folded_name
        );
        assert_eq!(actual.by_kind, expected.by_kind);
        assert_eq!(actual.children, expected.children);
        assert_eq!(actual.classes_by_name, expected.classes_by_name);
        assert_eq!(actual.typedefs_by_name, expected.typedefs_by_name);
        assert_eq!(actual.functions_by_name, expected.functions_by_name);
        assert_eq!(actual.members_by_owner, expected.members_by_owner);
        assert_eq!(actual.methods_by_owner_name, expected.methods_by_owner_name);
        assert_eq!(actual.fields_by_owner_name, expected.fields_by_owner_name);
    }

    fn assert_no_dangling_symbol_references(index: &SymbolIndex) {
        for symbol in index.symbols() {
            assert_eq!(
                index.symbol(symbol.id).map(|found| found.id),
                Some(symbol.id)
            );
            if let Some(parent) = symbol.parent {
                assert!(
                    index.symbol(parent).is_some(),
                    "dangling parent {:?} for {:?}",
                    parent,
                    symbol.id
                );
            }
            for child in index.children(symbol.id) {
                assert!(
                    index.symbol(*child).is_some(),
                    "dangling child {:?} for {:?}",
                    child,
                    symbol.id
                );
                assert_eq!(index.symbol(*child).unwrap().parent, Some(symbol.id));
            }
        }
    }
}
