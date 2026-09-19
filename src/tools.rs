//! The `-t` subtools: inspecting the graph, dumping commands, and so on.
//!
//! Each tool is a function that returns a `String` rather than printing, so it
//! can be used from a library as easily as from the command line.

use std::collections::{BTreeMap, BTreeSet};

use crate::canon::canonicalize_path;
use crate::depfile::parse_depfile;
use crate::deps_log::DepsLog;
use crate::disk::DiskInterface;
use crate::dyndep::load_dyndeps;
use crate::error::{Error, Result};
use crate::escape::{encode_json_string, escaped_for_host};
use crate::eval::ROOT_SCOPE;
use crate::hash::FxHashSet;
use crate::state::{EdgeId, NodeId, State};

/// Resolve a command-line target name to a node.
///
/// Supports ninja's `path^` syntax, which means "the first output of the first
/// edge that consumes `path`".
pub fn collect_target(state: &State, deps_log: Option<&DepsLog>, path: &str) -> Result<NodeId> {
    if path.is_empty() {
        return Err(Error::graph("empty path"));
    }
    let mut path = path.to_string();
    canonicalize_path(&mut path);

    let mut first_dependent = false;
    if path.ends_with('^') {
        path.pop();
        first_dependent = true;
    }

    let Some(node) = state.lookup_node(&path) else {
        let mut msg = format!("unknown target '{path}'");
        if path == "clean" {
            msg.push_str(", did you mean 'shuriken -t clean'?");
        } else if path == "help" {
            msg.push_str(", did you mean 'shuriken -h'?");
        } else if let Some(suggestion) = state.spellcheck_node(&path) {
            msg.push_str(&format!(
                ", did you mean '{}'?",
                state.node(suggestion).path()
            ));
        }
        return Err(Error::graph(msg));
    };

    if !first_dependent {
        return Ok(node);
    }

    if state.node(node).out_edges().is_empty() {
        // Fall back to the deps log: a header has no out edge in the manifest,
        // but something recorded a dependency on it.
        let rev = deps_log.and_then(|log| log.first_reverse_deps_node(node));
        return rev.ok_or_else(|| Error::graph(format!("'{path}' has no out edge")));
    }
    let edge = state.node(node).out_edges()[0];
    if state.edge(edge).outputs().is_empty() {
        return Err(Error::graph("edge has no outputs"));
    }
    Ok(state.edge(edge).outputs()[0])
}

/// Resolve every target name, defaulting to the manifest's default targets.
pub fn collect_targets(
    state: &State,
    deps_log: Option<&DepsLog>,
    names: &[String],
) -> Result<Vec<NodeId>> {
    if names.is_empty() {
        return state.default_nodes().map_err(Error::graph);
    }
    names
        .iter()
        .map(|n| collect_target(state, deps_log, n))
        .collect()
}

/// Load every pending dyndep file so tools see the full graph.
fn load_all_dyndeps(state: &mut State, disk: &dyn DiskInterface) {
    for edge in state.edge_ids().collect::<Vec<_>>() {
        if let Some(dyndep) = state.edge(edge).dyndep() {
            if state.node(dyndep).dyndep_pending {
                let _ = load_dyndeps(state, disk, dyndep);
            }
        }
    }
}

