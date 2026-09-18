use reforger_language_server::addon_sources::load_or_build_loaded_addon_indexes;
use reforger_language_server::index_build::IndexBuildControl;
use reforger_language_server::workbench::WORKBENCH_BRIDGE_VERSION;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const BASE_GAME_ADDON_GUID: &str = "58D0FB3206B6F859";

struct LayeredGameDataLaunch {
    arguments: Vec<String>,
    storage_root: PathBuf,
}

fn build_game_data_cache(scripts_root: &Path, cache_path: &Path) -> LayeredGameDataLaunch {
    let source_root = scripts_root
        .parent()
        .expect("fixture scripts root has an add-on root");
    let cache_root = cache_path
        .parent()
        .expect("fixture cache path has a parent");
    let inventory_path = cache_root.join("loaded-addons.json");
    let storage_root = cache_root.join("addon-indexes");
    fs::create_dir_all(cache_root).expect("create layered cache root");
    fs::write(
        &inventory_path,
        format!(
            r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"{BASE_GAME_ADDON_GUID}","id":"ArmaReforger","title":"Arma Reforger","sourceRoot":{}}}]}}"#,
            serde_json::to_string(source_root).expect("serialize fixture source root"),
        ),
    )
    .expect("write layered game-data inventory");
    load_or_build_loaded_addon_indexes(
        &inventory_path,
        &storage_root,
        &[],
        &IndexBuildControl::default(),
    )
    .expect("build layered parser-owned game-data cache");
    LayeredGameDataLaunch {
        arguments: vec![
            "mcp".to_string(),
            "--addon-source-inventory".to_string(),
            inventory_path.to_string_lossy().into_owned(),
            "--addon-index-storage".to_string(),
            storage_root.to_string_lossy().into_owned(),
            "--external-index-mode".to_string(),
            "loaded".to_string(),
        ],
        storage_root,
    }
}

fn assert_tool_error_code(response: &Value, code: &str) {
    assert_eq!(
        response.pointer("/result/structuredContent/code"),
        Some(&json!(code)),
        "expected structured tool error {code}: {response}"
    );
}

fn with_stale_catalogue_revision(symbol_ref: &Value) -> Value {
    let symbol_ref = symbol_ref.as_str().expect("symbol reference string");
    let encoded = symbol_ref
        .strip_prefix("sr2:")
        .expect("sr2 symbol reference");
    let bytes = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex pair"), 16).expect("hex byte")
        })
        .collect::<Vec<_>>();
    let mut decoded = serde_json::from_slice::<Value>(&bytes).expect("symbol reference JSON");
    decoded["catalogueRevision"] = json!("stale-catalogue-revision");
    let encoded = serde_json::to_vec(&decoded)
        .expect("serialize stale symbol reference")
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    json!(format!("sr2:{encoded}"))
}

#[test]
fn executable_rejects_unknown_modes_without_writing_protocol_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_reforger_language_server"))
        .arg("unknown-mode")
        .stdin(Stdio::null())
        .output()
        .expect("run language server");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown mode"),
        "stderr should explain the rejected mode: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mcp_stdio_initializes_lists_and_reports_game_data_status() {
    let fixture = TempFixture::new("mcp_status");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(scripts_root.join("Game")).expect("create scripts fixture");
    fs::write(
        scripts_root.join("Game").join("McpFixture.c"),
        "class McpFixture\n{\n\tvoid Run()\n\t{\n\t}\n}\n",
    )
    .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned(&game_data.arguments);

    let initialize = client.initialize(1);
    assert_eq!(
        initialize.pointer("/result/protocolVersion"),
        Some(&json!("2025-11-25"))
    );
    assert_eq!(
        initialize.pointer("/result/capabilities/tools/listChanged"),
        Some(&json!(false))
    );
    assert_eq!(
        initialize.pointer("/result/serverInfo/name"),
        Some(&json!("reforger-script-tools"))
    );
    let instructions = initialize
        .pointer("/result/instructions")
        .and_then(Value::as_str)
        .expect("server instructions");
    assert_eq!(
        instructions,
        "Search indexed Reforger knowledge and inspect or operate the configured live Workbench."
    );

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let tools = client.response(2);
    let listed = tools
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tools/list result");
    let tool = |name: &str| {
        listed
            .iter()
            .find(|tool| tool.get("name") == Some(&json!(name)))
            .unwrap_or_else(|| panic!("missing tool {name}"))
    };
    assert_eq!(listed.len(), 91);
    assert!(listed
        .iter()
        .all(|tool| tool.get("name") != Some(&json!("workbench_list_resources"))));
    let game_data_status = tool("game_data_status");
    assert!(listed
        .iter()
        .any(|tool| tool.get("name") == Some(&json!("workbench_convert_shape_points"))));
    assert!(listed
        .iter()
        .any(|tool| tool.get("name") == Some(&json!("workbench_transform_shape_points"))));
    assert!(listed
        .iter()
        .any(|tool| tool.get("name") == Some(&json!("workbench_resample_polyline"))));
    assert_eq!(
        game_data_status.pointer("/annotations/readOnlyHint"),
        Some(&json!(true))
    );
    assert_eq!(
        game_data_status.pointer("/annotations/openWorldHint"),
        Some(&json!(false))
    );
    assert_eq!(
        game_data_status.pointer("/inputSchema/additionalProperties"),
        Some(&json!(false))
    );
    assert!(game_data_status.get("outputSchema").is_some());
    let game_data_symbols = tool("search_game_data_symbols");
    let game_data_research = tool("research_game_data");
    assert_eq!(
        game_data_research.pointer("/inputSchema/required/0"),
        Some(&json!("query"))
    );
    assert!(game_data_research.get("outputSchema").is_some());
    tool("search_game_data_text");
    tool("search_workspace_text");
    assert_eq!(
        game_data_symbols.get("name"),
        Some(&json!("search_game_data_symbols"))
    );
    assert_eq!(
        game_data_symbols.pointer("/annotations/readOnlyHint"),
        Some(&json!(true))
    );
    assert_eq!(
        game_data_symbols.pointer("/annotations/openWorldHint"),
        Some(&json!(false))
    );
    assert_eq!(
        game_data_symbols.pointer("/inputSchema/additionalProperties"),
        Some(&json!(false))
    );
    assert_eq!(
        game_data_symbols.pointer("/inputSchema/required/0"),
        Some(&json!("query"))
    );
    for name in ["search_game_data_text", "search_workspace_text"] {
        let text_tool = tool(name);
        assert_eq!(
            text_tool.pointer("/inputSchema/required/0"),
            Some(&json!("query"))
        );
        for option in ["matchCase", "matchWholeWord", "useRegex"] {
            assert_eq!(
                text_tool.pointer(&format!("/inputSchema/properties/{option}/type")),
                Some(&json!("boolean"))
            );
        }
        assert!(text_tool.get("outputSchema").is_some());
    }
    for name in [
        "inspect_game_data_symbol",
        "list_game_data_symbol_members",
        "query_game_data_symbol_relationships",
        "query_source_symbol_relationships",
        "read_game_data_source",
    ] {
        tool(name);
    }
    assert!(listed
        .iter()
        .all(|tool| tool.get("name") != Some(&json!("search_game_data_examples"))));
    tool("read_workspace_source");
    let official_wiki_status = tool("official_wiki_status");
    assert_eq!(
        official_wiki_status.pointer("/annotations/readOnlyHint"),
        Some(&json!(true))
    );
    assert_eq!(
        official_wiki_status.pointer("/inputSchema/additionalProperties"),
        Some(&json!(false))
    );
    tool("search_official_wiki");
    tool("read_official_wiki");
    for name in [
        "workbench_status",
        "workbench_validate_scripts",
        "workbench_install_bridge",
        "workbench_state",
        "workbench_project_context",
        "workbench_inspect_resource",
        "workbench_search_resources",
        "workbench_world_selection_summary",
        "workbench_selected_entity_hierarchy",
        "workbench_list_entities",
        "workbench_search_world_entities",
        "workbench_layer_state",
        "workbench_find_entities_by_radius",
        "workbench_inspect_entity",
        "workbench_set_selection",
        "workbench_clear_selection",
        "workbench_create_entity",
        "workbench_rename_entity",
        "workbench_delete_entity",
        "workbench_list_editors",
        "workbench_open_editor",
        "workbench_open_resource",
        "workbench_start_play_session",
        "workbench_stop_play_session",
        "workbench_reload",
        "workbench_save",
        "workbench_read_logs",
        "workbench_list_windows",
        "workbench_capture_window",
        "workbench_launch",
        "workbench_stop",
        "workbench_restart",
    ]
    .into_iter()
    {
        assert!(listed
            .iter()
            .any(|tool| tool.get("name") == Some(&json!(name))));
    }
    assert!(!listed
        .iter()
        .any(|tool| tool.get("name") == Some(&json!("workbench_save_all"))));
    assert!(!listed
        .iter()
        .any(|tool| tool.get("name") == Some(&json!("workbench_save_world"))));
    assert_eq!(
        tool("workbench_delete_entity").pointer("/annotations/destructiveHint"),
        Some(&json!(true))
    );

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "game_data_status",
            "arguments": {}
        }
    }));
    let call = client.response(3);
    assert_eq!(call.pointer("/result/isError"), Some(&json!(false)));
    let structured = call
        .pointer("/result/structuredContent")
        .cloned()
        .expect("structured status result");
    assert_eq!(structured.get("available"), Some(&json!(true)));
    assert_eq!(
        structured.pointer("/source/acquisition"),
        Some(&json!("localpack"))
    );
    assert_eq!(
        structured.pointer("/authorities/sourceEvidence"),
        Some(&json!("evidence-catalogue"))
    );
    assert_eq!(
        structured.pointer("/authorities/sourceMetadata"),
        Some(&json!("filesystem"))
    );
    assert_eq!(
        structured.pointer("/authorities/semanticCatalogue"),
        Some(&json!("language-engine"))
    );
    assert_eq!(structured.get("cache"), None);
    assert_eq!(
        structured.get("scopeAuthority"),
        Some(&json!("workbench-loaded"))
    );
    assert_eq!(structured.pointer("/coverage/files"), Some(&json!(1)));
    assert!(structured
        .get("catalogueRevision")
        .and_then(Value::as_str)
        .is_some_and(|revision| revision.starts_with("gd2:")));
    assert!(structured.get("limits").is_some());
    assert!(structured.get("warnings").is_some());
    assert!(structured.get("recovery").is_some());
    assert!(
        !structured
            .to_string()
            .contains(fixture.path().to_str().expect("utf-8 fixture path")),
        "status must not disclose physical paths"
    );

    let compatibility_text = call
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .expect("compatibility JSON text");
    assert_eq!(
        serde_json::from_str::<Value>(compatibility_text).expect("valid compatibility JSON"),
        structured
    );

    let search_started = std::time::Instant::now();
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": { "name": "search_game_data_symbols", "arguments": { "query": "McpFixture" } }
    }));
    let search = client.response(4);
    assert!(
        search_started.elapsed() < Duration::from_secs(5),
        "ready-catalogue search exceeded the five-second ceiling"
    );
    assert_eq!(search.pointer("/result/isError"), Some(&json!(false)));
    let results = search
        .pointer("/result/structuredContent/results")
        .and_then(Value::as_array)
        .expect("search results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("name"), Some(&json!("McpFixture")));
    assert!(results[0].get("symbolRef").is_some());
    assert_eq!(
        results[0].pointer("/inspectInput/symbolRef"),
        results[0].get("symbolRef")
    );
    assert_eq!(
        results[0].pointer("/readSourceInput/relativePath"),
        Some(&json!("scripts/Game/McpFixture.c"))
    );

    client.send(json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "search_game_data_text", "arguments": { "query": "mcpfixture" } }
    }));
    let text_search = client.response(7);
    assert_eq!(text_search.pointer("/result/isError"), Some(&json!(false)));
    assert_eq!(
        text_search.pointer("/result/structuredContent/results/0/matchText"),
        Some(&json!("McpFixture"))
    );
    assert_eq!(
        text_search.pointer("/result/structuredContent/results/0/matchRange/startLine"),
        Some(&json!(1))
    );
    assert!(text_search
        .pointer("/result/structuredContent/stats/filesRead")
        .and_then(Value::as_u64)
        .is_some_and(|files| files >= 1));

    client.send(json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "inspect_game_data_symbol", "arguments": results[0]["inspectInput"] }
    }));
    let inspection = client.response(5);
    assert_eq!(inspection.pointer("/result/isError"), Some(&json!(false)));
    let inspected = inspection
        .pointer("/result/structuredContent")
        .expect("inspection result");
    assert_eq!(
        inspected.pointer("/qualifiedName"),
        Some(&json!("McpFixture"))
    );
    assert_eq!(
        inspected.pointer("/relativePath"),
        Some(&json!("scripts/Game/McpFixture.c"))
    );

    client.send(json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": { "name": "read_game_data_source", "arguments": results[0]["readSourceInput"] }
    }));
    let source = client.response(6);
    assert_eq!(source.pointer("/result/isError"), Some(&json!(false)));
    assert!(source
        .pointer("/result/structuredContent/content")
        .and_then(Value::as_str)
        .is_some_and(|content| content.contains("class McpFixture")));

    client.send(json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "search_game_data_text", "arguments": { "query": "SCR_" } }
    }));
    let text_search = client.response(7);
    assert_eq!(text_search.pointer("/result/isError"), Some(&json!(false)));
    let text_results = text_search
        .pointer("/result/structuredContent/results")
        .and_then(Value::as_array)
        .expect("text search results");
    assert!(text_results.is_empty());
    assert_eq!(
        text_search.pointer("/result/structuredContent/stats/filesConsidered"),
        Some(&json!(1))
    );

    client.send(json!({
        "jsonrpc": "2.0", "id": 8, "method": "tools/call",
        "params": { "name": "search_game_data_text", "arguments": { "query": "void" } }
    }));
    let text_hit_search = client.response(8);
    assert_eq!(
        text_hit_search.pointer("/result/isError"),
        Some(&json!(false))
    );
    assert!(text_hit_search
        .pointer("/result/structuredContent/results/0/matchRange/startLine")
        .and_then(Value::as_u64)
        .is_some_and(|line| line > 0));
    assert_eq!(
        text_hit_search.pointer("/result/structuredContent/results/0/readSourceInput/relativePath"),
        Some(&json!("scripts/Game/McpFixture.c"))
    );

    client.send(json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "search_game_data_text", "arguments": { "query": "mcpfixture", "matchCase": true } }
    }));
    let case_sensitive_search = client.response(9);
    assert_eq!(
        case_sensitive_search.pointer("/result/structuredContent/total"),
        Some(&json!(0))
    );

    client.send(json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": { "name": "search_game_data_text", "arguments": { "query": "(", "useRegex": true } }
    }));
    assert_tool_error_code(&client.response(10), "invalid_arguments");

    for (offset, tool) in listed.iter().enumerate() {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("listed tool name");
        if name == "game_data_status" {
            continue;
        }
        let id = 10_000_u64 + offset as u64;
        client.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":{"__routeProbe":true}}}));
        let serialized = client.response(id).to_string();
        assert!(
            !serialized.contains("Unknown tool")
                && !serialized.contains("without a typed call route"),
            "listed tool {name} has no typed call route: {serialized}"
        );
    }

    client.close_stdin();
    assert!(
        client.wait_for_exit(Duration::from_secs(3)),
        "MCP process should exit promptly after stdin EOF"
    );
}

