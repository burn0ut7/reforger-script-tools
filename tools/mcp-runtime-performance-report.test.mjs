import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const reportScript = join(process.cwd(), 'tools', 'mcp-runtime-performance-report.mjs');

test('exercises every listed non-Workbench tool and writes source-free JSON and Markdown', () => {
	const fixture = createFakeServer('available');
	const jsonPath = join(fixture.root, 'report.json');
	const markdownPath = join(fixture.root, 'report.md');
	const result = runReport(fixture, jsonPath, markdownPath, [
		'--require-all',
		'--paired-baseline-server', process.execPath,
	]);

	assert.equal(result.status, 0, result.stderr);
	const report = JSON.parse(readFileSync(jsonPath, 'utf8'));
	const markdown = readFileSync(markdownPath, 'utf8');
	const calls = readFileSync(fixture.tracePath, 'utf8').trim().split(/\r?\n/).map(line => JSON.parse(line));

	assert.equal(report.schemaVersion, 2);
	assert.equal(report.coverage.listed, 23);
	assert.equal(report.coverage.exercised, 23);
	assert.equal(report.coverage.skipped, 0);
	assert.equal(report.coverage.failed, 0);
	assert.equal(report.verdict, 'pass');
	assert.equal(report.operations.length, 24);
	assert.ok(report.operations.some(operation => operation.name === 'current_mixed_source_client_search'));
	assert.ok(report.operations.every(operation => operation.firstMs >= 0));
	assert.ok(report.operations.every(operation => operation.warm.count === 2));
	assert.ok(report.operations.every(operation => operation.responseBytes > 0));
	assert.ok(report.operations.every(operation => operation.fingerprint));
	assert.equal(report.coldProcess.count, 1);
	assert.equal(report.coldProcess.firstNavigation.count, 1);
	assert.equal(report.coldProcess.firstSourceRead.count, 1);
	assert.equal(report.coldProcess.missingNavigationHandoffs, 0);
	assert.deepEqual(report.concurrency.probes.map(probe => probe.requested), [1, 2]);
	assert.ok(report.concurrency.probes.every(probe => probe.completed === probe.requested));
	assert.ok(report.operations.flatMap(operation => operation.variants).some(variant => variant.scenario === 'regular-expression'));
	assert.ok(report.operations.flatMap(operation => operation.variants).some(variant => variant.scenario === 'pagination'));
	assert.ok(report.operations.find(operation => operation.name === 'read_game_data_source').variants.some(variant => variant.scenario === 'research-handoff'));
	assert.ok(report.operations.find(operation => operation.name === 'query_source_symbol_relationships').variants.some(variant => variant.scenario === 'all-level-hierarchy'));
	assert.ok(report.memory.some(sample => sample.stage === 'after-first-relationship-projection'));
	assert.equal(report.configuration.buildProfile, 'release');
	assert.equal(report.configuration.toolProfile, 'all');
	for (const name of ['inspect_symbol', 'list_symbol_members', 'query_symbol_relationships']) {
		const operation = report.operations.find(operation => operation.name === name);
		assert.equal(operation.scenario, 'gameData');
		assert.ok(operation.variants.some(variant => variant.scenario === 'workspace'));
	}
	assert.equal(report.configuration.queries.relationshipMethod, 'OnActivate');
	assert.equal(report.corpus.workspace.indexedSymbols, 42);
	assert.deepEqual(
		Object.keys(report.paired.coldGate.phases),
		['processToInitialize', 'toolsList', 'gameDataReadiness'],
	);
	assert.ok(Object.values(report.paired.coldGate.phases).every(gate => Number.isFinite(gate.medianBudgetMs) && Number.isFinite(gate.p95BudgetMs)));
	assert.ok(report.paired.operations.some(operation => operation.name === 'current_mixed_source_client_search'));
	assert.match(markdown, /23 \/ 23/);
	assert.match(markdown, /search_game_data_symbols/);
	assert.match(markdown, /Concurrency Probe/);
	assert.doesNotMatch(markdown, /DO_NOT_COPY/);
	assert.doesNotMatch(JSON.stringify(report), /DO_NOT_COPY/);
	assert.ok(!calls.some(call => call.name.startsWith('workbench_')));
	assert.ok(calls.some(call => call.arguments?.useRegex === true));
	assert.ok(calls.some(call => call.arguments?.cursor === 'cursor-1'));
	for (const tool of nonWorkbenchTools) {
		assert.ok(calls.some(call => call.name === tool), `expected ${tool} to be called`);
	}

	fixture.cleanup();
});

