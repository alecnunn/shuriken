#!/usr/bin/env python3
"""Differential test harness: run the same scenarios through ninja and shuriken
and compare observable behaviour."""
import os, re, shutil, struct, subprocess, sys, tempfile

NINJA = shutil.which("ninja")
SHURIKEN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/shuriken")
ROOT = tempfile.mkdtemp(prefix="difftest-")

def norm_text(s, prog):
    s = s.replace(prog, "BUILD")
    s = re.sub(r"\b(ninja|shuriken)\b", "BUILD", s)
    # Timings and rates vary run to run.
    s = re.sub(r"\d+\.\d+s?", "T", s)
    # mtimes differ between runs by construction.
    s = re.sub(r"deps mtime \d+", "deps mtime M", s)
    s = re.sub(r"^\[\d+/\d+\] ", "[N/N] ", s, flags=re.M)
    return s

def parse_log(path):
    """Return {"header": ..., "entries": {output: hash}} from a .ninja_log.

    The log is a map from output to command hash: entries are appended and the
    last one for a path wins, and the order in which a recompaction rewrites
    them is not part of the format. Timings and mtimes are ignored because they
    differ between runs by construction.
    """
    if not os.path.exists(path):
        return None
    header = None
    entries = {}
    with open(path) as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("#"):
                header = line
                continue
            parts = line.split("\t")
            if len(parts) != 5:
                continue
            entries[parts[3]] = parts[4]
    return {"header": header, "entries": entries}

