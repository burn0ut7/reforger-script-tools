use reforger_language_server::game_data_catalogue::{
    GameDataCatalogueConfig, GameDataExternalIndexMode,
};
use reforger_language_server::lsp::{
    run_stdio as run_lsp_stdio, BracketColoringMode, ExternalIndexMode, LspServerOptions,
};
use reforger_language_server::mcp::{
    render_api_reference, render_api_reference_bundle, run_stdio as run_mcp_stdio, McpServerOptions,
};
use reforger_language_server::workbench::{
    WorkbenchControllerOptions, WorkbenchFailureCode, WorkbenchInstallAuthorization,
};
use std::env;
use std::path::PathBuf;
use std::time::Instant;

enum ServerMode {
    Lsp(LspServerOptions),
    Mcp(McpServerOptions),
    McpApi,
    McpApiBundle,
    WorkbenchApi(WorkbenchApiCommand, WorkbenchControllerOptions),
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkbenchApiCommand {
    Status,
    Validate,
    LoadedAddonGraph,
    ReadLogs,
    IntegrationStatus,
    BootstrapIntegration,
    MaintainIntegration,
    ProcessStatus,
    LaunchDefault,
    InstallBridge,
    ReloadBridge,
}

fn main() {
    let mode = match parse_args() {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    // Every host-dependent operation resolves through the one host this
    // process was started for, so it is settled before any mode runs.
    reforger_language_server::host_platform::configure_workbench_host(workbench_wine_prefix(&mode));

    let result = match mode {
        ServerMode::Lsp(options) => run_lsp_stdio(options),
        ServerMode::Mcp(options) => run_mcp_stdio(options),
        ServerMode::McpApi => {
            print!("{}", render_api_reference());
            Ok(())
        }
        ServerMode::McpApiBundle => {
            print!("{}", render_api_reference_bundle());
            Ok(())
        }
        ServerMode::WorkbenchApi(command, options) => run_workbench_api(command, options),
        ServerMode::Help => {
            print_help();
            Ok(())
        }
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

/// The Wine prefix the caller pointed this process at, in whichever mode it
/// was started.
fn workbench_wine_prefix(mode: &ServerMode) -> Option<&std::path::Path> {
    match mode {
        ServerMode::Lsp(options) => options.workbench_wine_prefix.as_deref(),
        ServerMode::Mcp(options) => options.workbench.wine_prefix.as_deref(),
        ServerMode::WorkbenchApi(_, options) => options.wine_prefix.as_deref(),
        ServerMode::McpApi | ServerMode::McpApiBundle | ServerMode::Help => None,
    }
}

fn parse_args() -> Result<ServerMode, String> {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from(args: impl Iterator<Item = String>) -> Result<ServerMode, String> {
    let mut args = args.peekable();
    match args.peek().map(String::as_str) {
        Some("mcp") => {
            args.next();
            parse_mcp_args(args).map(ServerMode::Mcp)
        }
        Some("mcp-api") => {
            args.next();
            if let Some(argument) = args.next() {
                Err(format!("unexpected argument for mcp-api mode: {argument}"))
            } else {
                Ok(ServerMode::McpApi)
            }
        }
        Some("mcp-api-bundle") => {
            args.next();
            if let Some(argument) = args.next() {
                Err(format!(
                    "unexpected argument for mcp-api-bundle mode: {argument}"
                ))
            } else {
                Ok(ServerMode::McpApiBundle)
            }
        }
        Some("workbench-api") => {
            args.next();
            parse_workbench_api_args(args)
        }
        Some("--help" | "-h") => {
            args.next();
            if let Some(argument) = args.next() {
                Err(format!("unexpected argument after help flag: {argument}"))
            } else {
                Ok(ServerMode::Help)
            }
        }
        Some(argument) if !argument.starts_with('-') => Err(format!("unknown mode '{argument}'")),
        _ => parse_lsp_args(args).map(ServerMode::Lsp),
    }
}

fn parse_workbench_api_args(mut args: impl Iterator<Item = String>) -> Result<ServerMode, String> {
    let command = match args.next().as_deref() {
        Some("status") => WorkbenchApiCommand::Status,
        Some("validate") => WorkbenchApiCommand::Validate,
        Some("loaded-addon-graph") => WorkbenchApiCommand::LoadedAddonGraph,
        Some("read-logs") => WorkbenchApiCommand::ReadLogs,
        Some("integration-status") => WorkbenchApiCommand::IntegrationStatus,
        Some("bootstrap-integration") => WorkbenchApiCommand::BootstrapIntegration,
        Some("maintain-integration") => WorkbenchApiCommand::MaintainIntegration,
        Some("process-status") => WorkbenchApiCommand::ProcessStatus,
        Some("launch-default") => WorkbenchApiCommand::LaunchDefault,
        Some("install-bridge") => WorkbenchApiCommand::InstallBridge,
        Some("reload-bridge") => WorkbenchApiCommand::ReloadBridge,
        Some(value) => return Err(format!("unknown workbench-api command '{value}'")),
        None => return Err("missing workbench-api command".to_string()),
    };
    let mut options = WorkbenchControllerOptions::default();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--host" => options.gateway.host = string_value(&mut args, "--host")?,
            "--port" => {
                let value = string_value(&mut args, "--port")?;
                options.gateway.port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid value for --port: {value}"))?;
            }
            "--deadline-ms" => {
                let value = string_value(&mut args, "--deadline-ms")?;
                let deadline = std::time::Duration::from_millis(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid value for --deadline-ms: {value}"))?,
                );
                options.gateway.status_deadline = deadline;
                options.gateway.validation_deadline = deadline;
            }
            "--workbench-wine-prefix" => {
                options.wine_prefix = Some(path_value(&mut args, "--workbench-wine-prefix")?)
            }
            _ => return Err(format!("unknown workbench-api argument '{argument}'")),
        }
    }
    Ok(ServerMode::WorkbenchApi(command, options))
}

fn run_workbench_api(
    command: WorkbenchApiCommand,
    options: WorkbenchControllerOptions,
) -> Result<(), String> {
    let started = Instant::now();
    let controller = reforger_language_server::workbench::WorkbenchController::new(options);
    let controller_setup_ms = started.elapsed().as_millis();
    let result = match command {
        WorkbenchApiCommand::Status => {
            controller
                .native_status_with_timing()
                .and_then(|(value, timing)| {
                    serde_json::to_value(value)
                        .map(|value| (value, Some(timing)))
                        .map_err(|_| unreachable!())
                })
        }
        WorkbenchApiCommand::Validate => controller.native_validate_scripts().and_then(|value| {
            serde_json::to_value(value)
                .map(|value| (value, None))
                .map_err(|_| unreachable!())
        }),
        WorkbenchApiCommand::LoadedAddonGraph => controller
            .loaded_addon_graph_with_timing()
            .and_then(|(value, timing)| {
                serde_json::to_value(value)
                    .map(|value| (value, Some(timing)))
                    .map_err(|_| unreachable!())
            }),
        WorkbenchApiCommand::ReadLogs => controller
            .read_logs("workbench", "tail", Some(500))
            .and_then(|value| {
                serde_json::to_value(value)
                    .map(|value| (value, None))
                    .map_err(|_| unreachable!())
            }),
        WorkbenchApiCommand::IntegrationStatus => Ok((
            serde_json::to_value(controller.overview()).unwrap_or_else(|_| unreachable!()),
            None,
        )),
        WorkbenchApiCommand::BootstrapIntegration => {
            controller.bootstrap_integration().and_then(|value| {
                serde_json::to_value(value)
                    .map(|value| (value, None))
                    .map_err(|_| unreachable!())
            })
        }
        WorkbenchApiCommand::MaintainIntegration => {
            controller.maintain_integration().and_then(|value| {
                serde_json::to_value(value)
                    .map(|value| (value, None))
                    .map_err(|_| unreachable!())
            })
        }
        WorkbenchApiCommand::ProcessStatus => serde_json::to_value(controller.process_status())
            .map(|value| (value, None))
            .map_err(|_| unreachable!()),
        WorkbenchApiCommand::LaunchDefault => {
            controller.launch_default_project().and_then(|value| {
                serde_json::to_value(value)
                    .map(|value| (value, None))
                    .map_err(|_| unreachable!())
            })
        }
        WorkbenchApiCommand::InstallBridge => controller
            .install_bridge(WorkbenchInstallAuthorization::UserApprovedFirstInstall)
            .and_then(|value| {
                serde_json::to_value(value)
                    .map(|value| (value, None))
                    .map_err(|_| unreachable!())
            }),
        WorkbenchApiCommand::ReloadBridge => controller.activate_scripts().and_then(|value| {
            serde_json::to_value(value)
                .map(|value| (value, None))
                .map_err(|_| unreachable!())
        }),
    };
    match result {
        Ok((value, request_timing)) => {
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "value": value,
                    "timing": {
                        "controllerSetupMs": controller_setup_ms,
                        "commandMs": started.elapsed().as_millis(),
                        "request": request_timing,
                    },
                })
            );
        }
        Err(failure) => {
            let category = match failure.code {
                WorkbenchFailureCode::ConsentRequired => "consent-required",
                WorkbenchFailureCode::Unavailable => "unavailable",
                WorkbenchFailureCode::Timeout => "timeout",
                WorkbenchFailureCode::Protocol => "protocol",
                WorkbenchFailureCode::WorkbenchError => "workbench-error",
                WorkbenchFailureCode::CaptureUnavailable
                | WorkbenchFailureCode::CaptureInvalidRegion
                | WorkbenchFailureCode::CaptureTooLarge => "capture",
            };
            println!(
                "{}",
                serde_json::json!({"ok": false, "failure": {"category": category}})
            );
        }
    }
    Ok(())
}

