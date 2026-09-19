//! The `shuriken` command-line tool: a drop-in, ninja-compatible build driver.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use shuriken::build::{BuildConfig, Verbosity};
use shuriken::clean::CleanReport;
use shuriken::deps_log::DepsLog;
use shuriken::disk::DiskInterface;
use shuriken::engine::{Engine, EngineOptions};
use shuriken::error::{Error, Result};
use shuriken::exec::ExitStatus;
use shuriken::parse::{ParserOptions, PhonyCycleAction};
use shuriken::state::State;
use shuriken::status::{ConsoleStatus, Status};
use shuriken::tools::{self, InputsOptions};
use shuriken::util::guess_parallelism;
use shuriken::version::{NINJA_COMPAT_VERSION, SHURIKEN_VERSION};

mod platform;

const PROGRAM: &str = "shuriken";

fn main() -> ExitCode {
    let code = real_main();
    // Flush before exiting so nothing is lost.
    let _ = std::io::stdout().flush();
    ExitCode::from(code.min(255) as u8)
}

fn usage(parallelism: usize) {
    eprint!(
        "usage: {PROGRAM} [options] [targets...]

if targets are unspecified, builds the 'default' target (see manual).

options:
  --version      print version (\"{SHURIKEN_VERSION}\", ninja-compatible {NINJA_COMPAT_VERSION}\")
  -v, --verbose  show all command lines while building
  --quiet        don't show progress status, just command output

  -C DIR   change to DIR before doing anything else
  -f FILE  specify input build file [default=build.ninja]

  -j N     run N jobs in parallel (0 means infinity) [default={parallelism} on this system]
  -k N     keep going until N jobs fail (0 means infinity) [default=1]
  -l N     do not start new jobs if the load average is greater than N
  -n       dry run (don't run commands but act like they succeeded)

  -d MODE  enable debugging (use '-d list' to list modes)
  -t TOOL  run a subtool (use '-t list' to list subtools)
    terminates toplevel options; further flags are passed to the tool
  -w FLAG  adjust warnings (use '-w list' to list warnings)
"
    );
}

#[derive(Debug, Default)]
struct Options {
    input_file: String,
    working_dir: Option<String>,
    tool: Option<String>,
    tool_args: Vec<String>,
    targets: Vec<String>,
    phony_cycle_should_err: bool,
}

/// Outcome of parsing the command line.
enum Parsed {
    Run(Options, BuildConfig),
    Exit(u8),
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let (options, config) = match parse_args(&args) {
        Parsed::Exit(code) => return code as i32,
        Parsed::Run(o, c) => (o, c),
    };

    if let Some(dir) = &options.working_dir {
        // The quoting here is deliberate: it lets Emacs notice the directory
        // change for subsequent compiler messages.
        if options.tool.is_none() && config.verbosity != Verbosity::NoStatusUpdate {
            println!("{PROGRAM}: Entering directory `{dir}'");
        }
        if let Err(e) = std::env::set_current_dir(dir) {
            fatal(&format!("chdir to '{dir}' - {e}"));
            return 1;
        }
    }

    match run(options, config) {
        Ok(code) => code,
        Err(e) => report_error(&e),
    }
}

/// Print an error the way ninja does, and return the exit code to use.
fn report_error(e: &Error) -> i32 {
    match e {
        Error::Interrupted => {
            println!("{PROGRAM}: build stopped: interrupted by user.");
            ExitStatus::INTERRUPTED.code()
        }
        Error::Fatal(msg) => {
            fatal(msg);
            1
        }
        // A build that ran and stopped is reported on stdout, like ninja, so
        // that error output from the failing command stays the last thing on
        // stderr.
        Error::Build(msg) => {
            println!("{PROGRAM}: build stopped: {msg}.");
            1
        }
        other => {
            eprintln!("{PROGRAM}: error: {other}");
            1
        }
    }
}

fn fatal(message: &str) {
    eprintln!("{PROGRAM}: fatal: {message}");
}

fn parse_args(args: &[String]) -> Parsed {
    let mut options = Options {
        input_file: "build.ninja".to_string(),
        ..Default::default()
    };
    let mut config = BuildConfig::default();
    let mut parallelism_explicit = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // Once a tool is chosen, everything else belongs to the tool.
        if options.tool.is_some() {
            options.tool_args.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "--" {
            options.targets.extend(args[i + 1..].iter().cloned());
            break;
        }

        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline_value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            match name {
                "help" => {
                    usage(config.parallelism);
                    return Parsed::Exit(1);
                }
                "version" => {
                    println!("{NINJA_COMPAT_VERSION} ({PROGRAM} {SHURIKEN_VERSION})");
                    return Parsed::Exit(0);
                }
                "verbose" => config.verbosity = Verbosity::Verbose,
                "quiet" => config.verbosity = Verbosity::NoStatusUpdate,
                other => {
                    eprintln!("{PROGRAM}: unrecognized option '--{other}'");
                    usage(config.parallelism);
                    return Parsed::Exit(1);
                }
            }
            let _ = inline_value;
            i += 1;
            continue;
        }

        if arg.len() > 1 && arg.starts_with('-') {
            // A cluster of short options, the last of which may take a value.
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut ci = 0;
            while ci < chars.len() {
                let c = chars[ci];
                let takes_value = matches!(c, 'd' | 'f' | 'j' | 'k' | 'l' | 't' | 'w' | 'C');
                let value = if takes_value {
                    let rest: String = chars[ci + 1..].iter().collect();
                    if !rest.is_empty() {
                        ci = chars.len();
                        Some(rest)
                    } else {
                        i += 1;
                        match args.get(i) {
                            Some(v) => {
                                ci = chars.len();
                                Some(v.clone())
                            }
                            None => {
                                eprintln!("{PROGRAM}: option '-{c}' requires an argument");
                                return Parsed::Exit(1);
                            }
                        }
                    }
                } else {
                    ci += 1;
                    None
                };

                match c {
                    'd' => match debug_enable(&value.unwrap(), &mut config) {
                        Ok(true) => {}
                        Ok(false) => return Parsed::Exit(0),
                        Err(msg) => {
                            eprintln!("{PROGRAM}: {msg}");
                            return Parsed::Exit(1);
                        }
                    },
                    'f' => options.input_file = value.unwrap(),
                    'j' => {
                        let v = value.unwrap();
                        match v.parse::<i64>() {
                            Ok(n) if n >= 0 => {
                                config.parallelism = if n == 0 { usize::MAX } else { n as usize };
                                parallelism_explicit = true;
                            }
                            _ => {
                                fatal("invalid -j parameter");
                                return Parsed::Exit(1);
                            }
                        }
                    }
                    'k' => {
                        let v = value.unwrap();
                        match v.parse::<i64>() {
                            Ok(n) => {
                                config.failures_allowed =
                                    if n > 0 { n as usize } else { usize::MAX };
                            }
                            Err(_) => {
                                fatal("-k parameter not numeric; did you mean -k 0?");
                                return Parsed::Exit(1);
                            }
                        }
                    }
                    'l' => {
                        let v = value.unwrap();
                        match v.parse::<f64>() {
                            Ok(n) => config.max_load_average = n,
                            Err(_) => {
                                fatal("-l parameter not numeric: did you mean -l 0.0?");
                                return Parsed::Exit(1);
                            }
                        }
                    }
                    'n' => config.dry_run = true,
                    't' => {
                        let name = value.unwrap();
                        if name == "list" {
                            print_tool_list();
                            return Parsed::Exit(0);
                        }
                        if !is_known_tool(&name) {
                            let names = tool_names();
                            match shuriken::util::spellcheck(&name, &names) {
                                Some(s) => {
                                    fatal(&format!("unknown tool '{name}', did you mean '{s}'?"))
                                }
                                None => fatal(&format!("unknown tool '{name}'")),
                            }
                            return Parsed::Exit(1);
                        }
                        options.tool = Some(name);
                    }
                    'v' => config.verbosity = Verbosity::Verbose,
                    'w' => match warning_enable(&value.unwrap(), &mut options) {
                        Ok(true) => {}
                        Ok(false) => return Parsed::Exit(0),
                        Err(msg) => {
                            eprintln!("{PROGRAM}: {msg}");
                            return Parsed::Exit(1);
                        }
                    },
                    'C' => options.working_dir = Some(value.unwrap()),
                    'h' => {
                        usage(config.parallelism);
                        return Parsed::Exit(1);
                    }
                    other => {
                        eprintln!("{PROGRAM}: unrecognized option '-{other}'");
                        usage(config.parallelism);
                        return Parsed::Exit(1);
                    }
                }
            }
            i += 1;
            continue;
        }

        options.targets.push(arg.clone());
        i += 1;
    }

    if !parallelism_explicit {
        config.parallelism = guess_parallelism();
    }
    Parsed::Run(options, config)
}

fn debug_enable(name: &str, config: &mut BuildConfig) -> std::result::Result<bool, String> {
    match name {
        "list" => {
            println!(
                "debugging modes:
  stats        print operation counts/timing info
  explain      explain what caused a command to execute
  keepdepfile  don't delete depfiles after they're read
  keeprsp      don't delete @response files on success
multiple modes can be enabled via -d FOO -d BAR"
            );
            Ok(false)
        }
        "stats" => {
            STATS.store(true, Ordering::SeqCst);
            Ok(true)
        }
        "explain" => {
            config.explain = true;
            Ok(true)
        }
        "keepdepfile" => {
            config.keep_depfile = true;
            Ok(true)
        }
        "keeprsp" => {
            config.keep_rsp = true;
            Ok(true)
        }
        other => Err(format!("unknown debug setting '{other}'")),
    }
}

static STATS: AtomicBool = AtomicBool::new(false);

fn warning_enable(name: &str, options: &mut Options) -> std::result::Result<bool, String> {
    match name {
        "list" => {
            println!(
                "warning flags:
  phonycycle={{err,warn}}  phony build statement references itself"
            );
            Ok(false)
        }
        "phonycycle=err" => {
            options.phony_cycle_should_err = true;
            Ok(true)
        }
        "phonycycle=warn" => {
            options.phony_cycle_should_err = false;
            Ok(true)
        }
        "dupbuild=err" | "dupbuild=warn" => {
            eprintln!("{PROGRAM}: warning: deprecated warning 'dupbuild'");
            Ok(true)
        }
        "depfilemulti=err" | "depfilemulti=warn" => {
            eprintln!("{PROGRAM}: warning: deprecated warning 'depfilemulti'");
            Ok(true)
        }
        other => {
            let known = ["phonycycle=err", "phonycycle=warn"];
            match shuriken::util::spellcheck(other, &known) {
                Some(s) => Err(format!(
                    "unknown warning flag '{other}', did you mean '{s}'?"
                )),
                None => Err(format!("unknown warning flag '{other}'")),
            }
        }
    }
}

/// Tools that need the manifest but not the logs.
const TOOLS: &[(&str, &str)] = &[
    ("clean", "clean built files"),
    (
        "cleandead",
        "clean built files that are no longer produced by the manifest",
    ),
    (
        "commands",
        "list all commands required to rebuild given targets",
    ),
    ("compdb", "dump JSON compilation database to stdout"),
    (
        "compdb-targets",
        "dump JSON compilation database for a given list of targets to stdout",
    ),
    ("deps", "show dependencies stored in the deps log"),
    ("graph", "output graphviz dot file for targets"),
    (
        "inputs",
        "list all inputs required to rebuild given targets",
    ),
    (
        "missingdeps",
        "check deps log dependencies on generated files",
    ),
    (
        "multi-inputs",
        "print one or more sets of inputs required to build targets",
    ),
    ("query", "show inputs/outputs for a path"),
    ("recompact", "recompacts ninja-internal data structures"),
    ("restat", "restats all outputs in the build log"),
    ("rules", "list all rules"),
    ("targets", "list targets by their rule or depth in the DAG"),
];

fn tool_names() -> Vec<&'static str> {
    TOOLS.iter().map(|(n, _)| *n).collect()
}

