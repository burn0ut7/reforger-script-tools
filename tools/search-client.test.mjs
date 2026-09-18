import assert from 'node:assert/strict';
import test from 'node:test';
import { fakeChild, loadSearchClient } from './testing/search-client-fixture.mjs';

test('disposal clears every request timer and ignores the old process after restart', async () => {
	const children = [];
	const timers = new Set();
	const { McpSearchClient } = await loadSearchClient({
		spawn: () => { const child = fakeChild(); children.push(child); return child; },
		setTimeout: callback => { timers.add(callback); return callback; },
		clearTimeout: timer => timers.delete(timer),
	});
	const client = new McpSearchClient({ serverPath: 'fixture' });
	const firstStart = client.start();
	const firstRejected = assert.rejects(firstStart, /closed/);
	children[0].stdout.emit('data', Buffer.from('{"partial":'));
	assert.equal(timers.size, 1);
	client.dispose();
	assert.equal(timers.size, 0, 'disposed requests must release timeout callbacks');
	const nextStart = client.start();
	await firstRejected;
	assert.equal(client.process, children[1], 'old initialization rejection must not dispose the replacement');
	children[0].emit('exit', 0);
	children[0].emit('error', new Error('late old error'));
	children[0].stdout.emit('data', Buffer.from('invalid old output\n'));
	assert.equal(client.process, children[1]);
	children[1].reply(children[1].sent[0].id);
	await nextStart;
	assert.equal(timers.size, 0);
	assert.equal(children[1].killed, undefined);
	client.dispose();
});

test('current process exit clears pending requests, buffered output, and cached scope', async () => {
	const child = fakeChild();
	const { McpSearchClient } = await loadSearchClient({ spawn: () => child });
	const client = new McpSearchClient({ serverPath: 'fixture' });
	const started = client.start();
	child.reply(child.sent[0].id);
	await started;
	client.searchPageCaches.set('fixture', new Map());
	client.lastCatalogueRevision = 'old';
	const request = client.discoverScope();
	const rejected = assert.rejects(request, /stopped/);
	await new Promise(resolve => setImmediate(resolve));
	child.stdout.emit('data', Buffer.from('{"partial":'));
	child.emit('exit', 1);
	await rejected;
	assert.equal(client.pending.size, 0);
	assert.equal(client.searchPageCaches.size, 0);
	assert.equal(client.lastCatalogueRevision, undefined);
	assert.equal(client.receiveBuffer.length, 0);
});

test('query caches retain back-navigation and invalidate on scope changes', async () => {
	const { McpSearchClient } = await loadSearchClient();
	const client = new McpSearchClient({});
	client.start = async () => {};
	let requests = 0;
	let revision = 'one';
	client.callTool = async (name, input) => {
		if (name === 'game_data_status') return { catalogueRevision: revision, scopeRevision: revision, addons: [] };
		requests += 1;
		const offset = Number(input.offset ?? input.cursor ?? 0);
		return { total: 300, nextCursor: String(offset + input.limit), results: Array.from({ length: input.limit }, (_, i) => ({
			name: `${input.query}-${offset + i}`, matchText: `${input.query}-${offset + i}`, kind: 'class', relativePath: 'Fixture.c',
			readSourceInput: { catalogueRevision: revision, relativePath: 'Fixture.c' },
		})) };
	};
	await client.discoverScope();
	for (const mode of ['semantic', 'text']) {
		for (let i = 0; i < 40; i += 1) await client.search(`Query${i}`, ['workspace'], 100, 2, undefined, mode);
		assert.equal(client.searchPageCaches.size, 32);
		const before = requests;
		const back = await client.search('Query39', ['workspace'], 100, 1, undefined, mode);
		const forward = await client.search('Query39', ['workspace'], 100, 2, undefined, mode);
		assert.equal(requests, before, 'back and forward reuse retained pages');
		assert.equal(back.results.length, 100);
		assert.equal(forward.results[0].title, 'Query39-100');
	}
	revision = 'two';
	await client.discoverScope();
	assert.equal(client.searchPageCaches.size, 0);
	client.dispose();
});

test('a stale text cursor rebuilds its chain once and preserves requested paging', async () => {
	const { McpSearchClient, McpToolError } = await loadSearchClient();
	const client = new McpSearchClient({});
	client.start = async () => {};
	let rejectCursor = true;
	const offsets = [];
	client.callTool = async (_name, input) => {
		const offset = Number(input.cursor ?? 0);
		offsets.push(offset);
		if (offset > 0 && rejectCursor) {
			rejectCursor = false;
			throw new McpToolError('cursor belongs to another source revision.', 'invalid_arguments');
		}
		return { total: 300, nextCursor: String(offset + input.limit), results: Array.from({ length: input.limit }, (_, i) => ({
			matchText: `Hit${offset + i}`, relativePath: 'Fixture.c', readSourceInput: { relativePath: 'Fixture.c' },
		})) };
	};
	const response = await client.search('Hit', ['workspace'], 100, 2, undefined, 'text');
	assert.equal(response.page, 2);
	assert.equal(response.results[0].title, 'Hit100');
	assert.deepEqual(offsets, [0, 100, 0, 100]);
	client.dispose();
});
