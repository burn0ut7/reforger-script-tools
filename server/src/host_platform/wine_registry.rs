//! The Wine prefix registry hive.
//!
//! Wine keeps `HKEY_CURRENT_USER` in the prefix's `user.reg` text hive. A
//! wineserver reads the hive when it starts and rewrites it from memory when it
//! shuts down, so an edit made from outside the prefix only survives while no
//! wineserver holds it. Callers check [`prefix_in_use`] before writing and
//! report the prefix as busy rather than making an edit Wine would discard.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::canonicalize_lenient;

/// Reads one string value from a hive, returning `None` when the key, the
/// value, or its string type is absent.
pub fn read_string(hive: &Path, key: &str, value_name: Option<&str>) -> Option<String> {
    let text = fs::read_to_string(hive).ok()?;
    let lines = text.lines().collect::<Vec<_>>();
    let start = section_start(&lines, key)?;
    let end = section_end(&lines, start);
    lines[start + 1..end].iter().find_map(|line| {
        parse_value(line)
            .filter(|(name, _)| matches_name(name.as_deref(), value_name))
            .map(|(_, value)| value)
    })
}

/// Writes one string value into a hive, reporting whether the hive changed.
///
/// The hive must already exist: an uninitialized prefix is reported as such
/// rather than seeded with a hive Wine did not write.
pub fn write_string(
    hive: &Path,
    key: &str,
    value_name: Option<&str>,
    value: &str,
) -> io::Result<bool> {
    let text = fs::read_to_string(hive)?;
    let mut lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    let formatted = format_value(value_name, value);
    match section_start(&lines, key) {
        Some(start) => {
            let end = section_end(&lines, start);
            let existing = (start + 1..end).find(|index| {
                parse_value(&lines[*index])
                    .is_some_and(|(name, _)| matches_name(name.as_deref(), value_name))
            });
            match existing {
                Some(index) => {
                    if parse_value(&lines[index]).is_some_and(|(_, current)| current == value) {
                        return Ok(false);
                    }
                    lines[index] = formatted;
                }
                None => {
                    // Wine writes its own directives directly under the section
                    // header; the value belongs after them.
                    let mut insert = start + 1;
                    while insert < end && lines[insert].starts_with('#') {
                        insert += 1;
                    }
                    lines.insert(insert, formatted);
                }
            }
        }
        None => {
            if lines.last().is_some_and(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{}] {}", escape(key), unix_seconds()));
            lines.push(formatted);
        }
    }
    let mut contents = lines.join("\n");
    contents.push('\n');
    write_atomic(hive, &contents)?;
    Ok(true)
}

/// Whether a wineserver currently holds the prefix, which makes an external
/// registry edit unreliable.
pub fn prefix_in_use(prefix_root: &Path) -> bool {
    platform::prefix_in_use(&canonicalize_lenient(prefix_root))
}

fn section_start<S: AsRef<str>>(lines: &[S], key: &str) -> Option<usize> {
    lines.iter().position(|line| {
        section_key(line.as_ref()).is_some_and(|existing| existing.eq_ignore_ascii_case(key))
    })
}

fn section_end<S: AsRef<str>>(lines: &[S], start: usize) -> usize {
    lines[start + 1..]
        .iter()
        .position(|line| section_key(line.as_ref()).is_some())
        .map_or(lines.len(), |offset| start + 1 + offset)
}

/// The key a `[Escaped\\Key] 1700000000` section header names.
fn section_key(line: &str) -> Option<String> {
    let rest = line.strip_prefix('[')?;
    Some(unescape(&rest[..rest.rfind(']')?]))
}

/// The name and string content of a `"Name"="Value"` or `@="Value"` line.
/// Values of any other registry type are not string values and read as absent.
fn parse_value(line: &str) -> Option<(Option<String>, String)> {
    let line = line.trim_start();
    let (name, rest) = match line.strip_prefix('@') {
        Some(rest) => (None, rest),
        None => {
            let (name, rest) = parse_quoted(line)?;
            (Some(name), rest)
        }
    };
    let (value, rest) = parse_quoted(rest.strip_prefix('=')?)?;
    rest.trim().is_empty().then_some((name, value))
}

fn parse_quoted(text: &str) -> Option<(String, &str)> {
    let body = text.strip_prefix('"')?;
    let mut value = String::new();
    let mut escaped = false;
    for (offset, character) in body.char_indices() {
        if escaped {
            value.push(match character {
                'n' => '\n',
                'r' => '\r',
                '0' => '\0',
                other => other,
            });
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Some((value, &body[offset + 1..])),
            other => value.push(other),
        }
    }
    None
}

fn matches_name(parsed: Option<&str>, requested: Option<&str>) -> bool {
    match (parsed, requested) {
        (None, None) => true,
        (Some(parsed), Some(requested)) => parsed.eq_ignore_ascii_case(requested),
        _ => false,
    }
}