/// `-t graph`: a graphviz description of the graph reachable from `targets`.
pub fn graph(state: &mut State, disk: &dyn DiskInterface, targets: &[NodeId]) -> String {
    load_all_dyndeps(state, disk);

    let mut out = String::from("digraph ninja {\n");
    out.push_str("rankdir=\"LR\"\n");
    out.push_str("node [fontsize=10, shape=box, height=0.25]\n");
    out.push_str("edge [fontsize=10]\n");

    let mut visited_nodes: BTreeSet<NodeId> = BTreeSet::new();
    let mut visited_edges: BTreeSet<EdgeId> = BTreeSet::new();
    let mut stack: Vec<NodeId> = targets.iter().rev().copied().collect();

    while let Some(node) = stack.pop() {
        if !visited_nodes.insert(node) {
            continue;
        }
        let path = state.node(node).path().replace('\\', "/");
        out.push_str(&format!("\"n{}\" [label=\"{}\"]\n", node.0, path));

        let Some(edge) = state.node(node).in_edge() else {
            continue; // Leaf.
        };
        if !visited_edges.insert(edge) {
            continue;
        }

        let rule = state.edge_rule_name(edge).to_string();
        let e = state.edge(edge);
        if e.inputs().len() == 1 && e.outputs().len() == 1 {
            out.push_str(&format!(
                "\"n{}\" -> \"n{}\" [label=\" {}\"]\n",
                e.inputs()[0].0,
                e.outputs()[0].0,
                rule
            ));
        } else {
            out.push_str(&format!(
                "\"e{}\" [label=\"{}\", shape=ellipse]\n",
                edge.0, rule
            ));
            for &o in e.outputs() {
                out.push_str(&format!("\"e{}\" -> \"n{}\"\n", edge.0, o.0));
            }
            for (i, &input) in e.inputs().iter().enumerate() {
                let order_only = if e.is_order_only(i) {
                    " style=dotted"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "\"n{}\" -> \"e{}\" [arrowhead=none{}]\n",
                    input.0, edge.0, order_only
                ));
            }
        }

        for &input in state.edge(edge).inputs() {
            stack.push(input);
        }
    }

    out.push_str("}\n");
    out
}

/// `-t query`: what produces a path, and what consumes it.
pub fn query(state: &mut State, disk: &dyn DiskInterface, targets: &[NodeId]) -> String {
    let mut out = String::new();
    for &node in targets {
        // Load dyndep info for this node's edge, like ninja does.
        if let Some(edge) = state.node(node).in_edge() {
            if let Some(dyndep) = state.edge(edge).dyndep() {
                if state.node(dyndep).dyndep_pending {
                    let _ = load_dyndeps(state, disk, dyndep);
                }
            }
        }

        out.push_str(&format!("{}:\n", state.node(node).path()));
        if let Some(edge) = state.node(node).in_edge() {
            out.push_str(&format!("  input: {}\n", state.edge_rule_name(edge)));
            let e = state.edge(edge);
            for (i, &input) in e.inputs().iter().enumerate() {
                let label = if e.is_implicit(i) {
                    "| "
                } else if e.is_order_only(i) {
                    "|| "
                } else {
                    ""
                };
                out.push_str(&format!("    {}{}\n", label, state.node(input).path()));
            }
            if !e.validations().is_empty() {
                out.push_str("  validations:\n");
                for &v in e.validations() {
                    out.push_str(&format!("    {}\n", state.node(v).path()));
                }
            }
        }
        out.push_str("  outputs:\n");
        for &edge in state.node(node).out_edges() {
            for &o in state.edge(edge).outputs() {
                out.push_str(&format!("    {}\n", state.node(o).path()));
            }
        }
        let validation_edges = state.node(node).validation_out_edges().to_vec();
        if !validation_edges.is_empty() {
            out.push_str("  validation for:\n");
            for edge in validation_edges {
                for &o in state.edge(edge).outputs() {
                    out.push_str(&format!("    {}\n", state.node(o).path()));
                }
            }
        }
    }
    out
}

/// `-t deps`: the dependencies recorded in the deps log.
pub fn deps(
    state: &State,
    disk: &dyn DiskInterface,
    deps_log: &DepsLog,
    targets: &[NodeId],
) -> String {
    let nodes: Vec<NodeId> = if targets.is_empty() {
        deps_log
            .nodes()
            .iter()
            .copied()
            .filter(|&n| DepsLog::is_deps_entry_live_for(state, n))
            .collect()
    } else {
        targets.to_vec()
    };

    let mut out = String::new();
    for node in nodes {
        let path = state.node(node).path();
        let Some(deps) = deps_log.get_deps(state, node) else {
            out.push_str(&format!("{path}: deps not found\n"));
            continue;
        };
        let mtime = disk.stat(path).unwrap_or(0);
        let validity = if mtime == 0 || mtime > deps.mtime {
            "STALE"
        } else {
            "VALID"
        };
        out.push_str(&format!(
            "{}: #deps {}, deps mtime {} ({})\n",
            path,
            deps.nodes.len(),
            deps.mtime,
            validity
        ));
        for &d in &deps.nodes {
            out.push_str(&format!("    {}\n", state.node(d).path()));
        }
        out.push('\n');
    }
    out
}

