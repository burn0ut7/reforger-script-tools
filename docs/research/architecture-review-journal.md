# Reforger Script Tools architecture review — 2026-09-18

Baseline: `2fb63cca`, branch `MCP`. Review scope: extension activation and
launch policy, Search UI, document analysis, MCP execution, add-on/cache
selection, Workbench ownership, developer tooling, and documentation routing.
This is a source-led architectural review, not exhaustive proof of every
language feature or live Workbench operation. No ADR files were present.

## Changes made

| Change | Before → after | Compatibility and verification |
| --- | --- | --- |
| Reuse lexical tokens | Each foreground/semantic stage lexed directly, then the parser lexed again → parser borrows that stage's existing token stream | Same parser, diagnostics, syntax construction, and cancellation checkpoints; regression checks cover tokenization count and malformed-source recovery. Foreground and semantic stages still parse independently. |
| Consolidate MCP cancellation | Five result-type-specific helpers → one generic helper | Same `IndexBuildControl` cancellation and 100 ms join grace; request deadlines, errors, and admission ownership are unchanged. The two-authority search helper remains distinct. |
| Remove executable substitution | Missing packaged runtime could select Cargo output in Production/Test → those modes require the packaged executable | Development keeps its existing development-first, packaged-second behavior. This matches the documented self-contained package contract. |
| Repair Search UI launch profile | Paginated Search UI launched `authoring`, which rejects its specialist tools → private Search UI process explicitly selects `all` | Native AI provider/configuration retains `authoring`; no new setting or parallel launch builder. Added real-process semantic/text search acceptance. |
| Remove retired skill tooling | Three orphaned validators required absent packaged skills → deleted `tools/agent-skills.mjs`, its test, and `tools/check-agent-skills.mjs` | Package manifest, VSIX allowlist, and existing activation tests already exclude these skills. Client-managed skills and `.codex/` are untouched. Removed 378 lines of obsolete tooling. |
| Remove repeated checks and stale routes | `pretest` repeated lint already run by `compile`; documentation index had 19 absent targets → one lint pass and existing document routes | Created this journal at the already-designated review path and removed the other 18 broken index entries. Corrected obsolete skill-packaging claims in README and owning docs. |

## Prioritized remaining work

The user authorized pursuing the full review as an active goal on 2026-09-18.
Complete each item through its acceptance checks or record the evidence for
retaining a necessary path. A documented uncertainty alone does not complete
an implementation priority. Commit and push coherent verified slices on `MCP`.

| Work | Execution status |
| --- | --- |
| 1. External scope selection | Completed: shared cached selection and removal of stale-graph fallback; build and 1,052 Rust tests pass. |
| 2. MCP worker lifetime | Completed: one launcher owns actual worker admission; build and 1,054 Rust tests pass. |
| 3. Candidate index ownership | Completed: exact runtime owners replace substitution; build and 1,057 Rust tests pass. |
| 4. Shared foreground syntax | Completed: shared immutable syntax; profile improves allocations/retention, build and 1,058 Rust tests pass. |
| 5. Cache metadata | Verified: retained compact/repair formats, removed unreachable decode, corrected cache-only provenance. Build and 1,058 Rust tests pass; first-navigation measurement follows the priority 6 report repair. |
| 6. Compatibility tools | Completed: shared dispatch, unchanged contracts, current report coverage; build, 1,059 Rust tests, API check, and five report tests pass. Real-cache navigation correctly rejects a changed pack. |
| 7. Search UI caching/lifecycle | Completed: shared cache setup/session cleanup; build, four direct client tests, and 209 editor tests pass; measured page reuse retained. |
| 8. Workbench ownership | Public launch now reached through the existing stdio test client, but Windows rejected process creation; diagnosis and live acceptance remain pending. |
| 9. Preview ownership | Completed: Rust lexical projection and existing semantic spans replace the TypeScript scanner; 1,063 Rust tests, five direct client tests, five report tests, and 210 editor tests pass. |
| Clean-window MCP activation | Completed: native discovery readiness race fixed in acceptance; three consecutive isolated-window runs and lint pass. |
| Final acceptance | Full Rust/extension/package checks and feasible live Workbench acceptance. |

Recommendation strength indicates confidence in investigating the seam, not
permission to delete behavior before its acceptance conditions are met.

