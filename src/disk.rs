//! Filesystem access, abstracted so embedders can substitute their own.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use crate::canon::dir_name;
use crate::error::{Error, Result};

/// Filesystem operations the build engine needs.
///
/// Implementations take `&self` so the engine can share one instance between
/// the dependency scanner and the builder; use interior mutability for caches.
pub trait DiskInterface {
    /// Return the mtime of `path` in nanoseconds since the Unix epoch, or 0 if
    /// it does not exist. Errors are reserved for real failures (e.g. EACCES).
    fn stat(&self, path: &str) -> Result<i64>;

    /// Create the directory `path`. Succeeds if it already exists.
    fn make_dir(&self, path: &str) -> Result<()>;

    /// Write `contents` to `path`, creating or truncating it. When
    /// `crlf_on_windows` is set, `\n` is translated to `\r\n` on Windows.
    fn write_file(&self, path: &str, contents: &str, crlf_on_windows: bool) -> Result<()>;

    /// Read `path`. Returns `Ok(None)` when the file does not exist.
    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>>;

    /// Remove `path`, like `rm -f`. Returns true if a file was removed, false
    /// if it did not exist.
    fn remove_file(&self, path: &str) -> Result<bool>;

    /// Create all parent directories of `path`, like `mkdir -p $(dirname path)`.
    fn make_dirs(&self, path: &str) -> Result<()> {
        let dir = dir_name(path);
        if dir.is_empty() {
            return Ok(()); // Reached the root; assume it exists.
        }
        let mtime = self.stat(dir)?;
        if mtime > 0 {
            return Ok(()); // Already there.
        }
        self.make_dirs(dir)?;
        self.make_dir(dir)
    }

    /// Read `path` as UTF-8 text.
    fn read_file_text(&self, path: &str) -> Result<Option<String>> {
        match self.read_file(path)? {
            None => Ok(None),
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Ok(Some(s)),
                Err(_) => Err(Error::io(
                    path.to_string(),
                    std::io::Error::new(ErrorKind::InvalidData, "file is not valid UTF-8"),
                )),
            },
        }
    }
}

/// The real filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDiskInterface;

impl RealDiskInterface {
    /// A handle to the real filesystem.
    pub fn new() -> RealDiskInterface {
        RealDiskInterface
    }
}

impl DiskInterface for RealDiskInterface {
    fn stat(&self, path: &str) -> Result<i64> {
        // ninja uses stat(2), i.e. symlinks are followed.
        match fs::metadata(Path::new(path)) {
            Ok(md) => Ok(mtime_of(&md)),
            Err(e) => match e.kind() {
                ErrorKind::NotFound | ErrorKind::NotADirectory => Ok(0),
                // A path component that is not a directory also means "absent".
                _ if is_not_a_directory(&e) => Ok(0),
                _ => Err(Error::io(format!("stat({path})"), e)),
            },
        }
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        match fs::create_dir(Path::new(path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(Error::io(format!("mkdir({path})"), e)),
        }
    }

    fn write_file(&self, path: &str, contents: &str, crlf_on_windows: bool) -> Result<()> {
        let mut f = fs::File::create(Path::new(path))
            .map_err(|e| Error::io(format!("WriteFile({path})"), e))?;
        let res = if crlf_on_windows && cfg!(windows) {
            let translated = contents.replace('\n', "\r\n");
            f.write_all(translated.as_bytes())
        } else {
            f.write_all(contents.as_bytes())
        };
        res.map_err(|e| Error::io(format!("WriteFile({path})"), e))?;
        Ok(())
    }

    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(Path::new(path)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) if is_not_a_directory(&e) => Ok(None),
            Err(e) => Err(Error::io(path.to_string(), e)),
        }
    }

    fn remove_file(&self, path: &str) -> Result<bool> {
        let p = Path::new(path);
        // Mirror ninja: on Windows, clear the read-only bit and pick the right
        // removal call for directories.
        #[cfg(windows)]
        {
            match fs::symlink_metadata(p) {
                Ok(md) => {
                    let mut perms = md.permissions();
                    if perms.readonly() {
                        perms.set_readonly(false);
                        let _ = fs::set_permissions(p, perms);
                    }
                    if md.is_dir() {
                        return match fs::remove_dir(p) {
                            Ok(()) => Ok(true),
                            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
                            Err(e) => Err(Error::io(format!("remove({path})"), e)),
                        };
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
                Err(_) => {}
            }
        }
        match fs::remove_file(p) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) if is_not_a_directory(&e) => Ok(false),
            Err(e) => {
                // `remove()` on POSIX also removes empty directories.
                if fs::remove_dir(p).is_ok() {
                    return Ok(true);
                }
                Err(Error::io(format!("remove({path})"), e))
            }
        }
    }
}

