import { access } from 'node:fs/promises';
import * as path from 'node:path';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { searchLimits } from '../extensionConfig/search';
import type { ExternalIndexMode } from '../extensionConfig/workbench';
import { buildMcpLaunchConfiguration } from '../mcp/mcpConfiguration';

export type SearchSource = 'workspace' | 'gameData' | 'wiki' | 'workbench';
export type SearchMode = 'semantic' | 'text' | 'resource';
export const workspaceScopeId = 'workspace';
export const wikiScopeId = 'wiki';

export function addonScopeLabel(title: string, displayId: string, fallbackId: string): string {
	const normalizedTitle = title.trim();
	const normalizedDisplayId = displayId.trim();
	const parentheticalStart = normalizedTitle.indexOf(' (');
	if (parentheticalStart > 0 && normalizedTitle.endsWith(')')) {
		const humanTitle = normalizedTitle.slice(parentheticalStart + 2, -1).trim();
		if (humanTitle) {
			return humanTitle;
		}
	}
	return normalizedTitle || normalizedDisplayId || fallbackId;
}

export interface SearchScopeSource {
	id: string;
	label: string;
	detail: string;
	kind: 'workspace' | 'addon' | 'wiki';
	pinned: boolean;
	defaultSelected: boolean;
}

export function asThumbnailColor(value: unknown): string | undefined {
	return typeof value === 'string' && /^#[0-9A-F]{6}$/i.test(value) ? value.toUpperCase() : undefined;
}

export interface SearchScopeDiscovery {
	scopeRevision?: string;
	scopeAuthority?: string;
	discoveryMs: number;
	unavailableScopeIds: string[];
	sources: SearchScopeSource[];
}
export interface TextSearchOptions {
	matchCase: boolean;
	matchWholeWord: boolean;
	useRegex: boolean;
}

export const defaultTextSearchOptions: TextSearchOptions = {
	matchCase: false,
	matchWholeWord: false,
	useRegex: false,
};
export type SearchSymbolKind =
	| 'class'
	| 'constructor'
	| 'destructor'
	| 'enum'
	| 'enumMember'
	| 'field'
	| 'function'
	| 'globalField'
	| 'method'
	| 'preprocessorMacro'
	| 'typedef';

export interface SearchKindFilter {
	value: string;
	label: string;
	kinds?: readonly SearchSymbolKind[];
}

export const searchKindFilters: readonly SearchKindFilter[] = [
	{ value: 'all', label: 'All results' },
	{ value: 'class', label: 'Classes', kinds: ['class'] },
	{ value: 'function', label: 'Functions', kinds: ['function', 'method', 'constructor', 'destructor'] },
	{ value: 'field', label: 'Fields', kinds: ['field', 'globalField'] },
	{ value: 'enum', label: 'Enums', kinds: ['enum', 'enumMember'] },
];

export type SearchResourceKind =
	| 'world'
	| 'script'
	| 'prefab'
	| 'config'
	| 'model'
	| 'material'
	| 'texture'
	| 'layout'
	| 'audio'
	| 'animation'
	| 'particle'
	| 'string'
	| 'ai'
	| 'other';

export interface SearchResourceKindFilter {
	value: string;
	label: string;
	kinds?: readonly SearchResourceKind[];
}

export const searchResourceKindFilters: readonly SearchResourceKindFilter[] = [
	{ value: 'all', label: 'All resources' },
	{ value: 'prefab', label: 'Prefabs', kinds: ['prefab'] },
	{ value: 'script', label: 'Scripts', kinds: ['script'] },
	{ value: 'audio', label: 'Audio', kinds: ['audio'] },
	{ value: 'world', label: 'Worlds', kinds: ['world'] },
	{ value: 'config', label: 'Configs', kinds: ['config'] },
	{ value: 'model', label: 'Models', kinds: ['model'] },
	{ value: 'material', label: 'Materials', kinds: ['material'] },
	{ value: 'texture', label: 'Textures', kinds: ['texture'] },
	{ value: 'layout', label: 'Layouts', kinds: ['layout'] },
	{ value: 'animation', label: 'Animations', kinds: ['animation'] },
	{ value: 'particle', label: 'Particles', kinds: ['particle'] },
	{ value: 'string', label: 'Strings', kinds: ['string'] },
	{ value: 'ai', label: 'AI', kinds: ['ai'] },
	{ value: 'other', label: 'Other', kinds: ['other'] },
];

const allSearchResourceKinds = searchResourceKindFilters
	.flatMap(filter => filter.kinds ?? [])
	.filter((kind, index, values) => values.indexOf(kind) === index);

const searchSymbolKinds: readonly SearchSymbolKind[] = [
	'class',
	'constructor',
	'destructor',
	'enum',
	'enumMember',
	'field',
	'function',
	'globalField',
	'method',
	'preprocessorMacro',
	'typedef',
];

export interface SearchHit {
	id: string;
	source: SearchSource;
	kind: 'symbol' | 'documentation' | 'text' | 'resource';
	title: string;
	detail: string;
	path: string;
	excerpt: string;
	matchKind?: string;
	sourceUrl?: string;
	sourceUri?: string;
	selectionStartLine?: number;
	selectionEndLine?: number;
	readInput: Record<string, unknown>;
	addonGuid?: string;
	addonLabel?: string;
	thumbnailColor?: string;
	textMatchStart?: number;
	textMatchLength?: number;
	resourceName?: string;
	workbenchLink?: string;
	resourcePhysicalPath?: string;
	resourceStale?: boolean;
	symbolKind?: SearchSymbolKind;
	symbolRef?: string;
	qualifiedName?: string;
	signature?: string;
	relationshipKind?: SearchRelationshipKind;
	relationshipDistance?: number;
	relationshipEvidence?: string;
}

export type SearchRelationshipKind =
	| 'direct'
	| 'directBase'
	| 'derivedType'
	| 'moddedExtension'
	| 'overriddenDeclaration'
	| 'override';

export interface SearchRelationshipAnchor {
	source: 'workspace' | 'gameData';
	symbolRef: string;
}

export interface SearchRelationshipRequest {
	anchor: SearchRelationshipAnchor;
	selectedScopeIds: string[];
	relationshipKinds: SearchRelationshipKind[];
	depth: 'one' | 'all';
	pageSize: number;
	page: number;
	symbolKinds?: readonly SearchSymbolKind[];
}

export interface SearchResponse {
	results: SearchHit[];
	warnings: string[];
	total: number;
	truncated: boolean;
	totalBySource: Partial<Record<SearchSource, number>>;
	page: number;
	pageSize: number;
	performance: SearchPerformance;
}

export interface SearchSourcePerformance {
	source: SearchSource;
	initialMs: number;
	rangeMs: number;
	remoteMs: number;
	pagesVisited: number;
	remoteRequests: number;
	cacheHits: number;
	firstPage: number | undefined;
	lastPage: number | undefined;
	cacheSize: number;
	addonTotals?: Record<string, number>;
	textStats?: Record<string, unknown>;
}

export interface SearchPerformance {
	totalMs: number;
	startupMs: number;
	initialSearchMs: number;
	rangeSearchMs: number;
	mergeMs: number;
	requestedPage: number;
	paginationMode: 'offset' | 'cursor' | 'mixed';
	searchMode: SearchMode;
	textOptions: TextSearchOptions;
	pageSize: number;
	sourcePageSize: number;
	sources: SearchSourcePerformance[];
	selectedScopeIds: string[];
	addonGuids: string[];
}

