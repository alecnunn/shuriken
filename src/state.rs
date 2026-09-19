//! The build graph: nodes (files), edges (build statements) and pools.

use std::sync::Arc;

use crate::canon::decanonicalize;
use crate::escape::append_escaped_for_host;
use crate::eval::{Env, EvalString, PHONY_RULE, ROOT_SCOPE, Rule, RuleId, ScopeId, Scopes};
use crate::hash::FxHashMap;
use crate::util::edit_distance;

/// Identifier of a node (a file) in the build graph.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(pub u32);

/// Identifier of an edge (a build statement) in the build graph.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EdgeId(pub u32);

/// Identifier of a pool.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PoolId(pub u32);

/// The implicit pool that does not limit parallelism.
pub const DEFAULT_POOL: PoolId = PoolId(0);
/// The built-in `console` pool (depth 1, shares ninja's stdio).
pub const CONSOLE_POOL: PoolId = PoolId(1);

/// Whether a node's existence on disk is known.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Existence {
    /// Not yet stat()ed.
    Unknown,
    /// stat()ed and absent. `mtime` holds the newest mtime of its inputs.
    Missing,
    /// stat()ed and present. `mtime` is the file's mtime.
    Exists,
}

/// A file in the build graph.
#[derive(Debug)]
pub struct Node {
    /// Shared with the path index, so each path is stored once.
    path: Arc<str>,
    slash_bits: u64,
    /// -1: not examined, 0: does not exist, >0: mtime in ns since the epoch
    /// (or, for missing nodes, the newest mtime among their dependencies).
    pub mtime: i64,
    pub(crate) exists: Existence,
    /// True when the file is out of date and must be rebuilt.
    pub dirty: bool,
    /// True when dyndep information is expected from this node but has not
    /// been loaded yet.
    pub dyndep_pending: bool,
    /// True when this node came from a depfile, dyndep file or the deps log
    /// rather than from the manifest. Such nodes may be missing without
    /// failing the build.
    pub generated_by_dep_loader: bool,
    pub(crate) in_edge: Option<EdgeId>,
    pub(crate) out_edges: Vec<EdgeId>,
    pub(crate) validation_out_edges: Vec<EdgeId>,
    /// Dense id assigned by the deps log, or -1.
    pub deps_log_id: i32,
}

impl Node {
    fn new(path: Arc<str>, slash_bits: u64) -> Node {
        Node {
            path,
            slash_bits,
            mtime: -1,
            exists: Existence::Unknown,
            dirty: false,
            dyndep_pending: false,
            // Nodes may be created by the deps log before the manifest is
            // parsed, so assume dep-loader provenance until proven otherwise.
            generated_by_dep_loader: true,
            in_edge: None,
            out_edges: Vec::new(),
            validation_out_edges: Vec::new(),
            deps_log_id: -1,
        }
    }

    /// The canonicalized path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The canonicalized path as a shared handle, so callers that need to keep
    /// it (log keys, for instance) do not copy it.
    pub fn path_shared(&self) -> Arc<str> {
        Arc::clone(&self.path)
    }

    /// The path with its original separator spelling restored (Windows).
    pub fn path_decanonicalized(&self) -> String {
        decanonicalize(&self.path, self.slash_bits)
    }

    /// Bitmask of separators that were backslashes before canonicalization.
    pub fn slash_bits(&self) -> u64 {
        self.slash_bits
    }

    /// True if the file is known to exist.
    pub fn exists(&self) -> bool {
        self.exists == Existence::Exists
    }

    /// True if the node has been stat()ed.
    pub fn status_known(&self) -> bool {
        self.exists != Existence::Unknown
    }

    /// The edge that produces this file, if any.
    pub fn in_edge(&self) -> Option<EdgeId> {
        self.in_edge
    }

    /// Edges that consume this file as an input.
    pub fn out_edges(&self) -> &[EdgeId] {
        &self.out_edges
    }

    /// Edges that name this file as a validation target.
    pub fn validation_out_edges(&self) -> &[EdgeId] {
        &self.validation_out_edges
    }

