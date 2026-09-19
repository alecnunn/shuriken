//! Progress reporting.
//!
//! [`Status`] is the interface the builder reports through; [`ConsoleStatus`]
//! is the terminal implementation, reproducing ninja's single-line progress
//! display, its `NINJA_STATUS` format strings and its failure output.

use std::io::{IsTerminal, Write};

use crate::escape::{elide_middle_in_place, strip_ansi_escape_codes};
use crate::exec::ExitStatus;
use crate::state::{EdgeId, State};

/// How much the build should print.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Verbosity {
    /// Print nothing (used by tests).
    Quiet,
    /// Print command output but no progress line.
    NoStatusUpdate,
    /// Print progress and command output.
    #[default]
    Normal,
    /// Print full command lines instead of descriptions.
    Verbose,
}

/// Receives progress information during a build.
///
/// Every method has a default no-op implementation, so an embedder only needs
/// to implement the parts it cares about.
pub trait Status {
    /// An edge was added to the build plan.
    fn edge_added_to_plan(&mut self, _state: &State, _edge: EdgeId) {}
    /// An edge was removed from the plan (a `restat` made it unnecessary).
    fn edge_removed_from_plan(&mut self, _state: &State, _edge: EdgeId) {}
    /// The build is about to start running commands.
    fn build_started(&mut self) {}
    /// The build has finished.
    fn build_finished(&mut self) {}
    /// An edge's command has started.
    fn build_edge_started(&mut self, _state: &State, _edge: EdgeId, _start_time_millis: i64) {}
    /// An edge's command has finished.
    fn build_edge_finished(
        &mut self,
        _state: &State,
        _edge: EdgeId,
        _start_time_millis: i64,
        _end_time_millis: i64,
        _status: ExitStatus,
        _output: &str,
    ) {
    }
    /// `-d explain` diagnostics for the edge that is about to be reported.
    fn explanations(&mut self, _lines: &[String]) {}
    /// An informational message.
    fn info(&mut self, _message: &str) {}
    /// A warning.
    fn warning(&mut self, _message: &str) {}
    /// An error.
    fn error(&mut self, _message: &str) {}
}

/// A [`Status`] that discards everything.
#[derive(Default, Debug)]
pub struct NullStatus;

impl Status for NullStatus {}

/// A [`Status`] that records messages, for tests and for embedders that want to
/// render progress themselves.
#[derive(Default, Debug)]
pub struct RecordingStatus {
    /// Descriptions (or commands) of started edges, in order.
    pub started: Vec<String>,
    /// Descriptions (or commands) of finished edges, in order.
    pub finished: Vec<String>,
    /// Captured command output.
    pub output: Vec<String>,
    /// Informational messages.
    pub infos: Vec<String>,
    /// Warnings.
    pub warnings: Vec<String>,
    /// Errors.
    pub errors: Vec<String>,
}

impl Status for RecordingStatus {
    fn build_edge_started(&mut self, state: &State, edge: EdgeId, _start: i64) {
        self.started.push(state.edge_command(edge));
    }

    fn build_edge_finished(
        &mut self,
        state: &State,
        edge: EdgeId,
        _start: i64,
        _end: i64,
        _status: ExitStatus,
        output: &str,
    ) {
        self.finished.push(state.edge_command(edge));
        if !output.is_empty() {
            self.output.push(output.to_string());
        }
    }

    fn info(&mut self, message: &str) {
        self.infos.push(message.to_string());
    }

    fn warning(&mut self, message: &str) {
        self.warnings.push(message.to_string());
    }

    fn error(&mut self, message: &str) {
        self.errors.push(message.to_string());
    }
}

/// Returns the terminal width, if it can be determined.
pub type WidthProvider = dyn Fn() -> Option<usize> + Send + Sync;

/// Prints a single status line that is overwritten in place, like ninja's
/// `LinePrinter`.
pub struct LinePrinter {
    smart_terminal: bool,
    supports_color: bool,
    have_blank_line: bool,
    console_locked: bool,
    line_buffer: String,
    line_full: bool,
    output_buffer: String,
    width_provider: Option<Box<WidthProvider>>,
}

impl Default for LinePrinter {
    fn default() -> Self {
        Self::new()
    }
}