def parse_deps(path):
    """Return {output: [inputs]} from a .ninja_deps, ignoring mtimes."""
    if not os.path.exists(path):
        return None
    data = open(path, "rb").read()
    sig = b"# ninjadeps\n"
    assert data.startswith(sig), "bad signature"
    version = struct.unpack_from("<i", data, len(sig))[0]
    pos = len(sig) + 4
    nodes = []
    deps = {}
    while pos + 4 <= len(data):
        raw = struct.unpack_from("<I", data, pos)[0]
        is_deps = bool(raw >> 31)
        size = raw & 0x7FFFFFFF
        body = data[pos + 4: pos + 4 + size]
        pos += 4 + size
        if is_deps:
            out_id, lo, hi = struct.unpack_from("<III", body, 0)
            ids = struct.unpack_from("<%dI" % ((size // 4) - 3), body, 12)
            deps[nodes[out_id]] = [nodes[i] for i in ids]
        else:
            path_bytes = body[:-4].rstrip(b"\0")
            checksum = struct.unpack_from("<I", body, size - 4)[0]
            assert (~checksum) & 0xFFFFFFFF == len(nodes), "checksum mismatch"
            nodes.append(path_bytes.decode())
    return {"version": version, "deps": deps}

def run_case(prog, name, files, steps, extra_env=None):
    d = os.path.join(ROOT, name, os.path.basename(prog))
    os.makedirs(d, exist_ok=True)
    for path, content in files.items():
        full = os.path.join(d, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(content)
        if path.endswith(".sh"):
            os.chmod(full, 0o755)
    env = dict(os.environ)
    env.pop("NINJA_STATUS", None)
    env["TERM"] = "dumb"
    if extra_env:
        env.update(extra_env)
    results = []
    for step in steps:
        p = subprocess.run([prog] + step, cwd=d, capture_output=True, text=True, env=env, timeout=120)
        out = norm_text(p.stdout, prog)
        err = norm_text(p.stderr, prog)
        # With more than one job in flight, completion order is not defined,
        # so compare the set of lines rather than their order.
        parallel = any(a.startswith("-j") and a not in ("-j1", "-j") for a in step) or \
                   ("-j" in step and step[step.index("-j") + 1] != "1")
        if parallel:
            out = "\n".join(sorted(out.splitlines()))
            err = "\n".join(sorted(err.splitlines()))
        results.append({"args": step, "code": p.returncode, "out": out, "err": err})
    return d, results

CASES = []

def case(name, files, steps, compare_logs=True, compare_output=True, extra_env=None):
    CASES.append(dict(name=name, files=files, steps=steps,
                      compare_logs=compare_logs, compare_output=compare_output,
                      extra_env=extra_env))

CAT = "rule cat\n  command = cat $in > $out\n\n"
TOUCH = "rule touch\n  command = touch $out\n\n"

# --- basic builds -----------------------------------------------------------
case("basic", {
    "build.ninja": CAT + "build out: cat in\n",
    "in": "hello\n",
}, [["-j1"], ["-j1"], ["-j1", "-v"]])

case("chain", {
    "build.ninja": CAT + "build b: cat a\nbuild c: cat b\nbuild d: cat c\ndefault d\n",
    "a": "a\n",
}, [["-j1"], ["-j1"]])

case("phony", {
    "build.ninja": CAT + "build b: cat a\nbuild all: phony b\ndefault all\n",
    "a": "a\n",
}, [["-j1"], ["-j1"], ["-j1", "all"]])

case("failure", {
    "build.ninja": "rule fail\n  command = echo oops 1>&2 && exit 3\n\nbuild out: fail\n",
}, [["-j1"], ["-j1", "-k", "0"]])

case("missing-input", {
    "build.ninja": CAT + "build out: cat missing\n",
}, [["-j1"]])

case("cycle", {
    "build.ninja": CAT + "build a: cat b\nbuild b: cat a\n",
}, [["-j1", "a"]])

case("restat", {
    "build.ninja": ("rule touch_restat\n  command = touch $out\n  restat = 1\n\n"
                    "rule unchanging\n  command = true\n  restat = 1\n\n"
                    "build mid: unchanging in\nbuild out: touch_restat mid\n"),
    "in": "in\n",
}, [["-j1"], ["-j1"]])

case("generator", {
    "build.ninja": ("rule regen\n  command = touch $out\n  generator = 1\n\n"
                    + CAT +
                    "build build.ninja: regen gen.input\nbuild out: cat in\ndefault out\n"),
    "in": "in\n",
    "gen.input": "x\n",
}, [["-j1"], ["-j1"]])

case("depfile", {
    "build.ninja": ("rule cc\n  command = cat $in > $out && printf '%s: %s h.h\\n' $out $in > $out.d\n"
                    "  depfile = $out.d\n\nbuild out: cc in\n"),
    "in": "in\n",
    "h.h": "h\n",
}, [["-j1"], ["-j1"]])

case("deps-gcc", {
    "build.ninja": ("rule cc\n  command = cat $in > $out && printf '%s: %s h.h\\n' $out $in > $out.d\n"
                    "  depfile = $out.d\n  deps = gcc\n\nbuild out: cc in\n"),
    "in": "in\n",
    "h.h": "h\n",
}, [["-j1"], ["-j1"], ["-j1", "-t", "deps"]])

case("pool", {
    "build.ninja": ("pool one\n  depth = 1\n\nrule slow\n  command = touch $out\n  pool = one\n\n"
                    "build a: slow\nbuild b: slow\nbuild all: phony a b\ndefault all\n"),
}, [["-j4"], ["-j4"]])

case("implicit-order-only", {
    "build.ninja": CAT + "build out | imp: cat in | ih || oo\nbuild oo: cat a\nbuild ih: cat a\n",
    "in": "in\n", "a": "a\n",
}, [["-j1", "out"], ["-j1", "out"]])

case("validation", {
    "build.ninja": CAT + TOUCH + "build out: cat in |@ v\nbuild v: touch\n",
    "in": "in\n",
}, [["-j1", "out"], ["-j1", "out"]])

case("rspfile", {
    "build.ninja": ("rule link\n  command = cat @$out.rsp > $out\n  rspfile = $out.rsp\n"
                    "  rspfile_content = $in\n\nbuild out: link in\n"),
    "in": "in\n",
}, [["-j1"], ["-j1"], ["-j1", "-d", "keeprsp"]])

case("dyndep", {
    "build.ninja": ("rule dd\n  command = printf 'ninja_dyndep_version = 1\\nbuild out: dyndep | extra\\n' > $out\n\n"
                    + CAT +
                    "rule copy\n  command = cat $in > $out\n  dyndep = dd\n\n"
                    "build dd: dd\nbuild out: copy in | dd\ndefault out\n"),
    "in": "in\n",
    "extra": "extra\n",
}, [["-j1"], ["-j1"]])

case("subninja-include", {
    "build.ninja": ("var = top\n" + CAT + "subninja sub.ninja\ninclude inc.ninja\n"
                    "build out: cat in\n  extra = $var\ndefault out\n"),
    "sub.ninja": "var = sub\nbuild subout: cat in\n",
    "inc.ninja": "var = inc\n",
    "in": "in\n",
}, [["-j1"], ["-j1", "-t", "commands", "out"], ["-j1", "subout"]])

case("console-pool", {
    "build.ninja": ("rule say\n  command = echo hello\n  pool = console\n\nbuild out: say\n"),
}, [["-j2"]])

case("multiple-outputs", {
    "build.ninja": ("rule two\n  command = touch $out\n\nbuild a b: two\ndefault a\n"),
}, [["-j1"], ["-j1", "b"]])

case("escaping", {
    "build.ninja": CAT + "build out$ put: cat in$ put\n",
    "in put": "x\n",
}, [["-j1", "-v"], ["-j1"]])

# --- tools ------------------------------------------------------------------
TOOLS_MANIFEST = (
    "cflags = -Wall\n"
    "rule cc\n  command = gcc $cflags -c $in -o $out\n  description = CC $out\n\n"
    "rule link\n  command = gcc $in -o $out\n\n"
    "build a.o: cc a.c\nbuild b.o: cc b.c\n  cflags = -O2\n"
    "build prog: link a.o b.o\n"
    "build all: phony prog\n"
    "default all\n"
)
case("tools", {
    "build.ninja": TOOLS_MANIFEST,
    "a.c": "int main(){}\n", "b.c": "int f(){}\n",
}, [
    ["-t", "targets"],
    ["-t", "targets", "all"],
    ["-t", "targets", "rule", "cc"],
    ["-t", "targets", "rule"],
    ["-t", "targets", "depth", "2"],
    ["-t", "commands", "prog"],
    ["-t", "commands", "-s", "prog"],
    ["-t", "rules"],
    ["-t", "rules", "-d"],
    ["-t", "query", "a.o"],
    ["-t", "query", "prog"],
    ["-t", "inputs", "prog"],
    ["-t", "inputs", "-E", "prog"],
    ["-t", "multi-inputs", "prog"],
    ["-t", "compdb"],
    ["-t", "compdb", "cc"],
    ["-t", "compdb-targets", "a.o"],
    ["-n"],
], compare_logs=False)

case("tool-clean", {
    "build.ninja": CAT + "build b: cat a\nbuild c: cat b\ndefault c\n",
    "a": "a\n",
}, [["-j1"], ["-t", "clean"], ["-j1"], ["-t", "clean", "b"], ["-j1"], ["-t", "clean", "-r", "cat"]])

case("tool-graph", {
    "build.ninja": TOOLS_MANIFEST,
}, [["-t", "graph", "prog"]], compare_logs=False, compare_output=False)

# --- manifest errors --------------------------------------------------------
ERRORS = {
    "err-unknown-rule": "build out: nope\n",
    "err-no-command": "rule r\n",
    "err-bad-var": "rule r\n  command = x\n  bogus = 1\n",
    "err-dupe-rule": "rule r\n  command = x\nrule r\n  command = y\n",
    "err-dupe-output": CAT + "build a: cat b\nbuild a: cat c\n",
    "err-tab": CAT + "build a: cat b\n\tx = 1\n",
    "err-bad-escape": "x = a$*b\n",
    "err-pool-depth": "pool p\n",
    "err-default-unknown": "default nope\n",
    "err-empty-build": "build\n",
    "err-expected-colon": CAT + "build out\n",
    "err-unexpected-eof": "x = 1",
    "err-required-version": "ninja_required_version = 99.9\n",
    "err-missing-include": "include nope.ninja\n",
    "err-bad-rspfile": "rule r\n  command = x\n  rspfile = y\n",
    "err-comment-eof": "# no newline",
    "err-phony-cycle": "build a: phony a\n",
}
for n, text in ERRORS.items():
    case(n, {"build.ninja": text}, [["-j1"]], compare_logs=False)


# --- batch 2: harder cases --------------------------------------------------
case("keep-going", {
    "build.ninja": ("rule fail\n  command = exit 1\n\n" + TOUCH +
                    "build f1: fail\nbuild f2: fail\nbuild ok: touch\n"
                    "build all: phony f1 f2 ok\ndefault all\n"),
}, [["-j1", "-k", "0"], ["-j1", "-k", "2"], ["-j1"]])

case("failure-then-success", {
    "build.ninja": ("rule maybe\n  command = test -f flag && touch $out\n\n"
                    "build out: maybe\n"),
}, [["-j1"], ["-j1"]])

case("builddir", {
    "build.ninja": "builddir = out/logs\n" + CAT + "build out/x: cat in\n",
    "in": "in\n",
}, [["-j1"], ["-j1"], ["-t", "recompact"], ["-j1"]])

case("subdir-outputs", {
    "build.ninja": CAT + "build deep/a/b/c.txt: cat in\n",
    "in": "in\n",
}, [["-j1"], ["-j1"]])

case("in-newline", {
    "build.ninja": ("rule show\n  command = printf '%s' \"$in_newline\" > $out\n\n"
                    "build out: show a b c\n"),
    "a": "a\n", "b": "b\n", "c": "c\n",
}, [["-j1"], ["-j1", "-t", "commands", "out"]])

case("rule-var-refs-rule-var", {
    "build.ninja": ("rule cc\n  command = $cc $flags -o $out $in\n  cc = gcc\n"
                    "  flags = $base -O2\n  base = -Wall\n\nbuild out: cc in\n"),
    "in": "in\n",
}, [["-t", "commands", "out"], ["-n"]])

case("default-multiple", {
    "build.ninja": CAT + "build a: cat i\nbuild b: cat i\nbuild c: cat i\ndefault a b\n",
    "i": "i\n",
}, [["-j1"], ["-j1", "-t", "targets"]])

case("crlf", {
    "build.ninja": "rule cat\r\n  command = cat $in > $out\r\n\r\nbuild out: cat in\r\n",
    "in": "in\n",
}, [["-j1"], ["-j1", "-t", "commands", "out"]])

case("unicode", {
    "build.ninja": CAT + "build oútput.txt: cat ínput.txt\n",
    "ínput.txt": "unicode\n",
}, [["-j1"], ["-j1"]])

case("depfile-mismatch", {
    "build.ninja": ("rule cc\n  command = cat $in > $out && printf 'other: x\\n' > $out.d\n"
                    "  depfile = $out.d\n\nbuild out: cc in\n"),
    "in": "in\n",
}, [["-j1"], ["-j1"]])

case("depfile-multiple-outs", {
    "build.ninja": ("rule cc\n  command = touch $out && printf 'a b: h.h\\n' > dep.d\n"
                    "  depfile = dep.d\n\nbuild a b: cc in\n"),
    "in": "in\n", "h.h": "h\n",
}, [["-j1"], ["-j1"]])

case("depfile-unknown-out", {
    "build.ninja": ("rule cc\n  command = touch $out && printf 'a zz: h.h\\n' > dep.d\n"
                    "  depfile = dep.d\n\nbuild a: cc in\n"),
    "in": "in\n", "h.h": "h\n",
}, [["-j1"], ["-j1"]])

case("deps-msvc", {
    "build.ninja": ("rule cl\n  command = cat $in > $out && printf 'Note: including file: h.h\\n'\n"
                    "  deps = msvc\n\nbuild out: cl in\n"),
    "in": "in\n", "h.h": "h\n",
}, [["-j1"], ["-j1"], ["-t", "deps"]])

case("deps-msvc-prefix", {
    "build.ninja": ("rule cl\n  command = cat $in > $out && printf 'INC: h.h\\n'\n"
                    "  deps = msvc\n  msvc_deps_prefix = INC: \n\nbuild out: cl in\n"),
    "in": "in\n", "h.h": "h\n",
}, [["-j1"], ["-j1"], ["-t", "deps"]])

case("restat-updates", {
    "build.ninja": ("rule gen\n  command = cat $in > $out\n  restat = 1\n\n"
                    + CAT + "build mid: gen in\nbuild out: cat mid\n"),
    "in": "in\n",
}, [["-j1"], ["-j1"]])

case("restat-no-output", {
    "build.ninja": ("rule noout\n  command = true\n  restat = 1\n\n"
                    + CAT + "build stamp: noout in\nbuild out: cat stamp\n"),
    "in": "in\n",
}, [["-j1", "out"], ["-j1", "out"]])

case("phony-chain", {
    "build.ninja": (TOUCH + "build a: touch\nbuild p1: phony a\nbuild p2: phony p1\n"
                    "build p3: phony p2\ndefault p3\n"),
}, [["-j1"], ["-j1"]])

case("phony-no-inputs", {
    "build.ninja": "build nothing: phony\nbuild also: phony nothing\ndefault also\n",
}, [["-j1"], ["-j1"]])

case("order-only-missing", {
    "build.ninja": CAT + "build out: cat in || missing_dir\n",
    "in": "in\n",
}, [["-j1"]])

case("tool-cleandead", {
    "build.ninja": CAT + "build a: cat i\nbuild b: cat i\ndefault a b\n",
    "i": "i\n",
}, [["-j1"], ["-t", "cleandead"], ["-f", "second.ninja", "-t", "cleandead"]])

case("tool-restat", {
    "build.ninja": CAT + "build a: cat i\n",
    "i": "i\n",
}, [["-j1"], ["-t", "restat"], ["-t", "restat", "a"], ["-j1"]])

case("tool-missingdeps", {
    "build.ninja": ("rule gen\n  command = touch $out\n\n"
                    "rule cc\n  command = cat $in > $out && printf '%s: %s gen.h\\n' $out $in > $out.d\n"
                    "  depfile = $out.d\n  deps = gcc\n\n"
                    "build gen.h: gen\nbuild out: cc in\nbuild all: phony out gen.h\ndefault all\n"),
    "in": "in\n",
}, [["-j1"], ["-t", "missingdeps"], ["-t", "missingdeps", "out"]])

case("jobs-zero", {
    "build.ninja": TOUCH + "build a: touch\nbuild b: touch\nbuild all: phony a b\ndefault all\n",
}, [["-j0"], ["-j0"]])

case("ninja-status-env", {
    "build.ninja": CAT + "build out: cat in\n",
    "in": "in\n",
}, [["-j1"]], extra_env={"NINJA_STATUS": "[%s/%t %p %f %r %u] "})

case("explain", {
    "build.ninja": CAT + "build out: cat in\n",
    "in": "in\n",
}, [["-j1", "-d", "explain"], ["-j1", "-d", "explain"]])

case("dry-run-then-build", {
    "build.ninja": CAT + "build out: cat in\n",
    "in": "in\n",
}, [["-n"], ["-j1"], ["-n"]])

case("many-edges", {
    "build.ninja": (CAT + "".join(f"build o{i}: cat in\n" for i in range(60))
                    + "build all: phony " + " ".join(f"o{i}" for i in range(60)) + "\ndefault all\n"),
    "in": "in\n",
}, [["-j1"], ["-j1"]])

case("deep-chain", {
    "build.ninja": (CAT + "build s0: cat in\n"
                    + "".join(f"build s{i}: cat s{i-1}\n" for i in range(1, 40))
                    + "default s39\n"),
    "in": "in\n",
}, [["-j1"], ["-j1"]])

case("var-shadowing", {
    "build.ninja": ("x = global\n"
                    "rule r\n  command = echo $x $y > $out\n  y = ruley\n\n"
                    "build a: r\nbuild b: r\n  x = edgex\n  y = edgey\n"
                    "build all: phony a b\ndefault all\n"),
}, [["-t", "commands", "all"], ["-j1"]])

case("dollar-escapes", {
    "build.ninja": ("rule r\n  command = printf '%s' '$$HOME $$$$ a$ b' > $out\n\n"
                    "build out: r\n"),
}, [["-j1", "-v"], ["-t", "commands", "out"]])

case("long-lines", {
    "build.ninja": (CAT + "build out: cat " + " ".join(f"f{i}" for i in range(50)) + "\n"),
    **{f"f{i}": f"{i}\n" for i in range(50)},
}, [["-j1"], ["-t", "commands", "out"]])

case("tool-unknown", {"build.ninja": CAT}, [["-t", "nosuchtool"], ["-t", "clen"]], compare_logs=False)

case("target-typo", {
    "build.ninja": CAT + "build target: cat in\n",
    "in": "in\n",
}, [["-j1", "targt"], ["-j1", "clean"], ["-j1", "help"]], compare_logs=False)

case("caret-syntax", {
    "build.ninja": CAT + "build b: cat a\nbuild c: cat b\n",
    "a": "a\n",
}, [["-j1", "a^"], ["-j1", "-t", "query", "a^"]])


# --- batch 3: log recompaction and churn ------------------------------------
# Enough entries and enough churn to cross the recompaction thresholds.
def churn_manifest(n, variant):
    m = ["rule tch\n  command = touch $out\n\n"]
    for i in range(n):
        m.append(f"build o{i}: tch i{i}\n")
    if variant:
        # A second generation of targets, so the old entries go stale.
        for i in range(n):
            m.append(f"build p{i}: tch i{i}\n")
    m.append("build all: phony " + " ".join(f"o{i}" for i in range(n)))
    if variant:
        m.append(" " + " ".join(f"p{i}" for i in range(n)))
    m.append("\ndefault all\n")
    return "".join(m)

case("log-recompact", {
    "build.ninja": churn_manifest(150, False),
    "second.ninja": churn_manifest(150, True),
    **{f"i{i}": "x\n" for i in range(150)},
}, [["-j4"], ["-f", "second.ninja", "-j4"], ["-t", "recompact"], ["-j4"], ["-t", "recompact"], ["-j4"]])

def deps_manifest(n, headers):
    m = ["rule cc\n  command = touch $out && printf '%s:" +
         "".join(f" h{h}.h" for h in range(headers)) + "\\n' $out > $out.d\n" +
         "  depfile = $out.d\n  deps = gcc\n\n"]
    for i in range(n):
        m.append(f"build o{i}: cc i{i}\n")
    m.append("build all: phony " + " ".join(f"o{i}" for i in range(n)) + "\ndefault all\n")
    return "".join(m)

case("deps-log-churn", {
    "build.ninja": deps_manifest(40, 5),
    "second.ninja": deps_manifest(40, 9),
    **{f"i{i}": "x\n" for i in range(40)},
    **{f"h{h}.h": "x\n" for h in range(9)},
}, [["-j4"], ["-f", "second.ninja", "-j4"], ["-t", "recompact"], ["-j4", "-t", "deps"], ["-j4"]])

case("log-shared-with-stale-entries", {
    "build.ninja": CAT + "build a: cat i\nbuild b: cat i\ndefault a b\n",
    "second.ninja": CAT + "build a: cat i\ndefault a\n",
    "i": "i\n",
}, [["-j1"], ["-f", "second.ninja", "-t", "recompact"], ["-f", "second.ninja", "-j1"],
    ["-f", "second.ninja", "-t", "cleandead"], ["-j1"]])

case("restat-chain", {
    "build.ninja": ("rule stamp\n  command = touch $out\n  restat = 1\n\n"
                    "rule nochange\n  command = true\n  restat = 1\n\n"
                    + CAT +
                    "build s1: nochange in\nbuild s2: nochange s1\nbuild out: cat s2\n"),
    "in": "in\n",
}, [["-j1", "out"], ["-j1", "out"], ["-j1", "out"]])

case("generator-restat", {
    "build.ninja": ("rule regen\n  command = true\n  generator = 1\n  restat = 1\n\n"
                    + CAT +
                    "build build.ninja: regen conf\nbuild out: cat in\ndefault out\n"),
    "in": "in\n", "conf": "c\n",
}, [["-j1"], ["-j1"]])

case("depfile-deleted-header", {
    "build.ninja": ("rule cc\n  command = cat $in > $out && printf '%s: %s dep.h\\n' $out $in > $out.d\n"
                    "  depfile = $out.d\n  deps = gcc\n\nbuild out: cc in\n"),
    "in": "in\n", "dep.h": "h\n",
}, [["-j1"], ["-j1"]])

case("output-is-directory", {
    "build.ninja": TOUCH + "build adir: touch\n",
}, [["-j1"], ["-t", "clean"]])

case("phony-as-input-of-real", {
    "build.ninja": (CAT + TOUCH + "build stamp: touch\nbuild p: phony stamp\n"
                    "build out: cat in | p\n"),
    "in": "in\n",
}, [["-j1", "out"], ["-j1", "out"]])

case("duplicate-input", {
    "build.ninja": CAT + "build out: cat in in in\n",
    "in": "in\n",
}, [["-j1"], ["-j1", "-t", "commands", "out"]])

case("same-order-only-twice", {
    "build.ninja": (CAT + TOUCH + "build oo: touch\n"
                    "build a: cat in || oo\nbuild b: cat a || oo\ndefault b\n"),
    "in": "in\n",
}, [["-j1"], ["-j1"]])


case("deps-gcc-then-dry-run", {
    "build.ninja": ("rule cc\n  command = cat $in > $out && printf '%s: %s h.h\\n' $out $in > $out.d\n"
                    "  depfile = $out.d\n  deps = gcc\n\nbuild out: cc in\n"),
    "in": "in\n", "h.h": "h\n",
}, [["-j1"], ["-n"], ["-j1"], ["-j1", "-d", "keepdepfile"], ["-j1"]])

case("msvc-blank-lines", {
    "build.ninja": ("rule cl\n  command = cat $in > $out && printf 'first\\n\\nNote: including file: h.h\\n\\nsecond\\n'\n"
                    "  deps = msvc\n\nbuild out: cl in\n"),
    "in": "in\n", "h.h": "h\n",
}, [["-j1"], ["-t", "deps"]])

# --- interop: ninja writes the logs, shuriken reads them (and vice versa) ---
INTEROP = {
    "build.ninja": ("rule cc\n  command = cat $in > $out && printf '%s: %s h.h\\n' $out $in > $out.d\n"
                    "  depfile = $out.d\n  deps = gcc\n\n"
                    "build out: cc in\nbuild out2: cc in2\ndefault out out2\n"),
    "in": "in\n", "in2": "in2\n", "h.h": "h\n",
}

def run_interop():
    failures = []
    for first, second in ((NINJA, SHURIKEN), (SHURIKEN, NINJA)):
        d = os.path.join(ROOT, "interop", os.path.basename(first))
        os.makedirs(d, exist_ok=True)
        for path, content in INTEROP.items():
            with open(os.path.join(d, path), "w") as f:
                f.write(content)
        env = dict(os.environ); env["TERM"] = "dumb"
        p1 = subprocess.run([first, "-j1"], cwd=d, capture_output=True, text=True, env=env)
        p2 = subprocess.run([second, "-j1"], cwd=d, capture_output=True, text=True, env=env)
        ok = p1.returncode == 0 and p2.returncode == 0 and "no work to do" in (p2.stdout + p2.stderr)
        if not ok:
            failures.append(f"interop {os.path.basename(first)} -> {os.path.basename(second)}:\n"
                            f"  first: rc={p1.returncode} {p1.stdout!r} {p1.stderr!r}\n"
                            f"  second: rc={p2.returncode} {p2.stdout!r} {p2.stderr!r}")
        # And a deps-driven rebuild must also agree: touch the header.
        os.utime(os.path.join(d, "h.h"), None)
        p3 = subprocess.run([second, "-j1"], cwd=d, capture_output=True, text=True, env=env)
        if p3.returncode != 0 or "no work to do" in (p3.stdout + p3.stderr):
            failures.append(f"interop header touch {os.path.basename(second)}: "
                            f"rc={p3.returncode} {p3.stdout!r} {p3.stderr!r}")
    return failures

def main():
    if not NINJA:
        print("ninja not found")
        return 1
    failures = []
    for c in CASES:
        nd, nres = run_case(NINJA, c["name"], c["files"], c["steps"], c["extra_env"])
        sd, sres = run_case(SHURIKEN, c["name"], c["files"], c["steps"], c["extra_env"])
        for i, (a, b) in enumerate(zip(nres, sres)):
            label = f"{c['name']} step {i} ({' '.join(a['args'])})"
            if a["code"] != b["code"]:
                failures.append(f"{label}: exit code {a['code']} vs {b['code']}\n"
                                f"  ninja out: {a['out']!r}\n  ninja err: {a['err']!r}\n"
                                f"  shuri out: {b['out']!r}\n  shuri err: {b['err']!r}")
            elif c["compare_output"] and (a["out"] != b["out"] or a["err"] != b["err"]):
                failures.append(f"{label}: output differs\n"
                                f"  ninja out: {a['out']!r}\n  shuri out: {b['out']!r}\n"
                                f"  ninja err: {a['err']!r}\n  shuri err: {b['err']!r}")
        if c["compare_logs"]:
            nl, sl = parse_log(os.path.join(nd, ".ninja_log")), parse_log(os.path.join(sd, ".ninja_log"))
            if nl != sl:
                failures.append(f"{c['name']}: .ninja_log differs\n  ninja: {nl}\n  shuri: {sl}")
            try:
                ndp = parse_deps(os.path.join(nd, ".ninja_deps"))
                sdp = parse_deps(os.path.join(sd, ".ninja_deps"))
            except AssertionError as e:
                failures.append(f"{c['name']}: .ninja_deps unparseable: {e}")
            else:
                if ndp != sdp:
                    failures.append(f"{c['name']}: .ninja_deps differs\n  ninja: {ndp}\n  shuri: {sdp}")
            # Compare the set of produced files.
            def tree(root):
                out = {}
                for dirpath, _, names in os.walk(root):
                    for n in names:
                        rel = os.path.relpath(os.path.join(dirpath, n), root)
                        if os.path.basename(rel) in (".ninja_log", ".ninja_deps", ".ninja_lock"):
                            continue
                        out[rel] = open(os.path.join(dirpath, n), "rb").read()
                return out
            if tree(nd) != tree(sd):
                a, b = tree(nd), tree(sd)
                only_n = {k: v for k, v in a.items() if b.get(k) != v}
                only_s = {k: v for k, v in b.items() if a.get(k) != v}
                failures.append(f"{c['name']}: file trees differ\n  ninja-only: {only_n}\n  shuri-only: {only_s}")

    failures.extend(run_interop())

    print(f"ran {len(CASES)} scenarios + interop")
    if failures:
        print(f"\n{len(failures)} FAILURES:\n")
        for f in failures:
            print("- " + f + "\n")
        return 1
    print("all scenarios agree")
    return 0

if __name__ == "__main__":
    sys.exit(main())