    /// Record the result of a stat().
    pub fn set_mtime(&mut self, mtime: i64) {
        self.mtime = mtime;
        self.exists = if mtime != 0 {
            Existence::Exists
        } else {
            Existence::Missing
        };
    }

    /// Mark the node as stat()ed and absent.
    pub fn mark_missing(&mut self) {
        if self.mtime == -1 {
            self.mtime = 0;
        }
        self.exists = Existence::Missing;
    }

    /// For phony outputs: expose the newest mtime of the inputs so dependents
    /// can compare against something meaningful.
    pub fn update_phony_mtime(&mut self, mtime: i64) {
        if !self.exists() {
            self.mtime = self.mtime.max(mtime);
        }
    }

    /// Forget stat results (used when the manifest is reloaded).
    pub fn reset_state(&mut self) {
        self.mtime = -1;
        self.exists = Existence::Unknown;
        self.dirty = false;
    }
}

/// How far a depth-first traversal has got with an edge.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum VisitMark {
    /// Not visited.
    None,
    /// On the current traversal stack (a second visit means a cycle).
    InStack,
    /// Fully visited.
    Done,
}

/// A build statement: inputs, outputs and the rule that connects them.
#[derive(Debug)]
pub struct Edge {
    pub(crate) rule: RuleId,
    pub(crate) pool: PoolId,
    /// Explicit inputs, then implicit inputs, then order-only inputs.
    pub(crate) inputs: Vec<NodeId>,
    /// Explicit outputs, then implicit outputs.
    pub(crate) outputs: Vec<NodeId>,
    pub(crate) validations: Vec<NodeId>,
    pub(crate) dyndep: Option<NodeId>,
    /// The scope in which this edge's bindings are resolved.
    pub(crate) scope: ScopeId,
    pub(crate) mark: VisitMark,
    pub(crate) id: u32,
    /// Scheduling priority: the longest weighted path to any target.
    pub(crate) critical_path_weight: i64,
    pub(crate) outputs_ready: bool,
    pub(crate) deps_loaded: bool,
    pub(crate) deps_missing: bool,
    pub(crate) implicit_deps: usize,
    pub(crate) order_only_deps: usize,
    pub(crate) implicit_outs: usize,
    /// Set when a dyndep file asked for `restat` behaviour on this edge.
    ///
    /// ninja stores this by adding a `restat = 1` binding to the edge's
    /// environment, which can leak into a shared scope; keeping it on the edge
    /// has the same effect without that hazard.
    pub(crate) dyndep_restat: bool,
    /// mtime of the lock file when the command started, used for `restat`.
    pub(crate) command_start_time: i64,
    /// How long this edge took last time, from the build log (-1 if unknown).
    pub(crate) prev_elapsed_time_millis: i64,
}

impl Edge {
    fn new(rule: RuleId, scope: ScopeId, id: u32) -> Edge {
        Edge {
            rule,
            pool: DEFAULT_POOL,
            inputs: Vec::new(),
            outputs: Vec::new(),
            validations: Vec::new(),
            dyndep: None,
            scope,
            mark: VisitMark::None,
            id,
            critical_path_weight: -1,
            outputs_ready: false,
            deps_loaded: false,
            deps_missing: false,
            implicit_deps: 0,
            order_only_deps: 0,
            implicit_outs: 0,
            dyndep_restat: false,
            command_start_time: 0,
            prev_elapsed_time_millis: -1,
        }
    }

    /// The rule this edge uses.
    pub fn rule(&self) -> RuleId {
        self.rule
    }

    /// The pool limiting this edge's concurrency.
    pub fn pool(&self) -> PoolId {
        self.pool
    }

    /// All inputs, explicit first.
    pub fn inputs(&self) -> &[NodeId] {
        &self.inputs
    }

    /// All outputs, explicit first.
    pub fn outputs(&self) -> &[NodeId] {
        &self.outputs
    }

    /// Validation targets.
    pub fn validations(&self) -> &[NodeId] {
        &self.validations
    }

    /// The node holding this edge's dyndep information, if any.
    pub fn dyndep(&self) -> Option<NodeId> {
        self.dyndep
    }

