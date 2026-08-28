import * as path from 'node:path';
import { performance } from 'node:perf_hooks';
import * as vscode from 'vscode';
import { diagnostic, diagnosticsEnabled } from '../diagnostics/diagnostics';
import { gameDataStorage } from '../extensionConfig/gameData';
import {
	loadedAddonSourceInventoryIsConfirmed,
	onDidConfirmLoadedAddonSourceInventory,
} from '../gameData/localSourceInventory';
import { languageClientIndexCache, languageClientSchemes } from '../extensionConfig/languageClient';
import { searchCommands, searchContext, searchLimits } from '../extensionConfig/search';
import {
	readWorkbenchWinePrefix,
	workbenchConfig,
} from '../extensionConfig/workbench';
import {
	provideLanguageServerPreviewContext,
	provideLanguageServerSemanticTokens,
	type LanguageServerPreviewContext,
} from '../languageClient/languageClient';
import { discoverWorkspaceProjectFiles, discoverWorkspaceScriptRoots } from '../languageClient/workspaceWatchBridge';
import { resolveLanguageServerPath } from '../languageClient/serverPath';
import { readExternalIndexMode } from '../mcp/mcpConfiguration';
import { semanticPreviewForLine, semanticPreviewForLines, type SemanticPreview } from './semanticPreview';
import {
	McpSearchClient,
	McpToolError,
	type SearchMode,
	type TextSearchOptions,
	searchKindFilters,
	searchResourceKindFilters,
	resourceKindsFor,
	sourceContextPreview,
	sourcePreviewLine,
	sourceMatchRange,
	stripSourceComments,
	type SearchDocument,
	type SearchHit,
	type SearchRelationshipKind,
	type SearchSymbolKind,
	type SearchSource,
	type SourceMatchRange,
	type WorkbenchProjectContext,
} from './mcpSearchClient';

const searchScheme = languageClientSchemes.searchPreview;
const maxSearchDocuments = 32;
const sourcePreviewWorkerCount = 8;
const previewUpdateBatchSize = 4;
let activePanel: vscode.WebviewPanel | undefined;
let activeSearch: ActiveSearch | undefined;
let documentSequence = 0;
const searchDocuments = new Map<string, string>();

export const searchDocumentContentProvider: vscode.TextDocumentContentProvider = {
	provideTextDocumentContent: uri => {
		const key = uri.path.split('/')[1];
		return searchDocuments.get(key) ?? '';
	},
};

interface ActiveSearch {
	panel: vscode.WebviewPanel;
	client: Promise<McpSearchClient> | undefined;
	latestResults: Map<string, SearchHit>;
	requestSequence: number;
	semanticDocuments: Map<string, Promise<SemanticSourceDocument | undefined>>;
	previewCancellation: vscode.CancellationTokenSource | undefined;
	searchInFlight: boolean;
	previewContextLines: number;
	latestQuery: string;
	scopeRefresh: Promise<void> | undefined;
	scopeRefreshPending: boolean;
	disposed: boolean;
}

interface SearchScopeRefreshState {
	scopeRefresh: Promise<void> | undefined;
	scopeRefreshPending: boolean;
	disposed: boolean;
}

interface SemanticSourceDocument {
	document: vscode.TextDocument;
	semanticTokens: vscode.SemanticTokens;
	startLine: number;
}

interface RawPreview {
	hit: SearchHit;
	document: SearchDocument;
	previewLine: number;
	preview: string;
	matchRange: SourceMatchRange | undefined;
	autoContext?: LanguageServerPreviewContext;
	semanticDocument?: SemanticSourceDocument;
}

interface RelationshipSearchState {
	anchor: {
		source: 'workspace' | 'gameData';
		symbolRef: string;
	};
	kinds: SearchRelationshipKind[];
	depth: 'one' | 'all';
}

export function localWorkbenchResourceLinkFor(hit: Pick<SearchHit, 'workbenchLink'>): string | undefined {
	return hit.workbenchLink?.startsWith('enfusion://') ? hit.workbenchLink : undefined;
}

export function resourceAddonIsLoaded(
	hit: Pick<SearchHit, 'kind' | 'addonLabel'>,
	loadedAddons: readonly string[],
): boolean {
	if (hit.kind !== 'resource' || !hit.addonLabel) {
		return false;
	}
	const addonId = hit.addonLabel.toLowerCase();
	return loadedAddons.some(loadedAddon => loadedAddon.toLowerCase() === addonId);
}

export function workbenchResourceOpenState(
	hit: Pick<SearchHit, 'kind' | 'addonLabel'>,
	context: WorkbenchProjectContext,
): 'loaded' | 'not-loaded' | 'unconfirmed' {
	if (resourceAddonIsLoaded(hit, context.loadedAddons)) {
		return 'loaded';
	}
	return !hit.addonLabel || context.loadedAddonsTruncated ? 'unconfirmed' : 'not-loaded';
}

export function resourcePathForClipboard(
	hit: Pick<SearchHit, 'kind' | 'resourceName'>,
): string | undefined {
	return hit.kind === 'resource' ? hit.resourceName : undefined;
}

export function resourcePhysicalPathFor(
	hit: Pick<SearchHit, 'kind' | 'resourcePhysicalPath'>,
): string | undefined {
	return hit.kind === 'resource' ? hit.resourcePhysicalPath : undefined;
}

export function registerSearchUi(context: vscode.ExtensionContext): void {
	diagnostic('searchUi.registered');
	void vscode.commands.executeCommand('setContext', searchContext.key, true);
	context.subscriptions.push(
		vscode.workspace.registerTextDocumentContentProvider(searchScheme, searchDocumentContentProvider),
		vscode.commands.registerCommand(searchCommands.open, () => openSearchPanel(context)),
		onDidConfirmLoadedAddonSourceInventory(() => {
			if (!activeSearch || activeSearch.disposed) {
				return;
			}
			diagnostic('searchUi.loadedAddonScopeChanged');
			void refreshSearchScope(context, activeSearch);
		}),
		new vscode.Disposable(() => {
			for (const key of searchDocuments.keys()) {
				searchDocuments.delete(key);
			}
		}),
	);
}

function openSearchPanel(context: vscode.ExtensionContext): void {
	if (activePanel) {
		activePanel.reveal(vscode.ViewColumn.One);
		void activePanel.webview.postMessage({ type: 'focusQuery' });
		return;
	}

	const panel = vscode.window.createWebviewPanel(
		'reforgerSearchUi',
		'Reforger Script Search',
		vscode.ViewColumn.One,
		{
			enableScripts: true,
			retainContextWhenHidden: true,
		},
	);
	const active: ActiveSearch = {
		panel,
		client: undefined,
		latestResults: new Map(),
		requestSequence: 0,
		semanticDocuments: new Map(),
		previewCancellation: undefined,
		searchInFlight: false,
		previewContextLines: 0,
		latestQuery: '',
		scopeRefresh: undefined,
		scopeRefreshPending: false,
		disposed: false,
	};
	activeSearch = active;
	activePanel = panel;
	panel.webview.html = renderSearchUi();
	const indexModeSubscription = vscode.workspace.onDidChangeConfiguration(event => {
		if (event.affectsConfiguration(`${workbenchConfig.section}.${workbenchConfig.settings.externalIndexMode}`)) {
			void refreshSearchScope(context, active);
		}
	});
	panel.webview.onDidReceiveMessage(message => {
		void handleMessage(context, active, message);
	}, undefined, context.subscriptions);
	panel.onDidDispose(() => {
		indexModeSubscription.dispose();
		active.disposed = true;
		active.previewCancellation?.cancel();
		active.previewCancellation?.dispose();
		if (active.client) {
			void active.client.then(client => client.dispose(), () => undefined);
		}
		if (activePanel === panel) {
			activePanel = undefined;
		}
		if (activeSearch === active) {
			activeSearch = undefined;
		}
	}, undefined, context.subscriptions);
}

async function restartSearchScope(
	context: vscode.ExtensionContext,
	active: ActiveSearch,
): Promise<void> {
	active.previewCancellation?.cancel();
	active.previewCancellation?.dispose();
	active.previewCancellation = undefined;
	active.searchInFlight = false;
	active.requestSequence += 1;
	active.latestResults.clear();
	active.semanticDocuments.clear();
	const previousClient = active.client;
	active.client = undefined;
	if (previousClient) {
		try {
			(await previousClient).dispose();
		} catch {
			// A failed or already-closed process is replaced below.
		}
	}
	await publishSearchScope(context, active, true);
}

export async function queueSearchScopeRefresh(
	state: SearchScopeRefreshState,
	refresh: () => Promise<void>,
): Promise<void> {
	state.scopeRefreshPending = true;
	if (state.scopeRefresh) {
		await state.scopeRefresh;
		return;
	}

	const queuedRefresh = Promise.resolve().then(async () => {
		do {
			state.scopeRefreshPending = false;
			await refresh();
		} while (state.scopeRefreshPending && !state.disposed);
	});
	state.scopeRefresh = queuedRefresh;
	try {
		await queuedRefresh;
	} finally {
		if (state.scopeRefresh === queuedRefresh) {
			state.scopeRefresh = undefined;
		}
	}
}

async function refreshSearchScope(
	context: vscode.ExtensionContext,
	active: ActiveSearch,
): Promise<void> {
	if (active.scopeRefresh) {
		diagnostic('searchUi.scopeRefreshQueued');
	}
	await queueSearchScopeRefresh(active, () => restartSearchScope(context, active));
}

async function handleMessage(
	context: vscode.ExtensionContext,
	active: ActiveSearch,
	message: unknown,
): Promise<void> {
	if (!isRecord(message) || typeof message.type !== 'string' || active.disposed) {
		return;
	}
	if (message.type === 'webviewReady') {
		diagnostic('searchUi.webviewReady', {
			width: numberField(message.width),
			height: numberField(message.height),
			devicePixelRatio: numberField(message.devicePixelRatio),
		});
		void publishSearchScope(context, active, false);
		return;
	}
	if (message.type === 'webviewError') {
		diagnostic('searchUi.webviewError', {
			message: textField(message.message),
			source: textField(message.source),
			line: numberField(message.line),
			column: numberField(message.column),
		});
		return;
	}
	if (message.type === 'debugSnapshot') {
		logSearchSnapshot(message.snapshot);
		return;
	}
	if (message.type === 'previewContext') {
		const contextLines = Math.max(0, Math.min(249, Math.floor(numberField(message.contextLines) ?? 0)));
		active.previewContextLines = contextLines;
		if (active.latestResults.size > 0) {
			const client = await getClient(context, active);
			void startPreviewHydration(active, client, active.requestSequence, [...active.latestResults.values()], active.latestQuery);
		}
		return;
	}
	if (message.type === 'search' && typeof message.query === 'string') {
		if (!isSearchKindValue(message.resultType) && !isSearchResourceKindValue(message.resultType)) {
			return;
		}
		const searchMode: SearchMode = message.searchMode === 'text'
			? 'text'
			: message.searchMode === 'resource' ? 'resource' : 'semantic';
		const textOptions: TextSearchOptions = {
			matchCase: message.matchCase === true,
			matchWholeWord: message.matchWholeWord === true,
			useRegex: message.useRegex === true,
		};
		const relationship = relationshipSearchState(message);
		await runSearch(
			context,
			active,
			message.query,
			searchMode,
			textOptions,
			message.scopeIds,
			message.resultType,
			numberField(message.page) ?? 1,
			numberField(message.pageSize) ?? 25,
			relationship,
		);
		return;
	}
	if (message.type === 'open' && typeof message.id === 'string') {
		await openSearchResult(active, message.id);
		return;
	}
	if (message.type === 'copyResourcePath' && typeof message.id === 'string') {
		const hit = active.latestResults.get(message.id);
		const resourcePath = hit ? resourcePathForClipboard(hit) : undefined;
		if (resourcePath) {
			await vscode.env.clipboard.writeText(resourcePath);
			vscode.window.setStatusBarMessage('Copied Reforger resource path.', 2_000);
		}
		return;
	}
	if (message.type === 'revealResource' && typeof message.id === 'string') {
		const hit = active.latestResults.get(message.id);
		const physicalPath = hit ? resourcePhysicalPathFor(hit) : undefined;
		if (physicalPath) {
			await vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(physicalPath));
		}
		return;
	}
	if (message.type === 'external' && typeof message.id === 'string') {
		const hit = active.latestResults.get(message.id);
		if (hit?.sourceUrl) {
			await vscode.env.openExternal(vscode.Uri.parse(hit.sourceUrl));
		}
	}
}

async function publishSearchScope(
	context: vscode.ExtensionContext,
	active: ActiveSearch,
	refreshSearch: boolean,
): Promise<void> {
	try {
		const client = await getClient(context, active);
		const scope = await client.discoverScope();
		if (active.disposed) {
			return;
		}
		diagnostic('searchUi.scopeDiscovered', {
			scopeRevision: scope.scopeRevision,
			scopeAuthority: scope.scopeAuthority,
			sourceCount: scope.sources.length,
			unavailableScopeIds: jsonField(scope.unavailableScopeIds),
			discoveryMs: scope.discoveryMs,
		});
		active.panel.webview.postMessage({ type: 'scope', requestId: active.requestSequence, scope, refreshSearch });
	} catch (error) {
		diagnostic('searchUi.scopeDiscoveryFailed', {
			message: error instanceof Error ? error.message : String(error),
		});
	}
}

function logSearchSnapshot(value: unknown): void {
	if (!diagnosticsEnabled()) {
		void vscode.window.showInformationMessage(
			'Search snapshot not logged. Enable Reforger Script Tools diagnostics first.',
		);
		return;
	}
	const snapshot = isRecord(value) ? value : {};
	const viewport = isRecord(snapshot.viewport) ? snapshot.viewport : {};
	const warnings = snapshotWarnings(snapshot.warnings);
	const results = snapshotResults(snapshot.results);
	diagnostic('searchUi.snapshot', {
		query: textField(snapshot.query),
		scopeOpen: snapshot.scopeOpen === true,
		scopeFilter: textField(snapshot.scopeFilter),
		scopeRevision: textField(snapshot.scopeRevision),
		scopeAuthority: textField(snapshot.scopeAuthority),
		scopeDiscoveryMs: numberField(snapshot.scopeDiscoveryMs),
		availableScopeIds: jsonField(Array.isArray(snapshot.availableScopeIds) ? snapshot.availableScopeIds.slice(0, 256) : []),
		unavailableScopeIds: jsonField(Array.isArray(snapshot.unavailableScopeIds) ? snapshot.unavailableScopeIds.slice(0, 256) : []),
		selectedScopeIds: jsonField(Array.isArray(snapshot.selectedScopeIds) ? snapshot.selectedScopeIds.slice(0, 256) : []),
		modeEligibleScopeIds: jsonField(Array.isArray(snapshot.modeEligibleScopeIds) ? snapshot.modeEligibleScopeIds.slice(0, 256) : []),
		removedScopeIds: jsonField(Array.isArray(snapshot.removedScopeIds) ? snapshot.removedScopeIds.slice(0, 256) : []),
		searchMode: textField(snapshot.searchMode),
		matchCase: snapshot.matchCase === true,
		matchWholeWord: snapshot.matchWholeWord === true,
		useRegex: snapshot.useRegex === true,
		resultType: textField(snapshot.resultType),
		resultLayout: textField(snapshot.resultLayout),
		status: textField(snapshot.status),
		requestId: numberField(snapshot.requestId),
		page: numberField(snapshot.page),
		pageSize: numberField(snapshot.pageSize),
		previewContextLines: numberField(snapshot.previewContextLines),
		total: numberField(snapshot.total),
		totalBySource: jsonField(snapshotTotalsBySource(snapshot.totalBySource)),
		totalPages: numberField(snapshot.totalPages),
		resultCount: numberField(snapshot.resultCount),
		visibleResultCount: numberField(snapshot.visibleResultCount),
		selectedId: textField(snapshot.selectedId),
		warningCount: Array.isArray(snapshot.warnings) ? snapshot.warnings.length : 0,
		warningsTruncated: Array.isArray(snapshot.warnings) && snapshot.warnings.length > 20,
		warnings: jsonField(warnings),
		error: textField(snapshot.error),
		searchPerformance: jsonField(snapshotPerformance(snapshot.searchPerformance)),
		previewPerformance: jsonField(snapshotPerformance(snapshot.previewPerformance)),
		uiPerformance: jsonField(snapshotPerformance(snapshot.uiPerformance)),
		viewportWidth: numberField(viewport.width),
		viewportHeight: numberField(viewport.height),
		devicePixelRatio: numberField(viewport.devicePixelRatio),
		resultMetadataCount: results.length,
		resultsTruncated: Array.isArray(snapshot.results) && snapshot.results.length > 100,
		results: jsonField(results),
	});
	void vscode.window.showInformationMessage('Search snapshot logged to extension diagnostics.');
}