fn is_known_tool(name: &str) -> bool {
    TOOLS.iter().any(|(n, _)| *n == name)
}

fn print_tool_list() {
    println!("{PROGRAM} subtools:");
    for (name, desc) in TOOLS {
        println!("{name:>13}  {desc}");
    }
}

fn run(options: Options, config: BuildConfig) -> Result<i32> {
    let interrupt = Arc::new(AtomicBool::new(false));
    platform::install_interrupt_handler(Arc::clone(&interrupt));

    let parser_options = ParserOptions {
        phony_cycle_action: if options.phony_cycle_should_err {
            PhonyCycleAction::Error
        } else {
            PhonyCycleAction::Warn
        },
        quiet: false,
    };

    // `-t recompact` and `-t restat` need the logs but must not rebuild the
    // manifest; the graph tools want the manifest only.
    let engine_options = EngineOptions {
        build: config.clone(),
        parser: parser_options,
        rebuild_manifest: options.tool.is_none(),
        interrupt: Some(Arc::clone(&interrupt)),
        disk: None,
        command_runner: None,
    };

    let mut engine = Engine::load(&options.input_file, engine_options)?;
    for w in engine.take_warnings() {
        eprintln!("{PROGRAM}: warning: {w}");
    }

    if let Some(tool) = options.tool.clone() {
        return run_tool(&tool, &options, &config, &mut engine);
    }

    let mut status = ConsoleStatus::new(config.verbosity, config.parallelism);
    status
        .printer_mut()
        .set_width_provider(Box::new(platform::terminal_width));

    let summary = match engine.build_with_status(&options.targets, &mut status) {
        Ok(s) => s,
        Err(Error::Interrupted) => {
            status.info("build stopped: interrupted by user.");
            return Ok(ExitStatus::INTERRUPTED.code());
        }
        Err(e @ (Error::Graph(_) | Error::Manifest(_) | Error::Io(..))) => {
            status.error(&e.to_string());
            return Ok(1);
        }
        Err(Error::Fatal(msg)) => {
            fatal(&msg);
            return Ok(1);
        }
        Err(e) => {
            // The build itself stopped; ninja reports this on stdout and exits
            // with the failing command's code.
            status.info(&format!("build stopped: {e}."));
            let code = engine.exit_code();
            return Ok(if code.success() { 1 } else { code.code() });
        }
    };

    if summary.up_to_date && config.verbosity != Verbosity::NoStatusUpdate {
        status.info("no work to do.");
    }
    if STATS.load(Ordering::SeqCst) {
        let t = engine.timings();
        let state = engine.state();
        eprintln!(
            "\nmetric                 value\n\
             manifest parse      {:>8.3} ms\n\
             log load            {:>8.3} ms\n\
             dependency scan     {:>8.3} ms\n\
             build               {:>8.3} ms\n\
             nodes               {:>8}\n\
             edges               {:>8}\n\
             edges started       {:>8}\n\
             edges finished      {:>8}",
            t.parse.as_secs_f64() * 1e3,
            t.logs.as_secs_f64() * 1e3,
            t.scan.as_secs_f64() * 1e3,
            t.build.as_secs_f64() * 1e3,
            state.nodes().len(),
            state.edges().len(),
            summary.edges_started,
            summary.edges_finished,
        );
    }
    Ok(0)
}