fn parse_lsp_args(mut args: impl Iterator<Item = String>) -> Result<LspServerOptions, String> {
    let mut options = LspServerOptions::default();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            // vscode-languageclient appends this marker for TransportKind.stdio.
            // The process already owns stdio; accepting the known marker keeps
            // the legacy LSP launch contract strict without changing transport.
            "--stdio" => {}
            "--log" => options.log_path = Some(path_value(&mut args, "--log")?),
            "--diagnostic-log" => {
                options.diagnostic_log_path = Some(path_value(&mut args, "--diagnostic-log")?)
            }
            "--addon-source-inventory" => {
                options.addon_source_inventory =
                    Some(path_value(&mut args, "--addon-source-inventory")?)
            }
            "--addon-index-storage" => {
                options.addon_index_storage = Some(path_value(&mut args, "--addon-index-storage")?)
            }
            "--external-index-mode" => {
                let value = string_value(&mut args, "--external-index-mode")?;
                options.external_index_mode = match value.as_str() {
                    "all" => ExternalIndexMode::All,
                    "loaded" => ExternalIndexMode::Loaded,
                    "none" => ExternalIndexMode::None,
                    _ => return Err(format!("invalid value for --external-index-mode: {value}")),
                };
            }
            "--workspace-scripts" => {
                options
                    .workspace_scripts
                    .push(path_value(&mut args, "--workspace-scripts")?);
            }
            "--dependency-project" => {
                options
                    .dependency_project_files
                    .push(path_value(&mut args, "--dependency-project")?);
            }
            "--workbench-wine-prefix" => {
                options.workbench_wine_prefix =
                    Some(path_value(&mut args, "--workbench-wine-prefix")?)
            }
            "--bracket-coloring" => {
                let value = string_value(&mut args, "--bracket-coloring")?;
                options.bracket_coloring = match value.as_str() {
                    "semantic" => BracketColoringMode::Semantic,
                    "punctuation" => BracketColoringMode::Punctuation,
                    "vscode" => BracketColoringMode::VsCode,
                    _ => {
                        return Err(format!("invalid value for --bracket-coloring: {value}"));
                    }
                };
            }
            _ => return Err(format!("unknown LSP argument '{argument}'")),
        }
    }

    Ok(options)
}

