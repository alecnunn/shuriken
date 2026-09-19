//! Lexer for ninja manifests.
//!
//! A hand-written transcription of ninja's re2c-generated lexer
//! (`lexer.in.cc`), including its longest-match behaviour, its treatment of
//! `$`-escapes and line continuations, and its error message formatting.

use crate::error::{Error, Result};
use crate::eval::EvalString;

/// A lexical token.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Token {
    /// A byte that cannot start any token.
    Error,
    /// The `build` keyword.
    Build,
    /// `:`
    Colon,
    /// The `default` keyword.
    Default,
    /// `=`
    Equals,
    /// An identifier.
    Ident,
    /// The `include` keyword.
    Include,
    /// Leading whitespace, which introduces an indented binding.
    Indent,
    /// End of a statement.
    Newline,
    /// `|`
    Pipe,
    /// `||`
    Pipe2,
    /// `|@`
    PipeAt,
    /// The `pool` keyword.
    Pool,
    /// The `rule` keyword.
    Rule,
    /// The `subninja` keyword.
    Subninja,
    /// End of input.
    Eof,
}

impl Token {
    /// A human-readable name, as used in ninja's error messages.
    pub fn name(self) -> &'static str {
        match self {
            Token::Error => "lexing error",
            Token::Build => "'build'",
            Token::Colon => "':'",
            Token::Default => "'default'",
            Token::Equals => "'='",
            Token::Ident => "identifier",
            Token::Include => "'include'",
            Token::Indent => "indent",
            Token::Newline => "newline",
            Token::Pipe2 => "'||'",
            Token::Pipe => "'|'",
            Token::PipeAt => "'|@'",
            Token::Pool => "'pool'",
            Token::Rule => "'rule'",
            Token::Subninja => "'subninja'",
            Token::Eof => "eof",
        }
    }

    fn error_hint(self) -> &'static str {
        match self {
            Token::Colon => " ($ also escapes ':')",
            _ => "",
        }
    }
}

#[inline]
const fn is_varname_char(c: u8) -> bool {
    matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-')
}

#[inline]
const fn is_simple_varname_char(c: u8) -> bool {
    matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-')
}

/// A lexer over the contents of one manifest file.
pub struct Lexer<'a> {
    filename: String,
    input: &'a [u8],
    ofs: usize,
    last_token: Option<usize>,
}

impl<'a> Lexer<'a> {
    /// Start lexing `input`, reporting errors against `filename`.
    pub fn new(filename: impl Into<String>, input: &'a [u8]) -> Lexer<'a> {
        Lexer {
            filename: filename.into(),
            input,
            ofs: 0,
            last_token: None,
        }
    }

    /// The file being lexed.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Current byte offset.
    pub fn offset(&self) -> usize {
        self.ofs
    }

    /// Read one token.
    pub fn read_token(&mut self) -> Token {
        let input = self.input;
        let len = input.len();
        let mut p = self.ofs;
        let token;
        let start;

        'outer: loop {
            let s = p;

            // Leading spaces: shared prefix of the comment, newline and indent
            // rules.
            let mut q = p;
            while q < len && input[q] == b' ' {
                q += 1;
            }
            let nspaces = q - p;

            if q < len && input[q] == b'#' {
                // `[ ]*"#"[^\0\n]*"\n"` -- a comment, including its newline.
                let mut r = q + 1;
                while r < len && input[r] != b'\n' && input[r] != 0 {
                    r += 1;
                }
                if r < len && input[r] == b'\n' {
                    p = r + 1;
                    continue 'outer;
                }
                // No terminating newline: the comment rule does not match, so
                // fall back to the longest rule that does.
                if nspaces > 0 {
                    token = Token::Indent;
                    start = s;
                    p = q;
                    break 'outer;
                }
                token = Token::Error;
                start = q;
                p = q + 1;
                break 'outer;
            }

            if q + 1 < len && input[q] == b'\r' && input[q + 1] == b'\n' {
                token = Token::Newline;
                start = s;
                p = q + 2;
                break 'outer;
            }
            if q < len && input[q] == b'\n' {
                token = Token::Newline;
                start = s;
                p = q + 1;
                break 'outer;
            }
            if nspaces > 0 {
                token = Token::Indent;
                start = s;
                p = q;
                break 'outer;
            }

            // Identifiers and keywords. re2c prefers the keyword rules only
            // when they match at least as much as `varname` does, so match the
            // maximal identifier and compare.
            let mut r = q;
            while r < len && is_varname_char(input[r]) {
                r += 1;
            }
            if r > q {
                token = match &input[q..r] {
                    b"build" => Token::Build,
                    b"pool" => Token::Pool,
                    b"rule" => Token::Rule,
                    b"default" => Token::Default,
                    b"include" => Token::Include,
                    b"subninja" => Token::Subninja,
                    _ => Token::Ident,
                };
                start = q;
                p = r;
                break 'outer;
            }

            if q >= len {
                token = Token::Eof;
                start = q;
                p = q;
                break 'outer;
            }

            start = q;
            match input[q] {
                b'=' => {
                    token = Token::Equals;
                    p = q + 1;
                }
                b':' => {
                    token = Token::Colon;
                    p = q + 1;
                }
                b'|' if q + 1 < len && input[q + 1] == b'@' => {
                    token = Token::PipeAt;
                    p = q + 2;
                }
                b'|' if q + 1 < len && input[q + 1] == b'|' => {
                    token = Token::Pipe2;
                    p = q + 2;
                }
                b'|' => {
                    token = Token::Pipe;
                    p = q + 1;
                }
                0 => {
                    token = Token::Eof;
                    p = q + 1;
                }
                _ => {
                    token = Token::Error;
                    p = q + 1;
                }
            }
            break 'outer;
        }

        self.last_token = Some(start);
        self.ofs = p;
        if token != Token::Newline && token != Token::Eof {
            self.eat_whitespace();
        }
        token
    }

