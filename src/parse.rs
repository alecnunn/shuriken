//! The `.ninja` manifest parser.

use crate::canon::canonicalize_path;
use crate::disk::DiskInterface;
use crate::error::{Error, Result};
use crate::eval::{EvalString, Rule, ScopeEnv, ScopeId};
use crate::lexer::{Lexer, Token};
use crate::state::{Pool, State};
use crate::version::check_required_version;

/// What to do about build statements of the form `build a: phony ... a ...`,
/// which old CMake versions emitted.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum PhonyCycleAction {
    /// Drop the self-reference and warn (ninja's default).
    #[default]
    Warn,
    /// Treat it as an error.
    Error,
}

/// Parser configuration.
#[derive(Clone, Debug, Default)]
pub struct ParserOptions {
    /// How to treat self-referencing phony edges.
    pub phony_cycle_action: PhonyCycleAction,
    /// Suppress warnings (used by tests).
    pub quiet: bool,
}

/// Parses manifests into a [`State`].
pub struct ManifestParser<'a> {
    state: &'a mut State,
    disk: &'a dyn DiskInterface,
    options: ParserOptions,
    warnings: Vec<String>,
    /// Reused across build statements so that parsing a large manifest does
    /// not allocate a fresh buffer per path.
    outs: PathList,
    ins: PathList,
    validations: PathList,
}

/// A growable pool of [`EvalString`]s used as a scratch list of paths.
///
/// Entries are cleared and reused rather than dropped, which keeps their string
/// allocations alive for the next build statement.
#[derive(Default)]
struct PathList {
    items: Vec<EvalString>,
    len: usize,
}

impl PathList {
    fn reset(&mut self) {
        self.len = 0;
    }

    /// Read one path into the list. Returns false when a delimiter was hit
    /// instead (an empty path), in which case nothing was added.
    fn read(&mut self, lexer: &mut Lexer<'_>) -> Result<bool> {
        if self.len == self.items.len() {
            self.items.push(EvalString::new());
        }
        let slot = &mut self.items[self.len];
        slot.clear();
        lexer.read_path(slot)?;
        if slot.is_empty() {
            return Ok(false);
        }
        self.len += 1;
        Ok(true)
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn get(&self, i: usize) -> &EvalString {
        &self.items[i]
    }
}

impl<'a> ManifestParser<'a> {
    /// Create a parser that fills in `state`, reading files through `disk`.
    pub fn new(
        state: &'a mut State,
        disk: &'a dyn DiskInterface,
        options: ParserOptions,
    ) -> ManifestParser<'a> {
        ManifestParser {
            state,
            disk,
            options,
            warnings: Vec::new(),
            outs: PathList::default(),
            ins: PathList::default(),
            validations: PathList::default(),
        }
    }

    /// Warnings produced while parsing (e.g. version or phony-cycle warnings).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Take the accumulated warnings.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Load and parse `filename` into the root scope.
    pub fn load(&mut self, filename: &str) -> Result<()> {
        let scope = self.state.root_scope();
        self.load_file(filename, scope, None)
    }

    /// Parse manifest text directly, without touching the filesystem.
    pub fn parse_text(&mut self, filename: &str, input: &[u8]) -> Result<()> {
        let scope = self.state.root_scope();
        self.parse(filename, input, scope)
    }

    fn load_file(
        &mut self,
        filename: &str,
        scope: ScopeId,
        parent: Option<&Lexer<'_>>,
    ) -> Result<()> {
        let contents = match self.disk.read_file(filename) {
            Ok(Some(c)) => c,
            Ok(None) => {
                let msg = format!("loading '{filename}': No such file or directory");
                return Err(match parent {
                    Some(p) => Error::Manifest(p.error_string(&msg)),
                    None => Error::Manifest(msg),
                });
            }
            Err(e) => {
                let msg = format!("loading '{filename}': {e}");
                return Err(match parent {
                    Some(p) => Error::Manifest(p.error_string(&msg)),
                    None => Error::Manifest(msg),
                });
            }
        };
        self.parse(filename, &contents, scope)
    }

