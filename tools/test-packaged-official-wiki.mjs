import { spawnSync } from 'node:child_process';
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { unzipSync } from 'fflate';

const root = process.cwd();
const sandbox = mkdtempSync(join(tmpdir(), 'reforger-packaged-wiki-'));
const vsix = join(sandbox, 'reforger-script-tools.vsix');
const installed = join(sandbox, 'installed client é space');
const clientWorkingDirectory = join(sandbox, 'independent cwd Ω');
const gameDataScripts = join(sandbox, 'Game Data é space', 'scripts');

try {
  run('npx', ['--no-install', 'vsce', 'package', '--no-dependencies', '--out', vsix]);
  const archive = readFileSync(vsix);
  const recordedModes = recordedUnixModes(archive);
  for (const [path, contents] of Object.entries(unzipSync(archive))) {
    const output = join(installed, path);
    mkdirSync(dirname(output), { recursive: true });
    writeFileSync(output, contents);
    // VS Code restores each entry's recorded Unix mode when it installs a
    // VSIX. Plain extraction does not, so restore it here as well; otherwise
    // this runs against a server the real install would have left runnable.
    const mode = recordedModes.get(path);
    if (mode !== undefined) writeMode(output, mode);
  }
  mkdirSync(clientWorkingDirectory, { recursive: true });
  mkdirSync(gameDataScripts, { recursive: true });
  writeFileSync(join(gameDataScripts, 'PackagedFixture.c'), 'class PackagedFixture {}\n');

  const sourcePages = markdownFiles(join(root, 'data', 'official-wiki'));
  const installedPages = markdownFiles(join(installed, 'extension', 'data', 'official-wiki'));
  if (JSON.stringify(Object.keys(sourcePages)) !== JSON.stringify(Object.keys(installedPages)) || Object.keys(sourcePages).some(path => !sourcePages[path].equals(installedPages[path]))) {
    throw new Error('The VSIX Official Wiki Corpus differs from the authoritative source tree.');
  }
  if (!Object.hasOwn(installedPages, 'index.md')) throw new Error('The authoritative index.md is missing from the VSIX.');

  const serverEntry = `extension/dist/server/${process.platform}-${process.arch}/${process.platform === 'win32' ? 'reforger_language_server.exe' : 'reforger_language_server'}`;
  // A host that carries an executable bit must find it recorded in the VSIX:
  // VS Code installs exactly what the archive records, and a server without it
  // cannot be started by the installed extension.
  if (process.platform !== 'win32' && ((recordedModes.get(serverEntry) ?? 0) & 0o111) === 0) {
    throw new Error(`The packaged language server is not recorded as executable (${serverEntry}); the installed extension could not start it.`);
  }
  const executable = join(installed, ...serverEntry.split('/'));
  const wikiSession = runMcp(executable, [], clientWorkingDirectory, [
    toolListRequest(2),
    toolCallRequest(3, 'official_wiki_status', {}),
  ]);
  const listed = response(wikiSession, 2).result.tools;
  const evidenceTools = listed.slice(0, 10);
  const installTool = listed.find(tool => tool.name === 'workbench_install_bridge');
  const stopTool = listed.find(tool => tool.name === 'workbench_stop');
  const restartTool = listed.find(tool => tool.name === 'workbench_restart');
  if (listed.length !== 87
    || evidenceTools.some(tool => tool.annotations?.readOnlyHint !== true || tool.annotations?.openWorldHint !== false)
    || installTool?.annotations?.destructiveHint !== true
    || stopTool?.annotations?.destructiveHint !== true
    || restartTool?.annotations?.destructiveHint !== true) {
    throw new Error(`Installed runtime did not advertise the expected evidence and guarded Workbench tools: ${wikiSession.stdout}`);
  }
  const status = response(wikiSession, 3);
  if (status?.result?.structuredContent?.available !== true) throw new Error(`Installed corpus was unavailable: ${wikiSession.stdout}`);
  if (status.result.structuredContent.fileCount !== Object.keys(sourcePages).filter(path => path !== 'wiki-index.md').length) throw new Error('Installed corpus page count is incomplete.');
  const wikiSearchSession = runMcp(executable, [], clientWorkingDirectory, [
    toolCallRequest(2, 'search_official_wiki', { query: 'Reforger', limit: 1 }),
  ]);
  assertUnderFiveSeconds('cold Official Wiki search', wikiSearchSession);
  const readInput = response(wikiSearchSession, 2).result.structuredContent.results?.[0]?.readInput;
  if (!readInput) throw new Error(`Installed wiki search did not return a read handoff: ${wikiSearchSession.stdout}`);
  const wikiReadSession = runMcp(executable, [], clientWorkingDirectory, [
    toolCallRequest(2, 'read_official_wiki', readInput),
  ]);
  const wikiRead = response(wikiReadSession, 2).result.structuredContent;
  if (!wikiRead?.sourceUrl || !wikiRead.content || wikiRead.relativePath !== readInput.relativePath) {
    throw new Error(`Installed wiki read did not complete the search handoff: ${wikiReadSession.stdout}`);
  }
  const gameDataReadySession = runMcp(executable, [
    '--workspace-scripts', gameDataScripts,
  ], clientWorkingDirectory, [
    toolCallRequest(2, 'search_workspace_symbols', { query: 'PackagedFixture' }),
  ]);
  if (response(gameDataReadySession, 2).result.structuredContent?.results?.[0]?.name !== 'PackagedFixture') {
    throw new Error(`Installed workspace catalogue did not become ready: ${gameDataReadySession.stdout}`);
  }
  const gameDataSession = runMcp(executable, [
    '--workspace-scripts', gameDataScripts,
  ], clientWorkingDirectory, [
    toolCallRequest(2, 'search_workspace_symbols', { query: 'PackagedFixture' }),
  ]);
  const gameDataSearch = response(gameDataSession, 2).result.structuredContent;
  assertUnderFiveSeconds('ready workspace search', gameDataSession);
  const gameDataHit = gameDataSearch?.results?.[0];
  if (gameDataHit?.name !== 'PackagedFixture') {
    throw new Error(`Installed workspace search did not complete: ${gameDataSession.stdout}`);
  }
  const gameDataInspectSession = runMcp(executable, [
    '--workspace-scripts', gameDataScripts,
  ], clientWorkingDirectory, [
    toolCallRequest(2, 'inspect_workspace_symbol', gameDataHit.inspectInput),
  ]);
  if (response(gameDataInspectSession, 2).result.structuredContent?.name !== 'PackagedFixture') {
    throw new Error(`Installed workspace inspection did not complete the search handoff: ${gameDataInspectSession.stdout}`);
  }
  const gameDataReadSession = runMcp(executable, [
    '--workspace-scripts', gameDataScripts,
  ], clientWorkingDirectory, [
    toolCallRequest(2, 'read_workspace_source', gameDataHit.readSourceInput),
  ]);
  if (!response(gameDataReadSession, 2).result.structuredContent?.content.includes('class PackagedFixture')) {
    throw new Error(`Installed workspace source read did not complete the search handoff: ${gameDataReadSession.stdout}`);
  }
  const physicalPaths = [installed, clientWorkingDirectory]
    .map(path => path.replaceAll('\\', '/'));
  for (const session of [wikiSession, wikiSearchSession, wikiReadSession, gameDataSession, gameDataInspectSession, gameDataReadSession]) {
    const output = session.stdout.replaceAll('\\', '/');
    if (physicalPaths.some(path => output.includes(path))) {
      throw new Error(`Installed MCP output leaked a physical path: ${session.stdout}`);
    }
  }
  console.log(`Verified ${Object.keys(sourcePages).length} byte-identical packaged Markdown files, 87 installed tools, and independent workspace and Official Wiki workflows.`);
} finally {
  rmSync(sandbox, { recursive: true, force: true });
}

