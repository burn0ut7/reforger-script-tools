//! Host platform facts for Workbench integration.
//!
//! Workbench is a Windows application. On Windows it runs natively, and a host
//! path is a Workbench path. On Linux it runs inside a Wine prefix — Steam's
//! Proton compatibility data for the Arma Reforger Tools app, or a prefix the
//! user points the extension at — and the two path spaces are different.
//!
//! This module owns the single resolution of where the host keeps Steam, which
//! prefix hosts Workbench, how the two path spaces map onto each other, and the
//! prefix registry hive Workbench reads its options from. The rest of the
//! server works in host paths and converts only at the Workbench boundary.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Steam app id of the Arma Reforger base game.
pub const REFORGER_GAME_APP_ID: &str = "1874880";
/// Steam app id of the Arma Reforger Tools installation that owns Workbench.
pub const REFORGER_TOOLS_APP_ID: &str = "1874910";

/// The Windows executable Workbench is started from, on every host.
pub const WORKBENCH_EXECUTABLE_NAME: &str = "ArmaReforgerWorkbenchSteamDiag.exe";
/// The same executable as the host reports its running process.
pub const WORKBENCH_PROCESS_NAME: &str = "ArmaReforgerWorkbenchSteamDiag";

static WORKBENCH_HOST: OnceLock<WorkbenchHost> = OnceLock::new();

/// Records the explicit Wine prefix before the first host-dependent operation.
///
/// The composition root calls this once while parsing its arguments. A later
/// call, or a call after the host has already been resolved, is ignored so that
/// every caller in a process observes the same host.
pub fn configure_workbench_host(explicit_wine_prefix: Option<&Path>) {
    let _ = WORKBENCH_HOST.set(WorkbenchHost::detect(explicit_wine_prefix));
}

/// The host that runs Workbench for this process.
pub fn workbench_host() -> &'static WorkbenchHost {
    WORKBENCH_HOST.get_or_init(|| {
        // Unit tests exercise host-independent behavior in the native path
        // space; host resolution and translation are tested against explicit
        // hosts in this module instead of against the developer's machine.
        #[cfg(test)]
        {
            WorkbenchHost::Native
        }
        #[cfg(not(test))]
        {
            WorkbenchHost::detect(None)
        }
    })
}

/// How Workbench runs on this host, and therefore how host paths and Workbench
/// paths relate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkbenchHost {
    /// Workbench runs natively. A host path is a Workbench path.
    Native,
    /// Workbench runs inside a Wine prefix. The two path spaces differ and are
    /// translated through the prefix's drive mapping.
    Wine(WinePrefix),
    /// This platform can host Workbench but no prefix was resolved. Language
    /// features remain available; Workbench operations are unavailable.
    Unavailable,
}

impl WorkbenchHost {
    /// Resolves the host without consulting the process-wide value. Prefer
    /// [`workbench_host`]; this exists for the composition root and for tests.
    pub fn detect(explicit_wine_prefix: Option<&Path>) -> Self {
        if cfg!(windows) {
            return Self::Native;
        }
        if let Some(prefix) =
            explicit_wine_prefix.and_then(|root| WinePrefix::open(root, WinePrefixSource::Explicit))
        {
            return Self::Wine(prefix);
        }
        if let Some(prefix) = std::env::var_os("WINEPREFIX")
            .map(PathBuf::from)
            .and_then(|root| WinePrefix::open(&root, WinePrefixSource::Environment))
        {
            return Self::Wine(prefix);
        }
        match steam_compatibility_prefix(REFORGER_TOOLS_APP_ID) {
            Some(root) => WinePrefix::open(&root, WinePrefixSource::SteamCompatibilityData)
                .map_or(Self::Unavailable, Self::Wine),
            None => Self::Unavailable,
        }
    }

