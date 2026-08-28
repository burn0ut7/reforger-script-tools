# Reforger Script Tools

## Mission

Build a high-fidelity Enfusion Script toolchain: correct language
understanding, reliable editor behavior, and a durable path to a complete
parser, semantic model, index, and language server. Do not take shortcuts that
make that destination worse.

## Router

Route work to the owner instead of growing a second implementation path.

- `src/extension.ts`: activation and top-level wiring only.
- `src/extensionConfig/`: extension-facing IDs, names, defaults, and limits.
- `src/gameData/`: game-data acquisition and source resolution.
- `src/languageClient/`: VS Code process lifecycle, transport, and thin editor
  bridges.
- `server/`: lexing, parsing, syntax, semantics, indexing, diagnostics,
  formatting, and LSP behavior.
- `server/src/host_platform.rs`: every fact about the host that runs Workbench —
  Steam layout, Wine prefix, path-space translation, Workbench's registry,
  process identity, launch route, and desktop URL schemes.
- `tools/`: developer tooling only; never a runtime dependency.

Keep TypeScript as the editor shell and Rust as the language engine. Workbench
and the compiler establish Enfusion truth; they are not a second language
server. Keep the runtime ownership boundaries intact.

## Documentation Router

Read `docs/README.md` before creating or updating documentation. It owns the
documentation lifecycle, including the completion check. Read only the document
that owns the question before changing a non-trivial area:

- `docs/overview.md`: project purpose and evidence hierarchy.
- `docs/architecture.md`: cross-layer flow and ownership boundaries.
- `docs/language-engine.md`: Rust analysis, snapshot, and LSP contract.
- `docs/development.md`: local build, test, and development workflow.
- `docs/host-platform.md`: Windows and Wine hosts, path-space translation, and
  host-owned process, registry, and launch routes.

Documentation records durable context; code and tests remain the implementation
source of truth. Extend an existing document when it owns the subject. Create a
new one only for a lasting subsystem contract, decision, or workflow. At task
completion, update the owning document when the change affects its contract.

## Taste

- Prefer precise language facts and full-fidelity parsing over text matching.
- Keep modules deep: expose a small, clear contract and hide the complexity
  behind it. Do not add a manager, registry, wrapper, setting, or validation
  layer without a concrete present need.
- Keep TypeScript typed, thin, deterministic, and editor-facing. Do not move
  parsing, indexing, completion, or semantic decisions out of Rust.
- Use one authoritative path for a feature. Temporary migrations need an
  explicit removal condition.
- Avoid fallbacks when the authoritative path can serve the request. A
  fallback must be explicitly provisional for a proven unavailable fact, not a
  parallel normal path that can race or diverge.
- When Workbench is the authority, acquire the complete fact through one
  Workbench-owned route. Do not combine default installation paths, user
  settings, name-based discovery, or guessed filesystem locations as normal
  alternatives. Preserve an unavailable or ambiguous Workbench fact as such
  rather than selecting a substitute.
- Do not branch behavior on a feature, API, or owner label when the same
  semantic or structural fact applies generally. Add a special case only for a
  proven distinction that requires different behavior.
- Marketplace installs must be self-contained: no user-installed Rust, Cargo,
  Node.js, npm dependencies, or separate language server.

## Reforger Truth

Before changing or asserting Enfusion Script, Workbench, game-data,
parser/model/index, diagnostics, formatting, or language-feature behavior,
consult the smallest relevant primary evidence directly.

Use evidence in this order:

1. Workbench/compiler behavior.
2. Official Reforger documentation.
3. Verified extracted game data.
4. Source examples and fixtures, labelled by confidence.

Never infer Enfusion behavior from C#, Unity, Unreal, Arma 3, SQF, or generic
language conventions.

### Exploratory evidence standard

Treat searches of official Reforger documentation, extracted game data, and
game-source examples as curious exploratory work. Do not stop at the first
plausible API, wiki page, or example. Search broadly enough to identify the
most useful supported surfaces, alternative implementation patterns, and
constraints before selecting an approach. Narrow to the smallest relevant
evidence set only after that exploration establishes which route is most
appropriate; record material uncertainty or coverage gaps rather than treating
an early match as exhaustive proof.

## Workbench Development Loop

For every change that affects Workbench bridge scripts, editor behavior, or an
Enfusion Script workflow, follow this continuous evidence loop. Do not treat a
passing validation run as proof that Workbench is running the new behavior.

### MCP-first Workbench control

- Always use and prioritize the named public `workbench_*` MCP tools for
  Workbench operations. Start with `docs/mcp-api.md`, then open only the exact
  linked contract under `docs/mcp-api/tools/` needed for the operation.
- Use MCP for Workbench launch, status, project context, editor and resource
  opening, world inspection and editing, saving, validation, bridge
  maintenance, reload, logs, window inspection, play sessions, stop, and
  restart whenever the public capability exists.