test('fails when a bounded concurrency probe returns tool errors', () => {
	const fixture = createFakeServer('concurrency-error');
	const result = runReport(
		fixture,
		join(fixture.root, 'report.json'),
		join(fixture.root, 'report.md'),
	);

	assert.equal(result.status, 1);
	const report = JSON.parse(readFileSync(join(fixture.root, 'report.json'), 'utf8'));
	assert.equal(report.verdict, 'fail');
	assert.ok(report.coverage.failed > 0);
	assert.ok(report.concurrency.probes.some(probe => probe.failed > 0));

	fixture.cleanup();
});

test('counts structured errors from fresh-process status samples as failures', () => {
	const fixture = createFakeServer('cold-error');
	const result = runReport(
		fixture,
		join(fixture.root, 'report.json'),
		join(fixture.root, 'report.md'),
	);

	assert.equal(result.status, 1);
	const report = JSON.parse(readFileSync(join(fixture.root, 'report.json'), 'utf8'));
	assert.equal(report.coldProcess.failed, 1);
	assert.equal(report.coldProcess.gameDataStatus.count, 0);
	assert.match(report.operations.find(operation => operation.name === 'game_data_status').reason, /game_data_cold_error/);

	fixture.cleanup();
});

test('reports unavailable families as skipped and makes complete coverage opt-in', () => {
	const fixture = createFakeServer('unavailable');
	const jsonPath = join(fixture.root, 'report.json');
	const markdownPath = join(fixture.root, 'report.md');
	const diagnostic = runReport(fixture, jsonPath, markdownPath);

	assert.equal(diagnostic.status, 0, diagnostic.stderr);
	const report = JSON.parse(readFileSync(jsonPath, 'utf8'));
	assert.equal(report.verdict, 'partial');
	assert.ok(report.coverage.skipped > 0);
	assert.equal(report.coverage.failed, 0);
	assert.match(readFileSync(markdownPath, 'utf8'), /unavailable/i);

	const strict = runReport(
		fixture,
		join(fixture.root, 'strict.json'),
		join(fixture.root, 'strict.md'),
		['--require-all'],
	);
	assert.equal(strict.status, 1);

	fixture.cleanup();
});

test('comparison proves controlled inputs and records semantic symbol counts', () => {
	const fixture = createFakeServer('available');
	const baselinePath = join(fixture.root, 'baseline.json');
	const baseline = runReport(fixture, baselinePath, join(fixture.root, 'baseline.md'));
	assert.equal(baseline.status, 0, baseline.stderr);
	const comparisonPath = join(fixture.root, 'comparison.json');
	const candidate = runReport(
		fixture,
		join(fixture.root, 'candidate.json'),
		join(fixture.root, 'candidate.md'),
		['--baseline-report', baselinePath, '--comparison-json-out', comparisonPath, '--comparison-markdown-out', join(fixture.root, 'comparison.md')],
	);
	assert.equal(candidate.status, 0, candidate.stderr);
	const comparison = JSON.parse(readFileSync(comparisonPath, 'utf8'));
	assert.equal(comparison.controlledInputsMatch, true);
	assert.deepEqual(comparison.controlledInputMismatches, []);
	assert.deepEqual(comparison.symbolCounts, { gameData: 84, workspace: 42 });

	const mismatchPath = join(fixture.root, 'mismatch.json');
	runReport(
		fixture,
		join(fixture.root, 'different.json'),
		join(fixture.root, 'different.md'),
		['--workspace-symbol-query', 'Different', '--baseline-report', baselinePath, '--comparison-json-out', mismatchPath, '--comparison-markdown-out', join(fixture.root, 'mismatch.md')],
	);
	const mismatch = JSON.parse(readFileSync(mismatchPath, 'utf8'));
	assert.equal(mismatch.controlledInputsMatch, false);
	assert.ok(mismatch.controlledInputMismatches.includes('queries'));
	assert.equal(mismatch.verdict, 'fail');

	const differentInventoryPath = join(fixture.root, 'different-workspace-index.md');
	writeFileSync(differentInventoryPath, workspaceIndexInventory(43), 'utf8');
	const corpusMismatchPath = join(fixture.root, 'corpus-mismatch.json');
	runReport(
		fixture,
		join(fixture.root, 'different-corpus.json'),
		join(fixture.root, 'different-corpus.md'),
		['--workspace-index-inventory', differentInventoryPath, '--baseline-report', baselinePath, '--comparison-json-out', corpusMismatchPath, '--comparison-markdown-out', join(fixture.root, 'corpus-mismatch.md')],
	);
	const corpusMismatch = JSON.parse(readFileSync(corpusMismatchPath, 'utf8'));
	assert.equal(corpusMismatch.corpusMatch, false);
	assert.ok(corpusMismatch.controlledInputMismatches.includes('corpus'));
	assert.equal(corpusMismatch.verdict, 'fail');
	fixture.cleanup();
});

