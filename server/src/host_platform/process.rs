//! Workbench process inspection and control.
//!
//! A process is addressed by its id together with its start time, so a reused
//! id can never be mistaken for the process that was observed. Every operation
//! re-checks that identity immediately before it acts.

use serde::Deserialize;
use std::io;

use super::WORKBENCH_EXECUTABLE_NAME;

/// A running process, identified so that a reused id cannot be mistaken for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub id: u32,
    pub start_ticks: u64,
}

/// How a Workbench process is asked to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseMode {
    /// Ask Workbench to close its main window and shut down on its own.
    Graceful,
    /// End the process immediately.
    Force,
}

/// Every Workbench process running on this host.
pub fn workbench_processes() -> Vec<ProcessIdentity> {
    platform::workbench_processes()
}

/// Every running Workbench process id.
pub fn workbench_process_ids() -> Vec<u32> {
    workbench_processes()
        .into_iter()
        .map(|process| process.id)
        .collect()
}

/// The command line of a Workbench process, in the argument form Workbench was
/// started with. Returns `None` unless the identity still matches.
pub fn command_line(process: ProcessIdentity) -> Option<Vec<String>> {
    platform::command_line(process)
}

/// The titles of the visible windows a Workbench process owns.
///
/// Returns `None` where the host has no supported route to window titles; on
/// those hosts the command line is the only source of the open project.
pub fn window_titles(process: ProcessIdentity) -> Option<Vec<String>> {
    platform::window_titles(process)
}

/// Ends a Workbench process, re-checking its identity first.
pub fn close(process: ProcessIdentity, mode: CloseMode) -> io::Result<()> {
    platform::close(process, mode)
}

/// Every process id on this host.
#[cfg(unix)]
pub(crate) fn process_ids() -> Vec<u32> {
    platform::process_ids()
}

/// The executable name of a process, as the kernel records it.
#[cfg(unix)]
pub(crate) fn process_name(id: u32) -> Option<String> {
    platform::process_name(id)
}

/// One environment variable a process was started with.
#[cfg(unix)]
pub(crate) fn environment_value(id: u32, name: &str) -> Option<String> {
    platform::environment_value(id, name)
}

/// Whether a command line is Workbench's own.
///
/// Only the image the process was started as counts. A Wine host runs Workbench
/// behind a chain of launchers — Steam's reaper, the runtime's container tools,
/// the Proton script, and Wine's own `steam.exe` — and every one of them
/// carries the Workbench path further along its command line.
fn is_workbench_command_line(arguments: &[String]) -> bool {
    arguments.first().is_some_and(|image| {
        image
            .trim_matches('"')
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(WORKBENCH_EXECUTABLE_NAME))
    })
}

#[cfg(unix)]
mod platform {
    use super::{is_workbench_command_line, CloseMode, ProcessIdentity};
    use std::fs;
    use std::io;
    use std::path::PathBuf;

    pub(super) fn workbench_processes() -> Vec<ProcessIdentity> {
        let mut processes = process_ids()
            .into_iter()
            .filter(|id| read_command_line(*id).is_some_and(|a| is_workbench_command_line(&a)))
            .filter_map(|id| start_ticks(id).map(|start_ticks| ProcessIdentity { id, start_ticks }))
            .collect::<Vec<_>>();
        processes.sort_by_key(|process| process.id);
        processes
    }

    pub(super) fn command_line(process: ProcessIdentity) -> Option<Vec<String>> {
        let arguments = read_command_line(process.id)?;
        (start_ticks(process.id) == Some(process.start_ticks)).then_some(arguments)
    }

    pub(super) fn window_titles(_process: ProcessIdentity) -> Option<Vec<String>> {
        // Wine window titles belong to the host window manager, which has no
        // route this server can depend on. On this host the command line is the
        // source of the open project.
        None
    }

