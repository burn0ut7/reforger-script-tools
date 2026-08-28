# Development

This guide covers the repository's stable local verification paths. Choose the
smallest command that proves the change, then run broader checks when the
change crosses a boundary.

## Build and Test

For a fresh checkout, install a current Node.js LTS release and the Rust toolchain
(including Cargo), then run `npm ci` from the repository root. Rust is a
development requirement only; packaged extension users receive the built server.

From the repository root:

| Command | Verifies |
| --- | --- |
| `npm run check-types` | TypeScript type checking. |
| `npm run lint` | Extension-source linting. |
| `npm run compile` | Type checking, linting, the bundled Rust server, and the extension bundle. |
| `npm test` | Extension test setup and test suite; its pretest step compiles tests and runs the full compile path. |
| `npm run package` | Production Rust server and production extension bundle. |
| `npm run test:packaged-official-wiki` | Build a VSIX, verify every Official Wiki Markdown byte is packaged, and launch the installed MCP runtime from an unrelated working directory. |
| `npm run mcp-api:generate` | Regenerate the MCP guide and per-tool contracts from live Rust descriptors. |
| `npm run mcp-api:check` | Fail when the guide or generated contract set has drifted. |

For Rust-only work, run `npm run test:server` in addition to focused tests. It
uses the non-default `test-hooks` feature required by spawned-process MCP
integration tests and writes its Cargo artifacts outside the shipped package.
For extension-facing TypeScript, language-client, or bundled-server changes,
run `npm run compile` after the final source edit.

### Cargo build artifacts

The normal development server build uses the ignored `server/target/` tree.
Test and investigation commands must use a separate ignored cache beneath
`.cache/cargo/<short-purpose>`; `npm run test:server` uses
`.cache/cargo/server-tests`. Do not create sibling `server/target-*`
directories. Cargo artifacts can grow by several gigabytes and may be removed
with `cargo clean --manifest-path server/Cargo.toml --target-dir <target-path>`.

The non-default `test-hooks` feature exists solely for spawned-process MCP
integration tests. Never enable it for `npm run compile`, `npm run package`, or
any distributable binary.

The development extension launches
`server/target/debug/reforger_language_server.exe`; `npm run compile` rebuilds
and restores it. Packaged installations use the executable copied beneath
`dist/server/`.

The Rust development profile retains debug information and assertions but uses
optimized code. This keeps the bundled development language server
representative enough for large-file editor latency while preserving the
release profile as the packaging authority.

### Developer report programs

Developer-only Rust reports and benchmarks live in `tools/server-reports/`,
outside the packaged server source tree. `server/Cargo.toml` declares them as
explicit example targets, so existing commands such as
`cargo run --manifest-path server/Cargo.toml --example <name> -- ...` remain
the supported way to run them. They are developer tooling and never a runtime
dependency of the extension.

To build a scorecard for the public non-Workbench MCP catalogue through real
stdio processes, run:

```powershell
npm run report:mcp-runtime -- --server <server.exe> `
  --addon-index-storage <addon-indexes> `
  --addon-source-inventory <graph.json> `
  --external-index-mode loaded `
  --workspace-scripts <scripts-root> `
  --dependency-project <addon.gproj> `
  --require-all
```

The runner discovers the live `tools/list` catalogue and excludes every
`workbench_*` tool. Recognized tools receive configured scenarios; a newly
listed tool without a scenario is a visible skipped coverage gap. The runner
follows real search handoffs into inspection, member, relationship, and bounded
source-read calls for Game Data and workspace code, follows an example result
into Game Data source, and follows the Official Wiki search-to-read handoff.
Text search covers narrow and broad literals, a regular expression, pagination
when a cursor is returned, and repeated same-process cache behavior. Each tool
records its first invocation, seven warm samples by default, response size, a
source-free stable-result fingerprint, result counts, and the applicable
operation budget.

Three fresh-process samples record process-to-initialize and first Game Data
status latency; these are process-cold observations, not claims about a cold OS
filesystem cache. Warm concurrency probes run at 1, 4, and 8 requests against
the first available semantic search surface, matching the server's bounded
admission ceiling. Change these with `--cold-samples`, `--samples`, and
`--concurrency-levels`; concurrency levels above eight are rejected. Use the
query flags shown by `--help` when defaults do not produce a handoff in the
selected corpus.

