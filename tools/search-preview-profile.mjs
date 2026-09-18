import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { loadSearchClient } from './testing/search-client-fixture.mjs';

const server = process.argv[2];
if (!server) throw new Error('Pass the compiled server executable as the first argument.');
const root = await mkdtemp(path.join(tmpdir(), 'rst-preview-profile-'));
const samples = [];
try {
	const scripts = path.join(root, 'Scripts');
	await mkdir(scripts);
	const source = Array.from({ length: 4000 }, (_, line) => line % 160 === 159
		? `/* declaration */ class PreviewProbe${String(line).padStart(4, '0')} { string url = "https://example.test"; } // note\n`
		: `class Padding${line} { string url = "https://example.test"; /* note */ int value; }\n`).join('');
	await writeFile(path.join(scripts, 'Preview.c'), source);
	for (let sample = 0; sample < 8; sample += 1) {
		// Alternate order to avoid giving either mode a consistent warm-machine advantage.
		for (const includePreview of sample % 2 ? [true, false] : [false, true]) {
			let readRequests = 0;
			let child;
			const { McpSearchClient, sourceLinePreview } = await loadSearchClient({
				spawn(command) {
					child = spawn(command, ['mcp', '--tool-profile', 'all', '--external-index-mode', 'none', '--workspace-scripts', scripts], { stdio: 'pipe', windowsHide: true });
					const write = child.stdin.write.bind(child.stdin);
					child.stdin.write = (text, ...rest) => {
						const request = JSON.parse(text);
						if (request.params?.name === 'read_workspace_source') {
							readRequests += 1;
							request.params.arguments.includePreview = includePreview;
						}
						return write(`${JSON.stringify(request)}\n`, ...rest);
					};
					return child;
				},
			});
			const client = new McpSearchClient({ serverPath: path.resolve(server) });
			try {
				const page = await client.search('PreviewProbe', ['workspace'], 25, 1);
				assert.equal(page.results.length, 25);
				let next = 0;
				let firstPreviewMs;
				const started = performance.now();
				await Promise.all(Array.from({ length: 8 }, async () => {
					while (next < page.results.length) {
						const hit = page.results[next++];
						const document = await client.read(hit, 1);
						const preview = sourceLinePreview(document, hit.selectionStartLine, hit.title);
						assert.ok(preview.includes(hit.title));
						assert.ok(document.content.includes('// note'));
						if (includePreview) assert.ok(!preview.includes('/* declaration */'));
						firstPreviewMs ??= performance.now() - started;
					}
				}));
				assert.equal(readRequests, 25);
				if (sample > 0) samples.push({ includePreview, firstPreviewMs, pagePreviewMs: performance.now() - started, readRequests });
			} finally {
				client.dispose();
				if (child && child.exitCode === null && child.signalCode === null) await new Promise(resolve => child.once('exit', resolve));
			}
		}
	}
	const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)];
	console.log(JSON.stringify({ workload: '25 first-page rows spread over one synthetic 4000-line file; eight parallel readers; fresh MCP process per sample; indexing excluded; one discarded warmup per mode',
		sourceBytes: Buffer.byteLength(source), samples,
		medians: [false, true].map(includePreview => {
			const selected = samples.filter(sample => sample.includePreview === includePreview);
			return { includePreview, firstPreviewMs: median(selected.map(sample => sample.firstPreviewMs)), pagePreviewMs: median(selected.map(sample => sample.pagePreviewMs)), readRequests: selected[0].readRequests };
		}),
	}, null, 2));
} finally {
	await rm(root, { recursive: true, force: true });
}