#[test]
fn exact_symbol_aliases_preserve_generic_results_errors_and_cursors() {
    fn call(client: &mut McpClient, id: &mut u64, name: &str, arguments: Value) -> Value {
        *id += 1;
        client.send(json!({"jsonrpc":"2.0", "id":*id, "method":"tools/call",
            "params":{"name":name, "arguments":arguments}}));
        let response = client.response(*id);
        response.get("result").or_else(|| response.get("error")).unwrap().clone()
    }

    let fixture = TempFixture::new("exact_symbol_aliases");
    let scripts_root = fixture.path().join("game-data").join("Scripts");
    let workspace_root = fixture.path().join("workspace").join("Scripts");
    for root in [&scripts_root, &workspace_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Fixture.c"),
            "class AliasFixture { int First; int Second; int Third; }\nclass DerivedOne : AliasFixture {}\nclass DerivedTwo : AliasFixture {}\n").unwrap();
    }
    let game_data = build_game_data_cache(&scripts_root, &fixture.path().join("cache/index.bin"));
    let mut arguments = game_data.arguments;
    arguments.extend(["--workspace-scripts".to_string(), workspace_root.to_string_lossy().into_owned()]);
    let mut client = McpClient::spawn_owned(&arguments);
    client.initialize(1);
    let mut id = 1;
    for (source, prefix) in [("gameData", "game_data"), ("workspace", "workspace")] {
        let search = call(&mut client, &mut id, &format!("search_{prefix}_symbols"), json!({"query":"AliasFixture", "kinds":["class"]}));
        let symbol_ref = search.pointer("/structuredContent/results/0/symbolRef").unwrap();
        for (generic, alias, extra) in [
            ("inspect_symbol", format!("inspect_{prefix}_symbol"), json!({})),
            ("list_symbol_members", format!("list_{prefix}_symbol_members"), json!({"kinds":["field"], "limit":1})),
            ("query_symbol_relationships", format!("query_{prefix}_symbol_relationships"), json!({"relationshipKinds":["derivedType"], "limit":1})),
        ] {
            let mut legacy_args = extra.clone();
            legacy_args["symbolRef"] = symbol_ref.clone();
            let mut generic_args = legacy_args.clone();
            generic_args["source"] = json!(source);
            let legacy = call(&mut client, &mut id, &alias, legacy_args.clone());
            let current = call(&mut client, &mut id, generic, generic_args.clone());
            assert_eq!(legacy["isError"], false, "{alias}: {legacy}");
            assert_eq!(legacy, current, "{alias} must preserve the generic result");
            if generic != "inspect_symbol" {
                let cursor = legacy.pointer("/structuredContent/nextCursor").expect("fixture has a second page");
                legacy_args["cursor"] = cursor.clone();
                generic_args["cursor"] = cursor.clone();
                let second = call(&mut client, &mut id, generic, generic_args.clone());
                assert_eq!(second["isError"], false);
                assert_eq!(second, call(&mut client, &mut id, &alias, legacy_args.clone()));
                // Cursors remain bound to their original filter, across either name.
                if generic == "list_symbol_members" {
                    legacy_args["kinds"] = json!(["method"]);
                    generic_args["kinds"] = json!(["method"]);
                } else {
                    legacy_args["relationshipKinds"] = json!(["directBase"]);
                    generic_args["relationshipKinds"] = json!(["directBase"]);
                }
                let invalid = call(&mut client, &mut id, &alias, legacy_args.clone());
                assert_eq!(invalid["isError"], true);
                assert_eq!(invalid, call(&mut client, &mut id, generic, generic_args));
            }
            let mut stale = extra.clone();
            stale["symbolRef"] = with_stale_catalogue_revision(symbol_ref);
            let old_error = call(&mut client, &mut id, &alias, stale.clone());
            stale["source"] = json!(source);
            assert_eq!(old_error["isError"], true);
            assert_eq!(old_error, call(&mut client, &mut id, generic, stale));
            let mut unknown = extra;
            unknown["symbolRef"] = symbol_ref.clone();
            unknown["unexpected"] = json!(true);
            let error = call(&mut client, &mut id, &alias, unknown);
            assert_eq!(error["code"], -32602);
            assert!(error["message"].as_str().unwrap().starts_with(&format!("Invalid {alias} arguments:")));
        }
    }
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn authoring_profile_exposes_one_concise_search_surface() {
    let mut client = McpClient::spawn(&["mcp", "--tool-profile", "authoring"]);
    let initialize = client.initialize(1);
    assert_eq!(
        initialize.pointer("/result/instructions"),
        Some(&json!(
            "Search indexed Reforger knowledge and inspect or operate the configured live Workbench."
        ))
    );

    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let response = client.response(2);
    let tools = response
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tool catalogue");
    let names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let searches = names
        .iter()
        .copied()
        .filter(|name| name.contains("search") || name.starts_with("research_"))
        .collect::<Vec<_>>();

    assert_eq!(tools.len(), 20);
    assert_eq!(searches, vec!["search_reforger"]);
    assert!(names.contains(&"workbench_validate_scripts"));
    assert!(names.contains(&"workbench_reload"));
    assert!(!names.contains(&"workbench_create_entity"));
    assert!(tools.iter().all(|tool| {
        tool.get("description")
            .and_then(Value::as_str)
            .is_some_and(|description| description.chars().count() <= 240)
    }));

    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn unified_search_returns_one_labeled_result_per_selected_authority() {
    let fixture = TempFixture::new("unified_reforger_search");
    let scripts_root = fixture.path().join("game-data").join("scripts");
    fs::create_dir_all(scripts_root.join("Game")).expect("create Game Data fixture");
    fs::write(
        scripts_root.join("Game").join("Lifecycle.c"),
        "class LifecycleService\n{\n\tvoid RegisterLifecycleGroup(int groupId) {}\n}\nclass SCR_FactionGroupManager {}\nclass SCR_AIGroupManager {}\nclass SCR_PlayerGroupManager {}\nclass SCR_GroupManagerBase {}\n",
    )
    .expect("write Game Data source");
    let game_data = build_game_data_cache(
        &scripts_root,
        &fixture.path().join("cache").join("game-data-index.bin"),
    );

    let workspace_root = fixture.path().join("workspace").join("Scripts");
    fs::create_dir_all(workspace_root.join("Game")).expect("create workspace fixture");
    fs::write(
        workspace_root.join("Game").join("Lifecycle.c"),
        "class LifecycleServiceCoordinator {}\n",
    )
    .expect("write workspace source");

    let wiki_root = fixture.path().join("official-wiki");
    fs::create_dir_all(wiki_root.join("Guides")).expect("create Wiki fixture");
    fs::write(wiki_root.join("wiki-index.md"), "# Wiki Markdown Index\n")
        .expect("write Wiki index");
    fs::write(
        wiki_root.join("Guides").join("Lifecycle.md"),
        "# [Lifecycle](https://community.bistudio.com/wiki/Arma_Reforger:Lifecycle)\n\n## Lifecycle callbacks\nLifecycleService callback guidance.\n",
    )
    .expect("write Wiki page");

    let mut arguments = game_data.arguments;
    arguments.extend([
        "--tool-profile".to_string(),
        "authoring".to_string(),
        "--workspace-scripts".to_string(),
        workspace_root.to_string_lossy().into_owned(),
        "--official-wiki-root".to_string(),
        wiki_root.to_string_lossy().into_owned(),
    ]);
    let mut client = McpClient::spawn_owned(&arguments);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"search_reforger","arguments":{"query":"LifecycleService"}}
    }));
    let response = client.response(2);
    assert_eq!(response.pointer("/result/isError"), Some(&json!(false)));
    let result = response
        .pointer("/result/structuredContent")
        .expect("unified search result");
    assert_eq!(result.get("query"), Some(&json!("LifecycleService")));
    let hits = result
        .get("results")
        .and_then(Value::as_array)
        .expect("search hits");
    assert_eq!(hits.len(), 3);
    assert_eq!(
        hits.iter()
            .filter_map(|hit| hit.get("source").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        vec!["gameData", "workspace", "officialWiki"]
    );
    assert!(hits.iter().all(|hit| {
        hit.get("title")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
            && hit
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
            && (hit.get("inspect").is_some() || hit.get("read").is_some())
    }));
    assert_eq!(
        result
            .get("sources")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(3)
    );

    let inspect = hits[0].get("inspect").expect("Game Data inspect handoff");
    assert_eq!(inspect.get("tool"), Some(&json!("inspect_symbol")));
    client.send(json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{
            "name":inspect.get("tool").expect("inspect tool"),
            "arguments":inspect.get("arguments").expect("inspect arguments")
        }
    }));
    assert_eq!(
        client
            .response(3)
            .pointer("/result/structuredContent/qualifiedName"),
        Some(&json!("LifecycleService"))
    );

    client.send(json!({
        "jsonrpc":"2.0","id":4,"method":"tools/call",
        "params":{
            "name":"search_reforger",
            "arguments":{"query":"LifecycleService","sources":["officialWiki"]}
        }
    }));
    let wiki_only = client.response(4);
    assert_eq!(
        wiki_only
            .pointer("/result/structuredContent/results")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        wiki_only.pointer("/result/structuredContent/results/0/source"),
        Some(&json!("officialWiki"))
    );

    client.send(json!({
        "jsonrpc":"2.0","id":5,"method":"tools/call",
        "params":{
            "name":"search_reforger",
            "arguments":{
                "query":"LifecycleService register lifecycle group",
                "sources":["gameData"]
            }
        }
    }));
    let member_search = client.response(5);
    assert_eq!(
        member_search.pointer("/result/structuredContent/results/0/status"),
        Some(&json!("resolved"))
    );
    assert_eq!(
        member_search.pointer("/result/structuredContent/results/0/relevantMember/title"),
        Some(&json!("RegisterLifecycleGroup"))
    );
    assert_eq!(
        member_search.pointer("/result/structuredContent/results/0/relevantMember/inspect/tool"),
        Some(&json!("inspect_symbol"))
    );

    client.send(json!({
        "jsonrpc":"2.0","id":6,"method":"tools/call",
        "params":{
            "name":"search_reforger",
            "arguments":{"query":"group manager","sources":["gameData"]}
        }
    }));
    let ambiguous = client.response(6);
    assert_eq!(
        ambiguous.pointer("/result/structuredContent/results/0/status"),
        Some(&json!("ambiguous"))
    );
    assert!(ambiguous
        .pointer("/result/structuredContent/results/0/alternatives")
        .and_then(Value::as_array)
        .is_some_and(|alternatives| !alternatives.is_empty()));
    for field in ["inspect", "read", "symbolRef", "relevantMember"] {
        assert!(
            ambiguous
                .pointer(&format!("/result/structuredContent/results/0/{field}"))
                .is_none(),
            "ambiguous hits must not expose {field}"
        );
    }

    client.send(json!({
        "jsonrpc":"2.0","id":7,"method":"tools/call",
        "params":{
            "name":"search_reforger",
            "arguments":{"query":"Lifecycle","sources":[]}
        }
    }));
    assert_eq!(
        client.response(7).pointer("/result/structuredContent/code"),
        Some(&json!("invalid_arguments"))
    );

    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn mcp_research_game_data_resolves_natural_language_without_dumping_source() {
    let fixture = TempFixture::new("mcp_intent_research");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(scripts_root.join("Game")).expect("create scripts fixture");
    fs::write(
        scripts_root.join("Game").join("Lifecycle.c"),
        "class SCR_BaseGameMode\n{\n\tvoid OnGameEnd() {}\n\tvoid RestartRound() {}\n}\n",
    )
    .expect("write intent fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned(&game_data.arguments);
    client.initialize(1);

    client.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {
            "name": "research_game_data",
            "arguments": { "query": "base game mode lifecycle callback when the game ends" }
        }
    }));
    let response = client.response(2);
    let result = response
        .pointer("/result/structuredContent")
        .expect("structured intent result");

    assert_eq!(result.get("status"), Some(&json!("resolved")));
    assert_eq!(
        result.pointer("/primary/qualifiedName"),
        Some(&json!("SCR_BaseGameMode.OnGameEnd"))
    );
    assert_eq!(result.get("followUp"), Some(&json!("none")));
    assert!(result
        .get("alternatives")
        .and_then(Value::as_array)
        .is_some_and(|alternatives| alternatives.len() <= 2));
    let serialized = result.to_string();
    assert!(!serialized.contains("rawDocumentation"));
    assert!(!serialized.contains("sourceExcerpt"));
    assert!(!serialized.contains("content"));
}

