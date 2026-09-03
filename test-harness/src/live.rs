//! Reading a live process, for capturing snapshots. Linux only.
//!
//! Uses `/proc/<pid>/mem` with positioned reads rather than `process_vm_readv`,
//! which needs no dependency and has the same permission model. That model is
//! the catch: with `kernel.yama.ptrace_scope` at its usual `1`, only a
//! descendant of the reader can be read. See `snapshots/README.md`.

use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::{fs::FileExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::Command,
};

use crate::memory::{flags, Memory, MemoryRange, ModuleInfo};

/// One line of `/proc/<pid>/maps`.
pub struct Mapping {
    pub address: u64,
    pub size: u64,
    pub flags: u64,
    /// Offset into the backing file, meaningless without `path`.
    pub file_offset: u64,
    pub path: Option<PathBuf>,
}

impl Mapping {
    pub fn readable(&self) -> bool {
        self.flags & flags::READ != 0
    }

    /// Mappings that cannot be read as ordinary memory, or that hang when
    /// tried. Device mappings are the game's GPU buffers under Proton.
    pub fn is_special(&self) -> bool {
        match self.path.as_deref().and_then(Path::to_str) {
            Some(path) => {
                path.starts_with("/dev/")
                    || path.starts_with("[vvar")
                    || path == "[vsyscall]"
                    || path.starts_with("/memfd:wine")
            }
            None => false,
        }
    }

    pub fn file_name(&self) -> Option<&str> {
        self.path.as_deref()?.file_name()?.to_str()
    }

    /// The range as the runtime would report it.
    pub fn to_range(&self) -> MemoryRange {
        MemoryRange {
            address: self.address,
            size: self.size,
            flags: self.flags,
        }
    }
}

/// Parses `/proc/<pid>/maps`.
pub fn mappings(pid: u32) -> io::Result<Vec<Mapping>> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    Ok(text.lines().filter_map(parse_mapping).collect())
}