/// `-t targets` with no arguments, or `depth N`: targets by depth in the DAG.
pub fn targets_by_depth(state: &State, depth: i32) -> Result<String> {
    let roots = state.root_nodes().map_err(Error::build)?;
    let mut out = String::new();
    targets_list(state, &roots, depth, 0, &mut out);
    Ok(out)
}

fn targets_list(state: &State, nodes: &[NodeId], depth: i32, indent: usize, out: &mut String) {
    for &node in nodes {
        for _ in 0..indent {
            out.push_str("  ");
        }
        let path = state.node(node).path();
        match state.node(node).in_edge() {
            Some(edge) => {
                out.push_str(&format!("{}: {}\n", path, state.edge_rule_name(edge)));
                if depth > 1 || depth <= 0 {
                    let inputs = state.edge(edge).inputs().to_vec();
                    targets_list(state, &inputs, depth - 1, indent + 1, out);
                }
            }
            None => out.push_str(&format!("{path}\n")),
        }
    }
}

/// `-t targets all`: every output and the rule that makes it.
pub fn targets_all(state: &State) -> String {
    let mut out = String::new();
    for edge in state.edge_ids() {
        for &o in state.edge(edge).outputs() {
            out.push_str(&format!(
                "{}: {}\n",
                state.node(o).path(),
                state.edge_rule_name(edge)
            ));
        }
    }
    out
}

/// `-t targets rule NAME`: outputs produced by a given rule.
pub fn targets_by_rule(state: &State, rule_name: &str) -> String {
    let mut paths: BTreeSet<&str> = BTreeSet::new();
    for edge in state.edge_ids() {
        if state.edge_rule_name(edge) == rule_name {
            for &o in state.edge(edge).outputs() {
                paths.insert(state.node(o).path());
            }
        }
    }
    let mut out = String::new();
    for p in paths {
        out.push_str(p);
        out.push('\n');
    }
    out
}

/// `-t targets rule` with no rule: all source files.
pub fn targets_source_list(state: &State) -> String {
    let mut out = String::new();
    for edge in state.edge_ids() {
        for &input in state.edge(edge).inputs() {
            if state.node(input).in_edge().is_none() {
                out.push_str(state.node(input).path());
                out.push('\n');
            }
        }
    }
    out
}

/// `-t commands`: the commands needed to build `targets`.
///
/// With `single`, only each target's own command is printed, not the whole
/// chain.
pub fn commands(state: &State, targets: &[NodeId], single: bool) -> String {
    let mut out = String::new();
    let mut seen: BTreeSet<EdgeId> = BTreeSet::new();
    for &node in targets {
        if let Some(edge) = state.node(node).in_edge() {
            print_commands(state, edge, &mut seen, single, &mut out);
        }
    }
    out
}

fn print_commands(
    state: &State,
    edge: EdgeId,
    seen: &mut BTreeSet<EdgeId>,
    single: bool,
    out: &mut String,
) {
    if !seen.insert(edge) {
        return;
    }
    if !single {
        for &input in state.edge(edge).inputs() {
            if let Some(producer) = state.node(input).in_edge() {
                print_commands(state, producer, seen, single, out);
            }
        }
    }
    if !state.edge_is_phony(edge) {
        out.push_str(&state.edge_command(edge));
        out.push('\n');
    }
}

/// Options for [`inputs`].
#[derive(Clone, Copy, Debug, Default)]
pub struct InputsOptions {
    /// Terminate each path with NUL instead of a newline.
    pub print0: bool,
    /// Shell-escape each path (on by default in ninja).
    pub shell_escape: bool,
    /// Keep dependency order instead of sorting alphabetically.
    pub dependency_order: bool,
}

/// `-t inputs`: every input needed to build `targets`.
pub fn inputs(state: &State, targets: &[NodeId], options: InputsOptions) -> String {
    let mut collector = InputsCollector::default();
    for &node in targets {
        collector.visit(state, node);
    }
    let mut paths = collector.as_strings(state, options.shell_escape);
    if !options.dependency_order {
        paths.sort();
    }
    let mut out = String::new();
    for p in paths {
        out.push_str(&p);
        out.push(if options.print0 { '\0' } else { '\n' });
    }
    out
}

