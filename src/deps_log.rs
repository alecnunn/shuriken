//! The dependency log (`.ninja_deps`): header dependencies discovered while
//! building, stored in ninja's version 4 binary format.
//!
//! The file is a sequence of records that can be appended to during a build
//! and read in one pass at startup:
//!
//! * a **path record** holds a path string, padded to a 4-byte boundary and
//!   followed by the one's complement of its expected index (which detects two
//!   processes writing the same log),
//! * a **deps record** holds an output path id, the output's mtime and the ids
//!   of its inputs.
//!
//! Later records for the same output win, so updates are pure appends.

use std::fs;
use std::io::Write;

use crate::build_log::{LoadResult, LoadStatus, replace_file};
use crate::error::{Error, Result};
use crate::state::{NodeId, State};

const FILE_SIGNATURE: &[u8] = b"# ninjadeps\n";
const CURRENT_VERSION: i32 = 4;
const MAX_RECORD_SIZE: usize = (1 << 19) - 1;

/// The recorded dependencies of one output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deps {
    /// mtime of the output when these dependencies were recorded.
    pub mtime: i64,
    /// The input nodes.
    pub nodes: Vec<NodeId>,
}

/// The deps log.
#[derive(Default)]
pub struct DepsLog {
    /// Nodes in file order; the index is the node's id in this log.
    nodes: Vec<NodeId>,
    /// Dependencies indexed by output node id.
    deps: Vec<Option<Deps>>,
    file: Option<fs::File>,
    /// Where to append; kept across [`DepsLog::close`] so writes reopen it.
    path: Option<String>,
    needs_recompaction: bool,
}

impl std::fmt::Debug for DepsLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DepsLog")
            .field("nodes", &self.nodes.len())
            .field("deps", &self.deps.len())
            .field("needs_recompaction", &self.needs_recompaction)
            .finish()
    }
}

impl DepsLog {
    /// An empty log.
    pub fn new() -> DepsLog {
        DepsLog::default()
    }

    /// Nodes known to the log, in file order.
    pub fn nodes(&self) -> &[NodeId] {
        &self.nodes
    }

    /// True if the log should be rewritten.
    pub fn needs_recompaction(&self) -> bool {
        self.needs_recompaction
    }

    /// The recorded dependencies of `node`, if any.
    pub fn get_deps(&self, state: &State, node: NodeId) -> Option<&Deps> {
        let id = state.node(node).deps_log_id;
        if id < 0 || id as usize >= self.deps.len() {
            return None;
        }
        self.deps[id as usize].as_ref()
    }

    /// The first output whose recorded dependencies include `node`.
    pub fn first_reverse_deps_node(&self, node: NodeId) -> Option<NodeId> {
        for (id, deps) in self.deps.iter().enumerate() {
            let Some(deps) = deps else { continue };
            if deps.nodes.contains(&node) {
                return self.nodes.get(id).copied();
            }
        }
        None
    }

    /// True if `node`'s deps entry is still reachable from the manifest.
    ///
    /// Entries for outputs that have left the build, or whose rule no longer
    /// sets `deps`, are dropped on recompaction.
    pub fn is_deps_entry_live_for(state: &State, node: NodeId) -> bool {
        match state.node(node).in_edge() {
            Some(e) => !state.edge_binding(e, "deps").is_empty(),
            None => false,
        }
    }

