//! Tests of the library API as an embedder sees it.

mod common;

use std::sync::{Arc, Mutex};

use common::TempDir;
use shuriken::build::{BuildConfig, Builder, Verbosity};
use shuriken::build_log::{BuildLog, LogEntry};
use shuriken::deps_log::DepsLog;
use shuriken::disk::{DiskInterface, MemDisk};
use shuriken::engine::{Engine, EngineOptions};
use shuriken::exec::{CommandResult, CommandRunner, ExitStatus};
use shuriken::parse::{ManifestParser, ParserOptions};
use shuriken::state::{EdgeId, State};
use shuriken::status::{Status, Verbosity as _Verbosity};
use shuriken::tools;

const CAT: &str = "rule cat\n  command = cat $in > $out\n  description = CAT $out\n\n";

fn parse(disk: &MemDisk, manifest: &str) -> State {
    let mut state = State::new();
    let mut parser = ManifestParser::new(
        &mut state,
        disk,
        ParserOptions {
            quiet: true,
            ..Default::default()
        },
    );
    parser
        .parse_text("build.ninja", manifest.as_bytes())
        .expect("parse manifest");
    drop(parser);
    state
}

/// A command runner that "builds" into a [`MemDisk`]: no processes involved.
struct InMemoryRunner<'d> {
    disk: &'d MemDisk,
    queue: std::collections::VecDeque<EdgeId>,
    log: Arc<Mutex<Vec<String>>>,
    fail: Option<String>,
}

impl<'d> InMemoryRunner<'d> {
    fn new(disk: &'d MemDisk, log: Arc<Mutex<Vec<String>>>) -> InMemoryRunner<'d> {
        InMemoryRunner {
            disk,
            queue: Default::default(),
            log,
            fail: None,
        }
    }
}

impl CommandRunner for InMemoryRunner<'_> {
    fn can_run_more(&self) -> usize {
        8
    }

