use reforger_language_server::addon_sources::load_or_build_loaded_addon_indexes;
use reforger_language_server::game_data_catalogue::{
    GameDataCatalogue, GameDataCatalogueConfig, GameDataExternalIndexMode,
};
use reforger_language_server::game_data_inspection::GameDataSourceReadRequest;
use reforger_language_server::game_data_search::{
    search, search_scoped, GameDataAddonIdentity, GameDataAddonMap, GameDataSearchRequest,
    SourceLineStarts,
};
use reforger_language_server::index::SymbolIndex;
use reforger_language_server::index_build::{
    build_index, IndexBuildConfig, IndexBuildControl, IndexSourceRoot,
};
use reforger_language_server::model::{SourceKind, SOURCE_PRIORITY_GAME_DATA};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[test]
fn scoped_search_qualifies_colliding_paths_and_references_by_addon_guid() {
    let fixture = TempFixture::new("scoped-collision");
    let left_root = fixture.path.join("Left");
    let right_root = fixture.path.join("Right");
    for root in [&left_root, &right_root] {
        fs::create_dir_all(root.join("Game")).expect("create scripts");
        fs::write(root.join("Game/Same.c"), "class Shared {}\n").expect("write source");
    }
    let build = |root: &PathBuf| {
        build_index(&IndexBuildConfig {
            roots: vec![IndexSourceRoot::new(
                root,
                SourceKind::GameData,
                SOURCE_PRIORITY_GAME_DATA,
            )],
        })
        .expect("index")
        .index
    };
    let index = SymbolIndex::layered([build(&left_root), build(&right_root)]);
    let lines = line_starts(&index);
    let file_ids = index.files().iter().map(|file| file.id).collect::<Vec<_>>();
    assert_eq!(file_ids.len(), 2);
    let mut addons = GameDataAddonMap::new();
    addons.insert(
        file_ids[0],
        GameDataAddonIdentity {
            guid: "AAAAAAAAAAAAAAAA".to_string(),
            label: "Left".to_string(),
            thumbnail_color: Some("#112233".to_string()),
        },
    );
    addons.insert(
        file_ids[1],
        GameDataAddonIdentity {
            guid: "BBBBBBBBBBBBBBBB".to_string(),
            label: "Right".to_string(),
            thumbnail_color: None,
        },
    );
    let mut left_request = GameDataSearchRequest::new("Shared");
    left_request.addon_guids = Some(vec!["aaaaaaaaaaaaaaaa".to_string()]);
    let left = search_scoped(
        &index,
        &lines,
        &addons,
        &IndexBuildControl::default(),
        "gd2:test",
        left_request,
    )
    .expect("left search");
    let mut right_request = GameDataSearchRequest::new("Shared");
    right_request.addon_guids = Some(vec!["BBBBBBBBBBBBBBBB".to_string()]);
    let right = search_scoped(
        &index,
        &lines,
        &addons,
        &IndexBuildControl::default(),
        "gd2:test",
        right_request,
    )
    .expect("right search");

    assert_eq!(left.total, 1);
    assert_eq!(
        left.results[0].addon_guid.as_deref(),
        Some("AAAAAAAAAAAAAAAA")
    );
    assert_eq!(left.results[0].thumbnail_color.as_deref(), Some("#112233"));
    assert_eq!(
        left.results[0].read_source_input.addon_guid.as_deref(),
        Some("AAAAAAAAAAAAAAAA")
    );
    assert_eq!(
        right.results[0].addon_guid.as_deref(),
        Some("BBBBBBBBBBBBBBBB")
    );
    assert_ne!(left.results[0].symbol_ref, right.results[0].symbol_ref);

    let mut all_request = GameDataSearchRequest::new("Shared");
    all_request.limit = Some(1);
    let all = search_scoped(
        &index,
        &lines,
        &addons,
        &IndexBuildControl::default(),
        "gd2:test",
        all_request,
    )
    .expect("all add-ons search");
    let mut changed_scope = GameDataSearchRequest::new("Shared");
    changed_scope.limit = Some(1);
    changed_scope.addon_guids = Some(vec!["AAAAAAAAAAAAAAAA".to_string()]);
    changed_scope.cursor = all.next_cursor;
    assert!(search_scoped(
        &index,
        &lines,
        &addons,
        &IndexBuildControl::default(),
        "gd2:test",
        changed_scope,
    )
    .is_err());
}

