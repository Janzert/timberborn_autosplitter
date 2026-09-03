//! Reading a live process into a capture, shared by `tb-dump` and `tb-record`.
//!
//! Both do the same thing at the moment they capture: stop the game, walk its
//! readable mappings, write what they read. Only the reason differs -- one is
//! asked to, the other is watching the splitter and captures when something
//! happens -- so the loop lives here rather than twice.

use std::io;

use crate::{
    freeze::Frozen,
    live::{self, LiveMemory},
    memory::Memory,
    snapshot::{Metadata, Snapshot, Writer},
};

/// Read in pieces rather than whole ranges: a range can be hundreds of MiB, and
/// one failing page should not discard all of it.
const READ_CHUNK: usize = 1 << 20;

pub struct Report {
    pub ranges: usize,
    pub bytes_written: u64,
    /// Ranges listed but unreadable, recorded without bytes.
    pub skipped: u64,
    /// Chunks that matched the base and were not stored again. Zero for a full
    /// capture, and the whole point of a delta.
    pub inherited: u64,
    pub frozen_for: std::time::Duration,
}

/// Captures `pid` into `dir`.
///
/// `base` makes this a delta: only chunks differing from it are stored. The
/// game is stopped for the read when `freeze` is set, which is the only way the
/// result is a single instant rather than a smear across several seconds.
pub fn capture(
    pid: u32,
    dir: std::path::PathBuf,
    metadata: Metadata,
    base: Option<Snapshot>,
    freeze: bool,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<Report> {
    let memory = LiveMemory::open(pid)?;
    let mappings = live::mappings(pid)?;
    let capturable: Vec<_> = mappings
        .iter()
        .filter(|m| m.readable() && !m.is_special())
        .collect();
    let total: u64 = capturable.iter().map(|m| m.size).sum();

    let mut writer = Writer::create(dir, metadata)?;
    writer.add_modules(live::modules(&mappings));
    if let Some(base) = base {
        writer = writer.with_base(base);
    }

    let started = std::time::Instant::now();
    let frozen = match freeze {
        true => Some(Frozen::stop(pid).map_err(io::Error::other)?),
        false => None,
    };

    let mut buffer = vec![0u8; READ_CHUNK];
    let mut done = 0u64;
    for mapping in &capturable {
        let mut bytes = Vec::with_capacity(mapping.size as usize);
        let mut readable = true;
        let mut at = 0u64;
        while at < mapping.size {
            let want = READ_CHUNK.min((mapping.size - at) as usize);
            if !memory.read(mapping.address + at, &mut buffer[..want]) {
                readable = false;
                break;
            }
            bytes.extend_from_slice(&buffer[..want]);
            at += want as u64;
        }
        writer.add_range(mapping.to_range(), readable.then_some(bytes.as_slice()))?;
        done += mapping.size;
        progress(done, total);
    }

    let frozen_for = started.elapsed();
    drop(frozen);

    let report = Report {
        ranges: capturable.len(),
        bytes_written: writer.bytes_written(),
        skipped: writer.skipped,
        inherited: writer.inherited,
        frozen_for,
    };
    writer.finish()?;
    Ok(report)
}
