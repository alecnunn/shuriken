//! The high-level entry point: load a manifest, keep the logs, run builds.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::build::{BuildConfig, Builder};
use crate::build_log::{BuildLog, LoadStatus};
use crate::canon::canonicalize_path;
use crate::clean::{CleanReport, Cleaner};
use crate::deps_log::DepsLog;
use crate::disk::{DiskInterface, RealDiskInterface};
use crate::error::{Error, Result};
use crate::exec::{CommandRunner, ExitStatus};
use crate::parse::{ManifestParser, ParserOptions};
use crate::state::{NodeId, State};
use crate::status::{NullStatus, Status};
use crate::tools;
use crate::util::now_millis;

/// How many times the manifest may regenerate itself before we give up.
const MANIFEST_REBUILD_LIMIT: usize = 100;

/// Options for [`Engine`].
pub struct EngineOptions {
    /// How the build itself behaves.
    pub build: BuildConfig,
    /// How the manifest is parsed.
    pub parser: ParserOptions,
    /// Rebuild the manifest first if the manifest is itself a build output.
    pub rebuild_manifest: bool,
    /// A flag that, when set, stops the build as if Ctrl-C had been pressed.
    pub interrupt: Option<Arc<AtomicBool>>,
    /// The filesystem to use; defaults to the real one.
    pub disk: Option<Box<dyn DiskInterface>>,
    /// How to execute commands; defaults to local subprocesses.
    ///
    /// This is the hook for running commands somewhere else (a sandbox, a
    /// container, a remote worker) or for recording them instead.
    pub command_runner: Option<Arc<CommandRunnerFactory>>,
}

/// Builds a [`CommandRunner`] for a build, given its configuration.
pub type CommandRunnerFactory = dyn Fn(&BuildConfig) -> Box<dyn CommandRunner + Send> + Send + Sync;

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            build: BuildConfig::default(),
            parser: ParserOptions::default(),
            rebuild_manifest: true,
            interrupt: None,
            disk: None,
            command_runner: None,
        }
    }
}

impl std::fmt::Debug for EngineOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineOptions")
            .field("build", &self.build)
            .field("parser", &self.parser)
            .field("rebuild_manifest", &self.rebuild_manifest)
            .finish_non_exhaustive()
    }
}

/// Where the time went, for `-d stats`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    /// Reading and parsing the manifest (including `include`/`subninja`).
    pub parse: Duration,
    /// Loading the build and deps logs.
    pub logs: Duration,
    /// Scanning the graph and building the plan.
    pub scan: Duration,
    /// Running commands.
    pub build: Duration,
}

/// What a build did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildSummary {
    /// True if nothing needed doing.
    pub up_to_date: bool,
    /// How many edges were started.
    pub edges_started: u64,
    /// How many edges finished.
    pub edges_finished: u64,
    /// True if the manifest regenerated itself and the build restarted.
    pub manifest_rebuilt: bool,
}

/// A loaded build: the graph, the logs, and the options to build it with.
pub struct Engine {
    state: State,
    disk: Box<dyn DiskInterface>,
    build_log: BuildLog,
    deps_log: DepsLog,
    build_dir: String,
    manifest_path: String,
    options: EngineOptions,
    warnings: Vec<String>,
    exit_code: ExitStatus,
    build_log_found: bool,
    deps_log_found: bool,
    timings: Timings,
}

impl Engine {
    /// Load `manifest_path` and open the build and deps logs beside it.
    pub fn load(manifest_path: impl Into<String>, options: EngineOptions) -> Result<Engine> {
        let manifest_path = manifest_path.into();
        let mut options = options;
        let disk: Box<dyn DiskInterface> = match options.disk.take() {
            Some(d) => d,
            None => Box::new(RealDiskInterface::new()),
        };

        let mut engine = Engine {
            state: State::new(),
            disk,
            build_log: BuildLog::new(),
            deps_log: DepsLog::new(),
            build_dir: String::new(),
            manifest_path,
            options,
            warnings: Vec::new(),
            exit_code: ExitStatus::SUCCESS,
            build_log_found: false,
            deps_log_found: false,
            timings: Timings::default(),
        };
        engine.parse_manifest()?;
        engine.ensure_build_dir_exists()?;
        engine.open_logs()?;
        Ok(engine)
    }