#[test]
fn search_ranks_exact_names_before_other_semantic_matches() {
    let fixture = TempFixture::new("search-ranking");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    fs::write(
        scripts.join("Search.c"),
        "class SearchTarget {}\nclass PrefixSearchTarget {}\nclass Typed { SearchTarget m_Value; }\n",
    )
    .expect("write source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;

    let page = search(
        &index,
        &line_starts(&index),
        &IndexBuildControl::default(),
        "gd1:test",
        GameDataSearchRequest::new("SearchTarget"),
    )
    .expect("search succeeds");

    assert_eq!(page.total, 3);
    assert_eq!(page.results[0].name, "SearchTarget");
    assert_eq!(page.results[0].match_kind, "exactName");
    assert_eq!(page.results[1].name, "PrefixSearchTarget");
    assert_eq!(page.results[1].match_kind, "nameSubstring");
    assert_eq!(page.results[2].name, "m_Value");
    assert_eq!(page.results[2].match_kind, "type");
}

#[test]
fn search_ranks_original_declarations_before_modded_and_override_declarations() {
    let fixture = TempFixture::new("search-origin-ranking");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    fs::write(
        scripts.join("Modded.c"),
        "modded class RankedType { int SharedField; override void SharedMethod(); }\n",
    )
    .expect("write modded source");
    fs::write(
        scripts.join("Original.c"),
        "class RankedType { int SharedField; void SharedMethod(); }\n",
    )
    .expect("write original source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;
    let lines = line_starts(&index);
    let control = IndexBuildControl::default();

    for query in ["RankedType", "SharedField", "SharedMethod"] {
        let page = search(
            &index,
            &lines,
            &control,
            "gd1:origin-ranking",
            GameDataSearchRequest::new(query),
        )
        .expect("search succeeds");
        let exact_paths = page
            .results
            .iter()
            .filter(|result| result.match_kind == "exactName")
            .map(|result| result.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(exact_paths, ["Game/Original.c", "Game/Modded.c"]);
    }
}

#[test]
fn loose_source_search_publishes_the_full_file_editor_identity() {
    let fixture = TempFixture::new("loose-source-uri");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    let source_path = scripts.join("Loose.c");
    fs::write(
        &source_path,
        "class LooseSource { void Test_Callback() {} }\n",
    )
    .expect("write loose source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;

    let page = search(
        &index,
        &line_starts(&index),
        &IndexBuildControl::default(),
        "gd1:test",
        GameDataSearchRequest::new("Test_Callback"),
    )
    .expect("search succeeds");

    assert_eq!(page.total, 1);
    assert_eq!(
        page.results[0].source_uri.as_deref(),
        Some(
            url::Url::from_file_path(&source_path)
                .expect("file URI")
                .as_str()
        )
    );
}

#[test]
fn identifier_prefix_search_does_not_return_hidden_context_matches() {
    let fixture = TempFixture::new("search-identifier-prefix");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    fs::write(
        scripts.join("Search.c"),
        "class SCR_Visible {}\nclass GC_Manager { void OnDamage(SCR_Visible hitZone) {} }\nclass GC_Fields { SCR_Visible m_Value; }\nclass SCR_Container { void Run() {} }\n",
    )
    .expect("write source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;

    let page = search(
        &index,
        &line_starts(&index),
        &IndexBuildControl::default(),
        "gd1:test",
        GameDataSearchRequest::new("SCR_"),
    )
    .expect("search succeeds");

    assert_eq!(page.total, 2);
    assert!(page
        .results
        .iter()
        .all(|result| result.name.starts_with("SCR_")));
    assert!(page
        .results
        .iter()
        .all(|result| result.match_kind == "namePrefix"));
}

#[test]
fn search_pages_a_canonical_filtered_result_set_with_a_bound_cursor() {
    let fixture = TempFixture::new("search-paging");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    fs::write(
        scripts.join("Paging.c"),
        "class Alpha {\n\tvoid Run(int amount) {}\n}\nclass Beta {\n\tvoid Run(string label) {}\n}\n",
    )
    .expect("write source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;
    let mut request = GameDataSearchRequest::new("Run");
    request.kinds = Some(vec!["method".to_string()]);
    request.source_categories = Some(vec!["game".to_string()]);
    request.limit = Some(1);
    let first = search(
        &index,
        &line_starts(&index),
        &IndexBuildControl::default(),
        "gd1:test",
        request.clone(),
    )
    .expect("first page");
    let cursor = first.next_cursor.clone().expect("next cursor");
    request.cursor = Some(cursor);
    let second = search(
        &index,
        &line_starts(&index),
        &IndexBuildControl::default(),
        "gd1:test",
        request,
    )
    .expect("second page");

    assert_eq!(first.total, 2);
    assert_eq!(first.results[0].qualified_name, "Alpha.Run");
    assert_eq!(second.results[0].qualified_name, "Beta.Run");
    assert_eq!(first.results[0].declaration_range.start_line, 2);
    assert_eq!(first.results[0].read_source_input.start_line, 2);
    assert_eq!(first.results[0].match_kind, "exactName");

    let mut random_access = GameDataSearchRequest::new("Run");
    random_access.kinds = Some(vec!["method".to_string()]);
    random_access.source_categories = Some(vec!["game".to_string()]);
    random_access.limit = Some(1);
    random_access.offset = Some(1);
    let direct = search(
        &index,
        &line_starts(&index),
        &IndexBuildControl::default(),
        "gd1:test",
        random_access,
    )
    .expect("direct page");
    assert_eq!(direct.results[0].qualified_name, "Beta.Run");

    let mut conflicting = GameDataSearchRequest::new("Run");
    conflicting.kinds = Some(vec!["method".to_string()]);
    conflicting.source_categories = Some(vec!["game".to_string()]);
    conflicting.limit = Some(1);
    conflicting.cursor = first.next_cursor;
    conflicting.offset = Some(1);
    assert!(search(
        &index,
        &line_starts(&index),
        &IndexBuildControl::default(),
        "gd1:test",
        conflicting,
    )
    .is_err());
}

#[test]
fn catalogue_search_keeps_source_lines_from_its_initialized_snapshot() {
    let fixture = TempFixture::new("search-snapshot");
    let scripts = fixture.path.join("scripts/Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    let source = scripts.join("Snapshot.c");
    fs::write(&source, "\nclass SnapshotTarget {}\n").expect("write source");
    let catalogue = layered_catalogue(&fixture.path);
    catalogue
        .status(&IndexBuildControl::default())
        .expect("initialize catalogue");
    fs::write(&source, "class SnapshotTarget {}\n").expect("change source after initialization");

    let page = catalogue
        .search(
            &IndexBuildControl::default(),
            GameDataSearchRequest::new("SnapshotTarget"),
        )
        .expect("search");

    assert_eq!(page.results[0].declaration_range.start_line, 2);
}

#[test]
fn catalogue_source_read_returns_the_authoritative_source_line() {
    let fixture = TempFixture::new("source-read");
    let scripts = fixture.path.join("scripts/Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    fs::write(scripts.join("Source.c"), "// docs\nclass SourceTarget {}\n").expect("write source");
    let catalogue = layered_catalogue(&fixture.path);
    let revision = catalogue
        .status(&IndexBuildControl::default())
        .expect("initialize catalogue")
        .catalogue_revision
        .expect("catalogue revision");
    let page = catalogue
        .search(
            &IndexBuildControl::default(),
            GameDataSearchRequest::new("SourceTarget"),
        )
        .expect("search");
    let input = &page.results[0].read_source_input;
    let source = catalogue
        .read_source(
            &IndexBuildControl::default(),
            GameDataSourceReadRequest {
                catalogue_revision: revision,
                addon_guid: input.addon_guid.clone(),
                relative_path: input.relative_path.clone(),
                start_line: Some(input.start_line),
                line_count: Some(1),
                include_preview: false,
            },
        )
        .expect("read source");

    assert_eq!(source["content"], "class SourceTarget {}\n");
    assert_eq!(source["startLine"], 2);
}

#[test]
fn search_applies_default_api_kinds_canonical_filters_and_cursor_revision_binding() {
    let fixture = TempFixture::new("search-filters");
    let game = fixture.path.join("Game");
    let core = fixture.path.join("Core");
    fs::create_dir_all(&game).expect("create game scripts");
    fs::create_dir_all(&core).expect("create core scripts");
    fs::write(
        game.join("GameSymbol.c"),
        "class SearchApi { void Match(int Match) {} }",
    )
    .expect("write game source");
    fs::write(core.join("CoreSymbol.c"), "class SearchCore {}").expect("write core source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;
    let lines = line_starts(&index);
    let control = IndexBuildControl::default();

    let default_page = search(
        &index,
        &lines,
        &control,
        "gd1:one",
        GameDataSearchRequest::new("Match"),
    )
    .expect("default search");
    let mut filtered = GameDataSearchRequest::new("Search");
    filtered.kinds = Some(vec!["class".to_string()]);
    filtered.source_categories = Some(vec!["core".to_string()]);
    filtered.limit = Some(999);
    let filtered_page =
        search(&index, &lines, &control, "gd1:one", filtered).expect("filtered search");
    let mut paged = GameDataSearchRequest::new("Search");
    paged.limit = Some(1);
    let first_page = search(&index, &lines, &control, "gd1:one", paged).expect("first page");
    let mut stale = GameDataSearchRequest::new("Search");
    stale.limit = Some(1);
    stale.cursor = first_page.next_cursor.clone();

    assert!(default_page
        .results
        .iter()
        .all(|result| result.kind != "parameter"));
    assert_eq!(filtered_page.applied_filters.limit, 100);
    assert_eq!(filtered_page.results.len(), 1);
    assert_eq!(filtered_page.results[0].source_category, "core");
    assert!(stale.cursor.is_some());
    assert!(search(&index, &lines, &control, "gd1:two", stale).is_err());
}

#[test]
fn empty_query_returns_all_symbols_for_the_selected_kind() {
    let fixture = TempFixture::new("search-empty-kind");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create game scripts");
    fs::write(
        scripts.join("Kinds.c"),
        "class EmptyQueryClass { void EmptyQueryMethod() {} }",
    )
    .expect("write source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;
    let lines = line_starts(&index);
    let mut request = GameDataSearchRequest::new("");
    request.kinds = Some(vec!["method".to_string()]);

    let page = search(
        &index,
        &lines,
        &IndexBuildControl::default(),
        "gd1:empty-kind",
        request,
    )
    .expect("empty query kind search");

    assert_eq!(page.query, "");
    assert!(!page.results.is_empty());
    assert!(page.results.iter().all(|result| result.kind == "method"));
    assert!(page
        .results
        .iter()
        .any(|result| result.name == "EmptyQueryMethod"));
    assert!(page
        .results
        .iter()
        .all(|result| result.match_kind == "kind"));
}

#[test]
fn search_documentation_summaries_use_shared_comment_rendering() {
    let fixture = TempFixture::new("search-documentation");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    fs::write(
        scripts.join("Documentation.c"),
        "//! \\brief Line summary.\nclass LineDocumented {}\n/*! Block summary. */\nclass BlockDocumented {}\n/*! \\brief Doxygen summary.\n * \\param value ignored in the compact summary.\n */\nclass DoxygenDocumented {}\n//! \nclass EmptyDocumented {}\n",
    )
    .expect("write source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;
    let lines = line_starts(&index);
    let control = IndexBuildControl::default();

    let summary = |query: &str| {
        search(
            &index,
            &lines,
            &control,
            "gd1:docs",
            GameDataSearchRequest::new(query),
        )
        .expect("search")
        .results[0]
            .documentation_summary
            .clone()
    };

    assert_eq!(summary("LineDocumented").as_deref(), Some("Line summary."));
    assert_eq!(
        summary("BlockDocumented").as_deref(),
        Some("Block summary.")
    );
    assert_eq!(
        summary("DoxygenDocumented").as_deref(),
        Some("Doxygen summary.")
    );
    assert_eq!(summary("EmptyDocumented"), None);
}

#[test]
fn broad_semantic_search_retains_only_the_best_reachable_results() {
    let fixture = TempFixture::new("bounded-semantic-search");
    let scripts = fixture.path.join("Game");
    fs::create_dir_all(&scripts).expect("create scripts");
    let source = (0..10_001)
        .map(|index| format!("class Common{index:05} {{}}\n"))
        .collect::<String>();
    fs::write(scripts.join("Broad.c"), source).expect("write source");
    let index = build_index(&IndexBuildConfig {
        roots: vec![IndexSourceRoot::new(
            &fixture.path,
            SourceKind::GameData,
            SOURCE_PRIORITY_GAME_DATA,
        )],
    })
    .expect("index")
    .index;
    let lines = line_starts(&index);
    let mut request = GameDataSearchRequest::new("Common");
    request.limit = Some(100);
    request.offset = Some(9_900);

    let page = search(
        &index,
        &lines,
        &IndexBuildControl::default(),
        "gd1:broad",
        request,
    )
    .expect("bounded broad semantic search");

    assert_eq!(page.total, 10_000);
    assert!(page.truncated);
    assert_eq!(page.returned, 100);
    assert_eq!(page.results[0].name, "Common09900");
    assert_eq!(page.results[99].name, "Common09999");
    assert!(page.next_cursor.is_none());
}

fn layered_catalogue(source_root: &PathBuf) -> GameDataCatalogue {
    let cache_root = source_root.join("cache");
    let inventory = cache_root.join("loaded-addons.json");
    let storage = cache_root.join("addon-indexes");
    fs::create_dir_all(&cache_root).expect("create layered cache root");
    fs::write(
        &inventory,
        format!(
            r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"58D0FB3206B6F859","id":"ArmaReforger","title":"Arma Reforger","sourceRoot":{}}}]}}"#,
            serde_json::to_string(source_root).expect("serialize fixture root"),
        ),
    )
    .expect("write loaded add-on inventory");
    load_or_build_loaded_addon_indexes(&inventory, &storage, &[], &IndexBuildControl::default())
        .expect("build layered fixture index");
    GameDataCatalogue::new(GameDataCatalogueConfig {
        addon_source_inventory: Some(inventory),
        addon_index_storage: Some(storage),
        external_index_mode: GameDataExternalIndexMode::Loaded,
        ..GameDataCatalogueConfig::default()
    })
}

fn line_starts(
    index: &reforger_language_server::index::SymbolIndex,
) -> BTreeMap<reforger_language_server::index::SourceFileId, SourceLineStarts> {
    index
        .files()
        .iter()
        .filter_map(|file| {
            let source = fs::read_to_string(file.metadata.absolute_path.as_ref()?).ok()?;
            Some((file.id, SourceLineStarts::from_source(&source)))
        })
        .collect()
}

struct TempFixture {
    path: PathBuf,
}

impl TempFixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "reforger-script-tools-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create fixture");
        Self { path }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