    /// Stable identifier for how this host was resolved, reported alongside the
    /// resolved Workbench paths.
    pub fn source(&self) -> &'static str {
        match self {
            Self::Native => "windows-native",
            Self::Wine(prefix) => prefix.source().label(),
            Self::Unavailable => "wine-prefix-unavailable",
        }
    }

    /// The Wine prefix hosting Workbench, when Workbench is hosted by Wine.
    pub fn wine_prefix(&self) -> Option<&WinePrefix> {
        match self {
            Self::Wine(prefix) => Some(prefix),
            Self::Native | Self::Unavailable => None,
        }
    }

    /// The host directory holding the Windows user profile Workbench writes to.
    pub fn user_directory(&self) -> Option<PathBuf> {
        match self {
            Self::Native => std::env::var_os("USERPROFILE").map(PathBuf::from),
            Self::Wine(prefix) => prefix.user_directory(),
            Self::Unavailable => None,
        }
    }

    /// Translates a path Workbench reported into the host path that reads it.
    pub fn to_host_path(&self, workbench_path: &str) -> Option<PathBuf> {
        match self {
            Self::Native => {
                let path = PathBuf::from(workbench_path);
                path.is_absolute().then_some(path)
            }
            Self::Wine(prefix) => prefix.to_host_path(workbench_path),
            Self::Unavailable => None,
        }
    }

    /// Translates a host path into the path Workbench addresses it by.
    pub fn to_workbench_path(&self, host_path: &Path) -> Option<OsString> {
        match self {
            Self::Native => host_path
                .is_absolute()
                .then(|| host_path.as_os_str().to_os_string()),
            Self::Wine(prefix) => prefix.to_workbench_path(host_path).map(OsString::from),
            Self::Unavailable => None,
        }
    }
}

/// Where the Wine prefix hosting Workbench came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinePrefixSource {
    /// The extension setting or the matching command-line option.
    Explicit,
    /// The `WINEPREFIX` environment variable.
    Environment,
    /// Steam's compatibility data for the Arma Reforger Tools app.
    SteamCompatibilityData,
}

impl WinePrefixSource {
    fn label(self) -> &'static str {
        match self {
            Self::Explicit => "wine-prefix-explicit",
            Self::Environment => "wine-prefix-environment",
            Self::SteamCompatibilityData => "wine-prefix-steam-compatibility-data",
        }
    }
}

/// A Wine prefix and the drive mapping that relates its Windows path space to
/// the host filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinePrefix {
    root: PathBuf,
    source: WinePrefixSource,
    drives: Vec<(char, PathBuf)>,
}

impl WinePrefix {
    /// Opens an initialized prefix. A directory without a Windows drive is not
    /// a prefix and is reported as unavailable rather than repaired.
    pub fn open(root: &Path, source: WinePrefixSource) -> Option<Self> {
        let root = canonicalize_lenient(root);
        if !root.join("drive_c").is_dir() {
            return None;
        }
        Some(Self {
            drives: read_drive_mapping(&root),
            root,
            source,
        })
    }

