//! Removing build outputs (`-t clean`, `-t cleandead`).

use std::collections::BTreeSet;

use crate::build_log::BuildLog;
use crate::canon::canonicalize_path;
use crate::disk::DiskInterface;
use crate::dyndep::load_dyndeps;
use crate::error::Result;
use crate::eval::ROOT_SCOPE;
use crate::state::{EdgeId, NodeId, State};

/// What a clean operation did.
#[derive(Debug, Default, Clone)]
pub struct CleanReport {
    /// Paths that were removed (or would be, in dry-run mode), in order.
    pub removed: Vec<String>,
    /// Number of files removed.
    pub count: usize,
    /// Problems encountered; non-empty means the tool should report failure.
    pub errors: Vec<String>,
}

impl CleanReport {
    /// True if every removal succeeded.
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Removes the files a build produced.
pub struct Cleaner<'a> {
    state: &'a mut State,
    disk: &'a dyn DiskInterface,
    dry_run: bool,
    report: CleanReport,
    already_removed: BTreeSet<String>,
    cleaned: BTreeSet<NodeId>,
}

impl<'a> Cleaner<'a> {
    /// A cleaner operating on `state`.
    pub fn new(state: &'a mut State, disk: &'a dyn DiskInterface, dry_run: bool) -> Cleaner<'a> {
        Cleaner {
            state,
            disk,
            dry_run,
            report: CleanReport::default(),
            already_removed: BTreeSet::new(),
            cleaned: BTreeSet::new(),
        }
    }

    fn reset(&mut self) {
        self.report = CleanReport::default();
        self.already_removed.clear();
        self.cleaned.clear();
    }

    fn file_exists(&mut self, path: &str) -> bool {
        match self.disk.stat(path) {
            Ok(mtime) => mtime > 0,
            Err(e) => {
                self.report.errors.push(e.to_string());
                false
            }
        }
    }

    fn remove(&mut self, path: &str) {
        if self.already_removed.contains(path) {
            return;
        }
        self.already_removed.insert(path.to_string());

        if self.dry_run {
            if self.file_exists(path) {
                self.report.count += 1;
                self.report.removed.push(path.to_string());
            }
            return;
        }
        match self.disk.remove_file(path) {
            Ok(true) => {
                self.report.count += 1;
                self.report.removed.push(path.to_string());
            }
            Ok(false) => {}
            Err(e) => self.report.errors.push(e.to_string()),
        }
    }

    fn remove_edge_files(&mut self, edge: EdgeId) {
        let depfile = self.state.edge_depfile(edge);
        if !depfile.is_empty() {
            self.remove(&depfile);
        }
        let rspfile = self.state.edge_rspfile(edge);
        if !rspfile.is_empty() {
            self.remove(&rspfile);
        }
    }

    /// Load dyndep files before they are cleaned away, so the extra outputs
    /// they declare are cleaned too. Errors are ignored: we clean as much of
    /// the graph as we can see.
    fn load_dyndeps(&mut self) {
        for edge in self.state.edge_ids().collect::<Vec<_>>() {
            if let Some(dyndep) = self.state.edge(edge).dyndep() {
                if self.state.node(dyndep).dyndep_pending {
                    let _ = load_dyndeps(self.state, self.disk, dyndep);
                }
            }
        }
    }

    /// Remove the outputs of every edge (`-t clean`).
    ///
    /// Outputs of `generator` rules are kept unless `generator` is true.
    pub fn clean_all(&mut self, generator: bool) -> CleanReport {
        self.reset();
        self.load_dyndeps();
        for edge in self.state.edge_ids().collect::<Vec<_>>() {
            if self.state.edge_is_phony(edge) {
                continue;
            }
            if !generator && self.state.edge_is_generator(edge) {
                continue;
            }
            for output in self.state.edge(edge).outputs().to_vec() {
                let path = self.state.node(output).path().to_string();
                self.remove(&path);
            }
            self.remove_edge_files(edge);
        }
        self.report.clone()
    }

    /// Remove outputs recorded in the build log that the manifest no longer
    /// produces (`-t cleandead`).
    pub fn clean_dead(&mut self, log: &BuildLog) -> CleanReport {
        self.reset();
        self.load_dyndeps();
        let mut paths: Vec<&str> = log.entries().keys().map(|k| &**k).collect();
        paths.sort();
        for path in paths {
            let stale = match self.state.lookup_node(path) {
                None => true,
                Some(n) => {
                    self.state.node(n).in_edge().is_none()
                        && self.state.node(n).out_edges().is_empty()
                }
            };
            if stale {
                let p = path.to_string();
                self.remove(&p);
            }
        }
        self.report.clone()
    }

    fn do_clean_target(&mut self, target: NodeId) {
        if let Some(edge) = self.state.node(target).in_edge() {
            if !self.state.edge_is_phony(edge) {
                let path = self.state.node(target).path().to_string();
                self.remove(&path);
                self.remove_edge_files(edge);
            }
            for input in self.state.edge(edge).inputs().to_vec() {
                if !self.cleaned.contains(&input) {
                    self.do_clean_target(input);
                }
            }
        }
        self.cleaned.insert(target);
    }

    /// Remove the outputs needed to build `targets`, transitively.
    pub fn clean_targets(&mut self, targets: &[String]) -> CleanReport {
        self.reset();
        self.load_dyndeps();
        for target in targets {
            if target.is_empty() {
                self.report
                    .errors
                    .push("failed to canonicalize '': empty path".to_string());
                continue;
            }
            let mut name = target.clone();
            canonicalize_path(&mut name);
            match self.state.lookup_node(&name) {
                Some(node) => self.do_clean_target(node),
                None => self
                    .report
                    .errors
                    .push(format!("unknown target '{name}'")),
            }
        }
        self.report.clone()
    }

    fn do_clean_rule(&mut self, rule_name: &str) {
        for edge in self.state.edge_ids().collect::<Vec<_>>() {
            if self.state.edge_rule_name(edge) != rule_name {
                continue;
            }
            for output in self.state.edge(edge).outputs().to_vec() {
                let path = self.state.node(output).path().to_string();
                self.remove(&path);
                self.remove_edge_files(edge);
            }
        }
    }

    /// Remove the outputs of every edge using one of `rules`.
    pub fn clean_rules(&mut self, rules: &[String]) -> CleanReport {
        self.reset();
        self.load_dyndeps();
        for rule in rules {
            if self.state.scopes.lookup_rule(ROOT_SCOPE, rule).is_none() {
                self.report.errors.push(format!("unknown rule '{rule}'"));
                continue;
            }
            self.do_clean_rule(rule);
        }
        self.report.clone()
    }
}

