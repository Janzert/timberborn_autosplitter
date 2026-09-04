//! The runtime's table of references to live managed objects.
//!
//! Unity's native side holds managed objects through a table of references,
//! and that table is the shortcut past the heap scan. Everything the splitter
//! has to locate -- the scene loader, the game's DI container, the run start's
//! `GameInitializer` -- has an entry in it, so a scene change costs a read of
//! the table plus the pages its entries point at, rather than a sweep of every
//! writable byte in the process.
//!
//! Measured against three recordings on two game builds:
//!
//! | moment | through the table | full sweep |
//! |---|---|---|
//! | main menu | 3 MiB | 811 MiB |
//! | first game | 20 MiB | 3916 MiB |
//! | second game of the session | 70 MiB | 5195 MiB |
//!
//! The sweep grows as the process does -- 3916 MiB to 5195 MiB between two
//! games of one session -- so the scan this replaces is at its worst exactly
//! when a runner is resetting for another attempt.
//!
//! # Nothing here is a fixed address
//!
//! The table is at `0x1f0000000` on one build and `0x230000000` on another, so
//! it is *found* rather than tabulated: sweep once for anything pointing at an
//! object we have already located, and the table is among the ranges that come
//! back. See [`ReferenceTable::find`].
//!
//! # What it is, and what it is not
//!
//! It is not a superset of the heap. A capture taken while a second game
//! loaded held a validated `SingletonRepository` with no entry here at all --
//! a main-menu container whose scene had already been torn down, with two
//! references left to it against the seven each live container had. The
//! reading that fits every observation is that an entry is dropped when the
//! object dies and the heap keeps the corpse until it is collected, which
//! would make this the *live* set and so strictly better than what a sweep
//! returns. That cannot be proven from a memory image, though, so nothing here
//! depends on it: a table search never reports a conclusive absence, and the
//! full sweep stays behind it.

use alloc::{format, vec, vec::Vec};

use asr::{future::next_tick, Address, MemoryRangeFlags, Process};

use crate::scan::{self, Validator};

/// How many of a candidate's words may point into the region where vtables
/// live -- i.e. look like object headers -- before it is a heap section rather
/// than a table.
///
/// Absolute rather than a fraction of the range, because the two populations
/// do not scale together. Measured across three recordings on two game builds:
/// a table has **0 to 2** such words, and the sparsest heap section competing
/// with one has **2355**. Sixty-four sits a factor of thirty from each side.
///
/// A fraction was tried and is worse in both directions at once. The heap
/// sections range from 0.7% to 30% object headers, so a threshold loose enough
/// to keep a 2600 KiB section out at 0.7% is tighter than the margin it buys
/// anywhere else -- and it was measured accepting that very section as a
/// table.
///
/// The count is checked as a candidate is read, so a heap section is abandoned
/// after sixty-five headers rather than read to the end.
const MAX_HEADER_WORDS: u32 = 64;

/// Read the table and the pages its entries point at in windows this size.
///
/// Bigger than a page because the cost through Wine is per read rather than
/// per byte: the entries of one table cluster into ~600 windows of this size
/// against ~18,000 individual pages.
const WINDOW: u64 = 64 << 10;

/// Refuse to treat anything larger as a candidate table.
///
/// Only a bound on the worst case. The table is 2 MiB on both builds measured;
/// this leaves room for it to grow by a factor of thirty-two before the
/// splitter would quietly stop finding it and fall back to sweeping.
const MAX_TABLE_BYTES: u64 = 64 << 20;

/// What a search through the table turned up.
pub struct Search {
    pub instances: Vec<Address>,
    /// Whether the table could be read at all. False means the caller learned
    /// nothing and should sweep; it is never a statement about the game.
    pub readable: bool,
}

/// Where the runtime keeps its references to live managed objects.
pub struct ReferenceTable {
    base: Address,
    size: u64,
}