    pub(super) fn close(process: ProcessIdentity, mode: CloseMode) -> io::Result<()> {
        if start_ticks(process.id) != Some(process.start_ticks) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the observed Workbench process is no longer running",
            ));
        }
        let signal = match mode {
            // Wine turns a termination request into the shutdown path a closed
            // main window takes; the caller has already confirmed the save that
            // makes ending the process safe.
            CloseMode::Graceful => libc::SIGTERM,
            CloseMode::Force => libc::SIGKILL,
        };
        // SAFETY: `kill` takes plain integers and has no memory effects. The
        // identity check above establishes that the id is the observed process.
        let delivered = unsafe { libc::kill(process.id as libc::pid_t, signal) };
        if delivered == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn process_ids() -> Vec<u32> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .collect()
    }

    pub(super) fn process_name(id: u32) -> Option<String> {
        let stat = read_process_file(id, "stat")?;
        let stat = String::from_utf8(stat).ok()?;
        Some(stat[stat.find('(')? + 1..stat.rfind(')')?].to_string())
    }

    pub(super) fn environment_value(id: u32, name: &str) -> Option<String> {
        let prefix = format!("{name}=");
        read_process_file(id, "environ")?
            .split(|byte| *byte == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok())
            .find_map(|entry| entry.strip_prefix(&prefix).map(str::to_string))
    }

    fn read_process_file(id: u32, name: &str) -> Option<Vec<u8>> {
        fs::read(PathBuf::from("/proc").join(id.to_string()).join(name)).ok()
    }

    fn read_command_line(id: u32) -> Option<Vec<String>> {
        let arguments = read_process_file(id, "cmdline")?
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect::<Vec<_>>();
        (!arguments.is_empty()).then_some(arguments)
    }

    /// The process start time in clock ticks since boot, from field 22 of
    /// `/proc/<id>/stat`. The executable name is parenthesized and may itself
    /// contain spaces or parentheses, so the numbered fields are read after the
    /// last closing parenthesis.
    fn start_ticks(id: u32) -> Option<u64> {
        let stat = String::from_utf8(read_process_file(id, "stat")?).ok()?;
        stat[stat.rfind(')')? + 1..]
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }
}

#[cfg(windows)]
mod platform {
    use super::super::WORKBENCH_PROCESS_NAME;
    use super::{CloseMode, ProcessIdentity};
    use std::io;

    pub(super) fn workbench_processes() -> Vec<ProcessIdentity> {
        let script = format!(
            "$items=@(Get-Process -Name {WORKBENCH_PROCESS_NAME} -ErrorAction SilentlyContinue | \
             ForEach-Object {{ [pscustomobject]@{{ id=[uint32]$_.Id; \
             startTicks=[uint64]$_.StartTime.ToUniversalTime().Ticks }} }}); \
             ConvertTo-Json -Compress -InputObject $items"
        );
        let Some(output) = powershell(&script) else {
            return Vec::new();
        };
        serde_json::from_slice(&output).unwrap_or_else(|_| {
            serde_json::from_slice::<ProcessIdentity>(&output)
                .map(|process| vec![process])
                .unwrap_or_default()
        })
    }

    pub(super) fn command_line(process: ProcessIdentity) -> Option<Vec<String>> {
        let script = format!(
            "{}\n(Get-CimInstance Win32_Process -Filter 'ProcessId = {}' \
             -ErrorAction Stop).CommandLine",
            identity_guard(process),
            process.id,
        );
        let command_line = String::from_utf8(powershell(&script)?).ok()?;
        Some(split_windows_command_line(command_line.trim()))
    }

