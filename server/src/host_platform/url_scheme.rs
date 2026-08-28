//! Host desktop registration for a URL scheme.
//!
//! Workbench resolves an `enfusion` link through its own registry, but a link
//! followed outside the prefix — from the editor, a browser, or a chat client —
//! is resolved by the host desktop instead. On a freedesktop host that means a
//! desktop entry declaring the scheme and an association naming it, both of
//! which live entirely in the user's own configuration.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A desktop handler for one URL scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemeHandler {
    pub scheme: String,
    /// Basename of the desktop entry, without the `.desktop` suffix.
    pub entry_id: String,
    pub display_name: String,
    /// The program and arguments that open the link, including the `%u` field
    /// code the desktop replaces with the URL.
    pub command: Vec<String>,
}

impl SchemeHandler {
    fn entry_file_name(&self) -> String {
        format!("{}.desktop", self.entry_id)
    }

    fn mime_type(&self) -> String {
        format!("x-scheme-handler/{}", self.scheme)
    }

    /// The desktop entry that declares this handler.
    fn entry(&self) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={}\n\
             Exec={}\n\
             Terminal=false\n\
             NoDisplay=true\n\
             MimeType={};\n",
            self.display_name,
            self.command
                .iter()
                .map(|argument| quote_exec_argument(argument))
                .collect::<Vec<_>>()
                .join(" "),
            self.mime_type(),
        )
    }
}

/// Registers the handler with the host desktop, reporting whether anything
/// changed.
pub fn register(handler: &SchemeHandler) -> io::Result<bool> {
    let entry_path = entry_path(handler).ok_or_else(|| unavailable_home())?;
    let entry = handler.entry();
    let mut changed = false;
    if fs::read_to_string(&entry_path).ok().as_deref() != Some(entry.as_str()) {
        if let Some(directory) = entry_path.parent() {
            fs::create_dir_all(directory)?;
        }
        fs::write(&entry_path, &entry)?;
        changed = true;
    }
    changed |= associate(handler)?;
    if changed {
        refresh_desktop_database(entry_path.parent());
    }
    Ok(changed)
}

/// Whether the host desktop already resolves the scheme through this handler.
pub fn registered(handler: &SchemeHandler) -> bool {
    let Some(entry_path) = entry_path(handler) else {
        return false;
    };
    fs::read_to_string(&entry_path).ok().as_deref() == Some(handler.entry().as_str())
        && associated(handler)
}

fn entry_path(handler: &SchemeHandler) -> Option<PathBuf> {
    Some(
        data_home()?
            .join("applications")
            .join(handler.entry_file_name()),
    )
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_directory().map(|home| home.join(".local").join("share")))
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_directory().map(|home| home.join(".config")))
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

fn unavailable_home() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "the host has no user data directory to register a URL scheme in",
    )
}

/// Names the handler as the default application for its scheme in the user's
/// own `mimeapps.list`, leaving every other association in place.
fn associate(handler: &SchemeHandler) -> io::Result<bool> {
    let path = config_home()
        .ok_or_else(unavailable_home)?
        .join("mimeapps.list");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let Some(updated) = set_association(&existing, handler) else {
        return Ok(false);
    };
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(&path, updated)?;
    Ok(true)
}

fn associated(handler: &SchemeHandler) -> bool {
    config_home()
        .map(|config| config.join("mimeapps.list"))
        .and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|list| set_association(&list, handler).is_none())
}