async function runSearch(
	context: vscode.ExtensionContext,
	active: ActiveSearch,
	query: string,
	mode: SearchMode,
	textOptions: TextSearchOptions,
	scopeValue: unknown,
	typeValue: string,
	page: number,
	pageSize: number,
	relationship?: RelationshipSearchState,
): Promise<void> {
	cancelInFlightSearch(active);
	active.previewCancellation?.cancel();
	active.previewCancellation?.dispose();
	active.previewCancellation = undefined;
	const normalizedQuery = mode === 'text' ? query : query.trim();
	active.latestQuery = normalizedQuery;
	const requestId = ++active.requestSequence;
	const startedAt = Date.now();
	diagnostic('searchUi.searchStarted', {
		requestId,
		queryLength: normalizedQuery.length,
		scopeIds: jsonField(scopeIdsFor(scopeValue)),
		mode,
		matchCase: textOptions.matchCase,
		matchWholeWord: textOptions.matchWholeWord,
		useRegex: textOptions.useRegex,
		page,
		pageSize,
	});
	const categorySearch = mode === 'resource' || (mode === 'semantic' && typeValue !== 'all');
	if (!normalizedQuery && !relationship && !categorySearch) {
		active.latestResults.clear();
		active.panel.webview.postMessage({ type: 'results', requestId, results: [], warnings: [], total: 0, truncated: false, totalBySource: {}, page, pageSize });
		return;
	}

	active.searchInFlight = true;
	active.panel.webview.postMessage({ type: 'loading', requestId });
	try {
		const client = await getClient(context, active);
		const result = relationship && mode === 'semantic'
			? await client.searchRelationships({
				anchor: relationship.anchor,
				selectedScopeIds: scopeIdsFor(scopeValue),
				relationshipKinds: relationship.kinds,
				depth: relationship.depth,
				pageSize,
				page,
				symbolKinds: searchKindsFor(typeValue),
			})
			: await client.search(
				normalizedQuery,
				scopeIdsFor(scopeValue),
				pageSize,
				page,
				mode === 'semantic' ? searchKindsFor(typeValue) : undefined,
				mode,
				textOptions,
				mode === 'resource' ? resourceKindsFor(typeValue) : undefined,
			);
		if (active.disposed || requestId !== active.requestSequence) {
			return;
		}
		active.latestResults = new Map(result.results.map(hit => [hit.id, hit]));
		diagnostic('searchUi.searchCompleted', {
			requestId,
			resultCount: result.results.length,
			warningCount: result.warnings.length,
			page: result.page,
			pageSize: result.pageSize,
			total: result.total,
			truncated: result.truncated,
			performance: jsonField(result.performance),
			elapsedMs: Date.now() - startedAt,
		});
		active.panel.webview.postMessage({
			type: 'results',
			requestId,
			results: result.results,
			warnings: result.warnings,
			total: result.total,
			totalBySource: result.totalBySource,
			page: result.page,
			pageSize: result.pageSize,
			performance: result.performance,
		});
		void startPreviewHydration(active, client, requestId, result.results, normalizedQuery);
	} catch (error) {
		if (!active.disposed && requestId === active.requestSequence) {
			const clearRelationshipAnchor = Boolean(relationship && error instanceof McpToolError && [
				'invalid_relationship_anchor',
				'stale_relationship_anchor',
				'source_relationships_unavailable',
			].includes(error.code ?? ''));
			diagnostic('searchUi.searchFailed', {
				requestId,
				elapsedMs: Date.now() - startedAt,
				message: error instanceof Error ? error.message : String(error),
			});
			active.panel.webview.postMessage({
				type: 'error',
				requestId,
				message: error instanceof Error ? error.message : String(error),
				recovery: error instanceof McpToolError ? error.recovery : undefined,
				clearRelationshipAnchor,
			});
		}
	} finally {
		if (requestId === active.requestSequence) {
			active.searchInFlight = false;
		}
	}
}

function startPreviewHydration(active: ActiveSearch, client: McpSearchClient, requestId: number, hits: SearchHit[], query: string): Promise<void> {
	active.previewCancellation?.cancel();
	active.previewCancellation?.dispose();
	const previewCancellation = new vscode.CancellationTokenSource();
	active.previewCancellation = previewCancellation;
	return hydrateSearchPreviews(active, client, requestId, hits, query, active.previewContextLines, previewCancellation.token)
		.finally(() => {
			if (active.previewCancellation === previewCancellation) {
				active.previewCancellation = undefined;
			}
			previewCancellation.dispose();
		});
}

function cancelInFlightSearch(active: ActiveSearch): void {
	if (!active.searchInFlight) {
		return;
	}
	active.searchInFlight = false;
	const previousClient = active.client;
	active.client = undefined;
	if (previousClient) {
		void previousClient.then(client => client.dispose(), () => undefined);
	}
}

async function hydrateSearchPreviews(
	active: ActiveSearch,
	client: McpSearchClient,
	requestId: number,
	hits: SearchHit[],
	query: string,
	contextLines: number,
	cancellationToken: vscode.CancellationToken,
): Promise<void> {
	const symbolHits = hits.filter(hit => hit.kind === 'symbol');
	const textHits = hits.filter(hit => hit.kind === 'text');
	const previewHits = [...symbolHits, ...textHits];
	const semanticHits = previewHits.filter(hit => hit.source !== 'wiki');
	if (previewHits.length === 0) {
		return;
	}
	const startedAt = Date.now();
	const previews: Record<string, string> = {};
	const matchRanges: Record<string, SourceMatchRange> = {};
	const semanticPreviews: Record<string, SemanticPreview> = {};
	const rawPreviews = new Map<string, RawPreview>();
	const previewDiagnostics: Array<Record<string, unknown>> = [];
	let readMs = 0;
	const readMsByAddon: Record<string, number> = {};
	const readFailuresByAddon: Record<string, number> = {};
	let semanticMs = 0;
	let semanticPhaseStartedAt: number | undefined;
	let firstSemanticMs: number | undefined;
	let rawMs: number | undefined;
	let nextIndex = 0;
	let firstRawMs: number | undefined;
	const pendingRawPreviewIds: string[] = [];
	const pendingSemanticPreviewIds: string[] = [];
	const flushRawPreviews = (): void => {
		if (pendingRawPreviewIds.length === 0 || active.disposed || requestId !== active.requestSequence) {
			return;
		}
		const ids = pendingRawPreviewIds.splice(0, pendingRawPreviewIds.length);
		const elapsedMs = Date.now() - startedAt;
		firstRawMs ??= elapsedMs;
		active.panel.webview.postMessage({
			type: 'previews',
			requestId,
			previews: Object.fromEntries(ids.map(id => [id, previews[id]])),
			matches: Object.fromEntries(ids.flatMap(id => matchRanges[id] ? [[id, matchRanges[id]]] : [])),
			performance: {
				phase: 'raw',
				totalMs: elapsedMs,
				rawMs: elapsedMs,
				firstRawMs,
				readMs,
				readMsByAddon,
				readFailuresByAddon,
				semanticMs: 0,
				requestedCount: previewHits.length,
				loadedCount: Object.keys(previews).length,
				semanticCount: 0,
			},
		});
	};
	const queueRawPreview = (id: string): void => {
		pendingRawPreviewIds.push(id);
		if (pendingRawPreviewIds.length >= previewUpdateBatchSize) {
			flushRawPreviews();
		}
	};
	const flushSemanticPreviews = (): void => {
		if (pendingSemanticPreviewIds.length === 0 || active.disposed || cancellationToken.isCancellationRequested || requestId !== active.requestSequence) {
			return;
		}
		const ids = pendingSemanticPreviewIds.splice(0, pendingSemanticPreviewIds.length);
		firstSemanticMs ??= Date.now() - startedAt;
		active.panel.webview.postMessage({
			type: 'semanticPreviews',
			requestId,
			previews: Object.fromEntries(ids.map(id => [id, semanticPreviews[id]])),
			performance: {
				phase: 'semantic',
				totalMs: Date.now() - startedAt,
				rawMs: rawMs ?? Date.now() - startedAt,
				firstSemanticMs,
				readMs,
				readMsByAddon,
				readFailuresByAddon,
				semanticMs,
				requestedCount: previewHits.length,
				loadedCount: rawPreviews.size,
				semanticCount: Object.keys(semanticPreviews).length,
			},
		});
	};
	const postSemanticUpdate = (id: string): void => {
		pendingSemanticPreviewIds.push(id);
		if (pendingSemanticPreviewIds.length >= previewUpdateBatchSize) {
			flushSemanticPreviews();
		}
	};
	const hydrateSemanticPreview = async (rawPreview: RawPreview): Promise<void> => {
		if (active.disposed || cancellationToken.isCancellationRequested || requestId !== active.requestSequence) {
			return;
		}
		const { hit, document, previewLine, preview, matchRange, autoContext } = rawPreview;
		try {
			const semanticStartedAt = performance.now();
			const semanticDocument = rawPreview.semanticDocument
				?? await semanticSourceDocument(active, client, hit, document, cancellationToken);
			semanticMs += performance.now() - semanticStartedAt;
			if (cancellationToken.isCancellationRequested || requestId !== active.requestSequence) {
				return;
			}
			let semanticPreview: SemanticPreview | undefined;
			if (semanticDocument) {
				const line = Math.max(0, previewLine - semanticDocument.startLine);
				semanticPreview = active.previewContextLines === 0 && autoContext
					? semanticPreviewForLines(
						semanticDocument.document,
						semanticDocument.semanticTokens,
						autoContext.startLine,
						autoContext.endLine,
						hit.kind === 'text',
					)
					: active.previewContextLines <= 1
					? semanticPreviewForLine(semanticDocument.document, semanticDocument.semanticTokens, line, hit.kind === 'text')
					: semanticPreviewForLines(
						semanticDocument.document,
						semanticDocument.semanticTokens,
						Math.max(0, line - active.previewContextLines),
						Math.min(semanticDocument.document.lineCount - 1, line + active.previewContextLines),
						hit.kind === 'text',
					);
				if (semanticPreview) {
					semanticPreviews[hit.id] = semanticPreview;
					if (hit.kind === 'symbol' && semanticPreview.text === preview) {
						const semanticMatchRange = sourceMatchRange(semanticPreview.text, query);
						if (semanticMatchRange) {
							matchRanges[hit.id] = semanticMatchRange;
						}
					}
					postSemanticUpdate(hit.id);
				}
			}
			const displayedMatchRange = matchRanges[hit.id];
			const semanticMatchRange = semanticPreview ? sourceMatchRange(semanticPreview.text, query) : undefined;
			previewDiagnostics.push({
				id: hit.id,
				title: hit.title,
				path: hit.path,
				selectionStartLine: hit.selectionStartLine,
				previewLine,
				previewText: preview.slice(0, 500),
				rawMatchStart: matchRange?.start,
				rawMatchLength: matchRange?.length,
				rawMatchText: matchRange ? preview.slice(matchRange.start, matchRange.start + matchRange.length) : undefined,
				matchStart: displayedMatchRange?.start,
				matchLength: displayedMatchRange?.length,
				matchText: displayedMatchRange ? preview.slice(displayedMatchRange.start, displayedMatchRange.start + displayedMatchRange.length) : undefined,
				displayedTextMatchesSemanticText: semanticPreview?.text === preview,
				semanticDocument: Boolean(semanticDocument),
				semanticLanguageId: semanticDocument?.document.languageId,
				semanticStartLine: semanticDocument?.startLine,
				autoContextKind: autoContext?.kind,
				autoContextStartLine: autoContext?.startLine,
				autoContextEndLine: autoContext?.endLine,
				autoContextTruncated: autoContext?.truncated,
				semanticPreviewText: semanticPreview?.text.slice(0, 500),
				semanticMatchStart: semanticMatchRange?.start,
				semanticMatchLength: semanticMatchRange?.length,
				semanticMatchText: semanticMatchRange
					? semanticPreview?.text.slice(semanticMatchRange.start, semanticMatchRange.start + semanticMatchRange.length)
					: undefined,
				semanticTokenCount: semanticPreview?.tokens.length ?? 0,
				semanticTokenRoles: semanticPreview ? [...new Set(semanticPreview.tokens.map(token => token.role))] : [],
				semanticEnabled: semanticPreview?.enabled,
				semanticForegrounds: semanticPreview?.foregrounds,
			});
		} catch (error) {
			if (!cancellationToken.isCancellationRequested) {
				previewDiagnostics.push({
					id: hit.id,
					title: hit.title,
					path: hit.path,
					phase: 'semantic-hydration-failed',
					message: error instanceof Error ? error.message : String(error),
				});
			}
		}
	};
	const semanticWorkerTails = Array.from(
		{ length: Math.min(4, semanticHits.length) },
		() => Promise.resolve(),
	);
	let nextSemanticWorker = 0;
	const queueSemanticPreview = (rawPreview: RawPreview): void => {
		if (semanticWorkerTails.length === 0 || rawPreview.hit.source === 'wiki') {
			return;
		}
		semanticPhaseStartedAt ??= Date.now();
		const worker = nextSemanticWorker;
		nextSemanticWorker = (nextSemanticWorker + 1) % semanticWorkerTails.length;
		semanticWorkerTails[worker] = semanticWorkerTails[worker]
			.then(() => hydrateSemanticPreview(rawPreview));
	};
	const readWorker = async (): Promise<void> => {
		while (nextIndex < previewHits.length && !active.disposed && requestId === active.requestSequence) {
			const hit = previewHits[nextIndex++];
			try {
				const readStartedAt = performance.now();
				const document = await client.read(hit, contextLines === 0 ? 1 : contextLines);
				const hitReadMs = performance.now() - readStartedAt;
				readMs += hitReadMs;
				if (hit.addonGuid) {
					readMsByAddon[hit.addonGuid] = (readMsByAddon[hit.addonGuid] ?? 0) + hitReadMs;
				}
				const previewLine = sourcePreviewLine(document, hit.selectionStartLine, hit.title);
				let semanticDocument: SemanticSourceDocument | undefined;
				let autoContext: LanguageServerPreviewContext | undefined;
				let preview = sourceContextPreview(document, previewLine, contextLines, hit.title);
				if (contextLines === 0 && hit.source !== 'wiki') {
					semanticDocument = await semanticSourceDocument(active, client, hit, document, cancellationToken);
					if (semanticDocument?.startLine === 1) {
						const requestedLine = Math.max(0, previewLine - 1);
						autoContext = await provideLanguageServerPreviewContext(
							semanticDocument.document,
							requestedLine,
							cancellationToken,
						);
						if (autoContext) {
							preview = sourceDocumentRangePreview(
								semanticDocument.document,
								autoContext.startLine,
								autoContext.endLine,
								hit.kind === 'text',
							);
						}
					}
				}
				previews[hit.id] = preview;
				const matchRange = sourceMatchRange(preview, query);
				if (matchRange) {
					matchRanges[hit.id] = matchRange;
				}
				const rawPreview = { hit, document, previewLine, preview, matchRange, autoContext, semanticDocument };
				rawPreviews.set(hit.id, rawPreview);
				queueRawPreview(hit.id);
				queueSemanticPreview(rawPreview);
			} catch (error) {
				if (hit.addonGuid) {
					readFailuresByAddon[hit.addonGuid] = (readFailuresByAddon[hit.addonGuid] ?? 0) + 1;
				}
				previewDiagnostics.push({
					id: hit.id,
					title: hit.title,
					path: hit.path,
					addonGuid: hit.addonGuid,
					phase: 'source-read-failed',
					message: error instanceof Error ? error.message : String(error),
				});
			}
		}
	};
	await Promise.all(Array.from({ length: Math.min(sourcePreviewWorkerCount, previewHits.length) }, () => readWorker()));
	flushRawPreviews();
	if (active.disposed || requestId !== active.requestSequence) {
		return;
	}
	rawMs = Date.now() - startedAt;
	diagnostic('searchUi.previewRawCompleted', {
		requestId,
		requestedCount: previewHits.length,
		loadedCount: rawPreviews.size,
		firstRawMs,
		lastRawMs: rawMs,
		readMs,
		readMsByAddon: jsonField(readMsByAddon),
		readFailuresByAddon: jsonField(readFailuresByAddon),
	});

	await Promise.all(semanticWorkerTails);
	if (active.disposed || requestId !== active.requestSequence) {
		return;
	}
	flushSemanticPreviews();
	const previewPerformance = {
		phase: 'complete',
		totalMs: Date.now() - startedAt,
		rawMs,
		firstSemanticMs,
		semanticWallMs: semanticPhaseStartedAt === undefined ? 0 : Date.now() - semanticPhaseStartedAt,
		readMs,
		readMsByAddon,
		readFailuresByAddon,
		semanticMs,
		requestedCount: previewHits.length,
		loadedCount: rawPreviews.size,
		semanticCount: Object.keys(semanticPreviews).length,
	};
	active.panel.webview.postMessage({ type: 'semanticPreviews', requestId, previews: {}, performance: previewPerformance });
	diagnostic('searchUi.previewHydrationCompleted', {
		requestId,
		requestedCount: previewHits.length,
		loadedCount: previewPerformance.loadedCount,
		semanticCount: previewPerformance.semanticCount,
		readMs: previewPerformance.readMs,
		readMsByAddon: jsonField(previewPerformance.readMsByAddon),
		readFailuresByAddon: jsonField(previewPerformance.readFailuresByAddon),
		semanticMs: previewPerformance.semanticMs,
		firstSemanticMs: previewPerformance.firstSemanticMs,
		semanticWallMs: previewPerformance.semanticWallMs,
		totalMs: previewPerformance.totalMs,
		rawMs: previewPerformance.rawMs,
		items: jsonField(previewDiagnostics.slice(0, 100)),
		elapsedMs: Date.now() - startedAt,
	});
}

