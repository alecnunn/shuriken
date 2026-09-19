//! Version reporting and `ninja_required_version` checking.

/// This crate's own version.
pub const SHURIKEN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The ninja version whose behaviour and file formats this engine implements.
///
/// This is what `--version` reports and what `ninja_required_version` is
/// checked against, so that manifests generated for ninja work unchanged.
pub const NINJA_COMPAT_VERSION: &str = "1.13.2";

/// Split a version string into (major, minor), ignoring any patch component.
pub fn parse_version(version: &str) -> (i32, i32) {
    let mut it = version.split('.');
    let major = it.next().map(atoi).unwrap_or(0);
    let minor = it.next().map(atoi).unwrap_or(0);
    (major, minor)
}

fn atoi(s: &str) -> i32 {
    let s = s.trim_start();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let v: i64 = digits.parse().unwrap_or(0);
    let v = if neg { -v } else { v };
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Check a manifest's `ninja_required_version` value.
///
/// Returns `Ok(None)` when the version is satisfied, `Ok(Some(warning))` when
/// the manifest targets an older ninja, and `Err(message)` when it requires a
/// newer one than this engine implements.
pub fn check_required_version(version: &str) -> Result<Option<String>, String> {
    let (bin_major, bin_minor) = parse_version(NINJA_COMPAT_VERSION);
    let (file_major, file_minor) = parse_version(version);

    if bin_major > file_major {
        return Ok(Some(format!(
            "ninja executable version ({NINJA_COMPAT_VERSION}) greater than build file \
             ninja_required_version ({version}); versions may be incompatible."
        )));
    }

    if (bin_major == file_major && bin_minor < file_minor) || bin_major < file_major {
        return Err(format!(
            "ninja version ({NINJA_COMPAT_VERSION}) incompatible with build file \
             ninja_required_version version ({version})."
        ));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.13.2"), (1, 13));
        assert_eq!(parse_version("2"), (2, 0));
        assert_eq!(parse_version(""), (0, 0));
    }

    #[test]
    fn required_version() {
        assert!(check_required_version("1.13").unwrap().is_none());
        assert!(check_required_version("0.9").unwrap().is_some());
        assert!(check_required_version("2.0").is_err());
        assert!(check_required_version("1.99").is_err());
    }
}