/// `-t multi-inputs`: like [`inputs`], but each line names its target too.
pub fn multi_inputs(
    state: &State,
    targets: &[NodeId],
    delimiter: &str,
    terminator: char,
) -> String {
    let mut out = String::new();
    for &node in targets {
        let mut collector = InputsCollector::default();
        collector.visit(state, node);
        for input in collector.as_strings(state, false) {
            out.push_str(state.node(node).path());
            out.push_str(delimiter);
            out.push_str(&input);
            out.push(terminator);
        }
    }
    out
}

/// Collects the transitive inputs of a set of nodes, skipping the outputs of
/// phony edges but following through them.
#[derive(Default)]
pub struct InputsCollector {
    inputs: Vec<NodeId>,
    visited: FxHashSet<NodeId>,
}

impl InputsCollector {
    /// Visit one root node.
    pub fn visit(&mut self, state: &State, node: NodeId) {
        let Some(edge) = state.node(node).in_edge() else {
            return; // A source file.
        };
        for &input in state.edge(edge).inputs() {
            if !self.visited.insert(input) {
                continue;
            }
            self.visit(state, input);
            let is_phony_output = match state.node(input).in_edge() {
                Some(e) => state.edge_is_phony(e),
                None => false,
            };
            if !is_phony_output {
                self.inputs.push(input);
            }
        }
    }

    /// The collected inputs, dependencies before dependents.
    pub fn inputs(&self) -> &[NodeId] {
        &self.inputs
    }

    /// The collected inputs as (optionally escaped) path strings.
    pub fn as_strings(&self, state: &State, shell_escape: bool) -> Vec<String> {
        self.inputs
            .iter()
            .map(|&n| {
                let path = state.node(n).path_decanonicalized();
                if shell_escape {
                    escaped_for_host(&path)
                } else {
                    path
                }
            })
            .collect()
    }
}

/// `-t compdb`: a JSON compilation database.
///
/// With `rules` non-empty, only edges using those rules are included.
pub fn compdb(state: &State, rules: &[String], expand_rspfile: bool, directory: &str) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for edge in state.edge_ids() {
        if state.edge(edge).inputs().is_empty() {
            continue;
        }
        if !rules.is_empty() && !rules.iter().any(|r| r == state.edge_rule_name(edge)) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&compdb_object(state, edge, expand_rspfile, directory));
    }
    out.push_str("\n]\n");
    out
}

/// `-t compdb-targets`: a compilation database for the given targets only.
pub fn compdb_targets(
    state: &State,
    targets: &[NodeId],
    expand_rspfile: bool,
    directory: &str,
) -> Result<String> {
    let mut edges: Vec<EdgeId> = Vec::new();
    let mut seen: BTreeSet<EdgeId> = BTreeSet::new();
    let mut stack: Vec<NodeId> = targets.to_vec();
    while let Some(node) = stack.pop() {
        let Some(edge) = state.node(node).in_edge() else {
            continue;
        };
        if !seen.insert(edge) {
            continue;
        }
        if !state.edge_is_phony(edge) && !state.edge(edge).inputs().is_empty() {
            edges.push(edge);
        }
        stack.extend(state.edge(edge).inputs().iter().copied());
    }
    edges.sort();

    let mut out = String::from("[");
    for (i, &edge) in edges.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&compdb_object(state, edge, expand_rspfile, directory));
    }
    out.push_str("\n]\n");
    Ok(out)
}

fn compdb_object(state: &State, edge: EdgeId, expand_rspfile: bool, directory: &str) -> String {
    let command = if expand_rspfile {
        command_with_rspfile_expanded(state, edge)
    } else {
        state.edge_command(edge)
    };
    format!(
        "\n  {{\n    \"directory\": \"{}\",\n    \"command\": \"{}\",\n    \"file\": \"{}\",\n    \"output\": \"{}\"\n  }}",
        encode_json_string(directory),
        encode_json_string(&command),
        encode_json_string(state.node(state.edge(edge).inputs()[0]).path()),
        encode_json_string(state.node(state.edge(edge).outputs()[0]).path()),
    )
}