#[test]
fn mcp_inspection_and_source_read_reject_stale_and_changed_handoffs() {
    let fixture = TempFixture::new("mcp_inspection_contract");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(scripts_root.join("Game")).expect("create scripts fixture");
    let source_path = scripts_root.join("Game").join("Inspectable.c");
    fs::write(
        &source_path,
        "//! \\brief Inspectable summary.\nclass Inspectable\n{\n\t/*! \\param[in] value input value.\n\t * \\return true when accepted.\n\t * \\warning requires setup.\n\t * \\note fixture note.\n\t */\n\tbool Run(int value);\n}\n",
    )
    .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned(&game_data.arguments);
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Run"}}}));
    let search = client.response(2);
    let result = search
        .pointer("/result/structuredContent/results/0")
        .expect("search hit");
    let symbol_ref = result.get("symbolRef").cloned().expect("symbol reference");
    let revision = search
        .pointer("/result/structuredContent/catalogueRevision")
        .cloned()
        .expect("revision");

    client.send(json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Run","limit":1,"offset":1}}}));
    let direct_page = client.response(20);
    assert_eq!(direct_page.pointer("/result/isError"), Some(&json!(false)));
    assert!(direct_page
        .pointer("/result/structuredContent/results")
        .and_then(Value::as_array)
        .is_some_and(|results| results.is_empty()));

    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inspect_game_data_symbol","arguments":{"symbolRef":symbol_ref}}}));
    let inspected = client.response(3);
    assert_eq!(inspected.pointer("/result/isError"), Some(&json!(false)));
    assert_eq!(
        inspected.pointer("/result/structuredContent/documentation/parameters/0/name"),
        Some(&json!("value"))
    );
    assert_eq!(
        inspected.pointer("/result/structuredContent/documentation/parameters/0/direction"),
        Some(&json!("in"))
    );
    assert_eq!(
        inspected.pointer("/result/structuredContent/documentation/returns"),
        Some(&json!("true when accepted."))
    );
    assert_eq!(
        inspected.pointer("/result/structuredContent/documentation/warnings/0"),
        Some(&json!("requires setup."))
    );
    assert_eq!(
        inspected.pointer("/result/structuredContent/documentation/notes/0"),
        Some(&json!("fixture note."))
    );

    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"inspect_game_data_symbol","arguments":{"symbolRef":"sr1:not-a-reference"}}}));
    let invalid = client.response(4);
    assert_eq!(invalid.pointer("/result/isError"), Some(&json!(true)));
    assert_tool_error_code(&invalid, "invalid_symbol_ref");

    client.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_game_data_source","arguments":{"catalogueRevision":revision,"addonGuid":BASE_GAME_ADDON_GUID,"relativePath":"../Inspectable.c"}}}));
    let invalid_path = client.response(5);
    assert_eq!(invalid_path.pointer("/result/isError"), Some(&json!(true)));
    assert_tool_error_code(&invalid_path, "invalid_arguments");

    fs::write(&source_path, "class Inspectable { int changed; }\n").expect("change backing data");
    client.send(json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_game_data_source","arguments":{"catalogueRevision":revision,"addonGuid":BASE_GAME_ADDON_GUID,"relativePath":"scripts/Game/Inspectable.c"}}}));
    let changed = client.response(6);
    assert_eq!(changed.pointer("/result/isError"), Some(&json!(false)));
    assert!(changed
        .pointer("/result/structuredContent/content")
        .and_then(Value::as_str)
        .is_some_and(|content| content.contains("class Inspectable { int changed; }")));
}