impl LinePrinter {
    /// A printer configured from the environment (`TERM`, `CLICOLOR_FORCE`) and
    /// whether stdout is a terminal.
    pub fn new() -> LinePrinter {
        let term = std::env::var("TERM").unwrap_or_default();
        let smart_terminal = std::io::stdout().is_terminal() && term != "dumb" && !term.is_empty();
        let mut supports_color = smart_terminal;
        if !supports_color {
            supports_color = match std::env::var("CLICOLOR_FORCE") {
                Ok(v) => v != "0",
                Err(_) => false,
            };
        }
        LinePrinter {
            smart_terminal,
            supports_color,
            have_blank_line: true,
            console_locked: false,
            line_buffer: String::new(),
            line_full: false,
            output_buffer: String::new(),
            width_provider: None,
        }
    }

    /// Whether the output is an interactive terminal.
    pub fn is_smart_terminal(&self) -> bool {
        self.smart_terminal
    }

    /// Force the "smart terminal" behaviour on or off.
    pub fn set_smart_terminal(&mut self, smart: bool) {
        self.smart_terminal = smart;
    }

    /// Whether ANSI colour sequences may be emitted.
    pub fn supports_color(&self) -> bool {
        self.supports_color
    }

    /// Provide a way to query the terminal width, used to elide long lines.
    ///
    /// Without a provider, `COLUMNS` is consulted.
    pub fn set_width_provider(&mut self, provider: Box<WidthProvider>) {
        self.width_provider = Some(provider);
    }

    fn width(&self) -> Option<usize> {
        if let Some(p) = &self.width_provider {
            if let Some(w) = p() {
                return Some(w);
            }
        }
        std::env::var("COLUMNS").ok()?.parse().ok()
    }

    /// Print `text` as the status line, eliding it to the terminal width unless
    /// `full` is set.
    pub fn print(&mut self, text: &str, full: bool) {
        if self.console_locked {
            self.line_buffer = text.to_string();
            self.line_full = full;
            return;
        }

        let mut out = std::io::stdout().lock();
        if self.smart_terminal {
            let _ = write!(out, "\r");
        }

        if self.smart_terminal && !full {
            let mut to_print = text.to_string();
            if let Some(w) = self.width() {
                elide_middle_in_place(&mut to_print, w);
            }
            let _ = write!(out, "{to_print}\x1b[K");
            let _ = out.flush();
            self.have_blank_line = false;
        } else {
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
        }
    }

    /// Print `text` starting on a fresh line, preserving the status line.
    pub fn print_on_new_line(&mut self, text: &str) {
        if self.console_locked && !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.output_buffer.push_str(&line);
            self.output_buffer.push('\n');
        }
        if !self.have_blank_line {
            self.write_or_buffer("\n");
        }
        if !text.is_empty() {
            self.write_or_buffer(text);
        }
        self.have_blank_line = text.is_empty() || text.ends_with('\n');
    }

    fn write_or_buffer(&mut self, text: &str) {
        if self.console_locked {
            self.output_buffer.push_str(text);
        } else {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
    }

    /// While the console is locked (a `console` pool edge is running), buffer
    /// our own output so it does not interleave with the child's.
    pub fn set_console_locked(&mut self, locked: bool) {
        if locked == self.console_locked {
            return;
        }
        if locked {
            self.print_on_new_line("");
        }
        self.console_locked = locked;
        if !locked {
            let buffered = std::mem::take(&mut self.output_buffer);
            self.print_on_new_line(&buffered);
            if !self.line_buffer.is_empty() {
                let line = std::mem::take(&mut self.line_buffer);
                let full = self.line_full;
                self.print(&line, full);
            }
        }
    }
}

/// Tracks a moving average of the completion rate over the last N edges.
#[derive(Debug)]
struct RateInfo {
    rate: f64,
    last_update: i64,
    updates: std::collections::VecDeque<i64>,
    window: usize,
}

impl RateInfo {
    fn new(window: usize) -> RateInfo {
        RateInfo {
            rate: -1.0,
            last_update: -1,
            updates: std::collections::VecDeque::new(),
            window: window.max(1),
        }
    }

    fn update(&mut self, finished_edges: i64, time_millis: i64) {
        if finished_edges == self.last_update {
            return;
        }
        self.last_update = finished_edges;
        self.updates.push_back(time_millis);
        if self.updates.len() > self.window {
            self.updates.pop_front();
        }
        if self.updates.len() >= 2 {
            let span = self.updates.back().unwrap() - self.updates.front().unwrap();
            if span > 0 {
                self.rate = (self.updates.len() - 1) as f64 / (span as f64 / 1000.0);
            }
        }
    }
}

