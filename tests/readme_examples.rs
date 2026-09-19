//! The examples in README.md, compiled so they cannot drift out of date.

use shuriken::exec::ExitStatus;
use shuriken::{CommandResult, CommandRunner, EdgeId, Engine, EngineOptions, State, tools};

/// README: "The quick path is `Engine`".
#[allow(dead_code)]
fn quick_path() -> shuriken::Result<()> {
    let mut engine = Engine::load("build.ninja", EngineOptions::default())?;
    let summary = engine.build(&["all"])?;
    println!("{} edges ran", summary.edges_finished);
    Ok(())
}

/// README: "Inspect the graph."
#[allow(dead_code)]
fn inspect_the_graph() -> shuriken::Result<()> {
    let engine = Engine::load("build.ninja", EngineOptions::default())?;
    let state = engine.state();
    for edge in state.edge_ids() {
        println!("{} <- {}", state.edge_rule_name(edge), state.edge_command(edge));
    }
    print!("{}", tools::targets_all(state));
    Ok(())
}

/// README: "Run commands somewhere else."
struct Recorder(Vec<String>, Vec<EdgeId>);

impl CommandRunner for Recorder {
    fn can_run_more(&self) -> usize {
        1
    }
    fn start_command(&mut self, state: &State, edge: EdgeId) -> shuriken::Result<()> {
        self.0.push(state.edge_command(edge));
        self.1.push(edge);
        Ok(())
    }
    fn wait_for_command(&mut self) -> Option<CommandResult> {
        let edge = self.1.pop()?;
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

#[test]
fn readme_examples_compile_and_the_runner_works() {
    // Exercise the README's runner so it is more than a compile check.
    let disk = shuriken::MemDisk::new();
    disk.create("a", "a\n");
    let mut state = State::new();
    {
        let mut parser = shuriken::ManifestParser::new(
            &mut state,
            &disk,
            shuriken::ParserOptions {
                quiet: true,
                ..Default::default()
            },
        );
        parser
            .parse_text(
                "build.ninja",
                b"rule cat\n  command = cat $in > $out\n\nbuild b: cat a\n",
            )
            .expect("parse");
    }

    let mut status = shuriken::status::NullStatus;
    let mut builder = shuriken::Builder::new(
        &mut state,
        shuriken::BuildConfig {
            verbosity: shuriken::Verbosity::Quiet,
            ..Default::default()
        },
        None,
        None,
        &disk,
        &mut status,
        0,
    );
    builder.set_command_runner(Box::new(Recorder(Vec::new(), Vec::new())));
    builder.add_target_by_name("b").expect("add target");
    builder.build().expect("build");
    assert_eq!(builder.edges_finished(), 1);
}