#[test]
fn mcp_progressive_retrieval_enforces_member_documentation_and_source_bounds() {
    let fixture = TempFixture::new("mcp_progressive_bounds");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(scripts_root.join("Game")).expect("create scripts fixture");
    let source_path = scripts_root.join("Game").join("Bounds.c");
    let mut source = "/*! \\brief Bounds summary.\n".repeat(1);
    for _ in 0..3_000 {
        source.push_str(" * documentation payload keeps the raw comment bounded.\n");
    }
    source.push_str(" */\nclass Bounds\n{\n");
    for index in 0..55 {
        source.push_str(&format!("\tint Member{index:02};\n"));
    }
    source.push_str("}\n");
    for index in 0..600 {
        source.push_str(&format!("int TopLevel{index:03};\n"));
    }
    fs::write(&source_path, source).expect("write bounds fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned(&game_data.arguments);
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Bounds"}}}));
    let search = client.response(2);
    let hit = search
        .pointer("/result/structuredContent/results/0")
        .expect("Bounds hit");
    let symbol_ref = hit.get("symbolRef").cloned().expect("symbol reference");
    let revision = search
        .pointer("/result/structuredContent/catalogueRevision")
        .cloned()
        .expect("catalogue revision");

    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inspect_game_data_symbol","arguments":{"symbolRef":symbol_ref}}}));
    let inspected = client.response(3);
    assert_eq!(inspected.pointer("/result/isError"), Some(&json!(false)));
    let inspection = inspected
        .pointer("/result/structuredContent")
        .expect("inspection");
    assert_eq!(inspection.pointer("/rawTruncated"), Some(&json!(true)));
    assert!(inspection
        .pointer("/rawDocumentation")
        .and_then(Value::as_str)
        .is_some_and(|text| text.len() <= 16 * 1024));
    assert_eq!(inspection.pointer("/membersReturned"), Some(&json!(50)));
    assert_eq!(inspection.pointer("/membersTotal"), Some(&json!(55)));
    assert_eq!(inspection.pointer("/membersTruncated"), Some(&json!(true)));
    assert_eq!(
        inspection.pointer("/members/0/name"),
        Some(&json!("Member00"))
    );
    assert_eq!(
        inspection.pointer("/members/49/name"),
        Some(&json!("Member49"))
    );
    assert!(inspection
        .pointer("/membersTruncationGuidance")
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("list_game_data_symbol_members")));

    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_game_data_source","arguments":{"catalogueRevision":revision,"addonGuid":BASE_GAME_ADDON_GUID,"relativePath":"scripts/Game/Missing.c"}}}));
    assert_tool_error_code(&client.response(4), "invalid_arguments");
    client.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_game_data_source","arguments":{"catalogueRevision":"gd2:stale","addonGuid":BASE_GAME_ADDON_GUID,"relativePath":"scripts/Game/Bounds.c"}}}));
    assert_tool_error_code(&client.response(5), "stale_symbol_ref");
    client.send(json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_game_data_source","arguments":{"catalogueRevision":revision,"addonGuid":BASE_GAME_ADDON_GUID,"relativePath":"scripts/Game/Bounds.c","startLine":0}}}));
    assert_tool_error_code(&client.response(6), "invalid_arguments");
    client.send(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_game_data_source","arguments":{"catalogueRevision":revision,"addonGuid":BASE_GAME_ADDON_GUID,"relativePath":"scripts/Game/Bounds.c","startLine":1,"lineCount":999}}}));
    let read = client.response(7);
    assert_eq!(read.pointer("/result/isError"), Some(&json!(false)));
    assert_eq!(
        read.pointer("/result/structuredContent/truncated"),
        Some(&json!(true))
    );
}

#[test]
fn mcp_game_data_research_tools_complete_the_progressive_lookup_loop() {
    let fixture = TempFixture::new("mcp_game_data_research");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(scripts_root.join("Game").join("generated"))
        .expect("create generated fixture");
    fs::create_dir_all(scripts_root.join("Game").join("Examples"))
        .expect("create handwritten fixture");
    fs::write(
        scripts_root.join("Game").join("generated").join("SpawnApi.c"),
        "class Resource {}\nclass BaseWorld {}\nclass IEntity {}\nclass EntitySpawnParams {}\nclass Game\n{\n\t//! Spawn a prefab into a world.\n\tstatic IEntity SpawnEntityPrefab(Resource resource, BaseWorld world, EntitySpawnParams params);\n\tstatic void Ambiguous(int value);\n\tstatic void Ambiguous(string value);\n}\n",
    )
    .expect("write generated API fixture");
    fs::write(
        scripts_root
            .join("Game")
            .join("generated")
            .join("ResearchPatterns.c"),
        "class GeneratedResearchPatterns\n{\n\tstatic void ConfigureRpc(RplId ownerId)\n\t{\n\t\tRplRpc();\n\t\tRpc(ownerId);\n\t}\n\tstatic void OnPostInit(IEntity owner)\n\t{\n\t\tSetEventMask(owner, EntityEvent.INIT);\n\t}\n\tstatic void CreateWidgets(Widget parent)\n\t{\n\t\tCreateWidget(parent);\n\t}\n}\n",
    )
    .expect("write generated research fixture");
    fs::write(
        scripts_root.join("Game").join("Examples").join("BaseSpawner.c"),
        "class BaseSpawner\n{\n\t//! Perform the configured spawn.\n\tvoid SpawnConfigured(int count = 1);\n}\n",
    )
    .expect("write base fixture");
    fs::write(
        scripts_root.join("Game").join("Examples").join("VehicleSpawner.c"),
        "class VehicleSpawner : BaseSpawner\n{\n\tint Field00;\n\tint Field01;\n\tint Field02;\n\tint Field03;\n\tint Field04;\n\tint Field05;\n\t//! Spawn the configured prefab.\n\toverride void SpawnConfigured(int amount)\n\t{\n\t\tGame.SpawnEntityPrefab(null, null, new EntitySpawnParams());\n\t}\n}\n",
    )
    .expect("write handwritten spawn fixture");
    fs::write(
        scripts_root.join("Game").join("Examples").join("SecondSpawner.c"),
        "class SecondSpawner\n{\n\tvoid Run()\n\t{\n\t\tGame.SpawnEntityPrefab(null, null, new EntitySpawnParams());\n\t\tGame.SpawnEntityPrefab;\n\t\tGame.Ambiguous(1);\n\t\t// Resource.Load, ResourceName, PrefabResource, SpawnEntityPrefab, and EntitySpawnParams are comments, not stronger evidence.\n\t}\n}\n",
    )
    .expect("write second handwritten fixture");
    fs::write(
        scripts_root
            .join("Game")
            .join("Examples")
            .join("CommentOnly.c"),
        "class CommentOnly\n{\n\t// SpawnEntityPrefab, EntitySpawnParams, ResourceName, and SpawnConfigured are text only.\n}\n",
    )
    .expect("write comment-only fixture");
    fs::write(
        scripts_root
            .join("Game")
            .join("Examples")
            .join("ReplicationPattern.c"),
        "class ReplicationPattern\n{\n\tvoid ConfigureRpc(RplId ownerId)\n\t{\n\t\tRplRpc();\n\t\tRpc(ownerId);\n\t\tReplication.FindItem(ownerId);\n\t}\n}\n",
    )
    .expect("write replication fixture");
    fs::write(
        scripts_root
            .join("Game")
            .join("Examples")
            .join("EntityLifecyclePattern.c"),
        "class EntityLifecyclePattern\n{\n\tvoid OnPostInit(IEntity owner)\n\t{\n\t\tSetEventMask(owner, EntityEvent.INIT);\n\t}\n}\n",
    )
    .expect("write lifecycle fixture");
    fs::write(
        scripts_root
            .join("Game")
            .join("Examples")
            .join("WidgetPattern.c"),
        "class WidgetPattern\n{\n\tvoid CreateWidgets(Widget parent)\n\t{\n\t\tCreateWidget(parent);\n\t}\n}\n",
    )
    .expect("write widget fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned(&game_data.arguments);
    client.initialize(1);

    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let tools = client.response(2);
    let listed = tools
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tool catalogue");
    let tool = |name: &str| {
        listed
            .iter()
            .find(|tool| tool.get("name") == Some(&json!(name)))
            .unwrap_or_else(|| panic!("missing tool {name}"))
    };
    assert_eq!(listed.len(), 91);
    let research = tool("research_game_data");
    tool("list_game_data_symbol_members");
    tool("query_game_data_symbol_relationships");
    assert!(listed
        .iter()
        .all(|tool| tool.get("name") != Some(&json!("search_game_data_examples"))));
    let research_primary_schema = &research["outputSchema"]["$defs"]["IntentSymbol"]["properties"];
    for field in [
        "qualifiedName",
        "signature",
        "returnType",
        "conditionalContext",
        "relevantMembers",
        "readSourceInput",
    ] {
        assert!(
            research_primary_schema.get(field).is_some(),
            "compact research schema omitted {field}"
        );
    }
    let inspection_schema = &tool("inspect_game_data_symbol")["outputSchema"]["properties"];
    for field in [
        "baseType",
        "type",
        "returnType",
        "sourceCategory",
        "readSourceInput",
        "membersTruncationGuidance",
        "parentSymbolRef",
    ] {
        assert!(
            inspection_schema.get(field).is_some(),
            "inspection schema omitted {field}"
        );
    }

    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"SpawnConfigured"}}}));
    let symbol_search = client.response(3);
    assert_eq!(
        symbol_search.pointer("/result/structuredContent/results/0/documentationSummary"),
        Some(&json!("Perform the configured spawn."))
    );
    let base_method_ref = symbol_search
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("base method reference");

    client.send(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"VehicleSpawner","kinds":["class"]}}}));
    let class_search = client.response(7);
    let class_ref = class_search
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("class reference");
    client.send(json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"list_game_data_symbol_members","arguments":{"symbolRef":class_ref,"kinds":["method"],"limit":1}}}));
    let methods = client.response(8);
    assert_eq!(
        methods.pointer("/result/structuredContent/source"),
        Some(&json!("language-engine"))
    );
    assert_eq!(
        methods.pointer("/result/structuredContent/results/0/name"),
        Some(&json!("SpawnConfigured"))
    );
    let override_method_ref = methods
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("override method reference");
    client.send(json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"list_game_data_symbol_members","arguments":{"symbolRef":class_ref,"kinds":["field"],"limit":2}}}));
    let fields = client.response(9);
    assert_eq!(
        fields.pointer("/result/structuredContent/results/0/name"),
        Some(&json!("Field00"))
    );
    let member_cursor = fields
        .pointer("/result/structuredContent/nextCursor")
        .cloned()
        .expect("member cursor");
    client.send(json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"list_game_data_symbol_members","arguments":{"symbolRef":class_ref,"kinds":["field"],"limit":2,"cursor":member_cursor}}}));
    assert_eq!(
        client
            .response(10)
            .pointer("/result/structuredContent/results/0/name"),
        Some(&json!("Field02"))
    );

    client.send(json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"query_game_data_symbol_relationships","arguments":{"symbolRef":base_method_ref,"relationshipKinds":["implementation","override"],"limit":20}}}));
    let method_relationships = client.response(11);
    assert_eq!(
        method_relationships.pointer("/result/structuredContent/source"),
        Some(&json!("language-engine"))
    );
    let relationships = method_relationships
        .pointer("/result/structuredContent/results")
        .and_then(Value::as_array)
        .expect("method relationships");
    assert!(relationships
        .iter()
        .any(|result| result["relationshipKind"] == "override"
            && result["qualifiedName"] == "VehicleSpawner.SpawnConfigured"));
    assert!(relationships
        .iter()
        .any(|result| result["relationshipKind"] == "implementation"
            && result["qualifiedName"] == "VehicleSpawner.SpawnConfigured"));
    client.send(json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"query_game_data_symbol_relationships","arguments":{"symbolRef":class_ref,"relationshipKinds":["directBase"]}}}));
    assert_eq!(
        client
            .response(14)
            .pointer("/result/structuredContent/results/0/qualifiedName"),
        Some(&json!("BaseSpawner"))
    );
    client.send(json!({"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"query_game_data_symbol_relationships","arguments":{"symbolRef":override_method_ref,"relationshipKinds":["overriddenDeclaration"]}}}));
    assert_eq!(
        client
            .response(15)
            .pointer("/result/structuredContent/results/0/qualifiedName"),
        Some(&json!("BaseSpawner.SpawnConfigured"))
    );
    client.send(json!({"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"BaseSpawner","kinds":["class"]}}}));
    let base_class_ref = client
        .response(16)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("base class reference");
    client.send(json!({"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"query_game_data_symbol_relationships","arguments":{"symbolRef":base_class_ref,"relationshipKinds":["derivedType"]}}}));
    assert_eq!(
        client
            .response(17)
            .pointer("/result/structuredContent/results/0/qualifiedName"),
        Some(&json!("VehicleSpawner"))
    );
    client.send(json!({"jsonrpc":"2.0","id":19,"method":"tools/call","params":{"name":"inspect_game_data_symbol","arguments":{"symbolRef":class_ref}}}));
    let inspected = client.response(19);
    let inspected = inspected
        .pointer("/result/structuredContent")
        .and_then(Value::as_object)
        .expect("typed inspection result");
    for field in inspected.keys() {
        assert!(
            inspection_schema.get(field).is_some(),
            "runtime inspection field {field} is absent from the advertised schema"
        );
    }
}

#[test]
fn game_data_research_handoffs_reject_stale_references_and_cursors() {
    let fixture = TempFixture::new("mcp_research_stale");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    let source_path = scripts_root.join("Paged.c");
    fs::write(
        &source_path,
        "class Paged\n{\n\tint First;\n\tint Second;\n}\nclass Base {}\nclass DerivedA : Base {}\nclass DerivedB : Base {}\n",
    )
    .expect("write source");
    fs::write(
        scripts_root.join("ExampleA.c"),
        "class ExampleA { void Run() { Game.SpawnEntityPrefab(null, null, new EntitySpawnParams()); } }\n",
    )
    .expect("write first example");
    fs::write(
        scripts_root.join("ExampleB.c"),
        "class ExampleB { void Run() { Game.SpawnEntityPrefab(null, null, new EntitySpawnParams()); } }\n",
    )
    .expect("write second example");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let arguments = game_data.arguments;

    let mut first = McpClient::spawn_owned(&arguments);
    first.initialize(1);
    first.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Paged","kinds":["class"]}}}));
    let old_symbol_ref = first
        .response(2)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("old symbol reference");
    first.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_game_data_symbol_members","arguments":{"symbolRef":old_symbol_ref,"kinds":["field"],"limit":1}}}));
    let old_cursor = first
        .response(3)
        .pointer("/result/structuredContent/nextCursor")
        .cloned()
        .expect("old member cursor");
    first.send(json!({"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Base","kinds":["class"]}}}));
    let base_ref = first
        .response(31)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("base reference");
    first.send(json!({"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"query_game_data_symbol_relationships","arguments":{"symbolRef":base_ref,"relationshipKinds":["derivedType"],"limit":1}}}));
    let relationship_response = first.response(32);
    let old_relationship_cursor = relationship_response
        .pointer("/result/structuredContent/nextCursor")
        .cloned();
    if old_relationship_cursor.is_none() {
        assert_tool_error_code(&relationship_response, "source_evidence_unavailable");
    }
    if let Some(old_relationship_cursor) = &old_relationship_cursor {
        first.send(json!({"jsonrpc":"2.0","id":36,"method":"tools/call","params":{"name":"query_game_data_symbol_relationships","arguments":{"symbolRef":base_ref,"relationshipKinds":["derivedType"],"limit":1,"cursor":old_relationship_cursor}}}));
        assert_eq!(
            first
                .response(36)
                .pointer("/result/structuredContent/returned"),
            Some(&json!(1)),
            "a valid relationship cursor must continue deterministically"
        );
    }
    first.close_stdin();
    assert!(first.wait_for_exit(Duration::from_secs(3)));

    fs::write(
        &source_path,
        "class Paged\n{\n\tint First;\n\tint Second;\n\tint Third;\n}\nclass Base {}\nclass DerivedA : Base {}\nclass DerivedB : Base {}\n",
    )
    .expect("change source revision");
    build_game_data_cache(&scripts_root, &cache_path);
    let mut second = McpClient::spawn_owned(&arguments);
    second.initialize(4);
    second.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Paged","kinds":["class"]}}}));
    let fresh_symbol_ref = second
        .response(5)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("fresh symbol reference");
    second.send(json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_game_data_symbol_members","arguments":{"symbolRef":old_symbol_ref,"kinds":["field"]}}}));
    assert_tool_error_code(&second.response(6), "stale_symbol_ref");
    second.send(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_game_data_symbol_members","arguments":{"symbolRef":fresh_symbol_ref,"kinds":["field"],"limit":1,"cursor":old_cursor}}}));
    assert_tool_error_code(&second.response(7), "stale_cursor");
    second.send(json!({"jsonrpc":"2.0","id":34,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Base","kinds":["class"]}}}));
    let fresh_base_ref = second
        .response(34)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("fresh base reference");
    if let Some(old_relationship_cursor) = old_relationship_cursor {
        second.send(json!({"jsonrpc":"2.0","id":35,"method":"tools/call","params":{"name":"query_game_data_symbol_relationships","arguments":{"symbolRef":fresh_base_ref,"relationshipKinds":["derivedType"],"limit":1,"cursor":old_relationship_cursor}}}));
        assert_tool_error_code(&second.response(35), "stale_cursor");
    }
}

#[test]
fn composed_relationship_tool_discovers_and_pages_cross_source_semantic_edges() {
    let fixture = TempFixture::new("mcp_source_relationships");
    let game_scripts = fixture.path().join("game").join("scripts");
    let workspace_scripts = fixture.path().join("workspace").join("Scripts");
    fs::create_dir_all(game_scripts.join("Game").join("Vehicles")).unwrap();
    fs::create_dir_all(workspace_scripts.join("Game").join("Vehicles")).unwrap();
    fs::write(
        game_scripts.join("Game").join("Vehicles").join("Vehicle.c"),
        "class Vehicle\n{\n\tvoid Move(int speed);\n}\n",
    )
    .unwrap();
    fs::write(
        workspace_scripts
            .join("Game")
            .join("Vehicles")
            .join("Car.c"),
        "class Car : Vehicle\n{\n\toverride void Move(int speed) {}\n}\n",
    )
    .unwrap();
    fs::write(
        workspace_scripts
            .join("Game")
            .join("Vehicles")
            .join("VehicleMod.c"),
        "modded class Vehicle\n{\n\toverride void Move(int speed) {}\n}\n",
    )
    .unwrap();
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let mut launch = build_game_data_cache(&game_scripts, &cache_path).arguments;
    launch.push("--workspace-scripts".to_string());
    launch.push(workspace_scripts.to_string_lossy().into_owned());
    let mut client = McpClient::spawn_owned(&launch);
    client.initialize(1);

    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Vehicle","kinds":["class"]}}}));
    let search = client.response(2);
    let anchor = search
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("exact Game Data anchor");
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":anchor,"includeWorkspace":true,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["derivedType","moddedExtension"],"depth":"all","limit":1}}}));
    let first = client.response(3);
    assert_eq!(
        first.pointer("/result/isError"),
        Some(&json!(false)),
        "{first}"
    );
    assert_eq!(
        first.pointer("/result/structuredContent/total"),
        Some(&json!(2)),
        "{first}"
    );
    assert_eq!(
        first.pointer("/result/structuredContent/results/0/source"),
        Some(&json!("workspace"))
    );
    assert!(first
        .pointer("/result/structuredContent/results/0/readSourceInput/relativePath")
        .and_then(Value::as_str)
        .is_some());
    assert!(first
        .pointer("/result/structuredContent/results/0/sourceUri")
        .and_then(Value::as_str)
        .is_some_and(|uri| uri.starts_with("file:")));
    let cursor = first
        .pointer("/result/structuredContent/nextCursor")
        .cloned()
        .expect("relationship next cursor");
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":anchor,"includeWorkspace":true,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["derivedType","moddedExtension"],"depth":"all","limit":1,"cursor":cursor}}}));
    let second = client.response(4);
    assert_eq!(
        second.pointer("/result/structuredContent/returned"),
        Some(&json!(1)),
        "{second}"
    );
    let read_input = first
        .pointer("/result/structuredContent/results/0/readSourceInput")
        .cloned()
        .expect("exact workspace read handoff");
    client.send(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_workspace_source","arguments":read_input}}));
    let read = client.response(7);
    assert_eq!(
        read.pointer("/result/isError"),
        Some(&json!(false)),
        "{read}"
    );
    assert!(read
        .pointer("/result/structuredContent/content")
        .and_then(Value::as_str)
        .is_some_and(
            |source| source.contains("class Car") || source.contains("modded class Vehicle")
        ));

    client.send(json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":anchor,"includeWorkspace":false,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["derivedType","moddedExtension"],"depth":"all","limit":1,"cursor":cursor}}}));
    assert_tool_error_code(&client.response(8), "stale_relationship_cursor");

    let stale_anchor = with_stale_catalogue_revision(&anchor);
    client.send(json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":stale_anchor,"includeWorkspace":true,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["direct"],"depth":"one","limit":20}}}));
    let stale = client.response(9);
    assert_tool_error_code(&stale, "stale_relationship_anchor");
    assert!(stale
        .pointer("/result/structuredContent/recovery")
        .and_then(Value::as_str)
        .is_some_and(|recovery| recovery.contains("rerun semantic discovery")));

    client.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Move","kinds":["method"]}}}));
    let method_anchor = client
        .response(5)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("exact method anchor");
    client.send(json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":method_anchor,"includeWorkspace":true,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["override"],"depth":"all","limit":20}}}));
    let overrides = client.response(6);
    assert_eq!(
        overrides.pointer("/result/structuredContent/total"),
        Some(&json!(2)),
        "{overrides}"
    );
}

#[test]
fn composed_relationship_tool_resolves_through_hidden_sources_and_rejects_requested_unavailable_scope(
) {
    let fixture = TempFixture::new("mcp_source_relationship_scope");
    let game_scripts = fixture.path().join("game").join("scripts");
    let workspace_scripts = fixture.path().join("workspace").join("Scripts");
    fs::create_dir_all(&game_scripts).unwrap();
    fs::create_dir_all(&workspace_scripts).unwrap();
    fs::write(
        game_scripts.join("Hierarchy.c"),
        "class Vehicle {}\nclass Leaf : Middle {}\n",
    )
    .unwrap();
    fs::write(
        workspace_scripts.join("Middle.c"),
        "class Middle : Vehicle {}\n",
    )
    .unwrap();
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&game_scripts, &cache_path);

    let mut unavailable = McpClient::spawn_owned(&game_data.arguments);
    unavailable.initialize(1);
    unavailable.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Vehicle","kinds":["class"]}}}));
    let unavailable_anchor = unavailable
        .response(2)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("exact Game Data anchor");
    unavailable.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":unavailable_anchor,"includeWorkspace":true,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["derivedType"],"depth":"all","limit":20}}}));
    assert_tool_error_code(&unavailable.response(3), "source_relationships_unavailable");
    unavailable.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":unavailable_anchor,"includeWorkspace":false,"addonGuids":[BASE_GAME_ADDON_GUID,"DEADBEEFDEADBEEF"],"relationshipKinds":["derivedType"],"depth":"all","limit":20}}}));
    let unavailable_addon = unavailable.response(4);
    assert_tool_error_code(&unavailable_addon, "source_relationships_unavailable");
    assert!(unavailable_addon
        .pointer("/result/structuredContent/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("DEADBEEFDEADBEEF")));
    unavailable.close_stdin();
    assert!(unavailable.wait_for_exit(Duration::from_secs(3)));

    let mut arguments = game_data.arguments;
    arguments.push("--workspace-scripts".to_string());
    arguments.push(workspace_scripts.to_string_lossy().into_owned());
    let mut hidden = McpClient::spawn_owned(&arguments);
    hidden.initialize(4);
    hidden.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Vehicle","kinds":["class"]}}}));
    let anchor = hidden
        .response(5)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("exact Game Data anchor");
    hidden.send(json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":anchor,"includeWorkspace":false,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["derivedType"],"depth":"all","limit":20}}}));
    let response = hidden.response(6);
    assert_eq!(
        response.pointer("/result/structuredContent/results/0/qualifiedName"),
        Some(&json!("Leaf")),
        "{response}"
    );
    assert_eq!(
        response.pointer("/result/structuredContent/results/0/distance"),
        Some(&json!(2)),
        "{response}"
    );
    assert!(response
        .pointer("/result/structuredContent/warnings")
        .and_then(Value::as_array)
        .is_some_and(|warnings| warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|warning| warning.contains("hidden by the selected output scope")))));
}

#[test]
fn lossy_utf8_game_data_remains_searchable_and_readable() {
    let fixture = TempFixture::new("mcp_lossy_utf8");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(scripts_root.join("Game")).expect("create scripts fixture");
    let mut source = b"class LossyFixture\n{\n\t// invalid byte: ".to_vec();
    source.push(0xff);
    source.extend_from_slice(b"\n}\n");
    fs::write(scripts_root.join("Game").join("LossyFixture.c"), source)
        .expect("write lossy UTF-8 fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned(&game_data.arguments);
    client.initialize(1);

    client.send(status_call(2));
    let status = client.response(2);
    assert_eq!(
        status.pointer("/result/structuredContent/available"),
        Some(&json!(true))
    );
    assert_eq!(
        status.pointer("/result/structuredContent/coverage/lossyFiles"),
        Some(&json!(1))
    );
    assert_eq!(
        status.pointer("/result/structuredContent/warnings/0/code"),
        Some(&json!("lossy_files_present"))
    );

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "search_game_data_symbols",
            "arguments": {"query": "LossyFixture"}
        }
    }));
    let search = client.response(3);
    let read_input = search
        .pointer("/result/structuredContent/results/0/readSourceInput")
        .cloned()
        .expect("lossy source read handoff");

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "read_game_data_source",
            "arguments": read_input
        }
    }));
    let read = client.response(4);
    assert_eq!(read.pointer("/result/isError"), Some(&json!(false)));
    assert!(read
        .pointer("/result/structuredContent/content")
        .and_then(Value::as_str)
        .is_some_and(|content| content.contains('\u{fffd}')));
}

#[test]
fn game_data_revision_is_immutable_per_process_and_shared_cache_loads_warm() {
    let fixture = TempFixture::new("mcp_immutable_revision");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(
        scripts_root.join("RevisionFixture.c"),
        "class RevisionFixture {}",
    )
    .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let arguments = game_data.arguments;

    let mut first = McpClient::spawn_owned(&arguments);
    first.initialize(1);
    let cold = first.call_status(2);
    let repeated = first.call_status(3);
    assert_eq!(cold, repeated, "one process retains one immutable snapshot");
    assert_eq!(cold.get("scopeAuthority"), Some(&json!("workbench-loaded")));
    assert_eq!(cold.get("cache"), None);
    let revision = cold
        .get("catalogueRevision")
        .cloned()
        .expect("cold catalogue revision");
    first.close_stdin();
    assert!(first.wait_for_exit(Duration::from_secs(3)));

    let mut second = McpClient::spawn_owned(&arguments);
    second.initialize(4);
    let warm = second.call_status(5);
    assert_eq!(warm.get("scopeAuthority"), Some(&json!("workbench-loaded")));
    assert_eq!(warm.get("cache"), None);
    assert_eq!(warm.get("catalogueRevision"), Some(&revision));
    second.close_stdin();
    assert!(second.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn unavailable_status_and_malformed_calls_are_sanitized_and_process_isolated() {
    let mut client = McpClient::spawn(&["mcp"]);
    client.initialize(1);

    let unavailable = client.call_status(2);
    assert_eq!(unavailable.get("available"), Some(&json!(false)));
    assert_eq!(
        unavailable.pointer("/warnings/0/code"),
        Some(&json!("game_data_addon_scope_not_configured"))
    );
    assert_eq!(
        unavailable.pointer("/limits/initializationDeadlineMs"),
        Some(&json!(120_000))
    );
    assert!(
        !unavailable.to_string().contains(":\\"),
        "sanitized status must not contain a Windows physical path"
    );

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "game_data_status",
            "arguments": {"unexpected": true}
        }
    }));
    let invalid = client.response(3);
    assert_eq!(invalid.pointer("/error/code"), Some(&json!(-32602)));
    assert!(invalid
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("empty object")));

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "not_a_tool",
            "arguments": {}
        }
    }));
    assert_eq!(
        client.response(4).pointer("/error/code"),
        Some(&json!(-32602))
    );

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "search_game_data_symbols",
            "arguments": {"query": "", "unexpected": true}
        }
    }));
    assert_eq!(
        client.response(5).pointer("/error/code"),
        Some(&json!(-32602))
    );

    client.send(json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "ping",
        "params": {}
    }));
    assert_eq!(client.response(6).get("result"), Some(&json!({})));

    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn workspace_search_returns_a_revision_bound_source_handoff() {
    let fixture = TempFixture::new("workspace_search");
    let scripts = fixture.path().join("Scripts");
    fs::create_dir_all(&scripts).expect("create workspace scripts");
    fs::write(
        scripts.join("Example.c"),
        "class Example { void Run() {} }\n",
    )
    .expect("write workspace script");
    let mut client = McpClient::spawn(&[
        "mcp",
        "--workspace-scripts",
        scripts.to_str().expect("utf-8 workspace scripts"),
    ]);
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_workspace_symbols","arguments":{"query":"Run"}}}));
    let page = client
        .response(2)
        .pointer("/result/structuredContent")
        .cloned()
        .expect("workspace search page");
    assert_eq!(page.pointer("/results/0/name"), Some(&json!("Run")));
    assert_eq!(
        page.pointer("/results/0/sourceCategory"),
        Some(&json!("workspace"))
    );
    let read_input = page
        .pointer("/results/0/readSourceInput")
        .cloned()
        .expect("workspace source handoff");
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_workspace_source","arguments":read_input}}));
    assert_eq!(
        client
            .response(3)
            .pointer("/result/structuredContent/content"),
        Some(&json!("class Example { void Run() {} }\n"))
    );
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_workspace_symbols","arguments":{"query":"Run","owner":"Example"}}}));
    assert_eq!(
        client.response(4).pointer("/error/code"),
        Some(&json!(-32602))
    );
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn search_official_wiki_projects_validated_sections_and_keeps_the_session_healthy() {
    let fixture = TempFixture::new("official_wiki_search");
    let wiki_root = fixture.path().join("official-wiki");
    fs::create_dir_all(wiki_root.join("Guides")).expect("create wiki fixture");
    fs::write(
        wiki_root.join("wiki-index.md"),
        "# Wiki Markdown Index\nneedle ignored\n",
    )
    .expect("write index");
    fs::write(wiki_root.join("Guides").join("Guide.md"), "# [Guide](https://community.bistudio.com/wiki/Arma_Reforger:Guide)\n\n## Needle\nneedle prose\n\n## Another\nneedle prose\n").expect("write page");
    let mut client = McpClient::spawn(&[
        "mcp",
        "--official-wiki-root",
        wiki_root.to_str().expect("utf-8 wiki root"),
    ]);
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_official_wiki","arguments":{"query":"needle","pathPrefix":"Guides","limit":1}}}));
    let response = client.response(2);
    let page = response
        .pointer("/result/structuredContent")
        .expect("structured search page");
    assert_eq!(page.get("returned"), Some(&json!(1)));
    assert_eq!(page.get("total"), Some(&json!(2)));
    assert_eq!(page.pointer("/results/0/heading"), Some(&json!("Needle")));
    let matched_line = page
        .pointer("/results/0/matchedLine")
        .and_then(Value::as_u64)
        .expect("matched line evidence");
    let excerpt_start = page
        .pointer("/results/0/excerptStartLine")
        .and_then(Value::as_u64)
        .expect("excerpt start line evidence");
    let excerpt_end = page
        .pointer("/results/0/excerptEndLine")
        .and_then(Value::as_u64)
        .expect("excerpt end line evidence");
    assert!(excerpt_start <= matched_line && matched_line <= excerpt_end);
    assert!(page.pointer("/results/0/excerpt").is_some());
    assert_eq!(
        page.pointer("/results/0/readInput/relativePath"),
        Some(&json!("Guides/Guide.md"))
    );
    assert_eq!(
        response.pointer("/result/content/0/text"),
        Some(&Value::String(page.to_string()))
    );
    let cursor = page.get("nextCursor").cloned().expect("next cursor");
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_official_wiki","arguments":{"query":"needle","pathPrefix":"Guides/","cursor":cursor}}}));
    assert_eq!(
        client
            .response(3)
            .pointer("/result/structuredContent/results/0/heading"),
        Some(&json!("Another"))
    );
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_official_wiki","arguments":{"query":"","unexpected":true}}}));
    assert_eq!(
        client.response(4).pointer("/error/code"),
        Some(&json!(-32602))
    );
    client.send(json!({"jsonrpc":"2.0","id":5,"method":"ping","params":{}}));
    assert_eq!(client.response(5).get("result"), Some(&json!({})));
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn read_official_wiki_follows_search_handoffs_with_bounded_continuations() {
    let fixture = TempFixture::new("official_wiki_read");
    let wiki_root = fixture.path().join("official-wiki");
    fs::create_dir_all(wiki_root.join("Guides")).expect("create wiki fixture");
    fs::write(
        wiki_root.join("Guides").join("Unicode.md"),
        "# [Unicode guide](https://community.bistudio.com/wiki/Arma_Reforger:Unicode)\n\n## Needle\nfirst caf\u{e9}\nsecond line\nthird line\n",
    )
    .expect("write page");
    let mut client = McpClient::spawn(&[
        "mcp",
        "--official-wiki-root",
        wiki_root.to_str().expect("utf-8 wiki root"),
    ]);
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_official_wiki","arguments":{"query":"needle"}}}));
    let input = client
        .response(2)
        .pointer("/result/structuredContent/results/0/readInput")
        .cloned()
        .expect("copy-ready read input");
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_official_wiki","arguments":input}}));
    let page = client
        .response(3)
        .pointer("/result/structuredContent")
        .cloned()
        .expect("structured wiki read");
    assert_eq!(page.get("relativePath"), Some(&json!("Guides/Unicode.md")));
    assert_eq!(
        page.get("sourceUrl"),
        Some(&json!(
            "https://community.bistudio.com/wiki/Arma_Reforger:Unicode"
        ))
    );
    assert_eq!(page.get("startLine"), Some(&json!(3)));
    assert_eq!(page.get("endLine"), Some(&json!(6)));
    assert_eq!(
        page.get("content"),
        Some(&json!(
            "## Needle\nfirst caf\u{e9}\nsecond line\nthird line\n"
        ))
    );
    assert_eq!(page.get("truncated"), Some(&json!(false)));
    assert_eq!(page.get("continuation"), Some(&Value::Null));
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision": input["corpusRevision"], "relativePath":input["relativePath"], "startLine":3, "lineCount":2}}}));
    let bounded = client.response(4);
    assert_eq!(
        bounded.pointer("/result/structuredContent/content"),
        Some(&json!("## Needle\nfirst caf\u{e9}\n"))
    );
    assert_eq!(
        bounded.pointer("/result/structuredContent/truncated"),
        Some(&json!(true))
    );
    assert_eq!(
        bounded.pointer("/result/structuredContent/continuation/startLine"),
        Some(&json!(5))
    );
    client.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision": input["corpusRevision"], "relativePath":"../Unicode.md"}}}));
    assert_tool_error_code(&client.response(5), "invalid_path");
    client.send(json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision": input["corpusRevision"], "relativePath":"Guides/Unicode.md", "unexpected":true}}}));
    assert_eq!(
        client.response(6).pointer("/error/code"),
        Some(&json!(-32602))
    );
    client.send(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision":"ow1:stale", "relativePath":"Guides/Unicode.md"}}}));
    assert_tool_error_code(&client.response(7), "stale_corpus_revision");
    client.send(json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision": input["corpusRevision"], "relativePath":"Guides/Missing.md"}}}));
    assert_tool_error_code(&client.response(8), "invalid_path");
    client.send(json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision": input["corpusRevision"], "relativePath":"Guides/Unicode.md", "startLine":0}}}));
    assert_tool_error_code(&client.response(9), "invalid_range");
    client.send(json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision": input["corpusRevision"], "relativePath":"Guides/Unicode.md", "startLine":7}}}));
    assert_tool_error_code(&client.response(10), "invalid_range");
    fs::write(
        wiki_root.join("Guides").join("Unicode.md"),
        "# [Unicode guide](https://community.bistudio.com/wiki/Arma_Reforger:Unicode)\nchanged\n",
    )
    .expect("change validated page");
    client.send(json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"read_official_wiki","arguments":input}}));
    assert_tool_error_code(&client.response(11), "official_wiki_changed");
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn read_official_wiki_rejects_a_reparse_escape_after_validation() {
    let fixture = TempFixture::new("official_wiki_reparse_escape");
    let wiki_root = fixture.path().join("official-wiki");
    let outside_root = fixture.path().join("outside");
    fs::create_dir_all(wiki_root.join("Escaped")).expect("create wiki fixture");
    fs::create_dir_all(&outside_root).expect("create outside fixture");
    fs::write(
        wiki_root.join("Escaped").join("Inside.md"),
        "# [Escaped](https://community.bistudio.com/wiki/Arma_Reforger:Escaped)\ntrusted\n",
    )
    .expect("write validated page");
    fs::write(
        outside_root.join("Inside.md"),
        "# [Outside](https://community.bistudio.com/wiki/Arma_Reforger:Outside)\nuntrusted\n",
    )
    .expect("write outside page");
    let mut client = McpClient::spawn(&[
        "mcp",
        "--official-wiki-root",
        wiki_root.to_str().expect("utf-8 wiki root"),
    ]);
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_official_wiki","arguments":{"query":"escaped"}}}));
    let input = client
        .response(2)
        .pointer("/result/structuredContent/results/0/readInput")
        .cloned()
        .expect("validated read input");
    let escaped = wiki_root.join("Escaped");
    fs::remove_dir_all(&escaped).expect("remove validated page directory");
    create_directory_reparse_escape(&outside_root, &escaped);
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_official_wiki","arguments":input}}));
    assert_tool_error_code(&client.response(3), "official_wiki_changed");
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[cfg(target_os = "windows")]
fn create_directory_reparse_escape(target: &Path, link: &Path) {
    assert!(
        Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .expect("create junction")
            .success(),
        "create reparse escape"
    );
}

#[cfg(unix)]
fn create_directory_reparse_escape(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create symlink escape");
}

#[test]
fn read_official_wiki_defaults_and_clamps_its_line_window() {
    let fixture = TempFixture::new("official_wiki_read_bounds");
    let wiki_root = fixture.path().join("official-wiki");
    fs::create_dir_all(&wiki_root).expect("create wiki fixture");
    let page = format!(
        "# [Bounds](https://community.bistudio.com/wiki/Arma_Reforger:Bounds)\n{}",
        (1..=600)
            .map(|line| format!("line {line}\n"))
            .collect::<String>()
    );
    fs::write(wiki_root.join("Bounds.md"), page).expect("write page");
    fs::write(
        wiki_root.join("Large.md"),
        format!(
            "# [Large](https://community.bistudio.com/wiki/Arma_Reforger:Large)\n{}\nfinal line\n",
            "x".repeat(128 * 1024 + 1),
        ),
    )
    .expect("write bounded page");
    let mut client = McpClient::spawn(&[
        "mcp",
        "--official-wiki-root",
        wiki_root.to_str().expect("utf-8 wiki root"),
    ]);
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"official_wiki_status","arguments":{}}}));
    let revision = client
        .response(2)
        .pointer("/result/structuredContent/corpusRevision")
        .cloned()
        .expect("corpus revision");
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision":revision,"relativePath":"Bounds.md","lineCount":1000}}}));
    let capped = client.response(3);
    assert_eq!(
        capped.pointer("/result/structuredContent/startLine"),
        Some(&json!(1))
    );
    assert_eq!(
        capped.pointer("/result/structuredContent/endLine"),
        Some(&json!(500))
    );
    assert_eq!(
        capped.pointer("/result/structuredContent/continuation/startLine"),
        Some(&json!(501))
    );
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision":revision,"relativePath":"Bounds.md","startLine":501}}}));
    assert_eq!(
        client
            .response(4)
            .pointer("/result/structuredContent/endLine"),
        Some(&json!(601))
    );
    client.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision":revision,"relativePath":"Large.md"}}}));
    let bounded = client.response(5);
    let bounded_content = bounded
        .pointer("/result/structuredContent/content")
        .and_then(Value::as_str)
        .expect("bounded content");
    assert!(bounded_content.len() <= 128 * 1024);
    assert!(bounded_content.ends_with('\n'));
    assert_eq!(
        bounded.pointer("/result/structuredContent/endLine"),
        Some(&json!(1))
    );
    assert_eq!(
        bounded.pointer("/result/structuredContent/continuation/startLine"),
        Some(&json!(2))
    );
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn read_official_wiki_cancellation_and_deadline_do_not_block_the_next_request() {
    let fixture = TempFixture::new("official_wiki_read_cancellation");
    let wiki_root = fixture.path().join("official-wiki");
    fs::create_dir_all(&wiki_root).expect("create wiki fixture");
    fs::write(
        wiki_root.join("Page.md"),
        "# [Page](https://community.bistudio.com/wiki/Arma_Reforger:Page)\ncontent\n",
    )
    .expect("write page");
    let mut client = McpClient::spawn_with_env(
        &[
            "mcp",
            "--official-wiki-root",
            wiki_root.to_str().expect("utf-8 wiki root"),
        ],
        &[("REFORGER_MCP_TEST_OFFICIAL_WIKI_READ_DELAY_MS", "5000")],
    );
    client.initialize(1);
    client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"official_wiki_status","arguments":{}}}));
    let revision = client
        .response(2)
        .pointer("/result/structuredContent/corpusRevision")
        .cloned()
        .expect("corpus revision");
    client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision":revision,"relativePath":"Page.md"}}}));
    client.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"test cancellation"}}));
    client.send(json!({"jsonrpc":"2.0","id":4,"method":"ping","params":{}}));
    let responses = client.responses_until(4);
    assert!(responses
        .iter()
        .all(|response| response.get("id") != Some(&json!(3))));
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));

    let mut deadline_client = McpClient::spawn_with_env(
        &[
            "mcp",
            "--official-wiki-root",
            wiki_root.to_str().expect("utf-8 wiki root"),
        ],
        &[
            ("REFORGER_MCP_TEST_OFFICIAL_WIKI_READ_DELAY_MS", "5000"),
            ("REFORGER_MCP_TEST_OFFICIAL_WIKI_DEADLINE_MS", "50"),
        ],
    );
    deadline_client.initialize(1);
    deadline_client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"official_wiki_status","arguments":{}}}));
    let deadline_revision = deadline_client
        .response(2)
        .pointer("/result/structuredContent/corpusRevision")
        .cloned()
        .expect("corpus revision");
    let started = std::time::Instant::now();
    deadline_client.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_official_wiki","arguments":{"corpusRevision":deadline_revision,"relativePath":"Page.md"}}}));
    let deadline = deadline_client.response(3);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_tool_error_code(&deadline, "deadline_exceeded");
    deadline_client.send(json!({"jsonrpc":"2.0","id":4,"method":"ping","params":{}}));
    assert_eq!(deadline_client.response(4).get("result"), Some(&json!({})));
    deadline_client.close_stdin();
    assert!(deadline_client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn official_wiki_status_reports_a_validated_packaged_style_corpus() {
    let fixture = TempFixture::new("official_wiki_status");
    let wiki_root = fixture.path().join("official-wiki");
    fs::create_dir_all(wiki_root.join("Guides")).expect("create wiki fixture");
    fs::write(
        wiki_root.join("index.md"),
        "# [Official index](https://community.bistudio.com/wiki/Category:Arma_Reforger)\n\nAuthoritative index.\n",
    )
    .expect("write index");
    fs::write(
        wiki_root.join("Guides").join("Getting Started.md"),
        "# [Getting Started](https://community.bistudio.com/wiki/Arma_Reforger:Getting_Started)\n\nStart here.\n",
    )
    .expect("write page");
    fs::write(wiki_root.join("wiki-index.md"), "# Wiki Markdown Index\n")
        .expect("write excluded index");
    fs::write(
        wiki_root.join("Guides").join("untrusted.md"),
        "# [Untrusted](https://example.com/wiki/not-authoritative)\n",
    )
    .expect("write malformed page");

    let mut client = McpClient::spawn(&[
        "mcp",
        "--official-wiki-root",
        wiki_root.to_str().expect("utf-8 wiki root"),
    ]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0", "id":2, "method":"tools/call",
        "params":{"name":"official_wiki_status", "arguments":{}}
    }));
    let response = client.response(2);
    let status = response
        .pointer("/result/structuredContent")
        .expect("structured official wiki status");
    assert_eq!(status.get("available"), Some(&json!(true)));
    assert_eq!(status.get("source"), Some(&json!("evidence-catalogue")));
    assert_eq!(status.get("fileCount"), Some(&json!(2)));
    assert_eq!(status.get("invalidFileCount"), Some(&json!(1)));
    assert_eq!(
        status.get("invalidFiles"),
        Some(&json!(["Guides/untrusted.md"]))
    );
    assert_eq!(status.get("excludedFiles"), Some(&json!(["wiki-index.md"])));
    assert!(status
        .get("corpusRevision")
        .and_then(Value::as_str)
        .is_some_and(|revision| revision.starts_with("ow1:")));
    assert_eq!(
        response.pointer("/result/content/0/text"),
        Some(&Value::String(status.to_string()))
    );
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn official_wiki_status_resolves_resources_from_an_installed_extension_layout() {
    let fixture = TempFixture::new("installed_official_wiki");
    let extension = fixture.path().join("extension");
    let runtime = extension
        .join("dist")
        .join("server")
        .join("test-platform")
        .join("reforger_language_server.exe");
    let wiki_root = extension.join("data").join("official-wiki");
    fs::create_dir_all(runtime.parent().expect("runtime parent"))
        .expect("create runtime directory");
    fs::create_dir_all(&wiki_root).expect("create installed corpus");
    fs::copy(env!("CARGO_BIN_EXE_reforger_language_server"), &runtime)
        .expect("copy packaged runtime");
    fs::write(
        wiki_root.join("index.md"),
        "# [Official index](https://community.bistudio.com/wiki/Category:Arma_Reforger)\n",
    )
    .expect("write installed index");

    let mut client = McpClient::spawn_program(&runtime, &["mcp"]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0", "id":2, "method":"tools/call",
        "params":{"name":"official_wiki_status", "arguments":{}}
    }));
    assert_eq!(
        client
            .response(2)
            .pointer("/result/structuredContent/available"),
        Some(&json!(true))
    );
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn cancellation_and_eof_with_in_flight_initialization_shutdown_cleanly() {
    let fixture = TempFixture::new("mcp_cancel");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(
        scripts_root.join("CancellationFixture.c"),
        "class CancellationFixture { int m_Value; }",
    )
    .expect("write cancellation fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let started_marker = fixture.path().join("initialization-started");
    let mut client = McpClient::spawn_owned_with_env(
        &game_data.arguments,
        &[
            ("REFORGER_MCP_TEST_INITIALIZATION_DELAY_MS", "5000"),
            (
                "REFORGER_MCP_TEST_INITIALIZATION_STARTED_MARKER",
                started_marker.to_str().expect("utf-8 marker path"),
            ),
        ],
    );
    client.initialize(1);
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "search_game_data_symbols",
            "arguments": {"query": "CancellationFixture"}
        }
    }));
    wait_for_file(&started_marker, Duration::from_secs(2));
    client.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {
            "requestId": 2,
            "reason": "wire cancellation test"
        }
    }));
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "ping",
        "params": {}
    }));

    let before_ping = client.responses_until(3);
    assert!(
        before_ping
            .iter()
            .all(|message| message.get("id") != Some(&json!(2))),
        "cancelled request must not publish a stale tool result"
    );
    client.close_stdin();
    assert!(
        client.wait_for_exit(Duration::from_secs(5)),
        "EOF must shut down even while blocking initialization unwinds"
    );
    assert!(
        !cache_path.exists(),
        "cancelled initialization must not continue into cache publication"
    );
}

