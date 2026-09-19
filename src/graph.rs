//! Deciding what is out of date.
//!
//! [`DependencyScan`] walks the graph from a target, stats files, loads
//! discovered dependencies (depfiles, the deps log, dyndep files) and marks
//! nodes dirty. It reproduces ninja's `DependencyScan` exactly, including the
//! `restat` and `generator` special cases and the `-d explain` diagnostics.

use std::collections::VecDeque;

use crate::build_log::BuildLog;
use crate::canon::canonicalize_path;
use crate::depfile::parse_depfile;
use crate::deps_log::DepsLog;
use crate::disk::DiskInterface;
use crate::dyndep::{DyndepFile, load_dyndeps};
use crate::error::{Error, Result};
use crate::hash::{FxHashMap, hash_command};
use crate::state::{EdgeId, NodeId, State, VisitMark};

/// Per-node explanations of why something is dirty, collected for
/// `-d explain`.
#[derive(Default, Debug)]
pub struct Explanations {
    map: FxHashMap<NodeId, Vec<String>>,
}

impl Explanations {
    /// An empty collection.
    pub fn new() -> Explanations {
        Explanations::default()
    }

    /// Record an explanation for `node`.
    pub fn record(&mut self, node: NodeId, message: String) {
        self.map.entry(node).or_default().push(message);
    }

    /// Append (and keep) the explanations recorded for `node`.
    pub fn lookup_and_append(&self, node: NodeId, out: &mut Vec<String>) {
        if let Some(v) = self.map.get(&node) {
            out.extend(v.iter().cloned());
        }
    }

    /// True if nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// All recorded explanations, for printing.
    pub fn all(&self) -> impl Iterator<Item = &String> {
        self.map.values().flatten()
    }
}

/// Scans the graph and updates the dirty state of nodes and edges.
pub struct DependencyScan<'a> {
    state: &'a mut State,
    disk: &'a dyn DiskInterface,
    build_log: Option<&'a BuildLog>,
    deps_log: Option<&'a DepsLog>,
    explanations: Option<&'a mut Explanations>,
}

