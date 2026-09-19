#!/usr/bin/env python3
"""Check that an interrupted build cleans up, for ninja and shuriken alike."""
import os, shutil, signal, subprocess, sys, tempfile, time

NINJA = shutil.which("ninja")
SHURIKEN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/shuriken")

MANIFEST = """\
rule slow
  command = printf partial > $out && sleep 5 && printf done >> $out
  description = SLOW $out
build out: slow
"""

def run_case(prog, signum):
    d = tempfile.mkdtemp(prefix="interrupt-")
    with open(os.path.join(d, "build.ninja"), "w") as f:
        f.write(MANIFEST)
    env = dict(os.environ); env["TERM"] = "dumb"
    # start_new_session so our own signal does not reach the child group; we
    # signal the build tool itself, exactly like a terminal Ctrl-C would.
    p = subprocess.Popen([prog, "-j1"], cwd=d, env=env, start_new_session=True,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    time.sleep(1.0)
    os.killpg(os.getpgid(p.pid), signum)
    try:
        out, err = p.communicate(timeout=20)
    except subprocess.TimeoutExpired:
        p.kill()
        out, err = p.communicate()
        return {"timeout": True}
    return {
        "exit": p.returncode,
        "output_left": os.path.exists(os.path.join(d, "out")),
        "lock_left": os.path.exists(os.path.join(d, ".ninja_lock")),
        "log_left": os.path.exists(os.path.join(d, ".ninja_log")),
        "stdout": out.strip().replace(prog, "BUILD"),
        "stderr": err.strip().replace(prog, "BUILD"),
        "dir": d,
    }

def main():
    failures = []
    for signame, signum in (("SIGINT", signal.SIGINT), ("SIGTERM", signal.SIGTERM)):
        print(f"\n### {signame}")
        results = {}
        for prog in (NINJA, SHURIKEN):
            r = run_case(prog, signum)
            results[prog] = r
            print(f"  {os.path.basename(prog):<9} exit={r.get('exit')} "
                  f"partial_output_left={r.get('output_left')} lock_left={r.get('lock_left')}")
            print(f"            stdout={r.get('stdout')!r}")
            shutil.rmtree(r.get("dir", "/nonexistent"), ignore_errors=True)
        a, b = results[NINJA], results[SHURIKEN]
        for key in ("exit", "output_left", "lock_left"):
            if a.get(key) != b.get(key):
                failures.append(f"{signame} {key}: ninja={a.get(key)} shuriken={b.get(key)}")
    if failures:
        print("\nFAILURES:")
        for f in failures:
            print(" - " + f)
        return 1
    print("\ninterrupt handling matches")
    return 0

if __name__ == "__main__":
    sys.exit(main())