fn parse_mcp_args(mut args: impl Iterator<Item = String>) -> Result<McpServerOptions, String> {
    let mut game_data = GameDataCatalogueConfig::default();
    let mut official_wiki_root = None;
    let mut workbench = WorkbenchControllerOptions::default();
    let mut workspace_scripts = Vec::new();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--addon-source-inventory" => {
                game_data.addon_source_inventory =
                    Some(path_value(&mut args, "--addon-source-inventory")?)
            }
            "--addon-index-storage" => {
                game_data.addon_index_storage =
                    Some(path_value(&mut args, "--addon-index-storage")?)
            }
            "--external-index-mode" => {
                let value = string_value(&mut args, "--external-index-mode")?;
                game_data.external_index_mode = match value.as_str() {
                    "all" => GameDataExternalIndexMode::All,
                    "loaded" => GameDataExternalIndexMode::Loaded,
                    "none" => GameDataExternalIndexMode::None,
                    _ => return Err(format!("invalid external index mode '{value}'")),
                };
            }
            "--official-wiki-root" => {
                official_wiki_root = Some(path_value(&mut args, "--official-wiki-root")?)
            }
            "--workspace-scripts" => {
                workspace_scripts.push(path_value(&mut args, "--workspace-scripts")?);
            }
            "--dependency-project" => {
                game_data
                    .dependency_project_files
                    .push(path_value(&mut args, "--dependency-project")?);
            }
            "--workbench-host" => {
                workbench.gateway.host = string_value(&mut args, "--workbench-host")?
            }
            "--workbench-port" => {
                let value = string_value(&mut args, "--workbench-port")?;
                workbench.gateway.port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid value for --workbench-port: {value}"))?;
            }
            "--workbench-executable" => {
                workbench.executable = Some(path_value(&mut args, "--workbench-executable")?)
            }
            "--reforger-game-directory" => {
                workbench.game_directory = Some(path_value(&mut args, "--reforger-game-directory")?)
            }
            "--reforger-tools-directory" => {
                workbench.tools_directory =
                    Some(path_value(&mut args, "--reforger-tools-directory")?)
            }
            "--workbench-user-directory" => {
                workbench.user_directory =
                    Some(path_value(&mut args, "--workbench-user-directory")?)
            }
            "--workbench-profile-directory" => {
                workbench.profile_directory =
                    Some(path_value(&mut args, "--workbench-profile-directory")?)
            }
            "--workbench-wine-prefix" => {
                workbench.wine_prefix = Some(path_value(&mut args, "--workbench-wine-prefix")?)
            }
            _ => return Err(format!("unknown MCP argument '{argument}'")),
        }
    }

    game_data.workspace_roots = workspace_scripts.clone();
    Ok(McpServerOptions {
        game_data,
        official_wiki_root,
        workbench,
        workspace_scripts,
    })
}