/**
 * The Unix file mode each archive entry records, read from the ZIP central
 * directory. Entries written by a host without file modes record none.
 */
function recordedUnixModes(archive) {
  const modes = new Map();
  const end = findEndOfCentralDirectory(archive);
  if (end === undefined) throw new Error('The VSIX has no ZIP end-of-central-directory record.');
  const entries = archive.readUInt16LE(end + 10);
  if (entries === 0xffff) throw new Error('The VSIX uses ZIP64, which this verification does not read.');
  let offset = archive.readUInt32LE(end + 16);
  for (let entry = 0; entry < entries; entry += 1) {
    if (archive.readUInt32LE(offset) !== 0x02014b50) throw new Error('The VSIX central directory is malformed.');
    const hostSystem = archive.readUInt8(offset + 5);
    const externalAttributes = archive.readUInt32LE(offset + 38);
    const nameLength = archive.readUInt16LE(offset + 28);
    const extraLength = archive.readUInt16LE(offset + 30);
    const commentLength = archive.readUInt16LE(offset + 32);
    const name = archive.toString('utf8', offset + 46, offset + 46 + nameLength);
    // Host system 3 is Unix; only those entries carry a file mode.
    if (hostSystem === 3) modes.set(name, (externalAttributes >>> 16) & 0o7777);
    offset += 46 + nameLength + extraLength + commentLength;
  }
  return modes;
}

