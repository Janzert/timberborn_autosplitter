//! Locating managed objects by scanning for instances of their class.
//!
//! Timberborn has no static roots into its Bindito DI container, so a service
//! cannot be reached by walking static fields. Every service is a singleton
//! though, so there is exactly one instance of each service class in the
//! process, and we can find it directly.
//!
//! Every managed object begins with a pointer to its class's vtable, so an
//! object is an instance of exactly one class iff its first pointer equals
//! that class's vtable address. Scanning for that value finds the instance
//! without needing any Unity-native offsets.
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

/// Give up after this much. A scan that gets this far has gone wrong, and we
/// would rather report that than stall the runtime indefinitely.
const MAX_BYTES: u64 = 8 << 30;

/// What a scan looked at. Logged by the spike to answer whether this approach
/// is viable at all.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    /// Ranges that passed the readable-writable filter and were scanned.
    pub ranges_scanned: u32,
    /// Ranges skipped by the filter.
    pub ranges_skipped: u32,
    /// Bytes actually read and compared.
    pub bytes_scanned: u64,
    /// Chunks whose read failed. Expected to be non-zero: the memory map can
    /// change under us mid-scan.
    pub read_failures: u32,
    /// Whether the scan stopped early on `limit` or [`MAX_BYTES`].
    pub truncated: bool,
}

/// Scans the target's writable memory for objects whose vtable pointer is
/// `vtable`, appending them to `out`. Stops after `limit` hits.
///
/// This is a blocking, whole-heap scan. Call it on attach and on scene change,
/// not per tick.
pub fn find_instances(
    process: &Process,
    vtable: Address,
    out: &mut Vec<Address>,
    limit: usize,
) -> Stats {
    let needle = vtable.value();
    let mut stats = Stats::default();
    let mut buf = vec![0u64; CHUNK_WORDS];

    for range in process.memory_ranges() {
        // The managed heap is readable and writable. Skipping executable pages
        // drops the code sections, which cannot hold objects.
        let Ok(flags) = range.flags() else {
            stats.ranges_skipped += 1;
            continue;
        };
        let wanted = MemoryRangeFlags::READ | MemoryRangeFlags::WRITE;
        if !flags.contains(wanted) || flags.contains(MemoryRangeFlags::EXECUTE) {
            stats.ranges_skipped += 1;
            continue;
        }
        let Ok((base, size)) = range.range() else {
            stats.ranges_skipped += 1;
            continue;
        };

        stats.ranges_scanned += 1;

        let mut offset = 0u64;
        while offset < size {
            if stats.bytes_scanned >= MAX_BYTES {
                stats.truncated = true;
                return stats;
            }

            let remaining = size - offset;
            let words = (remaining / 8).min(CHUNK_WORDS as u64) as usize;
            if words == 0 {
                break;
            }

            let chunk = &mut buf[..words];
            if process
                .read_into_buf(base.add(offset), bytemuck::cast_slice_mut(chunk))
                .is_err()
            {
                // The map can change while we walk it. Skip the chunk rather
                // than abandoning the range.
                stats.read_failures += 1;
                offset += (words * 8) as u64;
                continue;
            }

            stats.bytes_scanned += (words * 8) as u64;

            for (i, &word) in chunk.iter().enumerate() {
                if word == needle {
                    out.push(base.add(offset + (i * 8) as u64));
                    if out.len() >= limit {
                        stats.truncated = true;
                        return stats;
                    }
                }
            }

            offset += (words * 8) as u64;
        }
    }

    stats
}