    /// Builds a prefix from an explicit drive mapping, for tests.
    #[cfg(test)]
    pub(crate) fn from_drives(
        root: PathBuf,
        source: WinePrefixSource,
        drives: Vec<(char, PathBuf)>,
    ) -> Self {
        Self {
            root,
            source,
            drives,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn source(&self) -> WinePrefixSource {
        self.source
    }

    /// The prefix registry hive holding `HKEY_CURRENT_USER`.
    pub fn user_registry_path(&self) -> PathBuf {
        self.root.join("user.reg")
    }

    /// The host directory holding the Windows user profile inside the prefix.
    ///
    /// Proton always creates `steamuser`; a prefix made by Wine directly uses
    /// the host login name. A prefix with exactly one other user directory
    /// resolves to it; anything ambiguous stays unavailable.
    pub fn user_directory(&self) -> Option<PathBuf> {
        let users = self.drive('c')?.join("users");
        let named = ["steamuser".to_string()]
            .into_iter()
            .chain(std::env::var("USER"))
            .chain(std::env::var("LOGNAME"))
            .map(|name| users.join(name))
            .find(|directory| directory.is_dir());
        if named.is_some() {
            return named;
        }
        let mut discovered = fs::read_dir(&users)
            .ok()?
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !name.eq_ignore_ascii_case("Public"))
            })
            .collect::<Vec<_>>();
        (discovered.len() == 1).then(|| discovered.remove(0))
    }

    /// Translates a Windows path reported by Workbench into a host path.
    pub fn to_host_path(&self, workbench_path: &str) -> Option<PathBuf> {
        let value = workbench_path.trim();
        let value = value.strip_prefix(r"\\?\").unwrap_or(value);
        let mut characters = value.chars();
        let letter = characters.next()?.to_ascii_lowercase();
        if !letter.is_ascii_alphabetic() || characters.next()? != ':' {
            return None;
        }
        let remainder = &value[2..];
        if !remainder.is_empty() && !remainder.starts_with(['/', '\\']) {
            // A drive-relative path depends on Workbench's own working
            // directory and has no host meaning.
            return None;
        }
        let mut host = self.drive(letter)?.to_path_buf();
        for segment in remainder
            .split(['/', '\\'])
            .filter(|segment| !segment.is_empty() && *segment != ".")
        {
            if segment == ".." {
                return None;
            }
            host.push(segment);
        }
        Some(host)
    }

    /// Translates a host path into the Windows path Workbench addresses it by.
    ///
    /// The deepest matching drive wins, so a path inside the prefix resolves to
    /// `C:` rather than to the `Z:` mapping of the host root.
    pub fn to_workbench_path(&self, host_path: &Path) -> Option<String> {
        let host_path = canonicalize_lenient(host_path);
        let (letter, target) = self
            .drives
            .iter()
            .filter(|(_, target)| host_path.starts_with(target))
            .max_by_key(|(_, target)| target.as_os_str().len())?;
        let mut value = String::from(letter.to_ascii_uppercase());
        value.push(':');
        for component in host_path.strip_prefix(target).ok()?.components() {
            let Component::Normal(segment) = component else {
                return None;
            };
            value.push('\\');
            value.push_str(segment.to_str()?);
        }
        if value.len() == 2 {
            value.push('\\');
        }
        Some(value)
    }

    fn drive(&self, letter: char) -> Option<&Path> {
        self.drives
            .iter()
            .find(|(drive, _)| *drive == letter)
            .map(|(_, target)| target.as_path())
    }
}

/// Reads a prefix's `dosdevices` drive mapping, completing it with the layout
/// every prefix has even when the links are missing.
fn read_drive_mapping(root: &Path) -> Vec<(char, PathBuf)> {
    let mut drives: Vec<(char, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join("dosdevices")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let mut characters = name.chars();
            let (Some(letter), Some(':'), None) =
                (characters.next(), characters.next(), characters.next())
            else {
                continue;
            };
            if !letter.is_ascii_alphabetic() {
                continue;
            }
            let Ok(target) = fs::canonicalize(entry.path()) else {
                continue;
            };
            if target.is_dir() {
                drives.push((letter.to_ascii_lowercase(), target));
            }
        }
    }
    for (letter, target) in [('c', root.join("drive_c")), ('z', PathBuf::from("/"))] {
        if !drives.iter().any(|(drive, _)| *drive == letter) && target.is_dir() {
            drives.push((letter, canonicalize_lenient(&target)));
        }
    }
    drives.sort_by_key(|(letter, _)| *letter);
    drives.dedup_by_key(|(letter, _)| *letter);
    drives
}

/// Resolves a path to its real location as far as the filesystem allows,
/// keeping the trailing segments that do not exist yet.
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    let mut trailing = Vec::new();
    let mut current = path;
    while let (Some(parent), Some(name)) = (current.parent(), current.file_name()) {
        trailing.push(name.to_os_string());
        if let Ok(canonical) = fs::canonicalize(parent) {
            let mut resolved = canonical;
            resolved.extend(trailing.iter().rev());
            return resolved;
        }
        current = parent;
    }
    path.to_path_buf()
}

/// The host directory the extension writes its Workbench support log to.
pub fn support_log_directory() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("USERPROFILE").map(|user| {
            PathBuf::from(user)
                .join("AppData")
                .join("Local")
                .join("ReforgerScriptTools")
                .join("logs")
        });
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|state| state.is_absolute())
        .or_else(|| home_directory().map(|home| home.join(".local").join("state")))
        .map(|state| state.join("reforger-script-tools").join("logs"))
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