    /// Manifest order index.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Number of explicit (`$in`) inputs.
    pub fn explicit_deps(&self) -> usize {
        self.inputs.len() - self.implicit_deps - self.order_only_deps
    }

    /// Number of implicit inputs (after `|`).
    pub fn implicit_deps(&self) -> usize {
        self.implicit_deps
    }

    /// Number of order-only inputs (after `||`).
    pub fn order_only_deps(&self) -> usize {
        self.order_only_deps
    }

    /// Number of implicit outputs (before `|`).
    pub fn implicit_outs(&self) -> usize {
        self.implicit_outs
    }

    /// True if input `index` is order-only.
    pub fn is_order_only(&self, index: usize) -> bool {
        index >= self.inputs.len() - self.order_only_deps
    }

    /// True if input `index` is implicit.
    pub fn is_implicit(&self, index: usize) -> bool {
        index >= self.inputs.len() - self.order_only_deps - self.implicit_deps
            && !self.is_order_only(index)
    }

    /// True if output `index` is implicit.
    pub fn is_implicit_out(&self, index: usize) -> bool {
        index >= self.outputs.len() - self.implicit_outs
    }

    /// True once all of this edge's outputs are up to date.
    pub fn outputs_ready(&self) -> bool {
        self.outputs_ready
    }

    /// Scheduling weight (ninja weights every edge equally).
    pub fn weight(&self) -> i32 {
        1
    }

    /// Priority used by the ready queue.
    pub fn critical_path_weight(&self) -> i64 {
        self.critical_path_weight
    }

    /// Duration of the previous run in milliseconds, or -1.
    pub fn prev_elapsed_time_millis(&self) -> i64 {
        self.prev_elapsed_time_millis
    }

    /// Record how long this edge took last time, from the build log.
    pub fn set_prev_elapsed_time_millis(&mut self, millis: i64) {
        self.prev_elapsed_time_millis = millis;
    }

    /// True for self-referencing single-output phony edges, which old CMake
    /// versions emitted and which ninja tolerates with a warning.
    pub(crate) fn maybe_phonycycle_diagnostic(&self, is_phony: bool) -> bool {
        is_phony && self.outputs.len() == 1 && self.implicit_outs == 0 && self.implicit_deps == 0
    }
}

/// A named limit on how many edges may run concurrently.
#[derive(Debug, Clone)]
pub struct Pool {
    name: String,
    /// 0 means unlimited.
    depth: i32,
    current_use: i32,
}

impl Pool {
    /// A new pool. A depth of 0 means unlimited.
    pub fn new(name: impl Into<String>, depth: i32) -> Pool {
        Pool {
            name: name.into(),
            depth,
            current_use: 0,
        }
    }

    /// The pool's name (empty for the default pool).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Maximum concurrent weight, 0 meaning unlimited.
    pub fn depth(&self) -> i32 {
        self.depth
    }

    /// Weight currently scheduled from this pool.
    pub fn current_use(&self) -> i32 {
        self.current_use
    }

    /// True if this pool can delay edges.
    pub fn should_delay_edge(&self) -> bool {
        self.depth != 0
    }

    pub(crate) fn edge_scheduled(&mut self, weight: i32) {
        if self.depth != 0 {
            self.current_use += weight;
        }
    }

    pub(crate) fn edge_finished(&mut self, weight: i32) {
        if self.depth != 0 {
            self.current_use -= weight;
        }
    }

    pub(crate) fn reset(&mut self) {
        self.current_use = 0;
    }
}

/// How `$in`/`$out` are quoted when a binding is expanded.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Escape {
    /// Quote for the host shell (used for command lines).
    Shell,
    /// Leave paths as they are (used for depfile/rspfile/dyndep paths).
    None,
}