    /// Load a manifest from memory, without opening any logs.
    ///
    /// Useful for embedders that generate manifests programmatically and drive
    /// the graph themselves.
    pub fn from_manifest_text(
        filename: &str,
        text: &str,
        options: EngineOptions,
    ) -> Result<Engine> {
        let mut options = options;
        let disk: Box<dyn DiskInterface> = match options.disk.take() {
            Some(d) => d,
            None => Box::new(RealDiskInterface::new()),
        };
        let mut engine = Engine {
            state: State::new(),
            disk,
            build_log: BuildLog::new(),
            deps_log: DepsLog::new(),
            build_dir: String::new(),
            manifest_path: filename.to_string(),
            options,
            warnings: Vec::new(),
            exit_code: ExitStatus::SUCCESS,
            build_log_found: false,
            deps_log_found: false,
            timings: Timings::default(),
        };
        {
            let mut parser = ManifestParser::new(
                &mut engine.state,
                &*engine.disk,
                engine.options.parser.clone(),
            );
            parser.parse_text(filename, text.as_bytes())?;
            engine.warnings = parser.take_warnings();
        }
        engine.build_dir = engine.state.global_binding("builddir");
        Ok(engine)
    }

    fn parse_manifest(&mut self) -> Result<()> {
        let start = Instant::now();
        let mut parser =
            ManifestParser::new(&mut self.state, &*self.disk, self.options.parser.clone());
        parser.load(&self.manifest_path)?;
        self.warnings.extend(parser.take_warnings());
        self.timings.parse += start.elapsed();
        Ok(())
    }

    /// Where the time has gone so far.
    pub fn timings(&self) -> Timings {
        self.timings
    }

    fn ensure_build_dir_exists(&mut self) -> Result<()> {
        self.build_dir = self.state.global_binding("builddir");
        if !self.build_dir.is_empty() && !self.options.build.dry_run {
            let marker = format!("{}/.", self.build_dir);
            self.disk.make_dirs(&marker)?;
        }
        Ok(())
    }

    /// The path of the build log.
    pub fn build_log_path(&self) -> String {
        self.log_path(".ninja_log")
    }

    /// The path of the deps log.
    pub fn deps_log_path(&self) -> String {
        self.log_path(".ninja_deps")
    }

