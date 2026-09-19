//! Shell escaping and terminal-output helpers, matching ninja's behaviour.

/// Characters ninja considers safe to leave unquoted in a POSIX shell command.
#[inline]
const fn is_shell_safe(c: u8) -> bool {
    matches!(c, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'+' | b'-' | b'.' | b'/')
}

#[inline]
const fn is_win32_safe(c: u8) -> bool {
    !matches!(c, b' ' | b'"')
}

/// Append `input`, quoted for `/bin/sh` if necessary, to `out`.
pub fn append_shell_escaped(input: &str, out: &mut String) {
    if input.bytes().all(is_shell_safe) {
        out.push_str(input);
        return;
    }
    out.push('\'');
    for c in input.chars() {
        if c == '\'' {
            // Close the quote, emit an escaped quote, reopen.
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
}

/// Append `input`, quoted per the Windows `CommandLineToArgvW` rules if
/// necessary, to `out`.
pub fn append_win32_escaped(input: &str, out: &mut String) {
    if input.bytes().all(is_win32_safe) {
        out.push_str(input);
        return;
    }
    out.push('"');
    let mut consecutive_backslashes = 0usize;
    for c in input.chars() {
        match c {
            '\\' => {
                consecutive_backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Double the run of backslashes preceding the quote, then
                // escape the quote itself.
                for _ in 0..=consecutive_backslashes {
                    out.push('\\');
                }
                out.push('"');
                consecutive_backslashes = 0;
            }
            _ => {
                consecutive_backslashes = 0;
                out.push(c);
            }
        }
    }
    // Trailing backslashes would escape our closing quote.
    for _ in 0..consecutive_backslashes {
        out.push('\\');
    }
    out.push('"');
}

/// Escape a path for the host platform's command line.
pub fn append_escaped_for_host(input: &str, out: &mut String) {
    if cfg!(windows) {
        append_win32_escaped(input, out);
    } else {
        append_shell_escaped(input, out);
    }
}

/// Escape a path for the host platform's command line.
pub fn escaped_for_host(input: &str) -> String {
    let mut s = String::with_capacity(input.len() + 2);
    append_escaped_for_host(input, &mut s);
    s
}

/// Remove ANSI escape sequences from `input`, like ninja's
/// `StripAnsiEscapeCodes`.
pub fn strip_ansi_escape_codes(input: &str) -> String {
    let b = input.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            out.push(b[i]);
            i += 1;
            continue;
        }
        // Only strip CSI sequences (ESC '[' ... final-byte).
        if i + 1 >= b.len() || b[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        while j < b.len() && !b[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j == b.len() {
            break;
        }
        i = j + 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Iterate over the ANSI colour sequences (`ESC [ params m`) in a string.
struct AnsiColorSequences<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> AnsiColorSequences<'a> {
    fn new(input: &'a [u8]) -> Self {
        AnsiColorSequences { input, pos: 0 }
    }
}

impl<'a> Iterator for AnsiColorSequences<'a> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        let b = self.input;
        let mut from = self.pos;
        loop {
            let seq = b[from..].iter().position(|&c| c == 0x1b)? + from;
            // The shortest possible colour sequence is "\x1b[0m" (4 bytes).
            if seq + 4 > b.len() {
                return None;
            }
            if b[seq + 1] != b'[' {
                from = seq + 1;
                continue;
            }
            let mut end = seq + 2;
            while end < b.len() && (b[end].is_ascii_digit() || b[end] == b';') {
                end += 1;
            }
            if end >= b.len() {
                return None; // Incomplete sequence.
            }
            if b[end] != b'm' {
                from = seq + 3;
                continue;
            }
            self.pos = end + 1;
            return Some((seq, end + 1));
        }
    }
}

/// Shorten `s` to at most `max_width` visible columns by replacing the middle
/// with "...", preserving ANSI colour sequences. Mirrors ninja's
/// `ElideMiddleInPlace`.
pub fn elide_middle_in_place(s: &mut String, max_width: usize) {
    if s.len() <= max_width {
        return;
    }

    if !s.as_bytes().contains(&0x1b) {
        const ELLIPSIS: usize = 3;
        if max_width <= ELLIPSIS {
            s.clear();
            s.push_str(&"..."[..max_width]);
            return;
        }
        let remaining = max_width - ELLIPSIS;
        let left = remaining / 2;
        let right = remaining - left;
        let mut out = Vec::with_capacity(max_width);
        out.extend_from_slice(&s.as_bytes()[..left]);
        out.extend_from_slice(b"...");
        out.extend_from_slice(&s.as_bytes()[s.len() - right..]);
        *s = String::from_utf8_lossy(&out).into_owned();
        return;
    }

    let bytes = s.as_bytes().to_vec();
    let sequences: Vec<(usize, usize)> = AnsiColorSequences::new(&bytes).collect();
    let invisible: usize = sequences.iter().map(|(a, b)| b - a).sum();
    let visible_width = bytes.len() - invisible;
    if visible_width <= max_width {
        return;
    }

    let ellipsis_width = max_width.min(3);
    let visible_left = (max_width - ellipsis_width) / 2;
    let visible_right = (max_width - ellipsis_width) - visible_left;
    let visible_gap_start = visible_left;
    let visible_gap_end = visible_width - visible_right;

    // Walk the input tracking visible position.
    let is_visible = |i: usize| !sequences.iter().any(|&(a, b)| i >= a && i < b);

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut visible_pos = 0usize;
    let mut i = 0usize;

    // Left span.
    while i < bytes.len() {
        if visible_pos == visible_gap_start {
            break;
        }
        if is_visible(i) {
            visible_pos += 1;
        }
        i += 1;
    }
    out.extend_from_slice(&bytes[..i]);
    out.extend_from_slice(&b"..."[..ellipsis_width]);

    // Skip the gap, but keep any colour sequences inside it so that colours
    // after the ellipsis stay correct.
    while i < bytes.len() {
        if visible_pos == visible_gap_end {
            break;
        }
        if is_visible(i) {
            visible_pos += 1;
            i += 1;
        } else {
            let seq = sequences.iter().find(|&&(a, b)| i >= a && i < b).copied();
            match seq {
                Some((a, b)) => {
                    out.extend_from_slice(&bytes[a..b]);
                    i = b;
                }
                None => i += 1,
            }
        }
    }

    out.extend_from_slice(&bytes[i..]);
    *s = String::from_utf8_lossy(&out).into_owned();
}

/// JSON-encode a string, matching ninja's `EncodeJSONString`.
pub fn encode_json_string(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(input.len() + input.len() / 5);
    for c in input.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                out.push(HEX[(c as usize) >> 4] as char);
                out.push(HEX[(c as usize) & 0xf] as char);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(s: &str) -> String {
        let mut out = String::new();
        append_shell_escaped(s, &mut out);
        out
    }

    #[test]
    fn shell_escaping() {
        // ninja leaves the empty string alone: it has no unsafe characters.
        assert_eq!(sh(""), "");
        assert_eq!(sh("plain"), "plain");
        assert_eq!(sh("/usr/bin/gcc-9.2"), "/usr/bin/gcc-9.2");
        assert_eq!(sh("with space"), "'with space'");
        assert_eq!(sh("hi'there"), "'hi'\\''there'");
        assert_eq!(sh("$PATH"), "'$PATH'");
    }

    #[test]
    fn win32_escaping() {
        let mut out = String::new();
        append_win32_escaped("plain", &mut out);
        assert_eq!(out, "plain");
        out.clear();
        append_win32_escaped(r"a b", &mut out);
        assert_eq!(out, r#""a b""#);
        out.clear();
        append_win32_escaped(r#"a"b"#, &mut out);
        assert_eq!(out, r#""a\"b""#);
        out.clear();
        append_win32_escaped(r#"a\"#, &mut out);
        assert_eq!(out, r#"a\"#);
        out.clear();
        append_win32_escaped(r#"a b\"#, &mut out);
        assert_eq!(out, r#""a b\\""#);
    }

    #[test]
    fn elide() {
        let mut s = "hello".to_string();
        elide_middle_in_place(&mut s, 10);
        assert_eq!(s, "hello");

        let mut s = "0123456789".to_string();
        elide_middle_in_place(&mut s, 7);
        assert_eq!(s, "01...89");

        let mut s = "0123456789".to_string();
        elide_middle_in_place(&mut s, 3);
        assert_eq!(s, "...");

        let mut s = "0123456789".to_string();
        elide_middle_in_place(&mut s, 1);
        assert_eq!(s, ".");
    }

    #[test]
    fn elide_keeps_color() {
        let mut s = "\x1b[31m0123456789\x1b[0m".to_string();
        elide_middle_in_place(&mut s, 7);
        assert!(s.starts_with("\x1b[31m"));
        assert!(s.ends_with("\x1b[0m"));
        assert!(s.contains("..."));
    }

    #[test]
    fn strip_ansi() {
        assert_eq!(strip_ansi_escape_codes("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi_escape_codes("plain"), "plain");
    }

    #[test]
    fn json() {
        assert_eq!(encode_json_string("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(encode_json_string("\u{1}"), "\\u0001");
    }
}
