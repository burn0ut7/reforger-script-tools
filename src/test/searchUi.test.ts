import * as assert from 'node:assert';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vm from 'node:vm';
import * as vscode from 'vscode';
import { languageClientDocumentSelector, languageClientSchemes } from '../extensionConfig/languageClient';
import { searchLimits } from '../extensionConfig/search';
import { addonScopeLabel, asThumbnailColor, formatSearchKind, maxSearchPages, McpToolError, normalizeResourceSearchPage, normalizeSearchPage, normalizeSourceRelationshipPage, normalizeWorkbenchProjectContext, resourceKindsFor, searchKindFilters, searchResourceKindFilters, searchToolFor, sourceContextPreview, sourceLinePreview, sourceMatchRange, sourcePreviewLine, stripSourceComments, type SearchHit } from '../searchPrototype/mcpSearchClient';
import { semanticPreviewForLine, semanticPreviewForLines, semanticTokenSpansForLine } from '../searchPrototype/semanticPreview';
import { localWorkbenchResourceLinkFor, openSearchSourceDocument, queueSearchScopeRefresh, renderSearchUiForTest, resourceAddonIsLoaded, resourcePathForClipboard, resourcePhysicalPathFor, searchDocumentContentProvider, workbenchResourceOpenState } from '../searchPrototype/searchUiPrototype';

const searchUiSource = fs.readFileSync(
	path.join(__dirname, '../../src/searchPrototype/searchUiPrototype.ts'),
	'utf8',
);
const searchClientSource = fs.readFileSync(
	path.join(__dirname, '../../src/searchPrototype/mcpSearchClient.ts'),
	'utf8',
);