    fn start_command(&mut self, state: &State, edge: EdgeId) -> shuriken::Result<()> {
        self.log.lock().unwrap().push(state.edge_command(edge));
        if self.fail.is_none() {
            self.disk.tick();
            // Pretend the command concatenated its inputs.
            let mut contents = String::new();
            for &input in state.edge(edge).inputs() {
                let path = state.node(input).path();
                if let Some(bytes) = self.disk.contents(path) {
                    contents.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
            for &out in state.edge(edge).outputs() {
                self.disk
                    .create(state.node(out).path(), contents.as_bytes());
            }
        }
        self.queue.push_back(edge);
        Ok(())
    }

    fn wait_for_command(&mut self) -> Option<CommandResult> {
        let edge = self.queue.pop_front()?;
        Some(CommandResult {
            edge,
            status: match &self.fail {
                Some(_) => ExitStatus(2),
                None => ExitStatus::SUCCESS,
            },
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

#[derive(Default)]
struct CountingStatus {
    added: usize,
    started: Vec<String>,
    finished: Vec<(String, i32)>,
    infos: Vec<String>,
}

impl Status for CountingStatus {
    fn edge_added_to_plan(&mut self, _state: &State, _edge: EdgeId) {
        self.added += 1;
    }
    fn build_edge_started(&mut self, state: &State, edge: EdgeId, _start: i64) {
        self.started.push(state.edge_binding(edge, "description"));
    }
    fn build_edge_finished(
        &mut self,
        state: &State,
        edge: EdgeId,
        _start: i64,
        _end: i64,
        status: ExitStatus,
        _output: &str,
    ) {
        self.finished
            .push((state.edge_binding(edge, "description"), status.code()));
    }
    fn info(&mut self, message: &str) {
        self.infos.push(message.to_string());
    }
}

#[test]
fn a_build_can_run_entirely_in_memory() {
    let disk = MemDisk::new();
    disk.create("a", "a\n");
    let mut state = parse(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut status = CountingStatus::default();

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
        builder.set_command_runner(Box::new(InMemoryRunner::new(&disk, Arc::clone(&commands))));
        builder.add_target_by_name("c").expect("add target");
        assert!(!builder.already_up_to_date());
        builder.build().expect("build");
    }

    assert_eq!(
        *commands.lock().unwrap(),
        vec!["cat a > b".to_string(), "cat b > c".to_string()]
    );
    assert_eq!(disk.contents("c").unwrap(), b"a\n");
    assert_eq!(status.added, 2);
    assert_eq!(status.started.len(), 2);
    assert_eq!(status.finished[0].1, 0);
}

#[test]
fn a_failed_command_stops_the_build_and_reports_the_code() {
    let disk = MemDisk::new();
    disk.create("a", "a\n");
    let mut state = parse(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
    let mut status = CountingStatus::default();

    let (result, code) = {
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
        let mut runner = InMemoryRunner::new(&disk, Arc::new(Mutex::new(Vec::new())));
        runner.fail = Some("it broke".to_string());
        builder.set_command_runner(Box::new(runner));
        builder.add_target_by_name("c").expect("add target");
        let r = builder.build();
        (r.is_err(), builder.exit_code())
    };
    assert!(result);
    assert_eq!(code, ExitStatus(2));
    assert_eq!(status.finished.len(), 1, "the second edge never ran");
}

#[test]
fn state_exposes_the_graph() {
    let disk = MemDisk::new();
    let state = parse(
        &disk,
        "cflags = -O2\n\
         rule cc\n  command = gcc $cflags -c $in -o $out\n\n\
         build a.o: cc a.c | header.h || dir\nbuild all: phony a.o\ndefault all\n",
    );

    assert_eq!(state.edges().len(), 2);
    let edge = EdgeId(0);
    assert_eq!(state.edge_rule_name(edge), "cc");
    assert_eq!(state.edge_command(edge), "gcc -O2 -c a.c -o a.o");
    assert_eq!(state.edge(edge).explicit_deps(), 1);
    assert_eq!(state.edge(edge).implicit_deps(), 1);
    assert_eq!(state.edge(edge).order_only_deps(), 1);

    let node = state.lookup_node("a.o").expect("node exists");
    assert_eq!(state.node(node).in_edge(), Some(edge));
    assert_eq!(state.node(node).out_edges().len(), 1);

    let defaults = state.default_nodes().expect("defaults");
    assert_eq!(defaults.len(), 1);
    assert_eq!(state.node(defaults[0]).path(), "all");
}

#[test]
fn tools_are_callable_from_the_library() {
    let disk = MemDisk::new();
    let mut state = parse(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
    let c = state.lookup_node("c").unwrap();

    assert_eq!(tools::targets_all(&state), "b: cat\nc: cat\n");
    assert_eq!(
        tools::commands(&state, &[c], false),
        "cat a > b\ncat b > c\n"
    );
    assert_eq!(
        tools::inputs(&state, &[c], tools::InputsOptions::default()),
        "a\nb\n"
    );
    let compdb = tools::compdb(&state, &[], false, "/work");
    assert!(compdb.contains("\"output\": \"b\""));
    let dot = tools::graph(&mut state, &disk, &[c]);
    assert!(dot.starts_with("digraph ninja {"));
}

#[test]
fn engine_loads_manifest_text_without_touching_disk() {
    let disk = MemDisk::new();
    disk.create("a", "a\n");
    let engine = Engine::from_manifest_text(
        "build.ninja",
        &format!("{CAT}build b: cat a\n"),
        EngineOptions {
            disk: Some(Box::new(MemDisk::new())),
            ..Default::default()
        },
    )
    .expect("load");
    assert_eq!(engine.state().edges().len(), 1);
    assert!(engine.warnings().is_empty());
}

#[test]
fn engine_can_use_a_custom_command_runner() {
    // A runner that records commands instead of running them, wired in through
    // EngineOptions. The build still updates the logs and the graph.
    let dir = TempDir::new("api-custom-runner");
    dir.write("a", "a\n");
    // Absolute paths, because the test process runs in the crate root rather
    // than in this directory.
    dir.write(
        "build.ninja",
        &format!(
            "builddir = {}\n{CAT}build {}: cat {}\n",
            dir.path().display(),
            dir.join("b"),
            dir.join("a")
        ),
    );

    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_for_factory = Arc::clone(&recorded);

    struct Recorder {
        recorded: Arc<Mutex<Vec<String>>>,
        queue: std::collections::VecDeque<EdgeId>,
    }
    impl CommandRunner for Recorder {
        fn can_run_more(&self) -> usize {
            1
        }
        fn start_command(&mut self, state: &State, edge: EdgeId) -> shuriken::Result<()> {
            self.recorded.lock().unwrap().push(state.edge_command(edge));
            self.queue.push_back(edge);
            Ok(())
        }
        fn wait_for_command(&mut self) -> Option<CommandResult> {
            let edge = self.queue.pop_front()?;
            Some(CommandResult {
                edge,
                status: ExitStatus::SUCCESS,
                output: String::new(),
            })
        }
        fn active_edges(&self) -> Vec<EdgeId> {
            Vec::new()
        }
        fn abort(&mut self) {}
    }

    let mut engine = Engine::load(
        dir.path().join("build.ninja").to_str().unwrap(),
        EngineOptions {
            build: BuildConfig {
                verbosity: Verbosity::Quiet,
                ..Default::default()
            },
            command_runner: Some(Arc::new(move |_config| {
                Box::new(Recorder {
                    recorded: Arc::clone(&recorded_for_factory),
                    queue: Default::default(),
                })
            })),
            ..Default::default()
        },
    )
    .expect("load engine");

    let summary = engine.build(&[dir.join("b")]).expect("build");
    assert!(!summary.up_to_date);
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![format!("cat {} > {}", dir.join("a"), dir.join("b"))]
    );
}

#[test]
fn build_log_round_trips_and_matches_ninja_hashes() {
    let dir = TempDir::new("api-build-log");
    let path = dir.path().join(".ninja_log").to_str().unwrap().to_string();

    let mut log = BuildLog::new();
    log.insert_entry(LogEntry {
        output: "out".into(),
        command_hash: shuriken::hash::hash_command("cat in > out"),
        start_time: 5,
        end_time: 17,
        mtime: 1_234_567_890,
    });
    log.recompact(&path, &|_| false).expect("write log");
    log.close().expect("close");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# ninja log v7\n"), "{text}");

    let mut reloaded = BuildLog::new();
    reloaded.load(&path).expect("load");
    let entry = reloaded.lookup_by_output("out").expect("entry");
    assert_eq!(
        entry.command_hash,
        shuriken::hash::hash_command("cat in > out")
    );
    assert_eq!(entry.start_time, 5);
    assert_eq!(entry.end_time, 17);
    assert_eq!(entry.mtime, 1_234_567_890);
    assert_eq!(entry.duration_millis(), 12);
}

#[test]
fn deps_log_round_trips() {
    let dir = TempDir::new("api-deps-log");
    let path = dir.path().join(".ninja_deps").to_str().unwrap().to_string();

    let mut state = State::new();
    let out = state.get_node("out.o", 0);
    let h1 = state.get_node("one.h", 0);
    let h2 = state.get_node("two.h", 0);

    let mut log = DepsLog::new();
    log.open_for_write(&path, &mut state).expect("open");
    log.record_deps(&mut state, out, 4242, &[h1, h2])
        .expect("record");
    log.close().expect("close");

    let raw = std::fs::read(&path).unwrap();
    assert!(raw.starts_with(b"# ninjadeps\n"), "wrong signature");
    assert_eq!(raw[12..16], [4, 0, 0, 0], "version 4");

    let mut state2 = State::new();
    let mut log2 = DepsLog::new();
    log2.load(&path, &mut state2).expect("load");
    let out2 = state2.lookup_node("out.o").expect("node");
    let deps = log2.get_deps(&state2, out2).expect("deps");
    assert_eq!(deps.mtime, 4242);
    let paths: Vec<&str> = deps.nodes.iter().map(|&n| state2.node(n).path()).collect();
    assert_eq!(paths, vec!["one.h", "two.h"]);
}

#[test]
fn mem_disk_satisfies_the_disk_interface() {
    let disk = MemDisk::new();
    assert_eq!(disk.stat("nothing").unwrap(), 0);
    disk.write_file("some/file", "contents", false).unwrap();
    assert!(disk.stat("some/file").unwrap() > 0);
    assert_eq!(
        disk.read_file_text("some/file").unwrap().as_deref(),
        Some("contents")
    );
    assert!(disk.remove_file("some/file").unwrap());
    assert!(!disk.remove_file("some/file").unwrap());
}

#[test]
fn verbosity_is_re_exported_consistently() {
    // `shuriken::build::Verbosity` and `shuriken::status::Verbosity` are the
    // same type, so either import works in embedder code.
    let a: Verbosity = Verbosity::Normal;
    let b: _Verbosity = _Verbosity::Normal;
    assert_eq!(a, b);
}