/// The whole build graph plus the scopes, rules and pools it was built from.
#[derive(Debug)]
pub struct State {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    node_by_path: FxHashMap<Arc<str>, NodeId>,
    /// Lexical scopes and rules.
    pub scopes: Scopes,
    pools: Vec<Pool>,
    pool_by_name: FxHashMap<String, PoolId>,
    defaults: Vec<NodeId>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    /// A state containing only the built-in `phony` rule and the default and
    /// `console` pools.
    pub fn new() -> State {
        let mut pool_by_name = FxHashMap::default();
        pool_by_name.insert(String::new(), DEFAULT_POOL);
        pool_by_name.insert("console".to_string(), CONSOLE_POOL);
        State {
            nodes: Vec::new(),
            edges: Vec::new(),
            node_by_path: FxHashMap::default(),
            scopes: Scopes::new(),
            pools: vec![Pool::new("", 0), Pool::new("console", 1)],
            pool_by_name,
            defaults: Vec::new(),
        }
    }

    /// All nodes, indexed by [`NodeId`].
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// All edges, in manifest order.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Immutable access to one node.
    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// Mutable access to one node.
    #[inline]
    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0 as usize]
    }

    /// Immutable access to one edge.
    #[inline]
    pub fn edge(&self, id: EdgeId) -> &Edge {
        &self.edges[id.0 as usize]
    }

    /// Mutable access to one edge.
    #[inline]
    pub fn edge_mut(&mut self, id: EdgeId) -> &mut Edge {
        &mut self.edges[id.0 as usize]
    }

    /// All ids of nodes in the graph.
    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> + Clone {
        (0..self.nodes.len() as u32).map(NodeId)
    }

    /// All ids of edges in the graph, in manifest order.
    pub fn edge_ids(&self) -> impl Iterator<Item = EdgeId> + Clone {
        (0..self.edges.len() as u32).map(EdgeId)
    }

    /// Look up a node by canonicalized path.
    pub fn lookup_node(&self, path: &str) -> Option<NodeId> {
        self.node_by_path.get(path).copied()
    }

    /// Look up or create a node for `path` (which must be canonicalized).
    pub fn get_node(&mut self, path: &str, slash_bits: u64) -> NodeId {
        if let Some(id) = self.node_by_path.get(path) {
            return *id;
        }
        let id = NodeId(self.nodes.len() as u32);
        let path: Arc<str> = Arc::from(path);
        self.nodes.push(Node::new(Arc::clone(&path), slash_bits));
        self.node_by_path.insert(path, id);
        id
    }

    /// Add an edge using `rule`, resolved in `scope`.
    pub fn add_edge(&mut self, rule: RuleId, scope: ScopeId) -> EdgeId {
        let id = EdgeId(self.edges.len() as u32);
        self.edges.push(Edge::new(rule, scope, id.0));
        id
    }

    /// Drop the most recently added edge (used when a `build` statement turns
    /// out to be redundant).
    pub(crate) fn pop_edge(&mut self) {
        self.edges.pop();
    }

    /// Connect `path` as an input of `edge`.
    pub fn add_in(&mut self, edge: EdgeId, path: &str, slash_bits: u64) {
        let node = self.get_node(path, slash_bits);
        self.node_mut(node).generated_by_dep_loader = false;
        self.edge_mut(edge).inputs.push(node);
        self.node_mut(node).out_edges.push(edge);
    }

    /// Connect `path` as an output of `edge`. Fails if another edge already
    /// produces it.
    pub fn add_out(&mut self, edge: EdgeId, path: &str, slash_bits: u64) -> Result<(), String> {
        let node = self.get_node(path, slash_bits);
        if let Some(other) = self.node(node).in_edge {
            return Err(if other == edge {
                format!("{path} is defined as an output multiple times")
            } else {
                format!("multiple rules generate {path}")
            });
        }
        self.edge_mut(edge).outputs.push(node);
        let n = self.node_mut(node);
        n.in_edge = Some(edge);
        n.generated_by_dep_loader = false;
        Ok(())
    }

    /// Register `path` as a validation target of `edge`.
    pub fn add_validation(&mut self, edge: EdgeId, path: &str, slash_bits: u64) {
        let node = self.get_node(path, slash_bits);
        self.edge_mut(edge).validations.push(node);
        let n = self.node_mut(node);
        n.validation_out_edges.push(edge);
        n.generated_by_dep_loader = false;
    }

    /// Mark `path` (already canonicalized) as a default target.
    pub fn add_default(&mut self, path: &str) -> Result<(), String> {
        match self.lookup_node(path) {
            Some(n) => {
                self.defaults.push(n);
                Ok(())
            }
            None => Err(format!("unknown target '{path}'")),
        }
    }

    /// The explicitly declared default targets.
    pub fn defaults(&self) -> &[NodeId] {
        &self.defaults
    }

    /// Nodes that no edge consumes, i.e. the leaves of the dependency graph.
    pub fn root_nodes(&self) -> Result<Vec<NodeId>, String> {
        let mut roots = Vec::new();
        for e in &self.edges {
            for &out in &e.outputs {
                if self.node(out).out_edges.is_empty() {
                    roots.push(out);
                }
            }
        }
        if !self.edges.is_empty() && roots.is_empty() {
            return Err("could not determine root nodes of build graph".to_string());
        }
        Ok(roots)
    }

    /// The targets to build when none are named on the command line.
    pub fn default_nodes(&self) -> Result<Vec<NodeId>, String> {
        if self.defaults.is_empty() {
            self.root_nodes()
        } else {
            Ok(self.defaults.clone())
        }
    }

    /// Register a pool. Fails if the name is taken.
    pub fn add_pool(&mut self, pool: Pool) -> Result<PoolId, String> {
        if self.pool_by_name.contains_key(pool.name()) {
            return Err(format!("duplicate pool '{}'", pool.name()));
        }
        let id = PoolId(self.pools.len() as u32);
        self.pool_by_name.insert(pool.name().to_string(), id);
        self.pools.push(pool);
        Ok(id)
    }

    /// Find a pool by name.
    pub fn lookup_pool(&self, name: &str) -> Option<PoolId> {
        self.pool_by_name.get(name).copied()
    }

    /// Immutable access to a pool.
    pub fn pool(&self, id: PoolId) -> &Pool {
        &self.pools[id.0 as usize]
    }

    /// Mutable access to a pool.
    pub fn pool_mut(&mut self, id: PoolId) -> &mut Pool {
        &mut self.pools[id.0 as usize]
    }

    /// All pools.
    pub fn pools(&self) -> &[Pool] {
        &self.pools
    }

    /// Reset per-build state so the graph can be re-scanned.
    pub fn reset(&mut self) {
        for n in &mut self.nodes {
            n.reset_state();
        }
        for e in &mut self.edges {
            e.outputs_ready = false;
            e.deps_loaded = false;
            e.mark = VisitMark::None;
        }
        for p in &mut self.pools {
            p.reset();
        }
    }

    /// The node whose path is closest to `path`, for "did you mean" messages.
    pub fn spellcheck_node(&self, path: &str) -> Option<NodeId> {
        const MAX_VALID_EDIT_DISTANCE: usize = 3;
        let mut min_distance = MAX_VALID_EDIT_DISTANCE + 1;
        let mut result = None;
        for id in self.node_ids() {
            let d = edit_distance(self.node(id).path(), path, true, MAX_VALID_EDIT_DISTANCE);
            if d < min_distance {
                min_distance = d;
                result = Some(id);
            }
        }
        result
    }

    // ---- edge binding evaluation -------------------------------------------

    /// True if `edge` uses the built-in `phony` rule.
    pub fn edge_is_phony(&self, edge: EdgeId) -> bool {
        self.edge(edge).rule == PHONY_RULE
    }

    /// True if `edge` runs in the `console` pool.
    pub fn edge_use_console(&self, edge: EdgeId) -> bool {
        self.edge(edge).pool == CONSOLE_POOL
    }

    /// The rule name of `edge`.
    pub fn edge_rule_name(&self, edge: EdgeId) -> &str {
        self.scopes.rule(self.edge(edge).rule).name()
    }

    /// Expand `key` for `edge` with shell escaping (as for a command line).
    pub fn edge_binding(&self, edge: EdgeId, key: &str) -> String {
        let mut env = EdgeEnv::new(self, edge, Escape::Shell);
        env.lookup(key)
    }

    /// Expand `key` for `edge` without escaping.
    pub fn edge_binding_unescaped(&self, edge: EdgeId, key: &str) -> String {
        let mut env = EdgeEnv::new(self, edge, Escape::None);
        env.lookup(key)
    }

    /// True if `key` expands to a non-empty string.
    pub fn edge_binding_bool(&self, edge: EdgeId, key: &str) -> bool {
        !self.edge_binding(edge, key).is_empty()
    }

    /// True if `edge` has `restat` behaviour, either from a binding or from a
    /// dyndep file.
    pub fn edge_restat(&self, edge: EdgeId) -> bool {
        self.edge(edge).dyndep_restat || self.edge_binding_bool(edge, "restat")
    }

    /// True if `edge` is a generator (e.g. the rule that regenerates the
    /// manifest).
    pub fn edge_is_generator(&self, edge: EdgeId) -> bool {
        self.edge_binding_bool(edge, "generator")
    }

    /// Expand `key`, reporting a cycle among rule variables as an error.
    pub fn edge_binding_checked(&self, edge: EdgeId, key: &str) -> Result<String, String> {
        let mut env = EdgeEnv::new(self, edge, Escape::Shell);
        let value = env.lookup(key);
        match env.take_error() {
            Some(e) => Err(e),
            None => Ok(value),
        }
    }

    /// The command line for `edge`.
    pub fn edge_command(&self, edge: EdgeId) -> String {
        self.edge_binding(edge, "command")
    }

    /// The command line for `edge`, reporting rule-variable cycles.
    pub fn edge_command_checked(&self, edge: EdgeId) -> Result<String, String> {
        self.edge_binding_checked(edge, "command")
    }

    /// The string hashed into the build log: the command line plus the
    /// response file contents, if any.
    pub fn edge_command_for_hash(&self, edge: EdgeId) -> String {
        let mut command = self.edge_binding(edge, "command");
        let rspfile_content = self.edge_binding(edge, "rspfile_content");
        if !rspfile_content.is_empty() {
            command.push_str(";rspfile=");
            command.push_str(&rspfile_content);
        }
        command
    }

    /// The `depfile` path for `edge`, unescaped.
    pub fn edge_depfile(&self, edge: EdgeId) -> String {
        self.edge_binding_unescaped(edge, "depfile")
    }

    /// The `dyndep` path for `edge`, unescaped.
    pub fn edge_dyndep_binding(&self, edge: EdgeId) -> String {
        self.edge_binding_unescaped(edge, "dyndep")
    }

    /// The `rspfile` path for `edge`, unescaped.
    pub fn edge_rspfile(&self, edge: EdgeId) -> String {
        self.edge_binding_unescaped(edge, "rspfile")
    }

    /// True once every input of `edge` has been produced.
    pub fn all_inputs_ready(&self, edge: EdgeId) -> bool {
        self.edge(edge).inputs.iter().all(|&i| {
            match self.node(i).in_edge {
                Some(e) => self.edge(e).outputs_ready,
                None => true,
            }
        })
    }

    /// A human-readable dump of an edge, for `-d` style debugging.
    pub fn format_edge(&self, edge: EdgeId) -> String {
        let e = self.edge(edge);
        let mut s = String::from("[ ");
        for &i in &e.inputs {
            s.push_str(self.node(i).path());
            s.push(' ');
        }
        s.push_str("--");
        s.push_str(self.edge_rule_name(edge));
        s.push_str("-> ");
        for &o in &e.outputs {
            s.push_str(self.node(o).path());
            s.push(' ');
        }
        s.push(']');
        s
    }
}