export interface SearchDocument {
	content: string;
	startLine: number;
	endLine: number;
}

export interface WorkbenchProjectContext {
	loadedAddons: string[];
	loadedAddonsTruncated: boolean;
}

export function normalizeWorkbenchProjectContext(value: unknown): WorkbenchProjectContext {
	const record = asRecord(value);
	if (!Array.isArray(record.loadedAddons)
		|| record.loadedAddons.some(addon => typeof addon !== 'string' || addon.length === 0)
		|| typeof record.loadedAddonsTruncated !== 'boolean') {
		throw new Error('The live Workbench project context was malformed.');
	}
	return {
		loadedAddons: record.loadedAddons as string[],
		loadedAddonsTruncated: record.loadedAddonsTruncated,
	};
}

export function sourcePreviewLine(
	document: SearchDocument,
	line: number | undefined,
	needle?: string,
): number {
	const lines = document.content.split(/\r?\n/);
	const startLine = document.startLine > 0 ? document.startLine : line ?? 1;
	const requestedIndex = Math.max(0, Math.min(lines.length - 1, (line ?? startLine) - startLine));
	const candidateIndexes = [
		...Array.from({ length: Math.max(0, lines.length - requestedIndex) }, (_, index) => requestedIndex + index),
		...Array.from({ length: Math.max(0, requestedIndex) }, (_, index) => index),
	];
	const normalizedNeedle = needle?.trim().toLowerCase();
	const matchingIndex = normalizedNeedle
		? candidateIndexes.find(index => {
			const value = stripSourceComments(lines[index] ?? '');
			return value.trim().length > 0 && value.toLowerCase().includes(normalizedNeedle);
		})
		: undefined;
	const contentIndex = matchingIndex
		?? candidateIndexes.find(index => stripSourceComments(lines[index] ?? '').trim().length > 0)
		?? requestedIndex;
	return startLine + contentIndex;
}

export function sourceLinePreview(
	document: SearchDocument,
	line: number | undefined,
	needle?: string,
): string {
	const lines = document.content.split(/\r?\n/);
	const startLine = document.startLine > 0 ? document.startLine : line ?? 1;
	const selectedLine = sourcePreviewLine(document, line, needle);
	const lineIndex = Math.max(0, selectedLine - startLine);
	return stripSourceComments(lines[lineIndex] ?? lines[0] ?? '').trimStart().trimEnd();
}

export function sourceContextPreview(
	document: SearchDocument,
	line: number | undefined,
	contextLines: number,
	needle?: string,
): string {
	const lines = document.content.split(/\r?\n/);
	const startLine = document.startLine > 0 ? document.startLine : line ?? 1;
	const selectedLine = sourcePreviewLine(document, line, needle);
	if (contextLines <= 1) {
		const lineIndex = Math.max(0, selectedLine - startLine);
		return stripSourceComments(lines[lineIndex] ?? lines[0] ?? '').trimStart().trimEnd();
	}
	const selectedIndex = Math.max(0, selectedLine - startLine);
	const context = Math.max(1, Math.min(249, Math.floor(contextLines)));
	const first = Math.max(0, selectedIndex - context);
	const last = Math.min(lines.length, selectedIndex + context + 1);
	return lines.slice(first, last).map(value => stripSourceComments(value).trimEnd()).join('\n');
}

/** Removes comments while preserving quoted strings. */
export function stripSourceComments(value: string): string {
	let result = '';
	let quote: '"' | "'" | undefined;
	let escaped = false;
	let blockComment = false;
	for (let index = 0; index < value.length; index += 1) {
		const character = value[index];
		const next = value[index + 1];
		if (blockComment) {
			if (character === '*' && next === '/') {
				blockComment = false;
				index += 1;
			}
			continue;
		}
		if (quote) {
			result += character;
			if (escaped) {
				escaped = false;
			} else if (character === '\\') {
				escaped = true;
			} else if (character === quote) {
				quote = undefined;
			}
			continue;
		}
		if (character === '"' || character === "'") {
			quote = character;
			result += character;
		} else if (character === '/' && next === '/') {
			break;
		} else if (character === '/' && next === '*') {
			blockComment = true;
			index += 1;
		} else {
			result += character;
		}
	}
	return result;
}

export interface SourceMatchRange {
	start: number;
	length: number;
}

export function sourceMatchRange(text: string, title: string): SourceMatchRange | undefined {
	if (!text || !title) {
		return undefined;
	}
	const exactStart = text.indexOf(title);
	if (exactStart >= 0) {
		return { start: exactStart, length: title.length };
	}
	const foldedStart = text.toLowerCase().indexOf(title.toLowerCase());
	return foldedStart >= 0 ? { start: foldedStart, length: title.length } : undefined;
}

export interface McpSearchClientOptions {
	serverPath: string;
	addonSourceInventory: string;
	addonIndexStorage: string;
	externalIndexMode: ExternalIndexMode;
	workspaceScripts: string[];
	dependencyProjectFiles: string[];
	officialWikiRoot: string;
	workbenchWinePrefix?: string;
}

interface JsonRpcResult {
	result?: {
		isError?: boolean;
		structuredContent?: unknown;
	};
	error?: {
		message?: string;
	};
}

export class McpToolError extends Error {
	public constructor(
		message: string,
		public readonly code: string | undefined,
		public readonly recovery: string | undefined,
	) {
		super(message);
		this.name = 'McpToolError';
	}
}

interface PendingRequest {
	resolve: (value: JsonRpcResult) => void;
	reject: (error: Error) => void;
	timeout: ReturnType<typeof setTimeout>;
}

const requestTimeoutMs = 135_000;
const maxSearchPageCaches = 32;
const maxCachedPagesPerSearch = 32;
const sourcePageSize = 100;
export const maxSearchPages = searchLimits.maxPages;

interface RecordValue {
	[key: string]: unknown;
}

interface CachedSearchPage {
	results: SearchHit[];
	total: number;
	truncated: boolean;
	nextCursor?: string;
	stats?: Record<string, unknown>;
	addonTotals?: Record<string, number>;
}

interface SearchPerformanceTrace {
	startedAt: number;
	startupMs: number;
	initialSearchMs: number;
	rangeSearchStartedAt: number | undefined;
	rangeSearchMs: number;
	mergeMs: number;
	sources: Map<SearchSource, SearchSourcePerformance>;
}

function createSearchPerformanceTrace(): SearchPerformanceTrace {
	return {
		startedAt: performance.now(),
		startupMs: 0,
		initialSearchMs: 0,
		rangeSearchStartedAt: undefined,
		rangeSearchMs: 0,
		mergeMs: 0,
		sources: new Map(),
	};
}

function emptySearchResponse(pageSize: number, page: number, mode: SearchMode, selectedScopeIds: readonly string[]): SearchResponse {
	const normalizedPageSize = Math.min(100, Math.max(1, Number.isFinite(pageSize) ? Math.floor(pageSize) : 25));
	const normalizedPage = Math.min(maxSearchPages, Math.max(1, Number.isFinite(page) ? Math.floor(page) : 1));
	const trace = createSearchPerformanceTrace();
	return {
		results: [], warnings: [], total: 0, truncated: false, totalBySource: {},
		page: normalizedPage, pageSize: normalizedPageSize,
		performance: finishSearchPerformance(trace, normalizedPage, normalizedPageSize, [], mode, defaultTextSearchOptions, [...selectedScopeIds], []),
	};
}

