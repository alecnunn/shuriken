//! The build log (`.ninja_log`): command hashes, timings and output mtimes.
//!
//! The on-disk format is ninja's version 7, so a `.ninja_log` written by
//! ninja can be read here and vice versa.

use std::fs;
use std::io::Write;
use std::sync::Arc;

use crate::disk::DiskInterface;
use crate::error::{Error, Result};
use crate::hash::{FxHashMap, hash_command};
use crate::state::{EdgeId, State};

const FILE_SIGNATURE_PREFIX: &str = "# ninja log v";
const OLDEST_SUPPORTED_VERSION: i32 = 7;
const CURRENT_VERSION: i32 = 7;

/// One recorded command execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// The output path this entry describes. Shared with the index key, so
    /// each path is stored once.
    pub output: Arc<str>,
    /// Hash of the command line (see [`crate::hash::hash_command`]).
    pub command_hash: u64,
    /// Milliseconds from the start of the build to when the command started.
    pub start_time: i32,
    /// Milliseconds from the start of the build to when the command finished.
    pub end_time: i32,
    /// mtime of the output after the command ran, in nanoseconds.
    pub mtime: i64,
}

impl LogEntry {
    /// An entry with only its output set.
    pub fn new(output: impl AsRef<str>) -> LogEntry {
        LogEntry {
            output: Arc::from(output.as_ref()),
            command_hash: 0,
            start_time: 0,
            end_time: 0,
            mtime: 0,
        }
    }

    /// How long the command took, in milliseconds.
    pub fn duration_millis(&self) -> i64 {
        (self.end_time - self.start_time) as i64
    }
}

/// Outcome of loading a log.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LoadStatus {
    /// The log was loaded (possibly with a recoverable warning).
    Success,
    /// There was no log to load.
    NotFound,
}

/// The result of loading a log file.
#[derive(Clone, Debug)]
pub struct LoadResult {
    /// Whether a log was found.
    pub status: LoadStatus,
    /// A non-fatal problem worth telling the user about.
    pub warning: Option<String>,
}

impl LoadResult {
    fn found() -> LoadResult {
        LoadResult {
            status: LoadStatus::Success,
            warning: None,
        }
    }
    fn not_found() -> LoadResult {
        LoadResult {
            status: LoadStatus::NotFound,
            warning: None,
        }
    }
    fn with_warning(status: LoadStatus, warning: impl Into<String>) -> LoadResult {
        LoadResult {
            status,
            warning: Some(warning.into()),
        }
    }
}

/// A log of every command run, keyed by output path.
#[derive(Default)]
pub struct BuildLog {
    entries: FxHashMap<Arc<str>, LogEntry>,
    file: Option<fs::File>,
    /// Where to append. Set once writing is requested; the file itself is
    /// opened lazily on the first write, and reopened after [`BuildLog::close`].
    path: Option<String>,
    needs_recompaction: bool,
}

impl std::fmt::Debug for BuildLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildLog")
            .field("entries", &self.entries.len())
            .field("needs_recompaction", &self.needs_recompaction)
            .finish()
    }
}

impl BuildLog {
    /// An empty log.
    pub fn new() -> BuildLog {
        BuildLog::default()
    }

    /// All entries, keyed by output path.
    pub fn entries(&self) -> &FxHashMap<Arc<str>, LogEntry> {
        &self.entries
    }

    /// The entry for `path`, if the command has been run before.
    pub fn lookup_by_output(&self, path: &str) -> Option<&LogEntry> {
        self.entries.get(path)
    }

    /// True if the log should be rewritten (too many stale entries, or an
    /// older format version).
    pub fn needs_recompaction(&self) -> bool {
        self.needs_recompaction
    }

    /// Load the log at `path`.
    ///
    /// A log written by an unsupported version is deleted and reported as
    /// [`LoadStatus::NotFound`]: a missing log simply forces a rebuild.
    pub fn load(&mut self, path: &str) -> Result<LoadResult> {
        let contents = match fs::read(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LoadResult::not_found()),
            Err(e) => return Err(Error::io(path.to_string(), e)),
        };

        let mut log_version = 0i32;
        let mut unique_entry_count = 0usize;
        let mut total_entry_count = 0usize;
        let mut saw_any_line = false;

        for raw_line in contents.split(|&c| c == b'\n') {
            if raw_line.is_empty() {
                continue;
            }
            let line = String::from_utf8_lossy(raw_line);
            saw_any_line = true;

            if log_version == 0 {
                log_version = match line.strip_prefix(FILE_SIGNATURE_PREFIX) {
                    Some(rest) => rest.trim().parse::<i32>().unwrap_or(0),
                    None => 0,
                };
                let invalid = if log_version < OLDEST_SUPPORTED_VERSION {
                    Some("build log version is too old; starting over")
                } else if log_version > CURRENT_VERSION {
                    Some("build log version is too new; starting over")
                } else {
                    None
                };
                if let Some(msg) = invalid {
                    let _ = fs::remove_file(path);
                    self.entries.clear();
                    return Ok(LoadResult::with_warning(LoadStatus::NotFound, msg));
                }
                continue;
            }

            let trimmed = line.trim_end_matches(['\n', '\r']);
            let mut fields = trimmed.splitn(5, '\t');
            let (Some(start), Some(end), Some(mtime), Some(output), Some(hash)) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                continue; // Malformed line; skip it like ninja does.
            };

