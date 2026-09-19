//! Running a build: turning a plan into processes, then recording what
//! happened.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::build_log::BuildLog;
use crate::canon::canonicalize_path;
use crate::depfile::parse_depfile;
use crate::deps_log::DepsLog;
use crate::disk::DiskInterface;
use crate::error::{Error, Result};
use crate::exec::{CommandResult, CommandRunner, DryRunCommandRunner, ExitStatus, RealCommandRunner};
use crate::graph::{DependencyScan, Explanations};
use crate::msvc;
use crate::plan::{EdgeResult, Plan};
use crate::state::{EdgeId, NodeId, State};
use crate::status::Status;
use crate::util::now_millis;

pub use crate::status::Verbosity;

/// Options controlling how a build runs.
#[derive(Clone, Debug)]
pub struct BuildConfig {
    /// How much to print.
    pub verbosity: Verbosity,
    /// Report success without running anything (`-n`).
    pub dry_run: bool,
    /// How many commands to run at once (`-j`).
    pub parallelism: usize,
    /// How many failures to tolerate before stopping (`-k`).
    pub failures_allowed: usize,
    /// Do not start new commands while the load average is above this (`-l`).
    /// Negative values disable the limit.
    pub max_load_average: f64,
    /// Keep depfiles after reading them (`-d keepdepfile`).
    pub keep_depfile: bool,
    /// Keep response files after a command succeeds (`-d keeprsp`).
    pub keep_rsp: bool,
    /// Collect and print why each target is being rebuilt (`-d explain`).
    pub explain: bool,
}

impl Default for BuildConfig {
    fn default() -> Self {
        BuildConfig {
            verbosity: Verbosity::Normal,
            dry_run: false,
            parallelism: crate::util::guess_parallelism(),
            failures_allowed: 1,
            max_load_average: -1.0,
            keep_depfile: false,
            keep_rsp: false,
            explain: false,
        }
    }
}

/// Drives a build: scans targets, schedules edges, runs commands and updates
/// the logs.
pub struct Builder<'a> {
    state: &'a mut State,
    config: BuildConfig,
    plan: Plan,
    disk: &'a dyn DiskInterface,
    build_log: Option<&'a mut BuildLog>,
    deps_log: Option<&'a mut DepsLog>,
    status: &'a mut dyn Status,
    start_time_millis: i64,
    running_edges: BTreeMap<EdgeId, i64>,
    lock_file_path: String,
    explanations: Option<Explanations>,
    exit_code: ExitStatus,
    command_runner: Option<Box<dyn CommandRunner + 'a>>,
    interrupt: Option<Arc<AtomicBool>>,
    edges_started: u64,
    edges_finished: u64,
}

