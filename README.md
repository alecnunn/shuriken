# shuriken

A ninja-compatible build engine, written in Rust, meant to be embedded in other
programs as well as used on its own.

It reads the same `build.ninja` manifests as [ninja](https://ninja-build.org/),
shares its `.ninja_log` and `.ninja_deps` files byte-for-byte, and reproduces
its rebuild logic, so you can point it at an existing build tree and get the
same answers.

```
$ shuriken -j8            # like `ninja -j8`
$ shuriken -t targets     # like `ninja -t targets`
```

## Why

`ninja` is excellent, but it is a program. When a tool wants to *own* a build —
a language server, a task runner, a test harness, a CI agent, a code generator
that wants to build what it just generated — it has to shell out to a binary and
parse its output, or reimplement the dependency logic and get the corner cases
wrong.

`shuriken` is the same engine as a library:

* the build graph is a data structure you can inspect and mutate,
* the filesystem, the process launcher and the progress reporting are all
  traits you can replace,
* everything the CLI does is a public function that returns a value rather than
  printing.

The library has **no dependencies** (not even `libc`) and contains no `unsafe`
code — `#![forbid(unsafe_code)]`. The one crate-internal exception is in the
binary, which uses two libc calls for terminal width and signal handling.

## Using the command line

The CLI mirrors ninja's, including its flags, its output format, its exit codes
and its error messages:

```
usage: shuriken [options] [targets...]

  -C DIR   change to DIR before doing anything else
  -f FILE  specify input build file [default=build.ninja]
  -j N     run N jobs in parallel (0 means infinity)
  -k N     keep going until N jobs fail (0 means infinity)
  -l N     do not start new jobs if the load average is greater than N
  -n       dry run (don't run commands but act like they succeeded)
  -v       show all command lines while building
  --quiet  don't show progress status, just command output
  -d MODE  enable debugging (-d list)
  -t TOOL  run a subtool (-t list)
  -w FLAG  adjust warnings (-w list)
```

Subtools: `clean`, `cleandead`, `commands`, `compdb`, `compdb-targets`, `deps`,
`graph`, `inputs`, `missingdeps`, `multi-inputs`, `query`, `recompact`,
`restat`, `rules`, `targets`.

`NINJA_STATUS` is honoured, including the `%e`/`%w`/`%E`/`%W`/`%P` ETA
placeholders. `-d stats` reports where the time went:

```
$ shuriken -d stats
metric                 value
manifest parse        36.441 ms
log load              10.995 ms
dependency scan       49.825 ms
build                  0.000 ms
nodes                  60001
edges                  30001
```

## Using the library

```toml
[dependencies]
shuriken = "0.1"
```

The quick path is `Engine`, which loads a manifest, keeps the logs and runs
builds:

```rust
use shuriken::{Engine, EngineOptions};

let mut engine = Engine::load("build.ninja", EngineOptions::default())?;
let summary = engine.build(&["all"])?;
println!("{} edges ran", summary.edges_finished);
# Ok::<(), shuriken::Error>(())
```

Everything underneath is public, so you can take over any layer.

**Inspect the graph.** No build required:

```rust
use shuriken::{Engine, EngineOptions, tools};

let engine = Engine::load("build.ninja", EngineOptions::default())?;
let state = engine.state();
for edge in state.edge_ids() {
    println!("{} <- {}", state.edge_rule_name(edge), state.edge_command(edge));
}
print!("{}", tools::targets_all(state));
# Ok::<(), shuriken::Error>(())
```

**Run commands somewhere else.** Implement `CommandRunner` to sandbox commands,
send them to a remote worker, or just record them:

```rust
use shuriken::{CommandRunner, CommandResult, EdgeId, State};
use shuriken::exec::ExitStatus;

struct Recorder(Vec<String>, Vec<EdgeId>);

impl CommandRunner for Recorder {
    fn can_run_more(&self) -> usize { 1 }
    fn start_command(&mut self, state: &State, edge: EdgeId) -> shuriken::Result<()> {
        self.0.push(state.edge_command(edge));
        self.1.push(edge);
        Ok(())
    }
    fn wait_for_command(&mut self) -> Option<CommandResult> {
        let edge = self.1.pop()?;
        Some(CommandResult { edge, status: ExitStatus::SUCCESS, output: String::new() })
    }
    fn active_edges(&self) -> Vec<EdgeId> { Vec::new() }
    fn abort(&mut self) {}
}
```

**Replace the filesystem.** `MemDisk` implements `DiskInterface` in memory,
which makes build logic testable without touching disk or spawning processes;
that is how much of this crate's own test suite works.

**Report progress your way.** `Status` is a trait with default no-op methods, so
you implement only the callbacks you care about (`build_edge_started`,
`build_edge_finished`, `edge_added_to_plan`, ...). `ConsoleStatus` is the
terminal implementation the CLI uses.

## Compatibility

Implemented, and checked against ninja 1.13.2:

| Area | Notes |
| --- | --- |
| Manifest syntax | `rule`, `build`, `pool`, `default`, `include`, `subninja`, variables, `$`-escapes, line continuations, `\|` / `\|\|` / `\|@` |
| Variable scoping | edge bindings, then rule bindings expanded in the edge's scope, then enclosing scopes; `subninja` child scopes |
| Special bindings | `command`, `description`, `depfile`, `deps` (`gcc`, `msvc`), `msvc_deps_prefix`, `dyndep`, `generator`, `restat`, `rspfile`, `rspfile_content`, `pool` |
| Rebuild logic | mtimes, command-hash changes, `restat` propagation, `generator` exemption, missing/stale dependency info |
| `.ninja_log` | version 7, including rapidhash command hashes, recompaction and `-t restat` |
| `.ninja_deps` | version 4 binary format, recompaction, truncation recovery |
| Depfiles | GCC/Clang escaping rules, multiple outputs, `deps = gcc` depfile consumption and deletion |
| Dyndep | `ninja_dyndep_version = 1`, implicit inputs/outputs, `restat` |
| Scheduling | pools (including `console`), critical-path priority, `-j`, `-k`, `-l`, validations (`\|@`) |
| Manifest regeneration | rebuilds `build.ninja` and restarts the build, up to 100 cycles |
| Interrupts | deletes partial outputs, removes `.ninja_lock`, exits 130 |

Deliberately not implemented:

* **GNU make jobserver integration** (`MAKEFLAGS`). `-j` works as usual.
* **`-t browse`**, which needs a bundled Python web server.
* **`-t msvc` / `-t wincodepage`**, Windows-only helpers that predate `deps = msvc`.
* **`-d nostatcache`**, which toggles a Windows-only stat cache this engine does
  not have.

Other differences worth knowing:

* Manifests, depfiles and paths must be valid UTF-8. ninja treats them as
  arbitrary bytes.
* `-l` (load average) is implemented on Linux, by reading `/proc/loadavg`.
  Elsewhere it is ignored.
* Commands run via `/bin/sh -c` on Unix and, as in ninja, are handed to
  `CreateProcess` without `cmd.exe` on Windows.
* Windows support is cross-compiled and exercised: the test suite is built for
  `x86_64-pc-windows-msvc` with [cargo-xwin](https://github.com/rust-cross/cargo-xwin)
  and run under [Wine](https://www.winehq.org/), where 189 of the 195 tests
  pass (the six skipped ones need a POSIX shell to write depfiles, sleep, or
  run a generator script). That covers the Windows command-line quoting, the
  `CreateProcess`-style launcher, separator handling and the log formats. It
  has not yet run on real Windows.
* Deep dependency *chains* are handled better: the graph scan recurses per edge
  in both tools, but shuriken runs the build on a large stack, so a 60,000-deep
  chain builds where ninja 1.13.2 overflows its stack.

## Performance

Synthetic graphs, best of several runs, 16-core Linux box, versus ninja 1.13.2
(lower is better):

| Scenario | ninja | shuriken |
| --- | --- | --- |
| 5,000 edges, full build | 1.10 s | 1.05 s |
| 5,000 edges, no-op | 0.015 s | 0.016 s |
| 30,000 edges, full build | 7.47 s | 7.39 s |
| 30,000 edges, no-op | 0.095 s | 0.108 s |
| 30,000 edges, manifest parse | 34 ms | 36 ms |
| 30,000 edges, `.ninja_log` load | 10 ms | 10 ms |
| 2,000-edge serial chain, full build | 2.57 s | 2.68 s |

A no-op build is dominated by `stat` calls, and a full build by process
spawning, so both tools end up in the same place; shuriken's remaining gap is
in graph walking, and its remaining edge is in command dispatch. Peak memory on
the 30,000-edge graph is 31 MB against ninja's 29 MB.

## How it is tested

* 195 unit and integration tests (`cargo test`).
* A differential suite that runs 85 scenarios through both ninja and shuriken in
  identical trees and compares exit codes, stdout/stderr, the produced files,
  and the contents of `.ninja_log` and `.ninja_deps`. Scenarios cover builds,
  incremental rebuilds, failures, `restat`, `generator`, depfiles, `deps`,
  dyndep, pools, validations, response files, log recompaction, every subtool,
  and 17 kinds of malformed manifest.
* Log interop in both directions: ninja builds a tree and shuriken reports "no
  work to do" in it, and vice versa, including deps-driven rebuilds.
* A realistic C project built with a real compiler, compared step by step
  against ninja through a sequence of edits (touch a header, touch a shared
  header, edit a source file, change a compiler flag, delete an object, delete a
  header).
* Interrupt behaviour (SIGINT/SIGTERM) compared against ninja.
* The whole suite cross-compiled for Windows and run under Wine (see
  [`dev/`](dev/)), which is how the Windows-only code paths are covered.
* A real third-party project: a game decompilation that cross-compiles 28
  translation units with MSVC 6.0 under Wine, from a manifest generated by its
  own toolchain driver. Building the same tree with each tool produced objects
  that were identical except for the `TimeDateStamp` the compiler writes from
  the clock, identical `.ninja_log` command hashes, and an identical match
  percentage from the project's `objdiff` progress report. Alternating the two
  tools in one tree — build with one, touch a source, rebuild with the other,
  delete an output, clean, recompact the logs — agreed at every step, and the
  project's driver (which shells out to `ninja -t commands`) worked unmodified
  with shuriken standing in for ninja.

The differential harness lives in [`dev/`](dev/) and needs a `ninja` binary on
`PATH`:

```sh
cargo build --release
python3 dev/difftest.py target/release/shuriken      # 85 scenarios + log interop
python3 dev/realtest.py target/release/shuriken      # a real C project, step by step
python3 dev/interrupt_test.py target/release/shuriken
python3 dev/bench.py target/release/shuriken
```

## License

MIT OR Apache-2.0.

This is an independent implementation. ninja itself is Apache-2.0 licensed and
was used as the specification: its file formats, command-line behaviour and
rebuild rules are reproduced here deliberately so the two tools can share a
build tree.