fn parse_mapping(line: &str) -> Option<Mapping> {
    let mut fields = line.split_whitespace();
    let (start, end) = fields.next()?.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;

    let perms = fields.next()?.as_bytes();
    let mut bits = 0;
    if perms.first() == Some(&b'r') {
        bits |= flags::READ;
    }
    if perms.get(1) == Some(&b'w') {
        bits |= flags::WRITE;
    }
    if perms.get(2) == Some(&b'x') {
        bits |= flags::EXECUTE;
    }

    let file_offset = u64::from_str_radix(fields.next()?, 16).ok()?;

    // The path is the sixth field and may itself contain spaces, so it is
    // whatever remains of the line rather than another whitespace split.
    let mut rest = line;
    for _ in 0..5 {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    let path = Some(rest.trim())
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    if path.is_some() {
        bits |= flags::PATH;
    }

    Some(Mapping {
        address: start,
        size: end.saturating_sub(start),
        flags: bits,
        file_offset,
        path,
    })
}

/// Groups mappings into modules the way the real runtime reports them.
///
/// Mirrors livesplit-core's rule rather than inventing one: a module is a run
/// of *consecutive entries in the maps list* sharing a filename, its address is
/// the first entry's and its size the sum of the run's.
///
/// # Why every Proton module comes out one page long
///
/// Measured, not assumed. Wine maps only a PE's **header page** from the file;
/// the sections that follow are anonymous mappings with no path at all:
///
/// ```text
/// 6ffff9330000-6ffff9331000 r--p ... /Timberborn/.../mono-2.0-bdwgc.dll
/// 6ffff9331000-6ffff98a5000 r-xp 00000000 00:00 0
/// 6ffff98a5000-6ffff9a6a000 r--p 00000000 00:00 0
/// ```
///
/// So a filename-keyed rule can only ever see 0x1000, and livesplit-core
/// reports exactly that on Linux too. This is fidelity rather than a bug: the
/// harness has to show the splitter what the runtime shows it, not something
/// more accurate. Reads are by address and reach the sections regardless.
///
/// Note that desktop LiveSplit *inside the prefix* sees Wine's own module
/// table instead, with real image sizes -- one of the ways the two hosts
/// differ.
pub fn modules(mappings: &[Mapping]) -> Vec<ModuleInfo> {
    let mut modules: Vec<ModuleInfo> = Vec::new();
    for mapping in mappings {
        let Some(name) = mapping.file_name() else {
            continue;
        };
        match modules.last_mut() {
            Some(last) if last.name == name => last.size += mapping.size,
            _ => modules.push(ModuleInfo {
                name: name.to_owned(),
                address: mapping.address,
                size: mapping.size,
                path: mapping
                    .path
                    .as_deref()
                    .and_then(Path::to_str)
                    .map(str::to_owned),
            }),
        }
    }
    modules
}

/// The ranges the runtime would report, which is every readable mapping.
pub fn ranges(mappings: &[Mapping]) -> Vec<MemoryRange> {
    mappings
        .iter()
        .filter(|m| m.readable())
        .map(Mapping::to_range)
        .collect()
}

/// A live process's address space, read through `/proc/<pid>/mem`.
pub struct LiveMemory {
    mem: File,
}

impl LiveMemory {
    /// Opens a process's memory, directly if allowed and through the helper if
    /// not.
    ///
    /// A direct open needs ptrace rights, which `kernel.yama.ptrace_scope`
    /// withholds at its usual setting. Rather than granting a capability to
    /// every tool that wants to read -- all of which are rebuilt constantly,
    /// dropping it each time -- the privilege lives in `tb-ptrace-open`, which
    /// opens the file and passes the descriptor back. The check happens at open
    /// time, so the descriptor keeps working here.
    ///
    /// # Errors
    ///
    /// If both routes fail, with what to install and why -- `EACCES` on its own
    /// says nothing about which of the two is missing.
    pub fn open(pid: u32) -> io::Result<Self> {
        match File::open(format!("/proc/{pid}/mem")) {
            Ok(mem) => Ok(Self { mem }),
            Err(direct) => match open_via_helper(pid) {
                Ok(mem) => Ok(Self { mem }),
                Err(helper) => Err(io::Error::new(
                    direct.kind(),
                    format!(
                        "cannot read /proc/{pid}/mem.\n       \
                         Directly: {direct}\n       \
                         Through {HELPER}: {helper}\n       \
                         Install the helper and grant it the capability -- it is the \
                         only piece that needs one:\n       \
                         cargo install --path tb-ptrace-open --root ~/.local \\\n       \
                           && sudo setcap cap_sys_ptrace+ep ~/.local/bin/{HELPER}"
                    ),
                )),
            },
        }
    }
}

/// The privileged helper, by name on `PATH`. `TB_PTRACE_OPEN` names it
/// explicitly, which is what a test uses to point at a stand-in.
const HELPER: &str = "tb-ptrace-open";

/// Asks the helper to open the process and hand back the descriptor.
fn open_via_helper(pid: u32) -> io::Result<File> {
    let helper = std::env::var_os("TB_PTRACE_OPEN").unwrap_or_else(|| HELPER.into());
    spawn_and_receive(&mut Command::new(helper), &[pid.to_string()])
}

/// Runs `command` with a socket on fd 3 and takes the descriptor it sends back.
///
/// Public so the descriptor-passing can be tested against a stand-in helper;
/// nothing outside this crate should need it.
///
/// Passing the socket as an inherited descriptor rather than binding a path
/// means there is nothing to guess and no window for anyone else to connect.
#[doc(hidden)]
pub fn spawn_and_receive(command: &mut Command, args: &[String]) -> io::Result<File> {
    let (ours, theirs) = UnixStream::pair()?;
    let theirs = theirs.into_raw_fd();

    command.args(args);
    // SAFETY: between fork and exec only async-signal-safe calls are made.
    // `dup2` clears FD_CLOEXEC on the copy, which is what lets fd 3 survive the
    // exec.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(theirs, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().inspect_err(|_| {
        // SAFETY: `theirs` is ours to close on the failure path.
        unsafe { libc::close(theirs) };
    })?;
    // SAFETY: the child has its own copy now; holding this end open would make
    // a failed helper look like a hang rather than an EOF.
    unsafe { libc::close(theirs) };

    let received = receive_fd(ours.as_raw_fd());
    let status = child.wait()?;
    match received {
        Ok(fd) if status.success() => {
            // SAFETY: `fd` came from the kernel via SCM_RIGHTS and is ours.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
        Ok(fd) => {
            // SAFETY: as above; discarded because the helper reported failure.
            unsafe { libc::close(fd) };
            Err(io::Error::other(format!("helper exited with {status}")))
        }
        Err(error) if !status.success() => Err(io::Error::other(format!(
            "helper exited with {status} ({error})"
        ))),
        Err(error) => Err(error),
    }
}