// ---------------------------------------------------------------------------
// Steam
// ---------------------------------------------------------------------------

/// Every Steam installation root registered on this host.
pub fn steam_roots() -> Vec<PathBuf> {
    let mut roots = platform_steam_roots();
    roots.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
    roots.dedup();
    roots
}

#[cfg(windows)]
fn platform_steam_roots() -> Vec<PathBuf> {
    windows_registry::steam_roots()
}

#[cfg(not(windows))]
fn platform_steam_roots() -> Vec<PathBuf> {
    // Steam's own layout, the Debian package layout, and the Flatpak layout.
    // Each is a real installation root rather than a guessed directory: a root
    // only counts when it holds the `steamapps` directory Steam writes.
    let Some(home) = home_directory() else {
        return Vec::new();
    };
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local").join("share"));
    [
        home.join(".steam").join("steam"),
        home.join(".steam").join("root"),
        home.join(".steam").join("debian-installation"),
        data_home.join("Steam"),
        home.join(".var")
            .join("app")
            .join("com.valvesoftware.Steam")
            .join("data")
            .join("Steam"),
    ]
    .into_iter()
    .filter(|root| root.join("steamapps").is_dir())
    .map(|root| canonicalize_lenient(&root))
    .collect()
}

/// Every Steam library folder reachable from a Steam installation root.
pub fn steam_libraries(steam_root: &Path) -> Vec<PathBuf> {
    let mut libraries = vec![steam_root.to_path_buf()];
    if let Ok(vdf) = fs::read_to_string(steam_root.join("steamapps").join("libraryfolders.vdf")) {
        for line in vdf.lines() {
            let values = vdf_values(line);
            if values.first().is_some_and(|value| *value == "path") {
                if let Some(value) = values.get(1) {
                    libraries.push(PathBuf::from(value.replace("\\\\", "\\")));
                }
            }
        }
    }
    libraries
}

/// Reads one string field from a Steam `.acf` application manifest.
pub fn acf_string(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let values = vdf_values(line);
        (values.first().is_some_and(|value| *value == key))
            .then(|| values.get(1).map(|value| (*value).to_string()))
            .flatten()
    })
}

fn vdf_values(line: &str) -> Vec<&str> {
    line.split('"')
        .enumerate()
        .filter_map(|(index, value)| (index % 2 == 1).then_some(value))
        .collect()
}

/// The Proton compatibility prefix Steam keeps for an app, when exactly one
/// library holds it. An ambiguous installation stays unavailable.
pub fn steam_compatibility_prefix(app_id: &str) -> Option<PathBuf> {
    let mut prefixes = steam_roots()
        .iter()
        .flat_map(|root| steam_libraries(root))
        .map(|library| {
            library
                .join("steamapps")
                .join("compatdata")
                .join(app_id)
                .join("pfx")
        })
        .filter(|prefix| prefix.join("drive_c").is_dir())
        .map(|prefix| canonicalize_lenient(&prefix))
        .collect::<Vec<_>>();
    prefixes.sort();
    prefixes.dedup();
    (prefixes.len() == 1).then(|| prefixes.remove(0))
}

// ---------------------------------------------------------------------------
// Windows registry
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub mod windows_registry;

// ---------------------------------------------------------------------------
// Wine registry
// ---------------------------------------------------------------------------

pub mod wine_registry;

// ---------------------------------------------------------------------------
// Process inspection
// ---------------------------------------------------------------------------

pub mod process;

// ---------------------------------------------------------------------------
// Desktop URL schemes
// ---------------------------------------------------------------------------

pub mod url_scheme;

// ---------------------------------------------------------------------------
// Launching
// ---------------------------------------------------------------------------

