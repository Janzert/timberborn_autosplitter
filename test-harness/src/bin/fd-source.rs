//! A stand-in for `tb-ptrace-open`, so the descriptor-passing can be tested
//! without a capability or a running game.
//!
//! Opens the file named by its first argument and sends the descriptor over the
//! socket on fd 3 -- the same protocol, none of the privilege. Test fixture
//! only; nothing outside `live.rs`'s tests should invoke it.

use std::{ffi::CString, os::fd::RawFd, process::ExitCode};

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        return ExitCode::FAILURE;
    };
    if path == "--fail" {
        eprintln!("fd-source: asked to fail");
        return ExitCode::FAILURE;
    }
    let Ok(path) = CString::new(path) else {
        return ExitCode::FAILURE;
    };
    // SAFETY: a valid NUL-terminated path.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return ExitCode::FAILURE;
    }
    match test_harness::live::send_fd_for_test(3 as RawFd, fd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