fn path_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf, String> {
    string_value(args, flag).map(PathBuf::from)
}

fn string_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("missing value for {flag}"))?;
    if value.is_empty() || value.starts_with("--") {
        return Err(format!("missing value for {flag}"));
    }
    Ok(value)
}

fn print_help() {
    println!(
        "Usage:\n  reforger_language_server [LSP options]\n  reforger_language_server mcp [MCP options]\n  reforger_language_server mcp-api\n  reforger_language_server mcp-api-bundle\n  reforger_language_server workbench-api <status|validate|loaded-addon-graph|read-logs|integration-status|bootstrap-integration|maintain-integration|process-status|launch-default|install-bridge|reload-bridge> [--host <loopback>] [--port <port>] [--workbench-wine-prefix <path>]\n\nLSP options:\n  --log <path>\n  --diagnostic-log <path>\n  --addon-source-inventory <path>\n  --addon-index-storage <path>\n  --workspace-scripts <path> (repeatable)\n  --dependency-project <path> (repeatable)\n  --workbench-wine-prefix <path>\n  --bracket-coloring <semantic|punctuation|vscode>\n\nMCP options:\n  --addon-source-inventory <path>\n  --addon-index-storage <path>\n  --external-index-mode <all|loaded|none>\n  --workspace-scripts <path> (repeatable)\n  --dependency-project <path> (repeatable)\n  --official-wiki-root <development/test path>\n  --workbench-host <loopback host>\n  --workbench-port <port>\n  --workbench-executable <path>\n  --reforger-game-directory <path>\n  --reforger-tools-directory <path>\n  --workbench-user-directory <test/development override>\n  --workbench-profile-directory <test/development override>\n  --workbench-wine-prefix <path>"
    );
}