    fn parse(&mut self, filename: &str, input: &[u8], scope: ScopeId) -> Result<()> {
        let mut lexer = Lexer::new(filename, input);
        loop {
            let token = lexer.read_token();
            match token {
                Token::Pool => self.parse_pool(&mut lexer, scope)?,
                Token::Build => self.parse_edge(&mut lexer, scope)?,
                Token::Rule => self.parse_rule(&mut lexer, scope)?,
                Token::Default => self.parse_default(&mut lexer, scope)?,
                Token::Ident => {
                    lexer.unread_token();
                    let (name, let_value) = self.parse_let(&mut lexer)?;
                    let value = {
                        let mut env = ScopeEnv::new(&self.state.scopes, scope);
                        let_value.evaluate(&mut env)
                    };
                    // Check the required version immediately, before we can
                    // hit any syntax the manifest expects us to understand.
                    if name == "ninja_required_version" {
                        match check_required_version(&value) {
                            Ok(Some(warning)) => self.warn(warning),
                            Ok(None) => {}
                            Err(e) => return Err(Error::Fatal(e)),
                        }
                    }
                    self.state.scopes.add_binding(scope, name, value);
                }
                Token::Include => self.parse_file_include(&mut lexer, scope, false)?,
                Token::Subninja => self.parse_file_include(&mut lexer, scope, true)?,
                Token::Error => {
                    let msg = lexer.describe_last_error();
                    return Err(lexer.error(&msg));
                }
                Token::Eof => return Ok(()),
                Token::Newline => {}
                other => {
                    return Err(lexer.error(&format!("unexpected {}", other.name())));
                }
            }
        }
    }

    fn warn(&mut self, message: String) {
        if !self.options.quiet {
            self.warnings.push(message);
        }
    }

    fn parse_let(&mut self, lexer: &mut Lexer<'_>) -> Result<(String, EvalString)> {
        let key = match lexer.read_ident() {
            Some(k) => k,
            None => return Err(lexer.error("expected variable name")),
        };
        lexer.expect_token(Token::Equals)?;
        let mut value = EvalString::new();
        lexer.read_var_value(&mut value)?;
        Ok((key, value))
    }

    fn parse_pool(&mut self, lexer: &mut Lexer<'_>, scope: ScopeId) -> Result<()> {
        let name = match lexer.read_ident() {
            Some(n) => n,
            None => return Err(lexer.error("expected pool name")),
        };
        lexer.expect_token(Token::Newline)?;

        if self.state.lookup_pool(&name).is_some() {
            return Err(lexer.error(&format!("duplicate pool '{name}'")));
        }

        let mut depth: i32 = -1;
        while lexer.peek_token(Token::Indent) {
            let (key, value) = self.parse_let(lexer)?;
            if key == "depth" {
                let text = {
                    let mut env = ScopeEnv::new(&self.state.scopes, scope);
                    value.evaluate(&mut env)
                };
                depth = parse_leading_int(&text);
                if depth < 0 {
                    return Err(lexer.error("invalid pool depth"));
                }
            } else {
                return Err(lexer.error(&format!("unexpected variable '{key}'")));
            }
        }

        if depth < 0 {
            return Err(lexer.error("expected 'depth =' line"));
        }

        self.state
            .add_pool(Pool::new(name, depth))
            .map_err(|e| lexer.error(&e))?;
        Ok(())
    }

    fn parse_rule(&mut self, lexer: &mut Lexer<'_>, scope: ScopeId) -> Result<()> {
        let name = match lexer.read_ident() {
            Some(n) => n,
            None => return Err(lexer.error("expected rule name")),
        };
        lexer.expect_token(Token::Newline)?;

        if self
            .state
            .scopes
            .lookup_rule_current_scope(scope, &name)
            .is_some()
        {
            return Err(lexer.error(&format!("duplicate rule '{name}'")));
        }

        let mut rule = Rule::new(name);
        while lexer.peek_token(Token::Indent) {
            let (key, value) = self.parse_let(lexer)?;
            if Rule::is_reserved_binding(&key) {
                rule.add_binding(key, value);
            } else {
                return Err(lexer.error(&format!("unexpected variable '{key}'")));
            }
        }

        let has_rspfile = rule
            .binding("rspfile")
            .map(|e| !e.is_empty())
            .unwrap_or(false);
        let has_rspfile_content = rule
            .binding("rspfile_content")
            .map(|e| !e.is_empty())
            .unwrap_or(false);
        if has_rspfile != has_rspfile_content {
            return Err(lexer.error("rspfile and rspfile_content need to be both specified"));
        }

        if !rule
            .binding("command")
            .map(|e| !e.is_empty())
            .unwrap_or(false)
        {
            return Err(lexer.error("expected 'command =' line"));
        }

        self.state.scopes.add_rule(scope, rule);
        Ok(())
    }