/// Sends one descriptor as an SCM_RIGHTS message.
///
/// Only the test fixture needs this -- `tb-ptrace-open` carries its own copy
/// rather than depending on this crate, which changes constantly.
#[doc(hidden)]
pub fn send_fd_for_test(socket: RawFd, fd: RawFd) -> io::Result<()> {
    let mut payload = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u8; 64];

    // SAFETY: as in `receive_fd`.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    unsafe {
        message.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(io::Error::other("no room for the control message"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        if libc::sendmsg(socket, &message, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Takes one descriptor from an SCM_RIGHTS message.
fn receive_fd(socket: RawFd) -> io::Result<RawFd> {
    let mut payload = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u8; 64];

    // SAFETY: msghdr is a plain C struct, and every pointer outlives the call.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;

    unsafe {
        let read = libc::recvmsg(socket, &mut message, 0);
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        if read == 0 {
            return Err(io::Error::other("the helper sent nothing"));
        }
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other(
                "the helper's reply carried no file descriptor",
            ));
        }
        Ok(std::ptr::read_unaligned(
            libc::CMSG_DATA(header).cast::<RawFd>(),
        ))
    }
}

impl Memory for LiveMemory {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        self.mem.read_exact_at(buf, address).is_ok()
    }
}

/// Processes whose `/proc/<pid>/comm` matches `name`.
///
/// `comm` is what the auto splitting runtime matches on, and it is capped at 15
/// characters -- the cap behind the splitter's `AMBIGUOUS_NAMES`.
pub fn find_by_comm(name: &str) -> io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue;
        };
        if comm.trim_end() == name {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_maps_line() {
        let line = "7f2c4a800000-7f2c4a9f4000 r-xp 00001000 08:02 1234567  /usr/lib/libc.so.6";
        let m = parse_mapping(line).unwrap();
        assert_eq!(m.address, 0x7f2c_4a80_0000);
        assert_eq!(m.size, 0x1f4000);
        assert_eq!(m.file_offset, 0x1000);
        assert_eq!(m.flags, flags::READ | flags::EXECUTE | flags::PATH);
        assert_eq!(m.file_name(), Some("libc.so.6"));
    }

    /// Steam library paths have spaces in them, so the path cannot be taken as
    /// the sixth whitespace-separated field.
    #[test]
    fn parses_a_path_containing_spaces() {
        let line =
            "140000000-140010000 rw-p 00000000 08:02 99  /games/steam apps/Timberborn/Timberborn.exe";
        let m = parse_mapping(line).unwrap();
        assert_eq!(m.file_name(), Some("Timberborn.exe"));
        assert_eq!(m.flags, flags::READ | flags::WRITE | flags::PATH);
    }

    #[test]
    fn parses_an_anonymous_mapping() {
        let m = parse_mapping("55a0-55b0 rw-p 00000000 00:00 0").unwrap();
        assert!(m.path.is_none());
        assert_eq!(m.flags, flags::READ | flags::WRITE);
        assert!(!m.is_special());
    }

    #[test]
    fn groups_consecutive_mappings_of_one_file_into_a_module() {
        let maps = "\
400000-401000 r--p 00000000 08:02 1 /game/Timberborn.exe
401000-410000 r-xp 00001000 08:02 1 /game/Timberborn.exe
500000-501000 r--p 00000000 08:02 2 /game/mono-2.0-bdwgc.dll";
        let mappings: Vec<_> = maps.lines().filter_map(parse_mapping).collect();
        let modules = modules(&mappings);

        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].name, "Timberborn.exe");
        assert_eq!(modules[0].address, 0x400000);
        assert_eq!(modules[0].size, 0x10000, "the run's sizes summed");
        assert_eq!(modules[1].name, "mono-2.0-bdwgc.dll");
    }

    /// The read path, against the only process ptrace restrictions never block.
    #[test]
    fn reads_this_process_through_proc_mem() {
        static NEEDLE: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];

        let memory = LiveMemory::open(std::process::id()).unwrap();
        let mut buf = [0u8; 8];
        assert!(memory.read(std::ptr::addr_of!(NEEDLE) as u64, &mut buf));
        assert_eq!(buf, NEEDLE);

        assert!(!memory.read(0x10, &mut buf), "the null page is not mapped");
    }

    #[test]
    fn lists_this_process_by_its_comm() {
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", std::process::id())).unwrap();
        let pids = find_by_comm(comm.trim_end()).unwrap();
        assert!(pids.contains(&std::process::id()));
    }
}
