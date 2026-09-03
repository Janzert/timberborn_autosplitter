//! The seam everything else hangs off: a process with readable memory.
//!
//! A [`Memory`] answers reads at addresses and nothing else. Phase 3 will add a
//! backend that serves a snapshot of a real game; phase 4 one that serves a
//! synthesized Mono heap. Both plug in here, and neither changes anything above
//! this file.

use std::collections::BTreeMap;

/// A process's address space.
pub trait Memory {
    /// Fills `buf` from `address`. Returns false if any byte of the range is
    /// unreadable, which is what the real runtime does -- a partial read is
    /// never reported as success.
    fn read(&self, address: u64, buf: &mut [u8]) -> bool;
}

/// An address space with nothing mapped. Every read fails.
pub struct EmptyMemory;

impl Memory for EmptyMemory {
    fn read(&self, _address: u64, _buf: &mut [u8]) -> bool {
        false
    }
}

/// An address space assembled from explicitly placed blocks.
///
/// Enough to hand-build small cases without a snapshot or a fixture. Blocks may
/// not overlap, and a read spanning a gap fails as a whole.
#[derive(Default)]
pub struct SparseMemory {
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl SparseMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Places `bytes` at `address`.
    ///
    /// # Panics
    ///
    /// If the block would overlap one already placed. Silently overwriting
    /// would make a fixture's meaning depend on insertion order.
    pub fn put(&mut self, address: u64, bytes: impl Into<Vec<u8>>) -> &mut Self {
        let bytes = bytes.into();
        let end = address + bytes.len() as u64;
        if let Some((&start, prior)) = self.blocks.range(..end).next_back() {
            assert!(
                start + prior.len() as u64 <= address,
                "block at {address:#x} overlaps the one at {start:#x}"
            );
        }
        self.blocks.insert(address, bytes);
        self
    }

    /// Places a little-endian pointer-sized value, the common case by far.
    pub fn put_u64(&mut self, address: u64, value: u64) -> &mut Self {
        self.put(address, value.to_le_bytes())
    }
}

impl Memory for SparseMemory {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        let Some((&start, block)) = self.blocks.range(..=address).next_back() else {
            return false;
        };
        let offset = (address - start) as usize;
        let Some(available) = block.len().checked_sub(offset) else {
            return false;
        };
        if available < buf.len() {
            return false;
        }
        buf.copy_from_slice(&block[offset..offset + buf.len()]);
        true
    }
}

/// A loaded module, as `process_get_module_*` reports it.
#[derive(Clone)]
pub struct ModuleInfo {
    pub name: String,
    pub address: u64,
    pub size: u64,
    pub path: Option<String>,
}

/// Flag bits as `asr::MemoryRangeFlags` defines them.
///
/// Note the shift: `READ` is bit 1, not bit 0. Getting this wrong would make
/// the splitter's `READ | WRITE, not EXECUTE` filter select the wrong ranges,
/// silently and only in the harness.
pub mod flags {
    pub const READ: u64 = 1 << 1;
    pub const WRITE: u64 = 1 << 2;
    pub const EXECUTE: u64 = 1 << 3;
    /// The range is backed by a file.
    pub const PATH: u64 = 1 << 4;

    /// What the splitter's heap scan looks for.
    pub const HEAP: u64 = READ | WRITE;
}

/// A mapped range, as the `process_get_memory_range_*` family reports it.
#[derive(Clone)]
pub struct MemoryRange {
    pub address: u64,
    pub size: u64,
    /// A mask of [`flags`].
    pub flags: u64,
}

/// A process the fake runtime will let the splitter attach to.
pub struct FakeProcess {
    pub pid: u64,
    /// The name the runtime reports, which is what `process_attach` matches on.
    /// On Linux this is `/proc/<pid>/comm`, capped at 15 characters -- the cap
    /// that made the splitter need `AMBIGUOUS_NAMES` at all.
    pub name: String,
    pub path: Option<String>,
    pub modules: Vec<ModuleInfo>,
    pub ranges: Vec<MemoryRange>,
    pub memory: Box<dyn Memory>,
    /// Set false to model a process that has exited but is still attached to.
    pub open: bool,
}

impl FakeProcess {
    pub fn new(pid: u64, name: impl Into<String>) -> Self {
        Self {
            pid,
            name: name.into(),
            path: None,
            modules: Vec::new(),
            ranges: Vec::new(),
            memory: Box::new(EmptyMemory),
            open: true,
        }
    }

    pub fn with_memory(mut self, memory: impl Memory + 'static) -> Self {
        self.memory = Box::new(memory);
        self
    }

    pub fn with_module(mut self, name: impl Into<String>, address: u64, size: u64) -> Self {
        self.modules.push(ModuleInfo {
            name: name.into(),
            address,
            size,
            path: None,
        });
        self
    }

    pub fn with_range(mut self, address: u64, size: u64, flags: u64) -> Self {
        self.ranges.push(MemoryRange {
            address,
            size,
            flags,
        });
        self
    }
}