fn run_tool(
    tool: &str,
    options: &Options,
    config: &BuildConfig,
    engine: &mut Engine,
) -> Result<i32> {
    let args = &options.tool_args;
    match tool {
        "clean" => {
            let mut generator = false;
            let mut clean_rules = false;
            let mut rest: Vec<String> = Vec::new();
            for a in args {
                match a.as_str() {
                    "-g" => generator = true,
                    "-r" => clean_rules = true,
                    "-h" => {
                        println!(
                            "usage: {PROGRAM} -t clean [options] [targets]\n\n\
                             options:\n  \
                             -g     also clean files marked as generator output\n  \
                             -r     interpret targets as a list of rules to clean instead"
                        );
                        return Ok(1);
                    }
                    other => rest.push(other.to_string()),
                }
            }
            if clean_rules && rest.is_empty() {
                eprintln!("{PROGRAM}: error: expected a rule to clean");
                return Ok(1);
            }
            let report = if rest.is_empty() {
                engine.clean_all(generator)
            } else if clean_rules {
                engine.clean_rules(&rest)
            } else {
                engine.clean_targets(&rest)
            };
            print_clean_report(&report, config);
            Ok(if report.ok() { 0 } else { 1 })
        }
        "cleandead" => {
            let report = engine.clean_dead();
            print_clean_report(&report, config);
            Ok(if report.ok() { 0 } else { 1 })
        }
        "commands" => {
            let mut single = false;
            let mut rest = Vec::new();
            for a in args {
                match a.as_str() {
                    "-s" => single = true,
                    "-h" => {
                        println!(
                            "usage: {PROGRAM} -t commands [options] [targets]\n\n\
                             options:\n  \
                             -s     only print the final command to build [target], \
                             not the whole chain"
                        );
                        return Ok(1);
                    }
                    other => rest.push(other.to_string()),
                }
            }
            let targets = engine.resolve_targets(&rest)?;
            print!("{}", tools::commands(engine.state(), &targets, single));
            Ok(0)
        }
        "inputs" => {
            let mut opts = InputsOptions {
                shell_escape: true,
                ..Default::default()
            };
            let mut rest = Vec::new();
            for a in args {
                match a.as_str() {
                    "-0" | "--print0" => opts.print0 = true,
                    "-E" | "--no-shell-escape" => opts.shell_escape = false,
                    "-d" | "--dependency-order" => opts.dependency_order = true,
                    "-h" | "--help" => {
                        println!(
                            "usage: {PROGRAM} -t inputs [options] [targets]\n\n\
                             options:\n  \
                             -0, --print0            use NUL instead of newline as terminator\n  \
                             -E, --no-shell-escape   do not shell escape the result\n  \
                             -d, --dependency-order  sort results by dependency order"
                        );
                        return Ok(1);
                    }
                    other => rest.push(other.to_string()),
                }
            }
            let targets = engine.resolve_targets(&rest)?;
            let out = tools::inputs(engine.state(), &targets, opts);
            std::io::stdout().write_all(out.as_bytes()).ok();
            Ok(0)
        }
        "multi-inputs" => {
            let mut delimiter = "\t".to_string();
            let mut terminator = '\n';
            let mut rest = Vec::new();
            let mut it = args.iter().peekable();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-0" | "--print0" => terminator = '\0',
                    "-d" => {
                        if let Some(v) = it.next() {
                            delimiter = v.clone();
                        }
                    }
                    s if s.starts_with("--delimiter=") => {
                        delimiter = s["--delimiter=".len()..].to_string();
                    }
                    "-h" | "--help" => {
                        println!(
                            "usage: {PROGRAM} -t multi-inputs [options] [targets]\n\n\
                             options:\n  \
                             -d, --delimiter=DELIM   use DELIM instead of TAB\n  \
                             -0, --print0            use NUL instead of newline as terminator"
                        );
                        return Ok(1);
                    }
                    other => rest.push(other.to_string()),
                }
            }
            let targets = engine.resolve_targets(&rest)?;
            let out = tools::multi_inputs(engine.state(), &targets, &delimiter, terminator);
            std::io::stdout().write_all(out.as_bytes()).ok();
            Ok(0)
        }
        "deps" => {
            let targets = engine.resolve_targets_allow_empty(args)?;
            let out = {
                let (state, disk, deps_log) = engine.parts();
                tools::deps(state, disk, deps_log, &targets)
            };
            print!("{out}");
            Ok(0)
        }
        "missingdeps" => {
            let targets = engine.resolve_targets(args)?;
            let report = {
                let (state, disk, deps_log) = engine.parts_mut();
                tools::missing_deps(state, disk, deps_log, &targets)?
            };
            for line in &report.lines {
                println!("{line}");
            }
            print!("{}", report.summary());
            Ok(if report.had_missing_deps() { 3 } else { 0 })
        }
        "graph" => {
            let targets = engine.resolve_targets(args)?;
            let out = {
                let (state, disk) = engine.state_and_disk();
                tools::graph(state, disk, &targets)
            };
            print!("{out}");
            Ok(0)
        }
        "query" => {
            if args.is_empty() {
                eprintln!("{PROGRAM}: error: expected a target to query");
                return Ok(1);
            }
            let targets = engine.resolve_targets(args)?;
            let out = {
                let (state, disk) = engine.state_and_disk();
                tools::query(state, disk, &targets)
            };
            print!("{out}");
            Ok(0)
        }
        "targets" => {
            let mut depth = 1;
            if let Some(mode) = args.first() {
                match mode.as_str() {
                    "rule" => {
                        let rule = args.get(1).cloned().unwrap_or_default();
                        if rule.is_empty() {
                            print!("{}", tools::targets_source_list(engine.state()));
                        } else {
                            print!("{}", tools::targets_by_rule(engine.state(), &rule));
                        }
                        return Ok(0);
                    }
                    "all" => {
                        print!("{}", tools::targets_all(engine.state()));
                        return Ok(0);
                    }
                    "depth" => {
                        depth = args.get(1).and_then(|d| d.parse::<i32>().ok()).unwrap_or(1);
                    }
                    other => {
                        let known = ["rule", "depth", "all"];
                        match shuriken::util::spellcheck(other, &known) {
                            Some(s) => eprintln!(
                                "{PROGRAM}: error: unknown target tool mode '{other}', \
                                 did you mean '{s}'?"
                            ),
                            None => {
                                eprintln!("{PROGRAM}: error: unknown target tool mode '{other}'")
                            }
                        }
                        return Ok(1);
                    }
                }
            }
            print!("{}", tools::targets_by_depth(engine.state(), depth)?);
            Ok(0)
        }
        "compdb" => {
            let mut expand = false;
            let mut rules = Vec::new();
            for a in args {
                match a.as_str() {
                    "-x" => expand = true,
                    "-h" => {
                        println!(
                            "usage: {PROGRAM} -t compdb [options] [rules]\n\n\
                             options:\n  \
                             -x     expand @rspfile style response file invocations"
                        );
                        return Ok(1);
                    }
                    other => rules.push(other.to_string()),
                }
            }
            let dir = tools::working_directory();
            print!("{}", tools::compdb(engine.state(), &rules, expand, &dir));
            Ok(0)
        }
        "compdb-targets" => {
            let mut expand = false;
            let mut rest = Vec::new();
            for a in args {
                match a.as_str() {
                    "-x" => expand = true,
                    "-h" => {
                        println!("usage: {PROGRAM} -t compdb-targets [-hx] target [targets]");
                        return Ok(1);
                    }
                    other => rest.push(other.to_string()),
                }
            }
            if rest.is_empty() {
                println!("usage: {PROGRAM} -t compdb-targets [-hx] target [targets]");
                return Ok(1);
            }
            let targets = engine.resolve_targets(&rest)?;
            let dir = tools::working_directory();
            print!(
                "{}",
                tools::compdb_targets(engine.state(), &targets, expand, &dir)?
            );
            Ok(0)
        }
        "rules" => {
            let with_description = args.iter().any(|a| a == "-d");
            if args.iter().any(|a| a == "-h") {
                println!(
                    "usage: {PROGRAM} -t rules [options]\n\n\
                     options:\n  -d     also print the description of the rule"
                );
                return Ok(1);
            }
            print!("{}", tools::rules(engine.state(), with_description));
            Ok(0)
        }
        "recompact" => {
            engine.recompact_logs()?;
            Ok(0)
        }
        "restat" => {
            let outputs: Vec<String> = args.iter().filter(|a| *a != "-h").cloned().collect();
            engine.restat_log(&outputs)?;
            Ok(0)
        }
        other => {
            fatal(&format!("unknown tool '{other}'"));
            Ok(1)
        }
    }
}