#[test]
fn initialization_deadline_cancels_work_and_returns_stable_tool_error() {
    let fixture = TempFixture::new("mcp_deadline");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(scripts_root.join("Deadline.c"), "class Deadline {}")
        .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let started_marker = fixture.path().join("initialization-started");
    let mut client = McpClient::spawn_owned_with_env(
        &game_data.arguments,
        &[
            ("REFORGER_MCP_TEST_UNINTERRUPTIBLE_DELAY_MS", "5000"),
            ("REFORGER_MCP_TEST_INITIALIZATION_DEADLINE_MS", "50"),
            (
                "REFORGER_MCP_TEST_INITIALIZATION_STARTED_MARKER",
                started_marker.to_str().expect("utf-8 marker path"),
            ),
        ],
    );
    client.initialize(1);
    let call_started = std::time::Instant::now();
    client.send(status_call(2));
    let response = client.response(2);
    assert!(
        call_started.elapsed() < Duration::from_millis(500),
        "deadline response must not await a stalled blocking worker"
    );
    assert_eq!(response.pointer("/result/isError"), Some(&json!(true)));
    assert_tool_error_code(&response, "deadline_exceeded");
    assert!(started_marker.exists());
    for id in 3..6 {
        client.send(status_call(id));
        assert_tool_error_code(&client.response(id), "deadline_exceeded");
    }
    assert_eq!(
        file_line_count(&started_marker),
        1,
        "post-timeout retries must not spawn more blocking initialization workers"
    );

    client.close_stdin();
    assert!(
        client.wait_for_exit(Duration::from_secs(1)),
        "runtime shutdown must not wait for a stalled blocking worker"
    );
    assert!(
        game_data.storage_root.is_dir(),
        "the test uses the language-server cache artifact"
    );
}

