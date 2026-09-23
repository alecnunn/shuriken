# Development scripts

These compare `shuriken` against a real `ninja` binary. They are not part of the
crate and are not run by `cargo test`; they need `ninja` on `PATH` (1.13.x, and
a C compiler for `realtest.py`).

Build the release binary first:

```sh
cargo build --release
```

## `difftest.py` — behavioural differential suite

```sh
python3 dev/difftest.py target/release/shuriken
```

Runs 85 scenarios through both tools in identical temporary trees and compares:

* exit codes,
* stdout and stderr (normalised for the program name, timings, and — for
  parallel steps — completion order),
* every file produced,
* `.ninja_log` entries (output path → command hash, so hash compatibility is
  checked directly),
* `.ninja_deps` structure (paths and dependency lists, decoded from the binary
  format).

It also checks log interop in both directions: ninja builds a tree and shuriken
must then report "no work to do" in it, and vice versa, including a
deps-triggered rebuild after touching a header.

Scenarios cover builds and rebuilds, failures and `-k`, `restat`, `generator`
and manifest regeneration, depfiles and `deps = gcc` / `deps = msvc`, dyndep,
pools and the `console` pool, validations, response files, `builddir`, escaping,
CRLF and Unicode paths, log and deps-log recompaction, every subtool, and 17
kinds of malformed manifest.

Output: `ran 85 scenarios + interop` / `all scenarios agree`.

## `realtest.py` — a real C project

```sh
python3 dev/realtest.py target/release/shuriken
```

Generates a small C project (13 translation units, real headers, `gcc -MMD`
depfiles), then drives both tools through the same sequence of edits — full
build, no-op, touch a header, touch a shared header, edit a source file, change
a compiler flag, clean, rebuild, delete an object, delete a header — and
compares the exit code and the number of commands each tool ran at every step.
Also prints a rough timing comparison.

## `interrupt_test.py` — signals

```sh
python3 dev/interrupt_test.py target/release/shuriken
```

Starts a slow build, sends SIGINT (then SIGTERM) to the build tool, and checks
that both tools agree on the exit code, on deleting the partially written
output, and on removing `.ninja_lock`.

## Testing the Windows build from Linux

[cargo-xwin](https://github.com/rust-cross/cargo-xwin) supplies the MSVC CRT and
SDK so the crate cross-compiles, and Wine runs the result, so the Windows-only
code paths can be exercised without a Windows machine:

```sh
cargo install cargo-xwin          # needs clang-cl, lld-link and llvm-lib
export WINEDEBUG=-all
CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER=wine \
  cargo xwin test --target x86_64-pc-windows-msvc
cargo xwin clippy --target x86_64-pc-windows-msvc --all-targets
```

Six CLI tests are `#[cfg(unix)]`: they need a POSIX shell to write depfiles,
sleep, or run a generator script. Everything else runs, including builds that
spawn real processes through the `CreateProcess`-style launcher.

Wine is close to Windows, not identical to it — its filesystem layer is
case-insensitive over a case-sensitive one, for instance — so this catches
portability mistakes rather than replacing a real Windows run.

## `bench.py` — timings

```sh
python3 dev/bench.py target/release/shuriken
```

Synthetic graphs (5k and 30k independent edges, a 2k-edge chain, and 3k edges
with a 200-header deps log); reports full-build, no-op and `-t targets` times
for both tools.
## Releasing to crates.io

Publishing is driven by a tag. `.github/workflows/release.yml` re-runs the
checks (a tag push does not trigger CI, which watches branches), refuses a tag
whose version disagrees with `Cargo.toml`, and then publishes:

```sh
# 1. bump `version` in Cargo.toml, refresh Cargo.lock, commit
cargo check
git commit -am "release: 0.1.1"

# 2. tag and push; the tag drives the publish
git tag v0.1.1
git push origin main v0.1.1
```

The workflow needs a `CRATES_IO_TOKEN` repository secret holding a crates.io
API token with the `publish-update` scope (plus `publish-new` for the first
release).

Run the workflow manually (`workflow_dispatch`) for a rehearsal: it does
everything except the publish step. Locally, the same check is

```sh
cargo publish --dry-run --locked
cargo package --list          # what crates.io will receive
```

`dev/` and `.github/` are excluded from the packaged crate; `tests/` is kept so
the suite can run from the published sources.