    fn parse_default(&mut self, lexer: &mut Lexer<'_>, scope: ScopeId) -> Result<()> {
        let mut eval = EvalString::new();
        lexer.read_path(&mut eval)?;
        if eval.is_empty() {
            return Err(lexer.error("expected target name"));
        }

        loop {
            let mut path = {
                let mut env = ScopeEnv::new(&self.state.scopes, scope);
                eval.evaluate(&mut env)
            };
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            canonicalize_path(&mut path);
            if let Err(e) = self.state.add_default(&path) {
                return Err(lexer.error(&e));
            }

            eval.clear();
            lexer.read_path(&mut eval)?;
            if eval.is_empty() {
                break;
            }
        }

        lexer.expect_token(Token::Newline)
    }

    fn parse_file_include(
        &mut self,
        lexer: &mut Lexer<'_>,
        scope: ScopeId,
        new_scope: bool,
    ) -> Result<()> {
        let mut eval = EvalString::new();
        lexer.read_path(&mut eval)?;
        let path = {
            let mut env = ScopeEnv::new(&self.state.scopes, scope);
            eval.evaluate(&mut env)
        };

        let sub_scope = if new_scope {
            self.state.scopes.new_child(scope)
        } else {
            scope
        };

        self.load_file(&path, sub_scope, Some(lexer))?;
        lexer.expect_token(Token::Newline)
    }

    fn parse_edge(&mut self, lexer: &mut Lexer<'_>, scope: ScopeId) -> Result<()> {
        // Move the scratch buffers out so they can be borrowed independently of
        // `self.state`; they go back when we are done.
        let mut outs = std::mem::take(&mut self.outs);
        let mut ins = std::mem::take(&mut self.ins);
        let mut validations = std::mem::take(&mut self.validations);
        let result = self.parse_edge_inner(lexer, scope, &mut outs, &mut ins, &mut validations);
        self.outs = outs;
        self.ins = ins;
        self.validations = validations;
        result
    }

