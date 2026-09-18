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
- [MCP API Reference](mcp-api.md): generated capability index and one exact
  generated contract per tool under `mcp-api/tools/`.
- [Workbench MCP test workflows](workbench-mcp-test-workflows.md): dependency
  chains, ordered live coverage, readback invariants, cleanup, and corpus
  status rules for automated Workbench MCP tests.
- [Key input routing](key-input-routing.md): VS Code key-routing boundary and
  the ownership policy for atomic typing assists.

## Research

- [Architecture review journal](research/architecture-review-journal.md):
  module-boundary review notes and follow-up opportunities.
