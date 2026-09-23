//! Shared helpers for the integration tests.
//!
//! Each test binary pulls in the whole module, so not every helper is used by
//! every one of them.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A temporary directory that cleans itself up.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a uniquely named temporary directory.
    pub fn new(label: &str) -> TempDir {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "shuriken-it-{}-{}-{}-{}",
            label,
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write a file inside the directory, creating parents as needed.
    pub fn write(&self, name: &str, contents: &str) {
        let full = self.path.join(name);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(full, contents).expect("write file");
    }

    /// Read a file inside the directory.
    pub fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path.join(name)).expect("read file")
    }

    /// True if the named file exists.
    pub fn exists(&self, name: &str) -> bool {
        self.path.join(name).exists()
    }

    /// Remove a file inside the directory.
    pub fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.path.join(name));
    }

    /// Set a file's mtime to now, like `touch`.
    pub fn touch(&self, name: &str) {
        let full = self.path.join(name);
        let contents = std::fs::read(&full).unwrap_or_default();
        // Sleep briefly so the new mtime is distinguishable, then rewrite.
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(&full, contents).expect("touch file");
    }

    /// An absolute path inside the directory, as a string.
    pub fn join(&self, name: &str) -> String {
        self.path
            .join(name)
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    /// `dir/name` spelled for a ninja manifest: native separators, with `:`
    /// and ` ` escaped as the syntax requires. A Windows absolute path like
    /// `C:\\...` needs this, exactly as it would from any manifest generator.
    pub fn manifest_path(&self, name: &str) -> String {
        self.join(name).replace(':', "$:").replace(' ', "$ ")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A `command =` body that copies `$in` to `$out` on the host.
pub fn copy_cmd() -> &'static str {
    if cfg!(windows) {
        "cmd /c copy /y $in $out"
    } else {
        "cp $in $out"
    }
}

/// The same copy, spelled differently, for tests that need the command line to
/// change without changing what it does.
pub fn copy_cmd_variant() -> &'static str {
    if cfg!(windows) {
        "cmd /c copy /y /b $in $out"
    } else {
        "cp -f $in $out"
    }
}

/// A `command =` body that creates an empty `$out`.
pub fn touch_cmd() -> &'static str {
    if cfg!(windows) {
        "cmd /c type nul > $out"
    } else {
        "touch $out"
    }
}

/// A `command =` body that does nothing at all successfully.
pub fn no_op_cmd() -> &'static str {
    if cfg!(windows) {
        "cmd /c exit 0"
    } else {
        "true"
    }
}

/// A `command =` body that exits with `code` without writing anything.
pub fn exit_cmd(code: i32) -> String {
    if cfg!(windows) {
        format!("cmd /c exit {code}")
    } else {
        format!("exit {code}")
    }
}

/// A `command =` body that prints `text` on stdout.
pub fn echo_cmd(text: &str) -> String {
    if cfg!(windows) {
        format!("cmd /c echo {text}")
    } else {
        format!("echo {text}")
    }
}

/// A `command =` body that prints `text` on stderr and then fails with `code`.
pub fn fail_loudly_cmd(text: &str, code: i32) -> String {
    if cfg!(windows) {
        format!("cmd /c \"echo {text} 1>&2 & exit {code}\"")
    } else {
        format!("echo {text} 1>&2 && exit {code}")
    }
}

/// A `command =` body that writes the contents of `src` into `$out`.
pub fn cat_into_out_cmd(src: &str) -> String {
    if cfg!(windows) {
        format!("cmd /c type {src} > $out")
    } else {
        format!("cat {src} > $out")
    }
}

/// The result of running the `shuriken` binary.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// All output, for convenience in assertions.
    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Run the `shuriken` binary in `dir` with `args`.
pub fn shuriken(dir: &TempDir, args: &[&str]) -> Run {
    let exe = env!("CARGO_BIN_EXE_shuriken");
    let out = std::process::Command::new(exe)
        .args(args)
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .env_remove("NINJA_STATUS")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("run shuriken");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Count the progress lines (one per command) in a build's output.
pub fn commands_run(run: &Run) -> usize {
    run.stdout
        .lines()
        .filter(|l| l.starts_with('[') && l.contains('/'))
        .count()
}