    fn parse_edge_inner(
        &mut self,
        lexer: &mut Lexer<'_>,
        scope: ScopeId,
        outs: &mut PathList,
        ins: &mut PathList,
        validations: &mut PathList,
    ) -> Result<()> {
        outs.reset();
        ins.reset();
        validations.reset();

        // Explicit outputs.
        while outs.read(lexer)? {}

        // Implicit outputs.
        let explicit_outs = outs.len();
        if lexer.peek_token(Token::Pipe) {
            while outs.read(lexer)? {}
        }
        let implicit_outs = outs.len() - explicit_outs;

        if outs.is_empty() {
            return Err(lexer.error("expected path"));
        }

        lexer.expect_token(Token::Colon)?;

        let rule_name = match lexer.read_ident() {
            Some(n) => n,
            None => return Err(lexer.error("expected build command name")),
        };
        let rule = match self.state.scopes.lookup_rule(scope, &rule_name) {
            Some(r) => r,
            None => return Err(lexer.error(&format!("unknown build rule '{rule_name}'"))),
        };

        // Explicit inputs.
        while ins.read(lexer)? {}
        let explicit_ins = ins.len();

        // Implicit inputs.
        if lexer.peek_token(Token::Pipe) {
            while ins.read(lexer)? {}
        }
        let implicit = ins.len() - explicit_ins;

        // Order-only inputs.
        if lexer.peek_token(Token::Pipe2) {
            while ins.read(lexer)? {}
        }
        let order_only = ins.len() - explicit_ins - implicit;

        // Validations.
        if lexer.peek_token(Token::PipeAt) {
            while validations.read(lexer)? {}
        }

        lexer.expect_token(Token::Newline)?;

        // Edge bindings. Their values are expanded in the *enclosing* scope,
        // but paths below are expanded in the edge's own scope.
        let mut has_indent = lexer.peek_token(Token::Indent);
        let edge_scope = if has_indent {
            self.state.scopes.new_child(scope)
        } else {
            scope
        };
        while has_indent {
            let (key, val) = self.parse_let(lexer)?;
            let value = {
                let mut env = ScopeEnv::new(&self.state.scopes, scope);
                val.evaluate(&mut env)
            };
            self.state.scopes.add_binding(edge_scope, key, value);
            has_indent = lexer.peek_token(Token::Indent);
        }

        let edge = self.state.add_edge(rule, edge_scope);

        let pool_name = self.state.edge_binding(edge, "pool");
        if !pool_name.is_empty() {
            match self.state.lookup_pool(&pool_name) {
                Some(p) => self.state.edge_mut(edge).pool = p,
                None => {
                    self.state.pop_edge();
                    return Err(lexer.error(&format!("unknown pool name '{pool_name}'")));
                }
            }
        }

        for i in 0..outs.len() {
            let mut path = {
                let mut env = ScopeEnv::new(&self.state.scopes, edge_scope);
                outs.get(i).evaluate(&mut env)
            };
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            let slash_bits = canonicalize_path(&mut path);
            if let Err(e) = self.state.add_out(edge, &path, slash_bits) {
                return Err(lexer.error(&e));
            }
        }
        self.state.edge_mut(edge).implicit_outs = implicit_outs;

        for i in 0..ins.len() {
            let mut path = {
                let mut env = ScopeEnv::new(&self.state.scopes, edge_scope);
                ins.get(i).evaluate(&mut env)
            };
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            let slash_bits = canonicalize_path(&mut path);
            self.state.add_in(edge, &path, slash_bits);
        }
        self.state.edge_mut(edge).implicit_deps = implicit;
        self.state.edge_mut(edge).order_only_deps = order_only;

        for i in 0..validations.len() {
            let mut path = {
                let mut env = ScopeEnv::new(&self.state.scopes, edge_scope);
                validations.get(i).evaluate(&mut env)
            };
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            let slash_bits = canonicalize_path(&mut path);
            self.state.add_validation(edge, &path, slash_bits);
        }

        // CMake 2.8.12.x/3.0.x wrote self-referencing phony statements; ninja
        // filters them out with a warning by default.
        let is_phony = self.state.edge_is_phony(edge);
        if self.options.phony_cycle_action == PhonyCycleAction::Warn
            && self.state.edge(edge).maybe_phonycycle_diagnostic(is_phony)
        {
            let out = self.state.edge(edge).outputs()[0];
            let had = self.state.edge(edge).inputs().contains(&out);
            if had {
                self.state.edge_mut(edge).inputs.retain(|&i| i != out);
                let path = self.state.node(out).path().to_string();
                self.warn(format!(
                    "phony target '{path}' names itself as an input; ignoring [-w phonycycle=warn]"
                ));
            }
        }

        // A `dyndep` binding must name one of the edge's inputs.
        let dyndep = self.state.edge_dyndep_binding(edge);
        if !dyndep.is_empty() {
            let mut path = dyndep.clone();
            let slash_bits = canonicalize_path(&mut path);
            let node = self.state.get_node(&path, slash_bits);
            self.state.node_mut(node).dyndep_pending = true;
            if !self.state.edge(edge).inputs().contains(&node) {
                return Err(lexer.error(&format!("dyndep '{dyndep}' is not an input")));
            }
            self.state.edge_mut(edge).dyndep = Some(node);
        }

        Ok(())
    }
}

