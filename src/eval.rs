//! Variable expansion: `$`-interpolated strings, rules and lexical scopes.
//!
//! This mirrors ninja's `eval_env.{h,cc}`. The important subtlety is the
//! lookup order used when expanding a rule's bindings for a particular build
//! edge:
//!
//! 1. a value bound on the edge itself,
//! 2. a value bound on the rule, expanded *in the edge's scope*,
//! 3. a value bound in the scope enclosing the edge.
//!
//! See [`Scopes::lookup_with_fallback`], which implements step 1/2/3 for a
//! single variable.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::hash::FxHashMap;

/// A scope for `$variable` lookups.
pub trait Env {
    /// Look up `var`, returning the empty string when it is not bound.
    fn lookup(&mut self, var: &str) -> String;
}

/// One piece of a parsed `$`-interpolated string.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// Literal text.
    Text(String),
    /// A variable reference (the name, without `$`).
    Var(String),
}

/// A tokenized string containing variable references, evaluated against an
/// [`Env`].
///
/// Most strings in a manifest are a single run of literal text (a path), so
/// that case is held in `single` and the token vector stays empty. Clearing an
/// `EvalString` keeps both allocations, which lets a parser reuse one buffer
/// for every path in a manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalString {
    /// Non-empty only when the string is more than one run of literal text.
    parsed: Vec<Tok>,
    /// The single literal run, used while `parsed` is empty.
    single: String,
}

impl EvalString {
    /// An empty string with no tokens at all.
    pub fn new() -> EvalString {
        EvalString::default()
    }

    /// True when no tokens have been added. Note this is *not* "evaluates to
    /// the empty string": `$undefined` is not empty.
    pub fn is_empty(&self) -> bool {
        self.parsed.is_empty() && self.single.is_empty()
    }

    /// Forget all tokens, keeping the allocations for reuse.
    pub fn clear(&mut self) {
        self.parsed.clear();
        self.single.clear();
    }

    /// Append literal text, coalescing with a preceding literal.
    pub fn add_text(&mut self, text: &str) {
        if self.parsed.is_empty() {
            self.single.push_str(text);
        } else if let Some(Tok::Text(t)) = self.parsed.last_mut() {
            t.push_str(text);
        } else {
            self.parsed.push(Tok::Text(text.to_string()));
        }
    }

    /// Append a variable reference.
    pub fn add_var(&mut self, name: &str) {
        if self.parsed.is_empty() && !self.single.is_empty() {
            // Going from one token to two: the literal moves into the vector.
            let text = std::mem::take(&mut self.single);
            self.parsed.push(Tok::Text(text));
        }
        self.parsed.push(Tok::Var(name.to_string()));
    }

    /// Expand all variables using `env`.
    pub fn evaluate(&self, env: &mut dyn Env) -> String {
        if self.parsed.is_empty() {
            return self.single.clone();
        }
        let mut out = String::new();
        for t in &self.parsed {
            match t {
                Tok::Text(t) => out.push_str(t),
                Tok::Var(v) => out.push_str(&env.lookup(v)),
            }
        }
        out
    }

    /// Render back to source form, with variables as `${name}`.
    pub fn unparse(&self) -> String {
        if self.parsed.is_empty() {
            return self.single.clone();
        }
        let mut out = String::new();
        for t in &self.parsed {
            match t {
                Tok::Text(t) => out.push_str(t),
                Tok::Var(v) => {
                    out.push_str("${");
                    out.push_str(v);
                    out.push('}');
                }
            }
        }
        out
    }

    /// Debug representation used by tests: `[text][$var]`.
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        if self.parsed.is_empty() {
            if !self.single.is_empty() {
                out.push('[');
                out.push_str(&self.single);
                out.push(']');
            }
            return out;
        }
        for t in &self.parsed {
            out.push('[');
            match t {
                Tok::Text(t) => out.push_str(t),
                Tok::Var(v) => {
                    out.push('$');
                    out.push_str(v);
                }
            }
            out.push(']');
        }
        out
    }
}

/// Identifier for a rule in [`Scopes`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RuleId(pub u32);

/// Identifier for a lexical scope in [`Scopes`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ScopeId(pub u32);

