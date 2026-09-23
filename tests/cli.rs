//! End-to-end tests of the `shuriken` command-line tool.

mod common;

use common::{
    TempDir, cat_into_out_cmd, commands_run, copy_cmd, copy_cmd_variant, echo_cmd, exit_cmd,
    fail_loudly_cmd, no_op_cmd, shuriken, touch_cmd,
};

fn copy_rule() -> String {
    format!(
        "rule copy\n  command = {}\n  description = COPY $out\n\n",
        copy_cmd()
    )
}

fn simple_project(label: &str) -> TempDir {
    let dir = TempDir::new(label);
    dir.write("in.txt", "hello\n");
    dir.write(
        "build.ninja",
        &format!(
            "{}build mid.txt: copy in.txt\nbuild out.txt: copy mid.txt\ndefault out.txt\n",
            copy_rule()
        ),
    );
    dir
}

#[test]
fn builds_then_does_nothing() {
    let dir = simple_project("cli-basic");

    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert_eq!(commands_run(&run), 2, "{}", run.all());
    assert_eq!(dir.read("out.txt"), "hello\n");
    assert!(dir.exists(".ninja_log"));

    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0);
    assert_eq!(commands_run(&run), 0);
    assert!(run.stdout.contains("no work to do"), "{}", run.all());
}

#[test]
fn rebuilds_only_what_changed() {
    let dir = simple_project("cli-incremental");
    shuriken(&dir, &["-j1"]);

    dir.touch("in.txt");
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 2, "{}", run.all());

    // Deleting an intermediate file brings back just that step and its
    // dependents.
    dir.remove("mid.txt");
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 2, "{}", run.all());
}

#[test]
fn changing_a_command_forces_a_rebuild() {
    let dir = simple_project("cli-command-change");
    shuriken(&dir, &["-j1"]);

    dir.write(
        "build.ninja",
        &format!(
            "rule copy\n  command = {}\n  description = COPY $out\n\n\
             build mid.txt: copy in.txt\nbuild out.txt: copy mid.txt\ndefault out.txt\n",
            copy_cmd_variant()
        ),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 2, "{}", run.all());
}

#[test]
fn failing_command_reports_and_exits_nonzero() {
    let dir = TempDir::new("cli-failure");
    dir.write(
        "build.ninja",
        &format!(
            "rule fail\n  command = {}\n\nbuild out: fail\n",
            fail_loudly_cmd("problem", 7)
        ),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 7, "{}", run.all());
    assert!(run.all().contains("FAILED: [code=7]"), "{}", run.all());
    assert!(run.all().contains("problem"), "{}", run.all());
    assert!(
        run.stdout.contains("build stopped: subcommand failed."),
        "{}",
        run.all()
    );
}

#[test]
fn keep_going_runs_independent_work() {
    let dir = TempDir::new("cli-keep-going");
    dir.write(
        "build.ninja",
        &format!(
            "rule fail\n  command = {}\n\nrule touch\n  command = {}\n\n\
             build f1: fail\nbuild f2: fail\nbuild ok: touch\n\
             build all: phony f1 f2 ok\ndefault all\n",
            exit_cmd(1),
            touch_cmd()
        ),
    );
    let run = shuriken(&dir, &["-j1", "-k", "0"]);
    assert_ne!(run.code, 0);
    assert!(dir.exists("ok"), "independent work should still run");
    assert!(
        run.stdout
            .contains("build stopped: cannot make progress due to previous errors."),
        "{}",
        run.all()
    );
}

#[test]
fn missing_input_is_reported_as_an_error() {
    let dir = TempDir::new("cli-missing-input");
    dir.write(
        "build.ninja",
        &format!("{}build out: copy nope\n", copy_rule()),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("'nope', needed by 'out', missing and no known rule to make it"),
        "{}",
        run.all()
    );
}