function findEndOfCentralDirectory(archive) {
  for (let offset = archive.length - 22; offset >= 0; offset -= 1) {
    if (archive.readUInt32LE(offset) === 0x06054b50) return offset;
  }
  return undefined;
}

function writeMode(path, mode) {
  if (process.platform === 'win32' || mode === 0) return;
  chmodSync(path, mode);
}

function markdownFiles(directory) {
  return Object.fromEntries(readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return Object.entries(markdownFiles(path)).map(([child, contents]) => [join(entry.name, child), contents]);
    return entry.isFile() && entry.name.endsWith('.md') ? [[entry.name, readFileSync(path)]] : [];
  }).map(([path, contents]) => [path.replaceAll('\\', '/'), contents]).sort(([left], [right]) => left.localeCompare(right)));
}

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: 'inherit', shell: process.platform === 'win32' });
  if (result.status !== 0) throw new Error(`${command} failed with ${result.status}`);
}

function runMcp(executable, args, cwd, requests) {
  const request = [
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'package-test', version: '1' } } },
    { jsonrpc: '2.0', method: 'notifications/initialized' },
    ...requests,
  ].map(JSON.stringify).join('\n') + '\n';
  const startedAt = performance.now();
  const result = spawnSync(executable, ['mcp', ...args], { cwd, input: request, encoding: 'utf8', timeout: 15_000 });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Installed MCP runtime failed: ${result.stderr}`);
  return {
    stdout: result.stdout,
    elapsedMs: performance.now() - startedAt,
    responses: result.stdout.split(/\r?\n/).filter(Boolean).map(JSON.parse),
  };
}

function assertUnderFiveSeconds(operation, session) {
  if (session.elapsedMs >= 5_000) {
    throw new Error(`${operation} exceeded the five-second ceiling (${Math.round(session.elapsedMs)} ms).`);
  }
}

function response(session, id) {
  const result = session.responses.find(message => message.id === id);
  if (!result) throw new Error(`Installed MCP runtime did not respond to ${id}: ${session.stdout}`);
  return result;
}

function toolListRequest(id) {
  return { jsonrpc: '2.0', id, method: 'tools/list', params: {} };
}

function toolCallRequest(id, name, arguments_) {
  return { jsonrpc: '2.0', id, method: 'tools/call', params: { name, arguments: arguments_ } };
}