/// `atoi`-style parse: leading digits with optional sign, 0 when unparseable.
fn parse_leading_int(s: &str) -> i32 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::eval::ROOT_SCOPE;

    fn parse(text: &str) -> Result<State> {
        let disk = MemDisk::new();
        let mut state = State::new();
        {
            let mut p = ManifestParser::new(
                &mut state,
                &disk,
                ParserOptions {
                    quiet: true,
                    ..Default::default()
                },
            );
            p.parse_text("input", text.as_bytes())?;
        }
        Ok(state)
    }

    #[test]
    fn empty_manifest() {
        let s = parse("").unwrap();
        assert_eq!(s.edges().len(), 0);
    }

    #[test]
    fn simple_rule_and_edge() {
        let s = parse("rule cat\n  command = cat $in > $out\n\nbuild out: cat in1 in2\n").unwrap();
        assert_eq!(s.edges().len(), 1);
        let e = crate::state::EdgeId(0);
        assert_eq!(s.edge_command(e), "cat in1 in2 > out");
        assert_eq!(s.edge(e).inputs().len(), 2);
        assert_eq!(s.edge(e).outputs().len(), 1);
    }

    #[test]
    fn variables_and_scoping() {
        let s = parse(
            "cflags = -Wall\n\
             rule cc\n  command = gcc $cflags -c $in -o $out\n\n\
             build a.o: cc a.c\n\
             build b.o: cc b.c\n  cflags = -O2\n",
        )
        .unwrap();
        assert_eq!(
            s.edge_command(crate::state::EdgeId(0)),
            "gcc -Wall -c a.c -o a.o"
        );
        assert_eq!(
            s.edge_command(crate::state::EdgeId(1)),
            "gcc -O2 -c b.c -o b.o"
        );
    }

    #[test]
    fn rule_binding_sees_edge_scope() {
        // `$extra` is defined on the edge and referenced from the rule.
        let s = parse(
            "rule cc\n  command = gcc $extra -c $in -o $out\n\n\
             build a.o: cc a.c\n  extra = -g\n",
        )
        .unwrap();
        assert_eq!(
            s.edge_command(crate::state::EdgeId(0)),
            "gcc -g -c a.c -o a.o"
        );
    }

    #[test]
    fn implicit_and_order_only() {
        let s = parse(
            "rule cat\n  command = cat $in > $out\n\n\
             build out | imp_out: cat in | imp_in || oo_in\n",
        )
        .unwrap();
        let e = s.edge(crate::state::EdgeId(0));
        assert_eq!(e.outputs().len(), 2);
        assert_eq!(e.implicit_outs(), 1);
        assert_eq!(e.inputs().len(), 3);
        assert_eq!(e.implicit_deps(), 1);
        assert_eq!(e.order_only_deps(), 1);
        assert_eq!(e.explicit_deps(), 1);
        assert!(e.is_implicit(1));
        assert!(e.is_order_only(2));
        // $in only contains explicit inputs.
        assert_eq!(s.edge_command(crate::state::EdgeId(0)), "cat in > out");
    }

    #[test]
    fn validations() {
        let s = parse(
            "rule cat\n  command = cat $in > $out\n\n\
             build out: cat in |@ validate\n",
        )
        .unwrap();
        assert_eq!(s.edge(crate::state::EdgeId(0)).validations().len(), 1);
        let v = s.lookup_node("validate").unwrap();
        assert_eq!(s.node(v).validation_out_edges().len(), 1);
    }

    #[test]
    fn shell_escaping_in_command() {
        let s =
            parse("rule cat\n  command = cat $in > $out\n\nbuild out put: cat in$ put\n").unwrap();
        assert_eq!(
            s.edge_command(crate::state::EdgeId(0)),
            "cat 'in put' > out put"
        );
    }

    #[test]
    fn pools() {
        let s = parse(
            "pool link_pool\n  depth = 3\n\n\
             rule link\n  command = ld $in -o $out\n  pool = link_pool\n\n\
             build out: link in\n",
        )
        .unwrap();
        let p = s.lookup_pool("link_pool").unwrap();
        assert_eq!(s.pool(p).depth(), 3);
        assert_eq!(s.edge(crate::state::EdgeId(0)).pool(), p);
    }

    #[test]
    fn default_targets() {
        let s = parse(
            "rule cat\n  command = cat $in > $out\n\n\
             build a: cat i\nbuild b: cat i\ndefault b\n",
        )
        .unwrap();
        assert_eq!(s.defaults().len(), 1);
        assert_eq!(s.node(s.defaults()[0]).path(), "b");
    }

    #[test]
    fn errors() {
        let cases = [
            ("build\n", "expected path"),
            ("build x: nope\n", "unknown build rule 'nope'"),
            ("rule r\n", "expected 'command =' line"),
            (
                "rule r\n  command = x\n  bogus = y\n",
                "unexpected variable 'bogus'",
            ),
            ("pool p\n", "expected 'depth =' line"),
            ("pool p\n  depth = -1\n", "invalid pool depth"),
            ("default nonexistent\n", "unknown target 'nonexistent'"),
            (
                "rule r\n  command = x\n  rspfile = y\n",
                "rspfile and rspfile_content need to be both specified",
            ),
            ("x = 1\nx$ = 2\n", "expected '=', got"),
        ];
        for (text, expected) in cases {
            let err = parse(text).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "for {text:?}: expected {expected:?}, got {err}"
            );
        }
    }

    #[test]
    fn duplicate_rule_and_pool() {
        let e = parse("rule r\n  command = x\nrule r\n  command = y\n").unwrap_err();
        assert!(e.to_string().contains("duplicate rule 'r'"), "{e}");
        let e = parse("pool p\n  depth = 1\npool p\n  depth = 2\n").unwrap_err();
        assert!(e.to_string().contains("duplicate pool 'p'"), "{e}");
    }

    #[test]
    fn multiple_rules_generate() {
        let e = parse("rule cat\n  command = cat $in > $out\n\nbuild a: cat i\nbuild a: cat j\n")
            .unwrap_err();
        assert!(e.to_string().contains("multiple rules generate a"), "{e}");
    }

    #[test]
    fn include_and_subninja() {
        let disk = MemDisk::new();
        disk.create("sub.ninja", "var = sub\nbuild sub_out: cat sub_in\n");
        disk.create("inc.ninja", "var = inc\n");
        let mut state = State::new();
        {
            let mut p = ManifestParser::new(
                &mut state,
                &disk,
                ParserOptions {
                    quiet: true,
                    ..Default::default()
                },
            );
            p.parse_text(
                "input",
                b"rule cat\n  command = cat $in > $out\nvar = top\nsubninja sub.ninja\ninclude inc.ninja\n",
            )
            .unwrap();
        }
        // subninja gets a child scope, so it cannot change the parent's var;
        // include shares the scope, so it can.
        assert_eq!(state.scopes.lookup_variable(ROOT_SCOPE, "var"), "inc");
        assert!(state.lookup_node("sub_out").is_some());
    }

    #[test]
    fn missing_include_reports_position() {
        let disk = MemDisk::new();
        let mut state = State::new();
        let mut p = ManifestParser::new(
            &mut state,
            &disk,
            ParserOptions {
                quiet: true,
                ..Default::default()
            },
        );
        let e = p
            .parse_text("input", b"include missing.ninja\n")
            .unwrap_err();
        assert!(e.to_string().contains("loading 'missing.ninja'"), "{e}");
    }

    #[test]
    fn phony_self_cycle_is_filtered() {
        let disk = MemDisk::new();
        let mut state = State::new();
        let warnings = {
            let mut p = ManifestParser::new(&mut state, &disk, ParserOptions::default());
            p.parse_text("input", b"build a: phony a\n").unwrap();
            p.take_warnings()
        };
        assert_eq!(state.edge(crate::state::EdgeId(0)).inputs().len(), 0);
        assert!(warnings[0].contains("names itself as an input"));
    }

    #[test]
    fn dyndep_must_be_an_input() {
        let e = parse("rule r\n  command = x\n  dyndep = dd\n\nbuild out: r in\n").unwrap_err();
        assert!(e.to_string().contains("dyndep 'dd' is not an input"), "{e}");

        let s = parse("rule r\n  command = x\n  dyndep = dd\n\nbuild out: r in | dd\n").unwrap();
        let e = s.edge(crate::state::EdgeId(0));
        assert!(e.dyndep().is_some());
        assert!(s.node(e.dyndep().unwrap()).dyndep_pending);
    }

    #[test]
    fn atoi_like() {
        assert_eq!(parse_leading_int("12"), 12);
        assert_eq!(parse_leading_int("12abc"), 12);
        assert_eq!(parse_leading_int("abc"), 0);
        assert_eq!(parse_leading_int("-3"), -3);
        assert_eq!(parse_leading_int(""), 0);
    }
}