function sourcePerformanceFor(
	trace: SearchPerformanceTrace,
	source: SearchSource,
): SearchSourcePerformance {
	let value = trace.sources.get(source);
	if (!value) {
		value = {
			source,
			initialMs: 0,
			rangeMs: 0,
			remoteMs: 0,
			pagesVisited: 0,
			remoteRequests: 0,
			cacheHits: 0,
			firstPage: undefined,
			lastPage: undefined,
			cacheSize: 0,
		};
		trace.sources.set(source, value);
	}
	return value;
}

function recordVisitedPage(trace: SearchSourcePerformance, page: number): void {
	trace.pagesVisited += 1;
	trace.firstPage = trace.firstPage === undefined ? page : Math.min(trace.firstPage, page);
	trace.lastPage = trace.lastPage === undefined ? page : Math.max(trace.lastPage, page);
}

function finishSearchPerformance(
	trace: SearchPerformanceTrace,
	requestedPage: number,
	pageSize: number,
	sources: readonly SearchSource[],
	mode: SearchMode,
	textOptions: TextSearchOptions,
	selectedScopeIds: readonly string[],
	addonGuids: readonly string[],
): SearchPerformance {
	return {
		totalMs: performance.now() - trace.startedAt,
		startupMs: trace.startupMs,
		initialSearchMs: trace.initialSearchMs,
		rangeSearchMs: trace.rangeSearchMs,
		mergeMs: trace.mergeMs,
		requestedPage,
		paginationMode: paginationModeFor(mode, sources),
		searchMode: mode,
		textOptions,
		pageSize,
		sourcePageSize,
		sources: sources.map(source => sourcePerformanceFor(trace, source)),
		selectedScopeIds: [...selectedScopeIds],
		addonGuids: [...addonGuids],
	};
}

export class McpSearchClient {
	private process: ChildProcessWithoutNullStreams | undefined;
	private receiveBuffer = Buffer.alloc(0);
	private nextRequestId = 1;
	private readonly pending = new Map<number, PendingRequest>();
	private initialized: Promise<void> | undefined;
	private readonly searchPageCaches = new Map<string, Map<number, CachedSearchPage>>();
	private lastScopeRevision: string | undefined;
	private lastCatalogueRevision: string | undefined;

	public constructor(private readonly options: McpSearchClientOptions) {}

	public async search(
		query: string,
		selectedScopeIds: string[],
		pageSize: number,
		page: number,
		symbolKinds?: readonly SearchSymbolKind[],
		mode: SearchMode = 'semantic',
		textOptions: TextSearchOptions = defaultTextSearchOptions,
		resourceKinds?: readonly SearchResourceKind[],
	): Promise<SearchResponse> {
		if (mode === 'resource') {
			const addonGuids = selectedScopeIds
				.filter(value => /^[0-9a-f]{16}$/i.test(value))
				.map(value => value.toUpperCase())
				.sort();
			if (addonGuids.length === 0) {
				return emptySearchResponse(pageSize, page, 'resource', selectedScopeIds);
			}
			return this.searchResources(query, pageSize, page, resourceKinds ?? allSearchResourceKinds, addonGuids);
		}
		const trace = createSearchPerformanceTrace();
		await this.start();
		trace.startupMs = performance.now() - trace.startedAt;
		const normalizedPageSize = Math.min(100, Math.max(1, Number.isFinite(pageSize) ? Math.floor(pageSize) : 25));
		const requestedPage = Math.min(maxSearchPages, Math.max(1, Number.isFinite(page) ? Math.floor(page) : 1));
		const normalizedScopes = [...new Set(selectedScopeIds)];
		const addonGuids = normalizedScopes
			.filter(value => /^[0-9a-f]{16}$/i.test(value))
			.map(value => value.toUpperCase())
			.sort();
		const sources: SearchSource[] = [
			...(addonGuids.length > 0 ? ['gameData' as const] : []),
			...(normalizedScopes.includes(workspaceScopeId) ? ['workspace' as const] : []),
			...(normalizedScopes.includes(wikiScopeId) ? ['wiki' as const] : []),
		];
		const searchableSources = mode === 'semantic'
			? sources.filter(source => source !== 'wiki')
			: sources;
		if (searchableSources.length === 0) {
			return { results: [], warnings: [], total: 0, truncated: false, totalBySource: {}, page: 1, pageSize: normalizedPageSize, performance: finishSearchPerformance(trace, 1, normalizedPageSize, [], mode, textOptions, normalizedScopes, addonGuids) };
		}
		const initialSearchStartedAt = performance.now();
		const responses = await Promise.all(searchableSources.map(async source => {
			const sourceTrace = sourcePerformanceFor(trace, source);
			const startedAt = performance.now();
			try {
				const value = await this.searchPage(query, source, sourcePageSize, 1, addonGuids, symbolKinds, trace, mode, textOptions);
				if (value.stats) {
					sourceTrace.textStats = value.stats;
				}
				sourceTrace.initialMs += performance.now() - startedAt;
				return { source, value, warning: undefined };
			} catch (error) {
				sourceTrace.initialMs += performance.now() - startedAt;
				return { source, value: undefined, warning: searchErrorMessage(source, error) };
			}
		}));
		trace.initialSearchMs = performance.now() - initialSearchStartedAt;

		const warnings = responses
			.map(response => response.warning)
			.filter((warning): warning is string => warning !== undefined);
		if (responses.every(response => response.value === undefined) && warnings.length === responses.length) {
			throw new Error(warnings.join(' '));
		}
		const reportedTotal = responses.reduce((sum, response) => sum + (response.value?.total ?? 0), 0);
		const total = Math.min(searchLimits.maxResults, reportedTotal);
		const truncated = reportedTotal > total || responses.some(response => response.value?.truncated === true);
		const totalBySource: Partial<Record<SearchSource, number>> = {};
		const accessibleTotals = new Map<SearchSource, number>();
		let remainingResults = searchLimits.maxResults;
		for (const response of responses) {
			if (response.value) {
				const accessibleTotal = Math.min(response.value.total, remainingResults);
				totalBySource[response.source] = accessibleTotal;
				accessibleTotals.set(response.source, accessibleTotal);
				remainingResults -= accessibleTotal;
			}
		}
		const totalPages = Math.max(1, Math.ceil(total / normalizedPageSize));
		const normalizedPage = Math.min(requestedPage, totalPages);
		const pageStart = (normalizedPage - 1) * normalizedPageSize;
		const pageEnd = pageStart + normalizedPageSize;
		const results: SearchHit[] = [];
		let sourceOffset = 0;
		const mergeStartedAt = performance.now();
		trace.rangeSearchStartedAt = performance.now();
		for (const response of responses) {
			const sourceTotal = accessibleTotals.get(response.source) ?? 0;
			const sourceStart = Math.max(0, pageStart - sourceOffset);
			const sourceEnd = Math.min(sourceTotal, pageEnd - sourceOffset);
			if (response.value && sourceStart < sourceEnd) {
				results.push(...await this.sourceRange(query, response.source, sourceStart, sourceEnd, addonGuids, symbolKinds, trace, mode, textOptions));
			}
			sourceOffset += sourceTotal;
		}
		trace.rangeSearchMs = performance.now() - (trace.rangeSearchStartedAt ?? performance.now());
		trace.mergeMs = performance.now() - mergeStartedAt - trace.rangeSearchMs;
		return {
			results,
			warnings,
			total,
			truncated,
			totalBySource,
			page: normalizedPage,
			pageSize: normalizedPageSize,
			performance: finishSearchPerformance(trace, requestedPage, normalizedPageSize, searchableSources, mode, textOptions, normalizedScopes, addonGuids),
		};
	}