    fn log_path(&self, name: &str) -> String {
        if self.build_dir.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", self.build_dir, name)
        }
    }

    fn open_logs(&mut self) -> Result<()> {
        let start = Instant::now();
        let log_path = self.build_log_path();
        let result = self.build_log.load(&log_path)?;
        self.build_log_found = result.status == LoadStatus::Success;
        if let Some(w) = result.warning {
            self.warnings.push(w);
        }

        let deps_path = self.deps_log_path();
        let result = self.deps_log.load(&deps_path, &mut self.state)?;
        self.deps_log_found = result.status == LoadStatus::Success;
        if let Some(w) = result.warning {
            self.warnings.push(w);
        }

        if !self.options.build.dry_run {
            {
                let state = &self.state;
                let disk = &*self.disk;
                let is_path_dead = |path: &str| is_path_dead(state, disk, path);
                self.build_log.open_for_write(&log_path, &is_path_dead)?;
            }
            self.deps_log.open_for_write(&deps_path, &mut self.state)?;
        }
        self.timings.logs += start.elapsed();
        Ok(())
    }

    /// Recompact both logs (`-t recompact`).
    ///
    /// A log that does not exist is left alone rather than created.
    pub fn recompact_logs(&mut self) -> Result<()> {
        if self.build_log_found {
            let log_path = self.build_log_path();
            let state = &self.state;
            let disk = &*self.disk;
            let is_path_dead = |path: &str| is_path_dead(state, disk, path);
            self.build_log.recompact(&log_path, &is_path_dead)?;
        }
        if self.deps_log_found {
            let deps_path = self.deps_log_path();
            self.deps_log.recompact(&deps_path, &mut self.state)?;
        }
        Ok(())
    }

    /// Re-stat the outputs recorded in the build log (`-t restat`).
    pub fn restat_log(&mut self, outputs: &[String]) -> Result<()> {
        if !self.build_log_found {
            return Ok(());
        }
        let log_path = self.build_log_path();
        self.build_log.restat(&log_path, &*self.disk, outputs)?;
        Ok(())
    }

    /// Warnings produced while loading.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Take the accumulated warnings.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// The build graph.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Mutable access to the build graph.
    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    /// The filesystem in use.
    pub fn disk(&self) -> &dyn DiskInterface {
        &*self.disk
    }

    /// Borrow the graph mutably alongside the filesystem and the deps log.
    ///
    /// Tools that add nodes while reading depfiles need all three at once.
    pub fn split_mut(&mut self) -> (&mut State, &dyn DiskInterface, &DepsLog) {
        (&mut self.state, &*self.disk, &self.deps_log)
    }

    /// The build log.
    pub fn build_log(&self) -> &BuildLog {
        &self.build_log
    }

    /// The deps log.
    pub fn deps_log(&self) -> &DepsLog {
        &self.deps_log
    }

    /// The build options.
    pub fn config(&self) -> &BuildConfig {
        &self.options.build
    }

    /// Mutable access to the build options.
    pub fn config_mut(&mut self) -> &mut BuildConfig {
        &mut self.options.build
    }

    /// The exit code the process should use.
    pub fn exit_code(&self) -> ExitStatus {
        self.exit_code
    }

    /// Resolve target names (or the manifest's defaults, when empty).
    pub fn resolve_targets(&self, names: &[String]) -> Result<Vec<NodeId>> {
        tools::collect_targets(&self.state, Some(&self.deps_log), names)
    }

    /// Build `targets` (or the default targets), reporting to a null status.
    pub fn build(&mut self, targets: &[impl AsRef<str>]) -> Result<BuildSummary> {
        let names: Vec<String> = targets.iter().map(|t| t.as_ref().to_string()).collect();
        let mut status = NullStatus;
        self.build_with_status(&names, &mut status)
    }

    /// Build `targets`, reporting progress to `status`.
    pub fn build_with_status(
        &mut self,
        targets: &[String],
        status: &mut dyn Status,
    ) -> Result<BuildSummary> {
        let mut manifest_rebuilt = false;

        for _cycle in 0..MANIFEST_REBUILD_LIMIT {
            if self.options.rebuild_manifest {
                let rebuilt = self.rebuild_manifest(status)?;
                if rebuilt {
                    manifest_rebuilt = true;
                    // In dry-run mode regeneration would never converge.
                    if self.options.build.dry_run {
                        return Ok(BuildSummary {
                            up_to_date: false,
                            manifest_rebuilt: true,
                            ..Default::default()
                        });
                    }
                    self.reload()?;
                    continue;
                }
            }

            self.parse_previous_elapsed_times();
            let mut summary = self.run_build(targets, status)?;
            summary.manifest_rebuilt = manifest_rebuilt;
            return Ok(summary);
        }

        Err(Error::build(format!(
            "manifest '{}' still dirty after {} tries, perhaps system time is not set",
            self.manifest_path, MANIFEST_REBUILD_LIMIT
        )))
    }

    fn run_build(&mut self, targets: &[String], status: &mut dyn Status) -> Result<BuildSummary> {
        let target_nodes = self.resolve_targets(targets)?;

        let config = self.options.build.clone();
        let builder_config = config.clone();
        let start_time_millis = now_millis();
        let interrupt = self.options.interrupt.clone();
        let runner_factory = self.options.command_runner.clone();

        let mut builder = Builder::new(
            &mut self.state,
            config,
            Some(&mut self.build_log),
            Some(&mut self.deps_log),
            &*self.disk,
            status,
            start_time_millis,
        );
        if let Some(flag) = interrupt {
            builder.set_interrupt_flag(flag);
        }
        if let Some(factory) = &runner_factory {
            builder.set_command_runner(factory(&builder_config));
        }

        let scan_start = Instant::now();
        for node in target_nodes {
            // Problems found while scanning (missing inputs, cycles) are
            // reported before any command runs.
            builder.add_target(node).map_err(Error::into_graph)?;
        }
        let scan_elapsed = scan_start.elapsed();

        if builder.already_up_to_date() {
            drop(builder);
            self.timings.scan += scan_elapsed;
            return Ok(BuildSummary {
                up_to_date: true,
                ..Default::default()
            });
        }

        let build_start = Instant::now();
        let result = builder.build();
        let build_elapsed = build_start.elapsed();
        let summary = BuildSummary {
            up_to_date: false,
            edges_started: builder.edges_started(),
            edges_finished: builder.edges_finished(),
            manifest_rebuilt: false,
        };
        let code = builder.exit_code();
        drop(builder);
        self.exit_code = code;
        self.timings.scan += scan_elapsed;
        self.timings.build += build_elapsed;
        result?;
        Ok(summary)
    }

    /// Rebuild the manifest if it is itself a build output.
    ///
    /// Returns true if it changed, in which case the caller should reload.
    fn rebuild_manifest(&mut self, status: &mut dyn Status) -> Result<bool> {
        let mut path = self.manifest_path.clone();
        if path.is_empty() {
            return Err(Error::build("empty path"));
        }
        canonicalize_path(&mut path);
        let Some(node) = self.state.lookup_node(&path) else {
            return Ok(false); // The manifest is not generated.
        };

        let config = self.options.build.clone();
        let builder_config = config.clone();
        let start_time_millis = now_millis();
        let interrupt = self.options.interrupt.clone();
        let runner_factory = self.options.command_runner.clone();

        let outcome = {
            let mut builder = Builder::new(
                &mut self.state,
                config,
                Some(&mut self.build_log),
                Some(&mut self.deps_log),
                &*self.disk,
                status,
                start_time_millis,
            );
            if let Some(flag) = interrupt {
                builder.set_interrupt_flag(flag);
            }
            if let Some(factory) = &runner_factory {
                builder.set_command_runner(factory(&builder_config));
            }
            builder.add_target(node).map_err(Error::into_graph)?;
            if builder.already_up_to_date() {
                None
            } else {
                let r = builder.build();
                let code = builder.exit_code();
                Some((r, code))
            }
        };

        match outcome {
            None => Ok(false),
            Some((Err(e), code)) => {
                self.exit_code = code;
                Err(Error::build(format!(
                    "rebuilding '{}': {e}",
                    self.manifest_path
                )))
            }
            Some((Ok(_), _)) => {
                // It was only really rebuilt if it is still dirty; a `restat`
                // rule may have decided nothing changed.
                if !self.state.node(node).dirty {
                    self.state.reset();
                    return Ok(false);
                }
                Ok(true)
            }
        }
    }

    /// Re-read the manifest and logs from scratch, keeping the options.
    pub fn reload(&mut self) -> Result<()> {
        // ninja tears down and recreates its whole state here, which flushes
        // (and creates) both logs; match that so the files on disk agree.
        if !self.options.build.dry_run {
            self.build_log.ensure_created()?;
            self.deps_log.ensure_created()?;
        }
        self.build_log.close()?;
        self.deps_log.close()?;
        self.state = State::new();
        self.build_log = BuildLog::new();
        self.deps_log = DepsLog::new();
        self.warnings.clear();
        self.parse_manifest()?;
        self.ensure_build_dir_exists()?;
        self.open_logs()
    }

    /// Note how long each edge took last time, for ETA prediction.
    fn parse_previous_elapsed_times(&mut self) {
        for edge in self.state.edge_ids().collect::<Vec<_>>() {
            for output in self.state.edge(edge).outputs().to_vec() {
                let path = self.state.node(output).path().to_string();
                if let Some(entry) = self.build_log.lookup_by_output(&path) {
                    let duration = entry.duration_millis();
                    self.state
                        .edge_mut(edge)
                        .set_prev_elapsed_time_millis(duration);
                    break;
                }
            }
        }
    }

    /// Close both logs. Called automatically on drop.
    pub fn close(&mut self) -> Result<()> {
        self.build_log.close()?;
        self.deps_log.close()?;
        Ok(())
    }

    // ---- tools -------------------------------------------------------------

    /// `-t clean`: remove build outputs.
    pub fn clean_all(&mut self, generator: bool) -> CleanReport {
        let dry_run = self.options.build.dry_run;
        Cleaner::new(&mut self.state, &*self.disk, dry_run).clean_all(generator)
    }

    /// `-t clean TARGETS`: remove the outputs needed for these targets.
    pub fn clean_targets(&mut self, targets: &[String]) -> CleanReport {
        let dry_run = self.options.build.dry_run;
        Cleaner::new(&mut self.state, &*self.disk, dry_run).clean_targets(targets)
    }

    /// `-t clean -r RULES`: remove the outputs of these rules.
    pub fn clean_rules(&mut self, rules: &[String]) -> CleanReport {
        let dry_run = self.options.build.dry_run;
        Cleaner::new(&mut self.state, &*self.disk, dry_run).clean_rules(rules)
    }

    /// `-t cleandead`: remove outputs the manifest no longer produces.
    pub fn clean_dead(&mut self) -> CleanReport {
        let dry_run = self.options.build.dry_run;
        let mut cleaner = Cleaner::new(&mut self.state, &*self.disk, dry_run);
        cleaner.clean_dead(&self.build_log)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.build_log.close();
        let _ = self.deps_log.close();
    }
}

