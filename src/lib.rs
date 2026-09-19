//! `shuriken` is a ninja-compatible build engine that can be embedded in
//! other programs or used through its own command-line tool.
//!
//! It reads the same `build.ninja` manifests as
//! [ninja](https://ninja-build.org/), shares its `.ninja_log` and
//! `.ninja_deps` files byte-for-byte, and reproduces its rebuild logic, so it
//! can be dropped into an existing build tree.
//!
//! # Embedding
//!
//! The quickest way in is [`Engine`]:
//!
//! ```no_run
//! use shuriken::{Engine, EngineOptions};
//!
//! let mut engine = Engine::load("build.ninja", EngineOptions::default())?;
//! let summary = engine.build(&["all"])?;
//! println!("{} edges run", summary.edges_finished);
//! # Ok::<(), shuriken::Error>(())
//! ```
//!
//! Everything the engine does is also available piecemeal, so an embedder can
//! replace individual layers:
//!
//! * [`DiskInterface`] abstracts the filesystem ([`MemDisk`] is an in-memory
//!   implementation useful for tests),
//! * [`CommandRunner`] abstracts process execution,
//! * [`Status`] abstracts progress reporting,
//! * [`State`] is the build graph, built by [`ManifestParser`] and inspected
//!   or mutated directly.
//!
//! # Compatibility notes
//!
//! * Manifests, depfiles and paths must be valid UTF-8.
//! * Load-average limiting (`-l`) is implemented on Linux; elsewhere it is
//!   ignored.
//! * The GNU make jobserver protocol is not implemented.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod build;
pub mod build_log;
pub mod canon;
pub mod clean;
pub mod depfile;
pub mod deps_log;
pub mod disk;
pub mod dyndep;
pub mod engine;
pub mod error;
pub mod escape;
pub mod eval;
pub mod exec;
pub mod graph;
pub mod hash;
pub mod lexer;
pub mod msvc;
pub mod parse;
pub mod plan;
pub mod state;
pub mod status;
pub mod tools;
pub mod util;
pub mod version;

pub use build::{BuildConfig, Builder, Verbosity};
pub use build_log::{BuildLog, LogEntry};
pub use deps_log::DepsLog;
pub use disk::{DiskInterface, MemDisk, RealDiskInterface};
pub use engine::{BuildSummary, Engine, EngineOptions};
pub use error::{Error, Result};
pub use exec::{CommandResult, CommandRunner, RealCommandRunner};
pub use graph::DependencyScan;
pub use parse::{ManifestParser, ParserOptions, PhonyCycleAction};
pub use plan::Plan;
pub use state::{Edge, EdgeId, Node, NodeId, Pool, PoolId, State};
pub use status::{ConsoleStatus, Status};
pub use version::{NINJA_COMPAT_VERSION, SHURIKEN_VERSION};