The clean-window test originally made one discovery request before VS Code
1.138's `AfterRestored` MCP discovery contribution had registered extension
providers, then waited only for extension activation. Repeating native discovery
and server start until the provider is ready fixes that test race and also covers
cached definitions that activate only when resolved. No explicit extension
activation, unconditional startup event, or production workaround was added.
Three consecutive runs pass (approximately 0.8 s each), including the native
Official Wiki tool call and dormant Workbench checks. Evidence: installed VS Code
1.138 bundle's `mcpDiscovery` contribution, `discoverCollections`, and
`workbench.mcp.listServer`/`startServer`; logs
`.cache/reports/review-clean-window{,-repeat-1,-repeat-2}.log`.

| Priority | Candidate | Strength | Reason to defer implementation |
| --- | --- | --- | --- |
| 1 | One external scope policy for symbols and resources | Strong | Must specify offline/stale resource semantics and preserve source validation. |
| 2 | One MCP worker lifetime policy | Strong | Cancellation and concurrency changes require stalled-worker stress tests. |
| 3 | Remove cross-index candidate substitution | Strong | Must prove every candidate carries the correct owner, including fixtures and single-index callers. |
| 4 | Reuse foreground syntax in semantic analysis | Strong | Needs revision/cancellation and memory measurements, not an extra mutable cache. |
| 5 | Retire overlapping cache metadata formats | Worth exploring | Headers and locator tables deliberately avoid expensive warm-start reads. |
| 6 | Migrate compatibility tools by caller contract | Worth exploring | Search UI still uses specialist search; compact discovery is not equivalent. |
| 7 | Reduce Search UI caching and lifecycle complexity | Worth exploring | Page caches support back-navigation and cursor traversal; measure retained memory first. |
| 8 | Deepen Workbench internals at existing seams | Worth exploring | Live compilation/reload/recovery acceptance was unavailable. |
| 9 | Remove handwritten comment parsing from Search previews | Worth exploring | Preserve preview quality and latency through Rust-owned lexical facts. |

## 1. One external scope policy for symbols and resources

**Completed:** Cached instance filtering, stable ordering, and dependency GUID
preference now live in one selector consumed by symbols and resource metadata.
Resource `all` mode uses that selector without requiring an inventory; the
second stale-graph parser was deleted. Workspace loose-resource projection
remains separate and requires its current graph identity. Missing cached roots
retain their recorded identity, and resource reads explicitly label an exact
cached resource snapshot as stale. A corrupt semantic payload does not make
otherwise readable resource metadata unavailable.

Validation: the new regression covers no/missing inventory, disabled mode,
workspace exclusion and live-resource preservation, a corrupt semantic cache,
stale resource provenance, and warm operation with full manifests removed.
Existing duplicate-GUID and loaded-scope union tests remain green. `compile`
and all 1,052 Rust tests passed; logs are `.cache/reports/review-scope-compile.log`
and `.cache/reports/review-scope-server.log`. The selection refactor adds no
semantic decode or full-manifest read to resource selection or warm startup.
This is an I/O-path claim, not an end-to-end speedup claim. Primary resource
evidence: packaged `Resource Manager` lines 3–15 and `Data Modding Basics`
lines 5–28; no engine-facing identifiers changed.

The original investigation below records the problem and acceptance rationale.

**Files:** `server/src/game_data_catalogue.rs::initialize_catalogue`,
`server/src/resource_catalogue.rs::ResourceCatalogue::from_config_for_addons`,
`server/src/addon_sources.rs::read_cached_combined_addon_sources`.

**Problem:** `all` has two implementations. Symbol search loads every
compatible cached add-on index via `load_all_cached_addon_indexes`. Resource
search requires an inventory and tries `read_loaded_addon_sources`, then
`read_loaded_addon_sources_allow_stale` on any error. Thus the same requested
mode can select different add-ons or fail on different prerequisites. This is
observable source divergence; this review did not reproduce it against a
complete live resource corpus.

**Before / after:**

```text
Before: mode --> symbol catalogue --> compatible caches
             --> resource catalogue --> live inventory --> stale inventory
After:  mode --> one resolved add-on scope --> symbol and resource projections
```

**Solution:** Deepen the add-on scope module so both adapters consume the same
identity and authority decision. Keep packed-resource reading and semantic
index loading separate. Explicitly decide when a missing physical root is a
stale resource, unavailable resource, or an excluded add-on.

**Benefits:** Locality for scope decisions; leverage for LSP, MCP symbols, and
resources without duplicating source acquisition.

**Acceptance:** Cover `loaded`, `all`, and `none` with missing inventory,
stale roots, duplicate GUID instances, workspace exclusions, partial caches,
and one corrupt cache. Assert identical selected identities where the contract
promises them, explicit availability differences, and unchanged warm-start I/O.
Do not remove the deliberate offline index feature.

