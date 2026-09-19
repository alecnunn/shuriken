#!/usr/bin/env python3
"""Realistic C project test + performance comparison: ninja vs shuriken."""
import os, shutil, subprocess, sys, tempfile, time

NINJA = shutil.which("ninja")
SHURIKEN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/shuriken")
N_FILES = 12

def make_project(d):
    os.makedirs(d, exist_ok=True)
    os.makedirs(os.path.join(d, "src"), exist_ok=True)
    os.makedirs(os.path.join(d, "include"), exist_ok=True)
    with open(os.path.join(d, "include", "common.h"), "w") as f:
        f.write("#pragma once\n#define COMMON 1\nint helper(int);\n")
    for i in range(N_FILES):
        with open(os.path.join(d, "include", f"m{i}.h"), "w") as f:
            f.write(f'#pragma once\n#include "common.h"\nint f{i}(int);\n')
        with open(os.path.join(d, "src", f"m{i}.c"), "w") as f:
            f.write(f'#include "m{i}.h"\nint f{i}(int x) {{ return x + {i} + COMMON; }}\n')
    with open(os.path.join(d, "src", "main.c"), "w") as f:
        incs = "".join(f'#include "m{i}.h"\n' for i in range(N_FILES))
        calls = " + ".join(f"f{i}(1)" for i in range(N_FILES))
        f.write(f'#include <stdio.h>\n{incs}int helper(int x){{return x;}}\n'
                f'int main(){{ printf("%d\\n", {calls}); return 0; }}\n')
    objs = " ".join(f"obj/m{i}.o" for i in range(N_FILES)) + " obj/main.o"
    manifest = f"""\
cflags = -Iinclude -Wall -O1
rule cc
  command = gcc -MMD -MF $out.d $cflags -c $in -o $out
  depfile = $out.d
  deps = gcc
  description = CC $out

rule link
  command = gcc $in -o $out
  description = LINK $out

"""
    for i in range(N_FILES):
        manifest += f"build obj/m{i}.o: cc src/m{i}.c\n"
    manifest += "build obj/main.o: cc src/main.c\n"
    manifest += f"build prog: link {objs}\n"
    manifest += "default prog\n"
    with open(os.path.join(d, "build.ninja"), "w") as f:
        f.write(manifest)

def run(prog, d, args, expect=None):
    env = dict(os.environ); env["TERM"] = "dumb"
    p = subprocess.run([prog] + args, cwd=d, capture_output=True, text=True, env=env, timeout=300)
    return p

def steps(prog, d):
    """Run a realistic sequence; return a list of observations."""
    obs = []
    def note(label, p, extra=None):
        # Count how many edges ran, from the status lines.
        ran = len([l for l in p.stdout.splitlines() if l.startswith("[")])
        obs.append((label, p.returncode, ran, "no work to do" in p.stdout, extra))

    p = run(prog, d, ["-j4"]);                     note("full", p)
    p = run(prog, d, ["-j4"]);                     note("noop", p)
    # Program should work.
    out = subprocess.run([os.path.join(d, "prog")], capture_output=True, text=True)
    obs.append(("prog-output", out.returncode, 0, False, out.stdout.strip()))

    os.utime(os.path.join(d, "include", "m3.h"), None)
    p = run(prog, d, ["-j4"]);                     note("touch-header", p)

    os.utime(os.path.join(d, "include", "common.h"), None)
    p = run(prog, d, ["-j4"]);                     note("touch-common-header", p)

    with open(os.path.join(d, "src", "m5.c"), "a") as f:
        f.write("int extra5(void){return 5;}\n")
    p = run(prog, d, ["-j4"]);                     note("edit-source", p)

    # Changing a command line must rebuild everything that uses it.
    path = os.path.join(d, "build.ninja")
    text = open(path).read().replace("-O1", "-O2")
    open(path, "w").write(text)
    p = run(prog, d, ["-j4"]);                     note("change-flags", p)

    p = run(prog, d, ["-j4"]);                     note("noop2", p)
    p = run(prog, d, ["-t", "clean"]);             note("clean", p)
    p = run(prog, d, ["-j4"]);                     note("rebuild", p)

    # A deleted intermediate object must come back.
    os.remove(os.path.join(d, "obj", "m2.o"))
    p = run(prog, d, ["-j4"]);                     note("restore-object", p)

    # A deleted header that is still included must fail.
    os.remove(os.path.join(d, "include", "m7.h"))
    os.utime(os.path.join(d, "src", "m7.c"), None)
    p = run(prog, d, ["-j4"]);                     note("missing-header", p)
    return obs

def perf(prog, d, label, args, runs=3):
    best = None
    for _ in range(runs):
        t = time.perf_counter()
        p = run(prog, d, args)
        dt = time.perf_counter() - t
        assert p.returncode == 0, (label, p.stdout, p.stderr)
        best = dt if best is None else min(best, dt)
    return best

def make_big(d, n):
    os.makedirs(d, exist_ok=True)
    lines = ["rule noop\n  command = true\n  description = NOOP $out\n\n"]
    for i in range(n):
        lines.append(f"build out{i}: noop in{i}\n")
        with open(os.path.join(d, f"in{i}"), "w") as f:
            f.write("x\n")
    lines.append("build all: phony " + " ".join(f"out{i}" for i in range(n)) + "\n")
    lines.append("default all\n")
    with open(os.path.join(d, "build.ninja"), "w") as f:
        f.write("".join(lines))

def main():
    root = tempfile.mkdtemp(prefix="realtest-")
    print(f"workspace: {root}")
    results = {}
    for prog in (NINJA, SHURIKEN):
        d = os.path.join(root, "proj-" + os.path.basename(prog))
        make_project(d)
        results[prog] = steps(prog, d)

    failures = []
    for (a, b) in zip(results[NINJA], results[SHURIKEN]):
        if a != b:
            failures.append(f"step {a[0]}: ninja={a[1:]} shuriken={b[1:]}")

    print("\n--- realistic C project ---")
    for a, b in zip(results[NINJA], results[SHURIKEN]):
        flag = "ok " if a == b else "DIFF"
        print(f"  {flag} {a[0]:<22} rc={a[1]} ran={a[2]} noop={a[3]} extra={a[4]}")

    # ---- performance ----
    print("\n--- performance (best of 3, seconds) ---")
    n = 5000
    for prog in (NINJA, SHURIKEN):
        d = os.path.join(root, f"big-{n}-" + os.path.basename(prog))
        make_big(d, n)
        full = perf(prog, d, "full", ["-j8"], runs=1)
        noop = perf(prog, d, "noop", ["-j8"], runs=3)
        load = perf(prog, d, "load", ["-t", "targets", "all"], runs=3)
        print(f"  {os.path.basename(prog):<9} {n} edges: full={full:.3f} noop={noop:.3f} "
              f"parse+tool={load:.3f}")

    if failures:
        print(f"\n{len(failures)} FAILURES")
        for f in failures:
            print("- " + f)
        return 1
    print("\nrealistic project: behaviour matches")
    return 0

if __name__ == "__main__":
    sys.exit(main())