/// The built-in `phony` rule always has id 0.
pub const PHONY_RULE: RuleId = RuleId(0);
/// The root (global) scope always has id 0.
pub const ROOT_SCOPE: ScopeId = ScopeId(0);

/// An invocable build command plus its metadata (description, depfile, ...).
#[derive(Clone, Debug)]
pub struct Rule {
    name: String,
    bindings: Vec<(String, EvalString)>,
    phony: bool,
}

impl Rule {
    /// A new rule with no bindings.
    pub fn new(name: impl Into<String>) -> Rule {
        Rule {
            name: name.into(),
            bindings: Vec::new(),
            phony: false,
        }
    }

    /// The built-in `phony` rule.
    pub fn phony() -> Rule {
        Rule {
            name: "phony".to_string(),
            bindings: Vec::new(),
            phony: true,
        }
    }

    /// The rule's name as written in the manifest.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// True for the built-in `phony` rule.
    pub fn is_phony(&self) -> bool {
        self.phony
    }

    /// Bind `key` to `val`, replacing any previous binding.
    pub fn add_binding(&mut self, key: impl Into<String>, val: EvalString) {
        let key = key.into();
        match self.bindings.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = val,
            None => self.bindings.push((key, val)),
        }
    }

    /// The unexpanded value bound to `key`, if any.
    pub fn binding(&self, key: &str) -> Option<&EvalString> {
        self.bindings
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// All bindings, in declaration order.
    pub fn bindings(&self) -> &[(String, EvalString)] {
        &self.bindings
    }

    /// True if `var` is one of the variable names a `rule` block may bind.
    pub fn is_reserved_binding(var: &str) -> bool {
        matches!(
            var,
            "command"
                | "depfile"
                | "dyndep"
                | "description"
                | "deps"
                | "generator"
                | "pool"
                | "restat"
                | "rspfile"
                | "rspfile_content"
                | "msvc_deps_prefix"
        )
    }
}

/// A single lexical scope: variable bindings, rules, and a parent link.
#[derive(Debug, Default)]
struct Scope {
    bindings: FxHashMap<String, String>,
    rules: BTreeMap<String, RuleId>,
    parent: Option<ScopeId>,
}

/// Arena of lexical scopes and rules.
///
/// `subninja` creates a child scope; `include` reuses the current one.
#[derive(Debug)]
pub struct Scopes {
    scopes: Vec<Scope>,
    rules: Vec<Rule>,
}

impl Default for Scopes {
    fn default() -> Self {
        Self::new()
    }
}

impl Scopes {
    /// A fresh arena containing the root scope and the built-in `phony` rule.
    pub fn new() -> Scopes {
        let mut s = Scopes {
            scopes: vec![Scope::default()],
            rules: vec![Rule::phony()],
        };
        s.scopes[0].rules.insert("phony".to_string(), PHONY_RULE);
        s
    }

    /// Create a child scope of `parent`.
    pub fn new_child(&mut self, parent: ScopeId) -> ScopeId {
        self.scopes.push(Scope {
            parent: Some(parent),
            ..Scope::default()
        });
        ScopeId((self.scopes.len() - 1) as u32)
    }

    /// Bind `key` to the already-expanded `val` in `scope`.
    pub fn add_binding(&mut self, scope: ScopeId, key: impl Into<String>, val: impl Into<String>) {
        self.scopes[scope.0 as usize]
            .bindings
            .insert(key.into(), val.into());
    }