## 2. One MCP worker lifetime policy

**Completed:** All 22 blocking-worker launch sites now use one private
launcher that moves admission into the worker. Request cancellation, deadlines,
and tool-specific error mapping retain their existing behavior. The test-only
noncooperative delay now covers every family through that same launcher and
remains gated behind the non-default `test-hooks` feature and debug builds.

The new real-process tests cancel and time out eight admitted calls across 15
wiki, Game Data, and workspace operations, verify that a ninth waits beyond the
join grace, exercise ping while saturated, and check admission resumes and EOF
terminates the process. Cancelled requests publish no result. Existing intent
research, unified-search, panic, and Workbench tests remain separate coverage.
Focused stress checks, `compile`, and all 1,054 Rust tests pass. Logs:
`.cache/reports/review-admission-focused.log`,
`.cache/reports/review-admission-compile.log`, and
`.cache/reports/review-admission-server.log`. Live Workbench acceptance remains
part of the final review gate; this change preserves its existing worker-owned
permit and request-cancellation behavior.

The original investigation below records the problem and acceptance rationale.

**Files:** `server/src/mcp/mod.rs::{search_official_wiki,
search_game_data_symbols,research_game_data,search_reforger,
blocking_workbench_call,cancel_worker}`;
`server/tests/mcp_stdio.rs` cancellation/admission tests.

**Problem:** Cancellation helper duplication is removed, but admission still
has two implementations. Wiki search and several older catalogue operations
hold `_permit` in the async request; intent research, unified search, and
Workbench calls move it into the blocking worker. An async request can return
after its bounded cancellation join while a non-cooperative worker remains
alive. In the former arrangement the permit no longer bounds that worker's
lifetime. This is a control-flow risk, not a measured production overload.

**Before / after:**

```text
Before: request owns permit OR blocking worker owns permit
After:  admitted blocking work owns permit until that work actually exits
```

**Solution:** Establish one internal execution policy with explicit
per-operation deadlines and error projection. Preserve separate cancellation
controls where authorities differ. Avoid a configurable scheduler framework.

**Benefits:** Locality for cancellation/admission invariants; leverage across
tool families while preserving each public interface.

**Acceptance:** Inject workers that ignore cancellation beyond the join grace;
repeatedly cancel and time out each affected family. Assert actual live-worker
count remains bounded, ping remains responsive, EOF shuts down, cancelled
results cannot publish, and Workbench mutations retain their distinct effects.

## 3. Remove cross-index candidate substitution

**Completed:** Candidates now capture their producing index's runtime
identity. The LSP lookup accepts only that exact owner, regardless of evidence
category or slot, and the workspace/Game Data fallback chain is removed.
Independent clones and decoded indexes get distinct runtime identities without
changing the persisted format. Candidate deduplication also includes ownership;
previously equal numeric IDs could suppress a candidate from another index.

Focused regression checks cover colliding IDs, missing and replaced owners,
all four source categories, unchanged serialized snapshots, layered indexes,
fixture hover, static-constant coloring, and debug details. `compile` and all
1,057 Rust tests pass. Logs: `.cache/reports/review-ownership-focused.log`,
`.cache/reports/review-ownership-compile.log`, and
`.cache/reports/review-ownership-server.log`.

The original investigation below records the problem and acceptance rationale.

**Files:** `server/src/lsp/external_indexes.rs::ExternalIndexes::for_candidate`;
callers in `hover.rs`, `debug_hover.rs`, and `semantic_tokens.rs`.

**Problem:** The candidate's `source_kind` chooses an index, but absence falls
through `.or(self.workspace).or(self.game_data)`. Callers then use the
candidate's symbol ID in the selected index. This can hide an ownership defect
and potentially interpret an ID in another index. Production reachability is
not established; `Unknown` and `Fixture` metadata and single-index report
entry points need examination before removing it.

**Before / after:**

```text
Before: candidate owner --> matching index? --> workspace? --> game data?
After:  candidate owner --> that captured index, or explicit unavailable
```

**Solution:** Make candidate ownership sufficient to choose its captured
index. Remove substitution only after all producers and developer reports
preserve that fact. Do not special-case language-feature names.

**Benefits:** Better locality for symbol identity and leverage for every
feature using the same resolution result.

**Acceptance:** Test colliding numeric IDs across workspace/Game Data,
unavailable owners, layered indexes, fixture sources, hover, static-constant
coloring, and debug reports. Verify that a missing owner never selects a
different declaration and ordinary navigation remains identical.

## 4. Reuse foreground syntax in semantic analysis