fn is_not_a_directory(e: &std::io::Error) -> bool {
    // ENOTDIR is only surfaced as a distinct ErrorKind on recent toolchains;
    // fall back to the raw code.
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(20)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

#[cfg(unix)]
fn mtime_of(md: &fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let secs = md.mtime();
    let nsec = md.mtime_nsec();
    // ninja maps an mtime of exactly 0 to 1, because 0 is its "missing" value.
    if secs == 0 && nsec == 0 {
        return 1;
    }
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

#[cfg(not(unix))]
fn mtime_of(md: &fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    match md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    {
        Some(d) => {
            let v = d.as_nanos() as i64;
            if v == 0 { 1 } else { v }
        }
        // Pre-epoch or unavailable: report "exists" with a tiny mtime.
        None => 1,
    }
}

/// An in-memory filesystem, useful for tests and for embedders that generate
/// manifests on the fly.
#[derive(Debug, Default)]
pub struct MemDisk {
    inner: RefCell<MemDiskInner>,
}

#[derive(Debug, Default)]
struct MemDiskInner {
    files: BTreeMap<String, (Vec<u8>, i64)>,
    dirs: BTreeMap<String, i64>,
    clock: i64,
}

impl MemDisk {
    /// An empty filesystem.
    pub fn new() -> MemDisk {
        MemDisk {
            inner: RefCell::new(MemDiskInner {
                clock: 1_000_000_000,
                ..MemDiskInner::default()
            }),
        }
    }

    /// Advance the virtual clock, so files written next look newer.
    pub fn tick(&self) -> i64 {
        let mut inner = self.inner.borrow_mut();
        inner.clock += 1_000_000_000;
        inner.clock
    }

    /// The current virtual time.
    pub fn now(&self) -> i64 {
        self.inner.borrow().clock
    }

    /// Create or overwrite a file with the current virtual mtime.
    pub fn create(&self, path: &str, contents: impl AsRef<[u8]>) {
        let now = self.inner.borrow().clock;
        self.inner
            .borrow_mut()
            .files
            .insert(path.to_string(), (contents.as_ref().to_vec(), now));
    }

    /// True if `path` exists as a file.
    pub fn contains(&self, path: &str) -> bool {
        self.inner.borrow().files.contains_key(path)
    }

    /// The contents of `path`, if it exists.
    pub fn contents(&self, path: &str) -> Option<Vec<u8>> {
        self.inner.borrow().files.get(path).map(|(c, _)| c.clone())
    }

    /// Every file path present, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.inner.borrow().files.keys().cloned().collect()
    }
}

impl DiskInterface for MemDisk {
    fn stat(&self, path: &str) -> Result<i64> {
        let inner = self.inner.borrow();
        if let Some((_, mtime)) = inner.files.get(path) {
            return Ok(*mtime);
        }
        if let Some(mtime) = inner.dirs.get(path) {
            return Ok(*mtime);
        }
        Ok(0)
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        let now = self.inner.borrow().clock;
        self.inner.borrow_mut().dirs.insert(path.to_string(), now);
        Ok(())
    }

    fn write_file(&self, path: &str, contents: &str, _crlf_on_windows: bool) -> Result<()> {
        self.create(path, contents.as_bytes());
        Ok(())
    }

    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.borrow().files.get(path).map(|(c, _)| c.clone()))
    }

    fn remove_file(&self, path: &str) -> Result<bool> {
        Ok(self.inner.borrow_mut().files.remove(path).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_disk_stat_missing_is_zero() {
        let d = RealDiskInterface::new();
        assert_eq!(d.stat("definitely/not/here.txt").unwrap(), 0);
    }

    #[test]
    fn mem_disk_roundtrip() {
        let d = MemDisk::new();
        assert_eq!(d.stat("a").unwrap(), 0);
        d.create("a", b"hello");
        assert!(d.stat("a").unwrap() > 0);
        assert_eq!(d.read_file("a").unwrap().unwrap(), b"hello");
        assert!(d.remove_file("a").unwrap());
        assert!(!d.remove_file("a").unwrap());
    }

    #[test]
    fn mem_disk_make_dirs() {
        let d = MemDisk::new();
        d.make_dirs("a/b/c/file.txt").unwrap();
        assert!(d.stat("a/b/c").unwrap() > 0);
        assert!(d.stat("a/b").unwrap() > 0);
        assert!(d.stat("a").unwrap() > 0);
    }
}