- Never manually launch the Workbench executable, simulate Workbench UI input,
  call the private `workbench-api` process mode or raw NET API as a substitute,
  or use shell process control when a public MCP capability owns the action.
- If an MCP operation is unavailable or fails, preserve and diagnose that MCP
  failure through its structured result, recovery guidance, and MCP-exposed
  status or logs. Lower-level inspection may diagnose the MCP implementation,
  but it does not replace MCP acceptance and must not become a parallel normal
  workflow.

1. Search the official wiki/documentation for the relevant editor behavior and
   API surface.
2. Search verified game-data and game-source examples. Read the relevant
   examples to establish the correct API, expected behavior, and the most
   useful result for an AI caller.
3. Make the smallest change. Add targeted `PrintFormat` or `Print` diagnostics
   when they will prove the handler reached the expected state or isolate a
   failure. Do not leave noisy exploratory logging in the finished workflow.
4. Run the Workbench validation script and review its output. This establishes
   that the scripts compile, but does not activate them in a running
   Workbench.
5. Reload Workbench scripts using the Workbench reload command when ready for
   live testing. Use a complete restart only after that reload is unavailable,
   fails, or the editor reports that it cannot reload the affected scripts. In
   particular, if a managed handler script fails compilation and therefore
   prevents its own reload or execution, perform a complete graceful restart
   into the same resolved Workbench `.gproj` and addon context; do not treat a
   still-running process with stale handlers as a live acceptance environment.
6. Review Workbench logs and confirm that the scripts related to the change
   compiled and loaded without errors. The reload action's immediate acceptance
   response is only a dispatch observation; the fresh log marker sequence is
   the source of truth for whether a reload actually occurred.
7. Test the changed behavior through the real Workbench/API workflow.
8. Review Workbench logs again when needed, capture relevant `PrintFormat` or
   `Print` output, and use that output to confirm success or drive the next
   iteration.
9. Remove temporary debugging logs, diagnostic output, and provisional fixes
   that were only needed to investigate the issue. Keep only intentional,
   useful production-facing output.

Repeat this loop until the live behavior is verified. Use the public Workbench
MCP commands yourself; do not ask the user to reload, validate, or inspect logs
when the available MCP tooling can do it.

## Basic Rules

- Keep release inputs, test inputs, developer tooling, and generated artifacts
  separate. The release build may consume only its explicit runtime assets; it
  must never discover test fixtures, reports, debug binaries, coverage, or
  Cargo build output by directory traversal or a broad include.
- `server/` contains the shipped Rust package and its source. Put standalone
  test fixtures, test drivers, reports, benchmarks, and debugging utilities
  under `tools/` (or another explicit development-only root), never in a
  release-output directory such as `dist/`. Rust unit tests may stay beside the
  code they exercise when guarded by `#[cfg(test)]`.
- Generated build and test artifacts must be outside source and release-output
  trees, and ignored by Git. In particular, direct Cargo test, example, or
  validation commands must use an external or explicitly ignored target
  directory; do not create ad-hoc `server/target-*` directories. `dist/` is
  reserved for the explicitly selected packaged runtime assets.
- Test-only behavior must not be present in a release binary. Prefer
  `#[cfg(test)]`; when a spawned integration-test process genuinely needs a
  hook, put it behind an explicit non-default Cargo feature used only by that
  test build, never behind an ambient environment variable in a normal build.
- Package verification must inspect the final VSIX file list and confirm that
  it contains no test, report, debug, source, or build-cache material beyond
  the deliberately bundled runtime assets.
- Treat user settings as intentional controls; do not expose internal consent
  or bookkeeping as settings.
- Runtime state belongs under `globalStorageUri`; `globalState` is for small,
  durable flags only. Do not write runtime state into source files.
- Keep diagnostics optional, centralized, concise, and outside the workspace.
- Read the relevant source and the routed documentation before a non-trivial
  change. Do not add per-file documentation machinery.
- For extension-facing TypeScript,
  language-client, or bundled-server changes, run `npm run compile` after the
  final source edit. State any live Workbench/editor validation still pending.
- For changes to Rust server source or files embedded in the server binary,
  including `server/bridge/`, run `npm run compile` once after the final edit
  in a coherent batch and before relying on live runtime behavior. Do not rerun
  it when those inputs have not changed. Documentation, reports, scenarios,
  and developer-tool-only changes do not require rebuilding the server.
- Commit coherent, attributable local changes after verification, then push the
  commit to the current branch. Do not create or switch branches, open a PR,
  change remotes, force-push, or rewrite history unless explicitly asked.

## Agent skills

### Issue tracker

Issues live in GitHub Issues; external pull requests are not a triage surface.
See `docs/agents/issue-tracker.md`.

### Triage labels

The default five-role triage vocabulary is used. See
`docs/agents/triage-labels.md`.

### Domain docs

This is a single-context repository rooted at `CONTEXT.md`. See
`docs/agents/domain.md`.