/// Replace an `@rspfile` reference in a command with the response file's
/// contents, as `-t compdb -x` does.
pub fn command_with_rspfile_expanded(state: &State, edge: EdgeId) -> String {
    let command = state.edge_command(edge);
    let rspfile = state.edge_rspfile(edge);
    if rspfile.is_empty() {
        return command;
    }
    let Some(index) = command.find(&rspfile) else {
        return command;
    };
    if index == 0 {
        return command;
    }

    let content = state
        .edge_binding(edge, "rspfile_content")
        .replace('\n', " ");
    let before = &command[..index];
    if before.ends_with('@') {
        format!(
            "{}{}{}",
            &command[..index - 1],
            content,
            &command[index + rspfile.len()..]
        )
    } else if before.ends_with("-f ") {
        format!(
            "{}{}{}",
            &command[..index - 3],
            content,
            &command[index + rspfile.len()..]
        )
    } else if before.ends_with("--option-file=") {
        format!(
            "{}{}{}",
            &command[..index - "--option-file=".len()],
            content,
            &command[index + rspfile.len()..]
        )
    } else {
        command
    }
}

/// `-t rules`: every rule name, optionally with its description.
pub fn rules(state: &State, with_description: bool) -> String {
    let mut out = String::new();
    let mut names: Vec<(&str, crate::eval::RuleId)> = state.scopes.rules_in(ROOT_SCOPE).collect();
    names.sort_by(|a, b| a.0.cmp(b.0));
    for (name, id) in names {
        out.push_str(name);
        if with_description {
            if let Some(desc) = state.scopes.rule(id).binding("description") {
                out.push_str(&format!(": {}", desc.unparse()));
            }
        }
        out.push('\n');
    }
    out
}

/// The result of `-t missingdeps`.
#[derive(Debug, Default)]
pub struct MissingDepsReport {
    /// One line per missing dependency found.
    pub lines: Vec<String>,
    /// How many nodes were examined.
    pub nodes_processed: usize,
    /// How many distinct (target, generator rule) pairs are missing a path.
    pub missing_dep_path_count: usize,
    /// Targets that are missing a dependency path.
    pub nodes_missing_deps: usize,
    /// Distinct generated inputs involved.
    pub generated_nodes: usize,
    /// Distinct rules that generate them.
    pub generator_rules: usize,
}

impl MissingDepsReport {
    /// True if any missing dependency was found.
    pub fn had_missing_deps(&self) -> bool {
        self.missing_dep_path_count > 0
    }

    /// A human-readable summary, in ninja's wording.
    pub fn summary(&self) -> String {
        let mut out = format!("Processed {} nodes.\n", self.nodes_processed);
        if self.had_missing_deps() {
            out.push_str(&format!(
                "Error: There are {} missing dependency paths.\n",
                self.missing_dep_path_count
            ));
            out.push_str(&format!(
                "{} targets had depfile dependencies on {} distinct generated inputs (from {} \
                 rules)  without a non-depfile dep path to the generator.\n",
                self.nodes_missing_deps, self.generated_nodes, self.generator_rules
            ));
            out.push_str(
                "There might be build flakiness if any of the targets listed above are built \
                 alone, or not late enough, in a clean output directory.\n",
            );
        } else {
            out.push_str("No missing dependencies on generated files found.\n");
        }
        out
    }
}

/// `-t missingdeps`: find depfile dependencies on generated files that the
/// manifest does not declare, which make the build order-dependent.
pub fn missing_deps(
    state: &mut State,
    disk: &dyn DiskInterface,
    deps_log: &DepsLog,
    targets: &[NodeId],
) -> Result<MissingDepsReport> {
    let mut scanner = MissingDepsScanner {
        report: MissingDepsReport::default(),
        seen: BTreeSet::new(),
        generated_nodes: BTreeSet::new(),
        generator_rules: BTreeSet::new(),
        nodes_missing_deps: BTreeSet::new(),
        adjacency: BTreeMap::new(),
    };
    for &node in targets {
        scanner.process_node(state, disk, deps_log, node)?;
    }
    scanner.report.nodes_processed = scanner.seen.len();
    scanner.report.generated_nodes = scanner.generated_nodes.len();
    scanner.report.generator_rules = scanner.generator_rules.len();
    scanner.report.nodes_missing_deps = scanner.nodes_missing_deps.len();
    Ok(scanner.report)
}