function sourceDocumentRangePreview(
	document: vscode.TextDocument,
	startLine: number,
	endLine: number,
	preserveComments: boolean,
): string {
	const lines: string[] = [];
	for (let line = startLine; line <= endLine; line += 1) {
		const text = document.lineAt(line).text;
		lines.push((preserveComments ? text : stripSourceComments(text)).trimEnd());
	}
	return lines.join('\n');
}

async function semanticSourceDocument(
	active: ActiveSearch,
	client: McpSearchClient,
	hit: SearchHit,
	boundedDocument: SearchDocument,
	cancellationToken: vscode.CancellationToken,
): Promise<SemanticSourceDocument | undefined> {
	if (hit.source === 'wiki') {
		return undefined;
	}
	const cacheKey = hit.sourceUri ?? `${hit.source}:${String(hit.readInput.relativePath ?? hit.id)}`;
	const cached = active.semanticDocuments.get(cacheKey);
	if (cached) {
		const value = await cached;
		if (value || cancellationToken.isCancellationRequested) {
			return value;
		}
		if (active.semanticDocuments.get(cacheKey) === cached) {
			active.semanticDocuments.delete(cacheKey);
		}
	}
	if (cancellationToken.isCancellationRequested) {
		return undefined;
	}
	const pending = loadSemanticSourceDocument(client, hit, boundedDocument, cancellationToken)
		.catch(() => undefined);
	active.semanticDocuments.set(cacheKey, pending);
	const value = await pending;
	if (!value && active.semanticDocuments.get(cacheKey) === pending) {
		active.semanticDocuments.delete(cacheKey);
	}
	return value;
}

async function loadSemanticSourceDocument(
	client: McpSearchClient,
	hit: SearchHit,
	boundedDocument: SearchDocument,
	cancellationToken: vscode.CancellationToken,
): Promise<SemanticSourceDocument | undefined> {
	let document: vscode.TextDocument;
	let startLine = 1;
	try {
		if (hit.sourceUri) {
			document = await vscode.workspace.openTextDocument(vscode.Uri.parse(hit.sourceUri, true));
		} else {
			const sourcePath = await client.resolveSourcePath(hit);
			if (sourcePath) {
				document = await vscode.workspace.openTextDocument(vscode.Uri.file(sourcePath));
			} else if (hit.kind === 'text') {
				return undefined;
			} else {
				document = await vscode.workspace.openTextDocument({ content: boundedDocument.content, language: 'enforce' });
				startLine = boundedDocument.startLine > 0 ? boundedDocument.startLine : hit.selectionStartLine ?? 1;
			}
		}
		if (document.languageId !== 'enforce') {
			document = await vscode.languages.setTextDocumentLanguage(document, 'enforce');
		}
	} catch {
		return undefined;
	}
	const semanticTokens = await provideLanguageServerSemanticTokens(document, cancellationToken);
	return semanticTokens ? { document, semanticTokens, startLine } : undefined;
}

async function getClient(
	context: vscode.ExtensionContext,
	active: ActiveSearch,
): Promise<McpSearchClient> {
	if (!active.client) {
		active.client = createClient(context).catch(error => {
			active.client = undefined;
			throw error;
		});
	}
	return active.client;
}

async function createClient(context: vscode.ExtensionContext): Promise<McpSearchClient> {
	const serverPath = await resolveLanguageServerPath(context);
	if (!serverPath) {
		throw new Error('The bundled Reforger language server is not available yet.');
	}
	const addonSourceInventory = path.join(
		context.globalStorageUri.fsPath,
		gameDataStorage.rootFolder,
		gameDataStorage.inventoryFile,
	);
	const externalIndexMode = readExternalIndexMode();
	const authoritativeWorkbenchScope = externalIndexMode === 'loaded'
		&& loadedAddonSourceInventoryIsConfirmed(addonSourceInventory);
	return new McpSearchClient({
		serverPath,
		addonSourceInventory,
		addonIndexStorage: path.join(
			context.globalStorageUri.fsPath,
			languageClientIndexCache.rootFolder,
		),
		externalIndexMode,
		workspaceScripts: await discoverWorkspaceScriptRoots(),
		dependencyProjectFiles: authoritativeWorkbenchScope ? [] : await discoverWorkspaceProjectFiles(),
		officialWikiRoot: path.join(context.extensionPath, 'data', 'official-wiki'),
		workbenchWinePrefix: readWorkbenchWinePrefix(),
	});
}

