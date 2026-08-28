# Documentation

This directory records durable context that code alone cannot communicate:
architecture, module boundaries, workflow, and consequential decisions. Code
and tests are the source of truth for implementation.

Core contracts stay at this directory's root. Investigation journals and
supporting evidence belong under `research/`; accepted architectural decisions
belong under `adr/`; agent workflow contracts belong under `agents/`. Do not
recreate path-mirrored, per-file current-state documentation. Add a document
only when it explains a stable module boundary, a consequential decision, or a
reusable evidence contract. Update an existing document when it owns the
subject.

## Documentation Lifecycle

This is the repository's documentation policy. Read it before creating or
updating any documentation.

At task completion, decide whether the completed change altered a documented
contract, architecture boundary, workflow, design decision, or evidence
format. Update the document that owns that context when it did. Do not add a
documentation change merely to restate implementation details already clear in
code and tests.

For ticketed work, completion means every ticket requirement has been checked
against the final code and verification evidence. An independently verified
implementation slice is progress, not a completed ticket.

## Core Documents

- [System overview](overview.md): product purpose and sources of truth.
- [Architecture](architecture.md): module boundaries and runtime invariants.
- [Language engine](language-engine.md): Rust analysis and LSP contract.
- [Development](development.md): build, test, and local development workflow.
- [Host platform](host-platform.md): the Windows and Wine hosts that run
  Workbench, path-space translation, and host-owned process, registry, and
  launch routes.
- [MCP API Reference](mcp-api.md): generated AI usage guide and categorized
  router to one exact generated contract per tool under `mcp-api/tools/`.
- [MCP Runtime](mcp-runtime.md): process startup, parser-owned cache
  index reuse, searches, and the separate Workbench route.
- [Workbench world-entity relation search](workbench-world-entity-search.md):
  AI-facing discovery, paging, relation-evidence, and follow-up-inspection
  workflow for live World Editor entities.
- [Workbench MCP test workflows](workbench-mcp-test-workflows.md): dependency
  chains, ordered live coverage, readback invariants, cleanup, and corpus
  status rules for automated Workbench MCP tests.
- [Key input routing](key-input-routing.md): VS Code key-routing boundary and
  the ownership policy for atomic typing assists.

## Research

- [Architecture review journal](research/architecture-review-journal.md):
  module-boundary review notes and follow-up opportunities.
- [MCP server exploration journal](research/mcp-server-research.md): current
  capability boundary and future Workbench expansion for the local Reforger
  MCP runtime.
- [ArmoryForger MCP useful-feature backlog](research/mcp-feature-backlog-research.md):
  evidence-led comparison and prioritised candidate capabilities beyond the
  current MCP tool surface.
- [Workbench NET API exploration journal](research/workbench-net-api-research.md):
  extracted protocol evidence, adapter boundary, and validation backlog.
- [Workbench compiler-validation research](research/workbench-compiler-validation-research.md):
  `ValidateScripts` contract, continuous-validation design, diagnostics, and
  live-session acceptance experiments.
- [Workbench script-reload research](research/workbench-script-reload-research.md):
  version-pinned reload-command evidence, public API limits, and the
  background-safe acceptance experiment.
- [Workbench water-surface sampling research](research/workbench-water-surface-research.md):
  primary API evidence, live acceptance findings, and the boundary between
  engine-registered water and editor-authored water generators.
- [Enfusion structural-formatting research](research/enfusion-structural-formatting-research.md):
  evidence and design notes for automated editing behavior.
- [Enfusion `new` editing research](research/enfusion-new-formatting-research.md):
  primary-source evidence and safe completion/formatting boundary for `new`.
- [Add-on PAK indexing research](research/addon-pak-indexing-research.md):
  authoritative add-on discovery, PAK extraction boundaries, and independent
  cache-identity findings for the multi-add-on index design.
- [Virtual source indexing research](research/virtual-source-indexing-research.md):
  archive-backed source identities, VS Code navigation constraints, and the
  case for a virtual-source default with optional physical materialization.
- [Current indexing performance baseline](research/current-indexing-performance-baseline.md):
  measured physical Game Data cache, source-validation, PAC extraction, and
  bare-server startup timings for evaluating the PAC-backed design.
- [LSP feature performance review](research/lsp-feature-performance-review.md):
  live whole-runtime evidence plus controlled semantic-coloring and formatting
  before/after measurements.
- [Warm manifest and locator overhead research](research/warm-manifest-locator-overhead-research.md):
  measured manifest/locator costs and the recommended lazy, compact, and binary
  representations for warm startup and first source navigation.
- [s&box MCP server review](research/sbox-mcp-research.md): comparative source
  examples for future evaluation; it does not define Reforger MCP architecture.
- [Official Wiki Corpus report](research/official-wiki-corpus-report.md): validated
  packaged-corpus coverage, MCP retrieval evidence, and AI-use limits.
- [MCP search path-forward research](research/mcp-search-path-forward-research.md):
  proposed phases for workspace semantic search, text evidence, Wiki match
  quality, and AI routing.
- [Multi-add-on search-scope research](research/multi-addon-search-scope-research.md):
  authoritative add-on discovery, Prototype C behavior, multi-add-on MCP
  identity, filtering, source-read safety, and the staged implementation plan.
