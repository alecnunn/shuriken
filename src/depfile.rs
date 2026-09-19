//! Parser for Makefile-style dependency files (`gcc -MD` output).
//!
//! This follows what GCC and Clang actually emit, exactly as ninja's
//! `depfile_parser.in.cc` does:
//!
//! * a backslash escapes a space or a hash sign,
//! * a space preceded by 2N+1 backslashes is N backslashes then a space,
//! * a space preceded by 2N backslashes is 2N backslashes ending a filename,
//! * `\:` is a colon unless whitespace follows (then it is literal text),
//! * `$$` is a literal dollar sign,
//! * `\` at end of line is a line continuation.

/// The targets and prerequisites found in a depfile.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Depfile {
    /// Declared outputs (the left-hand side).
    pub outs: Vec<String>,
    /// Declared inputs (the right-hand side), de-duplicated.
    pub ins: Vec<String>,
}

#[inline]
const fn is_plain(c: u8) -> bool {
    matches!(c,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
        | b'+' | b'?' | b'"' | b'\'' | b'&' | b',' | b'/' | b'_' | b':' | b'.' | b'~'
        | b'(' | b')' | b'}' | b'{' | b'%' | b'=' | b'@' | b'[' | b']' | b'!' | b'-'
        | 0x80..=0xff)
}

/// Parse `content` as a depfile.
pub fn parse_depfile(content: &[u8]) -> Result<Depfile, String> {
    let mut outs: Vec<Vec<u8>> = Vec::new();
    let mut ins: Vec<Vec<u8>> = Vec::new();

    let mut have_target = false;
    let mut parsing_targets = true;
    let mut poisoned_input = false;
    let mut is_empty = true;

    let end = content.len();
    let mut i = 0usize;

    while i < end {
        let mut have_newline = false;
        let mut buf: Vec<u8> = Vec::new();

        // Accumulate one filename.
        loop {
            if i >= end {
                break;
            }
            let c = content[i];

            if c == 0 {
                i += 1;
                break;
            }

            if c == b'\\' {
                let mut k = 0usize;
                while i + k < end && content[i + k] == b'\\' {
                    k += 1;
                }
                let after = content.get(i + k).copied();
                match after {
                    Some(b' ') => {
                        let len = k + 1;
                        if k % 2 == 1 {
                            // 2N+1 backslashes plus space -> N backslashes plus space.
                            let n = len / 2 - 1;
                            buf.extend(std::iter::repeat_n(b'\\', n));
                            buf.push(b' ');
                            i += len;
                            continue;
                        } else {
                            // 2N backslashes plus space -> 2N backslashes, end of filename.
                            buf.extend(std::iter::repeat_n(b'\\', len - 1));
                            i += len;
                            break;
                        }
                    }
                    Some(b'#') => {
                        // De-escape the hash, preserving other backslashes.
                        let len = k + 1;
                        buf.extend(std::iter::repeat_n(b'\\', len.saturating_sub(2)));
                        buf.push(b'#');
                        i += len;
                        continue;
                    }
                    Some(b':') => {
                        let follow = content.get(i + k + 1).copied();
                        let ws = matches!(
                            follow,
                            Some(0) | Some(b' ') | Some(b'\r') | Some(b'\n') | Some(b'\t')
                        );
                        if ws {
                            // Backslashes, a colon and then whitespace: normal
                            // text, and the filename ends here.
                            buf.extend_from_slice(&content[i..i + k + 1]);
                            let last = content[i + k + 1];
                            i += k + 2;
                            if last == b'\n' {
                                have_newline = true;
                            }
                            break;
                        } else {
                            // De-escape the colon, preserving other backslashes.
                            let len = k + 1;
                            buf.extend(std::iter::repeat_n(b'\\', len.saturating_sub(2)));
                            buf.push(b':');
                            i += len;
                            continue;
                        }
                    }
                    None | Some(0) | Some(b'\r') | Some(b'\n') => {
                        if k >= 2 {
                            // A run of backslashes is plain text; the newline
                            // or NUL is handled on the next pass.
                            buf.extend_from_slice(&content[i..i + k]);
                            i += k;
                            continue;
                        }
                        // A single backslash before a newline is a line
                        // continuation, which ends the current filename.
                        if after == Some(b'\n') {
                            i += 2;
                            break;
                        }
                        if after == Some(b'\r') && content.get(i + 2) == Some(&b'\n') {
                            i += 3;
                            break;
                        }
                        // Anything else: swallow the backslash.
                        i += 1;
                        break;
                    }
                    Some(_) => {
                        // Backslashes followed by ordinary text: keep as-is.
                        buf.extend_from_slice(&content[i..i + k + 1]);
                        i += k + 1;
                        continue;
                    }
                }
            }

            if c == b'$' && content.get(i + 1) == Some(&b'$') {
                buf.push(b'$');
                i += 2;
                continue;
            }

            if is_plain(c) {
                let mut j = i;
                while j < end && is_plain(content[j]) {
                    j += 1;
                }
                buf.extend_from_slice(&content[i..j]);
                i = j;
                continue;
            }

            if c == b'\n' {
                have_newline = true;
                i += 1;
                break;
            }
            if c == b'\r' && content.get(i + 1) == Some(&b'\n') {
                have_newline = true;
                i += 2;
                break;
            }

            // Any other character (whitespace, a stray '$', ...) is swallowed
            // and ends the current filename.
            i += 1;
            break;
        }

        let is_dependency = !parsing_targets;
        if !buf.is_empty() && *buf.last().unwrap() == b':' {
            buf.pop(); // Strip the trailing colon.
            parsing_targets = false;
            have_target = true;
        }

        if !buf.is_empty() {
            is_empty = false;
            if ins.contains(&buf) {
                if !is_dependency {
                    // We passed an input on the left side; reject new inputs.
                    poisoned_input = true;
                }
            } else if is_dependency {
                if poisoned_input {
                    return Err("inputs may not also have inputs".to_string());
                }
                ins.push(buf);
            } else if !outs.contains(&buf) {
                outs.push(buf);
            }
        }

        if have_newline {
            parsing_targets = true;
            poisoned_input = false;
        }
    }

    if !have_target && !is_empty {
        return Err("expected ':' in depfile".to_string());
    }

    Ok(Depfile {
        outs: to_strings(outs)?,
        ins: to_strings(ins)?,
    })
}