impl ReferenceTable {
    /// Finds the table, given one object already located by other means.
    ///
    /// `anchor` should be something long-lived -- the scene loader is ideal,
    /// since it exists at the main menu and outlives every scene. The sweep
    /// for words pointing at it is the second and last full pass the splitter
    /// makes; every search after this one goes through the result.
    ///
    /// Candidates are told apart by content, not by size or address. A heap
    /// section is full of object headers -- words pointing into the region
    /// where vtables live -- and the table has none. See [`MAX_HEADER_WORDS`].
    pub async fn find(
        process: &Process,
        anchor: Address,
        mut on_tick: impl FnMut(),
    ) -> Option<Self> {
        let vtable = scan::vtable_of(process, anchor)?;
        let metadata = region_containing(process, vtable)?;

        asr::print_message("[table] looking for the runtime's reference table.");
        let holders = scan::Scan::new(process, anchor)
            .run_polling(process, scan::DEFAULT_BUDGET, &mut on_tick)
            .await;

        let mut candidates: Vec<(Address, u64)> = Vec::new();
        for &range in &holders.found_ranges {
            if range.1 <= MAX_TABLE_BYTES && !candidates.contains(&range) {
                candidates.push(range);
            }
        }
        asr::print_message(&format!(
            "[table] {} references to {anchor}, in {} candidate ranges.",
            holders.found.len(),
            candidates.len(),
        ));

        for (base, size) in candidates {
            // The range the anchor itself sits in is a heap section by
            // definition -- an object is in it -- so it needs no statistics.
            if base <= anchor && anchor < base.add(size) {
                continue;
            }
            let headers =
                header_words(process, base, size, metadata, MAX_HEADER_WORDS, &mut on_tick).await;
            if headers > MAX_HEADER_WORDS {
                continue;
            }
            asr::print_message(&format!(
                "[table] found at {base}, {} KiB, {headers} object headers in it.",
                size >> 10,
            ));
            return Some(Self { base, size });
        }
        asr::print_message("[table] no candidate range was one; sweeps it is.");
        None
    }

    /// Whether the table is still mapped where it was found.
    ///
    /// One read. A table that has gone is worth dropping rather than paying a
    /// failed lookup for on every scene change.
    pub fn still_mapped(&self, process: &Process) -> bool {
        process.read::<u64>(self.base).is_ok()
    }

    /// Every live instance of the class with this vtable, up to `limit`.
    ///
    /// Reads the table, then the pages its entries point at, and keeps the
    /// entries whose object has the vtable asked for. Validation is the same
    /// one a sweep applies -- the entries are real objects, so it rarely has
    /// anything to reject, but the check costs nothing next to the reads.
    pub async fn instances(
        &self,
        process: &Process,
        vtable: Address,
        validator: Validator,
        limit: usize,
        mut on_tick: impl FnMut(),
    ) -> Search {
        let ranges = writable_ranges(process);
        let Some(mut entries) = self.entries(process, &ranges, &mut on_tick).await else {
            return Search {
                instances: Vec::new(),
                readable: false,
            };
        };
        entries.sort_unstable();
        entries.dedup();

        let mut instances = Vec::new();
        let mut buf = vec![0u8; WINDOW as usize];
        let mut at = 0usize;
        let mut used = 0u64;
        while at < entries.len() {
            // Every entry inside one window, so a run of neighbouring objects
            // costs one read between them all.
            let start = entries[at] & !(WINDOW - 1);
            let end = start + WINDOW;
            let stop = entries[at..].partition_point(|&e| e < end) + at;

            if read_window(process, start, &mut buf) {
                for &object in &entries[at..stop] {
                    let offset = (object - start) as usize;
                    if offset + 8 > buf.len() {
                        continue;
                    }
                    let word = u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
                    let object = Address::new(object);
                    if word == vtable.value() && validator.accepts(process, object) {
                        instances.push(object);
                        if instances.len() >= limit {
                            return Search {
                                instances,
                                readable: true,
                            };
                        }
                    }
                }
            }

            at = stop;
            used += WINDOW;
            if used >= scan::DEFAULT_BUDGET {
                used = 0;
                on_tick();
                next_tick().await;
            }
        }
        Search {
            instances,
            readable: true,
        }
    }

