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
    /// Chunks whose bulk read failed and had to be retried page by page. The
    /// map changes under us mid-scan, and a chunk spanning one unmapped page
    /// fails as a whole.
    pub read_failures: u32,
    /// Bytes that could not be read even page by page, and so were never
    /// compared. Any hit inside them is missed, so a scan that finds nothing
    /// is only trustworthy when this is 0.
    pub bytes_unreadable: u64,
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
    /// A second value to notice in passing.
    ///
    /// A sweep already reads every byte, so looking for one more value costs a
    /// comparison per word against reads that dominate by orders of magnitude.
    /// It is how the reference table gets found without a pass of its own: give
    /// a sweep the address of something already located, and the ranges holding
    /// pointers to it come back for nothing.
    anchor: Option<u64>,
    validator: Option<Validator>,
    limit: Option<usize>,
    ranges: Vec<(Address, u64)>,
    range_index: usize,
    offset: u64,
    buf: Vec<u64>,
    /// Matches that passed validation, or all matches if there is no validator.
    pub found: Vec<Address>,
    /// The base of the range each entry in `found` came from. Same length as
    /// `found`, and what [`crate::table`] picks its candidate ranges out of.
    pub found_ranges: Vec<(Address, u64)>,
    /// Ranges a pointer to `anchor` was seen in, deduplicated.
    pub anchor_ranges: Vec<(Address, u64)>,
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
            anchor: None,
            anchor_ranges: Vec::new(),
            validator: None,
            limit: None,
            ranges,
            range_index: 0,
            offset: 0,
            buf: vec![0u64; CHUNK_WORDS],
            found: Vec::new(),
            found_ranges: Vec::new(),
            rejected: Vec::new(),
            stats,
        }
    }

    /// Whether an empty result actually means "not present".
    ///
    /// A scan proves absence only if it read everything it set out to read.
    /// Memory can be transiently unreadable -- measured at ~193 MiB during a
    /// scene teardown under Proton -- and a dying process reports no ranges at
    /// all, which would otherwise look like a clean negative.
    pub fn is_conclusive(&self) -> bool {
        self.stats.bytes_total > 0 && self.stats.bytes_unreadable == 0
    }

    /// Also note which ranges hold a pointer to `anchor`, in the same pass.
    ///
    /// For finding the reference table off the back of a sweep that was
    /// happening anyway, rather than paying for one of its own.
    pub fn also_finding(mut self, anchor: Address) -> Self {
        self.anchor = Some(anchor.value());
        self
    }

    /// Check each match, sorting them into `found` and `rejected`.
    pub fn validating(mut self, validator: Validator) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Stop once `limit` matches have been accepted.
    ///
    /// A limit of 1 suits a singleton. Classes that are legitimately
    /// multi-instance -- `DistrictBuildingRegistry` is per-district -- need a
    /// higher limit or none at all, and an unlimited scan always reads the
    /// whole address space.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
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
                .is_ok()
            {
                self.stats.bytes_scanned += bytes;
                if self.examine(process, start, words) {
                    self.offset += bytes;
                    return true;
                }
            } else {
                // A chunk spanning a single unmapped page fails as a whole.
                // Dropping it would silently skip 64 KiB and can hide the very
                // object we are looking for, so fall back to page-sized reads
                // and only give up on the pages that really are unreadable.
                self.stats.read_failures += 1;
                if self.examine_by_page(process, start, words) {
                    self.offset += bytes;
                    return true;
                }
            }

            self.offset += bytes;
            used += bytes;
        }

        false
    }

    /// Compares the first `words` of the buffer against the needle, recording
    /// matches. Returns `true` if the scan should stop.
    fn examine(&mut self, process: &Process, start: Address, words: usize) -> bool {
        // A separate pass rather than a second test inside the one below, so
        // that a scan with no anchor is untouched -- measured, folding the two
        // together cost 55% of the comparison loop, which is around 5% of a
        // sweep under Proton, and it would have been paid by every sweep
        // whether it wanted an anchor or not. As a second pass it is paid only
        // when asked for, and over a chunk that is still in cache from the read
        // that filled it.
        if let Some(anchor) = self.anchor {
            if self.buf[..words].contains(&anchor) {
                if let Some(range) = self.ranges.get(self.range_index).copied() {
                    if !self.anchor_ranges.contains(&range) {
                        self.anchor_ranges.push(range);
                    }
                }
            }
        }

        for i in 0..words {
            if self.buf[i] != self.needle {
                continue;
            }
            let candidate = start.add((i * 8) as u64);
            match &self.validator {
                Some(v) if !v.accepts(process, candidate) => self.rejected.push(candidate),
                _ => {
                    let base = self.ranges.get(self.range_index).copied();
                    self.found.push(candidate);
                    if let Some(base) = base {
                        self.found_ranges.push(base);
                    }
                    if self.limit.is_some_and(|n| self.found.len() >= n) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Retries a failed chunk one page at a time, so a single unmapped page
    /// costs us that page rather than the whole 64 KiB window.
    fn examine_by_page(&mut self, process: &Process, start: Address, words: usize) -> bool {
        const PAGE_WORDS: usize = 512; // 4 KiB

        let mut done = 0;
        while done < words {
            let n = PAGE_WORDS.min(words - done);
            let at = start.add((done * 8) as u64);
            if process
                .read_into_buf(at, bytemuck::cast_slice_mut(&mut self.buf[..n]))
                .is_ok()
            {
                self.stats.bytes_scanned += (n * 8) as u64;
                if self.examine(process, at, n) {
                    return true;
                }
            } else {
                self.stats.bytes_unreadable += (n * 8) as u64;
            }
            done += n;
        }
        false
    }

    /// Runs the scan to completion, yielding between slices, with `on_tick`
    /// called between them.
    ///
    /// A scan is seconds long under Proton, and anything that must be read
    /// every tick goes unread for all of it. The run start is the case that
    /// matters: `ShowUI` came and went inside a single clock scan, so a
    /// correctly bound watcher still missed the only moment it exists for.
    pub async fn run_polling(
        mut self,
        process: &Process,
        budget: u64,
        mut on_tick: impl FnMut(),
    ) -> Self {
        while !self.step(process, budget) {
            on_tick();
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