    /// Look up `var` in `scope`, walking up to the root scope.
    pub fn lookup_variable(&self, scope: ScopeId, var: &str) -> String {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            let sc = &self.scopes[s.0 as usize];
            if let Some(v) = sc.bindings.get(var) {
                return v.clone();
            }
            cur = sc.parent;
        }
        String::new()
    }

    /// The bindings declared directly in `scope`.
    pub fn bindings_in(&self, scope: ScopeId) -> &FxHashMap<String, String> {
        &self.scopes[scope.0 as usize].bindings
    }

    /// Register `rule` in `scope`, returning its id.
    pub fn add_rule(&mut self, scope: ScopeId, rule: Rule) -> RuleId {
        self.rules.push(rule);
        let id = RuleId((self.rules.len() - 1) as u32);
        let name = self.rules[id.0 as usize].name.clone();
        self.scopes[scope.0 as usize].rules.insert(name, id);
        id
    }

    /// Find a rule by name in `scope` or any ancestor.
    pub fn lookup_rule(&self, scope: ScopeId, name: &str) -> Option<RuleId> {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            let sc = &self.scopes[s.0 as usize];
            if let Some(id) = sc.rules.get(name) {
                return Some(*id);
            }
            cur = sc.parent;
        }
        None
    }

    /// Find a rule declared directly in `scope`.
    pub fn lookup_rule_current_scope(&self, scope: ScopeId, name: &str) -> Option<RuleId> {
        self.scopes[scope.0 as usize].rules.get(name).copied()
    }

    /// The rule with the given id.
    pub fn rule(&self, id: RuleId) -> &Rule {
        &self.rules[id.0 as usize]
    }

    /// All rules declared directly in `scope`, sorted by name.
    pub fn rules_in(&self, scope: ScopeId) -> impl Iterator<Item = (&str, RuleId)> + '_ {
        self.scopes[scope.0 as usize]
            .rules
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
    }

    /// All rules in the arena.
    pub fn all_rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Edge-aware variable lookup.
    ///
    /// Returns a binding from `scope` itself if present; otherwise expands
    /// `eval` (a rule binding) using `env`; otherwise continues the lookup in
    /// the parent scope.
    pub fn lookup_with_fallback(
        &self,
        scope: ScopeId,
        var: &str,
        eval: Option<&EvalString>,
        env: &mut dyn Env,
    ) -> String {
        let sc = &self.scopes[scope.0 as usize];
        if let Some(v) = sc.bindings.get(var) {
            return v.clone();
        }
        if let Some(eval) = eval {
            return eval.evaluate(env);
        }
        match sc.parent {
            Some(p) => self.lookup_variable(p, var),
            None => String::new(),
        }
    }
}

/// An [`Env`] that resolves variables in a single scope chain. This is what
/// the manifest parser uses while parsing.
pub struct ScopeEnv<'a> {
    scopes: &'a Scopes,
    scope: ScopeId,
}

impl<'a> ScopeEnv<'a> {
    /// Create an env for `scope`.
    pub fn new(scopes: &'a Scopes, scope: ScopeId) -> ScopeEnv<'a> {
        ScopeEnv { scopes, scope }
    }
}

impl Env for ScopeEnv<'_> {
    fn lookup(&mut self, var: &str) -> String {
        self.scopes.lookup_variable(self.scope, var)
    }
}

/// An [`Env`] backed by a plain map, useful for tests and for dyndep files
/// (which have no scopes of their own).
#[derive(Default)]
pub struct MapEnv {
    map: HashMap<String, String>,
}

impl MapEnv {
    /// An empty env.
    pub fn new() -> MapEnv {
        MapEnv::default()
    }
    /// Bind a variable.
    pub fn insert(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.map.insert(k.into(), v.into());
    }
}

impl Env for MapEnv {
    fn lookup(&mut self, var: &str) -> String {
        self.map.get(var).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_string_basics() {
        let mut e = EvalString::new();
        assert!(e.is_empty());
        e.add_text("foo");
        e.add_text("bar");
        e.add_var("baz");
        assert_eq!(e.serialize(), "[foobar][$baz]");
        assert_eq!(e.unparse(), "foobar${baz}");

        let mut env = MapEnv::new();
        env.insert("baz", "BAZ");
        assert_eq!(e.evaluate(&mut env), "foobarBAZ");
    }

    #[test]
    fn scope_chain() {
        let mut s = Scopes::new();
        s.add_binding(ROOT_SCOPE, "a", "root-a");
        s.add_binding(ROOT_SCOPE, "b", "root-b");
        let child = s.new_child(ROOT_SCOPE);
        s.add_binding(child, "b", "child-b");
        assert_eq!(s.lookup_variable(child, "a"), "root-a");
        assert_eq!(s.lookup_variable(child, "b"), "child-b");
        assert_eq!(s.lookup_variable(ROOT_SCOPE, "b"), "root-b");
        assert_eq!(s.lookup_variable(child, "nope"), "");
    }

    #[test]
    fn phony_rule_exists() {
        let s = Scopes::new();
        assert_eq!(s.lookup_rule(ROOT_SCOPE, "phony"), Some(PHONY_RULE));
        assert!(s.rule(PHONY_RULE).is_phony());
    }
}