suite('Reforger search UI MCP mapping', () => {
	test('accepts only canonical thumbnail colors for add-on header accents', () => {
		assert.strictEqual(asThumbnailColor('#12abEF'), '#12ABEF');
		assert.strictEqual(asThumbnailColor('red'), undefined);
		assert.strictEqual(asThumbnailColor('#123456; background:red'), undefined);
	});

	test('maps canonical Workbench resources and exposes their fixed kind filters', () => {
		assert.deepStrictEqual(resourceKindsFor('audio'), ['audio']);
		assert.deepStrictEqual(resourceKindsFor('texture'), ['texture']);
		assert.ok(resourceKindsFor('all').includes('prefab'));
		assert.ok(searchResourceKindFilters.some(filter => filter.value === 'script'));
		assert.deepStrictEqual(normalizeResourceSearchPage({
			results: [{
				resourceName: '{58D0FB3206B6F859}Prefabs/Props/Radio.et',
				addonGuid: '58D0FB3206B6F859',
				addonId: 'ArmaReforger',
				thumbnailColor: '#12abef',
				logicalPath: 'Prefabs/Props/Radio.et',
				basename: 'Radio.et',
				name: 'Radio',
				extension: 'et',
				workbenchLink: 'enfusion://ResourceManager/~ArmaReforger:Prefabs/Props/Radio.et',
			}],
		}), [{
			id: 'game-data-resource-0-{58D0FB3206B6F859}Prefabs/Props/Radio.et',
			source: 'gameData',
			kind: 'resource',
			title: 'Radio.et',
			detail: 'et',
			path: 'Prefabs/Props/Radio.et',
			excerpt: '',
			matchKind: 'resource',
			readInput: {},
			resourceName: '{58D0FB3206B6F859}Prefabs/Props/Radio.et',
			workbenchLink: 'enfusion://ResourceManager/~ArmaReforger:Prefabs/Props/Radio.et',
			addonGuid: '58D0FB3206B6F859',
			addonLabel: 'ArmaReforger',
			thumbnailColor: '#12ABEF',
		}]);
	});

	test('maps script resources to complete VS Code source previews', () => {
		assert.deepStrictEqual(normalizeResourceSearchPage({
			results: [{
				resourceName: '{58D0FB3206B6F859}Scripts/Game/SCR_Radio.c',
				addonGuid: '58D0FB3206B6F859',
				addonId: 'ArmaReforger',
				logicalPath: 'Scripts/Game/SCR_Radio.c',
				basename: 'SCR_Radio.c',
				extension: 'c',
				workbenchLink: 'enfusion://ScriptEditor/Scripts/Game/SCR_Radio.c',
			}],
		}, 'gd1:revision')[0].readInput, {
			catalogueRevision: 'gd1:revision',
			addonGuid: '58D0FB3206B6F859',
			relativePath: 'Scripts/Game/SCR_Radio.c',
		});
		assert.match(searchUiSource, /hit\.kind === 'resource' && !hit\.readInput\.relativePath/);
		assert.match(searchUiSource, /client\.readComplete\(hit\)/);
		assert.match(searchUiSource, /showTextDocument\(documentWithLanguage, \{ preview: true \}\)/);
	});

	test('keeps string tables on the local Resource Manager protocol', () => {
		const target = localWorkbenchResourceLinkFor({
			workbenchLink: 'enfusion://ResourceManager/~ArmaReforger:Language/localization.st',
		});
		const expected = 'enfusion://ResourceManager/~ArmaReforger:Language/localization.st';
		assert.strictEqual(target, expected);
		assert.strictEqual(localWorkbenchResourceLinkFor({
			workbenchLink: 'enfusion://ResourceManager/~ArmaReforger:Prefabs/Props/Radio.et',
		}), 'enfusion://ResourceManager/~ArmaReforger:Prefabs/Props/Radio.et');
		assert.strictEqual(localWorkbenchResourceLinkFor({
			workbenchLink: 'https://enfusionengine.com/api/redirect?to=enfusion://ResourceManager/~ArmaReforger:Language/localization.st',
		}), undefined);
	});

	test('opens Workbench resources only when their add-on is in the live project context', () => {
		const resource = {
			kind: 'resource' as const,
			addonLabel: 'ArmaReforger',
		};
		assert.strictEqual(resourceAddonIsLoaded(resource, ['TestBullshit', 'armareforger']), true);
		assert.strictEqual(resourceAddonIsLoaded(resource, ['TestBullshit']), false);
		assert.strictEqual(resourceAddonIsLoaded({ kind: 'resource' }, ['ArmaReforger']), false);
		assert.strictEqual(workbenchResourceOpenState(resource, {
			loadedAddons: ['ArmaReforger'], loadedAddonsTruncated: false,
		}), 'loaded');
		assert.strictEqual(workbenchResourceOpenState(resource, {
			loadedAddons: [], loadedAddonsTruncated: false,
		}), 'not-loaded');
		assert.strictEqual(workbenchResourceOpenState(resource, {
			loadedAddons: [], loadedAddonsTruncated: true,
		}), 'unconfirmed');
		assert.match(searchClientSource, /callTool\('workbench_project_context', \{\}\)/);
		assert.match(searchUiSource, /workbenchResourceOpenState\(hit, projectContext\)/);
		assert.match(searchUiSource, /The add-on .* is not loaded in Workbench/);
		assert.match(searchUiSource, /could not be confirmed because Workbench returned an incomplete loaded add-on list/);
		assert.match(searchUiSource, /did not identify its owning add-on/);
	});

	test('rejects malformed live Workbench project context', () => {
		assert.deepStrictEqual(normalizeWorkbenchProjectContext({
			loadedAddons: ['ArmaReforger'],
			loadedAddonsTruncated: false,
		}), {
			loadedAddons: ['ArmaReforger'],
			loadedAddonsTruncated: false,
		});
		assert.throws(
			() => normalizeWorkbenchProjectContext({ loadedAddons: [] }),
			/The live Workbench project context was malformed/,
		);
	});

	test('offers resource context actions and enables Explorer reveal only for loose files', () => {
		const [loose] = normalizeResourceSearchPage({
			results: [{
				resourceName: '{C014582791ECBF24}Language/localization.st',
				addonGuid: 'C014582791ECBF24',
				addonId: 'ArmaReforger',
				logicalPath: 'Language/localization.st',
				basename: 'localization.st',
				extension: 'st',
				physicalPath: 'C:\\Addons\\ArmaReforger\\Language\\localization.st',
				workbenchLink: 'enfusion://ResourceManager/~ArmaReforger:Language/localization.st',
			}],
		});
		assert.strictEqual(
			resourcePathForClipboard(loose),
			'{C014582791ECBF24}Language/localization.st',
		);
		assert.strictEqual(
			resourcePhysicalPathFor(loose),
			'C:\\Addons\\ArmaReforger\\Language\\localization.st',
		);
		assert.strictEqual(resourcePhysicalPathFor({ kind: 'resource' }), undefined);
		assert.match(searchUiSource, /data-copy-resource>Copy Resource Path/);
		assert.match(searchUiSource, /data-reveal-resource[\s\S]*Show in File Explorer/);
		assert.match(searchUiSource, /result\.resourcePhysicalPath \? '' : ' disabled aria-disabled="true"/);
		assert.match(searchUiSource, /vscode\.env\.clipboard\.writeText\(resourcePath\)/);
		assert.match(searchUiSource, /executeCommand\('revealFileInOS', vscode\.Uri\.file\(physicalPath\)\)/);
		assert.match(searchUiSource, /element\.addEventListener\('contextmenu', event => openResourceContextMenu\(element, event\)\)/);
	});

	test('offers a resource search mode backed by the canonical Workbench tools', () => {
		assert.match(searchUiSource, /data-mode="resource">Resources/);
		assert.match(searchUiSource, /resourceResultTypes/);
		assert.match(searchUiSource, /resourceKindsFor\(typeValue\)/);
		assert.match(searchUiSource, /hit\.kind === 'resource'/);
		assert.match(searchClientSource, /search_game_data_resources/);
		assert.match(searchUiSource, /workbenchLink/);
		assert.match(searchUiSource, /const resultPreview = result => result\.kind === 'resource'[\s\S]*?\? ''/);
		assert.match(searchUiSource, /const resultTitle = result => result\.kind === 'resource' \? result\.path : result\.title/);
		assert.match(searchUiSource, /const resultPath = result => result\.kind === 'resource' \? ''/);
		assert.match(searchUiSource, /const hasActiveSearch = \(\) =>/);
		assert.match(searchUiSource, /includeLayoutToggle && state\.mode !== 'resource'/);
		assert.match(searchUiSource, /const navigationDisabled = !hasActiveSearch\(\)/);
		assert.match(searchUiSource, /mode === 'semantic' && state\.type !== 'all'/);
		assert.match(searchUiSource, /message\.refreshSearch === true && !state\.query\.trim\(\) && !state\.relationAnchor && hasActiveSearch\(\)/);
		assert.match(searchClientSource, /const basename = asString\(hit\.basename/);
	});

	test('supersedes an in-flight search when a mode or type filter changes', () => {
		assert.match(searchUiSource, /function cancelInFlightSearch\(active: ActiveSearch\): void/);
		assert.match(searchUiSource, /cancelInFlightSearch\(active\);/);
		assert.match(searchUiSource, /active\.client = undefined;/);
		assert.match(searchUiSource, /previousClient\.then\(client => client\.dispose\(\)/);
		assert.match(searchUiSource, /state\.page = 1; render\(false\); search\(true\);/);
	});

	test('refreshes a text cursor once when its source revision changes between pages', () => {
		assert.match(searchClientSource, /function isInvalidTextCursor\(error: unknown\): boolean/);
		assert.match(searchClientSource, /pages\.clear\(\);[\s\S]*?return this\.searchPage\(/);
		assert.match(searchClientSource, /retryInvalidCursor = true/);
		assert.match(searchClientSource, /cursor is invalid for this text query or source revision/);
	});

	test('shows one human-facing add-on name in Search Scope', () => {
		assert.strictEqual(
			addonScopeLabel('GlobalConflictsCore (Global Conflicts CORE)', 'GlobalConflictsCore', '623555110E2B2CA0'),
			'Global Conflicts CORE',
		);
		assert.strictEqual(
			addonScopeLabel('ACE_Captives_Dev (ACE Captives Dev)', 'ACE Captives Dev', '0000000000000000'),
			'ACE Captives Dev',
		);
		assert.strictEqual(
			addonScopeLabel('core (Enfusion core data)', 'Enfusion core data', '0000000000000000'),
			'Enfusion core data',
		);
		assert.strictEqual(addonScopeLabel('Arma Reforger', 'ArmaReforger', '58D0FB3206B6F859'), 'Arma Reforger');
	});

	test('routes each source to its authoritative search tool', () => {
		assert.strictEqual(searchToolFor('workspace'), 'search_workspace_symbols');
		assert.strictEqual(searchToolFor('gameData'), 'search_game_data_symbols');
		assert.strictEqual(searchToolFor('wiki'), 'search_official_wiki');
		assert.strictEqual(searchToolFor('workspace', 'text'), 'search_workspace_text');
		assert.strictEqual(searchToolFor('gameData', 'text'), 'search_game_data_text');
		assert.strictEqual(searchToolFor('wiki', 'text'), 'search_official_wiki');
	});

	test('maps literal text matches to exact line previews and source-read handoffs', () => {
		const results = normalizeSearchPage('workspace', {
			results: [{
				relativePath: 'Scripts/Example.c',
				matchRange: { startLine: 12, startCharacter: 18, endLine: 12, endCharacter: 22 },
				excerptMatchStart: 17,
				excerpt: '    void Run() { SCR_(); }',
				matchText: 'SCR_',
				sourceUri: 'file:///C:/Addons/Example/Scripts/Example.c',
				readSourceInput: {
					catalogueRevision: 'ws1:revision',
					relativePath: 'Scripts/Example.c',
					startLine: 12,
				},
			}],
		}, 'text');

		assert.strictEqual(results[0].kind, 'text');
		assert.strictEqual(results[0].excerpt, 'void Run() { SCR_(); }');
		assert.strictEqual(results[0].textMatchStart, 13);
		assert.strictEqual(results[0].textMatchLength, 4);
		assert.strictEqual(results[0].sourceUri, 'file:///C:/Addons/Example/Scripts/Example.c');
		assert.strictEqual(results[0].readInput.catalogueRevision, 'ws1:revision');
	});

	test('queues script text matches for asynchronous semantic coloring', () => {
		assert.match(searchUiSource, /const semanticHits = previewHits\.filter\(hit => hit\.source !== 'wiki'\)/);
		assert.match(searchUiSource, /length: Math\.min\(4, semanticHits\.length\)/);
		assert.match(searchUiSource, /const rawPreview = \{ hit, document, previewLine, preview, matchRange, autoContext, semanticDocument \};[\s\S]*?queueRawPreview\(hit\.id\);[\s\S]*?queueSemanticPreview\(rawPreview\);/);
		assert.match(searchUiSource, /else if \(hit\.kind === 'text'\) \{\s*return undefined;/);
		assert.match(searchUiSource, /state\.sourcePreviews\[result\.id\] \?\? \(result\.kind === 'text' \? result\.excerpt : undefined\)/);
	});

	test('routes explicit text mode to corpus-specific literal search tools', () => {
		assert.strictEqual(searchToolFor('workspace', 'text'), 'search_workspace_text');
		assert.strictEqual(searchToolFor('gameData', 'text'), 'search_game_data_text');
		const results = normalizeSearchPage('workspace', {
			results: [{
				relativePath: 'Game/Use.c',
				matchRange: { startLine: 4, startCharacter: 13, endLine: 4, endCharacter: 17 },
				excerpt: '    void Run(SCR_ value);',
				matchText: 'SCR_',
				readSourceInput: { catalogueRevision: 'ws1:test', relativePath: 'Game/Use.c', startLine: 4 },
			}],
		}, 'text');
		assert.strictEqual(results[0].kind, 'text');
		assert.strictEqual(results[0].title, 'SCR_');
		assert.strictEqual(results[0].textMatchStart, 9);
		assert.strictEqual(results[0].textMatchLength, 4);
		assert.strictEqual(results[0].readInput.catalogueRevision, 'ws1:test');
	});

	test('formats compound symbol kinds for result details', () => {
		assert.strictEqual(formatSearchKind('enumMember'), 'enum Member');
		assert.strictEqual(formatSearchKind('globalField'), 'global Field');
		assert.strictEqual(formatSearchKind('method'), 'function');
		assert.strictEqual(formatSearchKind('constructor'), 'function');
		assert.strictEqual(formatSearchKind('destructor'), 'function');
	});

	test('preserves the authoritative symbol kind for result accents', () => {
		const results = normalizeSearchPage('workspace', {
			results: [{
				name: 'SCR_Example',
				kind: 'class',
				qualifiedName: 'SCR_Example',
				relativePath: 'Scripts/Example.c',
				declarationRange: { startLine: 4 },
				selectionRange: { startLine: 4, endLine: 4 },
				readSourceInput: { relativePath: 'Scripts/Example.c' },
			}],
		});

		assert.strictEqual(results[0].symbolKind, 'class');
	});

	test('extracts the authoritative source line for a symbol preview', () => {
		assert.strictEqual(sourceLinePreview({ content: '    class SCR_Mode\n', startLine: 18, endLine: 18 }, 18), 'class SCR_Mode');
		assert.strictEqual(sourceLinePreview({ content: 'only line', startLine: 0, endLine: 0 }, 1), 'only line');
	});

	test('renders a configurable source context around the selected preview line', () => {
		const document = { content: 'one\n  two\n\nthree\nfour\nfive', startLine: 1, endLine: 6 };
		assert.strictEqual(sourceContextPreview(document, 4, 1), 'three');
		assert.strictEqual(sourceContextPreview(document, 4, 2), '  two\n\nthree\nfour\nfive');
		assert.match(searchUiSource, /data-preview-context-down/);
		assert.match(searchUiSource, /data-preview-context-auto/);
		assert.match(searchUiSource, /data-preview-context-up/);
		assert.match(searchUiSource, /previewContextLines === 0 \? 'Auto' : previewContextLines/);
		assert.match(searchUiSource, /class="context-auto-icon" aria-hidden="true"/);
		assert.doesNotMatch(searchUiSource, /context-stepper-label[^\n]*>Context</);
		assert.match(searchUiSource, /type: 'previewContext', contextLines: nextContextLines/);
		assert.match(searchUiSource, /vscode\.postMessage\(\{ type: 'previewContext', contextLines: nextContextLines \}\)/);
		assert.match(searchUiSource, /\[data-preview-context-auto\][\s\S]*?setPreviewContext\(0\)/);
		assert.match(searchUiSource, /previewContextLines: numberField\(snapshot\.previewContextLines\)/);
	});

	test('round-trips explicit context back to Auto immediately', () => {
		assert.match(searchUiSource, /const nextContextLines = Math\.max\(0, Math\.min\(249, value\)\)/);
		assert.match(searchUiSource, /previewContextLines = nextContextLines;[\s\S]*?vscode\.postMessage\(\{ type: 'previewContext', contextLines: nextContextLines \}\)/);
		assert.doesNotMatch(searchUiSource, /previewContextTimer|setTimeout\(\(\) => vscode\.postMessage\(\{ type: 'previewContext'/);
	});

	test('anchors previews to the declaration and removes comments', () => {
		const document = { content: '// field documentation\n    SCR_Field value; // trailing note\n', startLine: 10, endLine: 11 };
		assert.strictEqual(sourcePreviewLine(document, 10, 'SCR_Field'), 11);
		assert.strictEqual(sourceLinePreview(document, 10, 'SCR_Field'), 'SCR_Field value;');
		assert.strictEqual(stripSourceComments('const url = "https://example.test"; // note'), 'const url = "https://example.test"; ');
	});

	test('finds the selected symbol occurrence instead of every query occurrence', () => {
		assert.deepStrictEqual(sourceMatchRange('void Foo(Foo value)', 'Foo'), { start: 5, length: 3 });
		assert.deepStrictEqual(sourceMatchRange('void foo()', 'FOO'), { start: 5, length: 3 });
		assert.strictEqual(sourceMatchRange('void Bar()', 'Foo'), undefined);
		assert.doesNotMatch(searchUiSource, /highlightText\(result\.excerpt, state\.query \+ ' ' \+ result\.title\)/);
		assert.match(searchUiSource, /startPreviewHydration\(active, client, requestId, result\.results, normalizedQuery\)/);
		assert.match(searchUiSource, /const matchRange = sourceMatchRange\(preview, query\);/);
	});

	test('decodes the language server semantic token legend for a preview line', () => {
		assert.deepStrictEqual(semanticTokenSpansForLine([
			0, 4, 5, 0, 0,
			1, 0, 8, 7, 0,
		], 1), [{ start: 0, length: 8, role: 'enumMember' }]);
	});

	test('keeps semantic token offsets across a multi-line preview', () => {
		assert.match(searchUiSource, /semanticPreviewForLines\(/);
		assert.match(searchUiSource, /active\.previewContextLines === 0 && autoContext[\s\S]*?semanticPreviewForLines/);
		assert.match(searchUiSource, /active\.previewContextLines <= 1[\s\S]*?semanticPreviewForLine/);
		assert.match(searchUiSource, /line - active\.previewContextLines/);
		assert.match(searchUiSource, /line \+ active\.previewContextLines/);
		assert.strictEqual(typeof semanticPreviewForLine, 'function');
		assert.strictEqual(typeof semanticPreviewForLines, 'function');
	});

	test('defines useful symbol kind filters without a documentation duplicate', () => {
		assert.deepStrictEqual(searchKindFilters.map(filter => filter.label), [
			'All results',
			'Classes',
			'Functions',
			'Fields',
			'Enums',
		]);
		assert.deepStrictEqual(searchKindFilters.find(filter => filter.value === 'function')?.kinds, [
			'function',
			'method',
			'constructor',
			'destructor',
		]);
		assert.doesNotMatch(searchUiSource, /Documentation/);
	});

	test('keeps full-text search explicit inside the existing search UI', () => {
		assert.doesNotMatch(searchUiSource, /Search the indexed workspace, loaded add-ons/);
		assert.doesNotMatch(searchUiSource, /class="intro"/);
		assert.match(searchUiSource, /const modeButtons = \(\) =>/);
		assert.match(searchUiSource, /state\.mode === 'text'/);
		assert.match(searchUiSource, /data-mode="semantic"/);
		assert.match(searchUiSource, /data-mode="text"/);
		assert.match(searchUiSource, /state\.mode === 'resource' \? resourceResultTypes : resultTypes/);
		assert.match(searchUiSource, /query\.addEventListener\('input', event => \{ state\.query = event\.target\.value; pendingQuerySelection = \{ start: event\.target\.selectionStart \?\? state\.query\.length, end: event\.target\.selectionEnd \?\? state\.query\.length \}; if \(state\.mode !== 'text'\) search\(true\); \}\)/);
		assert.doesNotMatch(searchUiSource, /searchTimer|scheduleSearch/);
		assert.match(searchUiSource, /searchMode: state\.mode/);
		assert.match(searchClientSource, /search_workspace_text/);
		assert.match(searchClientSource, /search_game_data_text/);
		assert.match(searchClientSource, /paginationMode: paginationModeFor\(mode, sources\)/);
		assert.match(searchClientSource, /mode === 'semantic'[\s\S]*sources\.filter\(source => source !== 'wiki'\)/);
		assert.match(searchUiSource, /state\.scopeSources\.filter\(source => source\.kind !== 'wiki' \|\| state\.mode === 'text'\)/);
		assert.match(searchUiSource, /selectedEligibleScopeIds\(\)/);
	});

	test('focuses the search field when reopening the panel and routes unclaimed typing to it', () => {
		assert.match(searchUiSource, /activePanel\.webview\.postMessage\(\{ type: 'focusQuery' \}\)/);
		assert.match(searchUiSource, /message\.type === 'focusQuery'\) \{ document\.getElementById\('query'\)\?\.focus\(\); return; \}/);
		assert.match(searchUiSource, /event\.data\?\.type !== 'focusQuery'[\s\S]*?query\.setSelectionRange\(query\.value\.length, query\.value\.length\)/);
		assert.match(searchUiSource, /document\.activeElement !== document\.body \|\| event\.ctrlKey \|\| event\.altKey \|\| event\.metaKey \|\| event\.isComposing \|\| event\.key\.length !== 1/);
		assert.match(searchUiSource, /query\.setRangeText\(event\.key, query\.selectionStart, query\.selectionEnd, 'end'\);/);
		assert.match(searchUiSource, /query\.dispatchEvent\(new Event\('input', \{ bubbles: true \}\)\);/);
	});

	test('preserves a query selection when an asynchronous search response rerenders the page', () => {
		assert.match(searchUiSource, /function render\(focusQuery = false\)/);
		assert.match(searchUiSource, /\nrender\(true\);\n<\/script>/);
		assert.match(searchUiSource, /pendingQuerySelection = \{ start: event\.target\.selectionStart \?\? state\.query\.length, end: event\.target\.selectionEnd \?\? state\.query\.length \}/);
		assert.match(searchUiSource, /else if \(pendingQuerySelection && query\) \{ query\.focus\(\); query\.setSelectionRange\(pendingQuerySelection\.start, pendingQuerySelection\.end\); pendingQuerySelection = undefined; \}/);
		assert.match(searchUiSource, /query\.focus\(\);\s+query\.setSelectionRange\(query\.value\.length, query\.value\.length\);\s+query\.setRangeText\(event\.key/);
	});

	test('keeps the primary search row free of decorative readiness status', () => {
		assert.doesNotMatch(searchUiSource, /Index ready|Query running/);
		assert.doesNotMatch(searchUiSource, /search-brand-mark|<span class="search-brand"/);
		assert.doesNotMatch(searchUiSource, /Limited to|result-limit/);
	});

	test('focuses and preserves the selection of the selected-source filter', () => {
		assert.match(searchUiSource, /if \(state\.scopeOpen\) focusScopeFilter\(state\.scopeFilter\.length\);/);
		assert.match(searchUiSource, /const selectionStart = element\.selectionStart \?\? element\.value\.length;[\s\S]*?focusScopeFilter\(selectionStart, selectionEnd\);/);
		assert.doesNotMatch(searchUiSource, /filter\.setSelectionRange\(filter\.value\.length, filter\.value\.length\)/);
	});

	test('uses discovered loaded add-ons as the production Search Scope', () => {
		assert.match(searchClientSource, /public async discoverScope\(\): Promise<SearchScopeDiscovery>/);
		assert.match(searchClientSource, /this\.callTool\('game_data_status', \{\}\)/);
		assert.match(searchClientSource, /defaultSelected: addon\.defaultSelected === true/);
		assert.doesNotMatch(searchClientSource, /baseGameScopeId|enfusionCoreScopeId/);
		assert.match(searchUiSource, /scopeOpen: false/);
		assert.ok(
			searchClientSource.indexOf("...(addonGuids.length > 0 ? ['gameData' as const] : [])")
				< searchClientSource.indexOf("...(normalizedScopes.includes(workspaceScopeId) ? ['workspace' as const] : [])"),
		);
		assert.match(searchUiSource, /const searchScope = \(\) =>/);
		assert.match(searchUiSource, /data-scope-choice/);
		assert.match(searchUiSource, /data-scope-all/);
		assert.doesNotMatch(searchUiSource, /data-scope-refresh/);
		assert.match(searchUiSource, /async function refreshSearchScope/);
		assert.doesNotMatch(searchUiSource, /type: 'refreshScope'/);
		assert.match(searchUiSource, /scopeRefresh: Promise<void> \| undefined/);
		assert.match(searchUiSource, /scopeRefreshPending: boolean/);
		assert.match(searchUiSource, /<div class="scope-actions"><input class="addon-filter"[\s\S]*?<button type="button" data-scope-all>/);
		assert.match(searchUiSource, /Select all/);
		assert.match(searchUiSource, /Unselect all/);
		assert.match(searchUiSource, /const allEligibleScopesSelected = \(\) => \{/);
		assert.match(searchUiSource, /eligible\.every\(source => state\.selectedScopeIds\.includes\(source\.id\)\)/);
		assert.match(searchUiSource, /const allSelected = allEligibleScopesSelected\(\);/);
		assert.match(searchUiSource, /allSelected \? 'Unselect all' : 'Select all'/);
		assert.match(searchUiSource, /selectedScopeIds/);
		assert.match(searchUiSource, /selectionTouched/);
		assert.match(searchUiSource, /message\.type === 'scope'/);
		assert.match(searchUiSource, /nextSources\.filter\(source => source\.defaultSelected\)/);
		assert.match(searchUiSource, /selected\.length > 1/);
		assert.match(searchUiSource, /class="scope-count">\+' \+ \(selected\.length - 1\)/);
		assert.match(searchUiSource, /<button class="addon-trigger"[\s\S]*?<span class="addon-summary">/);
		assert.doesNotMatch(searchUiSource, /addon-chips/);
		assert.doesNotMatch(searchUiSource, /addonPrototypeSources/);
		assert.doesNotMatch(searchUiSource, /SEARCH IN/);
		assert.match(searchUiSource, /No search scopes selected\./);
		assert.match(searchUiSource, /scopeDiscoveryMs/);
		assert.match(searchUiSource, /entry\.addonTotals = jsonField\(source\.addonTotals\)/);
		assert.match(searchUiSource, /readMsByAddon/);
		assert.match(searchUiSource, /readFailuresByAddon/);
		assert.match(searchUiSource, /unavailableScopeIds/);
		assert.match(searchClientSource, /unavailableScopeIds/);
		assert.doesNotMatch(searchUiSource, /\.addon-choice\.workspace/);
		assert.match(searchUiSource, /source\.pinned && !filtered\[index \+ 1\]\?\.pinned/);
		assert.match(searchUiSource, /pinned-boundary/);
		assert.match(searchUiSource, /\.scope-actions \[data-scope-all\] \{ flex: 0 0 76px; width: 76px; box-sizing: border-box;/);
		assert.match(searchUiSource, /document\.addEventListener\('click', event => \{/);
		assert.match(searchUiSource, /event\.target\.closest\('\.search-scope'\)/);
		assert.match(searchUiSource, /document\.querySelector\('\.addon-menu'\)\?\.remove\(\)/);
		assert.match(searchClientSource, /workspaceScopeId[\s\S]*?wikiScopeId[\s\S]*?\.\.\.addonSources/);
		assert.match(searchClientSource, /wikiScopeId[^\n]+pinned: true/);
		assert.match(searchUiSource, /class="control-block search-scope-control"><div class="group-label">SEARCH SCOPE<\/div>' \+ searchScope\(\)/);
	});

	test('runs one trailing scope refresh when loaded add-ons change during a refresh', async () => {
		let releaseFirstRefresh: (() => void) | undefined;
		const firstRefreshBlocked = new Promise<void>(resolve => {
			releaseFirstRefresh = resolve;
		});
		let firstRefreshStarted: (() => void) | undefined;
		const firstRefreshRunning = new Promise<void>(resolve => {
			firstRefreshStarted = resolve;
		});
		const state = {
			scopeRefresh: undefined as Promise<void> | undefined,
			scopeRefreshPending: false,
			disposed: false,
		};
		let refreshCount = 0;
		const refresh = async () => {
			refreshCount += 1;
			if (refreshCount === 1) {
				firstRefreshStarted?.();
				await firstRefreshBlocked;
			}
		};

		const initialRefresh = queueSearchScopeRefresh(state, refresh);
		await firstRefreshRunning;
		const deltaRefresh = queueSearchScopeRefresh(state, refresh);
		const coalescedDeltaRefresh = queueSearchScopeRefresh(state, refresh);
		releaseFirstRefresh?.();
		await Promise.all([initialRefresh, deltaRefresh, coalescedDeltaRefresh]);

		assert.strictEqual(refreshCount, 2);
		assert.strictEqual(state.scopeRefresh, undefined);
		assert.strictEqual(state.scopeRefreshPending, false);
	});

	test('uses one concise Source Search heading over the shared grouped match surface', () => {
		assert.match(searchUiSource, /function renderSearchUi\(\): string/);
		assert.doesNotMatch(searchUiSource, /prototypeSwitcher|pageRenderers|prototypeVariant/);
		assert.match(searchUiSource, /class="search-masthead"/);
		assert.match(searchUiSource, /<section class="search-masthead"><h1>Source Search<\/h1><\/section>/);
		assert.doesNotMatch(searchUiSource, /Source intelligence|Search the source atlas|Trace symbols, text, and resources/);
		assert.match(searchUiSource, /class="search-primary"[\s\S]*?class="search-query"[\s\S]*?class="search-count"/);
		assert.match(searchUiSource, /class="search-secondary"[\s\S]*?SEARCH SCOPE[\s\S]*?SEARCH MODE[\s\S]*?typeControl \+ pageControls\(true\)/);
		assert.match(searchUiSource, /\.search-scope-control \{ width: 180px;/);
		assert.match(searchUiSource, /const overflow = selected\.length > 1 \? '<span class="scope-count">\+'/);
		assert.doesNotMatch(searchUiSource, /addon-tags|addon-chip/);
		assert.match(searchUiSource, /\.atlas-card \{[^}]*border-left: 3px solid var\(--result-accent\);/);
		assert.match(searchUiSource, /\.atlas-card\.result-class/);
		assert.match(searchUiSource, /\.atlas-card\.result-string/);
		assert.match(searchUiSource, /\.atlas-card\.result-class \{ --result-accent: #40b5ac; \}/);
		assert.match(searchUiSource, /\.atlas-card\.result-function \{ --result-accent: #f3ad58; \}/);
		assert.match(searchUiSource, /\.atlas-card\.result-enum \{ --result-accent: #40b5ac; \}/);
		assert.match(searchUiSource, /\.atlas-card\.result-string \{ --result-accent: #c178dd; \}/);
		assert.match(searchUiSource, /const resultAccent = result =>/);
		assert.match(searchUiSource, /const resultGroups = \(\) => \{/);
		assert.match(searchUiSource, /visibleResults\(\)\.forEach\(result => \{/);
		assert.match(searchUiSource, /<section class="atlas-group"><div class="atlas-group-head"/);
		assert.match(searchUiSource, /--addon-header-color/);
		assert.match(searchUiSource, /const addonHeaderColor = result =>[\s\S]*?result\.thumbnailColor/);
		assert.doesNotMatch(searchUiSource, /scopeSources\.find\(source => source\.id === guid\)/);
		assert.doesNotMatch(searchUiSource, /\.atlas-group \{[^}]*--addon-header-color/);
		assert.doesNotMatch(searchUiSource, /\.atlas-card \{[^}]*--addon-header-color/);
		assert.match(searchUiSource, /results\.map\(resultCard\)\.join\(''\)/);
		assert.match(searchUiSource, /\.atlas-results\.masonry \{ display: block; column-count: 2;/);
		assert.match(searchUiSource, /const sharedMatchArea = \(\) => warnings \+ resultBody\(resultGroups\(\)\) \+ bottomPager/);
		assert.doesNotMatch(searchUiSource, /atlas-lanes|atlas-lane/);
	});

	test('executes semantic relationship expansion without adding another result type', () => {
		assert.match(searchUiSource, /state\.mode === 'semantic' \? relationControl\(\) : ''/);
		assert.match(searchUiSource, /class="relation-picker"/);
		assert.match(searchUiSource, /Related code/);
		assert.match(searchUiSource, /Parent classes/);
		assert.match(searchUiSource, /Child classes/);
		assert.match(searchUiSource, /Base implementations/);
		assert.match(searchUiSource, /Overrides/);
		assert.match(searchUiSource, /Modded extensions/);
		assert.match(searchUiSource, /data-relation-depth="direct"/);
		assert.match(searchUiSource, /data-relation-depth="all"/);
		assert.doesNotMatch(searchUiSource, /UI prototype only|results are not filtered yet/);
		assert.match(searchUiSource, /relationIncludes: \['direct'\], relationDepth: 'direct'/);
		assert.match(searchUiSource, /relationAnchor: null/);
		assert.match(searchUiSource, /Choose an exact declaration/);
		assert.match(searchUiSource, /data-relation-anchor/);
		assert.match(searchUiSource, /data-clear-relation-anchor/);
		assert.match(searchUiSource, /state\.relationAnchor\?\.symbolKind === 'class'/);
		assert.match(searchUiSource, /state\.relationAnchor\?\.symbolKind === 'method'/);
		assert.match(searchUiSource, /Your text can be broad; relationships begin from one selected symbol\./);
		assert.doesNotMatch(searchUiSource, /resultTypes = [^\n]+Related code/);
		assert.match(searchUiSource, /relationshipAnchor/);
		assert.match(searchUiSource, /relationIncludes/);
		assert.match(searchClientSource, /query_source_symbol_relationships/);
	});

	test('executes relationship webview state transitions through the shipped runtime script', () => {
		const harness = createSearchWebviewHarness();
		harness.message({
			type: 'scope',
			scope: {
				scopeRevision: 'scope-1',
				sources: [
					{ id: 'workspace', label: 'Workspace', detail: 'Live', kind: 'workspace', defaultSelected: true },
					{ id: 'game-guid', label: 'Game', detail: 'Loaded', kind: 'addon', defaultSelected: true },
					{ id: 'wiki', label: 'Wiki', detail: 'Docs', kind: 'wiki', defaultSelected: true },
				],
			},
		});
		harness.evaluate("state.query = 'Vehicle'");
		harness.message({
			type: 'results',
			requestId: 1,
			page: 1,
			pageSize: 25,
			total: 60,
			results: [{
				id: 'vehicle', source: 'gameData', addonGuid: 'game-guid', addonLabel: 'Game',
				kind: 'symbol', symbolKind: 'class', symbolRef: 'sr2:vehicle', title: 'Vehicle',
				qualifiedName: 'Vehicle', signature: 'class Vehicle', detail: 'class', path: 'Vehicle.c', excerpt: 'class Vehicle',
			}],
		});

		harness.evaluate("chooseRelationAnchor('vehicle')");
		assert.deepStrictEqual(harness.evaluate('state.relationIncludes'), ['direct']);
		assert.strictEqual(harness.evaluate('state.relationAnchor.symbolRef'), 'sr2:vehicle');
		assert.deepStrictEqual(
			harness.evaluate('relationOptionsForAnchor().map(option => option.value)'),
			['direct', 'parents', 'children', 'modded'],
		);
		harness.evaluate("setRelationChoice('parents', true)");
		harness.evaluate("setRelationChoice('direct', false)");
		harness.evaluate("setRelationChoice('parents', false)");
		assert.deepStrictEqual(harness.evaluate('state.relationIncludes'), ['parents'], 'the last relationship choice remains selected');
		harness.evaluate("setRelationDepth('all')");
		assert.strictEqual(harness.evaluate('state.relationDepth'), 'all');
		assert.strictEqual(lastPosted(harness.posted, 'search').relationDepth, 'all');

		harness.evaluate("state.status = 'ready'; requestPage(2)");
		assert.strictEqual(lastPosted(harness.posted, 'search').page, 2);
		harness.evaluate("state.relationAnchor.symbolKind = 'method'");
		assert.deepStrictEqual(
			harness.evaluate('relationOptionsForAnchor().map(option => option.value)'),
			['direct', 'baseMembers', 'overrides'],
		);

		harness.evaluate("setScopeChoice('game-guid', false)");
		assert.strictEqual(harness.evaluate('state.relationAnchor'), null, 'removing the anchor source clears relationship mode');
		assert.strictEqual(lastPosted(harness.posted, 'search').query, 'Vehicle');
		harness.evaluate("setScopeChoice('game-guid', true); state.status = 'ready'; chooseRelationAnchor('vehicle')");
		harness.message({
			type: 'error', requestId: 20, clearRelationshipAnchor: true,
			message: 'Stale anchor', recovery: 'Rerun semantic discovery.',
		});
		assert.strictEqual(harness.evaluate('state.relationAnchor'), null);
		assert.strictEqual(lastPosted(harness.posted, 'search').relationshipAnchor, undefined);

		harness.evaluate("state.status = 'ready'; chooseRelationAnchor('vehicle'); setSearchMode('text')");
		assert.strictEqual(harness.evaluate('state.mode'), 'text');
		assert.strictEqual(harness.evaluate('state.relationAnchor'), null);

		harness.evaluate(`state.results = [
			{ id: 'vehicle', source: 'gameData', addonLabel: 'Game', kind: 'symbol', symbolKind: 'class', title: 'Vehicle', detail: 'class', path: 'Vehicle.c', excerpt: 'class Vehicle', relationshipKind: 'direct' },
			{ id: 'child', source: 'workspace', kind: 'symbol', symbolKind: 'class', title: 'Car', detail: 'class', path: 'Car.c', excerpt: 'class Car', relationshipKind: 'derivedType', relationshipDistance: 2 }
		]`);
		const grouped = harness.evaluate<string>('resultGroups()');
		assert.match(grouped, /<h2>Game<\/h2>/);
		assert.match(grouped, /<h2>Workspace<\/h2>/);
		assert.match(grouped, /Child/);
		harness.evaluate('toggleResultLayout()');
		assert.match(harness.evaluate<string>('resultGroups()'), /atlas-results masonry/);
		harness.evaluate('setPreviewContext(3); setPreviewContext(0)');
		assert.strictEqual(lastPosted(harness.posted, 'previewContext').contextLines, 0);
		harness.evaluate("openResult({ dataset: { open: 'child' } })");
		assert.strictEqual(lastPosted(harness.posted, 'open').id, 'child');
	});

	test('normalizes cross-source relationship declarations into the existing result-card contract', () => {
		const results = normalizeSourceRelationshipPage({
			results: [{
				source: 'gameData',
				addonGuid: '58D0FB3206B6F859',
				addonLabel: 'Arma Reforger',
				symbolRef: 'sr2:child',
				name: 'Car',
				kind: 'class',
				qualifiedName: 'Car',
				signature: 'class Car : Vehicle',
				sourceCategory: 'game',
				relativePath: 'Game/Vehicles/Car.c',
				sourceUri: 'reforger-pak://58D0FB3206B6F859/current/Game/Vehicles/Car.c',
				relationshipKind: 'derivedType',
				distance: 2,
				evidence: 'indexed class base type and exact script module',
				declarationRange: { startLine: 4, endLine: 20 },
				selectionRange: { startLine: 4, endLine: 4 },
				readSourceInput: { catalogueRevision: 'gd1:test', addonGuid: '58D0FB3206B6F859', relativePath: 'Game/Vehicles/Car.c', startLine: 4 },
			}],
		});
		assert.strictEqual(results.length, 1);
		assert.strictEqual(results[0].source, 'gameData');
		assert.strictEqual(results[0].symbolKind, 'class');
		assert.strictEqual(results[0].relationshipKind, 'derivedType');
		assert.strictEqual(results[0].relationshipDistance, 2);
		assert.strictEqual(results[0].sourceUri, 'reforger-pak://58D0FB3206B6F859/current/Game/Vehicles/Car.c');
		assert.strictEqual(results[0].readInput.addonGuid, '58D0FB3206B6F859');
	});

	test('opens an unavailable relationship URI from the complete Enforce source handoff', async () => {
		const provider = vscode.workspace.registerTextDocumentContentProvider(languageClientSchemes.searchPreview, searchDocumentContentProvider);
		const hit: SearchHit = {
			id: 'relationship', source: 'gameData', kind: 'symbol', title: 'Test', detail: 'class',
			path: 'Game/Test.c:3', excerpt: 'class Test', sourceUri: 'missing-source:/Game/Test.c',
			selectionStartLine: 3, selectionEndLine: 3,
			readInput: { relativePath: 'Game/Test.c', startLine: 3 },
		};
		let completeReads = 0;
		try {
			const opened = await openSearchSourceDocument({
				resolveSourcePath: async () => undefined,
				read: async () => assert.fail('script fallback must not use a bounded read'),
				readComplete: async () => {
					completeReads += 1;
					return { content: 'class Base {}\n\nclass Test : Base {}\n', startLine: 1, endLine: 3 };
				},
			}, hit);
			assert.strictEqual(completeReads, 1);
			assert.strictEqual(opened.document.languageId, 'enforce');
			assert.strictEqual(opened.document.getText(), 'class Base {}\n\nclass Test : Base {}\n');
			assert.ok(languageClientDocumentSelector.some(selector => selector.scheme === opened.document.uri.scheme));
			assert.strictEqual(opened.sourceRead?.startLine, 1);
			assert.strictEqual(opened.physicalDocument, false);
		} finally {
			provider.dispose();
		}
	});

	test('preserves structured stale-anchor recovery and returns to broad discovery', () => {
		const error = new McpToolError(
			'The relationship anchor is stale.',
			'stale_relationship_anchor',
			'Rerun semantic discovery.',
		);
		assert.strictEqual(error.code, 'stale_relationship_anchor');
		assert.strictEqual(error.recovery, 'Rerun semantic discovery.');
		assert.match(searchUiSource, /clearRelationshipAnchor = Boolean\(relationship && error instanceof McpToolError/);
		assert.match(searchUiSource, /if \(message\.clearRelationshipAnchor\) \{ state\.relationAnchor = null; state\.relationIncludes = \['direct'\]; state\.relationOpen = false; if \(state\.query\.trim\(\)\) \{ search\(true\); return; \} \}/);
		assert.match(searchClientSource, /const catalogueRevision = asOptionalString\(status\.catalogueRevision\)/);
		assert.match(searchClientSource, /if \(pageNumber > 1 && pages\.has\(pageNumber\)\)/);
		assert.match(searchClientSource, /error\.code === 'stale_relationship_cursor'[\s\S]*?return this\.searchRelationships\(\{ \.\.\.request, page: 1 \}\)/);
	});

	test('keeps opening and closing Search Scope presentation-only', () => {
		assert.doesNotMatch(searchUiSource, /type: 'refreshScope'/);
		assert.match(searchUiSource, /const scopeSelectionChanged =/);
		assert.match(searchUiSource, /const scopeRevisionChanged =/);
		assert.match(searchUiSource, /const scopeSearchChanged = scopeSelectionChanged \|\| scopeRevisionChanged/);
		assert.match(searchUiSource, /if \(message\.refreshSearch === true && scopeSearchChanged && \(state\.query\.trim\(\) \|\| state\.relationAnchor\)\) search\(true\)/);
		assert.match(searchClientSource, /this\.lastScopeRevision !== scopeRevision[\s\S]*?this\.searchPageCaches\.clear\(\)/);
		assert.match(searchUiSource, /affectsConfiguration[\s\S]*?refreshSearchScope\(context, active\)/);
		assert.match(searchUiSource, /\[data-scope-open\][\s\S]*?state\.scopeOpen = !state\.scopeOpen; state\.relationOpen = false; render\(false\)/);
		assert.doesNotMatch(searchUiSource, /render\(false\); if \(state\.query\.trim\(\)\) search\(true\); return;/);
	});

	test('starts the custom Search MCP process with the configured external index mode', () => {
		assert.match(searchClientSource, /externalIndexMode: ExternalIndexMode/);
		assert.doesNotMatch(searchClientSource, /'--external-index-mode'/);
		assert.match(searchUiSource, /const externalIndexMode = readExternalIndexMode\(\)/);
		assert.match(searchUiSource, /externalIndexMode,/);
		assert.doesNotMatch(searchUiSource, /loadedAddonSourceInventoryIsConfirmed\(addonSourceInventory\)/);
		assert.match(searchUiSource, /dependencyProjectFiles: await discoverWorkspaceProjectFiles\(\)/);
		assert.match(searchUiSource, /onDidConfirmLoadedAddonSourceInventory[\s\S]*?refreshSearchScope\(context, activeSearch\)/);
		assert.doesNotMatch(searchClientSource, /dependencyProjectFiles\.flatMap/);
		assert.match(searchUiSource, /affectsConfiguration\(`\$\{workbenchConfig\.section\}\.\$\{workbenchConfig\.settings\.externalIndexMode\}`\)/);
		assert.match(searchUiSource, /queueSearchScopeRefresh\(active, \(\) => restartSearchScope\(context, active\)\)/);
		assert.match(searchUiSource, /\(await previousClient\)\.dispose\(\)/);
	});

	test('maps symbol search handoffs into source-browser rows', () => {
		const results = normalizeSearchPage('gameData', {
			results: [{
				name: 'SCR_BaseGameMode',
				kind: 'class',
				qualifiedName: 'SCR_BaseGameMode',
				signature: 'class SCR_BaseGameMode',
				relativePath: 'Game/GameMode/SCR_BaseGameMode.c',
				declarationRange: { startLine: 18 },
				symbolRef: 'gd1:symbol',
				readSourceInput: {
					catalogueRevision: 'gd1:revision',
					relativePath: 'Game/GameMode/SCR_BaseGameMode.c',
					startLine: 18,
				},
				sourceUri: 'reforger-pak://58D0FB3206B6F859/current/Game/GameMode/SCR_BaseGameMode.c',
			}],
		});

		assert.deepStrictEqual(results[0], {
			id: 'gameData-0-gd1:symbol',
			source: 'gameData',
			kind: 'symbol',
			symbolKind: 'class',
			symbolRef: 'gd1:symbol',
			title: 'SCR_BaseGameMode',
			detail: 'class',
			qualifiedName: 'SCR_BaseGameMode',
			signature: 'class SCR_BaseGameMode',
			path: 'Game/GameMode/SCR_BaseGameMode.c:18',
			excerpt: 'class SCR_BaseGameMode',
			matchKind: 'symbol',
			selectionStartLine: 18,
			selectionEndLine: 18,
			sourceUri: 'reforger-pak://58D0FB3206B6F859/current/Game/GameMode/SCR_BaseGameMode.c',
			readInput: {
				catalogueRevision: 'gd1:revision',
				relativePath: 'Game/GameMode/SCR_BaseGameMode.c',
				startLine: 18,
			},
		});
	});

	test('maps Wiki excerpts and citation links into documentation rows', () => {
		const results = normalizeSearchPage('wiki', {
			results: [{
				title: 'Game Master',
				heading: 'Overview',
				matchKind: 'body',
				relativePath: 'Modding/Game Master/Overview.md',
				matchedLine: 12,
				excerpt: 'Game Master controls the scenario.',
				sourceUrl: 'https://community.bistudio.com/wiki/Arma_Reforger:Game_Master',
				readInput: {
					corpusRevision: 'ow1:revision',
					relativePath: 'Modding/Game Master/Overview.md',
					startLine: 12,
					lineCount: 12,
				},
			}],
		});

		assert.strictEqual(results[0].kind, 'documentation');
		assert.strictEqual(results[0].path, 'Modding/Game Master/Overview.md:12');
		assert.strictEqual(results[0].sourceUrl, 'https://community.bistudio.com/wiki/Arma_Reforger:Game_Master');
		assert.strictEqual(results[0].excerpt, 'Game Master controls the scenario.');
		assert.strictEqual(results[0].selectionStartLine, 12);
		assert.strictEqual(results[0].selectionEndLine, 12);
	});

	test('preserves add-on identity while showing one display name in game-data rows', () => {
		const results = normalizeSearchPage('gameData', {
			results: [{
				name: 'SCR_AddonClass',
				kind: 'class',
				qualifiedName: 'SCR_AddonClass',
				signature: 'class SCR_AddonClass',
				relativePath: 'Scripts/SCR_AddonClass.c',
				declarationRange: { startLine: 7 },
				symbolRef: 'sr2:addon-symbol',
				addonGuid: 'A1B2C3D4E5F60718',
				addonLabel: 'ExampleAddon (Example Add-on)',
				thumbnailColor: '#654321',
				sourceUri: 'file:///C:/Addons/Example/Scripts/SCR_AddonClass.c',
				readSourceInput: {
					catalogueRevision: 'gd2:revision',
					addonGuid: 'A1B2C3D4E5F60718',
					relativePath: 'Scripts/SCR_AddonClass.c',
					startLine: 7,
				},
			}],
		});

		assert.strictEqual(results[0].addonGuid, 'A1B2C3D4E5F60718');
		assert.strictEqual(results[0].addonLabel, 'Example Add-on');
		assert.strictEqual(results[0].thumbnailColor, '#654321');
		assert.strictEqual(results[0].sourceUri, 'file:///C:/Addons/Example/Scripts/SCR_AddonClass.c');
		assert.strictEqual(results[0].readInput.addonGuid, 'A1B2C3D4E5F60718');
		assert.strictEqual(results[0].symbolRef, 'sr2:addon-symbol');
		assert.match(results[0].id, /A1B2C3D4E5F60718/);
	});

	test('keeps the full result path above a full-width preview', () => {
		assert.match(searchUiSource, /\.atlas-card-head \{ display: flex;[^}]*justify-content: space-between;/);
		assert.match(searchUiSource, /\.result-path \{[^}]*overflow-wrap: anywhere;/);
		assert.match(searchUiSource, /\.atlas-card \.result-path \{[^}]*max-width: none;[^}]*text-align: left;/);
		assert.match(searchUiSource, /<div class="atlas-card-head"><strong>[\s\S]*?resultPath\(result\)[\s\S]*?resultPreview\(result\)/);
		assert.match(searchUiSource, /const highlightText = \(value, query\) =>/);
		assert.match(searchUiSource, /<mark>/);
		assert.match(searchUiSource, /highlightRange\(sourceText, matchRange\)/);
		assert.match(searchUiSource, /state\.sourcePreviews\[result\.id\]/);
		assert.doesNotMatch(searchUiSource, /return highlightRange\(result\.excerpt, matchRange\)/);
		assert.match(searchUiSource, /state\.matchRanges = \{ \.\.\.state\.matchRanges/);
		assert.match(searchUiSource, /message\.type === 'previews'/);
		assert.match(searchUiSource, /hydrateSearchPreviews/);
		assert.match(searchUiSource, /provideLanguageServerSemanticTokens/);
		assert.match(searchUiSource, /semanticPreviewText/);
		assert.match(searchUiSource, /message\.type === 'semanticPreviews'/);
		assert.match(searchUiSource, /previewMatchText/);
		assert.match(searchUiSource, /semanticTokenRoles/);
		assert.match(searchUiSource, /semanticForegrounds/);
		assert.match(searchUiSource, /previewDiagnostics/);
	});

	test('cycles through one-column, masonry, and aligned-row layouts without rerunning the search', () => {
		const harness = createSearchWebviewHarness();
		harness.evaluate(`state.results = [
			{ id: 'one', source: 'workspace', kind: 'symbol', symbolKind: 'class', title: 'One', detail: 'class', path: 'Scripts/One.c', excerpt: 'class One' },
			{ id: 'two', source: 'workspace', kind: 'symbol', symbolKind: 'method', title: 'Two', detail: 'method', path: 'Scripts/Two.c', excerpt: 'void Two()' }
		]; state.status = 'ready'`);
		const searchCount = harness.posted.filter(message => message.type === 'search').length;

		assert.strictEqual(harness.evaluate('state.resultLayout'), 'single');
		assert.doesNotMatch(harness.evaluate<string>('resultGroups()'), /atlas-results (?:masonry|rows)/);
		assert.match(harness.evaluate<string>('pageControls(true)'), /Result layout: One column\. Activate for two-column masonry\./);

		harness.evaluate('toggleResultLayout()');
		assert.strictEqual(harness.evaluate('state.resultLayout'), 'masonry');
		assert.match(harness.evaluate<string>('resultGroups()'), /atlas-results masonry/);
		assert.match(harness.evaluate<string>('pageControls(true)'), /Result layout: Two-column masonry\. Activate for aligned rows\./);

		harness.evaluate('toggleResultLayout()');
		assert.strictEqual(harness.evaluate('state.resultLayout'), 'rows');
		assert.match(harness.evaluate<string>('resultGroups()'), /atlas-results rows/);
		assert.match(harness.evaluate<string>('pageControls(true)'), /Result layout: Aligned rows\. Activate for one column\./);

		harness.evaluate('toggleResultLayout()');
		assert.strictEqual(harness.evaluate('state.resultLayout'), 'single');
		assert.doesNotMatch(harness.evaluate<string>('resultGroups()'), /atlas-results (?:masonry|rows)/);
		assert.strictEqual(
			harness.posted.filter(message => message.type === 'search').length,
			searchCount,
		);

		assert.match(searchUiSource, /resultLayout: 'single'/);
		assert.match(searchUiSource, /\.atlas-results\.masonry \{ display: block; column-count: 2;/);
		assert.match(searchUiSource, /\.atlas-results\.rows \{[^}]*--result-row-columns: minmax\(150px, 28%\) minmax\(240px, 1fr\) 180px 110px;/);
		assert.match(searchUiSource, /\.atlas-results\.rows \.atlas-card \{[^}]*display: grid;[^}]*grid-template-columns: var\(--result-row-columns\);/);
		assert.match(searchUiSource, /\.atlas-results\.rows \.snippet, \.atlas-results\.rows \.md-preview \{ display: none; \}/);
		assert.match(searchUiSource, /\.page-controls \[data-result-layout\] \{ display: inline-flex; align-items: center; justify-content: center; padding: 0; line-height: 0; \}/);
		assert.match(searchUiSource, /const pageControls = \(includeLayoutToggle = false\) =>/);
		assert.match(searchUiSource, /return '<div class="page-controls" aria-label="Search result pages">' \+ previewControl \+ layoutToggle \+ '<select data-page-size/);
		assert.match(searchUiSource, /pageControls\(true\)/);
		assert.match(searchUiSource, /data-result-layout/);
		assert.match(searchUiSource, /const resultLayouts = \[[\s\S]*?id: 'single'[\s\S]*?id: 'masonry'[\s\S]*?id: 'rows'/);
		assert.match(searchUiSource, /state\.resultLayout = resultLayouts\[\(current \+ 1\) % resultLayouts\.length\]\.id; render\(false\);/);
		assert.doesNotMatch(searchUiSource, /data-result-layout[^\n]*search\(/);
		assert.match(searchUiSource, /resultLayout: state\.resultLayout/);
	});

	test('packs unequal cards into masonry columns within each source group', () => {
		assert.match(searchUiSource, /\.atlas-results\.masonry \{ display: block; column-count: 2; column-gap: 10px; \}/);
		assert.match(searchUiSource, /\.atlas-results\.masonry \.atlas-card \{ display: inline-block; width: 100%; margin: 0 0 10px; break-inside: avoid; \}/);
		assert.match(searchUiSource, /@media \(max-width: 980px\) \{ \.atlas-results\.masonry \{ column-count: 1; \}/);
	});

	test('separates matches with compact type-tinted card surfaces', () => {
		assert.match(searchUiSource, /\.atlas-group \.atlas-results \{[^}]*background: var\(--bg\);/);
		assert.match(searchUiSource, /\.atlas-card \{[^}]*padding: 12px 12px 8px;/);
		assert.match(searchUiSource, /\.atlas-card \{[^}]*background: var\(--panel\);[^}]*background: color-mix\(in srgb, var\(--result-accent\) 7%, var\(--panel\)\);/);
		assert.match(searchUiSource, /\.atlas-card:hover \{[^}]*background: color-mix\(in srgb, var\(--result-accent\) 12%, var\(--panel\)\);/);
		assert.match(searchUiSource, /\.atlas-card:hover, \.atlas-card\.selected \{[^}]*border-left-color: var\(--result-accent\);/);
	});

	test('publishes raw previews before semantic coloring completes', () => {
		const rawPhase = searchUiSource.indexOf('const flushRawPreviews = (): void =>');
		const rawMessage = searchUiSource.indexOf("type: 'previews',", rawPhase);
		const semanticPhase = searchUiSource.indexOf('const hydrateSemanticPreview = async');
		const semanticMessage = searchUiSource.indexOf("type: 'semanticPreviews'", rawMessage);
		const semanticQueued = searchUiSource.indexOf('queueSemanticPreview(rawPreview)');
		const rawWorkersComplete = searchUiSource.indexOf('await Promise.all(Array.from({ length: Math.min(sourcePreviewWorkerCount');
		assert.ok(rawPhase >= 0);
		assert.ok(rawMessage > rawPhase);
		assert.ok(semanticMessage > rawMessage);
		assert.ok(semanticPhase > semanticMessage);
		assert.ok(semanticQueued > semanticPhase);
		assert.ok(rawWorkersComplete > semanticQueued);
		assert.match(searchUiSource, /queueRawPreview\(hit\.id\)/);
		assert.match(searchUiSource, /flushRawPreviews\(\);/);
		assert.match(searchUiSource, /active\.previewCancellation\?\.cancel\(\)/);
		assert.match(searchUiSource, /firstSemanticMs/);
		assert.doesNotMatch(searchUiSource, /pendingUpdates/);
		assert.match(searchUiSource, /searchUi\.previewRawCompleted/);
		assert.match(searchUiSource, /phase: 'raw'/);
		assert.match(searchUiSource, /phase: 'semantic'/);
		assert.match(searchUiSource, /lastSemanticMessageMs/);
		assert.match(searchUiSource, /state\.previewPerformance = message\.performance \?\? state\.previewPerformance/);
		assert.match(searchUiSource, /data-result-preview="' \+ esc\(result\.id\) \+ '"/);
		assert.match(searchUiSource, /const updateResultPreviews = ids =>/);
		assert.match(searchUiSource, /state\.matchRanges = \{ \.\.\.state\.matchRanges, \.\.\.\(message\.matches \?\? \{\}\) \}; updateResultPreviews\(Object\.keys\(message\.previews \?\? \{\}\)\);/);
		assert.match(searchUiSource, /state\.semanticPreviews = \{ \.\.\.state\.semanticPreviews, \.\.\.\(message\.previews \?\? \{\}\) \}; updateResultPreviews\(Object\.keys\(message\.previews \?\? \{\}\)\);/);
		assert.doesNotMatch(searchUiSource, /state\.matchRanges = \{ \.\.\.state\.matchRanges, \.\.\.\(message\.matches \?\? \{\}\) \}; render\(\);/);
		assert.doesNotMatch(searchUiSource, /state\.semanticPreviews = \{ \.\.\.state\.semanticPreviews, \.\.\.\(message\.previews \?\? \{\}\) \}; render\(\);/);
	});

	test('opens result cards while preserving text selection and the Wiki page action', () => {
		assert.match(searchUiSource, /data-open="' \+ esc\(result\.id\) \+ '" tabindex="0" role="button"/);
		assert.doesNotMatch(searchUiSource, /<button class="open" data-open=/);
		assert.match(searchUiSource, /const hasTextSelection = \(\) => Boolean\(window\.getSelection\(\)\?\.toString\(\)\);/);
		assert.match(searchUiSource, /if \(event\.target\.closest\('\[data-external\], \[data-relation-anchor\]'\) \|\| hasTextSelection\(\)\) return;/);
		assert.match(searchUiSource, /keydown', event => \{ if \(event\.target\.closest\('\[data-external\], \[data-relation-anchor\]'\)\) return;/);
		assert.match(searchUiSource, /data-external="' \+ esc\(result\.id\) \+ '">Open official page/);
	});

	test('supports random-access result pages, cursor-compatible MCP responses, and selectable page sizes', () => {
		assert.strictEqual(maxSearchPages, searchLimits.maxPages);
		assert.match(searchClientSource, /public async search\([\s\S]*?pageSize: number,[\s\S]*?page: number/);
		assert.match(searchClientSource, /symbolKinds\?: readonly SearchSymbolKind\[\]/);
		assert.match(searchClientSource, /kinds: symbolKinds/);
		assert.match(searchClientSource, /limit: pageSize/);
		assert.match(searchClientSource, /nextCursor/);
		assert.match(searchClientSource, /nextCursor/);
		assert.match(searchClientSource, /total: number/);
		assert.match(searchClientSource, /totalBySource: Partial<Record<SearchSource, number>>/);
		assert.match(searchUiSource, /const pageSizeOptions = \[25, 50, 100\];/);
		assert.match(searchUiSource, /data-page-input/);
		assert.match(searchUiSource, /data-page-prev/);
		assert.match(searchUiSource, /data-page-next/);
		assert.match(searchUiSource, /data-page-size/);
		assert.match(searchUiSource, /<select data-page-size aria-label="Total results per page"/);
		assert.match(searchUiSource, /data-page-size aria-label="Total results per page"' \+ \(state\.status === 'loading' \? ' disabled' : ''\)/);
		assert.match(searchUiSource, /<span class="page-arrows"><button type="button" data-page-prev[\s\S]*data-page-next/);
		assert.match(searchUiSource, /<select data-page-size[\s\S]*<span class="page-status"><span class="muted">Page<\/span>/);
		assert.match(searchUiSource, /\.page-status \{ display: inline-flex; flex: 0 0 150px; align-items: center; justify-content: flex-end;/);
		assert.match(searchUiSource, /value="' \+ state\.page \+ '"/);
		assert.match(searchUiSource, /of ' \+ pageTotal \+ '<\/span>/);
		assert.match(searchUiSource, /state\.type = element\.dataset\.type; state\.page = 1; render\(false\); search\(true\)/);
		assert.match(searchUiSource, /resultType: state\.type/);
		assert.match(searchUiSource, /message\.resultType/);
		assert.match(searchUiSource, /const resultTypes = \$\{JSON\.stringify\(searchKindFilters\.map\(\(\{ value, label \}\) => \(\{ value, label \}\)\)\)\};/);
		assert.match(searchUiSource, /searchKindsFor\(typeValue\)/);
		assert.match(searchUiSource, /isSearchKindValue\(message\.resultType\) && !isSearchResourceKindValue\(message\.resultType\)/);
		assert.match(searchClientSource, /const sourcePageSize = 100;/);
		assert.match(searchUiSource, /const sourcePreviewWorkerCount = 8;/);
		assert.match(searchUiSource, /Math\.min\(sourcePreviewWorkerCount, previewHits\.length\)/);
		assert.match(searchUiSource, /const previewUpdateBatchSize = 4;/);
		assert.match(searchUiSource, /pendingRawPreviewIds\.length >= previewUpdateBatchSize/);
		assert.match(searchUiSource, /pendingSemanticPreviewIds\.length >= previewUpdateBatchSize/);
		assert.match(searchUiSource, /previews: Object\.fromEntries\(ids\.map\(id => \[id, semanticPreviews\[id\]\]\)\)/);
		assert.doesNotMatch(searchUiSource, /setTimeout\(\(\) => search\(true\), \d+\)/);
		assert.match(searchClientSource, /export const maxSearchPages = searchLimits\.maxPages;/);
		assert.match(searchClientSource, /paginationMode: 'offset'/);
		assert.match(searchClientSource, /export interface SearchPerformance/);
		assert.match(searchClientSource, /remoteRequests: number/);
		assert.match(searchClientSource, /cacheHits: number/);
		assert.match(searchClientSource, /performance: finishSearchPerformance/);
		assert.match(searchClientSource, /let sourceOffset = 0;/);
		assert.match(searchClientSource, /this\.searchPageCaches\.clear\(\);/);
		assert.match(searchUiSource, /const maxSearchPages = \$\{searchLimits\.maxPages\};/);
		assert.match(searchUiSource, /Math\.min\(maxSearchPages, Math\.max\(1, Math\.ceil\(state\.total \/ state\.pageSize\)\)\)/);
		assert.doesNotMatch(searchUiSource, /results per source/);
		assert.match(searchUiSource, /<span class="page-status"><span class="muted">Page<\/span><input data-page-input[\s\S]*?<span class="muted">of ' \+ pageTotal \+ '<\/span>/);
		assert.match(searchUiSource, /<div class="empty">No results match this search\.<\/div>/);
		assert.doesNotMatch(searchUiSource, /Enter a symbol, concept, or documentation term to search\./);
		assert.doesNotMatch(searchUiSource, /function search\(resetPagination\) \{ if \(resetPagination\) \{ state\.page = 1; state\.total = 0; \}/);
		assert.doesNotMatch(searchUiSource, /function search\(resetPagination\)[\s\S]*?state\.uiPerformance\.lastPreviewMessageMs = 0; render\(\); vscode\.postMessage/);
		assert.doesNotMatch(searchUiSource, /if \(message\.type === 'loading'\) \{[^}]*render\(\); \}/);
		assert.match(searchUiSource, /event\.ctrlKey && event\.key === 'F3'/);
		assert.match(searchUiSource, /if \(state\.status === 'loading'\) return;/);
		assert.match(searchUiSource, /state\.lastSearchKey/);
		assert.match(searchUiSource, /function requestPage\(value\) \{ if \(state\.status === 'loading'\) return;/);
		assert.match(searchClientSource, /const cached = pages\.get\(page\);/);
		assert.match(searchClientSource, /offset: \(page - 1\) \* pageSize/);
		assert.match(searchClientSource, /const firstPageNumber = Math\.floor\(start \/ sourcePageSize\) \+ 1;/);
		assert.match(searchClientSource, /const lastPageNumber = Math\.floor\(\(end - 1\) \/ sourcePageSize\) \+ 1;/);
		assert.match(searchUiSource, /type: 'debugSnapshot'/);
		assert.match(searchUiSource, /message\.type === 'debugSnapshot'/);
		assert.match(searchUiSource, /searchUi\.snapshot/);
		assert.match(searchUiSource, /searchPerformance: state\.searchPerformance/);
		assert.match(searchUiSource, /lastSearchResponseMs/);
		assert.match(searchUiSource, /state\.previewPerformance = message\.performance/);
		assert.match(searchUiSource, /totalBySource: state\.totalBySource/);
		assert.match(searchUiSource, /selectionStartLine: result\.selectionStartLine/);
		assert.match(searchUiSource, /excerptLength: typeof result\.excerpt === 'string'/);
		assert.match(searchUiSource, /function snapshotResults\(value: unknown\)/);
		assert.match(searchUiSource, /resultsTruncated: Array\.isArray\(snapshot\.results\) && snapshot\.results\.length > 100/);
		assert.match(searchUiSource, /const modeButtons = \(\) =>/);
		assert.match(searchUiSource, /data-mode="text"/);
		assert.match(searchUiSource, /state\.mode === 'text'/);
		assert.match(searchUiSource, /searchMode: state\.mode/);
		assert.match(searchUiSource, /matchCase: false, matchWholeWord: false, useRegex: false/);
		assert.match(searchUiSource, /data-text-option="matchCase"/);
		assert.match(searchUiSource, /title="Match case"[\s\S]*?>Aa<\/button>/);
		assert.match(searchUiSource, /data-text-option="matchWholeWord"/);
		assert.match(searchUiSource, /title="Match whole word"[\s\S]*?>\|ab\|<\/button>/);
		assert.match(searchUiSource, /data-text-option="useRegex"/);
		assert.match(searchUiSource, /title="Use regular expression"[\s\S]*?>\.\*<\/button>/);
		assert.match(searchUiSource, /state\[element\.dataset\.textOption\] = !state\[element\.dataset\.textOption\]/);
		assert.match(searchUiSource, /matchCase: state\.matchCase, matchWholeWord: state\.matchWholeWord, useRegex: state\.useRegex/);
		assert.match(searchClientSource, /matchCase: textOptions\.matchCase/);
		assert.match(searchClientSource, /matchWholeWord: textOptions\.matchWholeWord/);
		assert.match(searchClientSource, /useRegex: textOptions\.useRegex/);
		assert.match(searchClientSource, /textOptions\.matchCase.*textOptions\.matchWholeWord.*textOptions\.useRegex/);
		assert.match(searchClientSource, /search_game_data_text/);
		assert.match(searchClientSource, /search_workspace_text/);
	});
});

interface SearchWebviewHarness {
	posted: Array<Record<string, unknown>>;
	evaluate<T = unknown>(source: string): T;
	message(data: Record<string, unknown>): void;
}

function createSearchWebviewHarness(): SearchWebviewHarness {
	const html = renderSearchUiForTest();
	const scripts = [...html.matchAll(/<script[^>]*>([\s\S]*?)<\/script>/g)].map(match => match[1]);
	assert.ok(scripts.length >= 2, 'expected the shipped webview runtime script');
	const posted: Array<Record<string, unknown>> = [];
	const listeners = new Map<string, Array<(event: { data: Record<string, unknown> }) => void>>();
	const app = { innerHTML: '' };
	const body = {};
	const documentObject = {
		body,
		activeElement: body,
		getElementById: (id: string) => id === 'app' ? app : undefined,
		querySelectorAll: () => [],
		querySelector: () => undefined,
		addEventListener: () => undefined,
	};
	const windowObject = {
		__reforgerSearchVscode: {
			postMessage: (message: Record<string, unknown>) => {
				posted.push(JSON.parse(JSON.stringify(message)) as Record<string, unknown>);
			},
		},
		innerWidth: 1280,
		innerHeight: 720,
		devicePixelRatio: 1,
		getSelection: () => ({ toString: () => '' }),
		addEventListener: (type: string, listener: (event: { data: Record<string, unknown> }) => void) => {
			listeners.set(type, [...(listeners.get(type) ?? []), listener]);
		},
	};
	const context = vm.createContext({
		window: windowObject,
		document: documentObject,
		performance: { now: () => Date.now() },
		setTimeout,
		clearTimeout,
		console,
	});
	vm.runInContext(scripts.at(-1) ?? '', context);
	return {
		posted,
		evaluate<T>(source: string): T {
			const value = vm.runInContext(source, context) as T;
			if (value === undefined) {
				return value;
			}
			return JSON.parse(JSON.stringify(value)) as T;
		},
		message(data: Record<string, unknown>): void {
			for (const listener of listeners.get('message') ?? []) {
				listener({ data });
			}
		},
	};
}

function lastPosted(
	posted: Array<Record<string, unknown>>,
	type: string,
): Record<string, unknown> {
	for (let index = posted.length - 1; index >= 0; index -= 1) {
		if (posted[index].type === type) {
			return posted[index];
		}
	}
	assert.fail(`expected a ${type} webview message`);
}