#[cfg(test)]
mod tests {
    use super::{parse_args_from, ServerMode, WorkbenchApiCommand};
    use reforger_language_server::game_data_catalogue::GameDataExternalIndexMode;
    use reforger_language_server::lsp::BracketColoringMode;
    use std::path::PathBuf;

    #[test]
    fn bracket_coloring_argument_accepts_every_extension_setting_value() {
        for (value, expected) in [
            ("semantic", BracketColoringMode::Semantic),
            ("punctuation", BracketColoringMode::Punctuation),
            ("vscode", BracketColoringMode::VsCode),
        ] {
            let mode =
                parse_args_from(["--bracket-coloring".to_string(), value.to_string()].into_iter())
                    .expect("valid LSP arguments");
            let ServerMode::Lsp(options) = mode else {
                panic!("expected LSP mode");
            };
            assert_eq!(options.bracket_coloring, expected);
        }
    }

    #[test]
    fn explicit_mcp_mode_rejects_the_retired_single_cache_argument() {
        let result = parse_args_from(
            [
                "mcp".to_string(),
                "--index-cache".to_string(),
                "cache.bin".to_string(),
            ]
            .into_iter(),
        );
        let Err(error) = result else {
            panic!("the retired single-cache route must not be accepted");
        };

        assert!(error.contains("unknown MCP argument '--index-cache'"));
    }

    #[test]
    fn explicit_mcp_mode_accepts_the_loaded_addon_scope_inputs() {
        let mode = parse_args_from(
            [
                "mcp".to_string(),
                "--addon-source-inventory".to_string(),
                "graph.json".to_string(),
                "--addon-index-storage".to_string(),
                "addon-indexes".to_string(),
                "--external-index-mode".to_string(),
                "all".to_string(),
                "--workspace-scripts".to_string(),
                "Scripts".to_string(),
            ]
            .into_iter(),
        )
        .expect("valid MCP arguments");

        let ServerMode::Mcp(options) = mode else {
            panic!("expected MCP mode");
        };
        assert_eq!(
            options.game_data.addon_source_inventory,
            Some(PathBuf::from("graph.json"))
        );
        assert_eq!(
            options.game_data.addon_index_storage,
            Some(PathBuf::from("addon-indexes"))
        );
        assert_eq!(
            options.game_data.external_index_mode,
            GameDataExternalIndexMode::All
        );
        assert_eq!(
            options.game_data.workspace_roots,
            vec![PathBuf::from("Scripts")]
        );
    }

    #[test]
    fn unknown_modes_flags_and_missing_values_are_rejected() {
        for arguments in [
            vec!["unknown-mode".to_string()],
            vec!["--unknown".to_string()],
            vec!["--log".to_string()],
            vec!["mcp".to_string(), "--index-cache".to_string()],
        ] {
            assert!(parse_args_from(arguments.into_iter()).is_err());
        }
    }