struct MissingDepsScanner {
    report: MissingDepsReport,
    seen: BTreeSet<NodeId>,
    generated_nodes: BTreeSet<NodeId>,
    generator_rules: BTreeSet<String>,
    nodes_missing_deps: BTreeSet<NodeId>,
    adjacency: BTreeMap<(EdgeId, EdgeId), bool>,
}

impl MissingDepsScanner {
    fn process_node(
        &mut self,
        state: &mut State,
        disk: &dyn DiskInterface,
        deps_log: &DepsLog,
        node: NodeId,
    ) -> Result<()> {
        let Some(edge) = state.node(node).in_edge() else {
            return Ok(());
        };
        if !self.seen.insert(node) {
            return Ok(());
        }

        for input in state.edge(edge).inputs().to_vec() {
            self.process_node(state, disk, deps_log, input)?;
        }

        let deps_type = state.edge_binding(edge, "deps");
        let dep_nodes: Vec<NodeId> = if !deps_type.is_empty() {
            match deps_log.get_deps(state, node) {
                Some(d) => d.nodes.clone(),
                None => return Ok(()),
            }
        } else {
            let depfile = state.edge_depfile(edge);
            if depfile.is_empty() {
                return Ok(());
            }
            let content = disk.read_file(&depfile)?.unwrap_or_default();
            if content.is_empty() {
                return Ok(());
            }
            let parsed = match parse_depfile(&content) {
                Ok(p) => p,
                Err(_) => return Ok(()),
            };
            parsed
                .ins
                .into_iter()
                .map(|mut p| {
                    let bits = canonicalize_path(&mut p);
                    state.get_node(&p, bits)
                })
                .collect()
        };

        if dep_nodes.is_empty() {
            return Ok(());
        }
        self.process_node_deps(state, node, &dep_nodes);
        Ok(())
    }

    fn process_node_deps(&mut self, state: &State, node: NodeId, dep_nodes: &[NodeId]) {
        let edge = state.node(node).in_edge().expect("checked by caller");

        let mut deplog_edges: BTreeSet<EdgeId> = BTreeSet::new();
        for &dep in dep_nodes {
            // A dependency on build.ninja means "rebuild when reconfigured",
            // which is not a real missing dependency.
            if state.node(dep).path() == "build.ninja" {
                return;
            }
            if let Some(e) = state.node(dep).in_edge() {
                deplog_edges.insert(e);
            }
        }

        let missing: Vec<EdgeId> = deplog_edges
            .into_iter()
            .filter(|&from| !self.path_exists_between(state, from, edge))
            .collect();

        if missing.is_empty() {
            return;
        }

        let mut rule_names: BTreeSet<String> = BTreeSet::new();
        for producer in missing {
            for &dep in dep_nodes {
                if state.node(dep).in_edge() == Some(producer) {
                    self.generated_nodes.insert(dep);
                    let rule = state.edge_rule_name(producer).to_string();
                    self.generator_rules.insert(rule.clone());
                    rule_names.insert(rule.clone());
                    self.report.lines.push(format!(
                        "Missing dep: {} uses {} (generated by {})",
                        state.node(node).path(),
                        state.node(dep).path(),
                        rule
                    ));
                }
            }
        }
        self.report.missing_dep_path_count += rule_names.len();
        self.nodes_missing_deps.insert(node);
    }

    fn path_exists_between(&mut self, state: &State, from: EdgeId, to: EdgeId) -> bool {
        if let Some(&known) = self.adjacency.get(&(from, to)) {
            return known;
        }
        // Insert a provisional `false` to break cycles.
        self.adjacency.insert((from, to), false);
        let mut found = false;
        for &input in state.edge(to).inputs() {
            if let Some(e) = state.node(input).in_edge() {
                if e == from || self.path_exists_between(state, from, e) {
                    found = true;
                    break;
                }
            }
        }
        self.adjacency.insert((from, to), found);
        found
    }
}

