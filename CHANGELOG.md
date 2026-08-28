# Change Log

All notable changes to the "reforger-script-tools" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

## [Unreleased]

- Added Linux support. The language, indexing, search, and MCP features run
  natively, and Workbench integration works against the Wine prefix that runs
  Workbench, including Steam's Proton compatibility data.
- Added `reforgerScriptTools.workbench.winePrefix` for a Workbench prefix the
  extension cannot resolve on its own.
- Added host desktop registration for `enfusion://` links, so opening a resource
  from the Search UI reaches Workbench inside its prefix.
- Fixed a Workbench compiler error in the bundled bridge: the radius entity
  query named an `EQueryEntitiesFlags` member the engine does not define. Its
  unsupported `features` query scope is removed; `all`, `static`, and `dynamic`
  are unchanged.

## [2.0.0] - 2026-08-03

- Added automatic discovery and indexing for installed add-ons and base-game data.
- Added a Search browser for scripts, resources, text, and documentation; open it with `Ctrl+Alt+F`.
- Added a bundled MCP server for script, resource, add-on, and documentation search, plus Workbench inspection and editing.
- Improved indexing, search, source-preview, and startup performance.
- Added semantic, punctuation-colored, and native VS Code bracket presentation modes.
