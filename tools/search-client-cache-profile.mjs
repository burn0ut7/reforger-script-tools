import { loadSearchClient } from './testing/search-client-fixture.mjs';
import { performance } from 'node:perf_hooks';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

if (!global.gc) throw new Error('Run this profile with node --expose-gc.');
const selectedMode = process.argv[2];
if (!selectedMode) {
	const measurements = ['semantic', 'text', 'resource', 'relationship'].map(mode => {
		const child = spawnSync(process.execPath, ['--expose-gc', fileURLToPath(import.meta.url), mode], { encoding: 'utf8', windowsHide: true });
		if (child.status !== 0) throw new Error(child.stderr);
		return JSON.parse(child.stdout);
	});
	console.log(JSON.stringify({ workload: '40 queries, 40 pages, 100 synthetic hits per page; each mode in a fresh process; client-only heap after GC', measurements }, null, 2));
	process.exit(0);
}
const { McpSearchClient } = await loadSearchClient();
const measurements = [];
for (const mode of [selectedMode]) {
	const client = new McpSearchClient({});
	client.start = async () => {};
	client.lastCatalogueRevision = 'fixture-revision';
	let requests = 0;
	client.callTool = async (_name, input) => {
		requests += 1;
		const offset = Number(input.cursor ?? input.offset ?? 0);
		return {
			catalogueRevision: 'fixture-revision', relationshipRevision: 'fixture-relationships',
			total: 10_000, nextCursor: String(offset + input.limit),
			results: Array.from({ length: input.limit }, (_, index) => {
				const name = `${input.query ?? input.symbolRef}_${offset + index}`;
				return { name, qualifiedName: name, kind: 'class', symbolRef: name, source: 'workspace', relationshipKind: 'direct',
					resourceName: `{58D0FB3206B6F859}Scripts/${name}.c`, relativePath: `Scripts/${name}.c`,
					lineText: `class ${name} { int m_Value; }`, line: 1,
					readSourceInput: { catalogueRevision: 'fixture-revision', relativePath: `Scripts/${name}.c` } };
			}),
		};
	};
	const search = (query, page) => mode === 'relationship'
		? client.searchRelationships({ anchor: { source: 'workspace', symbolRef: query }, selectedScopeIds: ['workspace'], relationshipKinds: ['direct'], depth: 'one', pageSize: 100, page })
		: client.search(query, ['58D0FB3206B6F859', ...(mode === 'resource' ? [] : ['workspace'])], 100, page, undefined, mode);
	global.gc();
	const before = process.memoryUsage().heapUsed;
	for (let query = 0; query < 40; query += 1) {
		for (let page = 1; page <= 40; page += 1) {
			const result = await search(`Fixture${query}`, page);
			if (result.results.length !== 100) throw new Error(`Incomplete ${mode} fixture page`);
		}
	}
	global.gc();
	const retainedBytes = process.memoryUsage().heapUsed - before;
	const beforeBack = requests;
	const started = performance.now();
	await search('Fixture39', 39);
	const backMs = performance.now() - started;
	measurements.push({ mode, queries: client.searchPageCaches.size,
		pages: [...client.searchPageCaches.values()].reduce((n, pages) => n + pages.size, 0),
		retainedBytes, backMs, backRemoteRequests: requests - beforeBack });
	client.dispose();
}
console.log(JSON.stringify(measurements[0]));