/// How this host starts the Windows Workbench executable: the program to run,
/// the arguments and environment that reach Workbench before its own, and the
/// route they describe.
///
/// Both a launch and a desktop URL handler are rendered from this one value, so
/// a link followed outside the prefix starts Workbench exactly as the extension
/// does.
pub struct WorkbenchLaunchPrelude {
    pub program: PathBuf,
    pub leading_arguments: Vec<OsString>,
    pub environment: Vec<(String, OsString)>,
    pub working_directory: Option<PathBuf>,
    pub source: &'static str,
}

/// A command that starts Workbench, together with the route it uses.
pub struct WorkbenchLaunch {
    pub command: Command,
    pub source: &'static str,
}

impl WorkbenchHost {
    /// Resolves the one route this host starts Workbench through.
    ///
    /// Steam owns the Proton runtime behind a compatibility-data prefix, so
    /// that prefix is started through Steam. A prefix the user owns is started
    /// through the Wine that owns it.
    pub fn workbench_launch_prelude(&self, executable: &Path) -> Option<WorkbenchLaunchPrelude> {
        match self {
            Self::Native => Some(WorkbenchLaunchPrelude {
                program: executable.to_path_buf(),
                leading_arguments: Vec::new(),
                environment: Vec::new(),
                working_directory: executable.parent().map(Path::to_path_buf),
                source: "native",
            }),
            Self::Wine(prefix) => match prefix.source() {
                WinePrefixSource::SteamCompatibilityData => {
                    let (program, leading) = steam_client()?;
                    Some(WorkbenchLaunchPrelude {
                        program,
                        leading_arguments: leading
                            .into_iter()
                            .chain([
                                OsString::from("-applaunch"),
                                OsString::from(REFORGER_TOOLS_APP_ID),
                            ])
                            .collect(),
                        environment: Vec::new(),
                        working_directory: None,
                        source: "steam-proton",
                    })
                }
                WinePrefixSource::Explicit | WinePrefixSource::Environment => {
                    Some(WorkbenchLaunchPrelude {
                        program: executable_in_path("wine")?,
                        leading_arguments: vec![executable.as_os_str().to_os_string()],
                        environment: vec![(
                            "WINEPREFIX".to_string(),
                            prefix.root().as_os_str().to_os_string(),
                        )],
                        working_directory: executable.parent().map(Path::to_path_buf),
                        source: "wine",
                    })
                }
            },
            Self::Unavailable => None,
        }
    }

    /// Builds the command that starts Workbench with the given arguments.
    pub fn workbench_launch(
        &self,
        executable: &Path,
        arguments: &[OsString],
    ) -> Option<WorkbenchLaunch> {
        let prelude = self.workbench_launch_prelude(executable)?;
        let mut command = Command::new(&prelude.program);
        command.args(&prelude.leading_arguments).args(arguments);
        for (name, value) in &prelude.environment {
            command.env(name, value);
        }
        if let Some(working_directory) = &prelude.working_directory {
            command.current_dir(working_directory);
        }
        Some(WorkbenchLaunch {
            command,
            source: prelude.source,
        })
    }

    /// The host desktop handler that starts Workbench for a URL scheme.
    ///
    /// A native host resolves the scheme through its own registry and needs no
    /// separate desktop entry.
    pub fn url_scheme_handler(
        &self,
        scheme: &str,
        display_name: &str,
        executable: &Path,
        arguments: &[String],
    ) -> Option<url_scheme::SchemeHandler> {
        if self.wine_prefix().is_none() {
            return None;
        }
        let prelude = self.workbench_launch_prelude(executable)?;
        let mut command = Vec::new();
        if !prelude.environment.is_empty() {
            // A desktop entry cannot set the environment itself.
            command.push(executable_in_path("env")?.to_str()?.to_string());
            for (name, value) in &prelude.environment {
                command.push(format!("{name}={}", value.to_str()?));
            }
        }
        command.push(prelude.program.to_str()?.to_string());
        for argument in &prelude.leading_arguments {
            command.push(argument.to_str()?.to_string());
        }
        command.extend(arguments.iter().cloned());
        Some(url_scheme::SchemeHandler {
            scheme: scheme.to_string(),
            entry_id: format!("reforger-script-tools-{scheme}"),
            display_name: display_name.to_string(),
            command,
        })
    }
}