    /// The table's entries that point at something writable, in table order.
    ///
    /// `None` if the table could not be read at all, which is the caller's cue
    /// to sweep instead.
    async fn entries(
        &self,
        process: &Process,
        ranges: &[(u64, u64)],
        on_tick: &mut impl FnMut(),
    ) -> Option<Vec<u64>> {
        let mut entries = Vec::new();
        let mut buf = vec![0u64; (WINDOW / 8) as usize];
        let mut read_any = false;
        let mut offset = 0u64;
        let mut used = 0u64;
        while offset < self.size {
            let words = (((self.size - offset) / 8) as usize).min(buf.len());
            let at = self.base.add(offset);
            if process
                .read_into_buf(at, bytemuck::cast_slice_mut(&mut buf[..words]))
                .is_ok()
            {
                read_any = true;
                for &value in &buf[..words] {
                    if value != 0 && value % 8 == 0 && mapped(ranges, value) {
                        entries.push(value);
                    }
                }
            }
            offset += (words * 8) as u64;
            used += (words * 8) as u64;
            // Yielded by budget rather than by window: a tick is worth about
            // 10ms, and a 2 MiB table read a window at a time would spend a
            // third of a second doing nothing else.
            if used >= scan::DEFAULT_BUDGET {
                used = 0;
                on_tick();
                next_tick().await;
            }
        }
        read_any.then_some(entries)
    }
}

/// Reads a window, falling back to pages so one unmapped page costs a page
/// rather than the whole window. Unreadable pages are left as zeros, which
/// cannot match a vtable.
fn read_window(process: &Process, start: u64, buf: &mut [u8]) -> bool {
    if process.read_into_buf(Address::new(start), buf).is_ok() {
        return true;
    }
    const PAGE: usize = 4096;
    let mut any = false;
    for (i, page) in buf.chunks_mut(PAGE).enumerate() {
        let at = Address::new(start + (i * PAGE) as u64);
        if process.read_into_buf(at, page).is_ok() {
            any = true;
        } else {
            page.fill(0);
        }
    }
    any
}

/// How many of a range's words point into `metadata`, i.e. look like object
/// headers. Stops counting once it passes `limit`, so a hundred-megabyte heap
/// section is not read to the end to be rejected.
async fn header_words(
    process: &Process,
    base: Address,
    size: u64,
    metadata: (u64, u64),
    limit: u32,
    on_tick: &mut impl FnMut(),
) -> u32 {
    let mut buf = vec![0u64; (WINDOW / 8) as usize];
    let mut headers = 0u32;
    let mut offset = 0u64;
    let mut used = 0u64;
    while offset < size {
        let words = (((size - offset) / 8) as usize).min(buf.len());
        let at = base.add(offset);
        if process
            .read_into_buf(at, bytemuck::cast_slice_mut(&mut buf[..words]))
            .is_ok()
        {
            for &value in &buf[..words] {
                if value % 8 == 0 && value >= metadata.0 && value < metadata.0 + metadata.1 {
                    headers += 1;
                    if headers > limit {
                        return headers;
                    }
                }
            }
        }
        offset += (words * 8) as u64;
        used += (words * 8) as u64;
        if used >= scan::DEFAULT_BUDGET {
            used = 0;
            on_tick();
            next_tick().await;
        }
    }
    headers
}

/// The mapped range an address falls in.
fn region_containing(process: &Process, address: Address) -> Option<(u64, u64)> {
    process.memory_ranges().find_map(|range| {
        let (base, size) = range.range().ok()?;
        let base = base.value();
        (base <= address.value() && address.value() < base + size).then_some((base, size))
    })
}

/// Every writable range, sorted, for deciding whether an entry points at
/// anything at all.
fn writable_ranges(process: &Process) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = process
        .memory_ranges()
        .filter(|range| {
            range
                .flags()
                .is_ok_and(|flags| flags.contains(MemoryRangeFlags::WRITE))
        })
        .filter_map(|range| range.range().ok().map(|(base, size)| (base.value(), size)))
        .collect();
    ranges.sort_unstable();
    ranges
}

fn mapped(ranges: &[(u64, u64)], value: u64) -> bool {
    match ranges.binary_search_by_key(&value, |&(base, _)| base) {
        Ok(_) => true,
        Err(0) => false,
        Err(i) => {
            let (base, size) = ranges[i - 1];
            value < base + size
        }
    }
}