fn print_clean_report(report: &CleanReport, config: &BuildConfig) {
    if config.verbosity == Verbosity::Quiet {
        return;
    }
    if config.verbosity == Verbosity::Verbose {
        println!("Cleaning...");
        for path in &report.removed {
            println!("Remove {path}");
        }
    } else {
        print!("Cleaning... ");
    }
    println!("{} files.", report.count);
    for e in &report.errors {
        eprintln!("{PROGRAM}: error: {e}");
    }
}

/// Small helpers so the tools can borrow the engine's parts at once.
trait EngineParts {
    fn parts(&self) -> (&State, &dyn DiskInterface, &DepsLog);
    fn parts_mut(&mut self) -> (&mut State, &dyn DiskInterface, &DepsLog);
    fn state_and_disk(&mut self) -> (&mut State, &dyn DiskInterface);
    fn resolve_targets_allow_empty(&self, names: &[String]) -> Result<Vec<shuriken::NodeId>>;
}

impl EngineParts for Engine {
    fn parts(&self) -> (&State, &dyn DiskInterface, &DepsLog) {
        (self.state(), self.disk(), self.deps_log())
    }

    fn parts_mut(&mut self) -> (&mut State, &dyn DiskInterface, &DepsLog) {
        self.split_mut()
    }

    fn state_and_disk(&mut self) -> (&mut State, &dyn DiskInterface) {
        let (state, disk, _) = self.split_mut();
        (state, disk)
    }

    /// Like `resolve_targets`, but an empty list stays empty (the `deps` tool
    /// treats "no targets" as "everything in the log").
    fn resolve_targets_allow_empty(&self, names: &[String]) -> Result<Vec<shuriken::NodeId>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        self.resolve_targets(names)
    }
}