/// Returns the list with the association applied, or `None` when the list
/// already names this handler for the scheme.
fn set_association(list: &str, handler: &SchemeHandler) -> Option<String> {
    const SECTION: &str = "[Default Applications]";
    let entry = format!("{}={}", handler.mime_type(), handler.entry_file_name());
    let mut lines = list.lines().map(str::to_string).collect::<Vec<_>>();
    let section = lines.iter().position(|line| line.trim() == SECTION);
    let Some(section) = section else {
        if lines.last().is_some_and(|line| !line.is_empty()) {
            lines.push(String::new());
        }
        lines.push(SECTION.to_string());
        lines.push(entry);
        return Some(join_lines(lines));
    };
    let end = lines[section + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| section + 1 + offset);
    let prefix = format!("{}=", handler.mime_type());
    match (section + 1..end).find(|index| lines[*index].trim_start().starts_with(&prefix)) {
        Some(index) if lines[index].trim() == entry => None,
        Some(index) => {
            lines[index] = entry;
            Some(join_lines(lines))
        }
        None => {
            lines.insert(end, entry);
            Some(join_lines(lines))
        }
    }
}

fn join_lines(lines: Vec<String>) -> String {
    let mut joined = lines.join("\n");
    joined.push('\n');
    joined
}

/// Rebuilds the desktop database when the host provides the tool. The entry and
/// the association are already in place either way; this only refreshes the
/// cache some desktops read first.
fn refresh_desktop_database(applications: Option<&Path>) {
    let (Some(applications), Some(tool)) = (applications, executable_in_path()) else {
        return;
    };
    let _ = std::process::Command::new(tool)
        .arg(applications)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn executable_in_path() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join("update-desktop-database"))
        .find(|candidate| candidate.is_file())
}

/// Quotes one `Exec` argument. A field code such as `%u` is a directive rather
/// than text and is never quoted.
fn quote_exec_argument(argument: &str) -> String {
    if argument.starts_with('%') || !argument.contains([' ', '\t', '"', '\'', '\\', '$', '`']) {
        return argument.to_string();
    }
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    for character in argument.chars() {
        if matches!(character, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> SchemeHandler {
        SchemeHandler {
            scheme: "enfusion".to_string(),
            entry_id: "reforger-script-tools-enfusion".to_string(),
            display_name: "Arma Reforger Workbench".to_string(),
            command: vec![
                "/usr/bin/steam".to_string(),
                "-applaunch".to_string(),
                "1874910".to_string(),
                "-gproj".to_string(),
                "C:/Arma Reforger/addons/data/ArmaReforger.gproj".to_string(),
                "-uri=%u".to_string(),
            ],
        }
    }

    #[test]
    fn the_entry_declares_the_scheme_and_quotes_only_what_needs_it() {
        let entry = handler().entry();
        assert!(entry.contains("MimeType=x-scheme-handler/enfusion;\n"));
        assert!(entry.contains(
            "Exec=/usr/bin/steam -applaunch 1874910 -gproj \
             \"C:/Arma Reforger/addons/data/ArmaReforger.gproj\" -uri=%u\n"
        ));
    }

    #[test]
    fn the_association_is_added_to_a_list_that_has_no_section() {
        let updated = set_association("", &handler()).expect("the association is missing");
        assert!(updated.contains("[Default Applications]\n"));
        assert!(
            updated.contains("x-scheme-handler/enfusion=reforger-script-tools-enfusion.desktop\n")
        );
    }

    #[test]
    fn the_association_joins_an_existing_section_without_disturbing_it() {
        let list = "[Added Associations]\ntext/plain=editor.desktop\n\n\
                    [Default Applications]\ntext/html=browser.desktop\n";
        let updated = set_association(list, &handler()).expect("the association is missing");
        assert!(updated.contains("text/html=browser.desktop\n"));
        assert!(updated.contains("text/plain=editor.desktop\n"));
        assert_eq!(updated.matches("[Default Applications]").count(), 1);
        assert!(
            set_association(&updated, &handler()).is_none(),
            "an association that already names this handler is left alone",
        );
    }

    #[test]
    fn an_association_naming_another_application_is_replaced() {
        let list = "[Default Applications]\nx-scheme-handler/enfusion=other.desktop\n";
        let updated = set_association(list, &handler()).expect("the association differs");
        assert!(!updated.contains("other.desktop"));
        assert!(
            updated.contains("x-scheme-handler/enfusion=reforger-script-tools-enfusion.desktop\n")
        );
    }
}
