//! Path canonicalization, matching ninja's `CanonicalizePath` exactly.
//!
//! Canonicalization collapses `.`, `..` and duplicate separators without
//! touching the filesystem, so that two spellings of the same path in a
//! manifest map to the same graph node. On Windows, backslashes are folded to
//! forward slashes and a bitmask records which separators were backslashes so
//! the original spelling can be restored for command lines
//! ([`decanonicalize`]).

/// True if `c` separates path components on the host platform.
#[inline]
pub const fn is_path_separator(c: u8) -> bool {
    if cfg!(windows) {
        c == b'/' || c == b'\\'
    } else {
        c == b'/'
    }
}

/// Canonicalize `path` in place, returning its "slash bits" (always 0 on
/// non-Windows hosts).
pub fn canonicalize_path(path: &mut String) -> u64 {
    let mut bytes = std::mem::take(path).into_bytes();
    let bits = canonicalize_bytes(&mut bytes);
    // Canonicalization only deletes whole path components, moves byte ranges
    // and inserts ASCII, so UTF-8 validity is preserved.
    *path = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    bits
}

/// Canonicalize a path, returning the result and its slash bits.
pub fn canonicalized(path: &str) -> (String, u64) {
    let mut s = path.to_string();
    let bits = canonicalize_path(&mut s);
    (s, bits)
}

/// Canonicalize raw bytes in place, returning the slash bits.
pub fn canonicalize_bytes(v: &mut Vec<u8>) -> u64 {
    let end = v.len();
    if end == 0 {
        return 0;
    }

    let mut dst: usize = 0;
    let mut src: usize = 0;
    let mut dst_start: usize;

    if is_path_separator(v[0]) {
        // For absolute paths, keep the leading separator (two of them for
        // Windows network paths).
        if cfg!(windows) && end >= 2 && is_path_separator(v[1]) {
            src = 2;
            dst = 2;
        } else {
            src = 1;
            dst = 1;
        }
        dst_start = dst;
    } else {
        dst_start = 0;
        // Leading "../" sequences are common and can never be simplified.
        while src + 3 <= end
            && v[src] == b'.'
            && v[src + 1] == b'.'
            && is_path_separator(v[src + 2])
        {
            src += 3;
            dst += 3;
        }
    }

    let mut component_count: usize = 0;
    let dst0 = dst;

    // All components but the last one (which has no trailing separator).
    while src < end {
        let mut next_sep = src;
        while next_sep < end && !is_path_separator(v[next_sep]) {
            next_sep += 1;
        }
        if next_sep == end {
            break; // Handled below.
        }
        let src_next = next_sep + 1;
        let component_len = next_sep - src;

        if component_len <= 2 {
            if component_len == 0 {
                src = src_next; // "foo//bar" -> "foo/bar"
                continue;
            }
            if v[src] == b'.' {
                if component_len == 1 {
                    src = src_next; // "./foo" -> "foo"
                    continue;
                } else if v[src + 1] == b'.' {
                    if component_count > 0 {
                        component_count -= 1;
                        loop {
                            dst -= 1;
                            if !(dst > dst0 && !is_path_separator(v[dst - 1])) {
                                break;
                            }
                        }
                    } else {
                        v[dst] = b'.';
                        v[dst + 1] = b'.';
                        v[dst + 2] = v[src + 2];
                        dst += 3;
                    }
                    src = src_next;
                    continue;
                }
            }
        }

        component_count += 1;
        if dst != src {
            v.copy_within(src..src_next, dst);
        }
        dst += src_next - src;
        src = src_next;
    }

    // The trailing component, which has no separator after it.
    let component_len = end - src;
    'last: {
        if component_len == 0 {
            break 'last; // "foo//" -> "foo/"
        }
        if v[src] == b'.' {
            if component_len == 1 {
                break 'last; // "foo/." -> "foo/"
            }
            if component_len == 2 && v[src + 1] == b'.' {
                if component_count > 0 {
                    loop {
                        dst -= 1;
                        if !(dst > dst0 && !is_path_separator(v[dst - 1])) {
                            break;
                        }
                    }
                } else {
                    v[dst] = b'.';
                    v[dst + 1] = b'.';
                    dst += 2;
                }
                break 'last;
            }
        }
        if dst != src {
            v.copy_within(src..src + component_len, dst);
        }
        dst += component_len;
    }

    // Drop a trailing separator, but never the leading one(s).
    if dst > dst_start && is_path_separator(v[dst - 1]) {
        dst -= 1;
    }
    if dst == 0 {
        // e.g. "aa/.." -> "."
        v[0] = b'.';
        dst = 1;
    }
    v.truncate(dst);
    dst_start = dst_start.min(dst); // silence unused-assignment lint on unix
    let _ = dst_start;

    if cfg!(windows) {
        let mut bits: u64 = 0;
        let mut mask: u64 = 1;
        for c in v.iter_mut() {
            match *c {
                b'\\' => {
                    bits |= mask;
                    *c = b'/';
                    mask <<= 1;
                }
                b'/' => mask <<= 1,
                _ => {}
            }
        }
        bits
    } else {
        0
    }
}