function runReport(fixture, jsonPath, markdownPath, extra = []) {
	const workspaceInventoryPath = join(fixture.root, 'workspace-index.md');
	writeFileSync(workspaceInventoryPath, workspaceIndexInventory(42), 'utf8');
	return spawnSync(process.execPath, [
		reportScript,
		'--server', process.execPath,
		'--server-prefix-arg', fixture.serverPath,
		'--samples', '2',
		'--cold-samples', '1',
		'--concurrency-levels', '1,2',
		'--timeout-ms', '5000',
		'--workspace-scripts', fixture.root,
		'--workspace-index-inventory', workspaceInventoryPath,
		'--json-out', jsonPath,
		'--markdown-out', markdownPath,
		...extra,
	], {
		cwd: process.cwd(),
		encoding: 'utf8',
		env: {
			...process.env,
			RST_FAKE_MCP_MODE: fixture.mode,
			RST_FAKE_MCP_TRACE: fixture.tracePath,
		},
	});
}

function workspaceIndexInventory(indexedSymbols) {
	return `# Index Build Baseline Report

### Source Roots

| Source kind | Files | Bytes | Indexed symbols | Parse diagnostics |
| --- | ---: | ---: | ---: | ---: |
| \`workspace\` | 1 | 100 | ${indexedSymbols} | 0 |
`;
}

function createFakeServer(mode) {
	const root = mkdtempSync(join(tmpdir(), 'mcp-runtime-report-'));
	const serverPath = join(root, 'fake-mcp-server.mjs');
	const tracePath = join(root, 'calls.log');
	writeFileSync(serverPath, fakeServerSource, 'utf8');
	writeFileSync(tracePath, '', 'utf8');
	return {
		root,
		serverPath,
		tracePath,
		mode,
		cleanup: () => rmSync(root, { recursive: true, force: true }),
	};
}

const nonWorkbenchTools = [
	'search_reforger',
	'inspect_symbol',
	'list_symbol_members',
	'query_symbol_relationships',
	'research_game_data',
	'search_game_data_resources',
	'game_data_status',
	'search_game_data_symbols',
	'search_workspace_symbols',
	'search_game_data_text',
	'search_workspace_text',
	'inspect_workspace_symbol',
	'list_workspace_symbol_members',
	'query_workspace_symbol_relationships',
	'inspect_game_data_symbol',
	'list_game_data_symbol_members',
	'query_game_data_symbol_relationships',
	'query_source_symbol_relationships',
	'read_game_data_source',
	'read_workspace_source',
	'official_wiki_status',
	'search_official_wiki',
	'read_official_wiki',
];