/// True if `path` is no longer produced by the build and no longer exists.
fn is_path_dead(state: &State, disk: &dyn DiskInterface, path: &str) -> bool {
    if let Some(n) = state.lookup_node(path) {
        if state.node(n).in_edge().is_some() {
            return false;
        }
    }
    // Keep entries for files that still exist: generators may want them.
    disk.stat(path).unwrap_or(0) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(name: &str) -> Tmp {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "shuriken-engine-{}-{}-{}",
                name,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.0.join(name), contents).unwrap();
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }
        fn exists(&self, name: &str) -> bool {
            self.0.join(name).exists()
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn end_to_end_build() {
        // Absolute paths throughout, so the test never depends on (or changes)
        // the process-wide working directory.
        let tmp = Tmp::new("e2e");
        let dir = tmp.0.to_str().unwrap().to_string();
        tmp.write("in.txt", "hello\n");
        tmp.write(
            "build.ninja",
            &format!(
                "builddir = {dir}\nrule copy\n  command = cp $in $out\n\n\
                 build {dir}/out.txt: copy {dir}/in.txt\n"
            ),
        );

        let mut engine = Engine::load(
            tmp.path("build.ninja"),
            EngineOptions {
                build: BuildConfig {
                    verbosity: crate::status::Verbosity::Quiet,
                    parallelism: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let summary = engine.build(&[format!("{dir}/out.txt")]).unwrap();

        assert!(!summary.up_to_date);
        assert_eq!(summary.edges_finished, 1);
        assert!(tmp.exists("out.txt"));
        assert!(tmp.exists(".ninja_log"));
        assert_eq!(
            std::fs::read_to_string(tmp.path("out.txt")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn second_build_is_a_no_op() {
        let tmp = Tmp::new("noop");
        let dir = tmp.0.to_str().unwrap().to_string();
        tmp.write("in.txt", "hello\n");
        tmp.write(
            "build.ninja",
            &format!(
                "builddir = {dir}\nrule copy\n  command = cp $in $out\n\n\
                 build {dir}/out.txt: copy {dir}/in.txt\n"
            ),
        );

        let opts = || EngineOptions {
            build: BuildConfig {
                verbosity: crate::status::Verbosity::Quiet,
                parallelism: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = format!("{dir}/out.txt");

        let first = {
            let mut engine = Engine::load(tmp.path("build.ninja"), opts()).unwrap();
            engine.build(std::slice::from_ref(&target)).unwrap()
        };
        let second = {
            let mut engine = Engine::load(tmp.path("build.ninja"), opts()).unwrap();
            engine.build(&[target]).unwrap()
        };

        assert!(!first.up_to_date);
        assert!(second.up_to_date, "second build should have nothing to do");
    }
}