/// Remove `path`, reporting whether anything was deleted.
pub fn remove_file(disk: &dyn DiskInterface, path: &str) -> Result<bool> {
    disk.remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::parse::{ManifestParser, ParserOptions};

    fn setup(disk: &MemDisk, manifest: &str) -> State {
        let mut state = State::new();
        {
            let mut p = ManifestParser::new(
                &mut state,
                disk,
                ParserOptions {
                    quiet: true,
                    ..Default::default()
                },
            );
            p.parse_text("input", manifest.as_bytes()).unwrap();
        }
        state
    }

    const CAT: &str = "rule cat\n  command = cat $in > $out\n\n";

    #[test]
    fn clean_all_removes_outputs() {
        let disk = MemDisk::new();
        disk.create("in", "x");
        disk.create("out", "x");
        let mut state = setup(&disk, &format!("{CAT}build out: cat in\n"));
        let report = Cleaner::new(&mut state, &disk, false).clean_all(false);
        assert_eq!(report.count, 1);
        assert!(!disk.contains("out"));
        assert!(disk.contains("in"));
    }

    #[test]
    fn clean_all_keeps_generator_outputs() {
        let disk = MemDisk::new();
        disk.create("build.ninja", "x");
        let mut state = setup(
            &disk,
            "rule gen\n  command = gen\n  generator = 1\n\nbuild build.ninja: gen\n",
        );
        let report = Cleaner::new(&mut state, &disk, false).clean_all(false);
        assert_eq!(report.count, 0);
        assert!(disk.contains("build.ninja"));

        let report = Cleaner::new(&mut state, &disk, false).clean_all(true);
        assert_eq!(report.count, 1);
    }

    #[test]
    fn clean_removes_depfiles_and_rspfiles() {
        let disk = MemDisk::new();
        disk.create("out", "x");
        disk.create("out.d", "x");
        disk.create("out.rsp", "x");
        let mut state = setup(
            &disk,
            "rule r\n  command = c\n  depfile = $out.d\n  rspfile = $out.rsp\n  \
             rspfile_content = x\n\nbuild out: r\n",
        );
        let report = Cleaner::new(&mut state, &disk, false).clean_all(false);
        assert_eq!(report.count, 3);
    }

    #[test]
    fn clean_target_is_transitive() {
        let disk = MemDisk::new();
        disk.create("a", "x");
        disk.create("b", "x");
        disk.create("c", "x");
        let mut state = setup(&disk, &format!("{CAT}build b: cat a\nbuild c: cat b\n"));
        let report =
            Cleaner::new(&mut state, &disk, false).clean_targets(&["c".to_string()]);
        assert_eq!(report.count, 2);
        assert!(disk.contains("a"));
        assert!(!disk.contains("b"));
        assert!(!disk.contains("c"));
    }

    #[test]
    fn clean_rule() {
        let disk = MemDisk::new();
        disk.create("x", "x");
        disk.create("y", "y");
        let mut state = setup(
            &disk,
            &format!("{CAT}rule other\n  command = o\n\nbuild x: cat in\nbuild y: other\n"),
        );
        let report = Cleaner::new(&mut state, &disk, false).clean_rules(&["cat".to_string()]);
        assert_eq!(report.count, 1);
        assert!(!disk.contains("x"));
        assert!(disk.contains("y"));
    }

    #[test]
    fn dry_run_keeps_files() {
        let disk = MemDisk::new();
        disk.create("out", "x");
        let mut state = setup(&disk, &format!("{CAT}build out: cat in\n"));
        let report = Cleaner::new(&mut state, &disk, true).clean_all(false);
        assert_eq!(report.count, 1);
        assert!(disk.contains("out"));
    }

    #[test]
    fn clean_dead_removes_unknown_log_entries() {
        let disk = MemDisk::new();
        disk.create("stale", "x");
        disk.create("out", "x");
        let mut state = setup(&disk, &format!("{CAT}build out: cat in\n"));
        let mut log = BuildLog::new();
        log.insert_entry(crate::build_log::LogEntry::new("stale"));
        log.insert_entry(crate::build_log::LogEntry::new("out"));
        let report = Cleaner::new(&mut state, &disk, false).clean_dead(&log);
        assert_eq!(report.removed, vec!["stale".to_string()]);
        assert!(disk.contains("out"));
    }
}