    /// Rewind to the start of the token most recently read.
    pub fn unread_token(&mut self) {
        if let Some(t) = self.last_token {
            self.ofs = t;
        }
    }

    /// If the next token is `token`, consume it and return true.
    pub fn peek_token(&mut self, token: Token) -> bool {
        if self.read_token() == token {
            true
        } else {
            self.unread_token();
            false
        }
    }

    /// Require the next token to be `expected`, producing ninja's
    /// "expected X, got Y" error otherwise.
    pub fn expect_token(&mut self, expected: Token) -> Result<()> {
        let token = self.read_token();
        if token == expected {
            return Ok(());
        }
        Err(self.error(&format!(
            "expected {}, got {}{}",
            expected.name(),
            token.name(),
            expected.error_hint()
        )))
    }

    /// Skip spaces and `$`-newline line continuations.
    fn eat_whitespace(&mut self) {
        let input = self.input;
        let len = input.len();
        let mut p = self.ofs;
        loop {
            if p < len && input[p] == b' ' {
                while p < len && input[p] == b' ' {
                    p += 1;
                }
                continue;
            }
            if p + 2 < len && input[p] == b'$' && input[p + 1] == b'\r' && input[p + 2] == b'\n' {
                p += 3;
                continue;
            }
            if p + 1 < len && input[p] == b'$' && input[p + 1] == b'\n' {
                p += 2;
                continue;
            }
            break;
        }
        self.ofs = p;
    }

    /// Read a simple identifier (a rule, pool or variable name).
    pub fn read_ident(&mut self) -> Option<String> {
        let input = self.input;
        let len = input.len();
        let start = self.ofs;
        let mut p = start;
        while p < len && is_varname_char(input[p]) {
            p += 1;
        }
        self.last_token = Some(start);
        if p == start {
            return None;
        }
        self.ofs = p;
        self.eat_whitespace();
        Some(String::from_utf8_lossy(&input[start..p]).into_owned())
    }

    /// Read a path, stopping (without consuming) at a delimiter. The returned
    /// path may be empty, which means a delimiter was hit immediately.
    pub fn read_path(&mut self, eval: &mut EvalString) -> Result<()> {
        self.read_eval_string(eval, true)
    }

    /// Read the value side of a `var = value` line.
    pub fn read_var_value(&mut self, eval: &mut EvalString) -> Result<()> {
        self.read_eval_string(eval, false)
    }