**Completed:** The foreground result owns one immutable lexical/parse
allocation. Semantic jobs capture that exact allocation with the admitted
source revision. The document no longer retains a second parser output, and
semantic analysis no longer rebuilds tokens/parse or copies parser diagnostics.
No mutable cache or request-time rebuild was added. Existing admission and
publication gates remain intact.

An opt-in unit benchmark compares repeated construction with sharing on the same
286,890-byte synthetic document (2,000 declaration fixtures), using seven warm
samples. At the median-total sample, foreground time was 12.70 / 11.76 ms and
combined analysis time was 72.83 / 59.91 ms; allocation calls were 391,184 /
287,155 and retained bytes were 36,643,502 / 27,205,054 (repeated / shared).
This isolates construction in one process and counts allocations on the measured
thread; it is not an editor end-to-end latency or process-RSS claim. The allocator
wrapper exists only in the unit-test build. Log:
`.cache/reports/review-syntax-profile.log`.

Focused checks prove one tokenization, shared ownership, malformed-source
diagnostics, rejection of superseded results, and release of an obsolete parse.
The executor check covers both stages without semantic re-tokenization.
`compile` and all 1,058 Rust tests pass, including cancellation, closed-document,
overload, and external-generation cases. The manual profile is intentionally
ignored by the regular suite and passed separately. Logs:
`.cache/reports/review-syntax-compile.log` and
`.cache/reports/review-syntax-server.log`.

The original investigation below records the problem and acceptance rationale.

**Files:** `server/src/lsp/runtime_scheduler.rs::RuntimeWorkExecutor::execute`,
`server/src/lsp/open_documents.rs::{OpenDocument,FileIndexAnalysis,
file_index_for_source_with_timings}`, `server/src/lsp/document_runtime.rs`.

**Problem:** The completed change removes repeated lexing within each stage.
The foreground worker still builds tokens and a parse, and the semantic worker
later builds them again for the same revision. `OpenDocument` retains syntax
and foreground facts alongside `FileIndexAnalysis`'s parse and tokens.

**Before / after:**

```text
Before: snapshot --> foreground lex/parse --> foreground publication
                --> semantic lex/parse --> semantic publication
After:  snapshot --> immutable lexical/syntax facts --> both publications
```

**Solution:** Carry immutable syntax from admitted foreground work into semantic
work while keeping the existing stage scheduling and publication gates.
The deletion test favors removing repeated construction; it does not favor
collapsing latency-sensitive foreground work into the slower semantic stage.

**Benefits:** Depth in the snapshot module, locality of revision ownership, and
less duplicate CPU/allocation work without another feature cache.

**Acceptance:** Measure large-file edit latency, retained bytes, and allocation
counts. Exercise superseded edits, cancellation, closed documents, parser
errors, overload, and external-index changes. A mutable shared parse or
request-time synchronous rebuild would violate the current contract.

## 5. Retire overlapping cache metadata formats

**Decision:** Retain the compact catalogue/header, semantic container, and full
repair manifest. They serve different I/O and recovery contracts. Current
add-on writers always embed binary locators; the generic cache writer also
supports indexes without locators. Compatible older add-on caches still need
the JSON-locator recovery path, and missing headers must remain recoverable
offline. Retire that compatibility only with an explicit cache-format transition
and a verified replacement for offline source navigation. No format migration
is justified merely to reduce the file count.

The current local three-instance cache has 7,563 bytes of headers versus
3,576,289 bytes of full manifests, plus an 8,353-byte catalogue. Thirty warm
Node read/JSON-parse samples had medians of 0.37 ms for the headers and 6.73 ms
for the manifests. This diagnostic-reader comparison is not Rust startup
timing; it confirms the scale of metadata avoided. The real-process baseline
loaded 146,931 symbols: initial Game Data status took 136.83 ms, repeated status
median 2.82 ms, and two fresh processes had first-status median 123.80 ms.
The initial report did not measure first navigation: it omitted an explicit
tool profile and had no scenarios for the compact generic tools. The repaired
priority 6 report reaches all 23 tools, but first navigation remains unavailable:
the installed `data007.pak` differs from the recorded cache revision. Source
reads and source-backed references correctly return `source_evidence_unavailable`.
All declared source roots and pack files exist; the rejection is an integrity
check, not missing-path discovery. Do not bypass that check to obtain a timing.
Reconcile through the authoritative Workbench scope before repeating navigation.