#[test]
fn ready_game_data_operations_use_their_own_five_second_deadline() {
    let fixture = TempFixture::new("mcp_ready_game_data_deadline");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(scripts_root.join("Deadline.c"), "class Deadline {}")
        .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned_with_env(
        &game_data.arguments,
        &[
            ("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DELAY_MS", "5000"),
            ("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DEADLINE_MS", "50"),
        ],
    );
    client.initialize(1);
    client.send(status_call(2));
    assert_eq!(
        client
            .response(2)
            .pointer("/result/structuredContent/available"),
        Some(&json!(true)),
        "status prepares the catalogue before the ready-operation deadline is exercised",
    );

    let call_started = std::time::Instant::now();
    client.send(json!({
        "jsonrpc":"2.0",
        "id":3,
        "method":"tools/call",
        "params":{"name":"search_game_data_symbols","arguments":{"query":"Deadline"}},
    }));
    let response = client.response(3);
    assert!(
        call_started.elapsed() < Duration::from_millis(500),
        "a ready Game Data operation must not inherit the cold initialization deadline",
    );
    assert_tool_error_code(&response, "deadline_exceeded");
}

#[test]
fn source_relationship_cancellation_and_deadline_leave_the_runtime_responsive() {
    let fixture = TempFixture::new("mcp_source_relationship_bounds");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    let mut source = "class Deadline {}\n".to_string();
    for index in 0..5_000 {
        source.push_str(&format!("class DeadlineFanout{index} : Deadline {{}}\n"));
    }
    fs::write(scripts_root.join("Deadline.c"), source).expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);

    let mut discovery = McpClient::spawn_owned(&game_data.arguments);
    discovery.initialize(1);
    discovery.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_game_data_symbols","arguments":{"query":"Deadline","kinds":["class"]}}}));
    let anchor = discovery
        .response(2)
        .pointer("/result/structuredContent/results/0/symbolRef")
        .cloned()
        .expect("exact relationship anchor");
    discovery.close_stdin();
    assert!(discovery.wait_for_exit(Duration::from_secs(3)));

    let environment = &[
        ("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DELAY_MS", "5000"),
        ("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DEADLINE_MS", "1"),
        ("REFORGER_MCP_TEST_INITIALIZATION_DEADLINE_MS", "1"),
    ];
    let mut cancelled = McpClient::spawn_owned_with_env(&game_data.arguments, environment);
    cancelled.initialize(1);
    cancelled.send(status_call(2));
    let _ = cancelled.response(2);
    cancelled.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":anchor,"includeWorkspace":false,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["derivedType"],"depth":"all","limit":100}}}));
    cancelled.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"cancel broad source relationship query"}}));
    cancelled.send(json!({"jsonrpc":"2.0","id":4,"method":"ping","params":{}}));
    assert!(cancelled
        .responses_until(4)
        .iter()
        .all(|response| response.get("id") != Some(&json!(3))));
    cancelled.close_stdin();
    assert!(cancelled.wait_for_exit(Duration::from_secs(3)));

    let mut deadline = McpClient::spawn_owned_with_env(&game_data.arguments, environment);
    deadline.initialize(1);
    let started = std::time::Instant::now();
    deadline.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_source_symbol_relationships","arguments":{"anchorSource":"gameData","symbolRef":anchor,"includeWorkspace":false,"addonGuids":[BASE_GAME_ADDON_GUID],"relationshipKinds":["derivedType"],"depth":"all","limit":100}}}));
    let response = deadline.response(3);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_tool_error_code(&response, "deadline_exceeded");
    deadline.send(json!({"jsonrpc":"2.0","id":4,"method":"ping","params":{}}));
    assert_eq!(deadline.response(4).get("result"), Some(&json!({})));
}

#[test]
fn first_game_data_search_uses_the_cold_initialization_deadline() {
    let fixture = TempFixture::new("mcp_first_game_data_search_deadline");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(scripts_root.join("Deadline.c"), "class Deadline {}")
        .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let mut client = McpClient::spawn_owned_with_env(
        &game_data.arguments,
        &[
            ("REFORGER_MCP_TEST_INITIALIZATION_DELAY_MS", "5000"),
            ("REFORGER_MCP_TEST_INITIALIZATION_DEADLINE_MS", "50"),
            ("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DEADLINE_MS", "50"),
        ],
    );
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"tools/call",
        "params":{"name":"search_game_data_symbols","arguments":{"query":"Deadline"}},
    }));
    let response = client.response(2);
    assert_tool_error_code(&response, "deadline_exceeded");
}

