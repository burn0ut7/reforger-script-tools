import * as vscode from 'vscode';

export const workbenchConfig = {
	section: 'reforgerScriptTools.workbench',
	settings: {
		enabled: 'enabled',
		host: 'host',
		port: 'port',
		saveOnIdle: 'saveOnIdle',
		externalIndexMode: 'externalIndexMode',
		winePrefix: 'winePrefix',
	},
} as const;

export const externalIndexModes = ['all', 'loaded', 'none'] as const;
export type ExternalIndexMode = typeof externalIndexModes[number];

export const workbenchDefaults = {
	enabled: false,
	host: '127.0.0.1',
	port: 5775,
	saveOnIdle: true,
	externalIndexMode: 'loaded' as ExternalIndexMode,
	winePrefix: '',
} as const;

/**
 * The Wine prefix that hosts Workbench, for a host that does not run it
 * natively. An empty setting leaves the language server to resolve the prefix
 * itself from Steam's compatibility data or `WINEPREFIX`.
 */
export function readWorkbenchWinePrefix(): string | undefined {
	const value = vscode.workspace.getConfiguration(workbenchConfig.section).get(
		workbenchConfig.settings.winePrefix,
		workbenchDefaults.winePrefix,
	);
	return typeof value === 'string' && value.trim().length > 0 ? value.trim() : undefined;
}

/** The command-line arguments that point a server process at that prefix. */
export function workbenchWinePrefixArguments(): string[] {
	const prefix = readWorkbenchWinePrefix();
	return prefix ? ['--workbench-wine-prefix', prefix] : [];
}

export const workbenchCommands = {
	validateScripts: 'reforger-sript-tools.workbench.validateScripts',
	openCompilerDiagnostic: 'reforger-sript-tools.workbench.openCompilerDiagnostic',
} as const;

export const workbenchTestCommands = {
	observeCompiler: 'reforger-sript-tools.test.observeWorkbenchCompiler',
	disposeCompiler: 'reforger-sript-tools.test.disposeWorkbenchCompiler',
	restartCompiler: 'reforger-sript-tools.test.restartWorkbenchCompiler',
	armStartupValidation: 'reforger-sript-tools.test.armWorkbenchStartupValidation',
	resetFailureNotification: 'reforger-sript-tools.test.resetWorkbenchFailureNotification',
} as const;

export const workbenchDiagnostics = {
	collectionName: 'Workbench',
	source: 'Workbench Compiler',
	outputChannelName: 'Reforger Workbench Compiler',
	outputLanguageId: 'reforger-workbench-compiler-output',
} as const;
