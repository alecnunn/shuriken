//! The build plan: which edges we intend to run, and in what order.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use crate::dyndep::DyndepFile;
use crate::error::{Error, Result};
use crate::graph::DependencyScan;
use crate::state::{EdgeId, NodeId, PoolId, State};
use crate::status::Status;

/// What we intend to do with an edge.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Want {
    /// We do not want to build this edge, but we may want a dependent of it.
    Nothing,
    /// We want to build it and have not scheduled it yet.
    ToStart,
    /// It has been scheduled and we are waiting for it.
    ToFinish,
}

/// How an edge ended.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EdgeResult {
    /// The command failed.
    Failed,
    /// The command succeeded.
    Succeeded,
}

/// Priority key for the ready queue: highest critical-path weight first, then
/// lowest edge id (i.e. manifest order).
#[derive(PartialEq, Eq, Debug)]
struct ReadyKey {
    weight: i64,
    id: u32,
}

impl Ord for ReadyKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` is a max-heap, so "greater" must mean "run sooner".
        self.weight
            .cmp(&other.weight)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for ReadyKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Ordering of edges delayed by a full pool: lightest first, then highest
/// priority, matching ninja's `WeightedEdgeCmp`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct DelayedKey {
    weight: i32,
    negated_critical_weight: i64,
    id: u32,
}

/// The set of edges we want to build, and the queue of those ready to run.
#[derive(Default)]
pub struct Plan {
    want: BTreeMap<EdgeId, Want>,
    ready: BinaryHeap<ReadyKey>,
    delayed: BTreeMap<PoolId, BTreeSet<DelayedKey>>,
    targets: Vec<NodeId>,
    command_edges: i64,
    wanted_edges: i64,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("want", &self.want.len())
            .field("ready", &self.ready.len())
            .field("command_edges", &self.command_edges)
            .field("wanted_edges", &self.wanted_edges)
            .finish()
    }
}

impl Plan {
    /// An empty plan.
    pub fn new() -> Plan {
        Plan::default()
    }

    /// Forget everything.
    pub fn reset(&mut self) {
        self.command_edges = 0;
        self.wanted_edges = 0;
        self.ready.clear();
        self.want.clear();
        self.delayed.clear();
        self.targets.clear();
    }

    /// True while there is still work to do.
    pub fn more_to_do(&self) -> bool {
        self.wanted_edges > 0 && self.command_edges > 0
    }

    /// The number of edges with commands still in the plan.
    pub fn command_edge_count(&self) -> i64 {
        self.command_edges
    }

    /// The number of edges we still want to run, including phony ones.
    pub fn wanted_edge_count(&self) -> i64 {
        self.wanted_edges
    }

    /// What we intend to do with `edge`, if it is in the plan.
    pub fn want(&self, edge: EdgeId) -> Option<Want> {
        self.want.get(&edge).copied()
    }

    /// Add `target` and everything it depends on to the plan.
    ///
    /// Returns false if there is nothing to do for this target.
    pub fn add_target(
        &mut self,
        state: &State,
        target: NodeId,
        status: &mut dyn Status,
    ) -> Result<bool> {
        self.targets.push(target);
        self.add_sub_target(state, target, None, status, None)
    }

    fn add_sub_target(
        &mut self,
        state: &State,
        node: NodeId,
        dependent: Option<NodeId>,
        status: &mut dyn Status,
        mut dyndep_walk: Option<&mut BTreeSet<EdgeId>>,
    ) -> Result<bool> {
        let Some(edge) = state.node(node).in_edge() else {
            // A leaf. If it is a manifest input and missing, the build cannot
            // proceed; if it came from a depfile or dyndep file, ignore it.
            if state.node(node).dirty && !state.node(node).generated_by_dep_loader {
                let referenced = match dependent {
                    Some(d) => format!(", needed by '{}',", state.node(d).path()),
                    None => String::new(),
                };
                return Err(Error::build(format!(
                    "'{}'{} missing and no known rule to make it",
                    state.node(node).path(),
                    referenced
                )));
            }
            return Ok(false);
        };

        if state.edge(edge).outputs_ready() {
            return Ok(false); // Nothing to do.
        }

        let first_visit = !self.want.contains_key(&edge);
        let want = *self.want.entry(edge).or_insert(Want::Nothing);

        if dyndep_walk.is_some() && want == Want::ToFinish {
            return Ok(false); // Already scheduled.
        }

        if state.node(node).dirty && want == Want::Nothing {
            self.want.insert(edge, Want::ToStart);
            self.edge_wanted(state, edge, status);
        }

        if let Some(walk) = dyndep_walk.as_mut() {
            walk.insert(edge);
        }

        if !first_visit {
            return Ok(true); // Inputs already processed.
        }

        for input in state.edge(edge).inputs().to_vec() {
            self.add_sub_target(
                state,
                input,
                Some(node),
                status,
                dyndep_walk.as_mut().map(|w| &mut **w),
            )?;
        }

        Ok(true)
    }

    fn edge_wanted(&mut self, state: &State, edge: EdgeId, status: &mut dyn Status) {
        self.wanted_edges += 1;
        if !state.edge_is_phony(edge) {
            self.command_edges += 1;
            status.edge_added_to_plan(state, edge);
        }
    }

    /// Compute scheduling priorities and fill the ready queue. Call once, after
    /// all targets have been added.
    pub fn prepare_queue(&mut self, state: &mut State) {
        self.compute_critical_path(state);
        self.schedule_initial_edges(state);
    }

    /// Take the next edge that is ready to run.
    pub fn find_work(&mut self) -> Option<EdgeId> {
        self.ready.pop().map(|k| EdgeId(k.id))
    }

    /// True if no edge is ready to run right now.
    pub fn ready_is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    fn push_ready(&mut self, state: &State, edge: EdgeId) {
        self.ready.push(ReadyKey {
            weight: state.edge(edge).critical_path_weight(),
            id: edge.0,
        });
    }

    fn delay_edge(&mut self, state: &State, edge: EdgeId) {
        let pool = state.edge(edge).pool();
        let e = state.edge(edge);
        self.delayed.entry(pool).or_default().insert(DelayedKey {
            weight: e.weight(),
            negated_critical_weight: -e.critical_path_weight(),
            id: edge.0,
        });
    }

    /// Move as many delayed edges of `pool` as fit into the ready queue.
    fn retrieve_ready_edges(&mut self, state: &mut State, pool: PoolId) {
        let Some(delayed) = self.delayed.get_mut(&pool) else {
            return;
        };
        let mut promoted: Vec<EdgeId> = Vec::new();
        {
            let depth = state.pool(pool).depth();
            let mut current_use = state.pool(pool).current_use();
            for key in delayed.iter() {
                if current_use + key.weight > depth {
                    break;
                }
                current_use += key.weight;
                promoted.push(EdgeId(key.id));
            }
        }
        for edge in &promoted {
            let key = DelayedKey {
                weight: state.edge(*edge).weight(),
                negated_critical_weight: -state.edge(*edge).critical_path_weight(),
                id: edge.0,
            };
            self.delayed.get_mut(&pool).unwrap().remove(&key);
        }
        for edge in promoted {
            let w = state.edge(edge).weight();
            state.pool_mut(pool).edge_scheduled(w);
            self.push_ready(state, edge);
        }
    }

    fn schedule_work(&mut self, state: &mut State, edge: EdgeId) {
        match self.want.get(&edge) {
            Some(Want::ToFinish) => {
                // Already scheduled. This happens when an edge and one of its
                // dependencies share an order-only input, or when a node
                // appears twice in an edge's inputs.
                return;
            }
            _ => {
                debug_assert_eq!(self.want.get(&edge), Some(&Want::ToStart));
            }
        }
        self.want.insert(edge, Want::ToFinish);

        let pool = state.edge(edge).pool();
        if state.pool(pool).should_delay_edge() {
            self.delay_edge(state, edge);
            self.retrieve_ready_edges(state, pool);
        } else {
            let w = state.edge(edge).weight();
            state.pool_mut(pool).edge_scheduled(w);
            self.push_ready(state, edge);
        }
    }

    /// Mark `edge` as done.
    ///
    /// Returns the nodes that carry dyndep information which now needs
    /// loading; the caller should load each one and call [`Plan::dyndeps_loaded`].
    pub fn edge_finished(
        &mut self,
        state: &mut State,
        edge: EdgeId,
        result: EdgeResult,
        status: &mut dyn Status,
    ) -> Result<Vec<NodeId>> {
        let mut pending = Vec::new();
        self.edge_finished_inner(state, edge, result, status, &mut pending)?;
        Ok(pending)
    }

    fn edge_finished_inner(
        &mut self,
        state: &mut State,
        edge: EdgeId,
        result: EdgeResult,
        status: &mut dyn Status,
        pending: &mut Vec<NodeId>,
    ) -> Result<()> {
        let want = match self.want.get(&edge) {
            Some(w) => *w,
            None => return Err(Error::build("internal error: finished an edge not in the plan")),
        };
        let directly_wanted = want != Want::Nothing;

        // Free up any delayed jobs in this edge's pool.
        let pool = state.edge(edge).pool();
        if directly_wanted {
            let w = state.edge(edge).weight();
            state.pool_mut(pool).edge_finished(w);
        }
        self.retrieve_ready_edges(state, pool);

        if result != EdgeResult::Succeeded {
            return Ok(());
        }

        if directly_wanted {
            self.wanted_edges -= 1;
        }
        self.want.remove(&edge);
        state.edge_mut(edge).outputs_ready = true;

        for output in state.edge(edge).outputs().to_vec() {
            self.node_finished(state, output, status, pending)?;
        }
        Ok(())
    }

    fn node_finished(
        &mut self,
        state: &mut State,
        node: NodeId,
        status: &mut dyn Status,
        pending: &mut Vec<NodeId>,
    ) -> Result<()> {
        // If this node carries dyndep information, it has to be loaded before
        // we can tell what else became ready.
        if state.node(node).dyndep_pending {
            pending.push(node);
            return Ok(());
        }

        for out_edge in state.node(node).out_edges().to_vec() {
            if !self.want.contains_key(&out_edge) {
                continue;
            }
            self.edge_maybe_ready(state, out_edge, status, pending)?;
        }
        Ok(())
    }

    fn edge_maybe_ready(
        &mut self,
        state: &mut State,
        edge: EdgeId,
        status: &mut dyn Status,
        pending: &mut Vec<NodeId>,
    ) -> Result<()> {
        if !state.all_inputs_ready(edge) {
            return Ok(());
        }
        match self.want.get(&edge) {
            Some(Want::Nothing) => {
                // We do not need this edge itself, but a dependent may need
                // its outputs to be marked ready.
                self.edge_finished_inner(state, edge, EdgeResult::Succeeded, status, pending)?;
            }
            Some(_) => self.schedule_work(state, edge),
            None => {}
        }
        Ok(())
    }

    /// Mark `node` clean during the build (a `restat` rule found its output
    /// unchanged) and propagate that through the plan.
    pub fn clean_node(
        &mut self,
        scan: &mut DependencyScan,
        node: NodeId,
        status: &mut dyn Status,
    ) -> Result<()> {
        scan.state_mut().node_mut(node).dirty = false;

        for out_edge in scan.state().node(node).out_edges().to_vec() {
            // Skip edges we do not want, and edges whose deps failed to load.
            match self.want.get(&out_edge) {
                None | Some(Want::Nothing) => continue,
                _ => {}
            }
            if scan.state().edge(out_edge).deps_missing {
                continue;
            }

            let non_order_only: Vec<NodeId> = {
                let e = scan.state().edge(out_edge);
                let end = e.inputs().len() - e.order_only_deps();
                e.inputs()[..end].to_vec()
            };

            // Only reconsider this edge once all its real inputs are clean.
            if non_order_only
                .iter()
                .any(|&i| scan.state().node(i).dirty)
            {
                continue;
            }

            let mut most_recent_input: Option<NodeId> = None;
            for &i in &non_order_only {
                if most_recent_input
                    .is_none_or(|m| scan.state().node(i).mtime > scan.state().node(m).mtime)
                {
                    most_recent_input = Some(i);
                }
            }

            let outputs_dirty = scan.recompute_outputs_dirty(out_edge, most_recent_input)?;
            if outputs_dirty {
                continue;
            }

            for output in scan.state().edge(out_edge).outputs().to_vec() {
                self.clean_node(scan, output, status)?;
            }

            self.want.insert(out_edge, Want::Nothing);
            self.wanted_edges -= 1;
            if !scan.state().edge_is_phony(out_edge) {
                self.command_edges -= 1;
                status.edge_removed_from_plan(scan.state(), out_edge);
            }
        }
        Ok(())
    }

    /// Update the plan after a dyndep file has been loaded.
    pub fn dyndeps_loaded(
        &mut self,
        scan: &mut DependencyScan,
        node: NodeId,
        ddf: &DyndepFile,
        status: &mut dyn Status,
    ) -> Result<()> {
        // Everything that depends on this node may have become dirty.
        self.refresh_dyndep_dependents(scan, node, status)?;

        // Walk the newly reachable part of the graph, starting from the edges
        // that are already in the plan.
        let mut dyndep_walk: BTreeSet<EdgeId> = BTreeSet::new();
        let roots: Vec<EdgeId> = ddf
            .entries
            .keys()
            .copied()
            .filter(|&e| !scan.state().edge(e).outputs_ready() && self.want.contains_key(&e))
            .collect();

        for edge in roots {
            let out0 = scan.state().edge(edge).outputs()[0];
            let inputs = ddf.entries[&edge].implicit_inputs.clone();
            for input in inputs {
                self.add_sub_target(
                    scan.state(),
                    input,
                    Some(out0),
                    status,
                    Some(&mut dyndep_walk),
                )?;
            }
        }

        // Also consider the out edges of this node, as `node_finished` would
        // have done had it not taken the dyndep path.
        for out_edge in scan.state().node(node).out_edges().to_vec() {
            if self.want.contains_key(&out_edge) {
                dyndep_walk.insert(out_edge);
            }
        }

        let mut pending = Vec::new();
        for edge in dyndep_walk {
            if !self.want.contains_key(&edge) {
                continue;
            }
            self.edge_maybe_ready(scan.state_mut(), edge, status, &mut pending)?;
        }

        for node in pending {
            let ddf = scan.load_dyndeps(node)?;
            self.dyndeps_loaded(scan, node, &ddf, status)?;
        }
        Ok(())
    }

    fn refresh_dyndep_dependents(
        &mut self,
        scan: &mut DependencyScan,
        node: NodeId,
        status: &mut dyn Status,
    ) -> Result<()> {
        // Collect the transitive dependents and let them be re-scanned.
        let mut dependents: BTreeSet<NodeId> = BTreeSet::new();
        self.unmark_dependents(scan.state_mut(), node, &mut dependents);

        for n in dependents {
            let mut validation_nodes = Vec::new();
            scan.recompute_dirty(n, &mut validation_nodes)?;

            for v in validation_nodes {
                if let Some(in_edge) = scan.state().node(v).in_edge() {
                    if !scan.state().edge(in_edge).outputs_ready() {
                        self.add_target(scan.state(), v, status)?;
                    }
                }
            }

            if !scan.state().node(n).dirty {
                continue;
            }

            // The edge was seen before, but we may not have wanted it because
            // its outputs were not known to be dirty. Now they are.
            let Some(edge) = scan.state().node(n).in_edge() else {
                continue;
            };
            if self.want.get(&edge) == Some(&Want::Nothing) {
                self.want.insert(edge, Want::ToStart);
                self.edge_wanted(scan.state(), edge, status);
            }
        }
        Ok(())
    }

    fn unmark_dependents(
        &self,
        state: &mut State,
        node: NodeId,
        dependents: &mut BTreeSet<NodeId>,
    ) {
        for out_edge in state.node(node).out_edges().to_vec() {
            if !self.want.contains_key(&out_edge) {
                continue;
            }
            if state.edge(out_edge).mark != crate::state::VisitMark::None {
                state.edge_mut(out_edge).mark = crate::state::VisitMark::None;
                for output in state.edge(out_edge).outputs().to_vec() {
                    if dependents.insert(output) {
                        self.unmark_dependents(state, output, dependents);
                    }
                }
            }
        }
    }

    /// Weight every edge by the longest path from it to any target, so that
    /// long dependency chains start early.
    fn compute_critical_path(&mut self, state: &mut State) {
        // Topological sort of everything reachable from the targets: each edge
        // appears after the edges producing its inputs.
        let mut visited: BTreeSet<EdgeId> = BTreeSet::new();
        let mut sorted: Vec<EdgeId> = Vec::new();

        // Iterative depth-first search to avoid deep recursion on big graphs.
        enum Step {
            Visit(EdgeId),
            Emit(EdgeId),
        }
        let mut stack: Vec<Step> = Vec::new();
        for &target in self.targets.iter().rev() {
            if let Some(e) = state.node(target).in_edge() {
                stack.push(Step::Visit(e));
            }
        }
        while let Some(step) = stack.pop() {
            match step {
                Step::Visit(edge) => {
                    if !visited.insert(edge) {
                        continue;
                    }
                    stack.push(Step::Emit(edge));
                    for input in state.edge(edge).inputs() {
                        if let Some(producer) = state.node(*input).in_edge() {
                            if !visited.contains(&producer) {
                                stack.push(Step::Visit(producer));
                            }
                        }
                    }
                }
                Step::Emit(edge) => sorted.push(edge),
            }
        }

        let weight_of = |state: &State, edge: EdgeId| -> i64 {
            if state.edge_is_phony(edge) { 0 } else { 1 }
        };

        for &edge in &sorted {
            let w = weight_of(state, edge);
            state.edge_mut(edge).critical_path_weight = w;
        }

        // Propagate weights from dependents to their producers.
        for &edge in sorted.iter().rev() {
            let edge_weight = state.edge(edge).critical_path_weight();
            for input in state.edge(edge).inputs().to_vec() {
                let Some(producer) = state.node(input).in_edge() else {
                    continue;
                };
                let candidate = edge_weight + weight_of(state, producer);
                if candidate > state.edge(producer).critical_path_weight() {
                    state.edge_mut(producer).critical_path_weight = candidate;
                }
            }
        }
    }

    fn schedule_initial_edges(&mut self, state: &mut State) {
        debug_assert!(self.ready.is_empty());
        let mut pools: BTreeSet<PoolId> = BTreeSet::new();

        let candidates: Vec<EdgeId> = self
            .want
            .iter()
            .filter(|&(_, &w)| w == Want::ToStart)
            .map(|(&e, _)| e)
            .collect();

        for edge in candidates {
            if !state.all_inputs_ready(edge) {
                continue;
            }
            let pool = state.edge(edge).pool();
            if state.pool(pool).should_delay_edge() {
                // Delay now and retrieve later, so the highest priority edges
                // are taken first rather than whichever came first in the map.
                self.want.insert(edge, Want::ToFinish);
                self.delay_edge(state, edge);
                pools.insert(pool);
            } else {
                self.schedule_work(state, edge);
            }
        }

        for pool in pools {
            self.retrieve_ready_edges(state, pool);
        }
    }

    /// A description of the plan, for debugging.
    pub fn dump(&self, state: &State) -> String {
        let mut s = format!("pending: {}\n", self.want.len());
        for (&edge, &want) in &self.want {
            if want != Want::Nothing {
                s.push_str("want ");
            }
            s.push_str(&state.format_edge(edge));
            s.push('\n');
        }
        s.push_str(&format!("ready: {}\n", self.ready.len()));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::parse::{ManifestParser, ParserOptions};
    use crate::status::NullStatus;

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

    fn scan_target(state: &mut State, disk: &MemDisk, target: &str) -> NodeId {
        let node = state.lookup_node(target).unwrap();
        let mut scan = DependencyScan::new(state, disk, None, None, None);
        let mut v = Vec::new();
        scan.recompute_dirty(node, &mut v).unwrap();
        node
    }

    #[test]
    fn basic_chain_runs_in_order() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        let mut state = setup(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
        let c = scan_target(&mut state, &disk, "c");

        let mut plan = Plan::new();
        let mut status = NullStatus;
        assert!(plan.add_target(&state, c, &mut status).unwrap());
        assert_eq!(plan.command_edge_count(), 2);
        plan.prepare_queue(&mut state);

        // Only `b` is ready at first.
        let first = plan.find_work().unwrap();
        assert_eq!(state.node(state.edge(first).outputs()[0]).path(), "b");
        assert!(plan.find_work().is_none());

        // Finishing `b` releases `c`.
        state.node_mut(state.edge(first).outputs()[0]).set_mtime(1);
        plan.edge_finished(&mut state, first, EdgeResult::Succeeded, &mut status)
            .unwrap();
        let second = plan.find_work().unwrap();
        assert_eq!(state.node(state.edge(second).outputs()[0]).path(), "c");
        plan.edge_finished(&mut state, second, EdgeResult::Succeeded, &mut status)
            .unwrap();
        assert!(!plan.more_to_do());
    }

    #[test]
    fn missing_input_is_an_error() {
        let disk = MemDisk::new();
        let mut state = setup(&disk, &format!("{CAT}build b: cat a\n"));
        let b = scan_target(&mut state, &disk, "b");
        let mut plan = Plan::new();
        let mut status = NullStatus;
        let err = plan.add_target(&state, b, &mut status).unwrap_err();
        assert!(
            err.to_string()
                .contains("'a', needed by 'b', missing and no known rule to make it"),
            "{err}"
        );
    }

    #[test]
    fn phony_edges_have_no_commands() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        let mut state = setup(&disk, &format!("{CAT}build b: cat a\nbuild all: phony b\n"));
        let all = scan_target(&mut state, &disk, "all");
        let mut plan = Plan::new();
        let mut status = NullStatus;
        plan.add_target(&state, all, &mut status).unwrap();
        assert_eq!(plan.command_edge_count(), 1);
        assert_eq!(plan.wanted_edge_count(), 2);
    }

    #[test]
    fn pool_limits_concurrency() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        disk.create("b", "b");
        let manifest = format!(
            "pool p\n  depth = 1\n\n\
             rule cat2\n  command = cat $in > $out\n  pool = p\n\n\
             build x: cat2 a\nbuild y: cat2 b\nbuild all: phony x y\n{CAT}"
        );
        let mut state = setup(&disk, &manifest);
        let all = scan_target(&mut state, &disk, "all");
        let mut plan = Plan::new();
        let mut status = NullStatus;
        plan.add_target(&state, all, &mut status).unwrap();
        plan.prepare_queue(&mut state);

        let first = plan.find_work().unwrap();
        // The pool only allows one at a time.
        assert!(plan.find_work().is_none());
        state.node_mut(state.edge(first).outputs()[0]).set_mtime(1);
        plan.edge_finished(&mut state, first, EdgeResult::Succeeded, &mut status)
            .unwrap();
        assert!(plan.find_work().is_some());
    }

    #[test]
    fn critical_path_prefers_long_chains() {
        let disk = MemDisk::new();
        disk.create("src", "x");
        // `short` is one step; `long1 -> long2` is two, so the long chain
        // should start first even though it comes later in the manifest.
        let manifest = format!(
            "{CAT}build short: cat src\nbuild long1: cat src\nbuild long2: cat long1\n\
             build all: phony short long2\n"
        );
        let mut state = setup(&disk, &manifest);
        let all = scan_target(&mut state, &disk, "all");
        let mut plan = Plan::new();
        let mut status = NullStatus;
        plan.add_target(&state, all, &mut status).unwrap();
        plan.prepare_queue(&mut state);

        let first = plan.find_work().unwrap();
        assert_eq!(state.node(state.edge(first).outputs()[0]).path(), "long1");
    }

    #[test]
    fn order_only_input_still_gates_scheduling() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        let mut state = setup(&disk, &format!("{CAT}build oo: cat a\nbuild out: cat a || oo\n"));
        let out = scan_target(&mut state, &disk, "out");
        let mut plan = Plan::new();
        let mut status = NullStatus;
        plan.add_target(&state, out, &mut status).unwrap();
        plan.prepare_queue(&mut state);

        let first = plan.find_work().unwrap();
        assert_eq!(state.node(state.edge(first).outputs()[0]).path(), "oo");
        assert!(plan.find_work().is_none());
    }
}
