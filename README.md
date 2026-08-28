# Reforger Script Tools

Reforger Script Tools brings Enfusion Script language support and Arma Reforger
Workbench compiler feedback to Visual Studio Code. Everything needed by the
extension is included; no additional tools or runtimes are required.

## Unofficial Project and Product Terms

Reforger Script Tools is an independent, unofficial project. It is not
affiliated with, authorized by, endorsed by, or supported by Bohemia
Interactive a.s.

Use of this extension with Bohemia Interactive games, tools, services, or
content remains subject to all applicable end-user license agreements, terms of
use, and content licenses, including but not limited to the
[Arma Reforger EULA](https://reforger.armaplatform.com/eula) and
[Arma Reforger Workshop Terms of Use](https://reforger.armaplatform.com/workshop-terms). This extension is not designed or intended to circumvent
those agreements, violate license restrictions, or enable others to do so.
Content created or generated with this extension must be made and used only
for Arma purposes, in accordance with the [Arma Public License
(APL)](https://www.bohemia.net/community/licenses/arma-public-license) and its
ArmaOnly condition where applicable. A mod that incorporates, adapts, or is
distributed with another mod's content must preserve and comply with that
content's applicable license terms, including any requirements for derivative
works. Users are responsible for ensuring that their use and all resulting
content comply with the agreements and licenses applicable to the Bohemia
Interactive products and content they use.

Bohemia Interactive, Arma, Arma Reforger, and associated logos and designs are
trademarks or registered trademarks of Bohemia Interactive a.s.

## Features

- Enfusion Script syntax highlighting and semantic coloring without replacing
  your selected VS Code theme.
- Context-aware completion, snippets, signature help, hover information, and
  go to definition.
- Document symbols and indexing of Reforger base-game data.
- Editor errors and authoritative Workbench compiler results shown separately.
- Range formatting plus experimental automatic formatting while typing,
  including indentation, comment pairs, and preprocessor separators.
- Semantic, punctuation-colored, or native VS Code bracket presentation.
- Automatic installed add-on discovery and PAC-backed base-game indexing.
- Automatic and manual script validation through the Workbench NET API.
- Script and resource search with `Ctrl+Alt+F`.
- Bundled MCP server for script, resource, add-on, and official documentation
  search, plus Workbench inspection and editing.

The extension recognizes `.c` files under `Scripts` or `scripts` directories as
Enfusion Script.

## Platform Support

Windows and Linux are both supported, and everything the extension does with the
Enfusion Script language itself — syntax, semantic coloring, completion, hover,
go to definition, symbols, formatting, indexing, search, and the MCP server —
works the same on either.

Workbench is a Windows application. On Windows it runs natively. On Linux it runs
inside a Wine prefix, and the extension resolves that prefix automatically from
Steam's Proton compatibility data for the Arma Reforger Tools app, or from
`WINEPREFIX`. Set **Workbench: Wine Prefix** if you keep Workbench in a prefix
the extension cannot find. Paths are translated in both directions, so compiler
findings open the right files and add-on sources are indexed from the right
places. Launching, stopping, and restarting Workbench go through Steam for a
Proton prefix and through `wine` for a prefix you maintain, and `enfusion://`
links opened from the Search UI are registered with your desktop so they reach
Workbench.

Two Workbench-related things stay Windows-only, because their hosts expose no
route to them under Wine: window capture (`workbench_capture_window` and
`workbench_list_windows`), and the suggest-widget UI Automation diagnostic
report. Everything else, including compiler validation, is available on both.

If Workbench is not installed at all, the language and indexing features still
work; only the Workbench integration reports itself unavailable.

## Workbench Integration

Workbench integration is disabled by default. On first activation, the
extension asks whether it may enable the integration and install its managed
bridge. Approval enables Workbench's local NET API, registers the per-user
`enfusion://` handler, installs the bridge, and remembers the approval for
future bridge updates. If Workbench is open, restart it when prompted. If it is
closed, setup completes without launching it. Declining keeps the integration
disabled while the other language and indexing features continue normally.

The extension reconnects automatically. The Workbench status item shows
availability, and **Reforger Script Tools: Validate Scripts in Workbench** runs
validation manually. Workbench validation is also requested at session start
and after an eligible save. By default, it also saves the active dirty script
and validates after three seconds without typing; disable **Workbench NET API:
Save and Validate On Idle** to use only explicit saves and manual validation.

These steps follow Bohemia Interactive's official
[Resource Manager options documentation](https://community.bistudio.com/wiki/Arma_Reforger%3AResource_Manager%3A_Options#Enable_net_API).


## MCP Server

Work-in progress framework. Not fully complete but commands and API are currently exposed. See github documentation on usage.


## Settings

Open **Preferences: Open Settings (UI)** and search for `Reforger Script
Tools`, or add the keys to `settings.json`.

| Setting | Default | Description |
| --- | --- | --- |
| `reforgerScriptTools.diagnostics.enabled` | `false` | Write detailed local support logs after the VS Code window is reloaded. Enable only while investigating a problem. |
| `reforgerScriptTools.experimentalAutoFormatting` | `true` | Apply experimental automatic source edits, including typing assists and preprocessor directive separators. |
| `reforgerScriptTools.bracketColoring` | `"semantic"` | Use `"semantic"` owner colors, `"punctuation"` palette color, or native `"vscode"` bracket coloring and matching. This setting applies across VS Code windows. |
| `reforgerScriptTools.workbench.enabled` | `false` | Enable Workbench NET API status checks, compiler validation, and the consent-gated managed bridge. |
| `reforgerScriptTools.workbench.host` | `"127.0.0.1"` | Workbench NET API loopback host. IPv4 loopback addresses and `::1` are accepted. |
| `reforgerScriptTools.workbench.port` | `5775` | Workbench NET API port, from `1` through `65535`. The extension does not scan other ports. |
| `reforgerScriptTools.workbench.saveOnIdle` | `true` | After three seconds without typing, save the active Enforce Script and validate in Workbench. Disable to validate only on explicit save or command. |
| `reforgerScriptTools.workbench.winePrefix` | `""` | Absolute path to the Wine prefix that runs Workbench on a host that does not run it natively. Leave empty to resolve Steam's Proton compatibility data for the Arma Reforger Tools app, or `WINEPREFIX`, automatically. Ignored on Windows. |
| `reforgerScriptTools.workbench.externalIndexMode` | `"loaded"` | Choose cached external indexes: `"loaded"` for the opened project's dependencies, `"all"` for every compatible cached index, or `"none"` for workspace scripts only. |

For the default `"semantic"` bracket mode, the extension contributes and
maintains these language-specific user settings when Enforce support activates:

```json
"[enforce]": {
  "editor.bracketPairColorization.enabled": false,
  "editor.matchBrackets": "never"
}
```

The `"punctuation"` mode uses the same editor settings. The `"vscode"` mode
sets them to `true` and `"always"` so native bracket coloring and matching take
over.

## Customize Semantic Colors

The previous custom color theme has been removed. The extension now supplies
default Enfusion Script colors through VS Code's native semantic-token settings
without replacing your selected theme. VS Code applies these defaults
automatically, so they do not appear in **User Settings (JSON)** until you add
your own overrides.

To change a color:

1. Open the Command Palette with `Ctrl+Shift+P`.
2. Run **Preferences: Open User Settings (JSON)**.
3. Add the selectors you want to override under
   `editor.semanticTokenColorCustomizations.rules`.

For example:

```json
{
  "editor.semanticTokenColorCustomizations": {
    "rules": {
      "class:enforce": "#4EC9B0",
      "function:enforce": "#DCDCAA",
      "reforgerField:enforce": "#9CDCFE",
      "keyword:enforce": "#569CD6",
      "comment:enforce": "#6A9955",
      "string:enforce": "#CE9178",
      "reforgerPunctuation:enforce": "#D4D4D4"
    }
  }
}
```

Only the selectors included in the user's settings are changed; all others keep
the extension defaults. The `:enforce` suffix limits each rule to Enfusion
Script. Available selectors are:

`class:enforce`, `enum:enforce`, `type:enforce`, `typeParameter:enforce`,
`function:enforce`, `reforgerField:enforce`, `variable:enforce`,
`parameter:enforce`, `enumMember:enforce`, `number:enforce`,
`operator:enforce`, `reforgerPunctuation:enforce`, `keyword:enforce`,
`comment:enforce`, `string:enforce`, and `reforgerPreprocessor:enforce`.
