//! Locating managed objects by scanning for instances of their class.
//!
//! Timberborn has no static roots into its Bindito DI container, so a service
//! cannot be reached by walking static fields. Services are singletons though,
//! so there is exactly one instance of each service class in the process, and
//! we can find it directly.
//!
//! Every managed object begins with a pointer to its class's vtable, so an
//! object is an instance of exactly that class iff its first pointer equals
//! that class's vtable address. Scanning for that value finds the instance
//! without needing any Unity-native offsets.
//!
//! A raw scan over-matches. Measured against the game, `DayNightCycle`
//! produced six matches: the real object, plus five words inside Mono's own
//! metadata, clustered around the vtable itself. Matches therefore have to be
//! validated -- see [`Validator`] -- and because validation is reliable, a scan
//! for a singleton can stop at the first match that survives it.
//!
//! Unity ships Mono with the Boehm collector (`mono-2.0-bdwgc.dll`), which does
//! not move objects, so an instance address stays valid for the lifetime of the
//! object. A rescan is only needed when the object itself is replaced, i.e. on
//! a scene change.

use alloc::{vec, vec::Vec};

use asr::{future::next_tick, Address, MemoryRangeFlags, Process};

/// Read 64 KiB at a time. Read as `u64` so the buffer is 8-byte aligned, which
/// lets us compare whole pointers rather than bytes.
const CHUNK_WORDS: usize = 8 * 1024;

/// Bytes to scan per slice. Measured at roughly 10-19ms per slice, which sits
/// about at the host's update interval.
pub const DEFAULT_BUDGET: u64 = 32 << 20;

/// Tells a real instance from a word that merely happens to equal the vtable
/// address.
///
/// Reads a reference-typed field off the candidate and confirms the object it
/// points at is an instance of the class that field is declared to hold. Mono's
/// own bookkeeping does not survive this: in the measured case one reject had a
/// non-null field that simply did not lead to an object of the right class, so
/// a null check alone would not have been enough.
#[derive(Clone, Copy)]
pub struct Validator {
    /// Offset of a reference-typed field on the candidate.
    pub field_offset: u32,
    /// Vtable the object that field points at must have.
    pub expected_vtable: Address,
}

impl Validator {
    pub fn accepts(&self, process: &Process, candidate: Address) -> bool {
        let Ok(field) = process.read::<u64>(candidate.add(self.field_offset as u64)) else {
            return false;
        };
        let field = Address::new(field);
        !field.is_null() && vtable_of(process, field) == Some(self.expected_vtable)
    }
}

/// What a scan looked at.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    /// Ranges that passed the readable-writable filter.
    pub ranges_scanned: u32,
    /// Ranges rejected by the filter.
    pub ranges_skipped: u32,
    /// Total bytes in the ranges to be scanned, before any early stop.
    pub bytes_total: u64,
    /// Bytes actually read and compared.
    pub bytes_scanned: u64,
    /// Chunks whose read failed. Expected to be non-zero: the memory map can
    /// change under us mid-scan.
    pub read_failures: u32,
    /// Slices the scan was spread across.
    pub slices: u32,
}

/// A resumable scan over the target's writable memory.
///
/// A whole-heap scan takes long enough that doing it in one go stalls the
/// splitter's update loop, which matters because the scan re-runs on scene
/// change -- exactly when a run starts. [`step`](Self::step) does a bounded
/// amount of work and returns, so the caller can yield between slices.
pub struct Scan {
    needle: u64,
    validator: Option<Validator>,
    stop_at_first: bool,
    ranges: Vec<(Address, u64)>,
    range_index: usize,
    offset: u64,
    buf: Vec<u64>,
    /// Matches that passed validation, or all matches if there is no validator.
    pub found: Vec<Address>,
    /// Matches that failed validation. Kept for diagnostics.
    pub rejected: Vec<Address>,
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
                    stats.bytes_total += r.1;
                    ranges.push(r);
                }
                Err(_) => stats.ranges_skipped += 1,
            }
        }

        Self {
            needle: vtable.value(),
            validator: None,
            stop_at_first: false,
            ranges,
            range_index: 0,
            offset: 0,
            buf: vec![0u64; CHUNK_WORDS],
            found: Vec::new(),
            rejected: Vec::new(),
            stats,
        }
    }

    /// Check each match, sorting them into `found` and `rejected`.
    pub fn validating(mut self, validator: Validator) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Stop as soon as one match is accepted. Only correct for classes that are
    /// genuinely singletons: `DistrictBuildingRegistry`, for instance, has one
    /// instance per district and needs a full scan.
    pub fn stop_at_first(mut self) -> Self {
        self.stop_at_first = true;
        self
    }

    /// Scans up to `budget` bytes. Returns `true` when the scan is finished,
    /// either because the address space is covered or because it stopped early.
    pub fn step(&mut self, process: &Process, budget: u64) -> bool {
        self.stats.slices += 1;
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
            let start = base.add(self.offset);

            if process
                .read_into_buf(start, bytemuck::cast_slice_mut(&mut self.buf[..words]))
                .is_err()
            {
                // The map can change while we walk it. Skip the chunk rather
                // than abandoning the range.
                self.stats.read_failures += 1;
            } else {
                self.stats.bytes_scanned += bytes;
                for i in 0..words {
                    if self.buf[i] != self.needle {
                        continue;
                    }
                    let candidate = start.add((i * 8) as u64);
                    match &self.validator {
                        Some(v) if !v.accepts(process, candidate) => {
                            self.rejected.push(candidate);
                        }
                        _ => {
                            self.found.push(candidate);
                            if self.stop_at_first {
                                self.offset += bytes;
                                return true;
                            }
                        }
                    }
                }
            }

            self.offset += bytes;
            used += bytes;
        }

        false
    }

    /// Runs the scan to completion, yielding between slices.
    pub async fn run(mut self, process: &Process, budget: u64) -> Self {
        while !self.step(process, budget) {
            next_tick().await;
        }
        self
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