**Implemented cleanup:** Removed the unreachable full-manifest retry after
header projection decoding fails: a valid full manifest already contains every
header field. Regression assertions preserve that projection and rebuilding a
missing catalogue when the compact header is also absent. Cache-only scopes
now report `cached-instances`, use the offline log phase, and no longer claim
that directory order is authoritative Workbench order in relationship evidence.
The 12 focused cache checks, production build, and all 1,058 Rust tests pass.
Logs: `.cache/reports/review-cache-compile.log` and
`.cache/reports/review-cache-server.log`.
Measurements are under `.cache/reports/review-cache-{metadata-profile.json,runtime.json}`.

The original investigation below records the problem and acceptance rationale.

**Files:** `server/src/addon_sources.rs::{cached_manifest_descriptors,
scan_cached_manifest_descriptors,load_cached_source_revision,
read_project_dependency_scope_guids}`; `server/src/index_cache.rs`;
`src/gameData/addonIndexReport.ts::readAddonCacheHeaders`.

**Problem:** Selection/navigation spans `cache-catalogue.json`,
`manifest-header.json`, `manifest.json`, and the self-describing binary plus
optional locator section. Missing catalogue triggers a directory scan;
missing headers can trigger full-manifest reads; absent binary locators trigger
JSON locator registration. Header corruption and header absence do not always
have the same repair policy. TypeScript's diagnostic report separately reads
the metadata pair.

**Before / after:**

```text
Before: catalogue --> scan --> header --> full manifest
        navigation --> header --> binary locators OR JSON locators
After:  one versioned persisted instance contract
        --> derived selection catalogue and diagnostic presentation
```

**Solution:** Audit all current writers before selecting one metadata authority
for a future cache version. Make repair an explicit reconstruction path and
retire old readers at a declared version transition. Preserve a compact
selection catalogue if it avoids opening every instance.

**Benefits:** Locality for identity/version validation; fewer cascading readers
and a smaller test surface. A separate diagnostic view need not become another
authority.

**Acceptance:** Warm startup must not decode full manifests or locators;
source reads must still verify revision/digest; partial/corrupt caches must not
discard healthy instances. Benchmark first navigation and offline cold/warm
startup. Existing tests include
`one_corrupt_warm_cache_does_not_discard_other_cached_instances` and
`dependency_cache_prefers_unpacked_source_for_a_duplicate_guid`.
Do not simply delete the fast header/catalogue as apparent duplication.

## 6. Migrate compatibility tools by caller contract

**Completed:** Six exact-symbol aliases now adapt their existing inputs into
the generic authority-selected dispatch. Their names, schemas, error messages,
filters, and cursor contracts remain compatible. Paginated search remains a
supported specialist surface; public exact aliases can be retired only after
supported callers migrate. The generated API guidance now makes that distinction.

A real stdio regression compares both sources across inspection, member and
relationship pages, cross-name cursor use, changed filters, stale references,
and unknown arguments. The performance harness explicitly launches `all`,
covers the 23 current non-Workbench tools (including generic calls for both
sources), and removes the obsolete example-search scenario. Fresh processes
measure first navigation before a text scan can warm source locators. The five
deterministic report checks, focused parity check, production build, all 1,059
Rust tests, and generated API check pass. Generated tool schemas are unchanged.
The real-cache report reaches all 23 tools, and exposes the changed-pack source
integrity rejection described in priority 5. This is not a successful navigation
sample or a zero-latency measurement. Logs: `.cache/reports/review-alias-{compile,
server,api,report-tests}.log`; the real-cache report is
`.cache/reports/review-alias-script-runtime.json`.

The original investigation below records the problem and acceptance rationale.

**Files:** `server/src/mcp/mod.rs::{McpToolProfile,full_tool_catalogue,
call_tool_by_name}`, `src/searchPrototype/mcpSearchClient.ts::searchToolFor`,
`server/src/source_relationships.rs`, `server/src/workspace_catalogue.rs`.

**Problem:** The compact discovery/generic symbol interface coexists with
authority-specific compatibility tools. The Search UI still calls paginated
`search_game_data_symbols`, `search_workspace_symbols`, text/resource tools,
and relationship queries. The profile bug fixed in this review demonstrates
that an exposed tool list and an actual caller's needs can diverge.
Generic symbol operations already dispatch into the same catalogue owners;
the names alone are not proof of two semantic implementations.

**Before / after:**

```text
Before: compact tools + compatibility tools + specialist Search UI callers
After:  compact discovery + explicit paginated specialists
        + generic exact-symbol handoffs; retire only superseded aliases
```

