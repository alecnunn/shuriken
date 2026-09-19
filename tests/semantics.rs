//! Tests for the subtler ninja semantics, using an in-memory filesystem so
//! mtimes and command effects are fully controlled.

mod common;

use std::cell::RefCell;

use shuriken::build::{BuildConfig, Builder, Verbosity};
use shuriken::build_log::BuildLog;
use shuriken::deps_log::DepsLog;
use shuriken::disk::MemDisk;
use shuriken::exec::{CommandResult, CommandRunner, ExitStatus};
use shuriken::parse::{ManifestParser, ParserOptions, PhonyCycleAction};
use shuriken::state::{EdgeId, State};
use shuriken::status::NullStatus;

/// What a fake command should do to its outputs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
    /// Write new content, so the output's mtime changes.
    Touch,
    /// Leave the outputs alone (what a `restat` rule that finds nothing to do
    /// would look like).
    LeaveAlone,
}

struct ScriptedRunner<'d> {
    disk: &'d MemDisk,
    effects: RefCell<std::collections::HashMap<String, Effect>>,
    default_effect: Effect,
    queue: std::collections::VecDeque<EdgeId>,
}

impl<'d> ScriptedRunner<'d> {
    fn new(disk: &'d MemDisk, default_effect: Effect) -> ScriptedRunner<'d> {
        ScriptedRunner {
            disk,
            effects: RefCell::new(Default::default()),
            default_effect,
            queue: Default::default(),
        }
    }

    /// Give one rule a different effect from the default.
    fn rule_effect(self, rule: &str, effect: Effect) -> Self {
        self.effects.borrow_mut().insert(rule.to_string(), effect);
        self
    }
}

impl CommandRunner for ScriptedRunner<'_> {
    fn can_run_more(&self) -> usize {
        4
    }

    fn start_command(&mut self, state: &State, edge: EdgeId) -> shuriken::Result<()> {
        let rule = state.edge_rule_name(edge).to_string();
        let effect = self
            .effects
            .borrow()
            .get(&rule)
            .copied()
            .unwrap_or(self.default_effect);
        if effect == Effect::Touch {
            self.disk.tick();
            for &out in state.edge(edge).outputs() {
                let path = state.node(out).path().to_string();
                self.disk
                    .create(&path, format!("built-{}", self.disk.now()));
            }
        }
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
        self.queue.iter().copied().collect()
    }

    fn abort(&mut self) {
        self.queue.clear();
    }
}

fn parse_with(disk: &MemDisk, manifest: &str, options: ParserOptions) -> shuriken::Result<State> {
    let mut state = State::new();
    {
        let mut parser = ManifestParser::new(&mut state, disk, options);
        parser.parse_text("build.ninja", manifest.as_bytes())?;
    }
    Ok(state)
}

fn parse(disk: &MemDisk, manifest: &str) -> State {
    parse_with(
        disk,
        manifest,
        ParserOptions {
            quiet: true,
            ..Default::default()
        },
    )
    .expect("parse")
}

/// Build `target` and return how many commands ran.
fn build(
    state: &mut State,
    disk: &MemDisk,
    log: Option<&mut BuildLog>,
    deps_log: Option<&mut DepsLog>,
    target: &str,
    default_effect: Effect,
    tweak: impl FnOnce(ScriptedRunner<'_>) -> ScriptedRunner<'_>,
) -> usize {
    let mut status = NullStatus;
    let runner = tweak(ScriptedRunner::new(disk, default_effect));
    {
        let mut builder = Builder::new(
            state,
            BuildConfig {
                verbosity: Verbosity::Quiet,
                ..Default::default()
            },
            log,
            deps_log,
            disk,
            &mut status,
            0,
        );
        builder.set_command_runner(Box::new(runner));
        builder.add_target_by_name(target).expect("add target");
        if builder.already_up_to_date() {
            0
        } else {
            builder.build().expect("build");
            builder.edges_finished() as usize
        }
    }
}

const CAT: &str = "rule cat\n  command = cat $in > $out\n\n";

#[test]
fn restat_that_changes_nothing_stops_the_rebuild() {
    // `stamp` has restat and never modifies its output, so `final` must not be
    // rebuilt when only `in` changed.
    let disk = MemDisk::new();
    disk.create("in", "a");
    let manifest = format!(
        "rule stamp\n  command = stamp\n  restat = 1\n\n{CAT}\
         build stamp.out: stamp in\nbuild final: cat stamp.out\n"
    );
    let mut state = parse(&disk, &manifest);
    let mut log = BuildLog::new();

    // First build: both steps run.
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "final",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 2);

    // Now change the input, but make `stamp` leave its output untouched.
    disk.tick();
    disk.create("in", "b");
    state.reset();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "final",
        Effect::Touch,
        |r| r.rule_effect("stamp", Effect::LeaveAlone),
    );
    assert_eq!(
        ran, 1,
        "only the restat rule should run; `final` stays clean"
    );
}