/// The terminal progress display.
pub struct ConsoleStatus {
    printer: LinePrinter,
    verbosity: Verbosity,
    progress_status_format: String,
    started_edges: i64,
    finished_edges: i64,
    total_edges: i64,
    running_edges: i64,
    time_millis: i64,
    cpu_time_millis: i64,
    current_rate: RateInfo,
    /// Tracking for ETA prediction, as in ninja's status printer.
    eta_predictable_edges_total: i64,
    eta_predictable_edges_remaining: i64,
    eta_predictable_cpu_time_total_millis: i64,
    eta_predictable_cpu_time_remaining_millis: i64,
    eta_unpredictable_edges_remaining: i64,
    time_predicted_percentage: f64,
    pending_explanations: Vec<String>,
}

impl ConsoleStatus {
    /// A status printer for the given verbosity and `-j` value.
    pub fn new(verbosity: Verbosity, parallelism: usize) -> ConsoleStatus {
        let mut printer = LinePrinter::new();
        // Anything but the default verbosity prints plain lines.
        if verbosity != Verbosity::Normal {
            printer.set_smart_terminal(false);
        }
        let progress_status_format =
            std::env::var("NINJA_STATUS").unwrap_or_else(|_| "[%f/%t] ".to_string());
        ConsoleStatus {
            printer,
            verbosity,
            progress_status_format,
            started_edges: 0,
            finished_edges: 0,
            total_edges: 0,
            running_edges: 0,
            time_millis: 0,
            cpu_time_millis: 0,
            current_rate: RateInfo::new(parallelism),
            eta_predictable_edges_total: 0,
            eta_predictable_edges_remaining: 0,
            eta_predictable_cpu_time_total_millis: 0,
            eta_predictable_cpu_time_remaining_millis: 0,
            eta_unpredictable_edges_remaining: 0,
            time_predicted_percentage: 0.0,
            pending_explanations: Vec::new(),
        }
    }

    /// Mutable access to the underlying printer (e.g. to install a terminal
    /// width provider).
    pub fn printer_mut(&mut self) -> &mut LinePrinter {
        &mut self.printer
    }

    /// Total number of edges with commands in the plan.
    pub fn total_edges(&self) -> i64 {
        self.total_edges
    }