**Solution:** Inventory callers and distinguish aliases from feature-bearing
specialists. Migrate exact inspection/member aliases through the existing
generic handoffs where equivalent; state a supported-client removal condition.
Retain pagination, scopes, filters, and relationship evidence semantics.

**Benefits:** A smaller protocol interface with the same module depth;
locality in the shared catalogue implementation.

**Acceptance:** Compare generated schemas, errors, cursor binding, source
handoffs, filters, and Search UI behavior through real stdio processes.
Replacing paginated search with `search_reforger`'s one hit per authority would
remove features and is not an acceptable cleanup.

## 7. Reduce Search UI caching and lifecycle complexity

**Completed:** Three copies of query-cache creation/eviction now share one
private operation. Dispose and process exit share session cleanup. Cleanup now
clears pending timers and partial protocol input; old process events and an old
initialization rejection cannot clear or dispose a replacement session. The new
behavioral tests reproduced the timer/buffer failures before the fix and cover
restart ownership, scope changes, cached navigation, and stale-cursor recovery.

**Retention decision:** Keep the bounded page caches and independent MCP
process. In a synthetic client-only workload (40 queries, 40 pages, 100 hits per
page, one fresh process per mode), retained heap was approximately 25.9 / 31.8 /
53.9 / 66.0 MB for semantic/text/resource/relationship modes. Revisited pages
required 0 / 0 / 0 / 1 remote requests; the relationship call refreshes revision
evidence on page one. Before/after profiles retain the same page counts and
request counts, with effectively unchanged retained memory. Removing these
caches would replace useful navigation reuse with remote requests. This is not
a claim about full editor memory, real-server latency, or a memory reduction.
Resource/relationship chains retain up to the existing 100-page UI limit;
semantic/text caches retain 32 pages per query. The outer bound remains 32
query keys. Logs: `.cache/reports/review-search-cache-{before,after}.json`.

The production build, four direct client tests, and 209 editor tests pass.
Preview cancellation and mode/scope changes retain their existing editor checks.
Logs: `.cache/reports/review-search-{compile,tests,editor}.log`.

The original investigation below records the problem and acceptance rationale.

**Files:** `src/searchPrototype/mcpSearchClient.ts::{search,sourceRange,
searchPage,searchRelationships,searchResources,startProcess}`;
`src/searchPrototype/searchUiPrototype.ts::{getClient,restartSearchScope}`;
`server/src/{workspace_catalogue,game_data_catalogue}.rs` text result caches.

**Problem:** The TypeScript client owns up to 32 query page caches, repeated
cursor traversal, source merging, and child-process/revision lifecycle. Rust
also caches query results; LSP and the Search UI's independent MCP process hold
their own workspace/index snapshots. These are separate costs and freshness
contracts, not automatically redundant implementations.

**Before / after:**

```text
Before: webview state --> TS query/page cache --> MCP result cache --> sources
After:  webview state --> minimal retained pages/cursors --> same MCP authority
```

**Solution:** Measure cache retention and navigation reuse before reducing TS
retention or sharing cursor bookkeeping across the three search modes. Keep
the standalone MCP process contract; do not make external MCP clients depend
on VS Code's LSP process. Renaming `searchPrototype` alone is cosmetic.

**Benefits:** Locality for paging/freshness, less retained duplicated output,
and a simpler presentation adapter if measurements support the change.

**Acceptance:** Back/forward pages, mixed sources, stale cursors, changed
workspace scope, cancelled previews, and process restarts must retain behavior.
Measure memory after many queries and latency of revisited pages. Do not trade
cached navigation for repeated full-corpus scans.

## 8. Deepen Workbench internals at existing seams

**Files:** `server/src/workbench.rs`, `server/src/workbench_bridge.rs`,
`server/src/workbench_capture.rs`, `server/src/mcp/mod.rs`,
`src/workbenchNetApi/gateway/workbenchGateway.ts`.

**Problem:** `workbench.rs` has 13,475 lines and MCP approximately 8,600,
including tests. Workbench combines transport, discovery, managed files,
process lifecycle, operation projection, confirmation state, and logs. A
mutation crosses these responsibilities, making a local change expensive to
understand. File length identifies a review hotspot, not a reason to split it
into shallow per-operation wrappers.

**Before / after:**

```text
Before: many typed operations --> one large mixed implementation
After:  same typed interface --> transport / managed package / lifecycle
                                 / editor operations at existing seams
```

**Solution:** Extract cohesive internal ownership without growing the public
interface or adding another gateway. Start with a responsibility whose tests
already cross one seam. Preserve one Workbench-owned discovery route and one
controller-owned mutation/recovery path.