            let entry = LogEntry {
                output: Arc::from(output),
                command_hash: u64::from_str_radix(hash.trim(), 16).unwrap_or(0),
                start_time: start.trim().parse().unwrap_or(0),
                end_time: end.trim().parse().unwrap_or(0),
                mtime: mtime.trim().parse().unwrap_or(0),
            };
            total_entry_count += 1;
            if self.entries.insert(Arc::clone(&entry.output), entry).is_none() {
                unique_entry_count += 1;
            }
        }

        if !saw_any_line {
            return Ok(LoadResult::found()); // Empty file.
        }

        // Decide whether to rewrite the log: on a version upgrade, or once it
        // is mostly stale entries.
        const MIN_COMPACTION_ENTRY_COUNT: usize = 100;
        const COMPACTION_RATIO: usize = 3;
        if log_version < CURRENT_VERSION
            || (total_entry_count > MIN_COMPACTION_ENTRY_COUNT
                && total_entry_count > unique_entry_count * COMPACTION_RATIO)
        {
            self.needs_recompaction = true;
        }

        Ok(LoadResult::found())
    }

    /// Prepare to append to the log at `path`, recompacting first if needed.
    ///
    /// `is_path_dead` reports whether an output is no longer part of the build.
    pub fn open_for_write(
        &mut self,
        path: &str,
        is_path_dead: &dyn Fn(&str) -> bool,
    ) -> Result<()> {
        if self.needs_recompaction {
            self.recompact(path, is_path_dead)?;
        }
        debug_assert!(self.file.is_none());
        self.path = Some(path.to_string());
        Ok(())
    }

    fn open_for_write_if_needed(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| Error::io(path.clone(), e))?;
        let len = file
            .metadata()
            .map_err(|e| Error::io(path.clone(), e))?
            .len();
        if len == 0 {
            write!(file, "{FILE_SIGNATURE_PREFIX}{CURRENT_VERSION}\n")
                .map_err(|e| Error::io(path.clone(), e))?;
        }
        self.file = Some(file);
        Ok(())
    }

    /// Record that `edge` ran, updating every one of its outputs.
    pub fn record_command(
        &mut self,
        state: &State,
        edge: EdgeId,
        start_time: i32,
        end_time: i32,
        mtime: i64,
    ) -> Result<()> {
        let command = state.edge_command_for_hash(edge);
        let command_hash = hash_command(&command);
        let outputs: Vec<Arc<str>> = state
            .edge(edge)
            .outputs()
            .iter()
            .map(|&o| state.node(o).path_shared())
            .collect();

        for path in outputs {
            let entry = self
                .entries
                .entry(Arc::clone(&path))
                .or_insert_with(|| LogEntry {
                    output: path,
                    command_hash: 0,
                    start_time: 0,
                    end_time: 0,
                    mtime: 0,
                });
            entry.command_hash = command_hash;
            entry.start_time = start_time;
            entry.end_time = end_time;
            entry.mtime = mtime;
            let line = format_entry(entry);

            self.open_for_write_if_needed()?;
            if let Some(file) = self.file.as_mut() {
                file.write_all(line.as_bytes())
                    .map_err(|e| Error::io("writing build log", e))?;
                file.flush().map_err(|e| Error::io("writing build log", e))?;
            }
        }
        Ok(())
    }

    /// Insert or replace an entry without writing to disk (used by tools).
    pub fn insert_entry(&mut self, entry: LogEntry) {
        self.entries.insert(Arc::clone(&entry.output), entry);
    }

    /// Close the log file without discarding where it lives, so that a later
    /// [`BuildLog::record_command`] reopens and appends to it.
    ///
    /// The file is not created if nothing has been written yet, matching what
    /// ninja leaves on disk.
    pub fn close(&mut self) -> Result<()> {
        self.file = None;
        Ok(())
    }

    /// Create the log file (with its header) if it does not exist yet.
    pub fn ensure_created(&mut self) -> Result<()> {
        self.open_for_write_if_needed()
    }

    /// Rewrite the log, dropping entries for outputs that are no longer part
    /// of the build.
    pub fn recompact(&mut self, path: &str, is_path_dead: &dyn Fn(&str) -> bool) -> Result<()> {
        self.close()?;
        let temp_path = format!("{path}.recompact");
        {
            let mut f =
                fs::File::create(&temp_path).map_err(|e| Error::io(temp_path.clone(), e))?;
            write!(f, "{FILE_SIGNATURE_PREFIX}{CURRENT_VERSION}\n")
                .map_err(|e| Error::io(temp_path.clone(), e))?;

            let mut dead: Vec<Arc<str>> = Vec::new();
            let mut keys: Vec<Arc<str>> = self.entries.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if is_path_dead(&key) {
                    dead.push(key);
                    continue;
                }
                let entry = &self.entries[&key];
                f.write_all(format_entry(entry).as_bytes())
                    .map_err(|e| Error::io(temp_path.clone(), e))?;
            }
            for d in dead {
                self.entries.remove(&d);
            }
        }

        replace_file(&temp_path, path)?;
        self.needs_recompaction = false;
        Ok(())
    }

    /// Re-stat the outputs in the log and rewrite their recorded mtimes.
    ///
    /// When `outputs` is non-empty, only those paths are updated.
    pub fn restat(
        &mut self,
        path: &str,
        disk: &dyn DiskInterface,
        outputs: &[String],
    ) -> Result<()> {
        self.close()?;
        let temp_path = format!("{path}.restat");
        {
            let mut f =
                fs::File::create(&temp_path).map_err(|e| Error::io(temp_path.clone(), e))?;
            write!(f, "{FILE_SIGNATURE_PREFIX}{CURRENT_VERSION}\n")
                .map_err(|e| Error::io(temp_path.clone(), e))?;

            let mut keys: Vec<Arc<str>> = self.entries.keys().cloned().collect();
            keys.sort();
            for key in keys {
                let skip = !outputs.is_empty() && !outputs.iter().any(|o| o.as_str() == &*key);
                if !skip {
                    let mtime = disk.stat(&key)?;
                    if let Some(e) = self.entries.get_mut(&key) {
                        e.mtime = mtime;
                    }
                }
                let entry = &self.entries[&key];
                f.write_all(format_entry(entry).as_bytes())
                    .map_err(|e| Error::io(temp_path.clone(), e))?;
            }
        }
        replace_file(&temp_path, path)?;
        Ok(())
    }
}