    /// Format a progress prefix from a `NINJA_STATUS`-style format string.
    pub fn format_progress_status(&mut self, format: &str, time_millis: i64) -> String {
        let mut out = String::new();
        let mut chars = format.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let Some(spec) = chars.next() else { break };
            match spec {
                '%' => out.push('%'),
                's' => out.push_str(&self.started_edges.to_string()),
                't' => out.push_str(&self.total_edges.to_string()),
                'r' => out.push_str(&self.running_edges.to_string()),
                'u' => out.push_str(&(self.total_edges - self.started_edges).to_string()),
                'f' => out.push_str(&self.finished_edges.to_string()),
                'o' => {
                    let rate = if self.time_millis > 0 {
                        self.finished_edges as f64 / (self.time_millis as f64 / 1e3)
                    } else {
                        -1.0
                    };
                    out.push_str(&format_rate(rate));
                }
                'c' => {
                    self.current_rate.update(self.finished_edges, self.time_millis);
                    out.push_str(&format_rate(self.current_rate.rate));
                }
                'p' => {
                    let percent = if self.finished_edges != 0 && self.total_edges != 0 {
                        (100 * self.finished_edges) / self.total_edges
                    } else {
                        0
                    };
                    out.push_str(&format!("{percent:3}%"));
                }
                'e' | 'w' | 'E' | 'W' => {
                    let elapsed_sec = time_millis as f64 / 1e3;
                    let eta_sec = if self.time_predicted_percentage != 0.0 {
                        let total = time_millis as f64 / self.time_predicted_percentage;
                        (total - time_millis as f64) / 1e3
                    } else {
                        -1.0
                    };
                    let print_with_hours = elapsed_sec >= 3600.0 || eta_sec >= 3600.0;
                    let sec = match spec {
                        'e' | 'w' => elapsed_sec,
                        _ => eta_sec,
                    };
                    if sec < 0.0 {
                        out.push('?');
                    } else {
                        match spec {
                            'e' | 'E' => out.push_str(&format!("{sec:.3}")),
                            _ => {
                                let t = sec as i64;
                                if print_with_hours {
                                    out.push_str(&format!(
                                        "{}:{:02}:{:02}",
                                        t / 3600,
                                        (t % 3600) / 60,
                                        t % 60
                                    ));
                                } else {
                                    out.push_str(&format!("{:02}:{:02}", t / 60, t % 60));
                                }
                            }
                        }
                    }
                }
                'P' => {
                    let p = (100.0 * self.time_predicted_percentage) as i64;
                    out.push_str(&format!("{p:3}%"));
                }
                other => {
                    // ninja treats this as fatal; keep going but make it visible.
                    out.push('%');
                    out.push(other);
                }
            }
        }
        out
    }

    fn recalculate_progress_prediction(&mut self) {
        self.time_predicted_percentage = 0.0;

        // Previous timings may be wildly wrong (think a cache that used to hit
        // and now misses), so only trust them while they roughly match what we
        // are seeing.
        let mut use_previous_times = self.eta_predictable_edges_remaining > 0
            && self.eta_predictable_cpu_time_remaining_millis > 0;

        if use_previous_times
            && self.total_edges > 0
            && self.finished_edges > 0
            && self.time_millis >= 15_000
            && (self.finished_edges as f64 / self.total_edges as f64) >= 0.05
        {
            let actual_average = self.cpu_time_millis as f64 / self.finished_edges as f64;
            let previous_average = self.eta_predictable_cpu_time_total_millis as f64
                / self.eta_predictable_edges_total.max(1) as f64;
            let ratio = actual_average.max(previous_average) / actual_average.min(previous_average);
            use_previous_times = ratio < 10.0;
        }

        let mut edges_with_known_runtime = self.finished_edges;
        if use_previous_times {
            edges_with_known_runtime += self.eta_predictable_edges_remaining;
        }
        if edges_with_known_runtime == 0 {
            return;
        }

        let edges_with_unknown_runtime = if use_previous_times {
            self.eta_unpredictable_edges_remaining
        } else {
            self.total_edges - self.finished_edges
        };

        let mut known_total = self.cpu_time_millis;
        if use_previous_times {
            known_total += self.eta_predictable_cpu_time_remaining_millis;
        }
        let average = known_total as f64 / edges_with_known_runtime as f64;
        let mut remaining = average * edges_with_unknown_runtime as f64;
        if use_previous_times {
            remaining += self.eta_predictable_cpu_time_remaining_millis as f64;
        }
        let total = self.cpu_time_millis as f64 + remaining;
        if total == 0.0 {
            return;
        }
        self.time_predicted_percentage = self.cpu_time_millis as f64 / total;
    }

    fn print_status(&mut self, state: &State, edge: EdgeId, time_millis: i64) {
        if !self.pending_explanations.is_empty() {
            self.printer.print_on_new_line("");
            let lines = std::mem::take(&mut self.pending_explanations);
            let mut err = std::io::stderr().lock();
            for line in lines {
                let _ = writeln!(err, "shuriken explain: {line}");
            }
        }

        if self.verbosity == Verbosity::Quiet || self.verbosity == Verbosity::NoStatusUpdate {
            return;
        }

        self.recalculate_progress_prediction();
        let force_full_command = self.verbosity == Verbosity::Verbose;

        let mut to_print = state.edge_binding(edge, "description");
        if to_print.is_empty() || force_full_command {
            to_print = state.edge_command(edge);
        }

        let format = self.progress_status_format.clone();
        let prefix = self.format_progress_status(&format, time_millis);
        self.printer
            .print(&format!("{prefix}{to_print}"), force_full_command);
    }
}

fn format_rate(rate: f64) -> String {
    if rate < 0.0 {
        "?".to_string()
    } else {
        format!("{rate:.1}")
    }
}

impl Status for ConsoleStatus {
    fn edge_added_to_plan(&mut self, state: &State, edge: EdgeId) {
        self.total_edges += 1;
        let prev = state.edge(edge).prev_elapsed_time_millis();
        if prev != -1 {
            self.eta_predictable_edges_total += 1;
            self.eta_predictable_edges_remaining += 1;
            self.eta_predictable_cpu_time_total_millis += prev;
            self.eta_predictable_cpu_time_remaining_millis += prev;
        } else {
            self.eta_unpredictable_edges_remaining += 1;
        }
    }