	public async discoverScope(): Promise<SearchScopeDiscovery> {
		const startedAt = performance.now();
		await this.start();
		const status = asRecord(await this.callTool('game_data_status', {}));
		const scopeRevision = asOptionalString(status.scopeRevision);
		const catalogueRevision = asOptionalString(status.catalogueRevision);
		if ((this.lastScopeRevision && scopeRevision && this.lastScopeRevision !== scopeRevision)
			|| (this.lastCatalogueRevision && catalogueRevision && this.lastCatalogueRevision !== catalogueRevision)) {
			this.searchPageCaches.clear();
		}
		this.lastScopeRevision = scopeRevision;
		this.lastCatalogueRevision = catalogueRevision;
		const addons = Array.isArray(status.addons) ? status.addons : [];
		const unavailableScopeIds = addons.flatMap(value => {
			const addon = asRecord(value);
			const id = asString(addon.addonGuid, '').toUpperCase();
			return /^[0-9A-F]{16}$/.test(id) && addon.available === false ? [id] : [];
		});
		const addonSources = addons.flatMap(value => {
			const addon = asRecord(value);
			const id = asString(addon.addonGuid, '').toUpperCase();
			if (!/^[0-9A-F]{16}$/.test(id) || addon.available === false) {
				return [];
			}
			const displayId = asString(addon.displayId, id);
			const title = addonScopeLabel(asString(addon.title, ''), displayId, id);
			const scriptCount = asNumber(addon.scriptCount, 0);
			return [{
				id,
				label: title,
				detail: `${scriptCount.toLocaleString()} scripts`,
				kind: 'addon' as const,
				pinned: addon.pinned === true,
				defaultSelected: addon.defaultSelected === true,
			}];
		});
		return {
			scopeRevision,
			scopeAuthority: asOptionalString(status.scopeAuthority),
			discoveryMs: performance.now() - startedAt,
			unavailableScopeIds,
			sources: [
				{ id: workspaceScopeId, label: 'Workspace', detail: 'Live', kind: 'workspace', pinned: true, defaultSelected: true },
				{ id: wikiScopeId, label: 'Official Wiki', detail: 'Text search', kind: 'wiki', pinned: true, defaultSelected: true },
				...addonSources,
			],
		};
	}

	public async searchRelationships(request: SearchRelationshipRequest): Promise<SearchResponse> {
		const trace = createSearchPerformanceTrace();
		await this.start();
		trace.startupMs = performance.now() - trace.startedAt;
		const pageSize = Math.min(100, Math.max(1, Math.floor(request.pageSize) || 25));
		const requestedPage = Math.min(maxSearchPages, Math.max(1, Math.floor(request.page) || 1));
		const selectedScopeIds = [...new Set(request.selectedScopeIds)];
		const addonGuids = selectedScopeIds
			.filter(value => /^[0-9a-f]{16}$/i.test(value))
			.map(value => value.toUpperCase())
			.sort();
		const includeWorkspace = selectedScopeIds.includes(workspaceScopeId);
		const cacheKey = [
			'relationship', request.anchor.source, request.anchor.symbolRef, includeWorkspace,
			addonGuids.join(','), [...request.relationshipKinds].sort().join(','), request.depth,
			request.symbolKinds?.join(',') ?? '', pageSize,
		].join('\u0000');
		let pages = this.searchPageCaches.get(cacheKey);
		if (!pages) {
			if (this.searchPageCaches.size >= maxSearchPageCaches) {
				const oldest = this.searchPageCaches.keys().next().value;
				if (oldest !== undefined) {
					this.searchPageCaches.delete(oldest);
				}
			}
			pages = new Map<number, CachedSearchPage>();
			this.searchPageCaches.set(cacheKey, pages);
		}
		const sourceTrace = sourcePerformanceFor(trace, request.anchor.source);
		const initialStartedAt = performance.now();
		try {
			for (let pageNumber = 1; pageNumber <= requestedPage; pageNumber += 1) {
				if (pageNumber > 1 && pages.has(pageNumber)) {
					sourceTrace.cacheHits += 1;
					continue;
				}
				const previous = pageNumber > 1 ? pages.get(pageNumber - 1) : undefined;
				if (pageNumber > 1 && !previous?.nextCursor) {
					break;
				}
				const remoteStartedAt = performance.now();
				const value = asRecord(await this.callTool('query_source_symbol_relationships', {
				anchorSource: request.anchor.source,
				symbolRef: request.anchor.symbolRef,
				includeWorkspace,
				addonGuids,
				relationshipKinds: request.relationshipKinds,
				depth: request.depth,
				limit: pageSize,
				...(request.symbolKinds?.length ? { kinds: request.symbolKinds } : {}),
				...(previous?.nextCursor ? { cursor: previous.nextCursor } : {}),
				}));
				const results = normalizeSourceRelationshipPage(value);
				const previousRevision = pageNumber === 1 && pages.get(1)?.stats
					? asOptionalString(pages.get(1)?.stats?.relationshipRevision)
					: undefined;
				const relationshipRevision = asOptionalString(value.relationshipRevision);
				if (pageNumber === 1 && previousRevision && relationshipRevision && previousRevision !== relationshipRevision) {
					pages.clear();
				}
				pages.set(pageNumber, {
				results,
				total: asNumber(value.total, results.length),
				truncated: value.truncated === true,
				...(asOptionalString(value.nextCursor) ? { nextCursor: asOptionalString(value.nextCursor) } : {}),
				stats: {
					relationshipRevision,
					warnings: Array.isArray(value.warnings) ? value.warnings : [],
				},
				});
				sourceTrace.remoteRequests += 1;
				sourceTrace.remoteMs += performance.now() - remoteStartedAt;
				recordVisitedPage(sourceTrace, pageNumber);
			}
		} catch (error) {
			if (error instanceof McpToolError && error.code === 'stale_relationship_cursor' && requestedPage > 1) {
				this.searchPageCaches.delete(cacheKey);
				return this.searchRelationships({ ...request, page: 1 });
			}
			throw error;
		}
		trace.initialSearchMs = performance.now() - initialStartedAt;
		const normalizedPage = pages.has(requestedPage) ? requestedPage : 1;
		const page = pages.get(normalizedPage) ?? { results: [], total: 0, truncated: false };
		const warnings = page.stats && Array.isArray(page.stats.warnings)
			? page.stats.warnings.filter((warning): warning is string => typeof warning === 'string')
			: [];
		const totalBySource: Partial<Record<SearchSource, number>> = {};
		for (const result of page.results) {
			totalBySource[result.source] = (totalBySource[result.source] ?? 0) + 1;
		}
		return {
			results: page.results,
			warnings,
			total: page.total,
			truncated: page.truncated,
			totalBySource,
			page: normalizedPage,
			pageSize,
			performance: {
				...finishSearchPerformance(trace, requestedPage, pageSize, [request.anchor.source], 'semantic', defaultTextSearchOptions, selectedScopeIds, addonGuids),
				paginationMode: 'cursor',
			},
		};
	}

