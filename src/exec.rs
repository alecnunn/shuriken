//! Running commands.
//!
//! [`CommandRunner`] is the interface the builder uses to execute edges;
//! [`RealCommandRunner`] runs them as subprocesses the way ninja does (through
//! `/bin/sh -c` on Unix, and by handing the command line straight to
//! `CreateProcess` on Windows), with stdout and stderr merged into one pipe so
//! interleaved output stays readable.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::Result;
use crate::state::{EdgeId, State};

/// A process exit code, with ninja's conventions: 0 is success and 130 means
/// the command was interrupted.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ExitStatus(pub i32);

impl ExitStatus {
    /// The command succeeded.
    pub const SUCCESS: ExitStatus = ExitStatus(0);
    /// The command failed in an unspecified way.
    pub const FAILURE: ExitStatus = ExitStatus(1);
    /// The command was interrupted (SIGINT/SIGTERM/SIGHUP).
    pub const INTERRUPTED: ExitStatus = ExitStatus(130);

    /// True if the command succeeded.
    pub fn success(self) -> bool {
        self == ExitStatus::SUCCESS
    }

    /// True if the command was interrupted.
    pub fn interrupted(self) -> bool {
        self == ExitStatus::INTERRUPTED
    }

    /// The raw code, for `exit()`.
    pub fn code(self) -> i32 {
        self.0
    }
}

/// The result of running one edge's command.
#[derive(Clone, Debug)]
pub struct CommandResult {
    /// The edge that ran.
    pub edge: EdgeId,
    /// Its exit status.
    pub status: ExitStatus,
    /// Everything it wrote to stdout and stderr, in order.
    pub output: String,
}

impl CommandResult {
    /// True if the command succeeded.
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// Executes the commands of a build.
pub trait CommandRunner {
    /// How many more commands may be started right now.
    fn can_run_more(&self) -> usize;

    /// Start `edge`'s command.
    fn start_command(&mut self, state: &State, edge: EdgeId) -> Result<()>;

    /// Wait for the next command to finish.
    ///
    /// Returns `None` if the build was interrupted.
    fn wait_for_command(&mut self) -> Option<CommandResult>;

    /// Edges whose commands are still running.
    fn active_edges(&self) -> Vec<EdgeId>;

    /// Stop all running commands.
    fn abort(&mut self);
}

/// A runner that pretends every command succeeds (`-n`).
#[derive(Default, Debug)]
pub struct DryRunCommandRunner {
    finished: VecDeque<EdgeId>,
}

impl DryRunCommandRunner {
    /// A new dry-run runner.
    pub fn new() -> DryRunCommandRunner {
        DryRunCommandRunner::default()
    }
}

impl CommandRunner for DryRunCommandRunner {
    fn can_run_more(&self) -> usize {
        usize::MAX
    }

    fn start_command(&mut self, _state: &State, edge: EdgeId) -> Result<()> {
        self.finished.push_back(edge);
        Ok(())
    }

    fn wait_for_command(&mut self) -> Option<CommandResult> {
        let edge = self.finished.pop_front()?;
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

/// How a command line is turned into a process.
///
/// The default matches ninja. Embedders can substitute their own launcher, for
/// example to run commands in a sandbox or on a remote worker.
pub type CommandLauncher = dyn Fn(&str) -> Command + Send + Sync;

/// Runs commands as real subprocesses.
pub struct RealCommandRunner {
    parallelism: usize,
    max_load_average: f64,
    /// Commands started but not yet reaped by [`CommandRunner::wait_for_command`].
    outstanding: usize,
    tx: Sender<CommandResult>,
    rx: Receiver<CommandResult>,
    jobs: Arc<Mutex<Vec<Job>>>,
    interrupt: Arc<AtomicBool>,
    launcher: Arc<CommandLauncher>,
}

struct Job {
    edge: EdgeId,
    child: Arc<Mutex<Option<Child>>>,
}

impl RealCommandRunner {
    /// A runner that starts at most `parallelism` commands at a time.
    pub fn new(parallelism: usize) -> RealCommandRunner {
        let (tx, rx) = channel();
        RealCommandRunner {
            parallelism: parallelism.max(1),
            max_load_average: -1.0,
            outstanding: 0,
            tx,
            rx,
            jobs: Arc::new(Mutex::new(Vec::new())),
            interrupt: Arc::new(AtomicBool::new(false)),
            launcher: Arc::new(default_launcher),
        }
    }

    /// Do not start new commands while the system load average exceeds this
    /// value. Negative values (the default) disable the limit.
    pub fn set_max_load_average(&mut self, load: f64) {
        self.max_load_average = load;
    }

    /// Share an interrupt flag with the caller; when it becomes true,
    /// [`CommandRunner::wait_for_command`] reports an interruption.
    pub fn set_interrupt_flag(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt = flag;
    }

    /// Replace the way command lines are turned into processes.
    pub fn set_launcher(&mut self, launcher: Arc<CommandLauncher>) {
        self.launcher = launcher;
    }

    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }
}

impl CommandRunner for RealCommandRunner {
    fn can_run_more(&self) -> usize {
        let mut capacity = self.parallelism.saturating_sub(self.outstanding) as i64;

        if self.max_load_average > 0.0 {
            if let Some(load) = crate::util::load_average() {
                let load_capacity = (self.max_load_average - load) as i64;
                if load_capacity < capacity {
                    capacity = load_capacity;
                }
            }
        }

        if capacity < 0 {
            capacity = 0;
        }
        // Always make progress, even if the load average is already too high.
        if capacity == 0 && self.outstanding == 0 {
            capacity = 1;
        }
        capacity as usize
    }