**Benefits:** Locality for effects and recovery; deeper modules that hide
implementation rather than move it into many caller-visible objects.

**Acceptance:** Native validation, reload generation/log evidence, public MCP
round trips, exact-identity process operations, persistence/readback, and
failure recovery. Keep save-first force-stop recovery and managed-file
migration until supported upgrade/recovery cases can be preserved another way.
The unavailable live endpoint prevented acceptance of such a refactor here.

## 9. Remove handwritten comment parsing from Search previews

**Completed:** Rich previews mask the comment spans already supplied by Rust.
Bounded symbol reads request an optional `previewContent` projection in their
existing MCP response. The shared Rust lexer scans only through the returned
range, retaining earlier multiline state. Raw `content`, complete-document
reads, text-match evidence, and Wiki content remain intact. No request, parser,
semantic job, or mutable cache was added. The TypeScript quote/comment scanner
was deleted; masking preserves UTF-16 columns for subsequent semantic tokens.

**Measured cost:** The 25-row first-page profile uses eight concurrent readers
and one 323,121-byte synthetic source, with seven measured fresh-process samples
per mode. Median first-row latency was 4.79 / 6.29 ms and whole-page latency
17.36 / 24.07 ms without / with the projection. Both modes made exactly 25
source reads. This is a small measured CPU cost, not a performance improvement
or a full editor latency claim; initial indexing and rich hydration are excluded.
A separate Rust microprofile measured 1 / 1,951 microseconds for a 75-byte
returned range after a 75 / 308,000-byte prefix. Evidence:
`.cache/reports/review-preview-{page-profile.json,profile.log}`.

Tests cover multiline comments beginning before the read window, strings and
escaped quotes, malformed source, CRLF, supplementary Unicode characters,
cancellation, both real MCP source authorities, raw-text preservation, and
semantic-token positioning. Build and generated API checks pass, as do all
1,063 Rust tests (two opt-in profiles ignored), five direct client checks, and
five report checks. The editor run first timed out in an unchanged compiler
test at its two-second Mocha deadline; an unchanged rerun passed all 210 tests.
Logs: `.cache/reports/review-preview-*-final.log` and
`.cache/reports/review-preview-editor-repeat.log`. Primary syntax evidence:
packaged `Scripting Values` lines 195–217, existing game-data-derived lexer
fixtures, and the Rust lexer's existing comment/string token contract. No new
engine API or language classification rule was introduced.

The original investigation below records the problem and acceptance rationale.

**Files:** `src/searchPrototype/mcpSearchClient.ts::{stripSourceComments,
sourcePreviewLine,sourceContextPreview}`, `src/searchPrototype/semanticPreview.ts`,
`server/src/lsp/preview_context.rs`, `server/src/lexer.rs`.

**Problem:** TypeScript tracks quotes, escapes, and block/line comment markers
to choose and trim preview lines. Multi-line previews call this scanner once
per line, resetting its block-comment state. Rust already owns lexical facts
and declaration-aware preview context. This is a second source-classification
path in the editor shell, even though its output is presentation only.

**Before / after:**

```text
Before: Rust source/context --> TS comment scanner --> preview
After:  Rust source/context plus lexical spans --> presentation-only preview
```

**Solution:** Use existing Rust lexical/semantic information to choose preview
content, retaining raw source for evidence reads. Delete the handwritten
scanner only after all preview callers can obtain the needed facts without a
new full-document analysis or per-result process round trip.

**Benefits:** Locality for comment/string interpretation and a thinner adapter;
the same lexical interface gains leverage in editor and search presentation.

**Acceptance:** Multi-line block comments, escaped quotes, comment delimiters
inside strings, comment-only hits, malformed source, and bounded source reads.
Compare first-page preview latency and request counts; do not delay every row
waiting for unnecessary rich semantic analysis.

## Fallbacks deliberately retained


| Path | Why deletion is not behavior-preserving |
| --- | --- |
| Native typing after a declined/unavailable formatting request | Enter/Tab/Space must still function when the language engine cannot supply an edit. |
| Current-revision lexical results while semantic work is pending | Keeps editing useful without borrowing stale semantic facts. |
| Offline dependency scope followed by live Workbench reconciliation | An intentional startup feature; authority changes and immutable publication are explicit. |
| Cache repair from authoritative source | Required after missing/incompatible/corrupt cached data; it is not a second language engine. |
| Status polling and missing-profile recovery | Workbench supplies no equivalent push lifecycle in the current contract. |
| Bounded locked-binary replacement retries | Windows process/file locks are an observed development constraint. |
| Separate LSP and standalone MCP adapters | They reuse the same Rust engine and have different client/process lifetimes. |