The default JSON and Markdown artifacts are written under ignored
`tools/reports/`. Retrieved source, excerpts, Wiki Markdown, and query strings
are not copied into either report. Unavailable authorities and missing
handoffs are reported as skipped rather than fast successes. Without
`--require-all`, partial coverage remains a successful diagnostic run; with it,
any skipped listed API makes the command exit nonzero. Tool errors, failed
concurrency calls, unstable fingerprints, protocol failures, and runner
failures always exit nonzero. Timings and over-budget statuses are local trend
evidence rather than portable CI gates; pass `--enforce-budgets` only for a
controlled machine/profile where those thresholds are intentional. Use
`npm run test:mcp-runtime-report` as the deterministic report contract gate.

To reproduce full-text search performance through the real MCP stdio path, run
one of:

```powershell
npm run report:text-search -- --server <server.exe> --source workspace `
  --workspace-scripts <scripts-root> --query <literal>
npm run report:text-search -- --server <server.exe> --source game-data `
  --addon-index-storage <addon-indexes> --addon-source-inventory <graph.json> `
  --dependency-project <addon.gproj> --external-index-mode loaded `
  --workspace-scripts <scripts-root> --query <literal>
```

The report initializes a dedicated MCP process, warms Game Data explicitly
when applicable, and records initialization, status, search, result-count, and
server-provided source-read and scan statistics without printing the query or source text. It
also reports `outsideScannerMs`, the end-to-end search time not accounted for
by the literal scanner, which primarily exposes source acquisition and MCP
projection overhead. The command defaults to a 5,000 ms performance budget and
a 35,000 ms hard timeout. Use
`--budget-ms` or `--timeout-ms` to change either bound. The command exits
nonzero for a timeout, structured tool failure, or over-budget search, making
it suitable for a local red/green optimization loop against real installed
Game Data. Machine-specific budgets are diagnostic evidence, not portable CI
thresholds. Matching ignores case by default; pass `--match-case`,
`--match-whole-word`, or `--regex` to exercise the corresponding MCP options.

To catalogue one or more PAC1 archives without extracting their contents, run:

```powershell
$env:CARGO_TARGET_DIR = ".cache/cargo/pack-catalogue-report"
cargo run --manifest-path server/Cargo.toml --example pack_catalogue_report -- `
  <archive.pak> [<another-archive.pak> ...]
```

The report lists catalogue and `.c` entry counts with elapsed time. It is a
safe inspection aid for add-on research, not an add-on discovery or indexing
command.

For the Workbench MCP contract gate, run:

```powershell
npm run test:workbench-mcp:runner
npm run test:workbench-mcp:contract -- --server server/target/debug/reforger_language_server.exe
```

The contract runner starts one real MCP stdio process, compares every
Workbench tool returned by `tools/list` with the generated MCP API Reference,
checks the public descriptor envelope, and writes a sanitized report beneath
`.cache/reports/`. It does not contact Workbench or launch processes. Live
scenarios are explicit and may be supplied with `--scenario <path>` together
with `--fixture <manifest>`. The fixture runner uses the public
`workbench_launch` MCP operation, waits for typed `workbench_status.isRunning`
readiness both before and after opening the editor, and owns cleanup only when
that operation reports that it started a new process. It records per-tool
minimum, maximum, p50, p95, p99, and failure counts. A failed scenario step is
not live evidence; an expected structured error is evidence of the tested
failure contract. Optional errors require an explicit structured code/phase
oracle. Steps that observe an unavailable or unsupported operation remain
explicit blocked evidence; they cannot substitute for a required
successful case. Each report contains `endpointPlan` and `endpointCorpus`: one
plan entry and one corpus record per published endpoint, with `passed`,
`failed`, `blocked`, or `not-run` status and the required cases, invocation
roles, facts, timings, structured errors, and assertion reasons supporting that
status.
When a fixture manifest disallows reuse of an existing Workbench process, the
runner also verifies the owned lifecycle: it restarts the exact process
reported by `workbench_launch`, adopts the replacement process ID, and stops
that replacement through the public MCP operations. The report records this
as `ownedLifecycle`; a reused-process smoke manifest intentionally skips it.
Live scenarios remain outside the normal fast test gate; the complete current
63-tool scenario is
`tools/fixtures/workbench-mcp/scenarios/test-bullshit-all-apis.json`. See
`tools/fixtures/workbench-mcp/README.md` for the manifest contract.

To extract only `.c` entries, pass `--extract-scripts <output-root>` before the
archives. The archive's logical `scripts/` path is retained below that root;
the command refuses to overwrite an existing extracted file.

For archive-reader performance evidence without filesystem extraction, pass
`--profile-scripts`. It reports catalogue and selection time, read/decode time,
compressed and original byte totals, throughput, compression distribution, and
the ten slowest selected entries. Catalogue timing is further split into chunk
scan, metadata-table read, and file-tree parse. These are local diagnostics,
not portable benchmark thresholds.

For physical-file extraction experiments, `--precreate-directories`,
`--sort-by-offset`, and `--workers <1-32>` can be combined with
`--extract-scripts`. Use a fresh output root for every run because extraction
deliberately refuses to overwrite files. Worker count is a machine-dependent
experiment, not a production default.

The bundled executable selects a protocol before either protocol starts:

```powershell
# Existing LSP mode (default)
dist/server/win32-x64/reforger_language_server.exe