#[test]
fn request_admission_bounds_in_flight_tool_calls() {
    let fixture = TempFixture::new("mcp_admission");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(scripts_root.join("Admission.c"), "class Admission {}")
        .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let admission_marker = fixture.path().join("admitted-requests");
    let mut client = McpClient::spawn_owned_with_env(
        &game_data.arguments,
        &[
            ("REFORGER_MCP_TEST_INITIALIZATION_DELAY_MS", "750"),
            (
                "REFORGER_MCP_TEST_ADMISSION_MARKER",
                admission_marker.to_str().expect("utf-8 marker path"),
            ),
        ],
    );
    client.initialize(1);
    for id in 10..19 {
        client.send(status_call(id));
    }

    wait_for_lines(&admission_marker, 8, Duration::from_secs(2));
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        file_line_count(&admission_marker),
        8,
        "the ninth call must wait outside the eight-request admission bound"
    );

    let responses = client.take_responses(9);
    assert_eq!(
        responses
            .iter()
            .filter(|response| response.pointer("/result/isError") == Some(&json!(false)))
            .count(),
        9
    );
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn timed_out_tool_families_bound_actual_workers_after_the_join_grace() {
    assert_noncooperative_tool_admission(false);
}

#[test]
fn cancelled_tool_families_bound_actual_workers_after_the_join_grace() {
    assert_noncooperative_tool_admission(true);
}

fn assert_noncooperative_tool_admission(cancel: bool) {
    let cases = [
        ("official_wiki_status", json!({})),
        ("search_official_wiki", json!({"query":"test"})),
        (
            "read_official_wiki",
            json!({"corpusRevision":"test", "relativePath":"test.md"}),
        ),
        ("game_data_status", json!({})),
        ("search_game_data_symbols", json!({"query":"test"})),
        ("search_game_data_resources", json!({"query":"test"})),
        ("search_game_data_text", json!({"query":"test"})),
        ("inspect_game_data_symbol", json!({"symbolRef":"test"})),
        (
            "read_game_data_source",
            json!({"catalogueRevision":"test", "addonGuid":"0000000000000000", "relativePath":"test.c"}),
        ),
        ("search_workspace_symbols", json!({"query":"test"})),
        ("search_workspace_text", json!({"query":"test"})),
        ("inspect_workspace_symbol", json!({"symbolRef":"test"})),
        (
            "read_workspace_source",
            json!({"catalogueRevision":"test", "relativePath":"test.c"}),
        ),
        ("list_workspace_symbol_members", json!({"symbolRef":"test"})),
        (
            "query_workspace_symbol_relationships",
            json!({"symbolRef":"test", "relationshipKinds":["derivedTypes"]}),
        ),
    ];
    for (tool, arguments) in cases {
        let fixture = TempFixture::new(&format!(
            "{tool}_{}",
            if cancel {
                "cancelled_workers"
            } else {
                "timed_out_workers"
            }
        ));
        let marker = fixture.path().join("admitted");
        let deadline = if cancel { "5000" } else { "50" };
        let mut client = McpClient::spawn_with_env(
            &["mcp"],
            &[
                ("REFORGER_MCP_TEST_WORKER_NONCOOPERATIVE_DELAY_MS", "600"),
                ("REFORGER_MCP_TEST_INITIALIZATION_DEADLINE_MS", deadline),
                (
                    "REFORGER_MCP_TEST_GAME_DATA_OPERATION_DEADLINE_MS",
                    deadline,
                ),
                ("REFORGER_MCP_TEST_TEXT_SEARCH_DEADLINE_MS", deadline),
                ("REFORGER_MCP_TEST_OFFICIAL_WIKI_DEADLINE_MS", deadline),
                (
                    "REFORGER_MCP_TEST_ADMISSION_MARKER",
                    marker.to_str().unwrap(),
                ),
            ],
        );
        client.initialize(1);
        for id in 10..18 {
            client.send(json!({"jsonrpc":"2.0", "id":id, "method":"tools/call",
                "params":{"name":tool,"arguments":arguments}}));
        }
        wait_for_lines(&marker, 8, Duration::from_secs(2));
        client.send(json!({"jsonrpc":"2.0", "id":18, "method":"tools/call",
            "params":{"name":tool,"arguments":arguments}}));
        if cancel {
            for id in 10..18 {
                client.send(json!({"jsonrpc":"2.0", "method":"notifications/cancelled",
                    "params":{"requestId":id,"reason":"admission stress test"}}));
            }
        }
        if cancel {
            // Wire cancellation suppresses responses entirely. Give the async
            // requests time to exit while the blocking jobs remain alive.
            thread::sleep(Duration::from_millis(200));
        } else {
            let first_responses = client.take_responses(8);
            assert!(
                first_responses.iter().all(|response| {
                    response.pointer("/result/structuredContent/code")
                        == Some(&json!("deadline_exceeded"))
                }),
                "{tool}: timed-out work must not publish success: {first_responses:?}"
            );
        }
        // Responses have returned, but noncooperative jobs are still sleeping.
        assert_eq!(
            file_line_count(&marker),
            8,
            "{tool}: ninth worker admitted before a worker exited"
        );
        client.send(json!({"jsonrpc":"2.0", "id":99, "method":"ping"}));
        assert!(
            client.response(99).get("result").is_some(),
            "{tool}: ping must remain responsive"
        );
        let last = client.response(18);
        if !cancel {
            assert_tool_error_code(&last, "deadline_exceeded");
        }
        assert_eq!(
            file_line_count(&marker),
            9,
            "{tool}: admission must resume after a worker exits"
        );
        client.close_stdin();
        assert!(
            client.wait_for_exit(Duration::from_secs(3)),
            "{tool}: EOF must terminate the process"
        );
    }
}

#[test]
fn timed_out_research_workers_retain_admission_until_they_exit() {
    let fixture = TempFixture::new("mcp_research_admission");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(scripts_root.join("Admission.c"), "class Admission {}")
        .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let admission_marker = fixture.path().join("admitted-requests");
    let mut client = McpClient::spawn_owned_with_env(
        &game_data.arguments,
        &[
            ("REFORGER_MCP_TEST_WORKER_NONCOOPERATIVE_DELAY_MS", "500"),
            ("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DEADLINE_MS", "50"),
            (
                "REFORGER_MCP_TEST_ADMISSION_MARKER",
                admission_marker.to_str().expect("utf-8 marker path"),
            ),
        ],
    );
    client.initialize(1);
    client.call_status(2);
    fs::write(&admission_marker, "").expect("clear status admission marker");

    for id in 10..19 {
        client.send(json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{
                "name":"research_game_data",
                "arguments":{"query":"Admission"}
            }
        }));
    }

    wait_for_lines(&admission_marker, 8, Duration::from_secs(2));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        file_line_count(&admission_marker),
        8,
        "the ninth request must remain outside admission while timed-out workers still run"
    );
    let responses = client.take_responses(9);
    assert!(responses.iter().all(|response| {
        response.pointer("/result/structuredContent/code") == Some(&json!("deadline_exceeded"))
    }));
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn timed_out_unified_search_workers_retain_admission_until_they_exit() {
    let fixture = TempFixture::new("mcp_unified_search_admission");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    fs::write(scripts_root.join("Admission.c"), "class Admission {}")
        .expect("write game-data fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let admission_marker = fixture.path().join("admitted-requests");
    let mut arguments = game_data.arguments;
    arguments.extend(["--tool-profile".to_string(), "authoring".to_string()]);
    let mut client = McpClient::spawn_owned_with_env(
        &arguments,
        &[
            ("REFORGER_MCP_TEST_WORKER_NONCOOPERATIVE_DELAY_MS", "500"),
            ("REFORGER_MCP_TEST_GAME_DATA_OPERATION_DEADLINE_MS", "50"),
            (
                "REFORGER_MCP_TEST_ADMISSION_MARKER",
                admission_marker.to_str().expect("utf-8 marker path"),
            ),
        ],
    );
    client.initialize(1);
    client.call_status(2);
    fs::write(&admission_marker, "").expect("clear status admission marker");
    for id in 10..19 {
        client.send(json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{
                "name":"search_reforger",
                "arguments":{"query":"Admission","sources":["gameData"]}
            }
        }));
    }

    wait_for_lines(&admission_marker, 8, Duration::from_secs(2));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        file_line_count(&admission_marker),
        8,
        "the ninth unified search must wait while timed-out workers still hold admission"
    );
    let responses = client.take_responses(9);
    assert!(responses.iter().all(|response| {
        response.pointer("/result/structuredContent/code") == Some(&json!("deadline_exceeded"))
    }));
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn cancelled_intent_research_releases_admission_for_the_next_request() {
    let fixture = TempFixture::new("mcp_cancelled_intent_research_admission");
    let scripts_root = fixture.path().join("scripts");
    fs::create_dir_all(&scripts_root).expect("create scripts fixture");
    let methods = (0..20_000)
        .map(|index| {
            format!("void ConfigureRpc{index}(RplId ownerId) {{ RplRpc(); Rpc(ownerId); }}\n")
        })
        .collect::<String>();
    fs::write(
        scripts_root.join("CancellationExamples.c"),
        format!("class CancellationExamples {{\n{methods}}}"),
    )
    .expect("write large example-search fixture");
    let cache_path = fixture.path().join("cache").join("game-data-index.bin");
    let game_data = build_game_data_cache(&scripts_root, &cache_path);
    let admission_marker = fixture.path().join("admitted-requests");
    let mut client = McpClient::spawn_owned_with_env(
        &game_data.arguments,
        &[(
            "REFORGER_MCP_TEST_ADMISSION_MARKER",
            admission_marker.to_str().expect("utf-8 marker path"),
        )],
    );
    client.initialize(1);
    client.call_status(2);
    fs::write(&admission_marker, "").expect("clear status admission marker");

    for id in 10..18 {
        client.send(json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{"name":"research_game_data","arguments":{"query":"replication rpc authority"}}
        }));
    }
    wait_for_lines(&admission_marker, 8, Duration::from_secs(2));
    for id in 10..18 {
        client.send(json!({
            "jsonrpc":"2.0",
            "method":"notifications/cancelled",
            "params":{"requestId":id,"reason":"cancel cooperative intent research"}
        }));
    }
    client.send(json!({
        "jsonrpc":"2.0",
        "id":19,
        "method":"tools/call",
        "params":{"name":"research_game_data","arguments":{"query":"ConfigureRpc19999"}}
    }));
    wait_for_lines(&admission_marker, 9, Duration::from_secs(2));
    let responses = client.responses_until(19);
    assert_eq!(
        responses.last().and_then(|response| response.get("id")),
        Some(&json!(19))
    );
    assert_eq!(
        responses
            .last()
            .and_then(|response| response.pointer("/result/structuredContent/status")),
        Some(&json!("resolved"))
    );
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn panicking_initialization_worker_is_isolated_from_the_mcp_process() {
    let mut client = McpClient::spawn_with_env(&["mcp"], &[("REFORGER_MCP_TEST_PANIC_ONCE", "1")]);
    client.initialize(1);
    client.send(status_call(2));
    let failed = client.response(2);
    assert_eq!(failed.pointer("/error/code"), Some(&json!(-32603)));
    assert!(failed
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("initialization worker failed")));

    let recovered = client.call_status(3);
    assert_eq!(recovered.get("available"), Some(&json!(false)));
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "ping",
        "params": {}
    }));
    assert_eq!(client.response(4).get("result"), Some(&json!({})));
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn workbench_status_is_served_through_the_public_mcp_seam() {
    use std::io::Read;
    use std::net::TcpListener;

    let fixture = TempFixture::new("mcp_workbench_status");
    let profile = fixture
        .path()
        .join("Documents")
        .join("My Games")
        .join("ArmaReforgerWorkbench")
        .join("profile");
    let bridge = profile.join("scripts").join("reforger-script-tools");
    fs::create_dir_all(&profile).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake Workbench");
    let port = listener.local_addr().unwrap().port();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Workbench request");
        let mut version = [0_u8; 4];
        stream.read_exact(&mut version).unwrap();
        assert_eq!(i32::from_le_bytes(version), 1);
        assert_eq!(read_net_api_string(&mut stream), "ReforgerScriptTools");
        assert_eq!(read_net_api_string(&mut stream), "JsonRPC");
        let payload: Value =
            serde_json::from_str(&read_net_api_string(&mut stream)).expect("request JSON");
        assert_eq!(payload, json!({"APIFunc":"IsWorkbenchRunning"}));
        write_net_api_string(&mut stream, "Ok");
        write_net_api_string(&mut stream, r#"{"IsRunning":true,"ScriptsCompiled":true}"#);
    });
    let port_string = port.to_string();
    let mut client = McpClient::spawn(&[
        "mcp",
        "--workbench-port",
        &port_string,
        "--workbench-user-directory",
        fixture.path().to_str().unwrap(),
    ]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"tools/call",
        "params":{"name":"workbench_status","arguments":{}}
    }));
    let response = client.response(2);
    assert_eq!(
        response.pointer("/result/structuredContent/isRunning"),
        Some(&json!(true)),
        "{response}"
    );
    assert_eq!(
        response.pointer("/result/structuredContent/scriptsCompiled"),
        Some(&json!(true))
    );
    assert!(
        !bridge.exists(),
        "status must not create the managed installation"
    );
    peer.join().unwrap();
}