#[test]
fn restat_that_does_change_its_output_propagates() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    let manifest = format!(
        "rule stamp\n  command = stamp\n  restat = 1\n\n{CAT}\
         build stamp.out: stamp in\nbuild final: cat stamp.out\n"
    );
    let mut state = parse(&disk, &manifest);
    let mut log = BuildLog::new();
    build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "final",
        Effect::Touch,
        |r| r,
    );

    disk.tick();
    disk.create("in", "b");
    state.reset();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "final",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 2, "a changed stamp output must rebuild dependents");
}

#[test]
fn a_second_build_with_a_log_does_nothing() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    let mut state = parse(&disk, &format!("{CAT}build out: cat in\n"));
    let mut log = BuildLog::new();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 1);

    state.reset();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 0, "nothing should run the second time");
}

#[test]
fn a_changed_command_rebuilds_even_when_mtimes_are_fine() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    let mut log = BuildLog::new();
    {
        let mut state = parse(&disk, &format!("{CAT}build out: cat in\n"));
        build(
            &mut state,
            &disk,
            Some(&mut log),
            None,
            "out",
            Effect::Touch,
            |r| r,
        );
    }
    // Same graph, different command line.
    let mut state = parse(
        &disk,
        "rule cat\n  command = cat -n $in > $out\n\nbuild out: cat in\n",
    );
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 1, "a new command hash must force a rebuild");
}

#[test]
fn a_generator_rule_ignores_command_changes() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    let mut log = BuildLog::new();
    {
        let mut state = parse(
            &disk,
            "rule gen\n  command = gen\n  generator = 1\n\nbuild out: gen in\n",
        );
        build(
            &mut state,
            &disk,
            Some(&mut log),
            None,
            "out",
            Effect::Touch,
            |r| r,
        );
    }
    let mut state = parse(
        &disk,
        "rule gen\n  command = gen --different\n  generator = 1\n\nbuild out: gen in\n",
    );
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 0, "generator rules ignore command changes");
}

#[test]
fn stale_deps_log_entry_forces_a_rebuild() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    disk.create("dep.h", "h");
    let manifest = "rule cc\n  command = cc\n  deps = gcc\n  depfile = $out.d\n\n\
                    build out: cc in\n";

    let mut state = parse(&disk, manifest);
    let mut log = BuildLog::new();
    let mut deps_log = DepsLog::new();

    // Record deps whose mtime is older than the output: that is what a build
    // interrupted between writing the output and recording deps looks like.
    let out = state.lookup_node("out").unwrap();
    let dep = state.get_node("dep.h", 0);
    disk.create("out", "built");
    deps_log
        .record_deps(&mut state, out, 1, &[dep])
        .expect("record deps");

    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        Some(&mut deps_log),
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 1, "stale deps info must force a rebuild");
}

#[test]
fn phony_output_used_as_an_input_is_transparent() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    let manifest = format!(
        "{CAT}rule touch\n  command = touch\n\n\
         build real: touch in\nbuild alias: phony real\nbuild out: cat in | alias\n"
    );
    let mut state = parse(&disk, &manifest);
    let mut log = BuildLog::new();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    // `real` and `out` run; `alias` is phony and runs no command.
    assert_eq!(ran, 2);

    state.reset();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 0);
}

#[test]
fn phony_cycle_can_be_an_error() {
    let disk = MemDisk::new();
    let err = parse_with(
        &disk,
        "build a: phony a\n",
        ParserOptions {
            phony_cycle_action: PhonyCycleAction::Error,
            quiet: true,
        },
    );
    // With phonycycle=err the self-reference is kept, and the cycle shows up
    // when the graph is scanned.
    let mut state = err.expect("parsing still succeeds");
    assert_eq!(state.edge(EdgeId(0)).inputs().len(), 1);

    let node = state.lookup_node("a").unwrap();
    let mut scan = shuriken::DependencyScan::new(&mut state, &disk, None, None, None);
    let mut validations = Vec::new();
    let e = scan.recompute_dirty(node, &mut validations).unwrap_err();
    assert!(e.to_string().contains("dependency cycle"), "{e}");
    assert!(e.to_string().contains("[-w phonycycle=err]"), "{e}");
}

