import * as vscode from 'vscode';
import {
	hoverSemanticPaletteForDocument,
	semanticTokenTypes,
	type HoverSemanticForegrounds,
} from '../languageClient/hoverSemanticPalette';

export interface SemanticPreviewToken {
	start: number;
	length: number;
	role: string;
}

export interface SemanticPreview {
	text: string;
	tokens: SemanticPreviewToken[];
	foregrounds: HoverSemanticForegrounds;
	enabled: boolean;
}

/** Masks Rust-classified comments without changing UTF-16 token coordinates. */
export function semanticPreviewSourceLine(
	text: string,
	tokens: readonly SemanticPreviewToken[],
	preserveComments = false,
): string {
	if (preserveComments) {
		return text;
	}
	let result = text;
	for (const token of tokens) {
		if (token.role !== 'comment') {
			continue;
		}
		const start = Math.max(0, token.start);
		const end = Math.min(text.length, token.start + token.length);
		if (end > start) {
			result = result.slice(0, start) + result.slice(start, end).replace(/[^\t]/g, ' ') + result.slice(end);
		}
	}
	return result;
}

/** Decodes the LSP delta-encoded semantic token stream for one document line. */
export function semanticTokenSpansForLine(
	data: Iterable<number>,
	targetLine: number,
): SemanticPreviewToken[] {
	if (!Number.isInteger(targetLine) || targetLine < 0) {
		return [];
	}
	const values = Array.from(data);
	const tokens: SemanticPreviewToken[] = [];
	let line = 0;
	let character = 0;
	for (let index = 0; index + 4 < values.length; index += 5) {
		const deltaLine = values[index] ?? 0;
		line += deltaLine;
		character = deltaLine === 0 ? character + (values[index + 1] ?? 0) : (values[index + 1] ?? 0);
		const length = values[index + 2] ?? 0;
		const role = semanticTokenTypes[values[index + 3] ?? -1];
		if (line === targetLine && length > 0 && role) {
			tokens.push({ start: character, length, role });
		}
	}
	return tokens;
}

/** Builds a trimmed, palette-aware preview from the same semantic tokens used by the editor. */
export function semanticPreviewForLine(
	document: vscode.TextDocument,
	semanticTokens: vscode.SemanticTokens,
	targetLine: number,
	preserveComments = false,
): SemanticPreview | undefined {
	if (targetLine < 0 || targetLine >= document.lineCount) {
		return undefined;
	}
	const documentLine = document.lineAt(targetLine).text;
	const spans = semanticTokenSpansForLine(semanticTokens.data, targetLine);
	const sourceText = semanticPreviewSourceLine(documentLine, spans, preserveComments);
	const leadingWhitespace = sourceText.length - sourceText.trimStart().length;
	const text = sourceText.slice(leadingWhitespace).trimEnd();
	const tokens = spans
		.filter(token => preserveComments || token.role !== 'comment')
		.map(token => ({
			...token,
			start: token.start - leadingWhitespace,
		}))
		.map(token => ({
			...token,
			length: Math.min(token.length, text.length - token.start),
		}))
		.filter(token => token.start >= 0 && token.length > 0 && token.start < text.length);
	if (tokens.length === 0) {
		return undefined;
	}
	const palette = hoverSemanticPaletteForDocument(document);
	return {
		text,
		tokens,
		foregrounds: palette.foregrounds,
		enabled: palette.enabled,
	};
}

/** Builds a palette-aware preview across an inclusive range of document lines. */
export function semanticPreviewForLines(
	document: vscode.TextDocument,
	semanticTokens: vscode.SemanticTokens,
	startLine: number,
	endLine: number,
	preserveComments = false,
): SemanticPreview | undefined {
	const first = Math.max(0, startLine);
	const last = Math.min(document.lineCount - 1, endLine);
	if (first > last) {
		return undefined;
	}
	let offset = 0;
	const textLines: string[] = [];
	const tokens: SemanticPreviewToken[] = [];
	for (let line = first; line <= last; line += 1) {
		const documentLine = document.lineAt(line).text;
		const spans = semanticTokenSpansForLine(semanticTokens.data, line);
		const sourceText = semanticPreviewSourceLine(documentLine, spans, preserveComments);
		const text = sourceText.trimEnd();
		textLines.push(text);
		tokens.push(...spans
			.filter(token => preserveComments || token.role !== 'comment')
			.map(token => ({ ...token, start: offset + token.start }))
			.map(token => ({ ...token, length: Math.min(token.length, offset + text.length - token.start) }))
			.filter(token => token.start >= offset && token.length > 0 && token.start < offset + text.length));
		offset += text.length + 1;
	}
	if (tokens.length === 0) {
		return undefined;
	}
	const palette = hoverSemanticPaletteForDocument(document);
	return { text: textLines.join('\n'), tokens, foregrounds: palette.foregrounds, enabled: palette.enabled };
}