impl<'a> DependencyScan<'a> {
    /// Create a scanner.
    pub fn new(
        state: &'a mut State,
        disk: &'a dyn DiskInterface,
        build_log: Option<&'a BuildLog>,
        deps_log: Option<&'a DepsLog>,
        explanations: Option<&'a mut Explanations>,
    ) -> DependencyScan<'a> {
        DependencyScan {
            state,
            disk,
            build_log,
            deps_log,
            explanations,
        }
    }

    /// The graph being scanned.
    pub fn state(&self) -> &State {
        self.state
    }

    /// Mutable access to the graph.
    pub fn state_mut(&mut self) -> &mut State {
        self.state
    }

    /// The build log, if one is loaded.
    pub fn build_log(&self) -> Option<&BuildLog> {
        self.build_log
    }

    fn explain(&mut self, node: NodeId, message: String) {
        if let Some(e) = self.explanations.as_mut() {
            e.record(node, message);
        }
    }

    fn explaining(&self) -> bool {
        self.explanations.is_some()
    }

    fn path(&self, node: NodeId) -> String {
        self.state.node(node).path().to_string()
    }

    fn stat_if_necessary(&mut self, node: NodeId) -> Result<()> {
        if self.state.node(node).status_known() {
            return Ok(());
        }
        let path = self.path(node);
        let mtime = self.disk.stat(&path)?;
        self.state.node_mut(node).set_mtime(mtime);
        Ok(())
    }

    /// Update the dirty state of `node` and everything it depends on.
    ///
    /// Validation nodes discovered along the way are appended to
    /// `validation_nodes`; the caller should add them to the build as extra
    /// top-level targets.
    pub fn recompute_dirty(
        &mut self,
        node: NodeId,
        validation_nodes: &mut Vec<NodeId>,
    ) -> Result<()> {
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        queue.push_back(node);

        while let Some(node) = queue.pop_front() {
            let mut stack = Vec::new();
            let mut new_validation_nodes = Vec::new();
            self.recompute_node_dirty(node, &mut stack, &mut new_validation_nodes)?;
            queue.extend(new_validation_nodes.iter().copied());
            validation_nodes.append(&mut new_validation_nodes);
        }
        Ok(())
    }

    fn recompute_node_dirty(
        &mut self,
        node: NodeId,
        stack: &mut Vec<NodeId>,
        validation_nodes: &mut Vec<NodeId>,
    ) -> Result<()> {
        let Some(edge) = self.state.node(node).in_edge() else {
            // A leaf: dirty exactly when it is missing.
            if self.state.node(node).status_known() {
                return Ok(());
            }
            self.stat_if_necessary(node)?;
            if !self.state.node(node).exists() && self.explaining() {
                let p = self.path(node);
                self.explain(node, format!("{p} has no in-edge and is missing"));
            }
            let exists = self.state.node(node).exists();
            self.state.node_mut(node).dirty = !exists;
            return Ok(());
        };

        if self.state.edge(edge).mark == VisitMark::Done {
            return Ok(());
        }
        self.verify_dag(node, stack)?;

        self.state.edge_mut(edge).mark = VisitMark::InStack;
        stack.push(node);

        let mut dirty = false;
        {
            let e = self.state.edge_mut(edge);
            e.outputs_ready = true;
            e.deps_missing = false;
        }

        if !self.state.edge(edge).deps_loaded {
            // First encounter with this edge. If it has a dyndep file that is
            // already up to date, load it now so we see its extra inputs and
            // outputs; otherwise the edge cannot be ready anyway, and the file
            // will be loaded during the build.
            if let Some(dyndep) = self.state.edge(edge).dyndep() {
                if self.state.node(dyndep).dyndep_pending {
                    self.recompute_node_dirty(dyndep, stack, validation_nodes)?;
                    let ready = match self.state.node(dyndep).in_edge() {
                        None => true,
                        Some(e) => self.state.edge(e).outputs_ready,
                    };
                    if ready {
                        self.load_dyndeps(dyndep)?;
                    }
                }
            }
        }

        // Stat the outputs so we can compare them against the newest input.
        for i in 0..self.state.edge(edge).outputs().len() {
            let o = self.state.edge(edge).outputs()[i];
            self.stat_if_necessary(o)?;
        }

        if !self.state.edge(edge).deps_loaded {
            self.state.edge_mut(edge).deps_loaded = true;
            if !self.load_deps(edge)? {
                // Dependency info is missing or stale: rebuild to regenerate it.
                dirty = true;
                self.state.edge_mut(edge).deps_missing = true;
            }
        }

        // Validation nodes are not recursed into here: that would trip the
        // cycle detector when a validation depends on this node.
        validation_nodes.extend(self.state.edge(edge).validations().iter().copied());

        // Visit the inputs.
        let input_count = self.state.edge(edge).inputs().len();
        let order_only = self.state.edge(edge).order_only_deps();
        let first_order_only = input_count - order_only;
        let mut most_recent_input: Option<NodeId> = None;

        for i in 0..input_count {
            let input = self.state.edge(edge).inputs()[i];
            self.recompute_node_dirty(input, stack, validation_nodes)?;

            if let Some(in_edge) = self.state.node(input).in_edge() {
                if !self.state.edge(in_edge).outputs_ready {
                    self.state.edge_mut(edge).outputs_ready = false;
                }
            }

            if i >= first_order_only {
                continue; // Order-only inputs never make us dirty.
            }

            if self.state.node(input).dirty {
                if self.explaining() {
                    let p = self.path(input);
                    self.explain(node, format!("{p} is dirty"));
                }
                dirty = true;
            } else if most_recent_input
                .is_none_or(|m| self.state.node(input).mtime > self.state.node(m).mtime)
            {
                most_recent_input = Some(input);
            }
        }

        if !dirty {
            dirty = self.recompute_outputs_dirty(edge, most_recent_input)?;
        }

        if dirty {
            for i in 0..self.state.edge(edge).outputs().len() {
                let o = self.state.edge(edge).outputs()[i];
                self.state.node_mut(o).dirty = true;
            }
        }

        // A dirty edge's outputs are not ready. Phony edges with no inputs
        // have nothing to do, so they are always ready.
        let is_phony = self.state.edge_is_phony(edge);
        if dirty && !(is_phony && self.state.edge(edge).inputs().is_empty()) {
            self.state.edge_mut(edge).outputs_ready = false;
        }

        self.state.edge_mut(edge).mark = VisitMark::Done;
        debug_assert_eq!(stack.last(), Some(&node));
        stack.pop();
        Ok(())
    }

    fn verify_dag(&mut self, node: NodeId, stack: &[NodeId]) -> Result<()> {
        let edge = self
            .state
            .node(node)
            .in_edge()
            .expect("verify_dag requires an in-edge");
        if self.state.edge(edge).mark != VisitMark::InStack {
            return Ok(());
        }

        // The edge is already on the stack: find where, and report the cycle.
        let start = stack
            .iter()
            .position(|&n| self.state.node(n).in_edge() == Some(edge))
            .expect("edge marked in-stack must be on the stack");

        // Report the cycle starting at the node that closed it, so that
        // `build a b: cat c` / `build c: cat a` reports a -> c -> a.
        let mut cycle: Vec<NodeId> = stack[start..].to_vec();
        cycle[0] = node;

        let mut err = String::from("dependency cycle: ");
        for &n in &cycle {
            err.push_str(self.state.node(n).path());
            err.push_str(" -> ");
        }
        err.push_str(self.state.node(cycle[0]).path());

        if cycle.len() == 1 {
            let is_phony = self.state.edge_is_phony(edge);
            if self.state.edge(edge).maybe_phonycycle_diagnostic(is_phony) {
                err.push_str(" [-w phonycycle=err]");
            }
        }
        Err(Error::build(err))
    }

    /// Recompute whether any output of `edge` is dirty.
    pub fn recompute_outputs_dirty(
        &mut self,
        edge: EdgeId,
        most_recent_input: Option<NodeId>,
    ) -> Result<bool> {
        let command = self.state.edge_command_for_hash(edge);
        for i in 0..self.state.edge(edge).outputs().len() {
            let output = self.state.edge(edge).outputs()[i];
            if self.recompute_output_dirty(edge, most_recent_input, &command, output) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn recompute_output_dirty(
        &mut self,
        edge: EdgeId,
        most_recent_input: Option<NodeId>,
        command: &str,
        output: NodeId,
    ) -> bool {
        if self.state.edge_is_phony(edge) {
            // Phony edges produce nothing. Their outputs are only dirty when
            // there are no inputs at all and the file is missing.
            if self.state.edge(edge).inputs().is_empty() && !self.state.node(output).exists() {
                if self.explaining() {
                    let p = self.path(output);
                    self.explain(
                        output,
                        format!("output {p} of phony edge with no inputs doesn't exist"),
                    );
                }
                return true;
            }
            // Expose the newest input mtime so dependents can compare against
            // the phony node.
            if let Some(m) = most_recent_input {
                let mtime = self.state.node(m).mtime;
                self.state.node_mut(output).update_phony_mtime(mtime);
            }
            return false;
        }

        if !self.state.node(output).exists() {
            if self.explaining() {
                let p = self.path(output);
                self.explain(output, format!("output {p} doesn't exist"));
            }
            return true;
        }

        let explaining = self.explaining();

        // A `restat` rule may have "cleaned" this output in a previous run, in
        // which case the mtime recorded in the log is authoritative and the
        // file's own mtime is ignored.
        let output_path: &str = self.state.node(output).path();
        let mut entry = None;
        let mut used_restat = false;
        if self.state.edge_restat(edge) {
            if let Some(log) = self.build_log {
                if let Some(e) = log.lookup_by_output(output_path) {
                    entry = Some(e);
                    used_restat = true;
                }
            }
        }

        if !used_restat {
            if let Some(m) = most_recent_input {
                let out_mtime = self.state.node(output).mtime;
                let in_mtime = self.state.node(m).mtime;
                if out_mtime < in_mtime {
                    if explaining {
                        let op = output_path.to_string();
                        let ip = self.path(m);
                        self.explain(
                            output,
                            format!(
                                "output {op} older than most recent input {ip} \
                                 ({out_mtime} vs {in_mtime})"
                            ),
                        );
                    }
                    return true;
                }
            }
        }

        if let Some(log) = self.build_log {
            let generator = self.state.edge_is_generator(edge);
            if entry.is_none() {
                entry = log.lookup_by_output(output_path);
            }
            if let Some(entry) = entry {
                if !generator && hash_command(command) != entry.command_hash {
                    if explaining {
                        let op = output_path.to_string();
                        self.explain(output, format!("command line changed for {op}"));
                    }
                    return true;
                }
                if let Some(m) = most_recent_input {
                    let in_mtime = self.state.node(m).mtime;
                    if entry.mtime < in_mtime {
                        // The recorded mtime can be older than the file's own
                        // mtime if a previous run wrote the output but failed
                        // or was interrupted.
                        if explaining {
                            let op = output_path.to_string();
                            let em = entry.mtime;
                            let ip = self.path(m);
                            self.explain(
                                output,
                                format!(
                                    "recorded mtime of {op} older than most recent \
                                     input {ip} ({em} vs {in_mtime})"
                                ),
                            );
                        }
                        return true;
                    }
                }
            } else if !generator {
                if explaining {
                    let op = output_path.to_string();
                    self.explain(output, format!("command line not found in log for {op}"));
                }
                return true;
            }
        }

        false
    }

    /// Load a dyndep file and apply it to the graph.
    pub fn load_dyndeps(&mut self, node: NodeId) -> Result<DyndepFile> {
        load_dyndeps(self.state, self.disk, node)
    }

    // ---- discovered dependencies -------------------------------------------

    /// Load implicit dependencies for `edge`.
    ///
    /// Returns `Ok(false)` when the information is missing or out of date (the
    /// edge must then be rebuilt), and an error only for real failures.
    pub fn load_deps(&mut self, edge: EdgeId) -> Result<bool> {
        let deps_type = self.state.edge_binding(edge, "deps");
        if !deps_type.is_empty() {
            return self.load_deps_from_log(edge);
        }
        let depfile = self.state.edge_depfile(edge);
        if !depfile.is_empty() {
            return self.load_depfile(edge, &depfile);
        }
        Ok(true)
    }

    fn load_depfile(&mut self, edge: EdgeId, path: &str) -> Result<bool> {
        // A missing depfile is treated as an empty one.
        let content = self.disk.read_file(path)?.unwrap_or_default();
        let first_output = self.state.edge(edge).outputs()[0];

        if content.is_empty() {
            if self.explaining() {
                self.explain(first_output, format!("depfile '{path}' is missing"));
            }
            return Ok(false);
        }

        let mut depfile =
            parse_depfile(&content).map_err(|e| Error::build(format!("{path}: {e}")))?;
        if depfile.outs.is_empty() {
            return Err(Error::build(format!("{path}: no outputs declared")));
        }

        canonicalize_path(&mut depfile.outs[0]);
        let opath = self.state.node(first_output).path().to_string();
        if opath != depfile.outs[0] {
            if self.explaining() {
                let got = depfile.outs[0].clone();
                self.explain(
                    first_output,
                    format!("expected depfile '{path}' to mention '{opath}', got '{got}'"),
                );
            }
            return Ok(false);
        }

        // Every output the depfile mentions must be an output of the edge.
        let edge_outputs: Vec<String> = self
            .state
            .edge(edge)
            .outputs()
            .iter()
            .map(|&o| self.state.node(o).path().to_string())
            .collect();
        for o in &depfile.outs {
            if !edge_outputs.iter().any(|p| p == o) {
                return Err(Error::build(format!(
                    "{path}: depfile mentions '{o}' as an output, but no such output was declared"
                )));
            }
        }

        let mut nodes = Vec::with_capacity(depfile.ins.len());
        for input in &depfile.ins {
            let mut p = input.clone();
            let slash_bits = canonicalize_path(&mut p);
            nodes.push(self.state.get_node(&p, slash_bits));
        }
        self.insert_implicit_deps(edge, &nodes);
        Ok(true)
    }

    fn load_deps_from_log(&mut self, edge: EdgeId) -> Result<bool> {
        // `deps` is only supported for the first output of an edge.
        let output = self.state.edge(edge).outputs()[0];
        let deps = match self.deps_log {
            Some(log) => log.get_deps(self.state, output).cloned(),
            None => None,
        };
        let Some(deps) = deps else {
            if self.explaining() {
                let p = self.path(output);
                self.explain(output, format!("deps for '{p}' are missing"));
            }
            return Ok(false);
        };

        let out_mtime = self.state.node(output).mtime;
        if out_mtime > deps.mtime {
            if self.explaining() {
                let p = self.path(output);
                let dm = deps.mtime;
                self.explain(
                    output,
                    format!("stored deps info out of date for '{p}' ({dm} vs {out_mtime})"),
                );
            }
            return Ok(false);
        }

        self.insert_implicit_deps(edge, &deps.nodes);
        Ok(true)
    }

    /// Splice `nodes` into `edge`'s inputs as implicit dependencies, before any
    /// order-only inputs.
    fn insert_implicit_deps(&mut self, edge: EdgeId, nodes: &[NodeId]) {
        if nodes.is_empty() {
            return;
        }
        {
            let e = self.state.edge(edge);
            let pos = e.inputs.len() - e.order_only_deps;
            let count = nodes.len();
            let e = self.state.edge_mut(edge);
            e.inputs.splice(pos..pos, nodes.iter().copied());
            e.implicit_deps += count;
        }
        for &n in nodes {
            self.state.node_mut(n).out_edges.push(edge);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::parse::{ManifestParser, ParserOptions};

    fn build_state(disk: &MemDisk, manifest: &str) -> State {
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

    const CAT: &str = "rule cat\n  command = cat $in > $out\n\n";

    #[test]
    fn missing_output_is_dirty() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        let mut state = build_state(&disk, &format!("{CAT}build out: cat in\n"));
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(state.node(out).dirty);
    }

    #[test]
    fn up_to_date_output_is_clean() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        disk.tick();
        disk.create("out", "x");
        let mut state = build_state(&disk, &format!("{CAT}build out: cat in\n"));
        let out = state.lookup_node("out").unwrap();
        // Without a build log, ninja has no command hash to compare, so a
        // scan with no log leaves the output clean.
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(!state.node(out).dirty);
    }

    #[test]
    fn older_output_is_dirty() {
        let disk = MemDisk::new();
        disk.create("out", "x");
        disk.tick();
        disk.create("in", "x");
        let mut state = build_state(&disk, &format!("{CAT}build out: cat in\n"));
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(state.node(out).dirty);
    }

    #[test]
    fn missing_input_is_not_an_error_until_planned() {
        let disk = MemDisk::new();
        let mut state = build_state(&disk, &format!("{CAT}build out: cat in\n"));
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        let input = state.lookup_node("in").unwrap();
        assert!(state.node(input).dirty);
        assert!(state.node(out).dirty);
    }

    #[test]
    fn cycle_is_detected() {
        let disk = MemDisk::new();
        let mut state = build_state(&disk, &format!("{CAT}build a: cat b\nbuild b: cat a\n"));
        let a = state.lookup_node("a").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        let e = scan.recompute_dirty(a, &mut v).unwrap_err();
        assert!(e.to_string().starts_with("dependency cycle: "), "{e}");
    }

    #[test]
    fn phony_with_no_inputs_is_clean_when_output_exists() {
        let disk = MemDisk::new();
        disk.create("out", "x");
        let mut state = build_state(&disk, "build out: phony\n");
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(!state.node(out).dirty);
    }

    #[test]
    fn phony_with_no_inputs_and_missing_output_is_dirty() {
        let disk = MemDisk::new();
        let mut state = build_state(&disk, "build out: phony\n");
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(state.node(out).dirty);
    }

    #[test]
    fn depfile_is_loaded() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        disk.create("out.d", "out: extra.h\n");
        disk.create("extra.h", "x");
        disk.tick();
        disk.create("out", "x");
        let mut state = build_state(
            &disk,
            "rule cc\n  command = cc $in\n  depfile = $out.d\n\nbuild out: cc in\n",
        );
        let out = state.lookup_node("out").unwrap();
        {
            let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
            let mut v = Vec::new();
            scan.recompute_dirty(out, &mut v).unwrap();
        }
        let edge = crate::state::EdgeId(0);
        assert_eq!(state.edge(edge).inputs().len(), 2);
        assert_eq!(state.edge(edge).implicit_deps(), 1);
        assert!(state.lookup_node("extra.h").is_some());
        assert!(!state.node(out).dirty);
    }

    #[test]
    fn depfile_with_newer_dep_makes_output_dirty() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        disk.create("out", "x");
        disk.create("out.d", "out: extra.h\n");
        disk.tick();
        disk.create("extra.h", "newer");
        let mut state = build_state(
            &disk,
            "rule cc\n  command = cc $in\n  depfile = $out.d\n\nbuild out: cc in\n",
        );
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(state.node(out).dirty);
    }

    #[test]
    fn missing_depfile_makes_output_dirty() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        disk.tick();
        disk.create("out", "x");
        let mut state = build_state(
            &disk,
            "rule cc\n  command = cc $in\n  depfile = $out.d\n\nbuild out: cc in\n",
        );
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(state.node(out).dirty);
        assert!(state.edge(crate::state::EdgeId(0)).deps_missing);
    }

    #[test]
    fn order_only_input_does_not_dirty_output() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        disk.create("out", "x");
        disk.tick();
        disk.create("oo", "newer");
        let mut state = build_state(&disk, &format!("{CAT}build out: cat in || oo\n"));
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert!(!state.node(out).dirty);
    }

    #[test]
    fn explanations_are_recorded() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        let mut state = build_state(&disk, &format!("{CAT}build out: cat in\n"));
        let out = state.lookup_node("out").unwrap();
        let mut explanations = Explanations::new();
        {
            let mut scan =
                DependencyScan::new(&mut state, &disk, None, None, Some(&mut explanations));
            let mut v = Vec::new();
            scan.recompute_dirty(out, &mut v).unwrap();
        }
        let mut msgs = Vec::new();
        explanations.lookup_and_append(out, &mut msgs);
        assert!(msgs.iter().any(|m| m.contains("doesn't exist")), "{msgs:?}");
    }

    #[test]
    fn validation_nodes_are_collected() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        let mut state = build_state(
            &disk,
            &format!("{CAT}build out: cat in |@ validate\nbuild validate: cat in\n"),
        );
        let out = state.lookup_node("out").unwrap();
        let mut scan = DependencyScan::new(&mut state, &disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(out, &mut v).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(state.node(v[0]).path(), "validate");
    }
}