    fn start_command(&mut self, state: &State, edge: EdgeId) -> Result<()> {
        let command = state
            .edge_command_checked(edge)
            .map_err(crate::error::Error::build)?;
        let use_console = state.edge_use_console(edge);
        let tx = self.tx.clone();
        let jobs = Arc::clone(&self.jobs);
        let launcher = Arc::clone(&self.launcher);

        let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        jobs.lock().unwrap_or_else(|e| e.into_inner()).push(Job {
            edge,
            child: Arc::clone(&child_slot),
        });

        let builder = std::thread::Builder::new().name(format!("shuriken-edge-{}", edge.0));
        let spawn_result = builder.spawn(move || {
            let result = run_one(&*launcher, &command, use_console, &child_slot);
            let (status, output) = match result {
                Ok(v) => v,
                Err(e) => (ExitStatus::FAILURE, format!("{e}\n")),
            };
            // Drop our handle on the child before reporting completion.
            *child_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
            {
                let mut jobs = jobs.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(pos) = jobs.iter().position(|j| j.edge == edge) {
                    jobs.remove(pos);
                }
            }
            let _ = tx.send(CommandResult {
                edge,
                status,
                output,
            });
        });

        match spawn_result {
            Ok(_handle) => {
                self.outstanding += 1;
                Ok(())
            }
            Err(e) => {
                let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(pos) = jobs.iter().position(|j| j.edge == edge) {
                    jobs.remove(pos);
                }
                Err(crate::error::Error::io("spawning build thread", e))
            }
        }
    }