    fn read_eval_string(&mut self, eval: &mut EvalString, path: bool) -> Result<()> {
        let input = self.input;
        let len = input.len();
        let mut p = self.ofs;
        let start;

        'outer: loop {
            let s = p;
            if p >= len || input[p] == 0 {
                self.last_token = Some(s);
                return Err(self.error("unexpected EOF"));
            }

            match input[p] {
                b'$' => {
                    if p + 1 >= len {
                        self.last_token = Some(s);
                        return Err(self.error("bad $-escape (literal $ must be written as $$)"));
                    }
                    match input[p + 1] {
                        b'$' => {
                            eval.add_text("$");
                            p += 2;
                        }
                        b' ' => {
                            eval.add_text(" ");
                            p += 2;
                        }
                        b':' => {
                            eval.add_text(":");
                            p += 2;
                        }
                        b'\n' => {
                            p += 2;
                            while p < len && input[p] == b' ' {
                                p += 1;
                            }
                        }
                        b'\r' if p + 2 < len && input[p + 2] == b'\n' => {
                            p += 3;
                            while p < len && input[p] == b' ' {
                                p += 1;
                            }
                        }
                        b'{' => {
                            let mut r = p + 2;
                            while r < len && is_varname_char(input[r]) {
                                r += 1;
                            }
                            if r > p + 2 && r < len && input[r] == b'}' {
                                eval.add_var(&String::from_utf8_lossy(&input[p + 2..r]));
                                p = r + 1;
                            } else {
                                self.last_token = Some(s);
                                return Err(
                                    self.error("bad $-escape (literal $ must be written as $$)")
                                );
                            }
                        }
                        c if is_simple_varname_char(c) => {
                            let mut r = p + 1;
                            while r < len && is_simple_varname_char(input[r]) {
                                r += 1;
                            }
                            eval.add_var(&String::from_utf8_lossy(&input[p + 1..r]));
                            p = r;
                        }
                        _ => {
                            self.last_token = Some(s);
                            return Err(
                                self.error("bad $-escape (literal $ must be written as $$)")
                            );
                        }
                    }
                }
                b'\r' => {
                    if p + 1 < len && input[p + 1] == b'\n' {
                        if path {
                            p = s;
                        } else {
                            p += 2;
                        }
                        start = s;
                        break 'outer;
                    }
                    self.last_token = Some(s);
                    let msg = self.describe_last_error();
                    return Err(self.error(&msg));
                }
                c @ (b' ' | b':' | b'|' | b'\n') => {
                    if path {
                        p = s;
                        start = s;
                        break 'outer;
                    }
                    if c == b'\n' {
                        p += 1;
                        start = s;
                        break 'outer;
                    }
                    eval.add_text(match c {
                        b' ' => " ",
                        b':' => ":",
                        _ => "|",
                    });
                    p += 1;
                }
                _ => {
                    let mut r = p;
                    while r < len
                        && !matches!(input[r], b'$' | b' ' | b':' | b'\r' | b'\n' | b'|' | 0)
                    {
                        r += 1;
                    }
                    eval.add_text(&String::from_utf8_lossy(&input[p..r]));
                    p = r;
                }
            }
        }

