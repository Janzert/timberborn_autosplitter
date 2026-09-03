//! Opens a Timberborn process's memory and passes the descriptor back.
//!
//! ```text
//! tb-ptrace-open <pid>        # with fd 3 a unix socket to the caller
//! ```
//!
//! # Why this exists
//!
//! Reading another process's memory needs ptrace rights, and the tools that
//! want to read -- `tb-dump`, `tb-record` -- change all the time. A capability
//! is an attribute of a file, so it is lost on every rebuild, and granting it to
//! tools under active development means re-granting it constantly and running
//! development binaries with more privilege than they need.
//!
//! `/proc/<pid>/mem` is permission-checked when it is **opened**, not on each
//! read -- verified on this kernel, not assumed: a process that cannot open its
//! parent's memory can still read it through a descriptor the parent passed
//! down. So the privilege can live in one small program that opens the file and
//! hands the descriptor over. The tools then read through it with no rights of
//! their own.
//!
//! This program should almost never change, which is the point. It does the
//! validation itself rather than trusting its caller, because it is the piece
//! holding the capability.
//!
//! # Install
//!
//! ```text
//! cargo install --path tb-ptrace-open --root ~/.local \
//!   && sudo setcap cap_sys_ptrace+ep ~/.local/bin/tb-ptrace-open
//! ```

use std::{ffi::CString, os::fd::RawFd, process::ExitCode};

/// The socket to the caller. Inherited, so there is no path to guess and no
/// window in which anyone else could connect.
const SOCKET_FD: RawFd = 3;

fn main() -> ExitCode {
    let Some(pid) = std::env::args().nth(1).and_then(|a| a.parse::<u32>().ok()) else {
        eprintln!("usage: tb-ptrace-open <pid>   (with fd 3 a unix socket to the caller)");
        return ExitCode::FAILURE;
    };

    // Checked here rather than in the caller: this is the program with the
    // capability, so this is the only place a check cannot be skipped by
    // whoever invokes it.
    if !is_timberborn(pid) {
        eprintln!("tb-ptrace-open: pid {pid} has no Timberborn data mapped; refusing.");
        return ExitCode::FAILURE;
    }

    let path = match CString::new(format!("/proc/{pid}/mem")) {
        Ok(path) => path,
        Err(_) => return ExitCode::FAILURE,
    };
    // SAFETY: a valid NUL-terminated path and a valid flag.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        eprintln!(
            "tb-ptrace-open: cannot open /proc/{pid}/mem: {}.\n\
             This binary needs the capability: sudo setcap cap_sys_ptrace+ep <this file>",
            std::io::Error::last_os_error()
        );
        return ExitCode::FAILURE;
    }

    match send_fd(SOCKET_FD, fd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tb-ptrace-open: cannot pass the descriptor back: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Whether the process is the game.
///
/// The same rule the tools use, restated here rather than shared: this program
/// deliberately depends on nothing that changes. `/proc/<pid>/exe` is no use
/// under Proton -- it points at Wine's preloader -- so the signal is that the
/// game's own data directory is mapped.
fn is_timberborn(pid: u32) -> bool {
    let Ok(maps) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else {
        return false;
    };
    maps.lines()
        .any(|line| line.contains("/Timberborn_Data/") || line.ends_with("/Timberborn.exe"))
}

/// Sends `fd` over `socket` as an SCM_RIGHTS ancillary message.
fn send_fd(socket: RawFd, fd: RawFd) -> std::io::Result<()> {
    let mut payload = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // Sized generously; the kernel only reads as far as msg_controllen says.
    let mut control = [0u8; 64];

    // SAFETY: msghdr is a plain C struct with no invalid bit patterns, and
    // every pointer below outlives the sendmsg call.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as _;

    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(std::io::Error::other("no room for the control message"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);

        if libc::sendmsg(socket, &message, 0) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