    fn wait_for_command(&mut self) -> Option<CommandResult> {
        loop {
            if self.interrupted() {
                return None;
            }
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => {
                    self.outstanding -= 1;
                    return Some(result);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn active_edges(&self) -> Vec<EdgeId> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|j| j.edge)
            .collect()
    }

    fn abort(&mut self) {
        // Kill whatever is still running. A child whose lock we cannot take is
        // already being reaped, so it will exit on its own.
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs.iter() {
            if let Ok(mut slot) = job.child.try_lock() {
                if let Some(child) = slot.as_mut() {
                    let _ = child.kill();
                }
            }
        }
        drop(jobs);

        // Drain anything that finishes promptly so threads are not left
        // blocked on the channel.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while self.outstanding > 0 && std::time::Instant::now() < deadline {
            match self.rx.try_recv() {
                Ok(_) => self.outstanding -= 1,
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }
}

fn run_one(
    launcher: &CommandLauncher,
    command: &str,
    use_console: bool,
    child_slot: &Arc<Mutex<Option<Child>>>,
) -> std::io::Result<(ExitStatus, String)> {
    let mut cmd = launcher(command);
    cmd.stdin(Stdio::null());

    if use_console {
        // Console edges share our stdio, so there is nothing to capture.
        let child = cmd.spawn()?;
        *child_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        let status = {
            let mut slot = child_slot.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_mut() {
                Some(child) => child.wait()?,
                None => return Ok((ExitStatus::FAILURE, String::new())),
            }
        };
        return Ok((map_status(status), String::new()));
    }

    let (reader, writer) = std::io::pipe()?;
    let writer2 = writer.try_clone()?;
    cmd.stdout(Stdio::from(writer)).stderr(Stdio::from(writer2));
    let child = cmd.spawn()?;
    // Drop the command so our copies of the pipe's write end are closed and
    // the read below can see EOF.
    drop(cmd);
    *child_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    let mut buf = Vec::new();
    let mut reader = reader;
    reader.read_to_end(&mut buf)?;
    drop(reader);

    let status = {
        let mut slot = child_slot.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(child) => child.wait()?,
            None => return Ok((ExitStatus::FAILURE, String::from_utf8_lossy(&buf).into_owned())),
        }
    };

    Ok((
        map_status(status),
        String::from_utf8_lossy(&buf).into_owned(),
    ))
}

fn map_status(status: std::process::ExitStatus) -> ExitStatus {
    if let Some(code) = status.code() {
        return ExitStatus(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            // SIGINT/SIGTERM/SIGHUP mean "interrupted"; anything else is
            // reported as 128 + signal, like a shell would.
            const SIGHUP: i32 = 1;
            const SIGINT: i32 = 2;
            const SIGTERM: i32 = 15;
            if signal == SIGINT || signal == SIGTERM || signal == SIGHUP {
                return ExitStatus::INTERRUPTED;
            }
            return ExitStatus(128 + signal);
        }
    }
    ExitStatus::FAILURE
}

/// The default launcher: `/bin/sh -c <command>` on Unix, and the command line
/// itself on Windows (as ninja does, without going through `cmd.exe`).
pub fn default_launcher(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(command);
        c
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let (program, rest) = split_windows_command(command);
        let mut c = Command::new(program);
        if !rest.is_empty() {
            c.raw_arg(rest);
        }
        c
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Split a Windows command line into its program and the rest, respecting
/// double quotes around the program name.
#[cfg(windows)]
fn split_windows_command(command: &str) -> (String, &str) {
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'"' {
        if let Some(end) = command[i + 1..].find('"') {
            let program = command[i + 1..i + 1 + end].to_string();
            return (program, &command[i + 2 + end..]);
        }
    }
    let start = i;
    while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' {
        i += 1;
    }
    (command[start..i].to_string(), &command[i..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::parse::{ManifestParser, ParserOptions};

    fn state_with(command: &str) -> State {
        let disk = MemDisk::new();
        let mut state = State::new();
        {
            let mut p = ManifestParser::new(
                &mut state,
                &disk,
                ParserOptions {
                    quiet: true,
                    ..Default::default()
                },
            );
            let manifest = format!("rule r\n  command = {command}\n\nbuild out: r\n");
            p.parse_text("input", manifest.as_bytes()).unwrap();
        }
        state
    }

    #[test]
    fn runs_a_command_and_captures_output() {
        let state = state_with("echo hello");
        let mut runner = RealCommandRunner::new(1);
        runner.start_command(&state, EdgeId(0)).unwrap();
        let result = runner.wait_for_command().unwrap();
        assert!(result.success());
        assert_eq!(result.output.trim(), "hello");
    }

    #[test]
    fn reports_failure_codes() {
        let state = state_with("exit 3");
        let mut runner = RealCommandRunner::new(1);
        runner.start_command(&state, EdgeId(0)).unwrap();
        let result = runner.wait_for_command().unwrap();
        assert_eq!(result.status, ExitStatus(3));
    }

    #[test]
    fn merges_stdout_and_stderr() {
        let state = state_with("echo out; echo err 1>&2");
        let mut runner = RealCommandRunner::new(1);
        runner.start_command(&state, EdgeId(0)).unwrap();
        let result = runner.wait_for_command().unwrap();
        assert!(result.output.contains("out"));
        assert!(result.output.contains("err"));
    }

    #[test]
    fn capacity_tracks_running_commands() {
        let state = state_with("true");
        let mut runner = RealCommandRunner::new(2);
        assert_eq!(runner.can_run_more(), 2);
        runner.start_command(&state, EdgeId(0)).unwrap();
        assert_eq!(runner.can_run_more(), 1);
        let _ = runner.wait_for_command().unwrap();
        assert_eq!(runner.can_run_more(), 2);
    }

    #[test]
    fn dry_run_runner() {
        let state = state_with("false");
        let mut runner = DryRunCommandRunner::new();
        runner.start_command(&state, EdgeId(0)).unwrap();
        let r = runner.wait_for_command().unwrap();
        assert!(r.success());
        assert!(runner.wait_for_command().is_none());
    }
}