#[test]
fn workbench_reload_rejects_the_obsolete_compile_target_input() {
    let fixture = TempFixture::new("mcp_workbench_reload_input");
    let mut client = McpClient::spawn(&[
        "mcp",
        "--workbench-user-directory",
        fixture.path().to_str().unwrap(),
    ]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"tools/call",
        "params":{"name":"workbench_reload","arguments":{"target":"scripts"}}
    }));
    let response = client.response(2);
    assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn workbench_install_bridge_uses_the_public_mcp_seam_and_preserves_unknown_files() {
    use std::io::Read;
    use std::net::TcpListener;

    let fixture = TempFixture::new("mcp_workbench_install");
    let bridge = fixture
        .path()
        .join("Documents")
        .join("My Games")
        .join("ArmaReforgerWorkbench")
        .join("profile")
        .join("scripts")
        .join("WorkbenchGame")
        .join("reforger-script-tools");
    fs::create_dir_all(&bridge).unwrap();
    fs::write(bridge.join("user-script.c"), "preserve").unwrap();
    fs::write(
        bridge.join("reforger-script-tools.manifest.json"),
        r#"{"bridgeVersion":"0.9.0","protocolVersion":1,"files":[]}"#,
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake Workbench");
    let port = listener.local_addr().unwrap().port();
    let peer = thread::spawn(move || {
        for expected in [
            json!({"APIFunc":"IsWorkbenchRunning"}),
            json!({"APIFunc":"ValidateScripts","Configuration":"WORKBENCH"}),
        ] {
            let (mut stream, _) = listener.accept().expect("accept Workbench request");
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            assert_eq!(i32::from_le_bytes(version), 1);
            assert_eq!(read_net_api_string(&mut stream), "ReforgerScriptTools");
            assert_eq!(read_net_api_string(&mut stream), "JsonRPC");
            let payload: Value =
                serde_json::from_str(&read_net_api_string(&mut stream)).expect("request JSON");
            assert_eq!(payload, expected);
            let response = match expected["APIFunc"].as_str().unwrap() {
                "IsWorkbenchRunning" => r#"{"IsRunning":true,"ScriptsCompiled":true}"#,
                "ValidateScripts" => r#"{"Success":true,"Errors":[],"Warnings":[]}"#,
                _ => unreachable!(),
            };
            write_net_api_string(&mut stream, "Ok");
            write_net_api_string(&mut stream, response);
        }
    });
    let port_string = port.to_string();
    let mut client = McpClient::spawn(&[
        "mcp",
        "--workbench-port",
        &port_string,
        "--workbench-user-directory",
        fixture.path().to_str().unwrap(),
    ]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"tools/call",
        "params":{"name":"workbench_install_bridge","arguments":{}}
    }));

    let response = client.response(2);

    assert_eq!(
        response.pointer("/result/structuredContent/activated"),
        Some(&json!(false))
    );
    assert!(bridge.join("RST_WorkbenchCapabilities.c").is_file());
    assert!(bridge.join("RST_WorkbenchState.c").is_file());
    assert!(!bridge.join("RST_WorkbenchReload.c").exists());
    assert!(bridge.join("reforger-script-tools.manifest.json").is_file());
    assert_eq!(
        fs::read_to_string(bridge.join("user-script.c")).unwrap(),
        "preserve"
    );
    peer.join().unwrap();
}

#[test]
fn workbench_failed_activation_keeps_the_managed_installation_for_diagnosis() {
    use std::io::Read;
    use std::net::TcpListener;

    let fixture = TempFixture::new("mcp_workbench_install_failed_activation");
    let profile = fixture
        .path()
        .join("Documents")
        .join("My Games")
        .join("ArmaReforgerWorkbench")
        .join("profile");
    let bridge = profile
        .join("scripts")
        .join("WorkbenchGame")
        .join("reforger-script-tools");
    fs::create_dir_all(&bridge).unwrap();
    fs::write(
        bridge.join("reforger-script-tools.manifest.json"),
        r#"{"bridgeVersion":"0.9.0","protocolVersion":1,"files":[]}"#,
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake Workbench");
    let port = listener.local_addr().unwrap().port();
    let peer = thread::spawn(move || {
        for expected in [
            json!({"APIFunc":"IsWorkbenchRunning"}),
            json!({"APIFunc":"ValidateScripts","Configuration":"WORKBENCH"}),
        ] {
            let (mut stream, _) = listener.accept().expect("accept Workbench request");
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            assert_eq!(i32::from_le_bytes(version), 1);
            assert_eq!(read_net_api_string(&mut stream), "ReforgerScriptTools");
            assert_eq!(read_net_api_string(&mut stream), "JsonRPC");
            let payload: Value =
                serde_json::from_str(&read_net_api_string(&mut stream)).expect("request JSON");
            assert_eq!(payload, expected);
            let response = match expected["APIFunc"].as_str().unwrap() {
                "IsWorkbenchRunning" => r#"{"IsRunning":true,"ScriptsCompiled":true}"#,
                "ValidateScripts" => {
                    r#"{"Success":false,"Errors":[{"error":"broken","file":"RST_WorkbenchState.c","line":1}],"Warnings":[]}"#
                }
                _ => unreachable!(),
            };
            write_net_api_string(&mut stream, "Ok");
            write_net_api_string(&mut stream, response);
        }
    });
    let port_string = port.to_string();
    let mut client = McpClient::spawn(&[
        "mcp",
        "--workbench-port",
        &port_string,
        "--workbench-user-directory",
        fixture.path().to_str().unwrap(),
    ]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"tools/call",
        "params":{"name":"workbench_install_bridge","arguments":{}}
    }));
    let response = client.response(2);
    assert_eq!(
        response.pointer("/result/structuredContent/activated"),
        Some(&json!(false))
    );
    assert_eq!(
        response.pointer("/result/structuredContent/installedVersion"),
        Some(&json!(WORKBENCH_BRIDGE_VERSION))
    );
    assert!(bridge.join("RST_WorkbenchCapabilities.c").is_file());
    assert!(bridge.join("RST_WorkbenchState.c").is_file());
    assert!(!bridge.join("RST_WorkbenchReload.c").exists());
    assert!(bridge.join("reforger-script-tools.manifest.json").is_file());
    peer.join().unwrap();
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn workbench_install_bridge_requires_extension_consent_before_first_install() {
    use std::io::Read;
    use std::net::TcpListener;

    let fixture = TempFixture::new("mcp_workbench_install_consent_required");
    let profile = fixture
        .path()
        .join("Documents")
        .join("My Games")
        .join("ArmaReforgerWorkbench")
        .join("profile");
    let bridge = profile.join("scripts").join("reforger-script-tools");
    fs::create_dir_all(&profile).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake Workbench");
    let port = listener.local_addr().unwrap().port();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Workbench request");
        let mut version = [0_u8; 4];
        stream.read_exact(&mut version).unwrap();
        assert_eq!(i32::from_le_bytes(version), 1);
        assert_eq!(read_net_api_string(&mut stream), "ReforgerScriptTools");
        assert_eq!(read_net_api_string(&mut stream), "JsonRPC");
        let payload: Value =
            serde_json::from_str(&read_net_api_string(&mut stream)).expect("request JSON");
        assert_eq!(payload, json!({"APIFunc":"IsWorkbenchRunning"}));
        write_net_api_string(&mut stream, "Ok");
        write_net_api_string(&mut stream, r#"{"IsRunning":true,"ScriptsCompiled":true}"#);
    });
    let port_string = port.to_string();
    let mut client = McpClient::spawn(&[
        "mcp",
        "--workbench-port",
        &port_string,
        "--workbench-user-directory",
        fixture.path().to_str().unwrap(),
    ]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"tools/call",
        "params":{"name":"workbench_install_bridge","arguments":{}}
    }));

    let response = client.response(2);

    assert_eq!(
        response.pointer("/result/structuredContent/code"),
        Some(&json!("workbench_installation_consent_required"))
    );
    assert_eq!(
        response.pointer("/result/structuredContent/retryable"),
        Some(&json!(false))
    );
    assert!(!bridge.exists());
    peer.join().unwrap();
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

#[test]
fn workbench_install_failure_returns_a_correlated_support_log_reference() {
    use std::io::Read;
    use std::net::TcpListener;

    let fixture = TempFixture::new("mcp_workbench_install_missing_profile");
    let profile = fixture
        .path()
        .join("Documents")
        .join("My Games")
        .join("ArmaReforgerWorkbench")
        .join("profile");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake Workbench");
    let port = listener.local_addr().unwrap().port();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Workbench request");
        let mut version = [0_u8; 4];
        stream.read_exact(&mut version).unwrap();
        assert_eq!(i32::from_le_bytes(version), 1);
        assert_eq!(read_net_api_string(&mut stream), "ReforgerScriptTools");
        assert_eq!(read_net_api_string(&mut stream), "JsonRPC");
        let payload: Value =
            serde_json::from_str(&read_net_api_string(&mut stream)).expect("request JSON");
        assert_eq!(payload, json!({"APIFunc":"IsWorkbenchRunning"}));
        write_net_api_string(&mut stream, "Ok");
        write_net_api_string(&mut stream, r#"{"IsRunning":true,"ScriptsCompiled":true}"#);
    });
    let port_string = port.to_string();
    let mut client = McpClient::spawn(&[
        "mcp",
        "--workbench-port",
        &port_string,
        "--workbench-user-directory",
        fixture.path().to_str().unwrap(),
    ]);
    client.initialize(1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"tools/call",
        "params":{"name":"workbench_install_bridge","arguments":{}}
    }));
    let failed = client.response(2);
    assert_eq!(
        failed.pointer("/result/structuredContent/code"),
        Some(&json!("workbench_unavailable"))
    );
    assert_eq!(
        failed.pointer("/result/structuredContent/phase"),
        Some(&json!("install"))
    );
    let reference = failed
        .pointer("/result/structuredContent/logReference")
        .and_then(Value::as_str)
        .expect("correlated log reference")
        .to_string();
    assert!(reference.starts_with("wb-"));
    assert!(
        !profile.exists(),
        "the Workbench profile must not be created"
    );

    client.send(json!({
        "jsonrpc":"2.0",
        "id":3,
        "method":"tools/call",
        "params":{"name":"workbench_read_logs","arguments":{"source":"integration","lineCount":20}}
    }));
    let logs = client.response(3);
    let record = logs
        .pointer("/result/structuredContent/lines")
        .and_then(Value::as_array)
        .and_then(|lines| {
            lines.iter().find_map(|line| {
                let record: Value = serde_json::from_str(line.as_str()?).ok()?;
                (record.get("reference") == Some(&json!(reference))).then_some(record)
            })
        })
        .expect("matching support log record");
    assert_eq!(record.get("operation"), Some(&json!("install")));
    assert_eq!(record.get("outcome"), Some(&json!("profile-missing")));
    assert_eq!(record.pointer("/details/profileFound"), Some(&json!(false)));
    assert_eq!(
        record.pointer("/details/managedDirectoryCreated"),
        Some(&json!(false))
    );
    peer.join().unwrap();
    client.close_stdin();
    assert!(client.wait_for_exit(Duration::from_secs(3)));
}

fn read_net_api_string(stream: &mut impl std::io::Read) -> String {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut bytes = vec![0_u8; i32::from_le_bytes(length) as usize];
    stream.read_exact(&mut bytes).unwrap();
    String::from_utf8(bytes).unwrap()
}

fn write_net_api_string(stream: &mut impl std::io::Write, value: &str) {
    stream
        .write_all(&(value.len() as i32).to_le_bytes())
        .unwrap();
    stream.write_all(value.as_bytes()).unwrap();
}

struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
}

impl McpClient {
    fn spawn(args: &[&str]) -> Self {
        Self::spawn_with_env(args, &[])
    }

    fn spawn_program(program: &Path, args: &[&str]) -> Self {
        Self::spawn_program_with_env(program, args, &[])
    }

    fn spawn_owned(args: &[String]) -> Self {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        Self::spawn(&args)
    }

    fn spawn_with_env(args: &[&str], environment: &[(&str, &str)]) -> Self {
        Self::spawn_program_with_env(
            Path::new(env!("CARGO_BIN_EXE_reforger_language_server")),
            args,
            environment,
        )
    }

    fn spawn_owned_with_env(args: &[String], environment: &[(&str, &str)]) -> Self {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        Self::spawn_with_env(&args, environment)
    }

    fn spawn_program_with_env(program: &Path, args: &[&str], environment: &[(&str, &str)]) -> Self {
        let mut command = Command::new(program);
        command.args(args);
        if args.first() == Some(&"mcp") && !args.contains(&"--tool-profile") {
            command.args(["--tool-profile", "all"]);
        }
        for (name, value) in environment {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn MCP server");
        let stdin = child.stdin.take().expect("MCP stdin");
        let stdout = child.stdout.take().expect("MCP stdout");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            stdout: receiver,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("open MCP stdin");
        serde_json::to_writer(&mut *stdin, &message).expect("write MCP JSON");
        stdin.write_all(b"\n").expect("write MCP newline");
        stdin.flush().expect("flush MCP message");
    }

    fn initialize(&mut self, id: u64) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "reforger-script-tools-test",
                    "version": "1.0.0"
                }
            }
        }));
        let response = self.response(id);
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        response
    }

    fn call_status(&mut self, id: u64) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "game_data_status",
                "arguments": {}
            }
        }));
        self.response(id)
            .pointer("/result/structuredContent")
            .cloned()
            .expect("structured Game Data status")
    }

    fn response(&self, id: u64) -> Value {
        let line = self
            .stdout
            .recv_timeout(RESPONSE_TIMEOUT)
            .unwrap_or_else(|error| panic!("timed out waiting for MCP response {id}: {error}"));
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("stdout was not an MCP JSON message: {error}: {line}"));
        assert_eq!(value.get("id"), Some(&json!(id)));
        value
    }

    fn responses_until(&self, id: u64) -> Vec<Value> {
        let started = std::time::Instant::now();
        let mut messages = Vec::new();
        while started.elapsed() < RESPONSE_TIMEOUT {
            let remaining = RESPONSE_TIMEOUT.saturating_sub(started.elapsed());
            let line = self
                .stdout
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("timed out waiting for MCP response {id}: {error}"));
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("stdout was not MCP JSON: {error}: {line}"));
            let complete = value.get("id") == Some(&json!(id));
            messages.push(value);
            if complete {
                return messages;
            }
        }
        panic!("timed out waiting for MCP response {id}");
    }

    fn take_responses(&self, count: usize) -> Vec<Value> {
        (0..count)
            .map(|_| {
                let line = self
                    .stdout
                    .recv_timeout(RESPONSE_TIMEOUT)
                    .expect("timed out waiting for MCP response");
                serde_json::from_str(&line).expect("stdout was not MCP JSON")
            })
            .collect()
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < timeout {
            if self.child.try_wait().expect("poll MCP process").is_some() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

fn status_call(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "game_data_status",
            "arguments": {}
        }
    })
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

fn wait_for_lines(path: &Path, count: usize, timeout: Duration) {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if file_line_count(path) >= count {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {count} lines in {}", path.display());
}

fn file_line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct TempFixture {
    path: PathBuf,
}

impl TempFixture {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "reforger-script-tools-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temporary fixture");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