/// The current working directory, as the compilation database reports it.
pub fn working_directory() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::parse::{ManifestParser, ParserOptions};

    const CAT: &str = "rule cat\n  command = cat $in > $out\n\n";

    fn setup(disk: &MemDisk, manifest: &str) -> State {
        let mut state = State::new();
        {
            let mut p = ManifestParser::new(
                &mut state,
                disk,
                ParserOptions {
                    quiet: true,
                    ..Default::default()
                },
            );
            p.parse_text("input", manifest.as_bytes()).unwrap();
        }
        state
    }

    #[test]
    fn targets_and_rules() {
        let disk = MemDisk::new();
        let state = setup(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
        assert_eq!(targets_all(&state), "b: cat\nc: cat\n");
        assert_eq!(targets_by_rule(&state, "cat"), "b\nc\n");
        assert_eq!(targets_source_list(&state), "a\n");
        assert_eq!(rules(&state, false), "cat\nphony\n");
        assert_eq!(targets_by_depth(&state, 1).unwrap(), "c: cat\n");
    }

    #[test]
    fn commands_tool() {
        let disk = MemDisk::new();
        let state = setup(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
        let c = state.lookup_node("c").unwrap();
        assert_eq!(commands(&state, &[c], false), "cat a > b\ncat b > c\n");
        assert_eq!(commands(&state, &[c], true), "cat b > c\n");
    }

    #[test]
    fn inputs_tool() {
        let disk = MemDisk::new();
        let state = setup(
            &disk,
            &format!("{CAT}build b: cat a\nbuild c: cat b\nbuild all: phony c\n"),
        );
        let all = state.lookup_node("all").unwrap();
        let out = inputs(
            &state,
            &[all],
            InputsOptions {
                shell_escape: false,
                ..Default::default()
            },
        );
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn query_tool() {
        let disk = MemDisk::new();
        let mut state = setup(&disk, &format!("{CAT}build b: cat a | i || o\n"));
        let b = state.lookup_node("b").unwrap();
        let out = query(&mut state, &disk, &[b]);
        assert!(
            out.contains("b:\n  input: cat\n    a\n    | i\n    || o\n"),
            "{out}"
        );
    }

    #[test]
    fn graph_tool() {
        let disk = MemDisk::new();
        let mut state = setup(&disk, &format!("{CAT}build b: cat a\n"));
        let b = state.lookup_node("b").unwrap();
        let out = graph(&mut state, &disk, &[b]);
        assert!(out.starts_with("digraph ninja {\n"));
        assert!(out.contains("label=\"b\""));
        assert!(out.contains("label=\" cat\""));
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn compdb_tool() {
        let disk = MemDisk::new();
        let state = setup(&disk, &format!("{CAT}build b: cat a\n"));
        let out = compdb(&state, &[], false, "/work");
        assert!(out.contains("\"directory\": \"/work\""));
        assert!(out.contains("\"command\": \"cat a > b\""));
        assert!(out.contains("\"file\": \"a\""));
        assert!(out.contains("\"output\": \"b\""));
    }

    #[test]
    fn compdb_expands_rspfile() {
        let disk = MemDisk::new();
        let state = setup(
            &disk,
            "rule link\n  command = link @$out.rsp\n  rspfile = $out.rsp\n  \
             rspfile_content = -o $out $in\n\nbuild b: link a\n",
        );
        let out = compdb(&state, &[], true, ".");
        assert!(out.contains("link -o b a"), "{out}");
    }

    #[test]
    fn collect_target_suggests_alternatives() {
        let disk = MemDisk::new();
        let state = setup(&disk, &format!("{CAT}build target: cat a\n"));
        let e = collect_target(&state, None, "targt").unwrap_err();
        assert!(e.to_string().contains("did you mean 'target'"), "{e}");
    }

    #[test]
    fn collect_target_caret() {
        let disk = MemDisk::new();
        let state = setup(&disk, &format!("{CAT}build b: cat a\n"));
        let n = collect_target(&state, None, "a^").unwrap();
        assert_eq!(state.node(n).path(), "b");
    }
}