    #[test]
    fn explicit_mcp_mode_accepts_a_workbench_profile_directory() {
        let mode = parse_args_from(
            [
                "mcp".to_string(),
                "--workbench-profile-directory".to_string(),
                "fixture-profile".to_string(),
            ]
            .into_iter(),
        )
        .expect("valid MCP arguments");

        let ServerMode::Mcp(options) = mode else {
            panic!("expected MCP mode");
        };
        assert_eq!(
            options.workbench.profile_directory,
            Some(PathBuf::from("fixture-profile"))
        );
    }

    #[test]
    fn explicit_mcp_mode_accepts_repeatable_workspace_script_roots() {
        let mode = parse_args_from(
            [
                "mcp".to_string(),
                "--workspace-scripts".to_string(),
                "addon-a/Scripts".to_string(),
                "--workspace-scripts".to_string(),
                "addon-b/Scripts".to_string(),
            ]
            .into_iter(),
        )
        .expect("valid MCP arguments");

        let ServerMode::Mcp(options) = mode else {
            panic!("expected MCP mode");
        };
        assert_eq!(
            options.workspace_scripts,
            vec![
                PathBuf::from("addon-a/Scripts"),
                PathBuf::from("addon-b/Scripts")
            ]
        );
    }

    #[test]
    fn lsp_accepts_the_language_client_stdio_transport_marker() {
        let mode = parse_args_from(["--stdio".to_string()].into_iter())
            .expect("known language-client transport marker");
        assert!(matches!(mode, ServerMode::Lsp(_)));
    }

    #[test]
    fn lsp_accepts_repeatable_dependency_project_files() {
        let mode = parse_args_from(
            [
                "--dependency-project".to_string(),
                "one/addon.gproj".to_string(),
                "--dependency-project".to_string(),
                "two/addon.gproj".to_string(),
            ]
            .into_iter(),
        )
        .expect("valid dependency project arguments");
        let ServerMode::Lsp(options) = mode else {
            panic!("expected LSP mode");
        };
        assert_eq!(
            options.dependency_project_files,
            vec![
                PathBuf::from("one/addon.gproj"),
                PathBuf::from("two/addon.gproj")
            ]
        );
    }

    #[test]
    fn mcp_accepts_repeatable_dependency_project_files() {
        let mode = parse_args_from(
            [
                "mcp".to_string(),
                "--dependency-project".to_string(),
                "one/addon.gproj".to_string(),
                "--dependency-project".to_string(),
                "two/addon.gproj".to_string(),
            ]
            .into_iter(),
        )
        .expect("valid MCP dependency project arguments");
        let ServerMode::Mcp(options) = mode else {
            panic!("expected MCP mode");
        };
        assert_eq!(
            options.game_data.dependency_project_files,
            vec![
                PathBuf::from("one/addon.gproj"),
                PathBuf::from("two/addon.gproj")
            ]
        );
    }

    #[test]
    fn workbench_api_exposes_extension_owned_integration_operations() {
        for (name, expected) in [
            ("loaded-addon-graph", WorkbenchApiCommand::LoadedAddonGraph),
            ("read-logs", WorkbenchApiCommand::ReadLogs),
            ("integration-status", WorkbenchApiCommand::IntegrationStatus),
            ("install-bridge", WorkbenchApiCommand::InstallBridge),
            ("reload-bridge", WorkbenchApiCommand::ReloadBridge),
        ] {
            let mode = parse_args_from(["workbench-api".to_string(), name.to_string()].into_iter())
                .expect("valid private Workbench API operation");
            let ServerMode::WorkbenchApi(actual, _) = mode else {
                panic!("expected Workbench API mode");
            };
            assert_eq!(actual, expected);
        }
    }
}