	public async read(hit: SearchHit, contextLines = 0): Promise<SearchDocument> {
		await this.start();
		const tool = hit.source === 'wiki' ? 'read_official_wiki' : hit.source === 'gameData'
			? 'read_game_data_source'
			: 'read_workspace_source';
		const selectedLine = hit.selectionStartLine;
		const context = Math.max(1, Math.min(249, Math.floor(contextLines)));
		const radius = context === 1 ? 0 : context;
		const input = selectedLine === undefined || context === 0
			? hit.readInput
			: { ...hit.readInput, startLine: Math.max(1, selectedLine - radius), lineCount: radius * 2 + 1 };
		const value = await this.callTool(tool, input);
		const record = asRecord(value);
		return {
			content: asString(record.content, 'The source read returned no content.'),
			startLine: asNumber(record.startLine, 0),
			endLine: asNumber(record.endLine, 0),
		};
	}

	public async workbenchProjectContext(): Promise<WorkbenchProjectContext> {
		await this.start();
		return normalizeWorkbenchProjectContext(await this.callTool('workbench_project_context', {}));
	}

	public async readComplete(hit: SearchHit): Promise<SearchDocument> {
		if (hit.source === 'wiki' || typeof hit.readInput.relativePath !== 'string') {
			return this.read(hit);
		}
		await this.start();
		const tool = hit.source === 'gameData' ? 'read_game_data_source' : 'read_workspace_source';
		let content = '';
		let endLine = 0;
		let nextStartLine = 1;
		const visitedStarts = new Set<number>();
		while (!visitedStarts.has(nextStartLine)) {
			visitedStarts.add(nextStartLine);
			const value = asRecord(await this.callTool(tool, {
				...hit.readInput,
				startLine: nextStartLine,
				lineCount: 500,
			}));
			if (typeof value.content !== 'string') {
				throw new Error('The source read returned no content.');
			}
			content += value.content;
			endLine = asNumber(value.endLine, endLine);
			if (value.truncated !== true) {
				return { content, startLine: 1, endLine };
			}
			const next = typeof value.nextStartLine === 'number'
				? Math.floor(value.nextStartLine)
				: 0;
			if (next <= nextStartLine) {
				break;
			}
			nextStartLine = next;
		}
		throw new Error('The complete source read did not make forward progress.');
	}

	public async resolveSourcePath(hit: SearchHit): Promise<string | undefined> {
		const relativePath = typeof hit.readInput.relativePath === 'string'
			? hit.readInput.relativePath
			: undefined;
		if (!relativePath || hit.source === 'gameData') {
			return undefined;
		}
		const roots = hit.source === 'wiki'
			? [this.options.officialWikiRoot]
			: this.options.workspaceScripts;
		for (const root of roots) {
			const candidate = path.resolve(root, relativePath);
			if (!isWithinRoot(root, candidate)) {
				continue;
			}
			try {
				await access(candidate);
				return candidate;
			} catch {
				// Try the next configured source root.
			}
		}
		return undefined;
	}

	public async start(): Promise<void> {
		if (this.initialized) {
			return this.initialized;
		}
		this.initialized = this.startProcess();
		try {
			await this.initialized;
		} catch (error) {
			this.dispose();
			throw error;
		}
	}

	public dispose(): void {
		const activeProcess = this.process;
		this.process = undefined;
		this.initialized = undefined;
		this.searchPageCaches.clear();
		this.lastScopeRevision = undefined;
		this.lastCatalogueRevision = undefined;
		const error = new Error('The Reforger search session was closed.');
		for (const request of this.pending.values()) {
			request.reject(error);
		}
		this.pending.clear();
		if (activeProcess && !activeProcess.killed) {
			activeProcess.stdin.end();
			activeProcess.kill();
		}
	}

	private async searchResources(
		query: string,
		pageSize: number,
		page: number,
		kinds: readonly SearchResourceKind[],
		addonGuids: readonly string[],
	): Promise<SearchResponse> {
		const trace = createSearchPerformanceTrace();
		await this.start();
		if (!this.lastCatalogueRevision) {
			await this.discoverScope();
		}
		const sourceCatalogueRevision = this.lastCatalogueRevision;
		trace.startupMs = performance.now() - trace.startedAt;
		const normalizedPageSize = Math.min(100, Math.max(1, Number.isFinite(pageSize) ? Math.floor(pageSize) : 25));
		const requestedPage = Math.min(maxSearchPages, Math.max(1, Number.isFinite(page) ? Math.floor(page) : 1));
		const source: SearchSource = 'gameData';
		const cacheKey = `resource\u0000${normalizedPageSize}\u0000${query}\u0000${kinds.join(',')}\u0000${addonGuids.join(',')}`;
		let pages = this.searchPageCaches.get(cacheKey);
		if (!pages) {
			if (this.searchPageCaches.size >= maxSearchPageCaches) {
				const oldest = this.searchPageCaches.keys().next().value;
				if (oldest !== undefined) {
					this.searchPageCaches.delete(oldest);
				}
			}
			pages = new Map<number, CachedSearchPage>();
			this.searchPageCaches.set(cacheKey, pages);
		}
		for (let current = 1; current <= requestedPage; current += 1) {
			if (pages.has(current)) {
				continue;
			}
			const previous = current > 1 ? pages.get(current - 1) : undefined;
			if (current > 1 && !previous?.nextCursor) {
				break;
			}
			const startedAt = performance.now();
			let value: RecordValue;
			try {
				value = asRecord(await this.callTool('search_game_data_resources', {
					kinds,
					query,
					...(addonGuids.length ? { addonGuids } : {}),
					...(previous?.stats?.catalogueRevision ? { catalogueRevision: previous.stats.catalogueRevision } : {}),
					limit: normalizedPageSize,
					...(previous?.nextCursor ? { cursor: previous.nextCursor } : {}),
				}));
			} catch (error) {
				if (error instanceof McpToolError && error.code === 'stale_resource_cursor' && requestedPage > 1) {
					this.searchPageCaches.delete(cacheKey);
					return this.searchResources(query, pageSize, 1, kinds, addonGuids);
				}
				throw error;
			}
			const results = normalizeResourceSearchPage(value, sourceCatalogueRevision);
			pages.set(current, {
				results,
				total: asNumber(value.total, results.length),
				truncated: value.truncated === true,
				stats: { catalogueRevision: asOptionalString(value.catalogueRevision) },
				...(typeof value.nextCursor === 'string' && value.nextCursor.length > 0 ? { nextCursor: value.nextCursor } : {}),
			});
			const sourceTrace = sourcePerformanceFor(trace, source);
			sourceTrace.remoteRequests += 1;
			sourceTrace.remoteMs += performance.now() - startedAt;
			recordVisitedPage(sourceTrace, current);
		}
		const currentPage = pages.get(requestedPage) ?? pages.get(1) ?? {
			results: [], total: 0, truncated: false,
		};
		const total = currentPage.total;
		trace.initialSearchMs = performance.now() - trace.startedAt;
		return {
			results: currentPage.results,
			warnings: [],
			total,
			truncated: currentPage.truncated,
			totalBySource: { gameData: total },
			page: pages.has(requestedPage) ? requestedPage : 1,
			pageSize: normalizedPageSize,
			performance: finishSearchPerformance(trace, requestedPage, normalizedPageSize, [source], 'resource', defaultTextSearchOptions, addonGuids, addonGuids),
		};
	}