# One MCP stdio session owned by the launching client
dist/server/win32-x64/reforger_language_server.exe mcp `
  --addon-source-inventory <global-storage-workbench-graph-path> `
  --addon-index-storage <global-storage-addon-index-directory> `
  --external-index-mode <all|loaded|none>
```

In VS Code, run **Reforger Script Tools: Copy MCP Configuration** and choose
Codex TOML or generic MCP JSON. The copied command contains the absolute
packaged runtime, loaded-add-on inventory, parser-owned index storage, and
packaged official-wiki evidence root, so the client does not depend on a
running VS Code process. The copied command captures the current
`reforgerScriptTools.workbench.externalIndexMode`; regenerate the configuration
after changing that setting. In `loaded` mode MCP filters compatible caches by
the persisted Workbench graph, while `all` publishes every compatible cached
add-on and `none` disables external Game Data.
After an extension upgrade, rerun the command and replace the client entry:
the versioned installed runtime path changes deliberately, and the extension
does not edit third-party client configuration itself.
The generated [MCP API Reference](mcp-api.md) is the inspectable agent-facing
guide and router to exact per-tool contracts; standard `tools/list` remains
authoritative at runtime.

## Official Wiki Corpus

`data/official-wiki/` contains copied Markdown from the official Arma
Reforger pages on `community.bistudio.com`. It remains the packaged source of
truth for the MCP Official Wiki authority; preserve each page's canonical
source URL in its H1 when updating it. Before redistributing an updated corpus,
review the upstream site terms and attribution requirements, retain required
notices, run the corpus validation/package tests, and verify the generated MCP
API reference. `wiki-index.md` is a rough AI navigation aid only and is never
authoritative runtime metadata.

For a repeatable release-binary LSP initialization measurement, run:

```powershell
node tools/lsp-startup-baseline.mjs `
  server/target/release/reforger_language_server.exe 7
```

For repeatable semantic-token latency checks against an installed game-data
index, use the dedicated benchmark example:

```powershell
cargo run --manifest-path server/Cargo.toml --example lsp_semantic_tokens_benchmark -- `
  --scripts <game-data-scripts-path> `
  --file <large-enfusion-file> `
  --iterations 7 `
  --max-median-resolver-ms <local-budget>
```

The command builds the external index once, warms the projection, reports
minimum/median/p95/maximum wall and resolver phase timings, and verifies that
token count, resolver-call count, and encoded-token fingerprint remain stable
between iterations. The optional latency budget makes the command fail for a
local regression loop; it is deliberately supplied by the caller rather than
treated as a portable machine-independent threshold.

For a repeated autocomplete measurement against one real source position, use
the completion benchmark:

```powershell
cargo run --release --manifest-path server/Cargo.toml `
  --example lsp_completion_benchmark -- `
  --scripts <game-data-scripts-path> `
  --file <source-file> `
  --line <one-based-line> `
  --character <zero-based-character> `
  --iterations 31 `
  --expect-fingerprint <sha256> `
  --max-median-us <local-budget>