impl<'a> Builder<'a> {
    /// Create a builder.
    ///
    /// `start_time_millis` is the time the whole build started, used for the
    /// timings recorded in the build log.
    pub fn new(
        state: &'a mut State,
        config: BuildConfig,
        build_log: Option<&'a mut BuildLog>,
        deps_log: Option<&'a mut DepsLog>,
        disk: &'a dyn DiskInterface,
        status: &'a mut dyn Status,
        start_time_millis: i64,
    ) -> Builder<'a> {
        let build_dir = state.global_binding("builddir");
        let lock_file_path = if build_dir.is_empty() {
            ".ninja_lock".to_string()
        } else {
            format!("{build_dir}/.ninja_lock")
        };
        let explanations = if config.explain {
            Some(Explanations::new())
        } else {
            None
        };
        Builder {
            state,
            config,
            plan: Plan::new(),
            disk,
            build_log,
            deps_log,
            status,
            start_time_millis,
            running_edges: BTreeMap::new(),
            lock_file_path,
            explanations,
            exit_code: ExitStatus::SUCCESS,
            command_runner: None,
            interrupt: None,
            edges_started: 0,
            edges_finished: 0,
        }
    }

    /// Share an interrupt flag, so Ctrl-C can stop the build cleanly.
    pub fn set_interrupt_flag(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt = Some(flag);
    }

    /// Substitute a different command runner (for tests, or to execute
    /// commands somewhere other than local subprocesses).
    pub fn set_command_runner(&mut self, runner: Box<dyn CommandRunner + 'a>) {
        self.command_runner = Some(runner);
    }

    /// The build plan.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The exit code to use for the process.
    pub fn exit_code(&self) -> ExitStatus {
        self.exit_code
    }

    /// How many edges were started.
    pub fn edges_started(&self) -> u64 {
        self.edges_started
    }

    /// How many edges finished.
    pub fn edges_finished(&self) -> u64 {
        self.edges_finished
    }

    /// Explanations collected when `explain` is enabled.
    pub fn explanations(&self) -> Option<&Explanations> {
        self.explanations.as_ref()
    }

    /// Add a target by path, resolving it in the graph first.
    pub fn add_target_by_name(&mut self, name: &str) -> Result<NodeId> {
        let node = self
            .state
            .lookup_node(name)
            .ok_or_else(|| Error::build(format!("unknown target: '{name}'")))?;
        self.add_target(node)?;
        Ok(node)
    }

    /// Scan `target` and add whatever needs building to the plan.
    pub fn add_target(&mut self, target: NodeId) -> Result<()> {
        let mut validation_nodes = Vec::new();
        {
            let mut scan = DependencyScan::new(
                &mut *self.state,
                self.disk,
                self.build_log.as_deref(),
                self.deps_log.as_deref(),
                self.explanations.as_mut(),
            );
            scan.recompute_dirty(target, &mut validation_nodes)?;
        }

        let needs_building = match self.state.node(target).in_edge() {
            None => true,
            Some(e) => !self.state.edge(e).outputs_ready(),
        };
        if needs_building {
            self.plan
                .add_target(&*self.state, target, &mut *self.status)?;
        }

        // Validations found while scanning become extra top-level targets.
        for v in validation_nodes {
            if let Some(in_edge) = self.state.node(v).in_edge() {
                if !self.state.edge(in_edge).outputs_ready() {
                    self.plan.add_target(&*self.state, v, &mut *self.status)?;
                }
            }
        }
        Ok(())
    }

    /// True if there is nothing to do.
    pub fn already_up_to_date(&self) -> bool {
        !self.plan.more_to_do()
    }

    /// Run the build.
    pub fn build(&mut self) -> Result<ExitStatus> {
        if self.already_up_to_date() {
            return Ok(ExitStatus::SUCCESS);
        }
        self.plan.prepare_queue(&mut *self.state);

        let mut pending_commands = 0usize;
        let mut failures_allowed = self.config.failures_allowed;

        if self.command_runner.is_none() {
            self.command_runner = Some(if self.config.dry_run {
                Box::new(DryRunCommandRunner::new())
            } else {
                let mut runner = RealCommandRunner::new(self.config.parallelism);
                runner.set_max_load_average(self.config.max_load_average);
                if let Some(flag) = &self.interrupt {
                    runner.set_interrupt_flag(Arc::clone(flag));
                }
                Box::new(runner)
            });
        }

        self.status.build_started();

        // The main loop: start as many commands as we are allowed to, then reap
        // the next one that finishes.
        while self.plan.more_to_do() {
            if failures_allowed > 0 {
                let mut capacity = self.runner().can_run_more();
                while capacity > 0 {
                    let Some(edge) = self.plan.find_work() else { break };

                    // A generator may rewrite (or delete) the build log, so
                    // close our handle on it first.
                    if self.state.edge_is_generator(edge) {
                        if let Some(log) = self.build_log.as_deref_mut() {
                            log.close()?;
                        }
                    }

                    if let Err(e) = self.start_edge(edge) {
                        self.cleanup();
                        self.status.build_finished();
                        self.exit_code = ExitStatus::FAILURE;
                        return Err(e);
                    }

                    if self.state.edge_is_phony(edge) {
                        if let Err(e) = self.plan_edge_finished(edge, EdgeResult::Succeeded) {
                            self.cleanup();
                            self.status.build_finished();
                            self.exit_code = ExitStatus::FAILURE;
                            return Err(e);
                        }
                    } else {
                        pending_commands += 1;
                        capacity -= 1;
                        // Re-check: the load average may have changed.
                        let current = self.runner().can_run_more();
                        if current < capacity {
                            capacity = current;
                        }
                    }
                }

                if pending_commands == 0 && !self.plan.more_to_do() {
                    break;
                }
            }

            if pending_commands > 0 {
                let result = self.runner_mut().wait_for_command();
                let mut result = match result {
                    Some(r) if !r.status.interrupted() => r,
                    _ => {
                        self.cleanup();
                        self.status.build_finished();
                        self.exit_code = ExitStatus::INTERRUPTED;
                        return Err(Error::Interrupted);
                    }
                };

                pending_commands -= 1;
                let finish = self.finish_command(&mut result);
                self.set_failure_code(result.status);
                if let Err(e) = finish {
                    self.cleanup();
                    self.status.build_finished();
                    if result.success() {
                        self.set_failure_code(ExitStatus::FAILURE);
                    }
                    return Err(e);
                }

                if !result.success() {
                    failures_allowed = failures_allowed.saturating_sub(1);
                }

                // We made progress; go around again.
                continue;
            }

            // No progress is possible.
            self.status.build_finished();
            return Err(Error::build(if failures_allowed == 0 {
                if self.config.failures_allowed > 1 {
                    "subcommands failed".to_string()
                } else {
                    "subcommand failed".to_string()
                }
            } else if failures_allowed < self.config.failures_allowed {
                "cannot make progress due to previous errors".to_string()
            } else {
                "stuck [this is a bug]".to_string()
            }));
        }

        self.status.build_finished();
        Ok(ExitStatus::SUCCESS)
    }

    fn runner(&self) -> &dyn CommandRunner {
        self.command_runner
            .as_deref()
            .expect("command runner is created before the build loop")
    }

    fn runner_mut(&mut self) -> &mut dyn CommandRunner {
        self.command_runner
            .as_deref_mut()
            .expect("command runner is created before the build loop")
    }

    fn set_failure_code(&mut self, code: ExitStatus) {
        if !code.success() {
            self.exit_code = code;
        }
    }

    /// Prepare and launch one edge's command.
    pub fn start_edge(&mut self, edge: EdgeId) -> Result<()> {
        if self.state.edge_is_phony(edge) {
            return Ok(());
        }

        let start_time_millis = now_millis() - self.start_time_millis;
        self.running_edges.insert(edge, start_time_millis);
        self.edges_started += 1;

        self.report_explanations(edge);
        self.status
            .build_edge_started(&*self.state, edge, start_time_millis);

        // Create the output directories, and note the filesystem's idea of
        // "now" so `restat` rules can tell whether a command touched anything.
        let mut build_start: i64 = if self.config.dry_run { 0 } else { -1 };
        let outputs: Vec<String> = self
            .state
            .edge(edge)
            .outputs()
            .iter()
            .map(|&o| self.state.node(o).path().to_string())
            .collect();
        for path in &outputs {
            self.disk.make_dirs(path)?;
            if build_start == -1 {
                self.disk.write_file(&self.lock_file_path, "", false)?;
                build_start = self.disk.stat(&self.lock_file_path).unwrap_or(0);
            }
        }
        self.state.edge_mut(edge).command_start_time = build_start;

        let depfile = self.state.edge_depfile(edge);
        if !depfile.is_empty() {
            self.disk.make_dirs(&depfile)?;
        }

        let rspfile = self.state.edge_rspfile(edge);
        if !rspfile.is_empty() {
            let content = self.state.edge_binding(edge, "rspfile_content");
            self.disk.write_file(&rspfile, &content, true)?;
        }

        let state: &State = self.state;
        self.command_runner
            .as_deref_mut()
            .expect("command runner")
            .start_command(state, edge)
            .map_err(|e| {
                Error::build(format!("command '{}' failed: {e}", state.edge_command(edge)))
            })
    }

    /// Record the outcome of one command.
    pub fn finish_command(&mut self, result: &mut CommandResult) -> Result<()> {
        let edge = result.edge;

        // Extract dependencies first: it filters the output (we want to strip
        // `/showIncludes` noise even from a failing compile) and can itself
        // fail, which makes the edge fail.
        let deps_type = self.state.edge_binding(edge, "deps");
        let deps_prefix = self.state.edge_binding(edge, "msvc_deps_prefix");
        let mut deps_nodes: Vec<NodeId> = Vec::new();
        if !deps_type.is_empty() {
            match self.extract_deps(result, &deps_type, &deps_prefix) {
                Ok(nodes) => deps_nodes = nodes,
                Err(e) => {
                    if result.success() {
                        if !result.output.is_empty() {
                            result.output.push('\n');
                        }
                        result.output.push_str(&e.to_string());
                        result.status = ExitStatus::FAILURE;
                    }
                }
            }
        }

        let start_time_millis = self.running_edges.remove(&edge).unwrap_or(0);
        let end_time_millis = now_millis() - self.start_time_millis;
        self.edges_finished += 1;

        self.report_explanations(edge);
        self.status.build_edge_finished(
            &*self.state,
            edge,
            start_time_millis,
            end_time_millis,
            result.status,
            &result.output,
        );

        if !result.success() {
            self.plan_edge_finished(edge, EdgeResult::Failed)?;
            return Ok(());
        }

        // Re-stat the outputs where it matters.
        let mut record_mtime = 0i64;
        if !self.config.dry_run {
            let restat = self.state.edge_restat(edge);
            let generator = self.state.edge_is_generator(edge);
            let mut node_cleaned = false;
            record_mtime = self.state.edge(edge).command_start_time;

            // `restat` and `generator` rules must look at the outputs again.
            // If we could not time the command's start, fall back to the
            // outputs' current mtimes.
            if record_mtime == 0 || restat || generator {
                for output in self.state.edge(edge).outputs().to_vec() {
                    let path = self.state.node(output).path().to_string();
                    let new_mtime = self.disk.stat(&path)?;
                    if new_mtime > record_mtime {
                        record_mtime = new_mtime;
                    }
                    if restat && self.state.node(output).mtime == new_mtime {
                        // The command did not change this output, so anything
                        // depending on it is still clean. This also covers
                        // outputs that do not exist (mtime 0).
                        let mut scan = DependencyScan::new(
                            &mut *self.state,
                            self.disk,
                            self.build_log.as_deref(),
                            self.deps_log.as_deref(),
                            self.explanations.as_mut(),
                        );
                        self.plan
                            .clean_node(&mut scan, output, &mut *self.status)?;
                        node_cleaned = true;
                    }
                }
            }
            if node_cleaned {
                record_mtime = self.state.edge(edge).command_start_time;
            }
        }

        let pending_dyndeps = {
            let state = &mut *self.state;
            self.plan
                .edge_finished(state, edge, EdgeResult::Succeeded, &mut *self.status)?
        };

        // Delete the response file, if any.
        let rspfile = self.state.edge_rspfile(edge);
        if !rspfile.is_empty() && !self.config.keep_rsp {
            self.disk.remove_file(&rspfile)?;
        }

        {
            let state: &State = self.state;
            if let Some(log) = self.build_log.as_deref_mut() {
                log.record_command(
                    state,
                    edge,
                    start_time_millis as i32,
                    end_time_millis as i32,
                    record_mtime,
                )?;
            }
        }

        if !deps_type.is_empty() && !self.config.dry_run {
            for output in self.state.edge(edge).outputs().to_vec() {
                let path = self.state.node(output).path().to_string();
                let deps_mtime = self.disk.stat(&path)?;
                let state = &mut *self.state;
                if let Some(log) = self.deps_log.as_deref_mut() {
                    log.record_deps(state, output, deps_mtime, &deps_nodes)?;
                }
            }
        }

        // Load any dyndep information that just became available.
        for node in pending_dyndeps {
            self.load_dyndeps(node)?;
        }

        Ok(())
    }

    fn plan_edge_finished(&mut self, edge: EdgeId, result: EdgeResult) -> Result<()> {
        let pending = {
            let state = &mut *self.state;
            self.plan
                .edge_finished(state, edge, result, &mut *self.status)?
        };
        for node in pending {
            self.load_dyndeps(node)?;
        }
        Ok(())
    }

    /// Load the dyndep file `node` names and fold it into the plan.
    pub fn load_dyndeps(&mut self, node: NodeId) -> Result<()> {
        let mut scan = DependencyScan::new(
            &mut *self.state,
            self.disk,
            self.build_log.as_deref(),
            self.deps_log.as_deref(),
            self.explanations.as_mut(),
        );
        let ddf = scan.load_dyndeps(node)?;
        self.plan
            .dyndeps_loaded(&mut scan, node, &ddf, &mut *self.status)
    }

    fn extract_deps(
        &mut self,
        result: &mut CommandResult,
        deps_type: &str,
        deps_prefix: &str,
    ) -> Result<Vec<NodeId>> {
        match deps_type {
            "msvc" => {
                let parsed = msvc::parse_show_includes(&result.output, deps_prefix);
                result.output = parsed.filtered_output;
                let mut nodes = Vec::with_capacity(parsed.includes.len());
                for include in parsed.includes {
                    // MSVC paths are reported with backslashes; ninja records
                    // them with all separators marked as backslashes.
                    nodes.push(self.state.get_node(&include, u64::MAX));
                }
                Ok(nodes)
            }
            "gcc" => {
                let depfile = self.state.edge_depfile(result.edge);
                if depfile.is_empty() {
                    return Err(Error::build("edge with deps=gcc but no depfile makes no sense"));
                }
                // A missing depfile is treated as empty.
                let content = self.disk.read_file(&depfile)?.unwrap_or_default();
                if content.is_empty() {
                    return Ok(Vec::new());
                }
                let deps = parse_depfile(&content)
                    .map_err(|e| Error::build(format!("{depfile}: {e}")))?;
                let mut nodes = Vec::with_capacity(deps.ins.len());
                for input in deps.ins {
                    let mut path = input;
                    let slash_bits = canonicalize_path(&mut path);
                    nodes.push(self.state.get_node(&path, slash_bits));
                }
                if !self.config.keep_depfile {
                    self.disk.remove_file(&depfile)?;
                }
                Ok(nodes)
            }
            other => Err(Error::build(format!("unknown deps type '{other}'"))),
        }
    }

    fn report_explanations(&mut self, edge: EdgeId) {
        let Some(explanations) = self.explanations.as_ref() else {
            return;
        };
        let mut lines = Vec::new();
        for &output in self.state.edge(edge).outputs() {
            explanations.lookup_and_append(output, &mut lines);
        }
        if !lines.is_empty() {
            self.status.explanations(&lines);
        }
    }

    /// Delete the outputs of interrupted commands, so a later build does not
    /// mistake a half-written file for a finished one.
    pub fn cleanup(&mut self) {
        if let Some(runner) = self.command_runner.as_deref_mut() {
            let active = runner.active_edges();
            runner.abort();

            for edge in active {
                let depfile = self.state.edge_depfile(edge);
                for output in self.state.edge(edge).outputs().to_vec() {
                    let path = self.state.node(output).path().to_string();
                    // Only delete an output that was actually modified: we do
                    // not want to lose a manifest we could have kept. But if
                    // the rule uses a depfile, always delete, since the command
                    // may have written the depfile and not the output.
                    let new_mtime = match self.disk.stat(&path) {
                        Ok(m) => m,
                        Err(e) => {
                            self.status.error(&e.to_string());
                            continue;
                        }
                    };
                    if !depfile.is_empty() || self.state.node(output).mtime != new_mtime {
                        let _ = self.disk.remove_file(&path);
                    }
                }
                if !depfile.is_empty() {
                    let _ = self.disk.remove_file(&depfile);
                }
            }
        }

        if self.disk.stat(&self.lock_file_path).unwrap_or(0) > 0 {
            let _ = self.disk.remove_file(&self.lock_file_path);
        }
    }
}

