//! Reading a live process, for capturing snapshots. Linux only.
//!
//! Uses `/proc/<pid>/mem` with positioned reads rather than `process_vm_readv`,
//! which needs no dependency and has the same permission model. That model is
//! the catch: with `kernel.yama.ptrace_scope` at its usual `1`, only a
//! descendant of the reader can be read. See `snapshots/README.md`.

use std::{
    fs::File,
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
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
/// the first entry's and its size the sum of the run's. Adjacency of addresses
/// does not come into it, which matters because Wine's PE images have gaps
/// between their sections.
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
    /// # Errors
    ///
    /// Opening fails with `EACCES` when ptrace is restricted, which is the
    /// usual reason rather than a missing process.
    pub fn open(pid: u32) -> io::Result<Self> {
        Ok(Self {
            mem: File::open(format!("/proc/{pid}/mem"))?,
        })
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