	private async startProcess(): Promise<void> {
		const launch = buildMcpLaunchConfiguration(this.options);
		const child = spawn(launch.command, launch.args, {
			stdio: ['pipe', 'pipe', 'pipe'],
			windowsHide: true,
		});
		this.process = child;
		child.stdout.on('data', chunk => this.consumeOutput(Buffer.from(chunk)));
		child.on('error', error => this.failPending(error));
		child.on('exit', () => {
			this.process = undefined;
			this.initialized = undefined;
			this.searchPageCaches.clear();
			this.lastScopeRevision = undefined;
			this.lastCatalogueRevision = undefined;
			this.failPending(new Error('The Reforger search server stopped.'));
		});
		child.stdin.on('error', error => this.failPending(error));
		child.stderr.on('data', () => undefined);

		await this.request('initialize', {
			protocolVersion: '2025-11-25',
			capabilities: {},
			clientInfo: { name: 'reforger-search-ui', version: '0.1.0' },
		});
		this.notify('notifications/initialized', {});
	}

	private async callTool(tool: string, argumentsValue: Record<string, unknown>): Promise<unknown> {
		const response = await this.request('tools/call', {
			name: tool,
			arguments: argumentsValue,
		});
		if (response.result?.isError) {
			const structured = asRecord(response.result.structuredContent);
			throw new McpToolError(
				asString(structured.message, `MCP tool ${tool} failed.`),
				typeof structured.code === 'string' ? structured.code : undefined,
				typeof structured.recovery === 'string' ? structured.recovery : undefined,
			);
		}
		return response.result?.structuredContent ?? {};
	}

	private async sourceRange(
		query: string,
		source: SearchSource,
		start: number,
		end: number,
		addonGuids: readonly string[],
		symbolKinds?: readonly SearchSymbolKind[],
		trace?: SearchPerformanceTrace,
		mode: SearchMode = 'semantic',
		textOptions: TextSearchOptions = defaultTextSearchOptions,
	): Promise<SearchHit[]> {
		const sourceStartedAt = performance.now();
		const results: SearchHit[] = [];
		const firstPageNumber = Math.floor(start / sourcePageSize) + 1;
		const lastPageNumber = Math.floor((end - 1) / sourcePageSize) + 1;
		for (let pageNumber = firstPageNumber; pageNumber <= lastPageNumber; pageNumber += 1) {
			const page = await this.searchPage(query, source, sourcePageSize, pageNumber, addonGuids, symbolKinds, trace, mode, textOptions);
			const pageStart = (pageNumber - 1) * sourcePageSize;
			const resultStart = Math.max(0, start - pageStart);
			const resultEnd = Math.min(page.results.length, end - pageStart);
			if (resultStart < resultEnd) {
				results.push(...page.results.slice(resultStart, resultEnd));
			}
			if (!page.nextCursor || page.results.length === 0) {
				break;
			}
		}
		if (trace) {
			sourcePerformanceFor(trace, source).rangeMs += performance.now() - sourceStartedAt;
		}
		return results;
	}

	private async searchPage(
		query: string,
		source: SearchSource,
		pageSize: number,
		page: number,
		addonGuids: readonly string[],
		symbolKinds?: readonly SearchSymbolKind[],
		trace?: SearchPerformanceTrace,
		mode: SearchMode = 'semantic',
		textOptions: TextSearchOptions = defaultTextSearchOptions,
		retryInvalidCursor = true,
	): Promise<CachedSearchPage> {
		const sourceTrace = trace ? sourcePerformanceFor(trace, source) : undefined;
		const cacheKey = `${mode}\u0000${source}\u0000${pageSize}\u0000${query}\u0000${addonGuids.join(',')}\u0000${symbolKinds?.join(',') ?? ''}\u0000${textOptions.matchCase}\u0000${textOptions.matchWholeWord}\u0000${textOptions.useRegex}`;
		let pages = this.searchPageCaches.get(cacheKey);
		if (!pages) {
			if (this.searchPageCaches.size >= maxSearchPageCaches) {
				const oldest = this.searchPageCaches.keys().next().value;
				if (oldest !== undefined) {
					this.searchPageCaches.delete(oldest);
				}
			}
			pages = new Map<number, CachedSearchPage>();
			this.searchPageCaches.set(cacheKey, pages);
		}

		const cached = pages.get(page);
		if (cached) {
			if (sourceTrace) {
				sourceTrace.cacheHits += 1;
				recordVisitedPage(sourceTrace, page);
				sourceTrace.cacheSize = pages.size;
			}
			return cached;
		}
		const usesCursor = mode === 'text' && source !== 'wiki';
		if (usesCursor && page > 1 && !pages.has(page - 1)) {
			await this.searchPage(query, source, pageSize, page - 1, addonGuids, symbolKinds, trace, mode, textOptions);
		}
		const previousPage = usesCursor && page > 1 ? pages.get(page - 1) : undefined;
		const argumentsValue: Record<string, unknown> = usesCursor ? {
			query,
			...(source === 'gameData' ? { addonGuids } : {}),
			limit: pageSize,
			matchCase: textOptions.matchCase,
			matchWholeWord: textOptions.matchWholeWord,
			useRegex: textOptions.useRegex,
			...(previousPage?.nextCursor ? { cursor: previousPage.nextCursor } : {}),
		} : {
			query,
			...(source === 'gameData' ? { addonGuids } : {}),
			limit: pageSize,
			offset: (page - 1) * pageSize,
			...(source !== 'wiki' && symbolKinds?.length ? { kinds: symbolKinds } : {}),
		};
		const remoteStartedAt = performance.now();
		let value: RecordValue;
		try {
			value = asRecord(await this.callTool(searchToolFor(source, mode), argumentsValue));
		} catch (error) {
			if (usesCursor && previousPage?.nextCursor && retryInvalidCursor && isInvalidTextCursor(error)) {
				pages.clear();
				return this.searchPage(
					query,
					source,
					pageSize,
					page,
					addonGuids,
					symbolKinds,
					trace,
					mode,
					textOptions,
					false,
				);
			}
			throw error;
		}
		if (sourceTrace) {
			sourceTrace.remoteRequests += 1;
			sourceTrace.remoteMs += performance.now() - remoteStartedAt;
			recordVisitedPage(sourceTrace, page);
		}
		const results = normalizeSearchPage(source, value, mode);
		const currentPage: CachedSearchPage = {
			results,
			total: asNumber(value.total, results.length),
			truncated: value.truncated === true,
			...(typeof value.nextCursor === 'string' && value.nextCursor.length > 0
				? { nextCursor: value.nextCursor }
				: {}),
			...(mode === 'text' && value.stats && typeof value.stats === 'object' ? { stats: asRecord(value.stats) } : {}),
			...(value.totalsByAddon && typeof value.totalsByAddon === 'object'
				? { addonTotals: numberRecord(value.totalsByAddon) }
				: {}),
		};
		if (sourceTrace && currentPage.addonTotals) {
			sourceTrace.addonTotals = currentPage.addonTotals;
		}
		if (sourceTrace && mode === 'text' && currentPage.stats) {
			sourceTrace.textStats = currentPage.stats;
		}
		pages.set(page, currentPage);
		while (pages.size > maxCachedPagesPerSearch) {
			const oldest = pages.keys().next().value;
			if (oldest !== 1) {
				if (oldest !== undefined) {
					pages.delete(oldest);
				}
				continue;
			}
			const remaining = pages.keys();
			remaining.next();
			const nextOldest = remaining.next().value;
			if (nextOldest === undefined) {
				break;
			}
			pages.delete(nextOldest);
		}
		if (sourceTrace) {
			sourceTrace.cacheSize = pages.size;
		}
		return currentPage;
	}

