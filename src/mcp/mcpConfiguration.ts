import * as path from 'node:path';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs/promises';
import * as vscode from 'vscode';
import { mcpCommands, mcpServer } from '../extensionConfig/mcp';
import { gameDataStorage } from '../extensionConfig/gameData';
import { languageClientIndexCache } from '../extensionConfig/languageClient';
import {
	externalIndexModes,
	type ExternalIndexMode,
	workbenchConfig,
	workbenchDefaults,
} from '../extensionConfig/workbench';
import { resolveLanguageServerPath } from '../languageClient/serverPath';
import { discoverWorkspaceProjectFiles, discoverWorkspaceScriptRoots } from '../languageClient/workspaceWatchBridge';

export interface McpLaunch {
	command: string;
	args: string[];
}

export interface McpLaunchInputs {
	serverPath: string;
	addonSourceInventory: string;
	addonIndexStorage: string;
	externalIndexMode: ExternalIndexMode;
	officialWikiRoot?: string;
	workspaceScripts?: string[];
	dependencyProjectFiles?: string[];
}

export interface McpDependencyProject {
	path: string;
	contents: string;
}

export interface McpLaunchPolicyInputs extends Omit<McpLaunchInputs, 'dependencyProjectFiles'> {
	dependencyProjects?: McpDependencyProject[];
	addonSourceInventoryContents?: string;
}

export interface McpLaunchPolicy {
	launch: McpLaunch;
	scopeIdentity: string;
}

type ConfigurationFormat = 'generic' | 'codex';

const genericChoice = 'Generic MCP JSON';
const codexChoice = 'Codex config.toml';

export function buildMcpLaunchConfiguration(
	inputs: McpLaunchInputs,
	toolProfile: 'authoring' | 'all' = 'authoring',
): McpLaunch {
	const args = [
		'mcp',
		'--tool-profile',
		toolProfile,
		'--addon-source-inventory',
		inputs.addonSourceInventory,
		'--addon-index-storage',
		inputs.addonIndexStorage,
		'--external-index-mode',
		inputs.externalIndexMode,
		...(inputs.officialWikiRoot ? ['--official-wiki-root', inputs.officialWikiRoot] : []),
		...(inputs.workspaceScripts ?? []).flatMap(root => ['--workspace-scripts', root]),
		...(inputs.dependencyProjectFiles ?? []).flatMap(projectFile => ['--dependency-project', projectFile]),
	];
	return {
		command: inputs.serverPath,
		args,
	};
}

export function buildMcpLaunchPolicy(inputs: McpLaunchPolicyInputs): McpLaunchPolicy {
	const launch = buildMcpLaunchConfiguration({
		...inputs,
		dependencyProjectFiles: inputs.dependencyProjects?.map(project => project.path),
	});
	const scopeIdentity = createHash('sha256')
		.update(JSON.stringify({
			launch,
			addonSourceInventoryContents: inputs.addonSourceInventoryContents ?? '<unavailable>',
			dependencyProjects: inputs.dependencyProjects ?? [],
		}))
		.digest('hex');
	return { launch, scopeIdentity };
}

export async function resolveMcpLaunchPolicy(
	context: vscode.ExtensionContext,
): Promise<McpLaunchPolicy> {
	const serverPath = await resolveLanguageServerPath(context);
	if (!serverPath) {
		throw new Error(mcpServer.runtimeUnavailableMessage);
	}
	const addonSourceInventory = path.join(
		context.globalStorageUri.fsPath,
		gameDataStorage.rootFolder,
		gameDataStorage.inventoryFile,
	);
	const dependencyProjectFiles = await discoverWorkspaceProjectFiles();
	const dependencyProjects = await Promise.all(
		dependencyProjectFiles.map(async projectFile => ({
			path: projectFile,
			contents: await readEvidenceIdentity(projectFile),
		})),
	);
	return buildMcpLaunchPolicy({
		serverPath,
		addonSourceInventory,
		addonSourceInventoryContents: await readEvidenceIdentity(addonSourceInventory),
		addonIndexStorage: path.join(
			context.globalStorageUri.fsPath,
			languageClientIndexCache.rootFolder,
		),
		externalIndexMode: readExternalIndexMode(),
		officialWikiRoot: path.join(context.extensionPath, 'data', 'official-wiki'),
		workspaceScripts: await discoverWorkspaceScriptRoots(),
		dependencyProjects,
	});
}

export function renderGenericMcpConfiguration(launch: McpLaunch): string {
	return `${JSON.stringify({
		mcpServers: {
			[mcpServer.name]: launch,
		},
	}, null, 2)}\n`;
}

export function renderCodexMcpConfiguration(launch: McpLaunch): string {
	return [
		`[mcp_servers.${mcpServer.name}]`,
		`command = ${tomlString(launch.command)}`,
		`args = [${launch.args.map(tomlString).join(', ')}]`,
		`startup_timeout_sec = ${mcpServer.initializationDeadlineSeconds}.0`,
		`tool_timeout_sec = ${mcpServer.toolTimeoutSeconds}.0`,
		'',
	].join('\n');
}

export function registerMcpConfigurationCommand(
	context: vscode.ExtensionContext,
): void {
	context.subscriptions.push(vscode.commands.registerCommand(
		mcpCommands.copyConfiguration,
		async (requestedFormat?: ConfigurationFormat) => {
			let policy: McpLaunchPolicy;
			try {
				policy = await resolveMcpLaunchPolicy(context);
			} catch (error) {
				await vscode.window.showErrorMessage(
					error instanceof Error ? error.message : mcpServer.runtimeUnavailableMessage,
				);
				return;
			}

			const format = requestedFormat ?? await selectConfigurationFormat();
			if (!format) {
				return;
			}
			const launch = policy.launch;
			const configuration = format === 'codex'
				? renderCodexMcpConfiguration(launch)
				: renderGenericMcpConfiguration(launch);

			await vscode.env.clipboard.writeText(configuration);
			await vscode.window.showInformationMessage(
				`${format === 'codex' ? codexChoice : genericChoice} copied to the clipboard.`,
			);
		},
	));
}

async function readEvidenceIdentity(filePath: string): Promise<string> {
	try {
		return await fs.readFile(filePath, 'utf8');
	} catch {
		return '<unavailable>';
	}
}

export function readExternalIndexMode(): ExternalIndexMode {
	const value = vscode.workspace.getConfiguration(workbenchConfig.section).get(
		workbenchConfig.settings.externalIndexMode,
		workbenchDefaults.externalIndexMode,
	);
	return typeof value === 'string' && externalIndexModes.includes(value as ExternalIndexMode)
		? value as ExternalIndexMode
		: workbenchDefaults.externalIndexMode;
}

async function selectConfigurationFormat(): Promise<ConfigurationFormat | undefined> {
	const selected = await vscode.window.showQuickPick(
		[
			{
				label: codexChoice,
				description: 'Paste into Codex user or trusted project config.toml',
				format: 'codex' as const,
			},
			{
				label: genericChoice,
				description: 'Use with clients that accept the common mcpServers JSON shape',
				format: 'generic' as const,
			},
		],
		{
			placeHolder: 'Choose an MCP client configuration format',
			title: 'Copy Reforger Script Tools MCP Configuration',
		},
	);
	return selected?.format;
}

function tomlString(value: string): string {
	return JSON.stringify(value);
}