/// An [`Env`] that expands a rule's bindings for one particular edge,
/// providing `$in`, `$out` and the three-level lookup order documented on
/// [`crate::eval`].
pub struct EdgeEnv<'a> {
    state: &'a State,
    edge: EdgeId,
    escape: Escape,
    lookups: Vec<String>,
    recursive: bool,
    error: Option<String>,
}

impl<'a> EdgeEnv<'a> {
    /// Create an env for `edge`.
    pub fn new(state: &'a State, edge: EdgeId, escape: Escape) -> EdgeEnv<'a> {
        EdgeEnv {
            state,
            edge,
            escape,
            lookups: Vec::new(),
            recursive: false,
            error: None,
        }
    }

    /// Take the recorded error, if a cycle among rule variables was found.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    fn make_path_list(&self, span: &[NodeId], sep: char) -> String {
        let mut result = String::new();
        for &n in span {
            if !result.is_empty() {
                result.push(sep);
            }
            let path = self.state.node(n).path_decanonicalized();
            match self.escape {
                Escape::Shell => append_escaped_for_host(&path, &mut result),
                Escape::None => result.push_str(&path),
            }
        }
        result
    }
}

impl Env for EdgeEnv<'_> {
    fn lookup(&mut self, var: &str) -> String {
        let state: &State = self.state;
        let edge = state.edge(self.edge);

        if var == "in" || var == "in_newline" {
            let n = edge.explicit_deps();
            let sep = if var == "in" { ' ' } else { '\n' };
            return self.make_path_list(&edge.inputs[..n], sep);
        }
        if var == "out" {
            let n = edge.outputs.len() - edge.implicit_outs;
            return self.make_path_list(&edge.outputs[..n], ' ');
        }

        // Detect cycles such as `var1 = $var2` / `var2 = $var1` among rule
        // bindings. ninja treats this as fatal; we record it and stop
        // expanding so the caller can report it.
        if self.recursive {
            if let Some(pos) = self.lookups.iter().position(|v| v == var) {
                if self.error.is_none() {
                    let mut cycle = String::new();
                    for v in &self.lookups[pos..] {
                        cycle.push_str(v);
                        cycle.push_str(" -> ");
                    }
                    cycle.push_str(var);
                    self.error = Some(format!("cycle in rule variables: {cycle}"));
                }
                return String::new();
            }
        }

        let eval: Option<&EvalString> = state.scopes.rule(edge.rule).binding(var);
        let record = self.recursive && eval.is_some();
        if record {
            self.lookups.push(var.to_string());
        }
        // Only start cycle bookkeeping after the first lookup: rule variables
        // referring to other rule variables are rare.
        self.recursive = true;
        let scope = edge.scope;
        let result = state.scopes.lookup_with_fallback(scope, var, eval, self);
        if record {
            self.lookups.pop();
        }
        result
    }
}