	private request(method: string, params: unknown): Promise<JsonRpcResult> {
		const activeProcess = this.process;
		if (!activeProcess || activeProcess.killed) {
			return Promise.reject(new Error('The Reforger search server is not running.'));
		}
		const id = this.nextRequestId++;
		const request = { jsonrpc: '2.0', id, method, params };
		const message = `${JSON.stringify(request)}\n`;
		return new Promise((resolve, reject) => {
			const timeout = setTimeout(() => {
				this.pending.delete(id);
				reject(new Error('The Reforger search server did not respond in time.'));
				this.dispose();
			}, requestTimeoutMs);
			this.pending.set(id, { resolve, reject, timeout });
			activeProcess.stdin.write(message, 'utf8');
		});
	}

	private notify(method: string, params: unknown): void {
		const activeProcess = this.process;
		if (!activeProcess || activeProcess.killed) {
			return;
		}
		activeProcess.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method, params })}\n`, 'utf8');
	}

	private consumeOutput(chunk: Buffer): void {
		this.receiveBuffer = Buffer.concat([this.receiveBuffer, chunk]);
		while (true) {
			const lineEnd = this.receiveBuffer.indexOf(0x0a);
			if (lineEnd < 0) {
				return;
			}
			const body = this.receiveBuffer.subarray(0, lineEnd).toString('utf8').trim();
			this.receiveBuffer = this.receiveBuffer.subarray(lineEnd + 1);
			if (!body) {
				continue;
			}
			try {
				this.resolveMessage(JSON.parse(body) as JsonRpcResult & { id?: number });
			} catch (error) {
				this.failPending(error instanceof Error ? error : new Error(String(error)));
				return;
			}
		}
	}

	private resolveMessage(message: JsonRpcResult & { id?: number }): void {
		if (message.id === undefined) {
			return;
		}
		const request = this.pending.get(message.id);
		if (!request) {
			return;
		}
		this.pending.delete(message.id);
		clearTimeout(request.timeout);
		if (message.error) {
			request.reject(new Error(message.error.message ?? 'MCP request failed.'));
		} else {
			request.resolve(message);
		}
	}

	private failPending(error: Error): void {
		for (const request of this.pending.values()) {
			clearTimeout(request.timeout);
			request.reject(error);
		}
		this.pending.clear();
	}
}

function isInvalidTextCursor(error: unknown): boolean {
	const message = error instanceof Error ? error.message : String(error);
	return message === 'cursor is invalid for this text query or source revision.'
		|| message === 'cursor belongs to another source revision.';
}

export function searchToolFor(source: SearchSource, mode: SearchMode = 'semantic'): string {
	if (source === 'wiki') {
		return 'search_official_wiki';
	}
	if (mode === 'text') {
		return source === 'gameData' ? 'search_game_data_text' : 'search_workspace_text';
	}
	return source === 'gameData'
		? 'search_game_data_symbols'
		: 'search_workspace_symbols';
}

function paginationModeFor(mode: SearchMode, sources: readonly SearchSource[]): 'offset' | 'cursor' | 'mixed' {
	if (mode === 'semantic' || sources.every(source => source === 'wiki')) {
		return 'offset';
	}
	return sources.includes('wiki') ? 'mixed' : 'cursor';
}

export function normalizeSearchPage(source: SearchSource, value: unknown, mode: SearchMode = 'semantic'): SearchHit[] {
	const results = asRecord(value).results;
	if (!Array.isArray(results)) {
		return [];
	}
	return results.flatMap((entry, index) => {
		const hit = asRecord(entry);
		if (source === 'wiki') {
			return normalizeWikiHit(hit, index);
		}
		return mode === 'text' ? normalizeTextHit(source, hit, index) : normalizeSymbolHit(source, hit, index);
	});
}

export function resourceKindsFor(value: string): readonly SearchResourceKind[] {
	return searchResourceKindFilters.find(filter => filter.value === value)?.kinds ?? allSearchResourceKinds;
}

export function normalizeResourceSearchPage(
	value: unknown,
	sourceCatalogueRevision?: string,
): SearchHit[] {
	const results = asRecord(value).results;
	if (!Array.isArray(results)) {
		return [];
	}
	return results.flatMap((entry, index) => {
		const hit = asRecord(entry);
		const resourceName = asOptionalString(hit.resourceName);
		if (!resourceName) {
			return [];
		}
		const logicalPath = asString(hit.logicalPath, resourceName);
		const basename = asString(hit.basename, logicalPath.split('/').pop() ?? logicalPath);
		const extension = asString(hit.extension, 'resource');
		const addonGuid = asOptionalString(hit.addonGuid);
		const thumbnailColor = asThumbnailColor(hit.thumbnailColor);
		const scriptReadInput = extension.toLowerCase() === 'c'
			&& addonGuid
			&& sourceCatalogueRevision
			? {
				catalogueRevision: sourceCatalogueRevision,
				addonGuid,
				relativePath: logicalPath,
			}
			: undefined;
		return [{
			id: `game-data-resource-${index}-${resourceName}`,
			source: 'gameData' as const,
			kind: 'resource' as const,
			title: basename,
			detail: `${extension}${hit.stale === true ? ' · stale cache' : ''}`,
			path: logicalPath,
			excerpt: '',
			matchKind: 'resource',
			readInput: scriptReadInput ?? {},
			resourceName,
			...(asOptionalString(hit.workbenchLink) ? { workbenchLink: asOptionalString(hit.workbenchLink) } : {}),
			...(asOptionalString(hit.physicalPath) ? { resourcePhysicalPath: asOptionalString(hit.physicalPath) } : {}),
			...(hit.stale === true ? { resourceStale: true } : {}),
			...(addonGuid ? { addonGuid } : {}),
			...(asOptionalString(hit.addonId) ? { addonLabel: asOptionalString(hit.addonId) } : {}),
			...(thumbnailColor ? { thumbnailColor } : {}),
		}];
	});
}

export function normalizeSourceRelationshipPage(value: unknown): SearchHit[] {
	const results = asRecord(value).results;
	if (!Array.isArray(results)) {
		return [];
	}
	return results.flatMap((entry, index) => {
		const hit = asRecord(entry);
		const sourceValue = asString(hit.source, '');
		if (sourceValue !== 'workspace' && sourceValue !== 'gameData') {
			return [];
		}
		const relationshipKind = asString(hit.relationshipKind, '');
		if (!isSearchRelationshipKind(relationshipKind)) {
			return [];
		}
		return normalizeSymbolHit(sourceValue, {
			...hit,
			matchKind: relationshipKind,
		}, index).map(result => ({
			...result,
			relationshipKind,
			relationshipDistance: Math.max(0, asNumber(hit.distance, 0)),
			relationshipEvidence: asOptionalString(hit.evidence),
		}));
	});
}

function isSearchRelationshipKind(value: string): value is SearchRelationshipKind {
	return ['direct', 'directBase', 'derivedType', 'moddedExtension', 'overriddenDeclaration', 'override'].includes(value);
}

export function formatSearchKind(kind: string): string {
	if (kind === 'method' || kind === 'constructor' || kind === 'destructor') {
		return 'function';
	}
	return kind.replace(/([a-z])([A-Z])/g, '$1 $2');
}

function normalizeSymbolHit(source: SearchSource, hit: RecordValue, index: number): SearchHit[] {
	const name = asString(hit.name, 'Unnamed symbol');
	const rawKind = asString(hit.kind, '');
	const kind = formatSearchKind(rawKind || 'Symbol');
	const qualifiedName = asString(hit.qualifiedName, name);
	const relativePath = asString(hit.relativePath, 'Unknown source');
	const range = asRecord(hit.declarationRange);
	const line = asNumber(range.startLine, 0);
	const signature = asString(hit.signature, qualifiedName);
	const documentation = typeof hit.documentationSummary === 'string' ? hit.documentationSummary : '';
	const excerpt = documentation ? `${signature}\n\n${documentation}` : signature;
	const readInput = asRecord(hit.readSourceInput);
	const addonGuid = asOptionalString(hit.addonGuid);
	const addonLabel = normalizedAddonLabel(hit);
	const thumbnailColor = asThumbnailColor(hit.thumbnailColor);
	const symbolRef = asOptionalString(hit.symbolRef);
	if (!readInput.relativePath) {
		return [];
	}
	return [{
		id: `${source}-${addonGuid ? `${addonGuid}-` : ''}${index}-${asString(hit.symbolRef, name)}`,
		source,
		kind: 'symbol',
		title: name,
		detail: kind,
		qualifiedName,
		signature,
		path: `${relativePath}:${line}`,
		excerpt,
		matchKind: asString(hit.matchKind, 'symbol'),
		...(isSearchSymbolKind(rawKind) ? { symbolKind: rawKind } : {}),
		...(symbolRef ? { symbolRef } : {}),
		...(typeof hit.sourceUri === 'string' ? { sourceUri: hit.sourceUri } : {}),
		selectionStartLine: asNumber(asRecord(hit.selectionRange).startLine, line),
		selectionEndLine: asNumber(asRecord(hit.selectionRange).endLine, line),
		readInput,
		...(addonGuid ? { addonGuid } : {}),
		...(addonLabel ? { addonLabel } : {}),
		...(thumbnailColor ? { thumbnailColor } : {}),
	}];
}

function isSearchSymbolKind(value: string): value is SearchSymbolKind {
	return searchSymbolKinds.includes(value as SearchSymbolKind);
}

function normalizeTextHit(source: SearchSource, hit: RecordValue, index: number): SearchHit[] {
	const relativePath = asString(hit.relativePath, 'Unknown source');
	const range = asRecord(hit.matchRange);
	const startLine = asNumber(range.startLine, 0);
	const rawExcerpt = asString(hit.excerpt, '');
	const leadingWhitespace = rawExcerpt.length - rawExcerpt.trimStart().length;
	const excerpt = rawExcerpt.trimStart().trimEnd();
	const matchText = asString(hit.matchText, '');
	const readInput = asRecord(hit.readSourceInput);
	const addonGuid = asOptionalString(hit.addonGuid);
	const addonLabel = normalizedAddonLabel(hit);
	const thumbnailColor = asThumbnailColor(hit.thumbnailColor);
	if (!readInput.relativePath) {
		return [];
	}
	const matchStart = Math.max(0, asNumber(hit.excerptMatchStart, asNumber(range.startCharacter, 0)) - leadingWhitespace);
	return [{
		id: `${source}-text-${addonGuid ? `${addonGuid}-` : ''}${index}-${relativePath}-${startLine}`,
		source,
		kind: 'text',
		title: matchText || 'Text match',
		detail: 'text',
		path: `${relativePath}:${startLine}`,
		excerpt,
		matchKind: 'text',
		...(typeof hit.sourceUri === 'string' ? { sourceUri: hit.sourceUri } : {}),
		selectionStartLine: startLine,
		selectionEndLine: asNumber(range.endLine, startLine),
		readInput,
		...(addonGuid ? { addonGuid } : {}),
		...(addonLabel ? { addonLabel } : {}),
		...(thumbnailColor ? { thumbnailColor } : {}),
		textMatchStart: matchStart,
		textMatchLength: matchText.length,
	}];
}

function normalizedAddonLabel(hit: RecordValue): string | undefined {
	const label = asOptionalString(hit.addonLabel);
	return label ? addonScopeLabel(label, '', label) : undefined;
}

function normalizeWikiHit(hit: RecordValue, index: number): SearchHit[] {
	const title = asString(hit.title, 'Official Wiki');
	const relativePath = asString(hit.relativePath, 'Official Wiki');
	const line = hit.matchedLine ?? hit.startLine;
	const readInput = asRecord(hit.readInput);
	if (!readInput.relativePath) {
		return [];
	}
	return [{
		id: `wiki-${index}-${relativePath}`,
		source: 'wiki',
		kind: 'documentation',
		title,
		detail: `${asString(hit.heading, 'Documentation')} · ${asString(hit.matchKind, 'match')}`,
		path: `${relativePath}:${typeof line === 'number' ? line : 0}`,
		excerpt: asString(hit.excerpt, ''),
		matchKind: asString(hit.matchKind, 'body'),
		selectionStartLine: asNumber(hit.matchedLine, asNumber(hit.excerptStartLine, asNumber(line, 0))),
		selectionEndLine: asNumber(hit.matchedLine, asNumber(hit.excerptEndLine, asNumber(line, 0))),
		sourceUrl: typeof hit.sourceUrl === 'string' ? hit.sourceUrl : undefined,
		readInput,
	}];
}

function searchErrorMessage(source: SearchSource, error: unknown): string {
	const label = source === 'wiki' ? 'Official Wiki' : source === 'gameData' ? 'Game Data' : 'workspace';
	return `${label} search unavailable: ${error instanceof Error ? error.message : String(error)}`;
}

function asRecord(value: unknown): RecordValue {
	return value && typeof value === 'object' && !Array.isArray(value) ? value as RecordValue : {};
}

function asString(value: unknown, fallback: string): string {
	return typeof value === 'string' && value.length > 0 ? value : fallback;
}

function asOptionalString(value: unknown): string | undefined {
	return typeof value === 'string' && value.length > 0 ? value : undefined;
}

function asNumber(value: unknown, fallback: number): number {
	return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function numberRecord(value: unknown): Record<string, number> {
	const record = asRecord(value);
	return Object.fromEntries(
		Object.entries(record)
			.filter((entry): entry is [string, number] => typeof entry[1] === 'number' && Number.isFinite(entry[1])),
	);
}

function isWithinRoot(root: string, candidate: string): boolean {
	const normalizedRoot = path.resolve(root).toLowerCase();
	const normalizedCandidate = candidate.toLowerCase();
	return normalizedCandidate === normalizedRoot
		|| normalizedCandidate.startsWith(`${normalizedRoot}${path.sep}`);
}
