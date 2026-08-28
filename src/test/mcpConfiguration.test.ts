import * as assert from 'assert';
import {
	buildMcpLaunchConfiguration,
	renderCodexMcpConfiguration,
	renderGenericMcpConfiguration,
} from '../mcp/mcpConfiguration';

suite('MCP configuration', () => {
	test('builds a stable launch independent of a running VS Code process', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: 'C:\\Extensions\\reforger_language_server.exe',
			addonSourceInventory: 'C:\\Storage\\addon-sources\\workbench-graph-v1.json',
			addonIndexStorage: 'C:\\Storage\\addon-indexes',
			externalIndexMode: 'loaded',
		});

		assert.deepStrictEqual(launch, {
			command: 'C:\\Extensions\\reforger_language_server.exe',
			args: [
				'mcp',
				'--addon-source-inventory',
				'C:\\Storage\\addon-sources\\workbench-graph-v1.json',
				'--addon-index-storage',
				'C:\\Storage\\addon-indexes',
				'--external-index-mode',
				'loaded',
			],
		});
	});

	test('uses the authoritative loaded add-on inventory and parser-owned index storage', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: '/extension/reforger_language_server',
			addonSourceInventory: '/storage/addon-sources/workbench-graph-v1.json',
			addonIndexStorage: '/storage/addon-indexes',
			externalIndexMode: 'loaded',
		});

		assert.deepStrictEqual(launch.args, [
			'mcp',
			'--addon-source-inventory',
			'/storage/addon-sources/workbench-graph-v1.json',
			'--addon-index-storage',
			'/storage/addon-indexes',
			'--external-index-mode',
			'loaded',
		]);
	});

	test('carries all mode into MCP instead of implicitly selecting the Workbench graph', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: '/extension/reforger_language_server',
			addonSourceInventory: '/storage/addon-sources/workbench-graph-v1.json',
			addonIndexStorage: '/storage/addon-indexes',
			externalIndexMode: 'all',
		});

		assert.deepStrictEqual(launch.args.slice(-2), ['--external-index-mode', 'all']);
	});

	test('adds optional evidence inputs through the same launch policy', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: '/extension/reforger_language_server',
			addonSourceInventory: '/storage/graph.json',
			addonIndexStorage: '/storage/addon-indexes',
			externalIndexMode: 'loaded',
			officialWikiRoot: '/extension/data/official-wiki',
		});

		assert.deepStrictEqual(launch.args.slice(-2), [
			'--official-wiki-root',
			'/extension/data/official-wiki',
		]);
	});

	test('passes discovered add-on script roots to workspace semantic search', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: '/extension/reforger_language_server',
			addonSourceInventory: '/storage/graph.json',
			addonIndexStorage: '/storage/addon-indexes',
			externalIndexMode: 'loaded',
			workspaceScripts: ['/projects/MyAddon/Scripts'],
		});

		assert.deepStrictEqual(launch.args, [
			'mcp',
			'--addon-source-inventory',
			'/storage/graph.json',
			'--addon-index-storage',
			'/storage/addon-indexes',
			'--external-index-mode',
			'loaded',
			'--workspace-scripts',
			'/projects/MyAddon/Scripts',
		]);
	});

	test('points the MCP runtime at the Wine prefix that hosts Workbench', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: '/extension/reforger_language_server',
			addonSourceInventory: '/storage/graph.json',
			addonIndexStorage: '/storage/addon-indexes',
			externalIndexMode: 'loaded',
			workbenchWinePrefix: '/home/dev/.steam/steam/steamapps/compatdata/1874910/pfx',
		});

		assert.deepStrictEqual(launch.args.slice(-2), [
			'--workbench-wine-prefix',
			'/home/dev/.steam/steam/steamapps/compatdata/1874910/pfx',
		]);
	});

	test('omits the Wine prefix argument when the host resolves it', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: '/extension/reforger_language_server',
			addonSourceInventory: '/storage/graph.json',
			addonIndexStorage: '/storage/addon-indexes',
			externalIndexMode: 'loaded',
		});

		assert.ok(!launch.args.includes('--workbench-wine-prefix'));
	});

	test('passes opened workspace projects to loaded dependency scope discovery', () => {
		const launch = buildMcpLaunchConfiguration({
			serverPath: '/extension/reforger_language_server',
			addonSourceInventory: '/storage/graph.json',
			addonIndexStorage: '/storage/addon-indexes',
			externalIndexMode: 'loaded',
			dependencyProjectFiles: ['/projects/CurrentAddon/addon.gproj'],
		});

		assert.deepStrictEqual(launch.args.slice(-2), [
			'--dependency-project',
			'/projects/CurrentAddon/addon.gproj',
		]);
	});

	test('renders generic JSON and Codex TOML from the same launch', () => {
		const launch = {
			command: 'C:\\Extension\\reforger_language_server.exe',
			args: ['mcp', '--external-index-mode', 'loaded'],
		};

		const generic = JSON.parse(renderGenericMcpConfiguration(launch));
		assert.deepStrictEqual(generic, {
			mcpServers: {
				'reforger-script-tools': launch,
			},
		});

		const codex = renderCodexMcpConfiguration(launch);
		assert.match(codex, /^\[mcp_servers\.reforger-script-tools\]/m);
		assert.match(codex, /command = "C:\\\\Extension\\\\reforger_language_server\.exe"/);
		assert.match(codex, /args = \["mcp", "--external-index-mode", "loaded"\]/);
		assert.match(codex, /startup_timeout_sec = 120\.0/);
		assert.match(codex, /tool_timeout_sec = 130\.0/);
	});

	test('regenerates configuration with the current packaged runtime after an extension upgrade', () => {
		const previous = renderCodexMcpConfiguration(buildMcpLaunchConfiguration({
			serverPath: 'C:\\Users\\Gray\\.vscode\\extensions\\burn0ut7.reforger-script-tools-1.0.1\\dist\\server\\win32-x64\\reforger_language_server.exe',
			addonSourceInventory: 'C:\\Storage\\graph.json',
			addonIndexStorage: 'C:\\Storage\\addon-indexes',
			externalIndexMode: 'loaded',
		}));
		const current = renderCodexMcpConfiguration(buildMcpLaunchConfiguration({
			serverPath: 'C:\\Users\\Gray\\.vscode\\extensions\\burn0ut7.reforger-script-tools-1.0.2\\dist\\server\\win32-x64\\reforger_language_server.exe',
			addonSourceInventory: 'C:\\Storage\\graph.json',
			addonIndexStorage: 'C:\\Storage\\addon-indexes',
			externalIndexMode: 'loaded',
		}));

		assert.doesNotMatch(current, /1\.0\.1/);
		assert.match(current, /1\.0\.2/);
		assert.notStrictEqual(current, previous);
	});
});