/// The Steam client command for this host, preferring a native install and
/// using the Flatpak entry point only where Steam lives there.
fn steam_client() -> Option<(PathBuf, Vec<OsString>)> {
    if let Some(steam) = executable_in_path("steam") {
        return Some((steam, Vec::new()));
    }
    let flatpak_root = home_directory()?
        .join(".var")
        .join("app")
        .join("com.valvesoftware.Steam");
    if !flatpak_root.is_dir() {
        return None;
    }
    Some((
        executable_in_path("flatpak")?,
        vec![
            OsString::from("run"),
            OsString::from("com.valvesoftware.Steam"),
        ],
    ))
}

fn executable_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Reports a host operation that only one platform can perform.
pub fn unsupported(operation: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{operation} is not supported on this host"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix() -> WinePrefix {
        WinePrefix::from_drives(
            PathBuf::from("/prefix"),
            WinePrefixSource::Explicit,
            vec![
                ('c', PathBuf::from("/prefix/drive_c")),
                ('z', PathBuf::from("/")),
            ],
        )
    }

    #[test]
    fn wine_paths_translate_through_the_deepest_matching_drive() {
        let prefix = prefix();
        assert_eq!(
            prefix.to_host_path(r"C:\users\steamuser\Documents"),
            Some(PathBuf::from("/prefix/drive_c/users/steamuser/Documents")),
        );
        assert_eq!(
            prefix.to_host_path("C:/Game/addons/data"),
            Some(PathBuf::from("/prefix/drive_c/Game/addons/data")),
        );
        assert_eq!(
            prefix.to_host_path(r"\\?\Z:\home\dev\addons"),
            Some(PathBuf::from("/home/dev/addons")),
        );
    }

    #[test]
    fn wine_paths_reject_values_with_no_host_meaning() {
        let prefix = prefix();
        assert_eq!(prefix.to_host_path(r"C:relative\path"), None);
        assert_eq!(prefix.to_host_path(r"C:\addons\..\..\etc"), None);
        assert_eq!(prefix.to_host_path(r"\\server\share\addons"), None);
        assert_eq!(prefix.to_host_path("Q:/missing/drive"), None);
    }

    #[test]
    fn host_paths_translate_back_to_the_deepest_matching_drive() {
        let prefix = prefix();
        assert_eq!(
            prefix.to_workbench_path(Path::new("/prefix/drive_c/Game/addons")),
            Some(r"C:\Game\addons".to_string()),
        );
        assert_eq!(
            prefix.to_workbench_path(Path::new("/home/dev/addons")),
            Some(r"Z:\home\dev\addons".to_string()),
        );
        assert_eq!(
            prefix.to_workbench_path(Path::new("/prefix/drive_c")),
            Some(r"C:\".to_string()),
        );
    }

    #[test]
    fn a_native_host_keeps_absolute_paths_and_rejects_relative_ones() {
        let host = WorkbenchHost::Native;
        let absolute = if cfg!(windows) {
            r"C:\Game\addons"
        } else {
            "/game/addons"
        };
        assert_eq!(
            host.to_host_path(absolute),
            Some(PathBuf::from(absolute)),
            "an absolute Workbench path is already a host path",
        );
        assert_eq!(host.to_host_path("addons"), None);
    }

    #[test]
    fn an_unavailable_host_translates_nothing() {
        let host = WorkbenchHost::Unavailable;
        assert_eq!(host.to_host_path(r"C:\Game"), None);
        assert_eq!(host.to_workbench_path(Path::new("/game")), None);
        assert_eq!(host.user_directory(), None);
    }

    #[test]
    fn steam_manifest_fields_read_from_the_installed_layout() {
        let manifest = "\"AppState\"\n{\n\t\"appid\"\t\"1874910\"\n\t\"installdir\"\t\"Arma Reforger Tools\"\n}\n";
        assert_eq!(
            acf_string(manifest, "installdir"),
            Some("Arma Reforger Tools".to_string()),
        );
        assert_eq!(acf_string(manifest, "missing"), None);
    }
}