/// Access to the rule of an edge without borrowing all of [`State`].
impl State {
    /// The [`Rule`] used by `edge`.
    pub fn edge_rule(&self, edge: EdgeId) -> &Rule {
        self.scopes.rule(self.edge(edge).rule)
    }

    /// The root scope id.
    pub fn root_scope(&self) -> ScopeId {
        ROOT_SCOPE
    }

    /// A global variable from the root scope (e.g. `builddir`).
    pub fn global_binding(&self, name: &str) -> String {
        self.scopes.lookup_variable(ROOT_SCOPE, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_are_interned() {
        let mut s = State::new();
        let a = s.get_node("a.txt", 0);
        let b = s.get_node("a.txt", 0);
        assert_eq!(a, b);
        assert_eq!(s.nodes().len(), 1);
    }

    #[test]
    fn duplicate_output_is_an_error() {
        let mut s = State::new();
        let e1 = s.add_edge(PHONY_RULE, ROOT_SCOPE);
        s.add_out(e1, "out", 0).unwrap();
        let e2 = s.add_edge(PHONY_RULE, ROOT_SCOPE);
        let err = s.add_out(e2, "out", 0).unwrap_err();
        assert_eq!(err, "multiple rules generate out");
    }

    #[test]
    fn default_pool_is_unlimited() {
        let s = State::new();
        assert!(!s.pool(DEFAULT_POOL).should_delay_edge());
        assert!(s.pool(CONSOLE_POOL).should_delay_edge());
        assert_eq!(s.pool(CONSOLE_POOL).depth(), 1);
    }
}
