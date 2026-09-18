import * as assert from 'node:assert';
import * as fs from 'node:fs/promises';
import * as os from 'node:os';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { resolveLanguageServerPath } from '../languageClient/serverPath';
import { McpSearchClient, workspaceScopeId } from '../searchPrototype/mcpSearchClient';

suite('Search UI MCP process', () => {
	test('can call its paginated semantic and text search tools', async function () {
		this.timeout(15_000);
		const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rst-search-profile-'));
		let client: McpSearchClient | undefined;
		try {
			const extensionPath = path.resolve(__dirname, '..', '..');
			const serverPath = await resolveLanguageServerPath({
				extensionPath, extensionMode: vscode.ExtensionMode.Test,
			} as vscode.ExtensionContext);
			assert.ok(serverPath, 'compile must provide the packaged server');
			const scripts = path.join(root, 'Scripts');
			await fs.mkdir(scripts);
			await fs.writeFile(path.join(scripts, 'SearchProfileProbe.c'),
				'/* Header\n still header */ class SearchProfileProbe { int value; } // note\n');
			client = new McpSearchClient({
				serverPath,
				addonSourceInventory: path.join(root, 'inventory.json'),
				addonIndexStorage: path.join(root, 'indexes'),
				externalIndexMode: 'none',
				workspaceScripts: [scripts],
				dependencyProjectFiles: [],
				officialWikiRoot: path.join(extensionPath, 'data', 'official-wiki'),
			});
			const semantic = await client.search('SearchProfileProbe', [workspaceScopeId], 25, 1);
			assert.deepStrictEqual(semantic.warnings, []);
			assert.ok(semantic.results.some(hit => hit.title === 'SearchProfileProbe'));
			const symbolHit = semantic.results.find(hit => hit.title === 'SearchProfileProbe');
			assert.ok(symbolHit);
			const preview = await client.read(symbolHit, 1);
			assert.strictEqual(preview.startLine, 2);
			assert.ok(preview.content.includes('still header */'));
			assert.strictEqual(preview.previewContent?.trim(), 'class SearchProfileProbe { int value; }');
			const complete = await client.readComplete(symbolHit);
			assert.ok(complete.content.startsWith('/* Header'));
			assert.strictEqual(complete.previewContent, undefined);
			const text = await client.search('SearchProfileProbe', [workspaceScopeId], 25, 1, undefined, 'text');
			assert.deepStrictEqual(text.warnings, []);
			assert.ok(text.results.some(hit => hit.kind === 'text'));
			const textHit = text.results.find(hit => hit.kind === 'text');
			assert.ok(textHit);
			const textPreview = await client.read(textHit, 1);
			assert.ok(textPreview.content.includes('// note'));
			assert.strictEqual(textPreview.previewContent, undefined);
		} finally {
			client?.dispose();
			await fs.rm(root, { recursive: true, force: true });
		}
	});
});