#[test]
fn dyndep_output_claimed_twice_is_an_error() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    disk.create(
        "dd",
        "ninja_dyndep_version = 1\nbuild out | taken: dyndep\n",
    );
    let manifest = format!(
        "{CAT}rule copy\n  command = copy\n  dyndep = dd\n\n\
         build taken: cat in\nbuild out: copy in | dd\n"
    );
    let mut state = parse(&disk, &manifest);
    let dd = state.lookup_node("dd").unwrap();
    let e = shuriken::dyndep::load_dyndeps(&mut state, &disk, dd).unwrap_err();
    assert!(
        e.to_string().contains("multiple rules generate taken"),
        "{e}"
    );
}

#[test]
fn depfile_may_name_every_output_of_its_edge() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    disk.create("dep.d", "a.out b.out: extra.h\n");
    disk.create("extra.h", "h");
    disk.tick();
    disk.create("a.out", "x");
    disk.create("b.out", "x");

    let manifest = "rule multi\n  command = multi\n  depfile = dep.d\n\n\
                    build a.out b.out: multi in\n";
    let mut state = parse(&disk, manifest);
    let node = state.lookup_node("a.out").unwrap();
    {
        let mut scan = shuriken::DependencyScan::new(&mut state, &disk, None, None, None);
        let mut validations = Vec::new();
        scan.recompute_dirty(node, &mut validations).expect("scan");
    }
    // The depfile's dependency was adopted as an implicit input.
    assert_eq!(state.edge(EdgeId(0)).implicit_deps(), 1);
    assert!(state.lookup_node("extra.h").is_some());
}

#[test]
fn depfile_naming_an_undeclared_output_is_an_error() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    disk.create("dep.d", "a.out surprise: extra.h\n");
    disk.tick();
    disk.create("a.out", "x");

    let manifest = "rule one\n  command = one\n  depfile = dep.d\n\nbuild a.out: one in\n";
    let mut state = parse(&disk, manifest);
    let node = state.lookup_node("a.out").unwrap();
    let mut scan = shuriken::DependencyScan::new(&mut state, &disk, None, None, None);
    let mut validations = Vec::new();
    let e = scan.recompute_dirty(node, &mut validations).unwrap_err();
    assert!(
        e.to_string()
            .contains("depfile mentions 'surprise' as an output"),
        "{e}"
    );
}

#[test]
fn order_only_dependency_does_not_trigger_a_rebuild() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    disk.create("oo.in", "o");
    let manifest = format!("{CAT}build oo: cat oo.in\nbuild out: cat in || oo\n");
    let mut state = parse(&disk, &manifest);
    let mut log = BuildLog::new();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 2);

    // Rebuilding the order-only input alone must not rebuild `out`.
    disk.tick();
    disk.create("oo.in", "changed");
    state.reset();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 1, "only the order-only input should rebuild");
}

#[test]
fn implicit_dependency_does_trigger_a_rebuild() {
    let disk = MemDisk::new();
    disk.create("in", "a");
    disk.create("imp", "i");
    let mut state = parse(&disk, &format!("{CAT}build out: cat in | imp\n"));
    let mut log = BuildLog::new();
    build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );

    disk.tick();
    disk.create("imp", "changed");
    state.reset();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "out",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 1);
}

#[test]
fn deleting_an_output_rebuilds_just_that_edge() {
    let disk = MemDisk::new();
    disk.create("a", "a");
    let mut state = parse(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
    let mut log = BuildLog::new();
    build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "c",
        Effect::Touch,
        |r| r,
    );

    disk.remove_file_for_test("c");
    state.reset();
    let ran = build(
        &mut state,
        &disk,
        Some(&mut log),
        None,
        "c",
        Effect::Touch,
        |r| r,
    );
    assert_eq!(ran, 1);
}

/// Small helper: `MemDisk` has no public delete-for-test method, so go through
/// the `DiskInterface` implementation.
trait RemoveForTest {
    fn remove_file_for_test(&self, path: &str);
}

impl RemoveForTest for MemDisk {
    fn remove_file_for_test(&self, path: &str) {
        use shuriken::disk::DiskInterface;
        self.remove_file(path).expect("remove");
    }
}
