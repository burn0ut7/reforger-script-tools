# Host Platform

Workbench is a Windows application. On Windows it runs natively. On Linux it
runs inside a Wine prefix — Steam's Proton compatibility data for the Arma
Reforger Tools app, or a prefix the user maintains. The language features do not
depend on either: lexing, parsing, indexing, diagnostics, formatting, and the
LSP and MCP surfaces are host-independent and available wherever the packaged
server runs.

Everything that is not host-independent belongs to `server/src/host_platform.rs`
and its submodules. It owns one resolution of where the host keeps Steam, which
prefix runs Workbench, how the two path spaces relate, the registry Workbench
reads its options from, Workbench process identity, the route that starts
Workbench, and desktop URL-scheme registration. No other module reads
`USERPROFILE`, the Windows registry, `/proc`, `dosdevices`, or a Steam
installation root.

## The resolved host

`WorkbenchHost` is that resolution:

| Variant | Meaning |
| --- | --- |
| `Native` | Workbench runs natively. A host path is a Workbench path. |
| `Wine(WinePrefix)` | Workbench runs inside a prefix. The two path spaces differ. |
| `Unavailable` | No prefix was resolved. Language features continue; Workbench operations report the host. |

On Windows the host is always `Native`. Elsewhere the prefix is resolved in one
ordered pass, and the first available answer wins:

1. `reforgerScriptTools.workbench.winePrefix`, passed to every server process as
   `--workbench-wine-prefix`.
2. The `WINEPREFIX` environment variable.
3. Steam's compatibility data for app `1874910`, when exactly one library holds
   it. An ambiguous installation stays unavailable rather than being guessed.

The host is settled once per process, before any host-dependent operation runs.
`WorkbenchController` and `WorkbenchGateway` each hold the host they resolved,
so every path one of them exchanges with Workbench uses one mapping.

## Path spaces

Workbench reports its own paths — the loaded add-on graph's source roots and
current project, the compiler's absolute diagnostic files, and the project
registry's `FilePath` entries. The extension reads and writes host paths. The two
are translated only at the Workbench boundary:

- `to_host_path` maps a path Workbench reported to the host path that reads it.
- `to_workbench_path` maps a host path to the address Workbench uses for it.

Under Wine the mapping comes from the prefix's `dosdevices` links, completed
with the `C:` and `Z:` mapping every prefix has. The deepest matching drive
wins, so a file inside a Steam library that the prefix maps as its own drive
resolves through that drive rather than through the host root at `Z:`. A path
Workbench cannot address — a drive-relative path, a path escaping its drive, or
a host path outside every mapping — leaves the operation unavailable rather than
producing a path Workbench would reject.

## Workbench's registry

Workbench reads `NetAPI_Enabled` and the `enfusion` URL protocol from
`HKEY_CURRENT_USER`. On Windows that is the real registry. Under Wine it is the
prefix's `user.reg` text hive.

A wineserver loads the hive when it starts and rewrites it from memory when it
shuts down, so an edit made from outside is only durable while no wineserver
holds the prefix. A write to a prefix in use is refused, with a busy result
telling the user to close Workbench, rather than being made and silently
discarded. This matches the existing setup flow, which already asks the user to
restart Workbench after the bridge is installed.

## Processes and launching

Workbench is addressed by process id together with its start time, so a reused
id can never be mistaken for the process that was observed. Every operation
re-checks that identity immediately before acting.

A Wine host starts Workbench behind a chain of launchers — Steam's reaper, the
runtime container tools, the Proton script, and Wine's own `steam.exe` — and
every one of them carries the Workbench path somewhere in its command line. Only
the process whose own image is the Workbench executable is Workbench.

The launch route follows whoever owns the prefix. Steam owns the Proton runtime
behind compatibility data, so that prefix is started with
`steam -applaunch 1874910`; a prefix the user owns is started through the `wine`
that owns it. Because both hand off to a process the launcher spawns, the
started Workbench is resolved by observing the new Workbench process rather than
by the child that was spawned.

Window enumeration has no route this server can depend on under Wine, so the
window title is unavailable there. Restart resolves the open project from
Workbench itself — the loaded add-on graph's current project file — which
answers on every host; the command line and window title remain for a running
Workbench whose bridge cannot answer.

## Opening `enfusion` links

Workbench resolves an `enfusion` link through its own registry. A link followed
outside the prefix — from the Search UI, a browser, or a chat client — is
resolved by the host desktop instead. On a Wine host the consented bootstrap
also writes a desktop entry declaring `x-scheme-handler/enfusion` and names it
for that scheme in the user's own `mimeapps.list`. The entry starts Workbench
through the same route a launch uses, so both reach the same Workbench. Both
writes stay inside the user's own configuration.

## Where host facts are verified

Host resolution and translation are tested against explicit hosts rather than
against the developer's machine: unit tests elsewhere resolve to `Native`, and
the Wine mapping, prefix registry, desktop registration, and launcher
discrimination are covered directly in `host_platform`. A change to Workbench
process control, launching, or registry writing still needs live acceptance on
the host it targets.
