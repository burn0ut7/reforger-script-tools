import { build } from 'esbuild';
import { createRequire } from 'node:module';
import { EventEmitter } from 'node:events';

const require = createRequire(import.meta.url);

// Exercise the actual client while replacing only the editor launch adapter.
export async function loadSearchClient(overrides = {}) {
	const compiled = await build({
		entryPoints: ['src/searchPrototype/mcpSearchClient.ts'],
		bundle: true, platform: 'node', format: 'cjs', write: false,
		plugins: [{
			name: 'test-launch-policy',
			setup(builder) {
				builder.onResolve({ filter: /mcpConfiguration$/ }, () => ({ path: 'launch', namespace: 'fixture' }));
				builder.onLoad({ filter: /.*/, namespace: 'fixture' }, () => ({
					contents: 'export function buildMcpLaunchConfiguration(options) { return {command: options.serverPath, args: []}; }',
				}));
			},
		}],
	});
	const module = { exports: {} };
	const load = name => name === 'node:child_process' && overrides.spawn
		? { spawn: overrides.spawn } : require(name);
	new Function('require', 'module', 'exports', 'setTimeout', 'clearTimeout', compiled.outputFiles[0].text)(
		load, module, module.exports, overrides.setTimeout ?? setTimeout, overrides.clearTimeout ?? clearTimeout,
	);
	return module.exports;
}

export function fakeChild() {
	const child = new EventEmitter();
	child.stdout = new EventEmitter();
	child.stderr = new EventEmitter();
	child.stdin = new EventEmitter();
	child.sent = [];
	child.stdin.write = text => child.sent.push(JSON.parse(text));
	child.stdin.end = () => {};
	child.kill = () => { child.killed = true; };
	child.reply = (id, result = {}) => child.stdout.emit('data', Buffer.from(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`));
	return child;
}
