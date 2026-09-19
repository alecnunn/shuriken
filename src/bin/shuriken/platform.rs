//! The small amount of platform glue the CLI needs: signal handling and the
//! terminal width.
//!
//! The library itself is `#![forbid(unsafe_code)]`; these two things need a
//! couple of libc calls, so they live here in the binary instead.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPT_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Arrange for `flag` to be set when the user interrupts the build.
///
/// Children run in our process group, so Ctrl-C reaches them directly; the flag
/// lets the build loop notice, stop starting work, and clean up half-written
/// outputs before exiting.
pub fn install_interrupt_handler(flag: Arc<AtomicBool>) {
    if INTERRUPT_FLAG.set(flag).is_err() {
        return; // Already installed.
    }
    #[cfg(unix)]
    unix::install();
}

/// The terminal width in columns, if stdout is a terminal.
pub fn terminal_width() -> Option<usize> {
    #[cfg(unix)]
    {
        unix::terminal_width()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
mod unix {
    use super::{INTERRUPT_FLAG, Ordering};

    type CInt = i32;

    const SIGHUP: CInt = 1;
    const SIGINT: CInt = 2;
    const SIGTERM: CInt = 15;

    // TIOCGWINSZ is not the same number everywhere.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const TIOCGWINSZ: u64 = 0x5413;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const TIOCGWINSZ: u64 = 0x4008_7468;

    #[repr(C)]
    #[derive(Default)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    unsafe extern "C" {
        fn signal(signum: CInt, handler: extern "C" fn(CInt)) -> usize;
        fn ioctl(fd: CInt, request: u64, ...) -> CInt;
    }

    extern "C" fn on_signal(_signum: CInt) {
        // Only an atomic store, which is safe to do from a signal handler.
        if let Some(flag) = INTERRUPT_FLAG.get() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn install() {
        // SAFETY: `signal` with a plain function pointer; the handler only
        // performs an atomic store.
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
            signal(SIGHUP, on_signal);
        }
    }

    pub fn terminal_width() -> Option<usize> {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            return None;
        }
        let mut size = Winsize::default();
        // SAFETY: `size` is a correctly sized, writable `struct winsize`.
        let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut size as *mut Winsize) };
        if rc != 0 || size.ws_col == 0 {
            return None;
        }
        Some(size.ws_col as usize)
    }
}
