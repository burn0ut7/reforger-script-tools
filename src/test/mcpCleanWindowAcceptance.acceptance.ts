import * as assert from 'node:assert';
import * as vscode from 'vscode';
import { mcpServer } from '../extensionConfig/mcp';
import {
	workbenchConfig,
	workbenchDefaults,
	workbenchTestCommands,
} from '../extensionConfig/workbench';
import type { WorkbenchCompilerObservation } from '../workbenchNetApi/compiler/workbenchCompiler';

suite('native MCP clean-window acceptance', () => {
	test('discovers the contribution on an MCP request without Enforce or Workbench activation', async () => {
		assert.strictEqual(vscode.workspace.workspaceFolders, undefined);
		assert.strictEqual(
			vscode.workspace.textDocuments.some(document => document.languageId === 'enforce'),
			false,
		);

		const extension = vscode.extensions.all.find(
			candidate => candidate.packageJSON.name === 'reforger-script-tools',
		);
		assert.ok(extension, 'development extension is discoverable');
		assert.strictEqual(extension.isActive, false);

		assert.deepStrictEqual(extension.packageJSON.contributes.chatSkills ?? [], []);
		assert.strictEqual(extension.isActive, false);

		// Native discovery is installed after workbench restoration. The extension
		// test host may start earlier, so retry the real request until it is ready.
		await waitUntil(async () => {
			const discovery = vscode.commands.executeCommand('workbench.mcp.listServer');
			await vscode.commands.executeCommand('workbench.action.closeQuickOpen');
			await discovery;
			// Cached definitions need not activate until the server is resolved.
			await vscode.commands.executeCommand('workbench.mcp.startServer', '*', { waitForLiveTools: true });
			return extension.isActive;
		});

		const providers = extension.packageJSON.contributes.mcpServerDefinitionProviders as Array<{
			id: string;
			label: string;
		}>;
		assert.deepStrictEqual(providers, [{
			id: mcpServer.providerId,
			label: mcpServer.label,
		}]);
		assert.strictEqual(
			vscode.workspace.getConfiguration(workbenchConfig.section).get(
				workbenchConfig.settings.enabled,
				workbenchDefaults.enabled,
			),
			false,
		);

		const wikiStatus = vscode.lm.tools.find(tool =>
			tool.name.endsWith('official_wiki_status'));
		assert.ok(
			wikiStatus,
			`Discovered MCP tools did not include Official Wiki status: ${vscode.lm.tools
				.map(tool => tool.name)
				.join(', ')}`,
		);
		const wikiResult = await vscode.lm.invokeTool(
			wikiStatus.name,
			{ input: {}, toolInvocationToken: undefined },
		);
		const wikiText = wikiResult.content
			.filter((part): part is vscode.LanguageModelTextPart =>
				part instanceof vscode.LanguageModelTextPart)
			.map(part => part.value)
			.join('\n');
		assert.match(wikiText, /"available"\s*:\s*true/);

		await new Promise(resolve => setTimeout(resolve, 250));
		const observation = await vscode.commands.executeCommand<WorkbenchCompilerObservation>(
			workbenchTestCommands.observeCompiler,
		);
		assert.strictEqual(observation.phase, 'disabled');
		assert.strictEqual(observation.statusVisible, false);
	});
});

async function waitUntil(predicate: () => Promise<boolean>): Promise<void> {
	for (let attempt = 0; attempt < 100; attempt += 1) {
		if (await predicate()) {
			return;
		}
		await new Promise(resolve => setTimeout(resolve, 50));
	}
	assert.fail('VS Code did not activate the contributed MCP provider');
}
