#!/usr/bin/env python3
"""Benchmark ninja vs shuriken on synthetic graphs."""
import os, shutil, statistics, subprocess, sys, tempfile, time

NINJA = shutil.which("ninja")
SHURIKEN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/shuriken")

def run(prog, d, args):
    env = dict(os.environ); env["TERM"] = "dumb"
    p = subprocess.run([prog] + args, cwd=d, capture_output=True, text=True, env=env, timeout=900)
    if p.returncode != 0:
        raise SystemExit(f"{prog} {args} failed: {p.stdout[-2000:]} {p.stderr[-2000:]}")
    return p

def timeit(prog, d, args, runs=5):
    times = []
    for _ in range(runs):
        t = time.perf_counter()
        run(prog, d, args)
        times.append(time.perf_counter() - t)
    return min(times), statistics.median(times)

def make_graph(d, n, deps_per_edge=0, chain=False):
    """n edges; each output is `touch`ed so rebuilds converge."""
    os.makedirs(d, exist_ok=True)
    lines = ["rule tch\n  command = touch $out\n  description = T $out\n\n"]
    if deps_per_edge:
        lines = ["rule tch\n  command = touch $out && : $deps\n  description = T $out\n\n"]
    for i in range(n):
        if chain and i > 0:
            lines.append(f"build out{i}: tch out{i-1}\n")
        else:
            lines.append(f"build out{i}: tch in{i}\n")
            with open(os.path.join(d, f"in{i}"), "w") as f:
                f.write("x\n")
    lines.append("build all: phony " + " ".join(f"out{i}" for i in range(n)) + "\n")
    lines.append("default all\n")
    with open(os.path.join(d, "build.ninja"), "w") as f:
        f.write("".join(lines))

def make_headers_graph(d, n, headers):
    """Edges with depfiles so the deps log gets large."""
    os.makedirs(d, exist_ok=True)
    for h in range(headers):
        with open(os.path.join(d, f"h{h}.h"), "w") as f:
            f.write("x\n")
    lines = ["rule cc\n  command = touch $out && printf '%s:" +
             "".join(f" h{h}.h" for h in range(headers)) + "\\n' $out > $out.d\n" +
             "  depfile = $out.d\n  deps = gcc\n  description = CC $out\n\n"]
    for i in range(n):
        lines.append(f"build out{i}: cc in{i}\n")
        with open(os.path.join(d, f"in{i}"), "w") as f:
            f.write("x\n")
    lines.append("build all: phony " + " ".join(f"out{i}" for i in range(n)) + "\n")
    lines.append("default all\n")
    with open(os.path.join(d, "build.ninja"), "w") as f:
        f.write("".join(lines))

def bench(label, make, *, runs=5, jobs="8"):
    print(f"\n### {label}")
    print(f"{'':<10} {'full(s)':>9} {'no-op(s)':>9} {'-t targets(s)':>14}")
    root = tempfile.mkdtemp(prefix="bench-")
    for prog in (NINJA, SHURIKEN):
        d = os.path.join(root, os.path.basename(prog))
        make(d)
        # Full build (once; it is dominated by process spawns).
        t = time.perf_counter()
        run(prog, d, [f"-j{jobs}"])
        full = time.perf_counter() - t
        noop_min, _ = timeit(prog, d, [f"-j{jobs}"], runs=runs)
        tool_min, _ = timeit(prog, d, ["-t", "targets", "all"], runs=runs)
        print(f"{os.path.basename(prog):<10} {full:9.3f} {noop_min:9.3f} {tool_min:14.3f}")
    shutil.rmtree(root, ignore_errors=True)

def main():
    bench("5,000 independent edges", lambda d: make_graph(d, 5000))
    bench("30,000 independent edges", lambda d: make_graph(d, 30000), runs=3)
    bench("2,000-edge chain", lambda d: make_graph(d, 2000, chain=True), runs=3, jobs="8")
    bench("3,000 edges x 200 headers (deps log)",
          lambda d: make_headers_graph(d, 3000, 200), runs=3)
    return 0

if __name__ == "__main__":
    sys.exit(main())