const fakeServerSource = String.raw`
import { appendFileSync } from 'node:fs';
import { createInterface } from 'node:readline';

const mode = process.env.RST_FAKE_MCP_MODE;
const trace = process.env.RST_FAKE_MCP_TRACE;
if (process.argv[process.argv.indexOf('--tool-profile') + 1] !== 'all') throw new Error('Expected explicit all tool profile');
const tools = ${JSON.stringify(nonWorkbenchTools)}.concat('workbench_status');
let responseSequence = 0;
const callCounts = new Map();
const lines = createInterface({ input: process.stdin });
for await (const line of lines) {
	const request = JSON.parse(line);
	if (request.method === 'notifications/initialized') continue;
	if (request.method === 'initialize') {
		respond(request.id, { protocolVersion: '2025-11-25', capabilities: { tools: { listChanged: false } }, serverInfo: { name: 'fake', version: '1' } });
		continue;
	}
	if (request.method === 'tools/list') {
		respond(request.id, { tools: tools.map(name => ({ name, description: name, inputSchema: { type: 'object' } })) });
		continue;
	}
	if (request.method === 'tools/call') {
		const name = request.params.name;
		callCounts.set(name, (callCounts.get(name) ?? 0) + 1);
		appendFileSync(trace, JSON.stringify({ name, arguments: request.params.arguments }) + '\n');
		respond(request.id, toolResult(name, request.params.arguments));
	}
}

function toolResult(name, argumentsValue) {
	const unavailable = mode === 'unavailable';
	if (mode === 'cold-error' && name === 'game_data_status') return error('game_data_cold_error');
	const values = {
		search_reforger: { results: [], returned: 0 },
		inspect_symbol: { name: 'Fixture', source: { relativePath: 'Game.c' } },
		list_symbol_members: { results: [], returned: 0, total: 0 },
		query_symbol_relationships: { results: [], returned: 0, total: 0 },
		research_game_data: { status: 'resolved', primary: { readSourceInput: { catalogueRevision: 'gd1:fixture', addonGuid: 'game', relativePath: 'Game.c' } }, alternatives: [], followUp: [] },
		search_game_data_resources: { results: [], returned: 0, total: 0 },
		game_data_status: { available: !unavailable, catalogueRevision: 'gd1:fixture', scopeRevision: 'scope1', scopeAuthority: 'workbench-loaded', coverage: { files: 2, indexedSymbols: 84 }, timingsMs: { total: 3 }, addons: [{ addonGuid: 'game', available: true, scriptCount: 2 }] },
		search_game_data_symbols: { catalogueRevision: 'gd1:fixture', results: unavailable ? [] : [{ symbolRef: 'gd-symbol', readSourceInput: { catalogueRevision: 'gd1:fixture', addonGuid: 'game', relativePath: 'Game.c' } }], returned: unavailable ? 0 : 1, total: unavailable ? 0 : 1 },
		inspect_game_data_symbol: { name: 'Fixture', members: [], source: { relativePath: 'Game.c' } },
		list_game_data_symbol_members: { results: [], returned: 0, total: 0 },
		query_game_data_symbol_relationships: { results: [], returned: 0, total: 0 },
		read_game_data_source: { content: 'DO_NOT_COPY game source', startLine: 1, endLine: 1 },
		search_workspace_symbols: { catalogueRevision: 'ws1:fixture', results: unavailable ? [] : [{ symbolRef: 'ws-symbol', readSourceInput: { catalogueRevision: 'ws1:fixture', relativePath: 'Workspace.c' } }], returned: unavailable ? 0 : 1, total: unavailable ? 0 : 1 },
		inspect_workspace_symbol: { name: 'WorkspaceFixture', members: [], source: { relativePath: 'Workspace.c' } },
		list_workspace_symbol_members: { results: [], returned: 0, total: 0 },
		query_workspace_symbol_relationships: { results: [], returned: 0, total: 0 },
		query_source_symbol_relationships: { relationshipRevision: 'rel1', results: [{ symbolRef: 'related-symbol', sourceAuthority: 'workspace', relationshipKind: 'derivedType', semanticDistance: 1, readSourceInput: { catalogueRevision: 'ws1:fixture', relativePath: 'Related.c' } }], returned: 1, total: 2, nextCursor: argumentsValue?.cursor ? undefined : 'relationship-cursor' },
		read_workspace_source: { content: 'DO_NOT_COPY workspace source', startLine: 1, endLine: 1 },
		search_game_data_text: { results: [], returned: 0, total: 1, nextCursor: 'cursor-1', stats: { scanMs: responseSequence++ } },
		search_workspace_text: { results: [], returned: 0, total: 1, nextCursor: 'cursor-1', stats: { scanMs: responseSequence++ } },
		official_wiki_status: { available: true, corpusRevision: 'ow1:fixture', fileCount: 1 },
		search_official_wiki: { corpusRevision: 'ow1:fixture', results: [{ readInput: { corpusRevision: 'ow1:fixture', relativePath: 'Guide.md', startLine: 1, lineCount: 5 } }], returned: 1, total: 1 },
		read_official_wiki: { content: 'DO_NOT_COPY wiki source', startLine: 1, endLine: 1 },
	};
	if (mode === 'concurrency-error' && name === 'query_source_symbol_relationships' && callCounts.get(name) >= 30) return error('concurrency_probe_failed');
	if (unavailable && name.startsWith('search_game_data')) return error('game_data_unavailable');
	if (unavailable && (name.includes('workspace') || name === 'read_game_data_source' || name.startsWith('inspect_game_data') || name.startsWith('list_game_data') || name.startsWith('query_game_data'))) return error('workspace_unavailable');
	const value = values[name] ?? {};
	return { content: [{ type: 'text', text: JSON.stringify(value) }], structuredContent: value, isError: false };
}

function error(code) {
	const value = { ok: false, code, message: code, recovery: ['configure the source'], retryable: false };
	return { content: [{ type: 'text', text: JSON.stringify(value) }], structuredContent: value, isError: true };
}

function respond(id, result) {
	process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');
}
`;
