import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { languageClientServer } from '../extensionConfig/languageClient';

export async function resolveLanguageServerPath(
	context: vscode.ExtensionContext,
): Promise<string | undefined> {
	const developmentPath = path.join(
		context.extensionPath,
		...languageClientServer.devBinaryRelativePath,
	);
	if (
		context.extensionMode === vscode.ExtensionMode.Development
		&& await isFile(developmentPath)
	) {
		return developmentPath;
	}

	const packagedPath = path.join(
		context.extensionPath,
		'dist',
		languageClientServer.distFolder,
		`${process.platform}-${process.arch}`,
		languageClientServer.binaryName,
	);
	if (await isFile(packagedPath)) {
		return packagedPath;
	}

	return undefined;
}

async function isFile(targetPath: string): Promise<boolean> {
	try {
		return (await fs.stat(targetPath)).isFile();
	} catch {
		return false;
	}
}