/// Restore the original separator spelling recorded in `slash_bits`.
///
/// This is a no-op on non-Windows hosts, where `slash_bits` is always 0.
pub fn decanonicalize(path: &str, slash_bits: u64) -> String {
    if !cfg!(windows) || slash_bits == 0 {
        return path.to_string();
    }
    let mut out = path.as_bytes().to_vec();
    let mut mask: u64 = 1;
    for c in out.iter_mut() {
        if *c == b'/' {
            if slash_bits & mask != 0 {
                *c = b'\\';
            }
            mask <<= 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| path.to_string())
}

/// ninja's `DirName`: the directory part of `path`, or `""` if there is none.
pub fn dir_name(path: &str) -> &str {
    let b = path.as_bytes();
    let mut slash_pos = match b.iter().rposition(|&c| is_path_separator(c)) {
        Some(p) => p,
        None => return "",
    };
    while slash_pos > 0 && is_path_separator(b[slash_pos - 1]) {
        slash_pos -= 1;
    }
    &path[..slash_pos]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(s: &str) -> String {
        let mut p = s.to_string();
        canonicalize_path(&mut p);
        p
    }

    #[test]
    fn basics() {
        assert_eq!(canon("foo.h"), "foo.h");
        assert_eq!(canon("./foo.h"), "foo.h");
        assert_eq!(canon("./foo/./bar.h"), "foo/bar.h");
        assert_eq!(canon("./x/foo/../bar.h"), "x/bar.h");
        assert_eq!(canon("./x/foo/../../bar.h"), "bar.h");
        assert_eq!(canon("foo//bar"), "foo/bar");
        assert_eq!(canon("foo//.//..///bar"), "bar");
        assert_eq!(canon("./x/../foo/../../bar.h"), "../bar.h");
        assert_eq!(canon("foo/./."), "foo");
        assert_eq!(canon("foo/bar/.."), "foo");
        assert_eq!(canon("foo/.hidden_bar"), "foo/.hidden_bar");
        assert_eq!(canon("/foo"), "/foo");
        assert_eq!(canon("//foo"), if cfg!(windows) { "//foo" } else { "/foo" });
        assert_eq!(canon("/"), "/");
        assert_eq!(canon("aa/.."), ".");
        assert_eq!(canon(".."), "..");
        assert_eq!(canon("../"), "..");
        assert_eq!(canon("../foo"), "../foo");
        assert_eq!(canon("a/b/c/d/e/../../../../f"), "a/f");
        assert_eq!(canon("./"), ".");
        assert_eq!(canon("."), ".");
        assert_eq!(canon(""), "");
    }

    #[test]
    fn dirnames() {
        assert_eq!(dir_name("foo.c"), "");
        assert_eq!(dir_name("a/b/c.c"), "a/b");
        assert_eq!(dir_name("/a/b.c"), "/a");
        assert_eq!(dir_name("/b.c"), "");
        assert_eq!(dir_name("a//b.c"), "a");
    }
}