fn format_value(value_name: Option<&str>, value: &str) -> String {
    match value_name {
        Some(name) => format!("\"{}\"=\"{}\"", escape(name), escape(value)),
        None => format!("@=\"{}\"", escape(value)),
    }
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' | '"' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn unescape(value: &str) -> String {
    let mut unescaped = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            unescaped.push(match character {
                'n' => '\n',
                'r' => '\r',
                '0' => '\0',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            unescaped.push(character);
        }
    }
    unescaped
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the registry hive has no directory",
        )
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("user.reg");
    let temporary = directory.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temporary, contents)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
mod platform {
    use super::super::process;
    use super::canonicalize_lenient;
    use std::path::{Path, PathBuf};

    pub(super) fn prefix_in_use(prefix_root: &Path) -> bool {
        process::process_ids()
            .into_iter()
            .any(|id| held_prefix(id).is_some_and(|held| held == prefix_root))
    }

    /// The prefix a process holds, from the environment Wine passes down.
    fn held_prefix(id: u32) -> Option<PathBuf> {
        if let Some(value) = process::environment_value(id, "WINEPREFIX") {
            return (!value.is_empty()).then(|| canonicalize_lenient(Path::new(&value)));
        }
        // A wineserver started without WINEPREFIX holds Wine's default prefix.
        (process::process_name(id)? == "wineserver")
            .then(|| std::env::var_os("HOME"))
            .flatten()
            .map(|home| canonicalize_lenient(&PathBuf::from(home).join(".wine")))
    }
}

#[cfg(not(unix))]
mod platform {
    use std::path::Path;

    pub(super) fn prefix_in_use(_prefix_root: &Path) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIVE: &str = concat!(
        "WINE REGISTRY Version 2\n",
        ";; All keys relative to \\\\User\\\\S-1-5-21-0-0-0-1000\n",
        "\n",
        "#arch=win64\n",
        "\n",
        "[Software\\\\Bohemia Interactive\\\\Arma Reforger Workbench\\\\Workbench] 1700000000\n",
        "#time=1d9f000000000000\n",
        "\"Language\"=\"en\"\n",
        "\n",
        "[Software\\\\Wine] 1700000001\n",
        "\"Version\"=\"win10\"\n",
    );

    fn hive_path(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("rst-wine-registry-{}-{name}", std::process::id()));
        fs::write(&path, HIVE).expect("hive fixture written");
        path
    }

    #[test]
    fn existing_values_read_through_the_escaped_key() {
        let path = hive_path("read");
        assert_eq!(
            read_string(
                &path,
                r"Software\Bohemia Interactive\Arma Reforger Workbench\Workbench",
                Some("Language"),
            ),
            Some("en".to_string()),
        );
        assert_eq!(read_string(&path, r"Software\Wine", Some("Missing")), None);
        assert_eq!(read_string(&path, r"Software\Absent", None), None);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_new_value_joins_the_existing_section_after_its_directives() {
        let path = hive_path("insert");
        let key = r"Software\Bohemia Interactive\Arma Reforger Workbench\Workbench";
        assert!(write_string(&path, key, Some("NetAPI_Enabled"), "1").expect("hive written"));
        assert_eq!(
            read_string(&path, key, Some("NetAPI_Enabled")),
            Some("1".to_string()),
        );
        assert_eq!(
            read_string(&path, key, Some("Language")),
            Some("en".to_string()),
            "the existing values in the section are preserved",
        );
        assert!(
            !write_string(&path, key, Some("NetAPI_Enabled"), "1").expect("hive re-read"),
            "writing the value it already holds changes nothing",
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_missing_section_is_appended_with_its_default_value() {
        let path = hive_path("append");
        let command = r#""C:\Tools\Workbench.exe" -uri="%1""#;
        assert!(write_string(
            &path,
            r"Software\Classes\enfusion\shell\open\command",
            None,
            command
        )
        .expect("hive written"));
        assert_eq!(
            read_string(&path, r"Software\Classes\enfusion\shell\open\command", None),
            Some(command.to_string()),
        );
        assert_eq!(
            read_string(&path, r"Software\Wine", Some("Version")),
            Some("win10".to_string()),
            "appending a section leaves the rest of the hive intact",
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn replacing_a_value_keeps_the_section_it_lives_in() {
        let path = hive_path("replace");
        assert!(write_string(&path, r"Software\Wine", Some("Version"), "win11").expect("written"));
        assert_eq!(
            read_string(&path, r"Software\Wine", Some("Version")),
            Some("win11".to_string()),
        );
        let contents = fs::read_to_string(&path).expect("hive read");
        assert_eq!(
            contents.matches("[Software\\\\Wine]").count(),
            1,
            "the section is edited in place rather than duplicated",
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_missing_hive_is_reported_rather_than_created() {
        let path = std::env::temp_dir().join(format!(
            "rst-wine-registry-absent-{}.reg",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let error = write_string(&path, r"Software\Wine", Some("Version"), "win11")
            .expect_err("an uninitialized prefix has no hive");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!path.exists());
    }
}