    pub(super) fn window_titles(process: ProcessIdentity) -> Option<Vec<String>> {
        let script = format!(
            r#"
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class RSTWorkbenchWindows {{
 public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
 [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int maxCount);
}}
'@
{guard}
$titles = [System.Collections.Generic.List[string]]::new()
$callback = [RSTWorkbenchWindows+EnumWindowsProc] {{ param([IntPtr]$hWnd, [IntPtr]$unused)
 [uint32]$owner = 0; [void][RSTWorkbenchWindows]::GetWindowThreadProcessId($hWnd, [ref]$owner)
 if ($owner -eq $p.Id -and [RSTWorkbenchWindows]::IsWindowVisible($hWnd)) {{
  $title = [System.Text.StringBuilder]::new(512); [void][RSTWorkbenchWindows]::GetWindowText($hWnd, $title, $title.Capacity)
  $value = $title.ToString(); if ($value) {{ $titles.Add($value) }}
 }}
 return $true
}}
[void][RSTWorkbenchWindows]::EnumWindows($callback, [IntPtr]::Zero)
$titles
"#,
            guard = identity_guard(process),
        );
        let titles = String::from_utf8(powershell(&script)?).ok()?;
        Some(
            titles
                .lines()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }

    pub(super) fn close(process: ProcessIdentity, mode: CloseMode) -> io::Result<()> {
        let action = match mode {
            CloseMode::Graceful => "[void]$p.CloseMainWindow()",
            CloseMode::Force => "Stop-Process -Id $p.Id -Force",
        };
        let script = format!("{}\n{action}", identity_guard(process));
        let status = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "the Workbench close request exited with {:?}",
                status.code(),
            )))
        }
    }

    /// Binds `$p` to the observed process and exits before any action when the
    /// identity no longer matches.
    fn identity_guard(process: ProcessIdentity) -> String {
        format!(
            "$p=Get-Process -Id {} -ErrorAction Stop; \
             if ($p.ProcessName -ne '{WORKBENCH_PROCESS_NAME}' -or \
                 [uint64]$p.StartTime.ToUniversalTime().Ticks -ne [uint64]{}) {{ exit 2 }}",
            process.id, process.start_ticks,
        )
    }

    fn powershell(script: &str) -> Option<Vec<u8>> {
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }

    /// Splits a Windows command line into the arguments the process received.
    fn split_windows_command_line(command_line: &str) -> Vec<String> {
        let mut arguments = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        let mut started = false;
        for character in command_line.chars() {
            match character {
                '"' => {
                    quoted = !quoted;
                    started = true;
                }
                character if character.is_whitespace() && !quoted => {
                    if started {
                        arguments.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                character => {
                    current.push(character);
                    started = true;
                }
            }
        }
        if started {
            arguments.push(current);
        }
        arguments
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::{CloseMode, ProcessIdentity};
    use std::io;

    pub(super) fn workbench_processes() -> Vec<ProcessIdentity> {
        Vec::new()
    }

    pub(super) fn command_line(_process: ProcessIdentity) -> Option<Vec<String>> {
        None
    }

    pub(super) fn window_titles(_process: ProcessIdentity) -> Option<Vec<String>> {
        None
    }

    pub(super) fn close(_process: ProcessIdentity, _mode: CloseMode) -> io::Result<()> {
        Err(super::super::unsupported("Workbench process control"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_line(arguments: &[&str]) -> Vec<String> {
        arguments.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn workbench_is_recognized_by_its_own_image_in_either_path_space() {
        assert!(is_workbench_command_line(&command_line(&[
            r"S:\common\Arma Reforger Tools\Workbench\ArmaReforgerWorkbenchSteamDiag.exe"
        ])));
        assert!(is_workbench_command_line(&command_line(&[
            "/games/Arma Reforger Tools/Workbench/ArmaReforgerWorkbenchSteamDiag.exe",
            "-gproj",
            "S:/Addon/addon.gproj",
        ])));
        assert!(is_workbench_command_line(&command_line(&[
            "\"C:/Tools/ArmaReforgerWorkbenchSteamDiag.exe\""
        ])));
    }

    /// A Wine host runs Workbench behind several launchers, each naming the
    /// Workbench executable somewhere in its own command line.
    #[test]
    fn the_launchers_that_carry_the_workbench_path_are_not_workbench() {
        let workbench = "/Steam/steamapps/common/Arma Reforger Tools/Workbench/ArmaReforgerWorkbenchSteamDiag.exe";
        for launcher in [
            command_line(&[
                "/Steam/ubuntu12_32/reaper",
                "SteamLaunch",
                "AppId=1874910",
                "--",
                workbench,
            ]),
            command_line(&[
                "python3",
                "/compatibilitytools.d/proton",
                "waitforexitandrun",
                workbench,
            ]),
            command_line(&[r"c:\windows\system32\steam.exe", workbench]),
            command_line(&["/usr/bin/wine", workbench]),
        ] {
            assert!(
                !is_workbench_command_line(&launcher),
                "{launcher:?} starts Workbench but is not Workbench",
            );
        }
        assert!(!is_workbench_command_line(&command_line(&[
            "ArmaReforgerSteam.exe"
        ])));
        assert!(!is_workbench_command_line(&[]));
    }

    #[cfg(unix)]
    #[test]
    fn this_process_is_not_mistaken_for_workbench() {
        let processes = workbench_processes();
        assert!(processes
            .iter()
            .all(|process| process.id != std::process::id()));
    }
}