async function openSearchResult(active: ActiveSearch, id: string): Promise<void> {
	const hit = active.latestResults.get(id);
	if (!hit) {
		return;
	}
	try {
		diagnostic('searchUi.resultOpenStarted', { source: hit.source, kind: hit.kind });
		if (hit.kind === 'resource' && !hit.readInput.relativePath) {
			const resourceLink = localWorkbenchResourceLinkFor(hit);
			if (!resourceLink) {
				throw new Error('The resource search result did not include a complete Workbench resource link.');
			}
			const client = await getClientFromActive(active);
			const projectContext = await client.workbenchProjectContext();
			const openState = workbenchResourceOpenState(hit, projectContext);
			if (openState !== 'loaded') {
				const addon = hit.addonLabel ?? 'for this resource';
				diagnostic('searchUi.resultOpenBlocked', {
					source: hit.source,
					addon,
					loadedAddonsTruncated: projectContext.loadedAddonsTruncated,
				});
				const reason = !hit.addonLabel
					? 'The resource result did not identify its owning add-on, so its loaded state could not be confirmed.'
					: openState === 'unconfirmed'
					? `The loaded state of add-on ${addon} could not be confirmed because Workbench returned an incomplete loaded add-on list.`
					: `The add-on ${addon} is not loaded in Workbench.`;
				await vscode.window.showWarningMessage(`${reason} ${hit.title} was not opened.`);
				return;
			}
			await vscode.env.openExternal(vscode.Uri.parse(resourceLink, true));
			diagnostic('searchUi.resultOpenCompleted', { source: hit.source, enfusionResourceLink: true });
			return;
		}
		const client = await getClientFromActive(active);
		const openedSource = await openSearchSourceDocument(client, hit);
		const documentWithLanguage = openedSource.document;
		if (hit.source === 'wiki') {
			await vscode.commands.executeCommand('markdown.showPreview', documentWithLanguage.uri);
			diagnostic('searchUi.resultOpenCompleted', {
				source: hit.source,
				physicalDocument: openedSource.physicalDocument,
				markdownPreview: true,
			});
			return;
		}
		const editor = hit.kind === 'resource'
			? await vscode.window.showTextDocument(documentWithLanguage, { preview: true })
			: await vscode.window.showTextDocument(documentWithLanguage);
		const range = selectionRange(documentWithLanguage, hit, openedSource.sourceRead?.startLine ?? 1);
		editor.selection = new vscode.Selection(range.start, range.end);
		editor.revealRange(range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
		diagnostic('searchUi.resultOpenCompleted', {
			source: hit.source,
			physicalDocument: openedSource.physicalDocument,
			markdownPreview: false,
		});
	} catch (error) {
		diagnostic('searchUi.resultOpenFailed', {
			source: hit.source,
			message: error instanceof Error ? error.message : String(error),
		});
		await vscode.window.showErrorMessage(
			`Could not open ${hit.title}: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
}

type SearchSourceReader = Pick<McpSearchClient, 'read' | 'readComplete' | 'resolveSourcePath'>;

export async function openSearchSourceDocument(
	client: SearchSourceReader,
	hit: SearchHit,
): Promise<{ document: vscode.TextDocument; sourceRead?: SearchDocument; physicalDocument: boolean }> {
	if (hit.sourceUri) {
		try {
			const uri = vscode.Uri.parse(hit.sourceUri, true);
			const document = await vscode.workspace.openTextDocument(uri);
			return {
				document: await setSearchDocumentLanguage(document, hit),
				physicalDocument: uri.scheme === 'file',
			};
		} catch {
			// The revision-bound source read remains authoritative when its editor provider is unavailable.
		}
	}
	const sourcePath = await client.resolveSourcePath(hit);
	if (sourcePath) {
		try {
			const document = await vscode.workspace.openTextDocument(vscode.Uri.file(sourcePath));
			return {
				document: await setSearchDocumentLanguage(document, hit),
				physicalDocument: true,
			};
		} catch {
			// Continue to the same revision-bound source read used for packed sources.
		}
	}
	const sourceRead = hit.source === 'wiki' ? await client.read(hit) : await client.readComplete(hit);
	const key = `document-${++documentSequence}`;
	while (searchDocuments.size >= maxSearchDocuments) {
		const oldest = searchDocuments.keys().next().value;
		if (oldest === undefined) {
			break;
		}
		searchDocuments.delete(oldest);
	}
	searchDocuments.set(key, sourceRead.content);
	const uri = vscode.Uri.parse(`${searchScheme}:/${key}/${encodeURIComponent(hit.path)}`);
	const document = await vscode.workspace.openTextDocument(uri);
	return {
		document: await setSearchDocumentLanguage(document, hit),
		sourceRead,
		physicalDocument: false,
	};
}

async function setSearchDocumentLanguage(document: vscode.TextDocument, hit: SearchHit): Promise<vscode.TextDocument> {
	const language = hit.source === 'wiki' ? 'markdown' : 'enforce';
	return document.languageId === language
		? document
		: vscode.languages.setTextDocumentLanguage(document, language);
}

async function getClientFromActive(active: ActiveSearch): Promise<McpSearchClient> {
	if (!active.client) {
		throw new Error('The search session is not ready.');
	}
	return active.client;
}


function scopeIdsFor(value: unknown): string[] {
	if (!Array.isArray(value)) {
		return [];
	}
	return value
		.filter((item): item is string => typeof item === 'string')
		.slice(0, 256);
}

function relationshipSearchState(message: Record<string, unknown>): RelationshipSearchState | undefined {
	if (!isRecord(message.relationshipAnchor)) {
		return undefined;
	}
	const source = message.relationshipAnchor.source;
	const symbolRef = message.relationshipAnchor.symbolRef;
	if ((source !== 'workspace' && source !== 'gameData') || typeof symbolRef !== 'string' || !symbolRef) {
		return undefined;
	}
	const kindMap: Record<string, SearchRelationshipKind> = {
		direct: 'direct',
		parents: 'directBase',
		children: 'derivedType',
		baseMembers: 'overriddenDeclaration',
		overrides: 'override',
		modded: 'moddedExtension',
	};
	const kinds = Array.isArray(message.relationIncludes)
		? [...new Set(message.relationIncludes.flatMap(value => typeof value === 'string' && kindMap[value] ? [kindMap[value]] : []))]
		: [];
	if (kinds.length === 0) {
		return undefined;
	}
	return {
		anchor: { source, symbolRef },
		kinds,
		depth: message.relationDepth === 'all' ? 'all' : 'one',
	};
}

function isSearchKindValue(value: unknown): value is string {
	return typeof value === 'string' && searchKindFilters.some(filter => filter.value === value);
}

function isSearchResourceKindValue(value: unknown): value is string {
	return typeof value === 'string' && searchResourceKindFilters.some(filter => filter.value === value);
}

function searchKindsFor(value: string): readonly SearchSymbolKind[] | undefined {
	return searchKindFilters.find(filter => filter.value === value)?.kinds;
}

function enfusionResourceUri(resourceName: string): vscode.Uri {
	return vscode.Uri.parse(`enfusion://${resourceName}`, true);
}

function selectionRange(
	document: vscode.TextDocument,
	hit: SearchHit,
	contentStartLine: number,
): vscode.Range {
	const startLine = Math.max(0, (hit.selectionStartLine ?? contentStartLine) - contentStartLine);
	const endLine = Math.min(
		document.lineCount - 1,
		Math.max(startLine, (hit.selectionEndLine ?? hit.selectionStartLine ?? contentStartLine) - contentStartLine),
	);
	return new vscode.Range(
		startLine,
		0,
		endLine,
		document.lineAt(endLine).text.length,
	);
}

function renderSearchUi(): string {
	const nonce = createNonce();
	return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
<title>Reforger Search</title>
<style>
:root { color-scheme: dark; --bg: var(--vscode-editor-background); --panel: var(--vscode-sideBar-background); --alt: var(--vscode-editorWidget-background); --border: var(--vscode-panel-border); --text: var(--vscode-foreground); --muted: var(--vscode-descriptionForeground); --accent: var(--vscode-textLink-foreground); --selected: var(--vscode-list-activeSelectionBackground); --selected-text: var(--vscode-list-activeSelectionForeground); }
* { box-sizing: border-box; }
body { margin: 0; color: var(--text); background: var(--bg); font: 13px var(--vscode-font-family); }
button, input, select { font: inherit; color: inherit; }
button { border: 1px solid var(--border); background: var(--alt); cursor: pointer; }
button:hover { border-color: var(--accent); }
.shell { min-height: 100vh; padding: 24px 28px 70px; }
h1, h2, h3, p { margin-top: 0; }
h1 { font-size: 24px; margin: 6px 0 8px; }
h2 { font-size: 16px; margin-bottom: 6px; }
h3 { font-size: 13px; margin: 0 0 4px; }
.group-label { padding: 0 4px 6px; color: var(--muted); font-size: 11px; font-weight: 700; letter-spacing: .07em; }
.search-scope { margin: 0 0 12px; padding: 8px; border: 1px solid var(--border); background: var(--alt); }
.search-scope .addon-trigger { display: flex; align-items: center; justify-content: space-between; gap: 8px; margin: 0; padding: 8px 9px; border: 1px solid var(--border); background: var(--panel); }
.addon-summary { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.addon-menu { position: absolute; z-index: 4; left: 10px; width: 290px; margin-top: 5px; padding: 9px; border: 1px solid var(--accent); background: var(--vscode-menu-background, var(--panel)); box-shadow: 0 8px 24px rgba(0, 0, 0, .35); }
.addon-menu.inline { position: static; width: auto; margin-top: 0; padding: 0; border: 0; box-shadow: none; background: transparent; }
.addon-filter { flex: 1 1 auto; min-width: 0; box-sizing: border-box; margin: 0; padding: 6px 7px; border: 1px solid var(--border); background: var(--alt); outline: none; }
.addon-filter:focus { border-color: var(--accent); }
.addon-choice { display: grid; grid-template-columns: 16px minmax(0, 1fr) auto; align-items: center; gap: 6px; min-height: 29px; color: var(--fg); cursor: pointer; }
.addon-choice input { margin: 0; accent-color: var(--accent); }
.addon-choice small { color: var(--muted); font-size: 10px; }
.addon-choice.pinned-boundary { margin-bottom: 5px; padding-bottom: 5px; border-bottom: 1px solid var(--border); }
.scope-actions { display: flex; align-items: center; gap: 6px; margin-bottom: 7px; }
.scope-actions button { flex: 0 0 auto; width: auto; margin: 0; padding: 5px 7px; color: var(--accent); font-size: 10px; white-space: nowrap; }
.scope-actions [data-scope-all] { flex: 0 0 76px; width: 76px; box-sizing: border-box; }
.page-controls { display: flex; align-items: center; justify-content: flex-end; flex-wrap: wrap; gap: 14px; }
.page-controls [data-result-layout] { display: inline-flex; align-items: center; justify-content: center; padding: 0; line-height: 0; }
.layout-toggle { display: inline-grid; width: 14px; height: 14px; grid-template-columns: repeat(2, 5px); grid-template-rows: repeat(2, 5px); place-content: center; gap: 2px; }
.layout-toggle span { display: block; border: 1px solid currentColor; }
.layout-toggle.layout-single { display: block; }
.layout-toggle.layout-single span:first-child { width: 12px; height: 12px; }
.layout-toggle.layout-single span:not(:first-child) { display: none; }
.layout-toggle.layout-rows { grid-template-columns: 14px; grid-template-rows: repeat(3, 2px); gap: 2px; }
.layout-toggle.layout-rows span { border: 0; background: currentColor; }
.layout-toggle.layout-rows span:last-child { display: none; }
.page-controls button.active { border-color: var(--accent); color: var(--accent); background: var(--selected); }
.page-status { display: inline-flex; flex: 0 0 150px; align-items: center; justify-content: flex-end; gap: 6px; white-space: nowrap; }
.page-arrows { display: inline-flex; gap: 2px; }
.page-controls button, .page-controls input, .page-controls select { min-height: 28px; }
.page-controls button { min-width: 28px; padding: 3px 7px; }
.page-controls input { width: 48px; padding: 3px 5px; text-align: center; border: 1px solid var(--border); background: var(--alt); outline: none; }
.page-controls input:focus, .page-controls select:focus { border-color: var(--accent); }
.page-controls select { padding: 3px 5px; border: 1px solid var(--border); background: var(--alt); }
.page-bottom { display: flex; justify-content: flex-end; margin-top: 12px; }
.muted { color: var(--muted); }
.tag { border-radius: 12px; padding: 3px 8px; background: var(--alt); color: var(--muted); font-size: 11px; }
.relationship-tag { border: 1px solid color-mix(in srgb, var(--result-accent) 45%, var(--line)); color: var(--result-accent); }
.result-detail, .result-path { display: block; color: var(--muted); font-size: 12px; }
.result-path { max-width: 50%; margin-left: auto; overflow-wrap: anywhere; text-align: right; }
.snippet { margin: 9px 0 0; padding: 10px; overflow: auto; background: var(--alt); border: 1px solid var(--border); font: 12px/1.5 var(--vscode-editor-font-family); white-space: pre-wrap; }
.snippet mark { padding: 0 2px; background: var(--vscode-editor-findMatchHighlightBackground); color: var(--vscode-editor-findMatchForeground); }
.md-preview { margin: 9px 0 0; padding: 10px 12px; overflow: auto; background: var(--alt); border: 1px solid var(--border); line-height: 1.5; }
.md-preview h1, .md-preview h2, .md-preview h3, .md-preview h4, .md-preview h5, .md-preview h6 { margin: 0 0 7px; font-size: 14px; }
.md-preview p { margin: 0 0 8px; }
.md-preview p:last-child, .md-preview ul:last-child, .md-preview blockquote:last-child { margin-bottom: 0; }
.md-preview ul { margin: 0 0 8px; padding-left: 20px; }
.md-preview blockquote { margin: 0 0 8px; padding-left: 10px; border-left: 2px solid var(--accent); color: var(--muted); }
.md-preview a { color: var(--accent); }
.md-preview code { padding: 1px 4px; background: var(--panel); font-family: var(--vscode-editor-font-family); }
.md-preview .md-code { margin: 0 0 8px; padding: 8px; overflow: auto; background: var(--panel); font: 12px/1.45 var(--vscode-editor-font-family); white-space: pre-wrap; }
.result-actions { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 10px; }
.result-actions button { padding: 5px 8px; }
.error { padding: 10px 12px; border: 1px solid var(--vscode-inputValidation-errorBorder); color: var(--vscode-errorForeground); background: var(--vscode-inputValidation-errorBackground); }
.warning { padding: 8px 10px; border-left: 2px solid var(--vscode-editorWarning-foreground); color: var(--muted); }
.empty { padding: 30px 14px; border: 1px dashed var(--border); color: var(--muted); }
.query-field { width: 100%; min-width: 0; padding: 10px 12px; border: 1px solid var(--border); background: var(--alt); outline: none; }
.query-field:focus { border-color: var(--accent); }
.control-buttons { display: flex; flex-wrap: wrap; gap: 5px; }
.control-buttons button { min-height: 38px; padding: 7px 12px; }
.control-buttons button.active { border-color: var(--accent); background: var(--selected); color: var(--selected-text); }
.control-block { min-width: 0; }
.control-block .group-label { min-height: 22px; padding: 0 0 7px; }
.search-atlas { padding: 26px 28px 72px; }
.atlas-groups { display: grid; gap: 14px; margin-top: 12px; }
.atlas-group { min-width: 0; border: 1px solid var(--border); background: var(--panel); }
.atlas-group-head { display: flex; align-items: center; justify-content: space-between; padding: 10px 12px; border-bottom: 3px solid var(--addon-header-color, var(--border)); background: var(--alt); }
.atlas-group-head h2 { margin: 0; font-size: 13px; }
.atlas-results { display: grid; grid-template-columns: minmax(0, 1fr); gap: 10px; margin-top: 12px; align-items: start; }
.atlas-results.masonry { display: block; column-count: 2; column-gap: 10px; }
.atlas-results.masonry .atlas-card { display: inline-block; width: 100%; margin: 0 0 10px; break-inside: avoid; }
.atlas-results.rows { --result-row-columns: minmax(150px, 28%) minmax(240px, 1fr) 180px 110px; display: grid; grid-template-columns: minmax(640px, 1fr); gap: 1px; overflow-x: auto; }
.atlas-results.rows .atlas-card { display: grid; min-width: 640px; grid-template-columns: var(--result-row-columns); align-items: center; gap: 12px; padding: 6px 10px; box-shadow: none; }
.atlas-results.rows .atlas-card-head { display: contents; }
.atlas-results.rows .atlas-card-head > strong { grid-column: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.atlas-results.rows .atlas-card.result-resource .atlas-card-head > strong { grid-column: 1 / 3; }
.atlas-results.rows .atlas-card-tools { grid-column: 3; grid-row: 1; flex-wrap: nowrap; }
.atlas-results.rows .atlas-card .result-path { grid-column: 2; grid-row: 1; margin: 0; overflow: hidden; text-overflow: ellipsis; overflow-wrap: normal; white-space: nowrap; }
.atlas-results.rows .snippet, .atlas-results.rows .md-preview { display: none; }
.atlas-results.rows .result-actions { grid-column: 4; grid-row: 1; flex-wrap: nowrap; margin: 0; }
.atlas-group .atlas-results { margin: 0; padding: 10px; background: var(--bg); }
.atlas-card { --result-accent: var(--accent); min-width: 0; padding: 12px 12px 8px; border: 1px solid var(--vscode-widget-border, var(--border)); border-left: 3px solid var(--result-accent); background: var(--panel); background: color-mix(in srgb, var(--result-accent) 7%, var(--panel)); box-shadow: 0 2px 8px rgba(0, 0, 0, .16); cursor: pointer; user-select: text; }
.atlas-card.result-class { --result-accent: #40b5ac; }
.atlas-card.result-function { --result-accent: #f3ad58; }
.atlas-card.result-field { --result-accent: var(--vscode-symbolIcon-fieldForeground, #9cdcfe); }
.atlas-card.result-enum { --result-accent: #40b5ac; }
.atlas-card.result-string { --result-accent: #c178dd; }
.atlas-card.result-resource { --result-accent: var(--vscode-symbolIcon-fileForeground, var(--accent)); }
.atlas-card.result-documentation { --result-accent: var(--vscode-symbolIcon-keyForeground, var(--accent)); }
.atlas-card:hover { background: color-mix(in srgb, var(--result-accent) 12%, var(--panel)); }
.atlas-card:hover, .atlas-card.selected { border-color: var(--accent); border-left-color: var(--result-accent); }
.atlas-card.selected { background: color-mix(in srgb, var(--result-accent) 15%, var(--panel)); box-shadow: 0 0 0 1px var(--accent), 0 4px 14px rgba(0, 0, 0, .2); }
.atlas-card:focus-visible { outline: 1px solid var(--accent); outline-offset: 2px; }
.resource-context-menu { position: fixed; z-index: 20; min-width: 210px; padding: 4px; border: 1px solid var(--vscode-menu-border, var(--border)); color: var(--vscode-menu-foreground, var(--text)); background: var(--vscode-menu-background, var(--panel)); box-shadow: 0 8px 24px rgba(0, 0, 0, .4); }
.resource-context-menu button { display: block; width: 100%; padding: 6px 20px 6px 8px; border: 0; text-align: left; color: inherit; background: transparent; }
.resource-context-menu button:hover:not(:disabled), .resource-context-menu button:focus-visible { color: var(--vscode-menu-selectionForeground, var(--selected-text)); background: var(--vscode-menu-selectionBackground, var(--selected)); outline: none; }
.resource-context-menu button:disabled { opacity: .45; cursor: default; }
.atlas-card-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 10px; }
.atlas-card-tools { display: inline-flex; flex: 0 0 auto; align-items: center; gap: 5px; }
.result-anchor-action { display: inline-flex; width: 24px; height: 24px; align-items: center; justify-content: center; padding: 0; color: var(--muted); background: transparent; }
.result-anchor-action:hover { color: var(--result-accent); border-color: var(--result-accent); }
.result-anchor-action .relation-icon { transform: scale(.78); }
.atlas-card .result-path { max-width: none; margin: 4px 0 0; text-align: left; }
.search-masthead { padding: 4px 2px 12px; }
.search-masthead h1 { margin: 0; font-size: 23px; }
.search-header { margin-bottom: 12px; border-bottom: 1px solid var(--border); }
.search-primary { display: grid; grid-template-columns: minmax(320px, 1fr) max-content; align-items: center; gap: 14px; padding: 10px 12px; border: 1px solid var(--border); background: var(--alt); box-shadow: inset 3px 0 0 var(--accent); }
.search-query { display: flex; min-width: 0; }
.search-query .query-field { min-height: 38px; background: var(--bg); }
.query-anchor { --anchor-accent: var(--accent); display: flex; width: 100%; min-width: 0; min-height: 38px; align-items: center; gap: 9px; padding: 5px 6px 5px 10px; border: 1px solid var(--anchor-accent); background: color-mix(in srgb, var(--anchor-accent) 10%, var(--bg)); }
.query-anchor.anchor-class, .query-anchor.anchor-enum { --anchor-accent: #40b5ac; }
.query-anchor.anchor-function { --anchor-accent: #f3ad58; }
.query-anchor.anchor-field { --anchor-accent: var(--vscode-symbolIcon-fieldForeground, #9cdcfe); }
.query-anchor .relation-icon { color: var(--anchor-accent); }
.query-anchor-copy { display: flex; min-width: 0; flex: 1 1 auto; flex-direction: column; }
.query-anchor-copy strong, .query-anchor-copy small { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.query-anchor-copy small { color: var(--muted); font-size: 10px; }
.query-anchor-clear { flex: 0 0 auto; width: 27px; min-height: 27px; padding: 0; border: 0; color: var(--muted); background: transparent; font-size: 17px; }
.query-anchor-clear:hover { color: var(--text); }
.relation-picker { position: relative; display: flex; flex: 0 0 auto; }
.relation-trigger { display: inline-flex; width: 142px; min-height: 38px; align-items: center; justify-content: space-between; gap: 8px; margin-left: -1px; padding: 7px 9px; background: var(--bg); white-space: nowrap; }
.relation-trigger.active { z-index: 1; border-color: var(--accent); color: var(--accent); background: var(--selected); }
.relation-trigger-label { overflow: hidden; text-overflow: ellipsis; }
.relation-icon { position: relative; display: inline-block; width: 14px; height: 14px; flex: 0 0 14px; color: currentColor; }
.relation-icon::before { content: ''; position: absolute; top: 2px; bottom: 2px; left: 3px; width: 1px; background: currentColor; }
.relation-icon::after { content: ''; position: absolute; top: 6px; left: 3px; width: 8px; height: 5px; border-top: 1px solid currentColor; border-right: 1px solid currentColor; }
.relation-menu { position: absolute; z-index: 6; top: calc(100% + 5px); right: 0; width: 312px; padding: 11px; border: 1px solid var(--accent); color: var(--text); background: var(--vscode-menu-background, var(--panel)); box-shadow: 0 8px 24px rgba(0, 0, 0, .4); }
.relation-menu-head { margin-bottom: 10px; }
.relation-menu-head strong { display: block; margin-bottom: 3px; }
.relation-menu-head span, .relation-prototype-note { color: var(--muted); font-size: 11px; }
.relation-candidates { display: grid; gap: 5px; }
.relation-candidate { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 4px 8px; width: 100%; min-height: 42px; padding: 6px 8px; text-align: left; background: var(--panel); }
.relation-candidate strong, .relation-candidate small { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.relation-candidate small { grid-column: 1 / -1; color: var(--muted); font-size: 10px; }
.relation-candidate .tag { align-self: start; padding: 2px 6px; }
.relation-candidate-empty { padding: 10px 8px; border: 1px dashed var(--border); color: var(--muted); font-size: 11px; }
.relation-section + .relation-section { margin-top: 9px; padding-top: 9px; border-top: 1px solid var(--border); }
.relation-section-label { margin-bottom: 5px; color: var(--muted); font-size: 9px; font-weight: 700; letter-spacing: .07em; }
.relation-choice { display: grid; grid-template-columns: 16px minmax(0, 1fr); align-items: center; gap: 7px; min-height: 27px; cursor: pointer; }
.relation-choice input { margin: 0; accent-color: var(--accent); }
.relation-choice.disabled { cursor: default; opacity: .7; }
.relation-depth { display: grid; grid-template-columns: 1fr 1fr; gap: 5px; }
.relation-depth button { min-height: 29px; padding: 4px 8px; }
.relation-depth button.active { border-color: var(--accent); color: var(--selected-text); background: var(--selected); }
.relation-prototype-note { margin-top: 10px; padding-top: 9px; border-top: 1px solid var(--border); }
.text-option-buttons { display: inline-flex; margin-left: -1px; }
.text-option-buttons button { min-width: 38px; min-height: 38px; padding: 0 7px; border-left-color: transparent; background: var(--bg); font-family: var(--vscode-editor-font-family); }
.text-option-buttons button:first-child { border-left-color: var(--border); }
.text-option-buttons button.active { z-index: 1; border-color: var(--accent); color: var(--accent); background: var(--selected); }
.search-count { color: var(--muted); text-align: right; white-space: nowrap; }
.search-count strong { display: block; color: var(--text); font-size: 16px; }
.search-secondary { display: flex; align-items: start; gap: 14px; padding: 9px 12px 11px; border-right: 1px solid var(--border); border-left: 1px solid var(--border); }
.search-scope-control { width: 180px; }
.search-scope-control .search-scope { position: relative; margin: 0; padding: 0; border: 0; background: transparent; }
.search-scope-control .addon-trigger { width: 100%; min-height: 38px; }
.search-scope-control .addon-menu { left: 0; }
.scope-count { color: var(--accent); font-size: 10px; }
.search-secondary .search-types { min-width: 0; }
.search-secondary .page-controls { margin-left: auto; align-self: end; }
.search-secondary .group-label { min-height: auto; padding-bottom: 4px; font-size: 9px; }
.context-stepper { display: inline-flex; align-items: stretch; overflow: hidden; border: 1px solid var(--border); background: var(--alt); white-space: nowrap; }
.context-stepper button { min-width: 27px; min-height: 26px; padding: 2px 6px; border: 0; background: transparent; }
.context-stepper button + button { border-left: 1px solid var(--border); }
.context-stepper [data-preview-context-auto] { display: inline-flex; min-width: 58px; align-items: center; justify-content: center; gap: 5px; color: var(--text); }
.context-stepper [data-preview-context-auto].active { color: var(--accent); background: var(--selected); }
.context-auto-icon { display: inline-flex; width: 13px; flex-direction: column; align-items: center; gap: 2px; }
.context-auto-icon i { display: block; height: 1px; background: currentColor; }
.context-auto-icon i:nth-child(1) { width: 8px; }
.context-auto-icon i:nth-child(2) { width: 13px; }
.context-auto-icon i:nth-child(3) { width: 6px; }
@media (max-width: 980px) { .atlas-results.masonry { column-count: 1; } .atlas-results.rows { grid-template-columns: minmax(640px, 1fr); } }
@media (max-width: 1100px) { .search-primary { grid-template-columns: minmax(0, 1fr); } .search-count { display: none; } .search-secondary { flex-wrap: wrap; } .search-secondary .page-controls { flex-basis: 100%; margin-left: 0; } }
@media (max-width: 720px) { .shell { padding: 18px 14px 60px; } .search-primary { grid-template-columns: 1fr; } .search-secondary { flex-direction: column; } .search-query { flex-wrap: wrap; } .text-option-buttons { margin: -1px 0 0; } .atlas-card-head { align-items: flex-start; flex-wrap: wrap; } .result-path { max-width: 100%; margin-left: 0; text-align: left; } }
</style>
</head>
<body>
<main id="app"></main>
<script nonce="${nonce}">
window.__reforgerSearchVscode = acquireVsCodeApi();
const reportWebviewError = (message, source, line, column) => window.__reforgerSearchVscode.postMessage({ type: 'webviewError', message: String(message ?? 'Unknown webview error'), source, line, column });
window.addEventListener('error', event => reportWebviewError(event.message, event.filename, event.lineno, event.colno));
window.addEventListener('unhandledrejection', event => reportWebviewError(event.reason?.message ?? event.reason, 'unhandledrejection'));
window.__reforgerSearchVscode.postMessage({ type: 'webviewReady', width: window.innerWidth, height: window.innerHeight, devicePixelRatio: window.devicePixelRatio });
</script>
<script nonce="${nonce}">
const vscode = window.__reforgerSearchVscode;
const resultLayouts = [
  { id: 'single', label: 'One column' },
  { id: 'masonry', label: 'Two-column masonry' },
  { id: 'rows', label: 'Aligned rows' },
];
const state = { query: '', mode: 'semantic', matchCase: false, matchWholeWord: false, useRegex: false, type: 'all', relationOpen: false, relationAnchor: null, relationIncludes: ['direct'], relationDepth: 'direct', resultLayout: 'single', results: [], sourcePreviews: {}, matchRanges: {}, semanticPreviews: {}, warnings: [], status: 'idle', error: '', requestId: 0, selected: '', page: 1, pageSize: 25, total: 0, truncated: false, totalBySource: {}, lastSearchKey: '', searchPerformance: {}, previewPerformance: {}, scopeOpen: false, scopeFilter: '', scopeRevision: '', scopeAuthority: '', scopeDiscoveryMs: 0, unavailableScopeIds: [], scopeSources: [{ id: 'workspace', label: 'Workspace', detail: 'Live', kind: 'workspace', pinned: true, defaultSelected: true }, { id: 'wiki', label: 'Official Wiki', detail: 'Text search', kind: 'wiki', pinned: true, defaultSelected: true }], selectedScopeIds: ['workspace', 'wiki'], selectionTouched: false, removedScopeIds: [], uiPerformance: { renderCount: 0, lastRenderMs: 0, lastSearchResponseMs: 0, lastPreviewMessageMs: 0, lastSemanticMessageMs: 0 } };
let pendingQuerySelection;
let previewContextLines = 0;
const esc = value => String(value ?? '').replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char]));
const sourceLabel = result => result.addonLabel ?? (result.source === 'wiki' ? 'Official Wiki' : result.source === 'workspace' ? 'Workspace' : result.source === 'workbench' ? 'Workbench' : 'Game Data');
const visibleResults = () => state.results;
const modeButtons = () => '<button class="' + (state.mode === 'semantic' ? 'active' : '') + '" data-mode="semantic">Semantic</button><button class="' + (state.mode === 'text' ? 'active' : '') + '" data-mode="text">Text</button><button class="' + (state.mode === 'resource' ? 'active' : '') + '" data-mode="resource">Resources</button>';
const textSearchOptions = () => state.mode !== 'text' ? '' : '<div class="text-option-buttons" aria-label="Text matching options"><button type="button" data-text-option="matchCase" class="' + (state.matchCase ? 'active' : '') + '" aria-pressed="' + state.matchCase + '" title="Match case" aria-label="Match case">Aa</button><button type="button" data-text-option="matchWholeWord" class="' + (state.matchWholeWord ? 'active' : '') + '" aria-pressed="' + state.matchWholeWord + '" title="Match whole word" aria-label="Match whole word">|ab|</button><button type="button" data-text-option="useRegex" class="' + (state.useRegex ? 'active' : '') + '" aria-pressed="' + state.useRegex + '" title="Use regular expression" aria-label="Use regular expression">.*</button></div>';
const relationOptions = [
  { value: 'direct', label: 'Direct matches', summary: 'Direct', group: 'MATCH' },
  { value: 'parents', label: 'Parent classes', summary: 'Parents', group: 'CLASS HIERARCHY' },
  { value: 'children', label: 'Child classes', summary: 'Children', group: 'CLASS HIERARCHY' },
  { value: 'baseMembers', label: 'Base implementations', summary: 'Base', group: 'MEMBER BEHAVIOR' },
  { value: 'overrides', label: 'Overrides', summary: 'Overrides', group: 'MEMBER BEHAVIOR' },
  { value: 'modded', label: 'Modded extensions', summary: 'Modded', group: 'MODDING' },
];
const relationOptionsForAnchor = () => {
  if (state.relationAnchor?.symbolKind === 'class') return relationOptions.filter(option => ['direct', 'parents', 'children', 'modded'].includes(option.value));
  if (state.relationAnchor?.symbolKind === 'method') return relationOptions.filter(option => ['direct', 'baseMembers', 'overrides'].includes(option.value));
  return relationOptions.filter(option => option.value === 'direct');
};
const relationSummary = () => {
  if (!state.relationAnchor) return 'Related code';
  if (state.relationIncludes.length === 1 && state.relationIncludes[0] === 'direct') return 'Direct only';
  const first = relationOptions.find(option => option.value === state.relationIncludes[0])?.summary ?? 'Related';
  return first + (state.relationIncludes.length > 1 ? ' +' + (state.relationIncludes.length - 1) : '');
};
const relationChoices = () => {
  const options = relationOptionsForAnchor();
  const groups = [...new Set(options.map(option => option.group))];
  return groups.map(group => '<div class="relation-section"><div class="relation-section-label">' + group + '</div>' + options.filter(option => option.group === group).map(option => {
    const checked = state.relationIncludes.includes(option.value);
    const soleSelection = checked && state.relationIncludes.length === 1;
    return '<label class="relation-choice ' + (soleSelection ? 'disabled' : '') + '"><input type="checkbox" data-relation-choice="' + esc(option.value) + '"' + (checked ? ' checked' : '') + (soleSelection ? ' disabled' : '') + '><span>' + esc(option.label) + '</span></label>';
  }).join('') + '</div>').join('');
};
const relationCandidates = () => {
  const candidates = visibleResults().filter(result => result.kind === 'symbol' && result.symbolRef).slice(0, 6);
  if (!candidates.length) return '<div class="relation-candidate-empty">Type a semantic query to find declarations, then choose one exact result.</div>';
  return '<div class="relation-candidates">' + candidates.map(result => '<button type="button" class="relation-candidate" data-relation-anchor="' + esc(result.id) + '" title="' + esc(result.qualifiedName ?? result.title) + '"><strong>' + esc(result.qualifiedName ?? result.title) + '</strong><span class="tag">' + esc(result.detail) + '</span><small>' + esc((result.signature ?? result.title) + ' · ' + sourceLabel(result) + ' · ' + result.path) + '</small></button>').join('') + '</div>';
};
const relationDepthControl = () => state.relationAnchor?.symbolKind !== 'class' ? '' : '<div class="relation-section"><div class="relation-section-label">HIERARCHY DEPTH</div><div class="relation-depth"><button type="button" data-relation-depth="direct" class="' + (state.relationDepth === 'direct' ? 'active' : '') + '">One level</button><button type="button" data-relation-depth="all" class="' + (state.relationDepth === 'all' ? 'active' : '') + '">All levels</button></div></div>';
const relationCapabilityNote = () => ['class', 'method'].includes(state.relationAnchor?.symbolKind) ? '' : '<div class="relation-prototype-note">This symbol type has no hierarchy behavior in this prototype.</div>';
const relationMenu = () => !state.relationAnchor
  ? '<div class="relation-menu" role="dialog" aria-label="Choose an exact relationship anchor"><div class="relation-menu-head"><strong>Choose an exact declaration</strong><span>Your text can be broad; relationships begin from one selected symbol.</span></div>' + relationCandidates() + '<div class="relation-prototype-note">You can also use the branch icon on any result card.</div></div>'
  : '<div class="relation-menu" role="dialog" aria-label="Related code filters"><div class="relation-menu-head"><strong>' + esc(state.relationAnchor.qualifiedName) + '</strong><span>' + esc(state.relationAnchor.detail + ' · ' + state.relationAnchor.sourceLabel) + '</span></div>' + relationChoices() + relationDepthControl() + relationCapabilityNote() + '</div>';
const relationControl = () => '<div class="relation-picker"><button type="button" class="relation-trigger ' + (state.relationOpen || state.relationAnchor ? 'active' : '') + '" data-relation-open aria-haspopup="dialog" aria-expanded="' + state.relationOpen + '" title="Choose an exact declaration and its relationships"><span class="relation-icon" aria-hidden="true"></span><span class="relation-trigger-label">' + esc(relationSummary()) + '</span><span aria-hidden="true">' + (state.relationOpen ? '&#9650;' : '&#9660;') + '</span></button>' + (state.relationOpen ? relationMenu() : '') + '</div>';
const eligibleScopeSources = () => state.scopeSources.filter(source => source.kind !== 'wiki' || state.mode === 'text');
const selectedEligibleScopeIds = () => state.selectedScopeIds.filter(id => eligibleScopeSources().some(source => source.id === id));
const sameScopeIds = (left, right) => left.length === right.length && left.every(id => right.includes(id));
const allEligibleScopesSelected = () => {
  const eligible = eligibleScopeSources();
  return eligible.length > 0 && eligible.every(source => state.selectedScopeIds.includes(source.id));
};
const scopeChoices = () => {
  const eligible = eligibleScopeSources();
  const allSelected = allEligibleScopesSelected();
  const filter = state.scopeFilter.trim().toLowerCase();
  const filtered = eligible.filter(source => !filter || source.label.toLowerCase().includes(filter));
  const choices = filtered.map((source, index) => '<label class="addon-choice' + (source.pinned && !filtered[index + 1]?.pinned ? ' pinned-boundary' : '') + '"><input type="checkbox" data-scope-choice="' + esc(source.id) + '"' + (state.selectedScopeIds.includes(source.id) ? ' checked' : '') + '><span>' + esc(source.label) + '</span><small>' + esc(source.detail) + '</small></label>').join('');
  return '<div class="addon-menu"><div class="scope-actions"><input class="addon-filter" data-scope-filter value="' + esc(state.scopeFilter) + '" placeholder="Filter add-ons..." aria-label="Filter search scopes"><button type="button" data-scope-all>' + (allSelected ? 'Unselect all' : 'Select all') + '</button></div>' + choices + '</div>';
};
const searchScope = () => {
  const selected = eligibleScopeSources().filter(source => state.selectedScopeIds.includes(source.id));
  const first = selected[0]?.label ?? 'No sources';
  const overflow = selected.length > 1 ? '<span class="scope-count">+' + (selected.length - 1) + '</span>' : '';
  const title = selected.length > 0 ? selected.map(source => source.label).join(', ') : 'No search scopes selected';
  return '<div class="search-scope"><button class="addon-trigger" data-scope-open title="' + esc(title) + '" aria-label="Edit search scope: ' + esc(title) + '"><span class="addon-summary">' + esc(first) + ' ' + overflow + '</span><span>' + (state.scopeOpen ? '&#9650;' : '&#9660;') + '</span></button>' + (state.scopeOpen ? scopeChoices() : '') + '</div>';
};
const resultTypes = ${JSON.stringify(searchKindFilters.map(({ value, label }) => ({ value, label })))};
const resourceResultTypes = ${JSON.stringify(searchResourceKindFilters.map(({ value, label }) => ({ value, label })))};
const typeButtons = () => (state.mode === 'text' ? [] : state.mode === 'resource' ? resourceResultTypes : resultTypes).map(type => '<button class="' + (state.type === type.value ? 'active' : '') + '" data-type="' + esc(type.value) + '">' + esc(type.label) + '</button>').join('');
const modeControls = () => '<div class="control-buttons">' + modeButtons() + '</div>';
const typeControls = () => state.mode === 'text' ? '' : '<div class="control-buttons">' + typeButtons() + '</div>';
const queryField = () => state.relationAnchor
  ? '<div class="query-anchor anchor-' + resultAccent(state.relationAnchor) + '" aria-label="Exact relationship anchor"><span class="relation-icon" aria-hidden="true"></span><span class="query-anchor-copy"><strong>' + esc(state.relationAnchor.qualifiedName) + '</strong><small>' + esc(state.relationAnchor.signature + ' · ' + state.relationAnchor.sourceLabel + ' · ' + state.relationAnchor.path) + '</small></span><button type="button" class="query-anchor-clear" data-clear-relation-anchor aria-label="Clear exact symbol and return to search" title="Change exact symbol">&times;</button></div>'
  : '<input id="query" class="query-field" value="' + esc(state.query) + '" placeholder="Search a symbol, concept, or phrase..." aria-label="Search query">';
const pageSizeOptions = [25, 50, 100];
const maxSearchPages = ${searchLimits.maxPages};
const totalMatches = () => state.total;
const totalMatchesLabel = () => state.total.toLocaleString() + (state.truncated ? '+' : '');
const totalPages = () => Math.min(maxSearchPages, Math.max(1, Math.ceil(state.total / state.pageSize)));
const hasActiveSearch = () => Boolean(state.query.trim() || state.relationAnchor || state.mode === 'resource' || (state.mode === 'semantic' && state.type !== 'all'));
const pageControls = (includeLayoutToggle = false) => {
  const navigationDisabled = !hasActiveSearch() || state.status === 'loading';
  const pageTotal = totalPages();
  const sizes = pageSizeOptions.map(size => '<option value="' + size + '"' + (state.pageSize === size ? ' selected' : '') + '>' + size + ' results</option>').join('');
  const layoutIndex = Math.max(0, resultLayouts.findIndex(layout => layout.id === state.resultLayout));
  const layout = resultLayouts[layoutIndex];
  const nextLayout = resultLayouts[(layoutIndex + 1) % resultLayouts.length];
  const layoutLabel = 'Result layout: ' + layout.label + '. Activate for ' + nextLayout.label.toLowerCase() + '.';
  const layoutToggle = includeLayoutToggle ? '<button type="button" data-result-layout="' + esc(state.resultLayout) + '" aria-label="' + esc(layoutLabel) + '" title="' + esc(layoutLabel) + '"><span class="layout-toggle layout-' + esc(state.resultLayout) + '" aria-hidden="true"><span></span><span></span><span></span><span></span></span></button>' : '';
  const previewContextValue = previewContextLines === 0 ? 'Auto' : previewContextLines;
  const previewContextIcon = previewContextLines === 0 ? '<span class="context-auto-icon" aria-hidden="true"><i></i><i></i><i></i></span>' : '';
  const previewControl = includeLayoutToggle && state.mode !== 'resource' ? '<div class="context-stepper" aria-label="Result preview context"><button type="button" data-preview-context-down aria-label="Decrease preview context" title="Decrease preview context"' + (previewContextLines === 0 ? ' disabled' : '') + '>&minus;</button><button type="button" data-preview-context-auto class="' + (previewContextLines === 0 ? 'active' : '') + '" aria-pressed="' + (previewContextLines === 0) + '" aria-label="' + (previewContextLines === 0 ? 'Automatic enclosing scope' : 'Reset preview context to automatic') + '" title="' + (previewContextLines === 0 ? 'Automatic enclosing scope' : 'Reset to automatic enclosing scope') + '">' + previewContextIcon + '<span aria-live="polite">' + previewContextValue + '</span></button><button type="button" data-preview-context-up aria-label="Increase preview context" title="Increase preview context">+</button></div>' : '';
  return '<div class="page-controls" aria-label="Search result pages">' + previewControl + layoutToggle + '<select data-page-size aria-label="Total results per page"' + (state.status === 'loading' ? ' disabled' : '') + '>' + sizes + '</select><span class="page-status"><span class="muted">Page</span><input data-page-input type="number" min="1" max="' + pageTotal + '" value="' + state.page + '" aria-label="Current result page"' + (navigationDisabled ? ' disabled' : '') + '><span class="muted">of ' + pageTotal + '</span></span><span class="page-arrows"><button type="button" data-page-prev' + (navigationDisabled || state.page <= 1 ? ' disabled' : '') + ' aria-label="Previous page">‹</button><button type="button" data-page-next' + (navigationDisabled || state.page >= pageTotal ? ' disabled' : '') + ' aria-label="Next page">›</button></span></div>';
};
const inlineMarkdown = value => value
  .replace(/\\\\([*_])/g, '$1')
  .replace(/\`([^\`]+)\`/g, '<code>$1</code>')
  .replace(/!\\[([^\\]]*)\\]\\((?:[^()\\n]|\\([^()\\n]*\\))*\\)/g, '$1')
  .replace(/\\[\\]\\((?:[^()\\n]|\\([^()\\n]*\\))*\\)/g, '')
  .replace(/\\[([^\\]]+)\\]\\((?:[^()\\n]|\\([^()\\n]*\\))*\\)/g, '$1')
  .replace(/\\*\\*([^*]+)\\*\\*/g, '<strong>$1</strong>')
  .replace(/__([^_]+)__/g, '<strong>$1</strong>')
  .replace(/\\*([^*]+)\\*/g, '<em>$1</em>')
  .replace(/_([^_]+)_/g, '<em>$1</em>');
const renderMarkdown = value => {
  const lines = esc(value).split('\\n');
  const output = [];
  let paragraph = [];
  let list = false;
  let code = false;
  const flushParagraph = () => { if (paragraph.length) { output.push('<p>' + inlineMarkdown(paragraph.join(' ')) + '</p>'); paragraph = []; } };
  const closeList = () => { if (list) { output.push('</ul>'); list = false; } };
  lines.forEach(line => {
    if (line.trim().startsWith('\`\`\`')) {
      flushParagraph(); closeList();
      if (code) { output.push('</code></pre>'); } else { output.push('<pre class="md-code"><code>'); }
      code = !code;
      return;
    }
    if (code) { output.push(line + '\\n'); return; }
    if (!line.trim()) { flushParagraph(); closeList(); return; }
    const heading = line.match(/^(#{1,6})\\s+(.+)$/);
    if (heading) { flushParagraph(); closeList(); output.push('<h' + heading[1].length + '>' + inlineMarkdown(heading[2]) + '</h' + heading[1].length + '>'); return; }
    const item = line.match(/^\\s*[-*+]\\s+(.+)$/);
    if (item) { flushParagraph(); if (!list) { output.push('<ul>'); list = true; } output.push('<li>' + inlineMarkdown(item[1]) + '</li>'); return; }
    if (line.match(/^>\\s?/)) { flushParagraph(); closeList(); output.push('<blockquote>' + inlineMarkdown(line.replace(/^>\\s?/, '')) + '</blockquote>'); return; }
    closeList(); paragraph.push(line.trim());
  });
  flushParagraph(); closeList();
  if (code) { output.push('</code></pre>'); }
  return output.join('');
};
const highlightText = (value, query) => {
  const terms = [...new Set(String(query ?? '').trim().split(/\s+/).filter(Boolean))].sort((left, right) => right.length - left.length);
  if (!terms.length) return esc(value);
  const matcher = new RegExp(terms.map(term => term.replace(/[.*+?^$()|[\]\\]/g, '\\$&')).join('|'), 'gi');
  let output = '';
  let lastIndex = 0;
  let match;
  while ((match = matcher.exec(value)) !== null) {
    const matchIndex = match.index ?? 0;
    output += esc(value.slice(lastIndex, matchIndex)) + '<mark>' + esc(match[0]) + '</mark>';
    lastIndex = matchIndex + match[0].length;
  }
  return output + esc(value.slice(lastIndex));
};
const highlightRange = (value, range) => {
  if (!range || !Number.isFinite(Number(range.start)) || !Number.isFinite(Number(range.length))) return highlightText(value, state.query);
  const start = Math.max(0, Math.min(String(value).length, Number(range.start)));
  const end = Math.max(start, Math.min(String(value).length, start + Number(range.length)));
  return esc(String(value).slice(0, start)) + '<mark>' + esc(String(value).slice(start, end)) + '</mark>' + esc(String(value).slice(end));
};
const highlightPreviewPart = (value, offset, range) => {
  if (!range) return highlightText(value, state.query);
  const start = Number(range.start) - offset;
  const end = start + Number(range.length);
  if (end <= 0 || start >= String(value).length) return esc(value);
  const localStart = Math.max(0, start);
  const localEnd = Math.min(String(value).length, end);
  return esc(String(value).slice(0, localStart)) + '<mark>' + esc(String(value).slice(localStart, localEnd)) + '</mark>' + esc(String(value).slice(localEnd));
};
const safeSemanticColor = value => /^#[0-9a-f]{3,8}$/i.test(String(value ?? '')) ? String(value) : '';
const addonHeaderColor = result => {
  const value = result.thumbnailColor;
  return /^#[0-9A-F]{6}$/i.test(String(value ?? '')) ? String(value).toUpperCase() : '';
};
const semanticPreviewText = result => {
  const sourceText = state.sourcePreviews[result.id] ?? (result.kind === 'text' ? result.excerpt : undefined);
  if (typeof sourceText !== 'string') return '';
  const preview = state.semanticPreviews[result.id];
  const matchRange = state.matchRanges[result.id] ?? (result.kind === 'text' ? { start: result.textMatchStart, length: result.textMatchLength } : undefined);
  if (!preview || typeof preview.text !== 'string' || preview.text !== sourceText || !Array.isArray(preview.tokens)) return highlightRange(sourceText, matchRange);
  const text = sourceText;
  const tokens = preview.enabled === false ? [] : preview.tokens.slice().sort((left, right) => left.start - right.start);
  let output = '';
  let cursor = 0;
  tokens.forEach(token => {
    const start = Math.max(cursor, Math.min(text.length, Number(token.start) || 0));
    const end = Math.max(start, Math.min(text.length, start + (Number(token.length) || 0)));
    if (start > cursor) output += highlightPreviewPart(text.slice(cursor, start), cursor, matchRange);
    const color = safeSemanticColor(preview.foregrounds?.[token.role]);
    output += '<span data-semantic-token="' + esc(token.role) + '"' + (color ? ' style="color:' + esc(color) + ';"' : '') + '>' + highlightPreviewPart(text.slice(start, end), start, matchRange) + '</span>';
    cursor = end;
  });
  return output + highlightPreviewPart(text.slice(cursor), cursor, matchRange);
};
const resultPreview = result => result.kind === 'resource'
  ? ''
  : result.kind === 'documentation'
  ? '<div class="md-preview">' + renderMarkdown(result.excerpt) + '</div>'
  : '<pre class="snippet" data-result-preview="' + esc(result.id) + '">' + semanticPreviewText(result) + '</pre>';
const resultExternalAction = result => result.sourceUrl ? '<button data-external="' + esc(result.id) + '">Open official page</button>' : '';
const resultRelationAction = result => state.mode === 'semantic' && result.kind === 'symbol' && result.symbolRef ? '<button type="button" class="result-anchor-action" data-relation-anchor="' + esc(result.id) + '" aria-label="Explore relationships from ' + esc(result.title) + '" title="Explore relationships"><span class="relation-icon" aria-hidden="true"></span></button>' : '';
const relationshipLabels = { direct: 'Direct', directBase: 'Parent', derivedType: 'Child', moddedExtension: 'Modded', overriddenDeclaration: 'Base', override: 'Override' };
const resultRelationshipTag = result => result.relationshipKind ? '<span class="tag relationship-tag" title="' + esc(result.relationshipEvidence ?? '') + '">' + esc(relationshipLabels[result.relationshipKind] ?? result.relationshipKind) + (Number(result.relationshipDistance) > 1 ? ' · ' + result.relationshipDistance : '') + '</span>' : '';
const resultTitle = result => result.kind === 'resource' ? result.path : result.title;
const resultPath = result => result.kind === 'resource' ? '' : '<div class="result-path">' + esc(result.path) + '</div>';
const resultAccent = result => {
  if (result.kind === 'text') return 'string';
  if (result.kind === 'resource') return 'resource';
  if (result.kind === 'documentation') return 'documentation';
  if (result.symbolKind === 'class') return 'class';
  if (['function', 'method', 'constructor', 'destructor'].includes(result.symbolKind)) return 'function';
  if (['field', 'globalField'].includes(result.symbolKind)) return 'field';
  if (['enum', 'enumMember'].includes(result.symbolKind)) return 'enum';
  return 'default';
};
const resultCard = result => '<article class="atlas-card result-' + resultAccent(result) + ' ' + (state.selected === result.id ? 'selected' : '') + '" data-open="' + esc(result.id) + '" tabindex="0" role="button"><div class="atlas-card-head"><strong>' + esc(resultTitle(result)) + '</strong><span class="atlas-card-tools">' + resultRelationshipTag(result) + '<span class="tag">' + esc(result.detail) + '</span>' + resultRelationAction(result) + '</span></div>' + resultPath(result) + resultPreview(result) + '<div class="result-actions">' + resultExternalAction(result) + '</div></article>';
const resultGroups = () => {
  const groups = new Map();
  visibleResults().forEach(result => {
    const label = sourceLabel(result);
    const key = [result.source, result.addonGuid ?? '', label, addonHeaderColor(result)].join('\u0000');
    const group = groups.get(key) ?? { label, results: [] };
    group.results.push(result);
    groups.set(key, group);
  });
  return '<div class="atlas-groups">' + [...groups.values()].map(({ label, results }) => { const color = addonHeaderColor(results[0]); const layoutClass = state.resultLayout === 'single' ? '' : ' ' + state.resultLayout; return '<section class="atlas-group"><div class="atlas-group-head"' + (color ? ' style="--addon-header-color:' + esc(color) + ';"' : '') + '><h2>' + esc(label) + '</h2><span class="tag">' + results.length + '</span></div><div class="atlas-results' + layoutClass + '">' + results.map(resultCard).join('') + '</div></section>'; }).join('') + '</div>';
};
const updateResultPreviews = ids => {
  if (!ids.length) return;
  const pending = new Set(ids);
  const resultsById = new Map(state.results.map(result => [result.id, result]));
  document.querySelectorAll('[data-result-preview]').forEach(element => {
    const id = element.dataset.resultPreview;
    const result = resultsById.get(id);
    if (pending.has(id) && result) element.innerHTML = semanticPreviewText(result);
  });
};
const hasTextSelection = () => Boolean(window.getSelection()?.toString());
const captureSearchSnapshot = () => vscode.postMessage({ type: 'debugSnapshot', snapshot: {
  query: state.query,
  scopeOpen: state.scopeOpen,
  scopeFilter: state.scopeFilter,
  scopeRevision: state.scopeRevision,
  scopeAuthority: state.scopeAuthority,
  scopeDiscoveryMs: state.scopeDiscoveryMs,
  availableScopeIds: state.scopeSources.map(source => source.id),
  unavailableScopeIds: state.unavailableScopeIds,
  selectedScopeIds: state.selectedScopeIds,
  modeEligibleScopeIds: eligibleScopeSources().map(source => source.id),
  removedScopeIds: state.removedScopeIds,
  searchMode: state.mode,
  matchCase: state.matchCase,
  matchWholeWord: state.matchWholeWord,
  useRegex: state.useRegex,
  resultType: state.type,
  relationIncludes: state.relationIncludes,
  relationDepth: state.relationDepth,
  relationAnchor: state.relationAnchor,
  status: state.status,
  resultLayout: state.resultLayout,
  requestId: state.requestId,
  page: state.page,
  pageSize: state.pageSize,
  previewContextLines,
  total: state.total,
  totalBySource: state.totalBySource,
  totalPages: totalPages(),
  resultCount: state.results.length,
  visibleResultCount: visibleResults().length,
  selectedId: state.selected,
  warnings: state.warnings,
  error: state.error,
  searchPerformance: state.searchPerformance,
  previewPerformance: state.previewPerformance,
  uiPerformance: { ...state.uiPerformance, capturedAt: new Date().toISOString() },
  viewport: { width: window.innerWidth, height: window.innerHeight, devicePixelRatio: window.devicePixelRatio },
  results: state.results.map(result => ({
    id: result.id,
    source: result.source,
    kind: result.kind,
    symbolKind: result.symbolKind,
    title: result.title,
    detail: result.detail,
    path: result.path,
    matchKind: result.matchKind,
    sourceUrl: result.sourceUrl,
    sourceUri: result.sourceUri,
    selectionStartLine: result.selectionStartLine,
    selectionEndLine: result.selectionEndLine,
    excerptLength: typeof result.excerpt === 'string' ? result.excerpt.length : 0,
    excerptLineCount: typeof result.excerpt === 'string' ? result.excerpt.split('\\n').length : 0,
    previewMatchStart: state.matchRanges[result.id]?.start,
    previewMatchLength: state.matchRanges[result.id]?.length,
    previewMatchText: state.matchRanges[result.id] && typeof state.sourcePreviews[result.id] === 'string' ? state.sourcePreviews[result.id].slice(state.matchRanges[result.id].start, state.matchRanges[result.id].start + state.matchRanges[result.id].length) : undefined,
    previewText: typeof state.sourcePreviews[result.id] === 'string' ? state.sourcePreviews[result.id].slice(0, 500) : undefined,
    previewType: result.kind === 'documentation' ? 'markdown' : 'code',
    semanticAvailable: Boolean(state.semanticPreviews[result.id]),
    semanticEnabled: state.semanticPreviews[result.id]?.enabled,
    semanticTextLength: typeof state.semanticPreviews[result.id]?.text === 'string' ? state.semanticPreviews[result.id].text.length : undefined,
    semanticTokenCount: Array.isArray(state.semanticPreviews[result.id]?.tokens) ? state.semanticPreviews[result.id].tokens.length : 0,
    semanticTokenRoles: Array.isArray(state.semanticPreviews[result.id]?.tokens) ? [...new Set(state.semanticPreviews[result.id].tokens.map(token => token.role))] : [],
    semanticForegrounds: state.semanticPreviews[result.id]?.foregrounds,
  })),
} });
const openResult = element => {
  if (!element.dataset.open || hasTextSelection()) return;
  state.selected = element.dataset.open;
  vscode.postMessage({ type: 'open', id: element.dataset.open });
  render();
};
const closeResourceContextMenu = () => document.querySelector('.resource-context-menu')?.remove();
const openResourceContextMenu = (element, event) => {
  const result = state.results.find(candidate => candidate.id === element.dataset.open);
  if (result?.kind !== 'resource') return;
  event.preventDefault();
  event.stopPropagation();
  closeResourceContextMenu();
  const menu = document.createElement('div');
  menu.className = 'resource-context-menu';
  menu.setAttribute('role', 'menu');
  menu.setAttribute('aria-label', 'Resource actions');
  menu.innerHTML = '<button type="button" role="menuitem" data-copy-resource>Copy Resource Path</button><button type="button" role="menuitem" data-reveal-resource' + (result.resourcePhysicalPath ? '' : ' disabled aria-disabled="true" title="Available only for unpacked add-on resources"') + '>Show in File Explorer</button>';
  document.body.appendChild(menu);
  menu.style.left = Math.max(4, Math.min(event.clientX, window.innerWidth - menu.offsetWidth - 4)) + 'px';
  menu.style.top = Math.max(4, Math.min(event.clientY, window.innerHeight - menu.offsetHeight - 4)) + 'px';
  menu.querySelector('[data-copy-resource]').addEventListener('click', () => { vscode.postMessage({ type: 'copyResourcePath', id: result.id }); closeResourceContextMenu(); });
  menu.querySelector('[data-reveal-resource]').addEventListener('click', () => { if (!result.resourcePhysicalPath) return; vscode.postMessage({ type: 'revealResource', id: result.id }); closeResourceContextMenu(); });
  menu.querySelector('button:not(:disabled)')?.focus();
};
const focusScopeFilter = (selectionStart, selectionEnd = selectionStart) => {
  const filter = document.querySelector('[data-scope-filter]');
  if (!filter) return;
  filter.focus();
  filter.setSelectionRange(selectionStart, selectionEnd);
};
const chooseRelationAnchor = id => {
  const result = state.results.find(candidate => candidate.id === id && candidate.kind === 'symbol' && candidate.symbolRef);
  if (!result) return;
  state.relationAnchor = { id: result.id, symbolRef: result.symbolRef, title: result.title, qualifiedName: result.qualifiedName ?? result.title, signature: result.signature ?? result.title, detail: result.detail, path: result.path, source: result.source, sourceLabel: sourceLabel(result), symbolKind: result.symbolKind, addonGuid: result.addonGuid };
  const allowed = new Set(relationOptionsForAnchor().map(option => option.value));
  state.relationIncludes = state.relationIncludes.filter(value => allowed.has(value));
  if (!state.relationIncludes.length) state.relationIncludes = ['direct'];
  state.relationOpen = true;
  state.scopeOpen = false;
  pendingQuerySelection = undefined;
  render(false);
  search(true);
};
const relationAnchorInScope = () => !state.relationAnchor || (state.relationAnchor.source === 'workspace' ? state.selectedScopeIds.includes('workspace') : Boolean(state.relationAnchor.addonGuid && state.selectedScopeIds.includes(state.relationAnchor.addonGuid)));
const clearOutOfScopeRelationAnchor = () => { if (relationAnchorInScope()) return; state.relationAnchor = null; state.relationIncludes = ['direct']; state.relationOpen = false; };
const setSearchMode = value => { state.mode = value === 'text' ? 'text' : value === 'resource' ? 'resource' : 'semantic'; state.type = 'all'; state.relationOpen = false; state.relationAnchor = null; state.relationIncludes = ['direct']; state.page = 1; render(false); search(true); };
const setRelationChoice = (value, checked) => { const selected = checked ? [...new Set([...state.relationIncludes, value])] : state.relationIncludes.filter(candidate => candidate !== value); if (!selected.length) return; state.relationIncludes = selected; render(false); search(true); };
const setRelationDepth = value => { state.relationDepth = value === 'all' ? 'all' : 'direct'; render(false); search(true); };
const clearRelationAnchor = () => { state.relationAnchor = null; state.relationIncludes = ['direct']; state.relationOpen = false; render(true); search(true); };
const setScopeChoice = (value, checked) => { state.selectionTouched = true; state.selectedScopeIds = checked ? [...new Set([...state.selectedScopeIds, value])] : state.selectedScopeIds.filter(candidate => candidate !== value); clearOutOfScopeRelationAnchor(); render(false); search(true); };
const setPreviewContext = value => { const nextContextLines = Math.max(0, Math.min(249, value)); previewContextLines = nextContextLines; render(false); vscode.postMessage({ type: 'previewContext', contextLines: nextContextLines }); };
const toggleResultLayout = () => { const current = Math.max(0, resultLayouts.findIndex(layout => layout.id === state.resultLayout)); state.resultLayout = resultLayouts[(current + 1) % resultLayouts.length].id; render(false); };
const resultBody = content => state.error
  ? '<div class="error">' + esc(state.error) + '</div>'
  : state.mode !== 'resource' && selectedEligibleScopeIds().length === 0
    ? '<div class="empty">No search scopes selected.</div>'
    : visibleResults().length
      ? content
      : '<div class="empty">No results match this search.</div>';
function render(focusQuery = false) {
  closeResourceContextMenu();
  const renderStartedAt = performance.now();
  const warnings = state.warnings.map(warning => '<div class="warning">' + esc(warning) + '</div>').join('');
  const bottomPager = hasActiveSearch() && totalMatches() > 0 ? '<div class="page-bottom">' + pageControls() + '</div>' : '';
  const sourceCount = new Set(visibleResults().map(sourceLabel)).size;
  const sourceNoun = sourceCount === 1 ? ' source' : ' sources';
  const sharedMatchArea = () => warnings + resultBody(resultGroups()) + bottomPager;
  const typeControl = state.mode === 'text' ? '' : '<div class="control-block search-types"><div class="group-label">RESULT TYPE</div>' + typeControls() + '</div>';
  const page = '<div class="shell search-atlas"><section class="search-masthead"><h1>Source Search</h1></section><header class="search-header"><div class="search-primary"><div class="search-query">' + queryField() + textSearchOptions() + (state.mode === 'semantic' ? relationControl() : '') + '</div><div class="search-count"><strong>' + totalMatchesLabel() + '</strong>matches / ' + sourceCount + sourceNoun + '</div></div><div class="search-secondary"><div class="control-block search-scope-control"><div class="group-label">SEARCH SCOPE</div>' + searchScope() + '</div><div class="control-block"><div class="group-label">SEARCH MODE</div>' + modeControls() + '</div>' + typeControl + pageControls(true) + '</div></header>' + sharedMatchArea() + '</div>';
  document.getElementById('app').innerHTML = page;
  const query = document.getElementById('query');
  const focusSearchQuery = () => { if (!query) return; query.focus(); query.setSelectionRange(state.query.length, state.query.length); };
  if (focusQuery) focusSearchQuery();
  else if (pendingQuerySelection && query) { query.focus(); query.setSelectionRange(pendingQuerySelection.start, pendingQuerySelection.end); pendingQuerySelection = undefined; }
  if (query) {
    query.addEventListener('input', event => { state.query = event.target.value; pendingQuerySelection = { start: event.target.selectionStart ?? state.query.length, end: event.target.selectionEnd ?? state.query.length }; if (state.mode !== 'text') search(true); });
    query.addEventListener('keydown', event => { if (event.key === 'Enter') { event.preventDefault(); search(true); } });
  }
  document.querySelectorAll('[data-text-option]').forEach(element => element.addEventListener('click', () => { state[element.dataset.textOption] = !state[element.dataset.textOption]; render(false); search(true); }));
  document.querySelectorAll('[data-mode]').forEach(element => element.addEventListener('click', () => setSearchMode(element.dataset.mode)));
  document.querySelectorAll('[data-type]').forEach(element => element.addEventListener('click', () => { state.type = element.dataset.type; state.page = 1; render(false); search(true); }));
  document.querySelectorAll('[data-relation-open]').forEach(element => element.addEventListener('click', () => { state.relationOpen = !state.relationOpen; state.scopeOpen = false; render(false); }));
  document.querySelectorAll('[data-relation-choice]').forEach(element => element.addEventListener('change', () => setRelationChoice(element.dataset.relationChoice, element.checked)));
  document.querySelectorAll('[data-relation-depth]').forEach(element => element.addEventListener('click', () => setRelationDepth(element.dataset.relationDepth)));
  document.querySelectorAll('[data-relation-anchor]').forEach(element => element.addEventListener('click', event => { event.stopPropagation(); chooseRelationAnchor(element.dataset.relationAnchor); }));
  document.querySelectorAll('[data-clear-relation-anchor]').forEach(element => element.addEventListener('click', event => { event.stopPropagation(); clearRelationAnchor(); }));
  document.querySelectorAll('[data-scope-open]').forEach(element => element.addEventListener('click', () => { state.scopeOpen = !state.scopeOpen; state.relationOpen = false; render(false); if (state.scopeOpen) focusScopeFilter(state.scopeFilter.length); }));
  document.querySelectorAll('[data-scope-choice]').forEach(element => element.addEventListener('change', () => setScopeChoice(element.dataset.scopeChoice, element.checked)));
  document.querySelectorAll('[data-scope-all]').forEach(element => element.addEventListener('click', () => { const eligible = eligibleScopeSources(); const allSelected = allEligibleScopesSelected(); const eligibleIds = new Set(eligible.map(source => source.id)); state.selectionTouched = true; state.selectedScopeIds = allSelected ? state.selectedScopeIds.filter(id => !eligibleIds.has(id)) : [...new Set([...state.selectedScopeIds, ...eligibleIds])]; clearOutOfScopeRelationAnchor(); render(false); search(true); }));
  document.querySelectorAll('[data-scope-filter]').forEach(element => element.addEventListener('input', () => { const selectionStart = element.selectionStart ?? element.value.length; const selectionEnd = element.selectionEnd ?? selectionStart; state.scopeFilter = element.value; render(false); focusScopeFilter(selectionStart, selectionEnd); }));
  document.querySelectorAll('[data-page-prev]').forEach(element => element.addEventListener('click', () => requestPage(state.page - 1)));
  document.querySelectorAll('[data-page-next]').forEach(element => element.addEventListener('click', () => requestPage(state.page + 1)));
  document.querySelectorAll('[data-page-size]').forEach(element => element.addEventListener('change', event => { state.pageSize = Number(event.target.value); search(true); }));
  document.querySelectorAll('[data-preview-context-down]').forEach(element => element.addEventListener('click', () => setPreviewContext(previewContextLines - 1)));
  document.querySelectorAll('[data-preview-context-auto]').forEach(element => element.addEventListener('click', () => setPreviewContext(0)));
  document.querySelectorAll('[data-preview-context-up]').forEach(element => element.addEventListener('click', () => setPreviewContext(previewContextLines + 1)));
  document.querySelectorAll('[data-result-layout]').forEach(element => element.addEventListener('click', toggleResultLayout));
  document.querySelectorAll('[data-page-input]').forEach(element => {
    element.addEventListener('change', event => requestPage(event.target.value));
    element.addEventListener('keydown', event => { if (event.key === 'Enter') { event.preventDefault(); requestPage(event.target.value); } });
  });
  document.querySelectorAll('[data-open]').forEach(element => {
    element.addEventListener('click', event => { if (event.target.closest('[data-external], [data-relation-anchor]') || hasTextSelection()) return; openResult(element); });
    element.addEventListener('keydown', event => { if (event.target.closest('[data-external], [data-relation-anchor]')) return; if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); openResult(element); } });
    element.addEventListener('contextmenu', event => openResourceContextMenu(element, event));
  });
  document.querySelectorAll('[data-external]').forEach(element => element.addEventListener('click', event => { event.stopPropagation(); vscode.postMessage({ type: 'external', id: element.dataset.external }); }));
  state.uiPerformance.lastRenderMs = performance.now() - renderStartedAt;
  state.uiPerformance.renderCount += 1;
}
document.addEventListener('keydown', event => {
  if (event.key === 'Escape' && document.querySelector('.resource-context-menu')) { closeResourceContextMenu(); event.preventDefault(); return; }
  if (event.ctrlKey && event.key === 'F3') { event.preventDefault(); event.stopPropagation(); captureSearchSnapshot(); return; }
  if (document.activeElement !== document.body || event.ctrlKey || event.altKey || event.metaKey || event.isComposing || event.key.length !== 1) return;
  const query = document.getElementById('query');
  if (!query) return;
  query.focus();
  query.setSelectionRange(query.value.length, query.value.length);
  query.setRangeText(event.key, query.selectionStart, query.selectionEnd, 'end');
  query.dispatchEvent(new Event('input', { bubbles: true }));
  event.preventDefault();
});
document.addEventListener('click', event => { if (!(event.target instanceof Element) || !event.target.closest('.resource-context-menu')) closeResourceContextMenu(); });
document.addEventListener('click', event => {
  if (!state.scopeOpen || !(event.target instanceof Element) || event.target.closest('.search-scope')) return;
  state.scopeOpen = false;
  document.querySelector('.addon-menu')?.remove();
  const indicator = document.querySelector('[data-scope-open] span:last-child');
  if (indicator) indicator.textContent = '\u25bc';
});
document.addEventListener('click', event => {
  if (!state.relationOpen || !(event.target instanceof Element) || event.target.closest('.relation-picker')) return;
  state.relationOpen = false;
  document.querySelector('.relation-menu')?.remove();
  const trigger = document.querySelector('[data-relation-open]');
  trigger?.classList.toggle('active', Boolean(state.relationAnchor));
  trigger?.setAttribute('aria-expanded', 'false');
  const indicator = trigger?.querySelector('span:last-child');
  if (indicator) indicator.textContent = '\u25bc';
});
function requestPage(value) { if (state.status === 'loading') return; const requested = Number.parseInt(value, 10); if (!Number.isFinite(requested)) return; state.page = Math.min(totalPages(), Math.max(1, requested)); search(false); }
function search(resetPagination) { if (resetPagination) { state.page = 1; } const scopeIds = selectedEligibleScopeIds(); const searchKey = [state.mode, state.query, state.matchCase, state.matchWholeWord, state.useRegex, scopeIds.slice().sort().join(','), state.type, state.relationAnchor?.symbolRef ?? '', state.relationIncludes.slice().sort().join(','), state.relationDepth, state.page, state.pageSize].join('\\u0000'); if (state.status === 'loading' && state.lastSearchKey === searchKey) return; state.lastSearchKey = searchKey; state.error = ''; state.warnings = []; state.status = hasActiveSearch() ? 'loading' : 'idle'; state.selected = ''; state.sourcePreviews = {}; state.matchRanges = {}; state.semanticPreviews = {}; state.searchPerformance = {}; state.previewPerformance = {}; state.uiPerformance.searchStartedAt = performance.now(); state.uiPerformance.lastSearchResponseMs = 0; state.uiPerformance.lastPreviewMessageMs = 0; state.uiPerformance.lastSemanticMessageMs = 0; vscode.postMessage({ type: 'search', query: state.query, searchMode: state.mode, matchCase: state.matchCase, matchWholeWord: state.matchWholeWord, useRegex: state.useRegex, scopeIds, resultType: state.type, relationshipAnchor: state.relationAnchor ? { source: state.relationAnchor.source, symbolRef: state.relationAnchor.symbolRef } : undefined, relationIncludes: state.relationIncludes, relationDepth: state.relationDepth, page: state.page, pageSize: state.pageSize }); }
window.addEventListener('message', event => { const message = event.data; if (!message) return; if (message.type === 'focusQuery') { document.getElementById('query')?.focus(); return; } if (message.type === 'scope') { const nextSources = Array.isArray(message.scope?.sources) ? message.scope.sources : []; const available = new Set(nextSources.map(source => source.id)); const previous = state.selectedScopeIds.slice(); const previousScopeRevision = state.scopeRevision; state.scopeSources = nextSources; state.scopeRevision = message.scope?.scopeRevision ?? ''; state.scopeAuthority = message.scope?.scopeAuthority ?? ''; state.scopeDiscoveryMs = message.scope?.discoveryMs ?? 0; state.unavailableScopeIds = Array.isArray(message.scope?.unavailableScopeIds) ? message.scope.unavailableScopeIds : []; state.removedScopeIds = previous.filter(id => !available.has(id)); state.selectedScopeIds = state.selectionTouched ? previous.filter(id => available.has(id)) : nextSources.filter(source => source.defaultSelected).map(source => source.id); clearOutOfScopeRelationAnchor(); const scopeSelectionChanged = !sameScopeIds(previous, state.selectedScopeIds); const scopeRevisionChanged = Boolean(previousScopeRevision && state.scopeRevision && previousScopeRevision !== state.scopeRevision); const scopeSearchChanged = scopeSelectionChanged || scopeRevisionChanged; render(false); if (message.refreshSearch === true && scopeSearchChanged && (state.query.trim() || state.relationAnchor)) search(true); return; } if (message.requestId < state.requestId) return; state.requestId = message.requestId; if (message.type === 'loading') { state.status = 'loading'; state.error = ''; } if (message.type === 'results') { state.uiPerformance.lastSearchResponseMs = state.uiPerformance.searchStartedAt ? performance.now() - state.uiPerformance.searchStartedAt : 0; state.status = 'ready'; state.error = ''; state.results = message.results ?? []; state.sourcePreviews = {}; state.matchRanges = {}; state.semanticPreviews = {}; state.searchPerformance = message.performance ?? {}; state.previewPerformance = {}; state.warnings = message.warnings ?? []; state.total = message.total ?? 0; state.truncated = message.truncated === true; state.totalBySource = message.totalBySource ?? {}; state.page = message.page ?? state.page; state.pageSize = message.pageSize ?? state.pageSize; render(); } if (message.type === 'previews') { state.uiPerformance.lastPreviewMessageMs = state.uiPerformance.searchStartedAt ? performance.now() - state.uiPerformance.searchStartedAt : 0; state.previewPerformance = message.performance ?? {}; state.sourcePreviews = { ...state.sourcePreviews, ...(message.previews ?? {}) }; state.matchRanges = { ...state.matchRanges, ...(message.matches ?? {}) }; updateResultPreviews(Object.keys(message.previews ?? {})); } if (message.type === 'semanticPreviews') { state.uiPerformance.lastSemanticMessageMs = state.uiPerformance.searchStartedAt ? performance.now() - state.uiPerformance.searchStartedAt : 0; state.previewPerformance = message.performance ?? state.previewPerformance; state.semanticPreviews = { ...state.semanticPreviews, ...(message.previews ?? {}) }; updateResultPreviews(Object.keys(message.previews ?? {})); } if (message.type === 'error') { if (message.clearRelationshipAnchor) { state.relationAnchor = null; state.relationIncludes = ['direct']; state.relationOpen = false; if (state.query.trim()) { search(true); return; } } state.status = 'error'; state.error = [message.message ?? 'Search failed.', message.recovery].filter(Boolean).join(' '); state.results = []; state.sourcePreviews = {}; state.matchRanges = {}; state.semanticPreviews = {}; state.searchPerformance = {}; state.previewPerformance = {}; state.total = 0; state.truncated = false; state.totalBySource = {}; render(); } });
window.addEventListener('message', event => {
  const message = event.data;
  if (message?.type === 'scope' && message.refreshSearch === true && !state.query.trim() && !state.relationAnchor && hasActiveSearch()) search(true);
});
window.addEventListener('message', event => {
  if (event.data?.type !== 'focusQuery') return;
  const query = document.getElementById('query');
  if (query) query.setSelectionRange(query.value.length, query.value.length);
});
render(true);
</script>
</body>
</html>`;
}

export function renderSearchUiForTest(): string {
	return renderSearchUi();
}

function createNonce(): string {
	const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
	let value = '';
	for (let index = 0; index < 32; index += 1) {
		value += alphabet.charAt(Math.floor(Math.random() * alphabet.length));
	}
	return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function textField(value: unknown): string | undefined {
	return typeof value === 'string' ? value.slice(0, 500) : undefined;
}

function numberField(value: unknown): number | undefined {
	return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function snapshotWarnings(value: unknown): string[] {
	if (!Array.isArray(value)) {
		return [];
	}
	return value.slice(0, 20)
		.map(item => textField(item))
		.filter((item): item is string => item !== undefined);
}

function snapshotTotalsBySource(value: unknown): Partial<Record<SearchSource, number>> {
	if (!isRecord(value)) {
		return {};
	}
	const totals: Partial<Record<SearchSource, number>> = {};
	for (const source of ['workspace', 'gameData', 'wiki'] as const) {
		const total = numberField(value[source]);
		if (total !== undefined) {
			totals[source] = total;
		}
	}
	return totals;
}

function snapshotResults(value: unknown): Array<Record<string, unknown>> {
	if (!Array.isArray(value)) {
		return [];
	}
	return value.slice(0, 100).filter(isRecord).map(result => ({
		id: textField(result.id),
		source: textField(result.source),
		addonGuid: textField(result.addonGuid),
		addonLabel: textField(result.addonLabel),
		kind: textField(result.kind),
		title: textField(result.title),
		detail: textField(result.detail),
		path: textField(result.path),
		matchKind: textField(result.matchKind),
		sourceUrl: textField(result.sourceUrl),
		sourceUri: textField(result.sourceUri),
		selectionStartLine: numberField(result.selectionStartLine),
		selectionEndLine: numberField(result.selectionEndLine),
		excerptLength: numberField(result.excerptLength),
		excerptLineCount: numberField(result.excerptLineCount),
		previewMatchStart: numberField(result.previewMatchStart),
		previewMatchLength: numberField(result.previewMatchLength),
		previewMatchText: textField(result.previewMatchText),
		previewText: textField(result.previewText),
		previewType: textField(result.previewType),
		semanticAvailable: typeof result.semanticAvailable === 'boolean' ? result.semanticAvailable : undefined,
		semanticEnabled: typeof result.semanticEnabled === 'boolean' ? result.semanticEnabled : undefined,
		semanticTextLength: numberField(result.semanticTextLength),
		semanticTokenCount: numberField(result.semanticTokenCount),
		semanticTokenRoles: Array.isArray(result.semanticTokenRoles)
			? result.semanticTokenRoles.slice(0, 32).map(item => textField(item)).filter((item): item is string => item !== undefined)
			: [],
		semanticForegrounds: isRecord(result.semanticForegrounds)
			? Object.fromEntries(Object.entries(result.semanticForegrounds).slice(0, 32).flatMap(([role, color]) => {
				const value = textField(color);
				return value ? [[role, value]] : [];
			}))
			: {},
	}));
}

function snapshotPerformance(value: unknown): Record<string, unknown> {
	if (!isRecord(value)) {
		return {};
	}
	const result: Record<string, unknown> = {};
	for (const key of [
		'totalMs', 'rawMs', 'firstRawMs', 'startupMs', 'initialSearchMs', 'rangeSearchMs', 'mergeMs', 'requestedPage', 'pageSize',
		'sourcePageSize', 'readMs', 'semanticMs', 'firstSemanticMs', 'semanticWallMs', 'requestedCount', 'loadedCount', 'semanticCount',
		'renderCount', 'lastRenderMs', 'searchStartedAt', 'lastSearchResponseMs', 'lastPreviewMessageMs', 'lastSemanticMessageMs',
	]) {
		const number = numberField(value[key]);
		if (number !== undefined) {
			result[key] = number;
		}
	}
	const phase = textField(value.phase);
	if (phase !== undefined) {
		result.phase = phase;
	}
	const capturedAt = textField(value.capturedAt);
	if (capturedAt !== undefined) {
		result.capturedAt = capturedAt;
	}
	const searchMode = textField(value.searchMode);
	if (searchMode !== undefined) {
		result.searchMode = searchMode;
	}
	result.textOptions = jsonField(value.textOptions);
	result.readMsByAddon = jsonField(value.readMsByAddon);
	result.readFailuresByAddon = jsonField(value.readFailuresByAddon);
	if (Array.isArray(value.sources)) {
		result.sources = value.sources.slice(0, 8).filter(isRecord).map(source => {
			const entry: Record<string, unknown> = { source: textField(source.source) };
			for (const key of [
				'initialMs', 'rangeMs', 'remoteMs', 'pagesVisited', 'remoteRequests',
				'cacheHits', 'firstPage', 'lastPage', 'cacheSize',
			]) {
				const number = numberField(source[key]);
				if (number !== undefined) {
					entry[key] = number;
				}
			}
			entry.textStats = jsonField(source.textStats);
			entry.addonTotals = jsonField(source.addonTotals);
			return entry;
		});
	}
	return result;
}

function jsonField(value: unknown): string | undefined {
	if (value === undefined) {
		return undefined;
	}
	try {
		const serialized = JSON.stringify(value);
		return serialized.length > 20_000 ? `${serialized.slice(0, 20_000)}...[truncated]` : serialized;
	} catch {
		return '[unserializable]';
	}
}