impl Drop for Builder<'_> {
    fn drop(&mut self) {
        // Mirrors ninja's Builder destructor: remove the lock file, and any
        // half-written outputs of commands that never finished.
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::parse::{ManifestParser, ParserOptions};
    use crate::status::RecordingStatus;

    /// A command runner that records commands and pretends they succeed,
    /// optionally touching outputs in a `MemDisk`.
    struct FakeRunner<'d> {
        disk: &'d MemDisk,
        commands: Vec<String>,
        queue: std::collections::VecDeque<EdgeId>,
        fail: Option<String>,
        touch_outputs: bool,
    }

    impl<'d> FakeRunner<'d> {
        fn new(disk: &'d MemDisk) -> FakeRunner<'d> {
            FakeRunner {
                disk,
                commands: Vec::new(),
                queue: Default::default(),
                fail: None,
                touch_outputs: true,
            }
        }
    }

    impl CommandRunner for FakeRunner<'_> {
        fn can_run_more(&self) -> usize {
            4
        }

        fn start_command(&mut self, state: &State, edge: EdgeId) -> Result<()> {
            self.commands.push(state.edge_command(edge));
            if self.touch_outputs {
                self.disk.tick();
                for &o in state.edge(edge).outputs() {
                    self.disk.create(state.node(o).path(), "built");
                }
            }
            self.queue.push_back(edge);
            Ok(())
        }

        fn wait_for_command(&mut self) -> Option<CommandResult> {
            let edge = self.queue.pop_front()?;
            let status = match &self.fail {
                Some(_) => ExitStatus(1),
                None => ExitStatus::SUCCESS,
            };
            Some(CommandResult {
                edge,
                status,
                output: self.fail.clone().unwrap_or_default(),
            })
        }

        fn active_edges(&self) -> Vec<EdgeId> {
            self.queue.iter().copied().collect()
        }

        fn abort(&mut self) {
            self.queue.clear();
        }
    }

    const CAT: &str = "rule cat\n  command = cat $in > $out\n\n";

    fn parse_state(disk: &MemDisk, manifest: &str) -> State {
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
    fn builds_a_chain_once() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        let mut state = parse_state(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
        let mut status = RecordingStatus::default();
        let mut log = BuildLog::new();

        let commands = {
            let mut builder = Builder::new(
                &mut state,
                BuildConfig {
                    verbosity: Verbosity::Quiet,
                    ..Default::default()
                },
                Some(&mut log),
                None,
                &disk,
                &mut status,
                0,
            );
            let runner = FakeRunner::new(&disk);
            builder.set_command_runner(Box::new(runner));
            builder.add_target_by_name("c").unwrap();
            assert!(!builder.already_up_to_date());
            builder.build().unwrap();
            builder.edges_finished()
        };
        assert_eq!(commands, 2);
        assert!(disk.contains("b"));
        assert!(disk.contains("c"));
        assert_eq!(log.entries().len(), 2);
    }

    #[test]
    fn nothing_to_do_when_up_to_date() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        disk.tick();
        disk.create("b", "b");
        let mut state = parse_state(&disk, &format!("{CAT}build b: cat a\n"));
        let mut status = RecordingStatus::default();
        let mut builder = Builder::new(
            &mut state,
            BuildConfig {
                verbosity: Verbosity::Quiet,
                ..Default::default()
            },
            None,
            None,
            &disk,
            &mut status,
            0,
        );
        builder.add_target_by_name("b").unwrap();
        assert!(builder.already_up_to_date());
    }

    #[test]
    fn failure_stops_the_build() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        let mut state = parse_state(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
        let mut status = RecordingStatus::default();
        let mut builder = Builder::new(
            &mut state,
            BuildConfig {
                verbosity: Verbosity::Quiet,
                ..Default::default()
            },
            None,
            None,
            &disk,
            &mut status,
            0,
        );
        let mut runner = FakeRunner::new(&disk);
        runner.fail = Some("boom".to_string());
        runner.touch_outputs = false;
        builder.set_command_runner(Box::new(runner));
        builder.add_target_by_name("c").unwrap();
        let err = builder.build().unwrap_err();
        assert!(err.to_string().contains("subcommand failed"), "{err}");
        assert_eq!(builder.exit_code(), ExitStatus(1));
    }

    #[test]
    fn dry_run_touches_nothing() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        let mut state = parse_state(&disk, &format!("{CAT}build b: cat a\n"));
        let mut status = RecordingStatus::default();
        {
            let mut builder = Builder::new(
                &mut state,
                BuildConfig {
                    verbosity: Verbosity::Quiet,
                    dry_run: true,
                    ..Default::default()
                },
                None,
                None,
                &disk,
                &mut status,
                0,
            );
            builder.add_target_by_name("b").unwrap();
            builder.build().unwrap();
        }
        assert!(!disk.contains("b"));
    }

    #[test]
    fn rspfile_is_written_and_removed() {
        let disk = MemDisk::new();
        disk.create("a", "a");
        let mut state = parse_state(
            &disk,
            "rule link\n  command = link @$out.rsp\n  rspfile = $out.rsp\n  \
             rspfile_content = $in\n\nbuild b: link a\n",
        );
        let mut status = RecordingStatus::default();
        {
            let mut builder = Builder::new(
                &mut state,
                BuildConfig {
                    verbosity: Verbosity::Quiet,
                    ..Default::default()
                },
                None,
                None,
                &disk,
                &mut status,
                0,
            );
            builder.set_command_runner(Box::new(FakeRunner::new(&disk)));
            builder.add_target_by_name("b").unwrap();
            builder.build().unwrap();
        }
        // Written during the build, deleted afterwards.
        assert!(!disk.contains("b.rsp"));
    }
}