fn to_strings(v: Vec<Vec<u8>>) -> Result<Vec<String>, String> {
    v.into_iter()
        .map(|b| {
            String::from_utf8(b).map_err(|e| {
                format!(
                    "path is not valid UTF-8: {}",
                    String::from_utf8_lossy(e.as_bytes())
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Depfile {
        parse_depfile(s.as_bytes()).unwrap()
    }

    #[test]
    fn basic() {
        let d = parse("build/ninja.o: ninja.cc ninja.h eval_env.h manifest_parser.h\n");
        assert_eq!(d.outs, vec!["build/ninja.o"]);
        assert_eq!(
            d.ins,
            vec!["ninja.cc", "ninja.h", "eval_env.h", "manifest_parser.h"]
        );
    }

    #[test]
    fn early_newline_and_continuations() {
        let d = parse("foo.o: \\\n  bar.h \\\n  baz.h\n");
        assert_eq!(d.outs, vec!["foo.o"]);
        assert_eq!(d.ins, vec!["bar.h", "baz.h"]);
    }

    #[test]
    fn escaped_spaces() {
        let d = parse("a\\ b.o: c\\ d.h\n");
        assert_eq!(d.outs, vec!["a b.o"]);
        assert_eq!(d.ins, vec!["c d.h"]);
    }

    #[test]
    fn even_backslashes_end_filename() {
        // 2 backslashes then a space: two backslashes, filename ends.
        let d = parse("a: x\\\\ y\n");
        assert_eq!(d.ins, vec!["x\\\\", "y"]);
    }

    #[test]
    fn escaped_hash_and_dollar() {
        let d = parse("a: b\\#c d$$e\n");
        assert_eq!(d.ins, vec!["b#c", "d$e"]);
    }

    #[test]
    fn escaped_colon() {
        let d = parse("a: c\\:\\\\foo.h\n");
        assert_eq!(d.ins, vec!["c:\\\\foo.h"]);
    }

    #[test]
    fn windows_drive_letters() {
        let d = parse("c:\\a\\b.o: c:\\a\\b.cc\n");
        assert_eq!(d.outs, vec!["c:\\a\\b.o"]);
        assert_eq!(d.ins, vec!["c:\\a\\b.cc"]);
    }

    #[test]
    fn backslash_colon_whitespace_is_literal() {
        // "\:" followed by whitespace is literal text, and the trailing colon
        // still terminates the target list.
        let d = parse("foo\\: bar\n");
        assert_eq!(d.outs, vec!["foo\\"]);
        assert_eq!(d.ins, vec!["bar"]);
    }

    #[test]
    fn duplicates_are_dropped() {
        let d = parse("a: b b c\n");
        assert_eq!(d.ins, vec!["b", "c"]);
    }

    #[test]
    fn multiple_outputs() {
        let d = parse("a b: c\n");
        assert_eq!(d.outs, vec!["a", "b"]);
        assert_eq!(d.ins, vec!["c"]);
    }

    #[test]
    fn multiple_rules() {
        let d = parse("a: b\nc: d\n");
        assert_eq!(d.outs, vec!["a", "c"]);
        assert_eq!(d.ins, vec!["b", "d"]);
    }

    #[test]
    fn empty_is_ok() {
        assert_eq!(parse(""), Depfile::default());
        assert_eq!(parse("\n"), Depfile::default());
    }

    #[test]
    fn missing_colon_is_an_error() {
        let e = parse_depfile(b"foo.o foo.c\n").unwrap_err();
        assert_eq!(e, "expected ':' in depfile");
    }

    #[test]
    fn input_as_target_is_rejected() {
        let e = parse_depfile(b"a: b\nb: c\nc: d\na: b\nb: e\n").unwrap_err();
        assert_eq!(e, "inputs may not also have inputs");
    }

    #[test]
    fn crlf() {
        let d = parse("a.o: a.c\r\nb.o: b.c\r\n");
        assert_eq!(d.outs, vec!["a.o", "b.o"]);
        assert_eq!(d.ins, vec!["a.c", "b.c"]);
    }

    #[test]
    fn no_trailing_newline() {
        let d = parse("a.o: a.c");
        assert_eq!(d.outs, vec!["a.o"]);
        assert_eq!(d.ins, vec!["a.c"]);
    }
}