```

The external index and document analysis are built once. The command warms the
completion path, reports wall and completion-phase latency in microseconds,
and fails when the serialized LSP completion-list fingerprint changes or the
caller-supplied median budget is exceeded.

For repeatable game-data cache startup measurements, use:

```powershell
node tools/index-cache-baseline.mjs --out <report-path>
```

The report compares warm-cache loading with a direct source rebuild in both
development and release profiles. It separates file read, binary decode, and
runtime-index construction time, and verifies that file, public-symbol,
parameter, local-variable-pruning, and lookup-map counts remain consistent.

For a repeatable synthetic semantic-search ranking measurement, use:

```powershell
$env:CARGO_TARGET_DIR = ".cache/cargo/semantic-search-benchmark"
cargo run --release --manifest-path server/Cargo.toml `
  --example semantic_search_benchmark -- `
  --declaration-pairs 5000 --iterations 31
```

The benchmark builds one immutable index containing paired original and
`modded` declarations, warms the search path, then reports minimum, median,
p95, and maximum search latency. It also verifies a stable fingerprint across
iterations so ranking changes remain visible alongside their timing impact.

For a live editor capture, generate the runtime report from the local
language-server log:

```powershell
node tools/lsp-runtime-performance-report.mjs --since-minutes 10
```

The first-response section distinguishes rich-token convergence from the
editor's later collection request and reports how often rich work finished
before that request. The extension diagnostic log records the corresponding
edit-to-middleware age and event-loop-turn delay without document paths, source
text, identifiers, or LSP payloads.

To diagnose the complete extension startup path, temporarily enable
`reforgerScriptTools.diagnostics.enabled`, reload the VS Code window once, and
then run:

```powershell
node tools/lsp-startup-trace.mjs
```

The report correlates extension activation, Workbench loaded-add-on graph
acquisition, language-server spawn and initialize response, external-index
ready notification, first document opening, and first semantic-token response.
The regular log is bounded and excludes source text, paths, and LSP payloads.
Disable the setting after the capture.

### Search UI snapshots

To capture the current search state while investigating a search-result or
paging problem:

1. Enable `reforgerScriptTools.diagnostics.enabled`.
2. Reload the VS Code window so diagnostics are initialized.
3. Open the Reforger Search UI and press `Ctrl+F3`.

The extension appends a `searchUi.snapshot` record to
`logs/extension-diagnostics.jsonl` under VS Code global storage. The record
contains the query, source and result-type filters, request status, page and
page-size state, total and per-source counts, warnings/errors, viewport data,
and bounded metadata for the visible results. Result metadata includes source
targets, selection line ranges, preview type, and excerpt sizes, but not the
source or preview text itself. It also includes bounded performance data:
end-to-end search time, initial source searches, high-page range traversal,
source-page size, remote page requests, cursor/cache hits, preview reads,
semantic-token hydration, and Webview response/render timing. This is intended
to identify whether a delay is in MCP paging, source merging, preview
hydration, or UI rendering without logging source text. The UI confirms when
the snapshot is written;
if diagnostics were not enabled before activation, it asks you to enable the
setting and reload first.

## Ticket Completion

Break a ticket into small, behavior-preserving implementation slices when that
reduces risk, but keep an explicit checklist of its full requirements. Verify a
slice before starting the next one; do not treat that verification or its commit
as ticket completion. Before handoff, compare the completed code and tests with
the whole ticket, run the applicable final checks, and record any editor or
Workbench validation that still requires a live session.

## Local Extension Session

The `Run Extension` launch configuration runs `npm run compile` before it starts
an Extension Development Host, so the bundled development server exists before
the client starts. Use `npm run watch` separately when iterating on TypeScript
or the extension bundle, then use the existing host for live editor checks.

The server build refreshes the development and packaged server binaries. It
also stops repository-owned running language-server processes before replacing
them, so verify the active development session after a server rebuild rather
than assuming a previous process reflects the change.

The unified `reforgerScriptTools.workbench.enabled` setting defaults to
`false`. When there is no prior approval, the extension asks whether it may
enable Workbench integration and install the managed bridge. Approval updates
the setting to `true`, writes `NetAPI_Enabled` as `REG_SZ "1"`, and registers
the per-user `enfusion://` URL handler at
`HKCU\Software\Classes\enfusion`. The handler command uses the resolved
Workbench executable, base-game add-ons directory, and `ArmaReforger.gproj`,
with every path and the `%1` URI argument quoted. Setup then either asks the
user to restart an open Workbench or completes without launching a closed
Workbench. Until that first prompt is answered, activation does not
register the Workbench compiler features, start the language server, show the
indexing progress indicator, install bridge scripts, or build indexes. A
decline stores the Workbench setting as `false`, after which the normal
non-Workbench language-server and indexing startup may proceed. Approval is
retained as internal extension state. Later enabled activations maintain or
upgrade the bridge without prompting, leave already-correct registry values
untouched, and repair a missing or stale `enfusion://` registration through the
same consented bootstrap. An explicitly disabled setting remains off.
An approved installation without an explicit enablement value remains off; the
stored approval never rewrites the setting.