        self.last_token = Some(start);
        self.ofs = p;
        if path {
            // Non-path strings end at a newline, so there is nothing to eat.
            self.eat_whitespace();
        }
        Ok(())
    }

    /// Extra detail for an `Error` token, matching ninja's
    /// `DescribeLastError`.
    pub fn describe_last_error(&self) -> String {
        if let Some(t) = self.last_token {
            if t < self.input.len() && self.input[t] == b'\t' {
                return "tabs are not allowed, use spaces".to_string();
            }
        }
        "lexing error".to_string()
    }

    /// Build a positioned error message in ninja's format.
    pub fn error(&self, message: &str) -> Error {
        Error::Manifest(self.error_string(message))
    }

    /// The message body of [`Lexer::error`].
    pub fn error_string(&self, message: &str) -> String {
        let mut line = 1usize;
        let mut line_start = 0usize;
        let last = self.last_token.unwrap_or(0);
        for p in 0..last.min(self.input.len()) {
            if self.input[p] == b'\n' {
                line += 1;
                line_start = p + 1;
            }
        }
        let col = match self.last_token {
            Some(t) => t.saturating_sub(line_start),
            None => 0,
        };

        let mut out = format!("{}:{}: {}\n", self.filename, line, message);

        const TRUNCATE_COLUMN: usize = 72;
        if col > 0 && col < TRUNCATE_COLUMN {
            let mut truncated = true;
            let mut n = 0usize;
            while n < TRUNCATE_COLUMN {
                let i = line_start + n;
                if i >= self.input.len() || self.input[i] == 0 || self.input[i] == b'\n' {
                    truncated = false;
                    break;
                }
                n += 1;
            }
            out.push_str(&String::from_utf8_lossy(
                &self.input[line_start..line_start + n],
            ));
            if truncated {
                out.push_str("...");
            }
            out.push('\n');
            for _ in 0..col {
                out.push(' ');
            }
            out.push_str("^ near here");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: &str) -> Vec<Token> {
        let mut lexer = Lexer::new("input", input.as_bytes());
        let mut out = Vec::new();
        loop {
            let t = lexer.read_token();
            out.push(t);
            if t == Token::Eof || t == Token::Error {
                break;
            }
        }
        out
    }

    #[test]
    fn empty_input() {
        assert_eq!(tokens(""), vec![Token::Eof]);
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(tokens("# comment\n"), vec![Token::Eof]);
        assert_eq!(tokens("   # indented comment\n"), vec![Token::Eof]);
        // A comment without a trailing newline cannot match, so the '#' is an
        // error at column 0 (matching ninja).
        assert_eq!(tokens("# no newline"), vec![Token::Error]);
    }

    #[test]
    fn keywords_and_idents() {
        assert_eq!(
            tokens("build rule pool default include subninja\n"),
            vec![
                Token::Build,
                Token::Rule,
                Token::Pool,
                Token::Default,
                Token::Include,
                Token::Subninja,
                Token::Newline,
                Token::Eof
            ]
        );
        // Longest-match: `builder` is an identifier, not `build` + `er`.
        assert_eq!(
            tokens("builder\n"),
            vec![Token::Ident, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn punctuation() {
        assert_eq!(
            tokens("a: b | c || d |@ e\n"),
            vec![
                Token::Ident,
                Token::Colon,
                Token::Ident,
                Token::Pipe,
                Token::Ident,
                Token::Pipe2,
                Token::Ident,
                Token::PipeAt,
                Token::Ident,
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn indent_vs_newline() {
        // Trailing spaces before a newline lex as NEWLINE, not INDENT.
        assert_eq!(tokens("   \n"), vec![Token::Newline, Token::Eof]);
        assert_eq!(
            tokens("  x\n"),
            vec![Token::Indent, Token::Ident, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn tabs_are_rejected() {
        let mut lexer = Lexer::new("input", b"\tfoo");
        assert_eq!(lexer.read_token(), Token::Error);
        assert_eq!(
            lexer.describe_last_error(),
            "tabs are not allowed, use spaces"
        );
    }

    fn read_value(input: &str) -> Result<String> {
        let mut lexer = Lexer::new("input", input.as_bytes());
        let mut eval = EvalString::new();
        lexer.read_var_value(&mut eval)?;
        Ok(eval.serialize())
    }

    fn read_path(input: &str) -> Result<String> {
        let mut lexer = Lexer::new("input", input.as_bytes());
        let mut eval = EvalString::new();
        lexer.read_path(&mut eval)?;
        Ok(eval.serialize())
    }

    #[test]
    fn var_values() {
        assert_eq!(read_value("plain text\n").unwrap(), "[plain text]");
        assert_eq!(read_value("$foo\n").unwrap(), "[$foo]");
        assert_eq!(read_value("${foo.bar}\n").unwrap(), "[$foo.bar]");
        assert_eq!(read_value("a$$b\n").unwrap(), "[a$b]");
        assert_eq!(read_value("a$ b\n").unwrap(), "[a b]");
        assert_eq!(read_value("a$:b\n").unwrap(), "[a:b]");
        assert_eq!(read_value("a: b|c\n").unwrap(), "[a: b|c]");
        // Line continuation swallows the newline and following indentation.
        assert_eq!(read_value("a$\n   b\n").unwrap(), "[ab]");
        assert_eq!(read_value("a$\r\n   b\r\n").unwrap(), "[ab]");
    }

    #[test]
    fn bad_escapes() {
        let e = read_value("a$*b\n").unwrap_err();
        assert!(e.to_string().contains("bad $-escape"), "{e}");
        let e = read_value("a").unwrap_err();
        assert!(e.to_string().contains("unexpected EOF"), "{e}");
    }

    #[test]
    fn paths_stop_at_delimiters() {
        assert_eq!(read_path("foo bar\n").unwrap(), "[foo]");
        assert_eq!(read_path("foo:bar\n").unwrap(), "[foo]");
        assert_eq!(read_path("foo|bar\n").unwrap(), "[foo]");
        assert_eq!(read_path("fo$ o bar\n").unwrap(), "[fo o]");
        assert_eq!(read_path("fo$:o bar\n").unwrap(), "[fo:o]");
        // An immediate delimiter yields an empty path.
        assert_eq!(read_path(": foo\n").unwrap(), "");
    }

    #[test]
    fn error_message_format() {
        let mut lexer = Lexer::new("build.ninja", b"x = 1\nbuild y\n");
        // Position the lexer at the second line's 'build'.
        lexer.read_token();
        lexer.read_token();
        lexer.read_token();
        lexer.read_token();
        let t = lexer.read_token();
        assert_eq!(t, Token::Build);
        let msg = lexer.error_string("oops");
        assert!(msg.starts_with("build.ninja:2: oops\n"), "{msg}");
    }
}