fn format_entry(entry: &LogEntry) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{:x}\n",
        entry.start_time, entry.end_time, entry.mtime, entry.output, entry.command_hash
    )
}

pub(crate) fn replace_file(from: &str, to: &str) -> Result<()> {
    match fs::remove_file(to) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(to.to_string(), e)),
    }
    fs::rename(from, to).map_err(|e| Error::io(to.to_string(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(name: &str) -> Tmp {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "shuriken-test-{}-{}-{}",
                name,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_then_read() {
        let tmp = Tmp::new("buildlog");
        let path = tmp.path(".ninja_log");

        let mut log = BuildLog::new();
        log.open_for_write(&path, &|_| false).unwrap();
        log.insert_entry(LogEntry {
            output: "out".into(),
            command_hash: 0xdeadbeef,
            start_time: 1,
            end_time: 2,
            mtime: 12345,
        });
        // Force a write of the entry by recompacting.
        log.recompact(&path, &|_| false).unwrap();
        log.close().unwrap();

        let mut log2 = BuildLog::new();
        let r = log2.load(&path).unwrap();
        assert_eq!(r.status, LoadStatus::Success);
        let e = log2.lookup_by_output("out").unwrap();
        assert_eq!(e.command_hash, 0xdeadbeef);
        assert_eq!(e.start_time, 1);
        assert_eq!(e.end_time, 2);
        assert_eq!(e.mtime, 12345);
    }

    #[test]
    fn missing_log() {
        let tmp = Tmp::new("missing");
        let mut log = BuildLog::new();
        let r = log.load(&tmp.path("nope")).unwrap();
        assert_eq!(r.status, LoadStatus::NotFound);
    }

    #[test]
    fn old_version_is_discarded() {
        let tmp = Tmp::new("oldver");
        let path = tmp.path(".ninja_log");
        std::fs::write(&path, "# ninja log v5\n1\t2\t3\tout\tabc\n").unwrap();
        let mut log = BuildLog::new();
        let r = log.load(&path).unwrap();
        assert_eq!(r.status, LoadStatus::NotFound);
        assert!(r.warning.unwrap().contains("too old"));
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn duplicate_entries_are_deduplicated() {
        let tmp = Tmp::new("dup");
        let path = tmp.path(".ninja_log");
        std::fs::write(
            &path,
            "# ninja log v7\n1\t2\t3\tout\tabc\n4\t5\t6\tout\tdef\n",
        )
        .unwrap();
        let mut log = BuildLog::new();
        log.load(&path).unwrap();
        assert_eq!(log.entries().len(), 1);
        // The last entry wins.
        assert_eq!(log.lookup_by_output("out").unwrap().command_hash, 0xdef);
    }

    #[test]
    fn recompact_drops_dead_paths() {
        let tmp = Tmp::new("recompact");
        let path = tmp.path(".ninja_log");
        let mut log = BuildLog::new();
        log.insert_entry(LogEntry::new("alive"));
        log.insert_entry(LogEntry::new("dead"));
        log.recompact(&path, &|p| p == "dead").unwrap();
        log.close().unwrap();

        let mut log2 = BuildLog::new();
        log2.load(&path).unwrap();
        assert!(log2.lookup_by_output("alive").is_some());
        assert!(log2.lookup_by_output("dead").is_none());
    }
}