    /// Load the log at `path`, creating nodes in `state` as needed.
    pub fn load(&mut self, path: &str, state: &mut State) -> Result<LoadResult> {
        let contents = match fs::read(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadResult {
                    status: LoadStatus::NotFound,
                    warning: None,
                });
            }
            Err(e) => return Err(Error::io(path.to_string(), e)),
        };

        let header_len = FILE_SIGNATURE.len() + 4;
        let valid_header = contents.len() >= header_len && contents.starts_with(FILE_SIGNATURE);
        let version = if valid_header {
            i32::from_le_bytes(
                contents[FILE_SIGNATURE.len()..header_len]
                    .try_into()
                    .unwrap(),
            )
        } else {
            0
        };

        if !valid_header || version != CURRENT_VERSION {
            let msg = if version == 1 {
                "deps log version change; rebuilding"
            } else {
                "bad deps log signature or version; starting over"
            };
            let _ = fs::remove_file(path);
            // An empty deps log just means the outputs get rebuilt.
            return Ok(LoadResult {
                status: LoadStatus::Success,
                warning: Some(msg.to_string()),
            });
        }

        let mut offset = header_len;
        let mut pos = header_len;
        let mut read_failed = false;
        let mut unique_dep_record_count = 0usize;
        let mut total_dep_record_count = 0usize;

        loop {
            if pos + 4 > contents.len() {
                if pos != contents.len() {
                    read_failed = true;
                }
                break;
            }
            let raw = u32::from_le_bytes(contents[pos..pos + 4].try_into().unwrap());
            let is_deps = (raw >> 31) != 0;
            let size = (raw & 0x7FFF_FFFF) as usize;
            let body = pos + 4;
            if size > MAX_RECORD_SIZE || body + size > contents.len() {
                read_failed = true;
                break;
            }
            let buf = &contents[body..body + size];
            pos = body + size;
            offset = pos;

            if is_deps {
                if size % 4 != 0 || size < 12 {
                    read_failed = true;
                    break;
                }
                let out_id = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
                let mtime_lo = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as u64;
                let mtime_hi = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as u64;
                let mtime = ((mtime_hi << 32) | mtime_lo) as i64;
                let deps_count = size / 4 - 3;

                let mut nodes = Vec::with_capacity(deps_count);
                for i in 0..deps_count {
                    let off = 12 + i * 4;
                    let id = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                    match self.nodes.get(id) {
                        Some(n) => nodes.push(*n),
                        None => {
                            read_failed = true;
                            break;
                        }
                    }
                }
                if read_failed {
                    break;
                }

                total_dep_record_count += 1;
                if !self.update_deps(out_id, Deps { mtime, nodes }) {
                    unique_dep_record_count += 1;
                }
            } else {
                if size <= 4 {
                    read_failed = true;
                    break;
                }
                let mut path_size = size - 4;
                // Up to three bytes of padding.
                for _ in 0..3 {
                    if path_size > 0 && buf[path_size - 1] == 0 {
                        path_size -= 1;
                    }
                }
                if path_size == 0 {
                    read_failed = true;
                    break;
                }
                let subpath = match std::str::from_utf8(&buf[..path_size]) {
                    Ok(s) => s,
                    Err(_) => {
                        read_failed = true;
                        break;
                    }
                };
                // slash_bits can be 0 here: either the node is also in the
                // manifest (and already has the right value), or it is an
                // implicit dependency whose spelling never reaches a command.
                let node = state.get_node(subpath, 0);

                let checksum = u32::from_le_bytes(buf[size - 4..size].try_into().unwrap());
                let expected_id = !checksum;
                let id = self.nodes.len() as u32;
                if id != expected_id || state.node(node).deps_log_id >= 0 {
                    read_failed = true;
                    break;
                }
                state.node_mut(node).deps_log_id = id as i32;
                self.nodes.push(node);
            }
        }

        if read_failed {
            // Recover by truncating to the last complete record.
            truncate(path, offset as u64)?;
            return Ok(LoadResult {
                status: LoadStatus::Success,
                warning: Some("premature end of file; recovering".to_string()),
            });
        }

        const MIN_COMPACTION_ENTRY_COUNT: usize = 1000;
        const COMPACTION_RATIO: usize = 3;
        if total_dep_record_count > MIN_COMPACTION_ENTRY_COUNT
            && total_dep_record_count > unique_dep_record_count * COMPACTION_RATIO
        {
            self.needs_recompaction = true;
        }

        Ok(LoadResult {
            status: LoadStatus::Success,
            warning: None,
        })
    }

    /// Returns true if a previous entry was replaced.
    fn update_deps(&mut self, out_id: usize, deps: Deps) -> bool {
        if out_id >= self.deps.len() {
            self.deps.resize(out_id + 1, None);
        }
        self.deps[out_id].replace(deps).is_some()
    }

    /// Prepare to append to the log at `path`.
    pub fn open_for_write(&mut self, path: &str, state: &mut State) -> Result<()> {
        if self.needs_recompaction {
            self.recompact(path, state)?;
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
            file.write_all(FILE_SIGNATURE)
                .and_then(|()| file.write_all(&CURRENT_VERSION.to_le_bytes()))
                .map_err(|e| Error::io(path.clone(), e))?;
        }
        self.file = Some(file);
        Ok(())
    }

    /// Record the dependencies of `node`.
    ///
    /// Nothing is written when the information is unchanged.
    pub fn record_deps(
        &mut self,
        state: &mut State,
        node: NodeId,
        mtime: i64,
        nodes: &[NodeId],
    ) -> Result<()> {
        let mut made_change = false;

        if state.node(node).deps_log_id < 0 {
            self.record_id(state, node)?;
            made_change = true;
        }
        for &n in nodes {
            if state.node(n).deps_log_id < 0 {
                self.record_id(state, n)?;
                made_change = true;
            }
        }

        if !made_change {
            match self.get_deps(state, node) {
                Some(deps) if deps.mtime == mtime && deps.nodes == nodes => {}
                _ => made_change = true,
            }
        }

        if !made_change {
            return Ok(());
        }

        let size = 4 * (1 + 2 + nodes.len());
        if size > MAX_RECORD_SIZE {
            return Err(Error::build(format!(
                "deps record for '{}' is too large",
                state.node(node).path()
            )));
        }

        self.open_for_write_if_needed()?;
        let out_id = state.node(node).deps_log_id as u32;
        let mut buf = Vec::with_capacity(size + 4);
        buf.extend_from_slice(&((size as u32) | 0x8000_0000).to_le_bytes());
        buf.extend_from_slice(&out_id.to_le_bytes());
        buf.extend_from_slice(&((mtime as u64 & 0xffff_ffff) as u32).to_le_bytes());
        buf.extend_from_slice(&(((mtime as u64 >> 32) & 0xffff_ffff) as u32).to_le_bytes());
        for &n in nodes {
            buf.extend_from_slice(&(state.node(n).deps_log_id as u32).to_le_bytes());
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(&buf)
                .map_err(|e| Error::io("writing deps log", e))?;
            file.flush().map_err(|e| Error::io("writing deps log", e))?;
        }

        self.update_deps(
            out_id as usize,
            Deps {
                mtime,
                nodes: nodes.to_vec(),
            },
        );
        Ok(())
    }

    fn record_id(&mut self, state: &mut State, node: NodeId) -> Result<()> {
        let path = state.node(node).path().to_string();
        let path_size = path.len();
        debug_assert!(path_size > 0, "recording an empty path");
        let padding = (4 - path_size % 4) % 4;
        let size = path_size + padding + 4;
        if size > MAX_RECORD_SIZE {
            return Err(Error::build(format!("path '{path}' is too long for the deps log")));
        }

        self.open_for_write_if_needed()?;
        let id = self.nodes.len() as u32;
        let checksum = !id;
        let mut buf = Vec::with_capacity(size + 4);
        buf.extend_from_slice(&(size as u32).to_le_bytes());
        buf.extend_from_slice(path.as_bytes());
        buf.extend(std::iter::repeat_n(0u8, padding));
        buf.extend_from_slice(&checksum.to_le_bytes());
        if let Some(file) = self.file.as_mut() {
            file.write_all(&buf)
                .map_err(|e| Error::io("writing deps log", e))?;
            file.flush().map_err(|e| Error::io("writing deps log", e))?;
        }

        state.node_mut(node).deps_log_id = id as i32;
        self.nodes.push(node);
        Ok(())
    }

    /// Close the log file without forgetting where it lives.
    ///
    /// The file is not created if nothing has been written, matching what ninja
    /// leaves on disk.
    pub fn close(&mut self) -> Result<()> {
        self.file = None;
        Ok(())
    }

    /// Create the log file (with its header) if it does not exist yet.
    pub fn ensure_created(&mut self) -> Result<()> {
        self.open_for_write_if_needed()
    }

    /// Rewrite the log without dead entries, renumbering all ids.
    pub fn recompact(&mut self, path: &str, state: &mut State) -> Result<()> {
        self.close()?;
        let temp_path = format!("{path}.recompact");
        let _ = fs::remove_file(&temp_path);

        let mut new_log = DepsLog::new();
        new_log.open_for_write(&temp_path, state)?;

        // Clear all ids so the new log can assign its own.
        for &n in &self.nodes {
            state.node_mut(n).deps_log_id = -1;
        }

        for old_id in 0..self.deps.len() {
            let Some(deps) = self.deps[old_id].clone() else {
                continue;
            };
            let node = self.nodes[old_id];
            if !DepsLog::is_deps_entry_live_for(state, node) {
                continue;
            }
            new_log.record_deps(state, node, deps.mtime, &deps.nodes)?;
        }

        new_log.ensure_created()?;
        new_log.close()?;
        self.deps = std::mem::take(&mut new_log.deps);
        self.nodes = std::mem::take(&mut new_log.nodes);

        replace_file(&temp_path, path)?;
        self.needs_recompaction = false;
        Ok(())
    }
}

