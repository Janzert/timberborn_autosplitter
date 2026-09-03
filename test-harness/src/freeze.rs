//! Holding the game still while its memory is read.
//!
//! Capture walks gigabytes of a process that is otherwise still running, so
//! without this the last range is seconds younger than the first and the result
//! can hold a combination of values the game never actually had. Stopping it
//! first makes the capture a real instant.
//!
//! # Why SIGSTOP
//!
//! `ptrace` stops threads one at a time and the game has 108 of them, so
//! seizing them all is neither atomic nor cheap. The cgroup v2 freezer would be
//! ideal -- atomic, whole-subtree -- but Steam leaves the game in the login
//! session's own scope, shared with the desktop and with whatever is running
//! the capture, and that file is root-owned anyway. `SIGSTOP` stops every
//! thread of one process in one call, which is exactly the scope wanted.
//!
//! # The hole
//!
//! If this process dies without running its handlers -- `SIGKILL`, a power cut
//! -- the game stays stopped. Nothing can prevent that from the outside, so the
//! resume command is printed *before* stopping rather than after, where it
//! would never be reached.

use std::sync::atomic::{AtomicI32, Ordering};

/// The pid currently stopped, for the signal handlers. Zero means none.
static FROZEN: AtomicI32 = AtomicI32::new(0);

/// Stops a process for as long as this is alive.
///
/// Resuming happens on drop, which covers a normal return, an error return and
/// a panic. Signals are covered separately by [`install_handlers`], because a
/// terminating signal does not unwind and so does not run this.
pub struct Frozen {
    pid: i32,
}

impl Frozen {
    /// Stops `pid` and waits until the kernel reports it actually stopped.
    ///
    /// `kill` only *posts* the signal, so reading straight after it would race
    /// the very thing this exists to prevent.
    pub fn stop(pid: u32) -> Result<Self, String> {
        let pid = pid as i32;
        install_handlers();

        // SAFETY: `kill` with a valid signal is safe; the pid is checked by the
        // kernel and a bad one is reported as an error rather than acted on.
        if unsafe { libc::kill(pid, libc::SIGSTOP) } != 0 {
            return Err(format!(
                "cannot stop pid {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }
        FROZEN.store(pid, Ordering::SeqCst);

        for _ in 0..500 {
            match state(pid) {
                Some('T') => return Ok(Self { pid }),
                // Vanished mid-stop. Nothing left to resume.
                None => {
                    FROZEN.store(0, Ordering::SeqCst);
                    return Err(format!("pid {pid} disappeared while being stopped"));
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        // Leave the guard in place regardless, so the resume still happens.
        Err(format!(
            "pid {pid} did not report as stopped within a second; \
             resume it with: kill -CONT {pid}"
        ))
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        // SAFETY: as in `stop`.
        unsafe { libc::kill(self.pid, libc::SIGCONT) };
        FROZEN.store(0, Ordering::SeqCst);
    }
}

/// The process state from `/proc/<pid>/stat`: `T` once stopped.
///
/// The command name sits in parentheses and may itself contain spaces and
/// parentheses, so the state is the first field after the *last* `)`.
fn state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// Resumes the game if a terminating signal arrives, then dies as it would
/// have. Without this, Ctrl-C during a capture leaves the game stopped: a
/// terminating signal does not unwind, so [`Frozen`]'s `Drop` never runs.
fn install_handlers() {
    use std::sync::Once;
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: `on_signal` only calls `kill`, `signal` and `raise`, all
            // of which are async-signal-safe, and touches only an atomic.
            unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
        }
    });
}

extern "C" fn on_signal(signal: libc::c_int) {
    let pid = FROZEN.swap(0, Ordering::SeqCst);
    if pid != 0 {
        // SAFETY: async-signal-safe, and a failure here cannot be reported
        // anyway -- the printed resume command is the fallback.
        unsafe { libc::kill(pid, libc::SIGCONT) };
    }
    // SAFETY: restore the default disposition and die as we would have, so the
    // exit status still says which signal ended us.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}