## Evidence and verification

Source and existing tests establish the implementation facts above. Primary
Reforger evidence consulted: packaged Official Wiki `Script Editor`, lines
1–11, and `Enforce Script Syntax`, lines 10–74, corpus
`ow1:d40df5e4830cab07dbc5a9c1c06beadd521c4214315d43176954e54c27f682e7`.
The Game Data status reported 6,678 indexed files, 146,931 symbols, one lossy
source file, and one loaded add-on without a compatible index. These are
coverage observations, not proof of all language behavior. An exact
`ScriptEditor` search was ambiguous and was not used to justify engine calls.
No new engine-facing identifiers or bridge scripts were introduced.

`workbench_status` returned `workbench_unavailable` at the configured endpoint,
support reference `wb-82840-1789742692719-2`. No live compiler, reload, editor,
or runtime acceptance is claimed.

- Passed: TypeScript type checking, lint, 30-file bridge-style check,
  Rust `cargo check --features test-hooks --lib --bins --tests`, TypeScript test
  compilation, extension bundling, 73 focused VS Code tests, and diff
  whitespace checking. The focused suites cover executable selection, launch
  configuration, Search UI mapping, real-process semantic/text search, native
  MCP discovery, and the absence of packaged Agent Skills. All eight links in
  the documentation index resolve, and active code/build/docs have no remaining
  references to the deleted validator interface.
- Initially blocked: `npm run compile` and `npm run test:server` at the missing Microsoft
  `link.exe`. The current Rust changes were statically checked, including test
  targets, but their tests could not execute. VS Code real-process acceptance
  used the pre-existing packaged Rust binary; it does not validate the changed
  Rust parser/cancellation code. Fresh-binary VSIX acceptance remains pending.
- Local logs: `.cache/reports/deep-review-rust-tests.log` and
  `.cache/reports/deep-review-extension-tests.log`. The first focused run had
  71 passes and one obsolete exact-source-text assertion failure; that assertion
  was removed in favor of the new real-process acceptance, and the final run
  had 73 passes with no failures.
- No end-to-end performance improvement is claimed from operation-count
  reduction alone. Executable validation after installing the C++ toolchain is
  recorded in the follow-up below.

### Toolchain follow-up, 2026-09-18

Installing the MSVC x64/x86 tools and Windows SDK resolved the missing-linker
failure. The development build now links successfully, and the generated MCP
reference and all 91 tool contracts match the server.

Running the Rust suite exposed a test-only self-deadlock in
`failed_workbench_graph_refresh_clears_prior_game_data_without_hiding_workspace_facts`.
Its refresh call received `graph_generation` through a temporary mutex guard
that survived until the call returned; the refresh then tried to lock the same
state. Read the generation in a separate statement so the guard drops before
the call. This change is confined to `#[cfg(test)]`.

After the fix, `npm run compile` passed again, the focused regression passed in
0.02 seconds, and `npm run test:server` passed all 1,051 tests with no failures
or ignored tests. Final logs are `.cache/reports/toolchain-compile-final.log`,
`.cache/reports/toolchain-rust-focused.log`, and
`.cache/reports/toolchain-server-tests-final.log`.

The full VS Code workspace suite passed 209 tests against the freshly built
server. The separate no-workspace native MCP acceptance test failed under
VS Code 1.138.0: `workbench.mcp.listServer` did not activate the contributed
provider within the test's five-second wait. The cause is unresolved; provider
registration and direct MCP-process tests do not substitute for this activation
check. Investigate the clean-window activation path before declaring the full
extension gate green.
The run used `npm test --ignore-scripts` after separately completing its
pretest steps, avoiding a duplicate development build. Its log is
`.cache/reports/toolchain-extension-tests.log`.

Production packaging and `npm run test:packaged-official-wiki` passed. The
VSIX allowlist contains 319 files (excluding the container content-types entry),
and the installed runtime verified 311 byte-identical Wiki files, its 20-tool
authoring profile, and independent workspace/Wiki search, inspection, and read
workflows. This closes the original fresh-binary VSIX coverage gap. See
`.cache/reports/toolchain-package-tests.log`.

Live Workbench status remained unavailable, support reference
`wb-82840-1789744536236-4`; no live editor acceptance is claimed.

## Top recommendation

Resolve [external scope selection](#1-one-external-scope-policy-for-symbols-and-resources)
first: it contains two present implementations of the same user setting,
making it a better consolidation target than deleting intentional recovery or
splitting a large file for appearance.