    fn edge_removed_from_plan(&mut self, state: &State, edge: EdgeId) {
        self.total_edges -= 1;
        let prev = state.edge(edge).prev_elapsed_time_millis();
        if prev != -1 {
            self.eta_predictable_edges_total -= 1;
            self.eta_predictable_edges_remaining -= 1;
            self.eta_predictable_cpu_time_total_millis -= prev;
            self.eta_predictable_cpu_time_remaining_millis -= prev;
        } else {
            self.eta_unpredictable_edges_remaining -= 1;
        }
    }

    fn build_started(&mut self) {
        self.started_edges = 0;
        self.finished_edges = 0;
        self.running_edges = 0;
    }

    fn build_finished(&mut self) {
        self.printer.set_console_locked(false);
        self.printer.print_on_new_line("");
    }

    fn build_edge_started(&mut self, state: &State, edge: EdgeId, start_time_millis: i64) {
        self.started_edges += 1;
        self.running_edges += 1;
        self.time_millis = start_time_millis;

        let use_console = state.edge_use_console(edge);
        if use_console || self.printer.is_smart_terminal() {
            self.print_status(state, edge, start_time_millis);
        }
        if use_console {
            self.printer.set_console_locked(true);
        }
    }

    fn build_edge_finished(
        &mut self,
        state: &State,
        edge: EdgeId,
        start_time_millis: i64,
        end_time_millis: i64,
        status: ExitStatus,
        output: &str,
    ) {
        self.time_millis = end_time_millis;
        self.finished_edges += 1;

        let elapsed = end_time_millis - start_time_millis;
        self.cpu_time_millis += elapsed;

        let prev = state.edge(edge).prev_elapsed_time_millis();
        if prev != -1 {
            self.eta_predictable_edges_remaining -= 1;
            self.eta_predictable_cpu_time_remaining_millis -= prev;
        } else {
            self.eta_unpredictable_edges_remaining -= 1;
        }

        let use_console = state.edge_use_console(edge);
        if use_console {
            self.printer.set_console_locked(false);
        }

        if self.verbosity == Verbosity::Quiet {
            return;
        }

        if !use_console {
            self.print_status(state, edge, end_time_millis);
        }
        self.running_edges -= 1;

        // Show which command failed, and with what code, before its output.
        if !status.success() {
            let mut outputs = String::new();
            for &o in state.edge(edge).outputs() {
                outputs.push_str(state.node(o).path());
                outputs.push(' ');
            }
            let failed = format!("FAILED: [code={}] ", status.code());
            if self.printer.supports_color() {
                self.printer
                    .print_on_new_line(&format!("\x1b[31m{failed}\x1b[0m{outputs}\n"));
            } else {
                self.printer
                    .print_on_new_line(&format!("{failed}{outputs}\n"));
            }
            let command = state.edge_command(edge);
            self.printer.print_on_new_line(&format!("{command}\n"));
        }

        if !output.is_empty() {
            // Subprocesses see a pipe, so tools that colour their output when
            // told to may emit escape codes; strip them if we are not writing
            // to a terminal.
            if self.printer.supports_color() || !output.contains('\x1b') {
                self.printer.print_on_new_line(output);
            } else {
                self.printer
                    .print_on_new_line(&strip_ansi_escape_codes(output));
            }
        }
    }

    fn explanations(&mut self, lines: &[String]) {
        // Replace rather than append: these are the explanations for the edge
        // whose status comes next, and the same edge reports them both when it
        // starts and when it finishes.
        self.pending_explanations = lines.to_vec();
    }

    fn info(&mut self, message: &str) {
        self.printer.print_on_new_line("");
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "shuriken: {message}");
        let _ = out.flush();
    }

    fn warning(&mut self, message: &str) {
        self.printer.print_on_new_line("");
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "shuriken: warning: {message}");
    }

    fn error(&mut self, message: &str) {
        self.printer.print_on_new_line("");
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "shuriken: error: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_format() {
        let mut s = ConsoleStatus::new(Verbosity::Quiet, 4);
        s.total_edges = 10;
        s.finished_edges = 3;
        s.started_edges = 5;
        s.running_edges = 2;
        assert_eq!(s.format_progress_status("[%f/%t] ", 0), "[3/10] ");
        assert_eq!(s.format_progress_status("%s %r %u %p", 0), "5 2 5  30%");
        assert_eq!(s.format_progress_status("100%%", 0), "100%");
    }

    #[test]
    fn rate_info() {
        let mut r = RateInfo::new(4);
        r.update(1, 1000);
        r.update(2, 2000);
        assert!((r.rate - 1.0).abs() < 1e-9);
    }
}