The `reforgerScriptTools.workbench.externalIndexMode` setting controls external
index scope independently of NET API availability. Its default `loaded` mode
hydrates the compatible offline indexes for the opened project's dependency
GUIDs first, then reconciles them with the current live graph when Workbench is
available. It does not use a previous Workbench graph as a startup source.
`all` uses compatible cached indexes without a graph; `none`
disables external add-on indexes. Changing this setting immediately restarts
the language server and republishes the selected external-index layer; `none`
and `all` do not wait for a Workbench graph. On a warm approved startup, a
current managed manifest avoids repeating bridge maintenance and Workbench
process probing.

The Source Search panel owns a separate local MCP child. In `loaded` mode it
uses the same provisional dependency closure until the editor language server
accepts the current Workbench graph. That acceptance restarts an already-open
Search child and republishes Search Scope from the authoritative graph. Both
semantic and resource search therefore change scope together; resource search
resolves provisional dependency source roots from the parser-owned cache
catalogue rather than silently returning zero for a dependency GUID.

For `loaded` mode, an opened workspace with one unambiguous `.gproj` per folder
uses the project's transitive dependency descriptors for the no-Workbench
warmup. Rust consults the bounded Workbench project registry and adjacent
project descriptors by GUID, always adds the base-game dependency, and loads
matching caches only. The provisional scope is labelled separately from
Workbench-loaded data, and a live Workbench graph replaces it when available;
that live graph validates or builds missing or changed source roots. If the
cache contains duplicate GUIDs, the unpacked source-root cache is preferred
over a packed-only cache.
The **Reforger: Indexing loaded add-ons** progress indicator covers active
offline cache hydration, dependency indexing, PAC inspection, and index
publication. It closes when the offline index reaches a terminal state; it
does not remain open while waiting for an optional Workbench graph. A later
Workbench connection reconciles the authoritative scope independently.
Explicit user-requested refreshes show progress in VS Code's bottom status area
while actively indexing; automatic inactive-bridge recovery retries stay
silent. Diagnostic records
expose the same two ownership categories (`offline` and
`workbench-reconciliation`) as diagnostic `phase` values so warm-start
measurements can compare cache usability with the later authoritative refresh.
The progress stream may also emit operational sub-stages such as PAC
inspection and workspace indexing.

The active base-game artifacts are:

```text
<global storage>/addon-sources/workbench-graph-v1.json
<global storage>/addon-indexes/cache-catalogue.json
<global storage>/addon-indexes/<instance-key>/manifest.json
<global storage>/addon-indexes/<instance-key>/manifest-header.json
<global storage>/addon-indexes/<instance-key>/symbols.bin
<global storage>/addon-indexes/<discovered-guid>/inventory.json
```

No extracted script tree is part of the runtime contract. Go-to-definition
opens a read-only `reforger-pak:` document whose single source entry is decoded
by the Rust server on demand.