#[test]
fn dependency_cycle_is_reported() {
    let dir = TempDir::new("cli-cycle");
    dir.write(
        "build.ninja",
        &format!("{}build a: copy b\nbuild b: copy a\n", copy_rule()),
    );
    let run = shuriken(&dir, &["-j1", "a"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains("dependency cycle: a -> b -> a"),
        "{}",
        run.all()
    );
}

#[test]
fn manifest_errors_point_at_the_line() {
    let dir = TempDir::new("cli-manifest-error");
    dir.write(
        "build.ninja",
        "rule r\n  command = x\n\nbuild out: nosuchrule\n",
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("build.ninja:4: unknown build rule 'nosuchrule'"),
        "{}",
        run.all()
    );
    assert!(run.stderr.contains("^ near here"), "{}", run.all());
}

#[test]
fn dry_run_changes_nothing() {
    let dir = simple_project("cli-dry-run");
    let run = shuriken(&dir, &["-n"]);
    assert_eq!(run.code, 0);
    assert_eq!(commands_run(&run), 2);
    assert!(!dir.exists("out.txt"));
    assert!(!dir.exists(".ninja_log"));
}

#[test]
fn verbose_prints_commands() {
    let dir = simple_project("cli-verbose");
    let run = shuriken(&dir, &["-j1", "-v"]);
    let expected = copy_cmd()
        .replace("$in", "in.txt")
        .replace("$out", "mid.txt");
    assert!(run.stdout.contains(&expected), "{}", run.all());
    let dir = simple_project("cli-quiet");
    let run = shuriken(&dir, &["-j1", "--quiet"]);
    assert!(!run.stdout.contains("COPY"), "{}", run.all());
}

#[test]
fn explain_says_why() {
    let dir = simple_project("cli-explain");
    let run = shuriken(&dir, &["-j1", "-d", "explain"]);
    assert!(
        run.stderr.contains("explain: output mid.txt doesn't exist"),
        "{}",
        run.all()
    );
}

#[test]
fn stats_reports_phases() {
    let dir = simple_project("cli-stats");
    let run = shuriken(&dir, &["-j1", "-d", "stats"]);
    assert!(run.stderr.contains("manifest parse"), "{}", run.all());
    assert!(run.stderr.contains("dependency scan"), "{}", run.all());
    assert!(run.stderr.contains("edges finished"), "{}", run.all());
}

#[test]
fn ninja_status_format_is_honoured() {
    let dir = simple_project("cli-status-format");
    let exe = env!("CARGO_BIN_EXE_shuriken");
    let out = std::process::Command::new(exe)
        .args(["-j1"])
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .env("NINJA_STATUS", "<%f of %t> ")
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // Without an interactive terminal the status is printed when each edge
    // finishes, so %f already counts the edge just completed.
    assert!(stdout.contains("<1 of 2> "), "{stdout}");
    assert!(stdout.contains("<2 of 2> "), "{stdout}");
}

#[test]
fn version_and_help() {
    let dir = TempDir::new("cli-version");
    dir.write("build.ninja", "");
    let run = shuriken(&dir, &["--version"]);
    assert_eq!(run.code, 0);
    assert!(
        run.stdout.starts_with(shuriken::NINJA_COMPAT_VERSION),
        "{}",
        run.all()
    );

    let run = shuriken(&dir, &["-h"]);
    assert_eq!(run.code, 1, "ninja exits 1 after printing usage");
    assert!(run.stderr.contains("usage: shuriken"), "{}", run.all());
}

#[test]
fn chdir_and_alternate_manifest() {
    let dir = TempDir::new("cli-chdir");
    dir.write("sub/in.txt", "x\n");
    dir.write(
        "sub/custom.ninja",
        &format!(
            "{}build out.txt: copy in.txt\ndefault out.txt\n",
            copy_rule()
        ),
    );
    let run = shuriken(&dir, &["-C", "sub", "-f", "custom.ninja", "-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("Entering directory `sub'"),
        "{}",
        run.all()
    );
    assert!(dir.exists("sub/out.txt"));
}

#[test]
fn tools_report_the_graph() {
    let dir = TempDir::new("cli-tools");
    dir.write("a.c", "");
    dir.write("b.c", "");
    dir.write(
        "build.ninja",
        &format!(
            "rule cc\n  command = {t}\n  description = CC $out\n\n\
             rule link\n  command = {t}\n\n\
             build a.o: cc a.c\nbuild b.o: cc b.c\nbuild prog: link a.o b.o\ndefault prog\n",
            t = touch_cmd()
        ),
    );

    let run = shuriken(&dir, &["-t", "targets", "all"]);
    assert_eq!(run.stdout, "a.o: cc\nb.o: cc\nprog: link\n");

    let run = shuriken(&dir, &["-t", "targets", "rule", "cc"]);
    assert_eq!(run.stdout, "a.o\nb.o\n");

    let run = shuriken(&dir, &["-t", "rules"]);
    assert_eq!(run.stdout, "cc\nlink\nphony\n");

    let run = shuriken(&dir, &["-t", "commands", "prog"]);
    assert_eq!(run.stdout.lines().count(), 3);

    let run = shuriken(&dir, &["-t", "inputs", "prog"]);
    assert_eq!(run.stdout, "a.c\na.o\nb.c\nb.o\n");

    let run = shuriken(&dir, &["-t", "query", "prog"]);
    assert!(
        run.stdout
            .contains("prog:\n  input: link\n    a.o\n    b.o"),
        "{}",
        run.all()
    );

    let run = shuriken(&dir, &["-t", "compdb", "cc"]);
    assert!(run.stdout.contains("\"file\": \"a.c\""), "{}", run.all());
    assert!(!run.stdout.contains("prog"), "{}", run.all());

    let run = shuriken(&dir, &["-t", "graph", "prog"]);
    assert!(run.stdout.starts_with("digraph ninja {"), "{}", run.all());

    let run = shuriken(&dir, &["-t", "list"]);
    assert!(run.stdout.contains("clean"), "{}", run.all());
    assert!(run.stdout.contains("compdb"), "{}", run.all());
}

#[test]
fn clean_removes_outputs() {
    let dir = simple_project("cli-clean");
    shuriken(&dir, &["-j1"]);
    assert!(dir.exists("out.txt"));

    let run = shuriken(&dir, &["-t", "clean"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("2 files."), "{}", run.all());
    assert!(!dir.exists("out.txt"));
    assert!(!dir.exists("mid.txt"));
    assert!(dir.exists("in.txt"), "sources must survive");
}

#[test]
fn unknown_target_suggests_a_correction() {
    let dir = simple_project("cli-typo");
    let run = shuriken(&dir, &["out.tx"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("unknown target 'out.tx', did you mean 'out.txt'?"),
        "{}",
        run.all()
    );
}

#[test]
fn unknown_tool_suggests_a_correction() {
    let dir = simple_project("cli-tool-typo");
    let run = shuriken(&dir, &["-t", "clen"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("unknown tool 'clen', did you mean 'clean'?"),
        "{}",
        run.all()
    );
}

// Needs a POSIX shell to write depfiles, sleep, or run a generator
// script. The engine behaviour itself is covered by the unit tests and
// by the ninja differential suite in dev/.
#[cfg(unix)]
#[test]
fn depfile_dependencies_are_tracked_across_runs() {
    let dir = TempDir::new("cli-depfile");
    dir.write("in.c", "x\n");
    dir.write("header.h", "h\n");
    dir.write(
        "build.ninja",
        "rule cc\n  command = cp $in $out && printf '%s: %s header.h\\n' $out $in > $out.d\n  \
         depfile = $out.d\n  deps = gcc\n\nbuild out.o: cc in.c\n",
    );

    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 1, "{}", run.all());
    assert!(dir.exists(".ninja_deps"));
    // ninja deletes the depfile once it has been folded into the deps log.
    assert!(!dir.exists("out.o.d"));

    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 0, "{}", run.all());

    // Touching the discovered header rebuilds.
    dir.touch("header.h");
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 1, "{}", run.all());

    let run = shuriken(&dir, &["-t", "deps"]);
    assert!(run.stdout.contains("out.o: #deps 2"), "{}", run.all());
    assert!(run.stdout.contains("header.h"), "{}", run.all());
}

#[test]
fn restat_stops_the_build_early() {
    let dir = TempDir::new("cli-restat");
    dir.write("in", "x\n");
    dir.write(
        "build.ninja",
        &format!(
            "rule stamp\n  command = {}\n  restat = 1\n\n\
             rule copy\n  command = {}\n\n\
             build stamp.out: stamp in\nbuild final: copy in | stamp.out\ndefault final\n",
            no_op_cmd(),
            copy_cmd()
        ),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());

    // The stamp rule leaves its output alone, so after the first build nothing
    // downstream should run again even though the stamp "ran".
    dir.touch("in");
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
}

// Needs a POSIX shell to write depfiles, sleep, or run a generator
// script. The engine behaviour itself is covered by the unit tests and
// by the ninja differential suite in dev/.
#[cfg(unix)]
#[test]
fn pools_limit_concurrency() {
    let dir = TempDir::new("cli-pool");
    // Each command fails if another is running at the same time.
    dir.write(
        "build.ninja",
        "pool only_one\n  depth = 1\n\n\
         rule exclusive\n  command = test ! -e .busy && touch .busy && sleep 0.2 && \
         rm .busy && touch $out\n  pool = only_one\n\n\
         build a: exclusive\nbuild b: exclusive\nbuild c: exclusive\n\
         build all: phony a b c\ndefault all\n",
    );
    let run = shuriken(&dir, &["-j8"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(dir.exists("a") && dir.exists("b") && dir.exists("c"));
}

// Needs a POSIX shell to write depfiles, sleep, or run a generator
// script. The engine behaviour itself is covered by the unit tests and
// by the ninja differential suite in dev/.
#[cfg(unix)]
#[test]
fn unlimited_parallelism_actually_runs_in_parallel() {
    // 24 sleeps that would take about 2.4s serially.
    let dir = TempDir::new("cli-jobs-zero");
    let mut manifest = String::from("rule slow\n  command = sleep 0.1 && touch $out\n\n");
    for i in 0..24 {
        manifest.push_str(&format!("build o{i}: slow\n"));
    }
    manifest.push_str("build all: phony ");
    for i in 0..24 {
        manifest.push_str(&format!("o{i} "));
    }
    manifest.push_str("\ndefault all\n");
    dir.write("build.ninja", &manifest);

    let start = std::time::Instant::now();
    let run = shuriken(&dir, &["-j0"]);
    let elapsed = start.elapsed();
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        elapsed < std::time::Duration::from_millis(1200),
        "-j0 should run everything at once, took {elapsed:?}"
    );
}

#[test]
fn a_very_deep_chain_does_not_overflow_the_stack() {
    // Scanning the graph recurses per edge; 20k deep would overflow a default
    // 8MiB stack, so the tool runs the build on a larger one.
    let dir = TempDir::new("cli-deep-chain");
    let depth = 20_000;
    let mut manifest = format!(
        "rule tch\n  command = {}\n\nbuild s0: tch in\n",
        touch_cmd()
    );
    for i in 1..depth {
        manifest.push_str(&format!("build s{i}: tch s{}\n", i - 1));
    }
    manifest.push_str(&format!("default s{}\n", depth - 1));
    dir.write("build.ninja", &manifest);
    dir.write("in", "x\n");

    let run = shuriken(&dir, &["-n"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(commands_run(&run), depth);
}

#[test]
fn console_pool_passes_output_through() {
    let dir = TempDir::new("cli-console");
    dir.write(
        "build.ninja",
        &format!(
            "rule say\n  command = {}\n  pool = console\n\nbuild out: say\n",
            echo_cmd("from-console")
        ),
    );
    let run = shuriken(&dir, &["-j4"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.all().contains("from-console"), "{}", run.all());
}

// Needs a POSIX shell to write depfiles, sleep, or run a generator
// script. The engine behaviour itself is covered by the unit tests and
// by the ninja differential suite in dev/.
#[cfg(unix)]
#[test]
fn manifest_regenerates_itself() {
    let dir = TempDir::new("cli-regen");
    dir.write("in", "x\n");
    dir.write("config", "1\n");
    // The generator rewrites build.ninja to add a second target.
    dir.write("gen.sh", "");
    std::fs::write(
        dir.path().join("gen.sh"),
        "#!/bin/sh\ncat > build.ninja <<'EOF'\n\
         rule regen\n  command = ./gen.sh\n  generator = 1\n\n\
         rule copy\n  command = cp $in $out\n\n\
         build build.ninja: regen config gen.sh\n\
         build out: copy in\nbuild out2: copy in\n\
         default out out2\nEOF\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.path().join("gen.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    dir.write(
        "build.ninja",
        "rule regen\n  command = ./gen.sh\n  generator = 1\n\n\
         rule copy\n  command = cp $in $out\n\n\
         build build.ninja: regen config gen.sh\nbuild out: copy in\ndefault out\n",
    );
    // Make an input of the manifest newer than the manifest itself, so the
    // generator has something to do.
    dir.touch("config");

    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
    // The regenerated manifest added out2, and the same invocation built it.
    assert!(dir.exists("out"), "{}", run.all());
    assert!(
        dir.exists("out2"),
        "the reloaded manifest should be built too"
    );
}

// Needs a POSIX shell to write depfiles, sleep, or run a generator
// script. The engine behaviour itself is covered by the unit tests and
// by the ninja differential suite in dev/.
#[cfg(unix)]
#[test]
fn dyndep_adds_dependencies_during_the_build() {
    let dir = TempDir::new("cli-dyndep");
    dir.write("in", "x\n");
    dir.write("extra", "e\n");
    dir.write(
        "build.ninja",
        "rule makedd\n  command = printf 'ninja_dyndep_version = 1\\nbuild out: dyndep | extra\\n' > $out\n\n\
         rule copy\n  command = cp $in $out\n  dyndep = dd\n\n\
         build dd: makedd\nbuild out: copy in | dd\ndefault out\n",
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(dir.exists("out"));

    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 0, "{}", run.all());

    // The dynamically discovered input now drives rebuilds.
    dir.touch("extra");
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 1, "{}", run.all());
}

#[test]
fn validations_run_but_do_not_gate() {
    let dir = TempDir::new("cli-validation");
    dir.write("in", "x\n");
    dir.write(
        "build.ninja",
        &format!(
            "rule copy\n  command = {}\n\nrule check\n  command = {}\n\n\
             build out: copy in |@ checked\nbuild checked: check\ndefault out\n",
            copy_cmd(),
            touch_cmd()
        ),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(dir.exists("out"));
    assert!(
        dir.exists("checked"),
        "validation target should have been built"
    );
}

#[test]
fn response_files_are_written_and_removed() {
    let dir = TempDir::new("cli-rsp");
    dir.write("a", "a\n");
    dir.write("b", "b\n");
    dir.write(
        "build.ninja",
        &format!(
            "rule link\n  command = {}\n  rspfile = $out.rsp\n  \
             rspfile_content = $in\n\nbuild out: link a b\n",
            cat_into_out_cmd("$out.rsp")
        ),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert_eq!(dir.read("out"), "a b");
    assert!(!dir.exists("out.rsp"), "response file should be cleaned up");

    // ... unless asked to keep it.
    dir.remove("out");
    let run = shuriken(&dir, &["-j1", "-d", "keeprsp"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(dir.exists("out.rsp"));
}

#[test]
fn builddir_holds_the_logs() {
    let dir = TempDir::new("cli-builddir");
    dir.write("in", "x\n");
    dir.write(
        "build.ninja",
        // `touch` rather than `copy`: cmd's copy would read the forward slash
        // in `out/x` as a switch, while a redirect just opens the path.
        &format!(
            "builddir = out/logs\nrule make\n  command = {}\n\nbuild out/x: make in\n",
            touch_cmd()
        ),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(dir.exists("out/logs/.ninja_log"));
    assert!(dir.exists("out/x"));
}

#[test]
fn recompact_and_restat_tools_work() {
    let dir = simple_project("cli-recompact");
    shuriken(&dir, &["-j1"]);
    let before = dir.read(".ninja_log");

    let run = shuriken(&dir, &["-t", "recompact"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let after = dir.read(".ninja_log");
    assert!(after.starts_with("# ninja log v7"));
    assert_eq!(
        before.lines().count(),
        after.lines().count(),
        "no entries should be lost"
    );

    let run = shuriken(&dir, &["-t", "restat"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(
        commands_run(&run),
        0,
        "restat must not invalidate the build"
    );
}

// Needs a POSIX shell to write depfiles, sleep, or run a generator
// script. The engine behaviour itself is covered by the unit tests and
// by the ninja differential suite in dev/.
#[cfg(unix)]
#[test]
fn missingdeps_finds_undeclared_generated_inputs() {
    let dir = TempDir::new("cli-missingdeps");
    dir.write("in", "x\n");
    dir.write(
        "build.ninja",
        "rule gen\n  command = touch $out\n\n\
         rule cc\n  command = cp $in $out && printf '%s: %s generated.h\\n' $out $in > $out.d\n  \
         depfile = $out.d\n  deps = gcc\n\n\
         build generated.h: gen\nbuild out: cc in\nbuild all: phony out generated.h\ndefault all\n",
    );
    shuriken(&dir, &["-j1"]);
    let run = shuriken(&dir, &["-t", "missingdeps"]);
    assert_eq!(run.code, 3, "missing deps are reported with exit code 3");
    assert!(
        run.stdout.contains("Missing dep: out uses generated.h"),
        "{}",
        run.all()
    );
}

#[test]
fn shared_log_is_understood_after_a_manifest_edit() {
    // A build, then a manifest change that renames a target: the stale entry
    // must not confuse the next build.
    let dir = TempDir::new("cli-log-churn");
    dir.write("in", "x\n");
    dir.write(
        "build.ninja",
        &format!(
            "rule copy\n  command = {}\n\nbuild old: copy in\ndefault old\n",
            copy_cmd()
        ),
    );
    shuriken(&dir, &["-j1"]);
    dir.write(
        "build.ninja",
        &format!(
            "rule copy\n  command = {}\n\nbuild new: copy in\ndefault new\n",
            copy_cmd()
        ),
    );
    let run = shuriken(&dir, &["-j1"]);
    assert_eq!(commands_run(&run), 1, "{}", run.all());

    let run = shuriken(&dir, &["-t", "cleandead"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(!dir.exists("old"), "dead output should be cleaned");
    assert!(dir.exists("new"));
}
