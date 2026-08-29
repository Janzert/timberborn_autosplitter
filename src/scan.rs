//! Locating managed objects by scanning for instances of their class.
//!
//! Timberborn has no static roots into its Bindito DI container, so a service
//! cannot be reached by walking static fields. Every service is a singleton
//! though, so there is exactly one instance of each service class in the
//! process, and we can find it directly.
//!
//! Every managed object begins with a pointer to its class's vtable, so an
//! object is an instance of exactly that class iff its first pointer equals
//! that class's vtable address. Scanning for that value finds the instance
//! without needing any Unity-native offsets.
//!
//! A raw scan over-matches: the vtable address also appears in Mono's own
//! bookkeeping, including the `domain_vtables[0]` slot that
//! `Class::get_vtable` reads it out of. Candidates therefore have to be
//! validated -- see [`validate`].
//!
//! Unity ships Mono with the Boehm collector (`mono-2.0-bdwgc.dll`), which does
//! not move objects, so an instance address stays valid for the lifetime of the
//! object. A rescan is only needed when the object itself is replaced, i.e. on
//! a scene change.

use alloc::{vec, vec::Vec};

use asr::{Address, MemoryRangeFlags, Process};

/// Read 64 KiB at a time. Read as `u64` so the buffer is 8-byte aligned, which
/// lets us compare whole pointers rather than bytes.
const CHUNK_WORDS: usize = 8 * 1024;

/// What a scan looked at.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    /// Ranges that passed the readable-writable filter.
    pub ranges_scanned: u32,
    /// Ranges rejected by the filter.
    pub ranges_skipped: u32,
    /// Bytes actually read and compared.
    pub bytes_scanned: u64,
    /// Chunks whose read failed. Expected to be non-zero: the memory map can
    /// change under us mid-scan.
    pub read_failures: u32,
    /// Ticks the scan was spread across.
    pub ticks: u32,
}

/// A resumable scan over the target's writable memory.
///
/// A whole-heap scan takes long enough that doing it in one go stalls the
/// runtime, which matters because the scan has to re-run on scene change --
/// exactly when a run starts. [`step`](Self::step) does a bounded amount of
/// work and returns, so the caller can yield between slices.
pub struct Scan {
    needle: u64,
    ranges: Vec<(Address, u64)>,
    range_index: usize,
    offset: u64,
    buf: Vec<u64>,
    /// Every address whose first word matched. Unvalidated.
    pub hits: Vec<Address>,
    pub stats: Stats,
}

impl Scan {
    /// Enumerates and filters the target's memory ranges. Cheap; the reads
    /// happen in [`step`](Self::step).
    pub fn new(process: &Process, vtable: Address) -> Self {
        let mut ranges = Vec::new();
        let mut stats = Stats::default();

        for range in process.memory_ranges() {
            // The managed heap is readable and writable. Skipping executable
            // pages drops the code sections, which cannot hold objects.
            let wanted = MemoryRangeFlags::READ | MemoryRangeFlags::WRITE;
            let ok = range.flags().is_ok_and(|flags| {
                flags.contains(wanted) && !flags.contains(MemoryRangeFlags::EXECUTE)
            });
            if !ok {
                stats.ranges_skipped += 1;
                continue;
            }
            match range.range() {
                Ok(r) => {
                    stats.ranges_scanned += 1;
                    ranges.push(r);
                }
                Err(_) => stats.ranges_skipped += 1,
            }
        }

        Self {
            needle: vtable.value(),
            ranges,
            range_index: 0,
            offset: 0,
            buf: vec![0u64; CHUNK_WORDS],
            hits: Vec::new(),
            stats,
        }
    }

    /// Scans up to `budget` bytes. Returns `true` once the whole address space
    /// has been covered.
    pub fn step(&mut self, process: &Process, budget: u64) -> bool {
        self.stats.ticks += 1;
        let mut used = 0u64;

        while used < budget {
            let Some(&(base, size)) = self.ranges.get(self.range_index) else {
                return true;
            };

            if self.offset >= size {
                self.range_index += 1;
                self.offset = 0;
                continue;
            }

            let remaining = size - self.offset;
            let words = (remaining / 8).min(CHUNK_WORDS as u64) as usize;
            if words == 0 {
                // A tail shorter than a pointer cannot hold one.
                self.range_index += 1;
                self.offset = 0;
                continue;
            }

            let bytes = (words * 8) as u64;
            let chunk = &mut self.buf[..words];

            if process
                .read_into_buf(base.add(self.offset), bytemuck::cast_slice_mut(chunk))
                .is_err()
            {
                // The map can change while we walk it. Skip the chunk rather
                // than abandoning the range.
                self.stats.read_failures += 1;
            } else {
                self.stats.bytes_scanned += bytes;
                for (i, &word) in chunk.iter().enumerate() {
                    if word == self.needle {
                        self.hits.push(base.add(self.offset + (i * 8) as u64));
                    }
                }
            }

            self.offset += bytes;
            used += bytes;
        }

        false
    }
}

/// Reads the vtable pointer of a managed object, i.e. its class identity.
pub fn vtable_of(process: &Process, object: Address) -> Option<Address> {
    process
        .read::<u64>(object)
        .ok()
        .map(Address::new)
        .filter(|a| !a.is_null())
}

/// Checks that a scan hit is really an instance rather than a stray word.
///
/// Reads a reference-typed field off the candidate and confirms the object it
/// points at is an instance of the class that field is declared to hold. Words
/// that merely happen to equal the vtable address -- Mono's own bookkeeping,
/// stale stack slots -- do not survive this.
pub fn validate(
    process: &Process,
    candidate: Address,
    field_offset: u32,
    expected: Address,
) -> bool {
    let Ok(field) = process.read::<u64>(candidate.add(field_offset as u64)) else {
        return false;
    };
    let field = Address::new(field);
    if field.is_null() {
        return false;
    }
    vtable_of(process, field) == Some(expected)
}
