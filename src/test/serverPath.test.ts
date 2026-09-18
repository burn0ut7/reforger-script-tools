import * as assert from 'node:assert';
import * as fs from 'node:fs/promises';
import * as os from 'node:os';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { languageClientServer } from '../extensionConfig/languageClient';
import { resolveLanguageServerPath } from '../languageClient/serverPath';

suite('Language server executable selection', () => {
	let root: string;
	let developmentPath: string;
	let packagedPath: string;

	setup(async () => {
		root = await fs.mkdtemp(path.join(os.tmpdir(), 'rst-server-path-'));
		developmentPath = path.join(root, ...languageClientServer.devBinaryRelativePath);
		packagedPath = path.join(root, 'dist', languageClientServer.distFolder,
			`${process.platform}-${process.arch}`, languageClientServer.binaryName);
	});

	teardown(async () => {
		await fs.rm(root, { recursive: true, force: true });
	});

	function resolve(mode: vscode.ExtensionMode): Promise<string | undefined> {
		return resolveLanguageServerPath({ extensionPath: root, extensionMode: mode } as vscode.ExtensionContext);
	}

	async function createBinary(target: string): Promise<void> {
		await fs.mkdir(path.dirname(target), { recursive: true });
		await fs.writeFile(target, 'test executable placeholder');
	}

	test('prefers the development build only in development mode', async () => {
		await createBinary(developmentPath);
		await createBinary(packagedPath);
		assert.strictEqual(await resolve(vscode.ExtensionMode.Development), developmentPath);
		assert.strictEqual(await resolve(vscode.ExtensionMode.Production), packagedPath);
		assert.strictEqual(await resolve(vscode.ExtensionMode.Test), packagedPath);
	});

	test('never substitutes a development build for a missing packaged runtime', async () => {
		await createBinary(developmentPath);
		assert.strictEqual(await resolve(vscode.ExtensionMode.Production), undefined);
		assert.strictEqual(await resolve(vscode.ExtensionMode.Test), undefined);
		assert.strictEqual(await resolve(vscode.ExtensionMode.Development), developmentPath);
	});

	test('allows development to use the packaged runtime when no development build exists', async () => {
		await createBinary(packagedPath);
		assert.strictEqual(await resolve(vscode.ExtensionMode.Development), packagedPath);
	});

	test('rejects a directory in place of the packaged executable', async () => {
		await fs.mkdir(packagedPath, { recursive: true });
		assert.strictEqual(await resolve(vscode.ExtensionMode.Production), undefined);
	});
});
