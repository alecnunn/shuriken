//! Parsing `cl.exe /showIncludes` output, for `deps = msvc`.

use crate::canon::canonicalize_path;

/// The default English prefix MSVC uses for include notes.
pub const DEFAULT_DEPS_PREFIX: &str = "Note: including file: ";

/// The result of filtering compiler output.
#[derive(Debug, Default)]
pub struct ShowIncludes {
    /// Discovered include paths, sorted and de-duplicated.
    pub includes: Vec<String>,
    /// The compiler output with the include notes removed.
    pub filtered_output: String,
}

/// If `line` is an include note, return the path it mentions.
pub fn filter_show_includes(line: &str, deps_prefix: &str) -> Option<String> {
    let prefix = if deps_prefix.is_empty() {
        DEFAULT_DEPS_PREFIX
    } else {
        deps_prefix
    };
    let rest = line.strip_prefix(prefix)?;
    if line.len() <= prefix.len() {
        return None;
    }
    Some(rest.trim_start_matches(' ').to_string())
}

/// True for paths that look like they come from the toolchain rather than the
/// project, which ninja excludes from the dependency list.
pub fn is_system_include(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("program files") || lower.contains("microsoft visual studio")
}

/// True if `line` is just the input filename that `cl.exe` echoes.
pub fn is_input_filename(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [".c", ".cc", ".cxx", ".cpp", ".c++"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Split `output` into discovered includes and the output to show the user.
pub fn parse_show_includes(output: &str, deps_prefix: &str) -> ShowIncludes {
    let mut includes: Vec<String> = Vec::new();
    let mut filtered_output = String::new();
    let mut seen_show_includes = false;

    for line in output.split_inclusive(['\r', '\n']) {
        let line = line.trim_end_matches(['\r', '\n']);
        // `split_inclusive` yields an entry for the second half of "\r\n"; skip
        // the empty leftovers it produces.
        if line.is_empty() && !output.is_empty() {
            // Preserve genuinely blank lines, but not the artefacts of CRLF.
            // A blank line is one whose separator we just stripped.
            continue;
        }

        if let Some(include) = filter_show_includes(line, deps_prefix) {
            seen_show_includes = true;
            let mut normalized = include;
            canonicalize_path(&mut normalized);
            if !is_system_include(&normalized) && !includes.contains(&normalized) {
                includes.push(normalized);
            }
        } else if !seen_show_includes && is_input_filename(line) {
            // The echoed input filename is dropped.
        } else {
            filtered_output.push_str(line);
            filtered_output.push('\n');
        }
    }

    includes.sort();
    ShowIncludes {
        includes,
        filtered_output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_include_notes() {
        let output = "foo.cc\r\n\
                      Note: including file: c:\\foo\\bar.h\r\n\
                      warning: something\r\n";
        let r = parse_show_includes(output, "");
        assert_eq!(r.includes, vec!["c:\\foo\\bar.h"]);
        assert_eq!(r.filtered_output, "warning: something\n");
    }

    #[test]
    fn custom_prefix() {
        let output = "Hinweis: Einlesen der Datei: a.h\n";
        let r = parse_show_includes(output, "Hinweis: Einlesen der Datei: ");
        assert_eq!(r.includes, vec!["a.h"]);
        assert!(r.filtered_output.is_empty());
    }

    #[test]
    fn drops_system_includes() {
        let output = "Note: including file: C:\\Program Files\\x\\y.h\n\
                      Note: including file: mine.h\n";
        let r = parse_show_includes(output, "");
        assert_eq!(r.includes, vec!["mine.h"]);
    }

    #[test]
    fn deduplicates() {
        let output = "Note: including file: a.h\nNote: including file: a.h\n";
        let r = parse_show_includes(output, "");
        assert_eq!(r.includes, vec!["a.h"]);
    }
}