`symbols.bin` is a sectioned binary container. Its semantic-index section is
the warm-start payload; its optional locator section is read only when a packed
source document is first requested. `manifest-header.json` supplies compact
identity and artifact validation, and the full `manifest.json` is retained for
repair and diagnostics rather than the normal warm path.

To inspect the current graph/cache relationship, run **Reforger Script Tools:
Open Add-on Index Report**. It writes a report under extension global storage
with the Workbench graph snapshot, cache headers and exact source roots, and
recent external-index diagnostic events. Enable
`reforgerScriptTools.diagnostics.enabled` and reload VS Code before reproducing
if the lifecycle table is empty.

## Workbench Integration Verification

The deterministic Gateway tests run against a local TCP peer and verify real
wire framing, response decoding, typed failures, deadlines, and sanitized
outcomes. The compiler tests run in the Extension Development Host with
`src/test/workspace` opened as the single addon workspace. Use a focused
iteration command such as:

For a live NET API failure investigation, enable
`reforgerScriptTools.diagnostics.enabled`, reload VS Code, reproduce one
failure, and inspect the extension diagnostic JSONL file in the extension's
global-storage `logs` directory. The trace records the sanitized action and
deadline, child-process outcome, failure category, failure-inspection start and
completion, Workbench process state, log source/result, missing-handler match,
and notification dispatch. It never records NET API payloads, source text, or
raw Workbench log lines. The important event sequence is
`workbenchNetApiPrivateCallStarted`, `workbenchNetApiPrivateCallCompleted`,
`workbenchNetApiFailureInspectionStarted`,
`workbenchNetApiDiagnosisLogsRead`,
`workbenchNetApiFailureInspectionCompleted`, and
`workbenchNetApiFailureNotificationDispatched`; the first missing event
identifies the boundary where the report disappears.

```powershell
npm run compile-tests
node esbuild.js
npx vscode-test --grep "Workbench Gateway|Workbench compiler validation" --timeout 15000
```

The full `npm test` gate remains required before completion.

Automated peers do not replace live Workbench acceptance. With NET API enabled
in Workbench, open the same addon folder in VS Code and verify the configured
endpoint (default `127.0.0.1:5775`) before relying on protocol assumptions.
When launching a project manually, preserve every path containing spaces as a
single argument: quote both the `-gproj` path and the base-game `-addonsDir`
path, and use the Workbench installation directory as the working directory.
Otherwise Workbench can truncate a path at its first space, fail to load base
addon `58D0FB3206B6F859` (Arma Reforger), and cannot initialize the project.
The MCP launcher does not scan disks or guess a conventional installation
folder. It reads Steam's registered installation roots — the Windows registry,
or the installed Steam layouts on a Linux host — follows `libraryfolders.vdf`,
resolves app manifests `1874880` and `1874910`, and accepts only one unambiguous
manifest-backed installation for each app. On a Linux host it also resolves the
Wine prefix that runs Workbench and starts it through whoever owns that prefix;
see [Host platform](host-platform.md).
The live MCP fixture runner uses the public `workbench_launch` MCP operation;
it never manually constructs a Workbench process command line. It discovers
World Editor through `workbench_list_editors`, opens it with
`workbench_open_editor`, opens the known world with `workbench_open_resource`,
and verifies the canonical active path through `workbench_state`. The project
configuration used by `workbench_launch` must include the base-game add-on and
the test add-on dependencies in one available add-on tree. See the [official
startup parameter reference](https://community.bistudio.com/wiki/Arma_Reforger%3AStartup_Parameters)
for Workbench's own project configuration when preparing that environment.
Run a clean `WORKBENCH` validation, introduce and save a deliberate compiler
error, and confirm the reported file and line. Then edit again to observe a
stale finding and fix the error to observe atomic replacement. Also verify
disabled integration, a deliberately wrong configured port, and a save
conflict. Record any Workbench version-specific error-code, line-number, or
readiness behavior in the existing Workbench research journals before changing
the codec or diagnostic projection.

## Documentation Changes

For documentation-only changes, run `git diff --check` and verify changed
Markdown links and paths manually. Add documentation only for a lasting module
contract, workflow, decision, or reusable evidence format; route readers from
the [documentation index](README.md) instead of duplicating the same fact.