fn truncate(path: &str, size: u64) -> Result<()> {
    let f = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| Error::io(path.to_string(), e))?;
    f.set_len(size).map_err(|e| Error::io(path.to_string(), e))
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
    fn write_read_roundtrip() {
        let tmp = Tmp::new("depslog");
        let path = tmp.path(".ninja_deps");

        let mut state = State::new();
        let out = state.get_node("out.o", 0);
        let a = state.get_node("a.h", 0);
        let b = state.get_node("b.h", 0);

        let mut log = DepsLog::new();
        log.open_for_write(&path, &mut state).unwrap();
        log.record_deps(&mut state, out, 1234, &[a, b]).unwrap();
        log.close().unwrap();

        let mut state2 = State::new();
        let mut log2 = DepsLog::new();
        log2.load(&path, &mut state2).unwrap();
        let out2 = state2.lookup_node("out.o").unwrap();
        let deps = log2.get_deps(&state2, out2).unwrap();
        assert_eq!(deps.mtime, 1234);
        assert_eq!(deps.nodes.len(), 2);
        assert_eq!(state2.node(deps.nodes[0]).path(), "a.h");
        assert_eq!(state2.node(deps.nodes[1]).path(), "b.h");
    }

    #[test]
    fn unchanged_deps_are_not_rewritten() {
        let tmp = Tmp::new("depslog-nochange");
        let path = tmp.path(".ninja_deps");

        let mut state = State::new();
        let out = state.get_node("out.o", 0);
        let a = state.get_node("a.h", 0);

        let mut log = DepsLog::new();
        log.open_for_write(&path, &mut state).unwrap();
        log.record_deps(&mut state, out, 1, &[a]).unwrap();
        let size1 = std::fs::metadata(&path).unwrap().len();
        log.record_deps(&mut state, out, 1, &[a]).unwrap();
        let size2 = std::fs::metadata(&path).unwrap().len();
        assert_eq!(size1, size2);
        log.close().unwrap();
    }

    #[test]
    fn truncated_file_recovers() {
        let tmp = Tmp::new("depslog-trunc");
        let path = tmp.path(".ninja_deps");

        let mut state = State::new();
        let out = state.get_node("out.o", 0);
        let a = state.get_node("a.h", 0);
        let mut log = DepsLog::new();
        log.open_for_write(&path, &mut state).unwrap();
        log.record_deps(&mut state, out, 1, &[a]).unwrap();
        log.close().unwrap();

        // Chop off the last few bytes.
        let full = std::fs::metadata(&path).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full - 3).unwrap();
        drop(f);

        let mut state2 = State::new();
        let mut log2 = DepsLog::new();
        let r = log2.load(&path, &mut state2).unwrap();
        assert_eq!(r.status, LoadStatus::Success);
        assert!(r.warning.unwrap().contains("recovering"));
        assert!(std::fs::metadata(&path).unwrap().len() < full);
    }

    #[test]
    fn bad_signature_starts_over() {
        let tmp = Tmp::new("depslog-bad");
        let path = tmp.path(".ninja_deps");
        std::fs::write(&path, b"not a deps log at all").unwrap();
        let mut state = State::new();
        let mut log = DepsLog::new();
        let r = log.load(&path, &mut state).unwrap();
        assert!(r.warning.unwrap().contains("starting over"));
        assert!(!std::path::Path::new(&path).exists());
    }
}
