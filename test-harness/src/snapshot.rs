//! Capturing a real process's memory, and serving it back offline.
//!
//! A snapshot is a directory holding a text manifest and one blob of raw bytes.
//! It may also be a *delta*: a capture that names another as its `base` and
//! stores only the [`CHUNK`]-sized pieces that differ from it, which is what
//! makes a recorded sequence of captures cost hundreds of MiB a step instead of
//! gigabytes apiece. Reads fall through to the base for anything not stored.
//! It is the *oracle* rather than the deliverable: it shows what the game's
//! memory actually looks like, which is what keeps a synthesized fixture from
//! enshrining a misunderstanding. See TEST_HARNESS_PLAN.md in the parent repo.
//!
//! Snapshots are never committed -- they are large, and they are a copy of the
//! game's own data. `snapshots/` in this repo ignores everything but its own
//! `.gitignore` and `README.md`.
//!
//! # A snapshot is not an instant
//!
//! Capture reads range by range from a process that is still running, so the
//! last range is seconds younger than the first -- 5.6s across 5 GiB, measured.
//! Nothing pauses the game. Almost everything survives that, because the
//! objects the splitter reads are not being rewritten, but a snapshot *can*
//! hold a combination of values the game never actually had at any one moment.
//! Where a test turns on two values agreeing with each other, that is worth
//! remembering before concluding the splitter read one of them wrongly.

use std::{
    fmt::Write as _,
    fs::File,
    io::{self, BufWriter, Write},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use crate::memory::{FakeProcess, Memory, MemoryRange, ModuleInfo};

const MANIFEST: &str = "manifest.txt";
const BLOB: &str = "memory.bin";
const CHUNKS: &str = "chunks.idx";
const FORMAT_VERSION: u32 = 2;

/// Granularity of a delta capture, chosen by measurement rather than taste.
///
/// Between two captures of one idle state seconds apart, the fraction of memory
/// that differs is 5.4% at 4 KiB, 14.7% at 64 KiB and 25.5% at 1 MiB -- against
/// index sizes of 71255, 12140 and 1408 records. 64 KiB is the knee: a few
/// hundred MiB a step, and an index small enough to read in one go.
pub const CHUNK: u64 = 64 << 10;

/// Where snapshots live by default: `<repo>/snapshots`, so a fresh clone works
/// with no configuration. `TIMBERBORN_SNAPSHOTS` overrides it, which matters
/// because these run to gigabytes.
pub fn default_store() -> PathBuf {
    match std::env::var_os("TIMBERBORN_SNAPSHOTS") {
        Some(path) => PathBuf::from(path),
        // The harness lives one directory below the repo root.
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the harness crate always has a parent directory")
            .join("snapshots"),
    }
}

/// Reads only a capture's manifest, to see what it is without opening 5 GiB.
fn peek(dir: &Path) -> Option<Metadata> {
    Snapshot::open(dir).ok().map(|s| s.metadata)
}

/// Every step of a recorded scenario, in order.
///
/// A scenario is found the same way a single capture is -- by the state it is
/// of, never by a path -- and its steps are gathered by the scenario name they
/// share rather than by their directory names.
///
/// # Errors
///
/// With the instructions for recording it, and if the steps are there but do
/// not run 0, 1, 2..., saying so: a gap means a step was deleted or a recording
/// was interrupted mid-capture, and replaying across the hole would show the
/// splitter a jump that never happened.
pub fn find_scenario(requirement_id: &str) -> Result<Vec<PathBuf>, String> {
    Ok(find_scenarios(requirement_id)?.swap_remove(0).1)
}

/// Every recorded scenario satisfying the requirement, named, each in order.
///
/// More than one is the useful case rather than an awkward one: a run as
/// Folktails and a run as Iron Teeth are the same category down different code,
/// and the same assertions should hold for both. Grouped by the scenario name
/// its steps share, so two recordings cannot be spliced into one nonsense run.
pub fn find_scenarios(requirement_id: &str) -> Result<Vec<(String, Vec<PathBuf>)>, String> {
    let mut grouped: Vec<(String, Vec<(u32, PathBuf)>)> = Vec::new();
    for dir in find_all(requirement_id)? {
        let Some(metadata) = peek(&dir) else { continue };
        let (Some(name), Some(step)) = (metadata.scenario.clone(), metadata.step.clone()) else {
            continue;
        };
        match grouped.iter_mut().find(|(existing, _)| *existing == name) {
            Some((_, steps)) => steps.push((step.index, dir)),
            None => grouped.push((name, vec![(step.index, dir)])),
        }
    }

    if grouped.is_empty() {
        let requirement = crate::requirement::get(requirement_id)
            .ok_or_else(|| format!("no such requirement {requirement_id:?}"))?;
        return Err(format!(
            "No recorded scenario satisfies {requirement_id:?} ({}).\n\nTo record one:\n{}\n  \
             - then, with the game running and the run about to be played:\n      \
             tb-record --state {requirement_id} --notes '<what you did>'\n\n\
             See snapshots/README.md.",
            requirement.summary,
            requirement
                .reproduce
                .iter()
                .map(|step| format!("  - {step}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    let mut scenarios = Vec::new();
    for (name, mut steps) in grouped {
        steps.sort_unstable_by_key(|(index, _)| *index);
        for (position, (index, dir)) in steps.iter().enumerate() {
            if *index != position as u32 {
                return Err(format!(
                    "the recording {name:?} is missing step {position}: it jumps to \
                     {index} at {}. Replaying across the gap would show the splitter a \
                     change that never happened, so this is refused rather than patched \
                     over.",
                    dir.display()
                ));
            }
        }
        scenarios.push((name, steps.into_iter().map(|(_, dir)| dir).collect()));
    }
    scenarios.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(scenarios)
}

/// One capture of the state per distinct game version, preferring a frozen one.
///
/// What a cross-version test wants: the same assertions against every build
/// that has been captured, without paying twice for two captures of the same
/// build -- a suite that takes half a minute a capture stops being run.
pub fn find_per_version(requirement_id: &str) -> Result<Vec<PathBuf>, String> {
    let mut chosen: Vec<(String, PathBuf, bool)> = Vec::new();
    for dir in find_all(requirement_id)? {
        let Some(metadata) = peek(&dir) else { continue };
        match chosen
            .iter_mut()
            .find(|(version, _, _)| *version == metadata.game_version)
        {
            // A frozen capture is the better evidence, so it wins the slot.
            Some(slot) if metadata.frozen && !slot.2 => {
                slot.1 = dir;
                slot.2 = true;
            }
            Some(_) => {}
            None => chosen.push((metadata.game_version, dir, metadata.frozen)),
        }
    }
    Ok(chosen.into_iter().map(|(_, dir, _)| dir).collect())
}

/// Every capture of the state, newest game version last.
///
/// Running a test against all of them is what makes two captured builds worth
/// having: the same assertions across versions is a cross-version regression
/// check, and it stops a test quietly pinning itself to one build's behaviour.
pub fn find_all(requirement_id: &str) -> Result<Vec<PathBuf>, String> {
    let requirement = crate::requirement::get(requirement_id).ok_or_else(|| {
        format!(
            "no such requirement {requirement_id:?}. Known states:\n{}",
            crate::requirement::listing()
        )
    })?;

    // A store that does not exist yet is just a store with nothing in it. It
    // must not short-circuit past the instructions below -- being told the
    // directory is missing is exactly as useless as being told a file is.
    let store = default_store();
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&store)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    candidates.sort();

    let candidate_count = candidates.len();
    let found: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|dir| peek(dir).is_some_and(|m| m.satisfies.iter().any(|id| id == requirement_id)))
        .collect();

    if !found.is_empty() {
        return Ok(found);
    }
    Err({
        format!(
            "{}\n\nLooked in {}, which holds {} capture(s).",
            requirement.instructions(),
            store.display(),
            candidate_count
        )
    })
}

/// What was captured, alongside enough provenance to know what it is a snapshot
/// *of*. A snapshot with no build id is worthless a month later.
#[derive(Default, Clone)]
pub struct Metadata {
    /// The game's own version, e.g. `1.1.2.4-52e959e-sw`. What identifies a
    /// capture, and what `steam_versions` names its saves after, so the two
    /// stores can be paired.
    pub game_version: String,
    /// Steam's build id, where it could be established. Provenance only:
    /// a version run outside Steam has none that Steam knows about.
    pub build_id: String,
    pub label: String,
    pub captured_at: String,
    pub process_name: String,
    pub process_path: Option<String>,
    pub pid: u32,
    /// Free text: which faction, how far into the run, what was on screen.
    pub notes: String,
    /// Whether the game was stopped for the read. A capture that cannot say
    /// whether it is consistent is one nobody can trust later.
    pub frozen: bool,
    /// Which [`Requirement`](crate::requirement::Requirement) ids this capture
    /// is of. What tests search on, rather than the directory name.
    pub satisfies: Vec<String>,
    /// The capture this one stores differences against, by directory name.
    /// `None` for a full capture.
    pub base: Option<String>,
    /// The recording this capture is a step of. Steps of one scenario share it,
    /// which is what gathers them back into an ordered sequence.
    pub scenario: Option<String>,
    /// Position in a recorded sequence, and what happened at it. Empty for a
    /// capture that is not part of one.
    pub step: Option<Step>,
}

/// One moment in a recorded sequence, and why it was captured.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub index: u32,
    /// What the splitter did that made this a moment worth keeping --
    /// `start`, `split:Forester`, `reset` -- or `begin` for the state before
    /// anything happened.
    pub event: String,
}

/// A captured range and where its bytes sit in the blob.
struct Stored {
    range: MemoryRange,
    /// `None` when the range was reported but could not be read, which happens
    /// and is better recorded than silently dropped.
    offset: Option<u64>,
}

/// A [`CHUNK`]-aligned run of bytes held in this capture's own blob.
#[derive(Clone, Copy)]
struct Chunk {
    address: u64,
    len: u64,
    offset: u64,
}

/// Writes a snapshot directory.
pub struct Writer {
    dir: PathBuf,
    blob: BufWriter<File>,
    written: u64,
    modules: Vec<ModuleInfo>,
    stored: Vec<Stored>,
    metadata: Metadata,
    chunks: Vec<Chunk>,
    /// When set, only chunks differing from this are written; everything else
    /// is read through to it at replay.
    base: Option<Snapshot>,
    pub skipped: u64,
    pub inherited: u64,
}

impl Writer {
    pub fn create(dir: PathBuf, metadata: Metadata) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            blob: BufWriter::new(File::create(dir.join(BLOB))?),
            dir,
            written: 0,
            modules: Vec::new(),
            stored: Vec::new(),
            metadata,
            chunks: Vec::new(),
            base: None,
            skipped: 0,
            inherited: 0,
        })
    }

    /// Stores only what differs from `base`. The base's own bytes are never
    /// copied, so a sequence costs one full capture and a delta a step.
    pub fn with_base(mut self, base: Snapshot) -> Self {
        self.metadata.base = Some(base.directory_name.clone());
        self.base = Some(base);
        self
    }

    pub fn set_step(&mut self, index: u32, event: impl Into<String>) {
        self.metadata.step = Some(Step {
            index,
            event: event.into(),
        });
    }

    pub fn add_modules(&mut self, modules: Vec<ModuleInfo>) {
        self.modules = modules;
    }

    /// Stores one range's bytes, or the parts of them that are new.
    ///
    /// A range that cannot be read is still recorded, without bytes, so replay
    /// reports "unreadable" rather than "absent".
    pub fn add_range(&mut self, range: MemoryRange, bytes: Option<&[u8]>) -> io::Result<()> {
        let Some(bytes) = bytes else {
            self.skipped += 1;
            self.stored.push(Stored {
                range,
                offset: None,
            });
            return Ok(());
        };

        match &self.base {
            // Chunk by chunk against the base, keeping only what changed.
            Some(base) => {
                let mut comparison = vec![0u8; CHUNK as usize];
                let mut at = 0u64;
                while at < bytes.len() as u64 {
                    let len = CHUNK.min(bytes.len() as u64 - at);
                    let fresh = &bytes[at as usize..(at + len) as usize];
                    let address = range.address + at;
                    let same = base.read(address, &mut comparison[..len as usize])
                        && comparison[..len as usize] == *fresh;
                    if same {
                        self.inherited += 1;
                    } else {
                        let offset = self.written;
                        self.blob.write_all(fresh)?;
                        self.written += len;
                        self.chunks.push(Chunk {
                            address,
                            len,
                            offset,
                        });
                    }
                    at += len;
                }
                // The range table is still this capture's own: a mapping can
                // appear or vanish between steps.
                self.stored.push(Stored {
                    range,
                    offset: Some(0),
                });
            }
            None => {
                let offset = self.written;
                self.blob.write_all(bytes)?;
                self.written += bytes.len() as u64;
                self.chunks.push(Chunk {
                    address: range.address,
                    len: bytes.len() as u64,
                    offset,
                });
                self.stored.push(Stored {
                    range,
                    offset: Some(offset),
                });
            }
        }
        Ok(())
    }

    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    pub fn finish(mut self) -> io::Result<PathBuf> {
        self.blob.flush()?;

        let mut manifest = String::new();
        let m = &self.metadata;
        writeln!(manifest, "version {FORMAT_VERSION}").unwrap();
        writeln!(manifest, "game_version {}", m.game_version).unwrap();
        writeln!(manifest, "build_id {}", m.build_id).unwrap();
        writeln!(manifest, "label {}", m.label).unwrap();
        writeln!(manifest, "captured_at {}", m.captured_at).unwrap();
        writeln!(manifest, "pid {}", m.pid).unwrap();
        writeln!(manifest, "process_name {}", m.process_name).unwrap();
        writeln!(manifest, "frozen {}", if m.frozen { "yes" } else { "no" }).unwrap();
        if let Some(base) = &m.base {
            writeln!(manifest, "base {base}").unwrap();
        }
        if let Some(scenario) = &m.scenario {
            writeln!(manifest, "scenario {scenario}").unwrap();
        }
        if let Some(step) = &m.step {
            writeln!(manifest, "step {} {}", step.index, step.event).unwrap();
        }
        for id in &m.satisfies {
            writeln!(manifest, "satisfies {id}").unwrap();
        }
        if let Some(path) = &m.process_path {
            writeln!(manifest, "process_path {path}").unwrap();
        }
        for line in m.notes.lines() {
            writeln!(manifest, "note {line}").unwrap();
        }
        for module in &self.modules {
            writeln!(
                manifest,
                "module {:x} {:x} {}",
                module.address, module.size, module.name
            )
            .unwrap();
        }
        for stored in &self.stored {
            // The mark is presence, not a location: where the bytes are is the
            // chunk index's business, and in a delta most of them are not here
            // at all.
            let mark = if stored.offset.is_some() { "y" } else { "-" };
            writeln!(
                manifest,
                "range {:x} {:x} {} {mark}",
                stored.range.address, stored.range.size, stored.range.flags
            )
            .unwrap();
        }
        std::fs::write(self.dir.join(MANIFEST), manifest)?;

        // A fixed-width binary index: 12140 records for a delta step, which is
        // too many for a text manifest anyone is meant to read.
        let mut index = Vec::with_capacity(self.chunks.len() * 24);
        for chunk in &self.chunks {
            index.extend_from_slice(&chunk.address.to_le_bytes());
            index.extend_from_slice(&chunk.len.to_le_bytes());
            index.extend_from_slice(&chunk.offset.to_le_bytes());
        }
        std::fs::write(self.dir.join(CHUNKS), index)?;
        Ok(self.dir)
    }
}

/// A snapshot on disk, served as an address space.
pub struct Snapshot {
    blob: File,
    /// This capture's own chunks, sorted by address, so a read is a binary
    /// search. In a full capture there is one per range; in a delta, only the
    /// pieces that changed.
    chunks: Vec<Chunk>,
    /// Ranges as the runtime should report them, sorted, with a flag for
    /// whether this capture could read them at all.
    stored: Vec<(u64, u64, bool)>,
    /// What this capture stores differences against. Reads fall through to it.
    base: Option<Box<Snapshot>>,
    /// The directory's own name, which is how a delta names its base.
    pub directory_name: String,
    pub modules: Vec<ModuleInfo>,
    pub ranges: Vec<MemoryRange>,
    pub metadata: Metadata,
}

impl Snapshot {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = dir.as_ref();
        let manifest = std::fs::read_to_string(dir.join(MANIFEST))?;
        let blob = File::open(dir.join(BLOB))?;

        let mut metadata = Metadata::default();
        let mut modules = Vec::new();
        let mut ranges = Vec::new();
        let mut stored = Vec::new();
        let mut notes: Vec<&str> = Vec::new();

        for line in manifest.lines() {
            let Some((key, rest)) = line.split_once(' ') else {
                continue;
            };
            match key {
                "version" => {
                    let found: u32 = rest.trim().parse().unwrap_or(0);
                    if found != FORMAT_VERSION {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{} is format version {found}; this build reads {FORMAT_VERSION}. \
                                 Recapture it rather than guessing at the difference.",
                                dir.display()
                            ),
                        ));
                    }
                }
                "game_version" => metadata.game_version = rest.into(),
                "build_id" => metadata.build_id = rest.into(),
                "label" => metadata.label = rest.into(),
                "captured_at" => metadata.captured_at = rest.into(),
                "pid" => metadata.pid = rest.trim().parse().unwrap_or(0),
                "process_name" => metadata.process_name = rest.into(),
                "frozen" => metadata.frozen = rest.trim() == "yes",
                "satisfies" => metadata.satisfies.push(rest.trim().into()),
                "base" => metadata.base = Some(rest.trim().into()),
                "scenario" => metadata.scenario = Some(rest.trim().into()),
                "step" => {
                    let (index, event) = rest.trim().split_once(' ').unwrap_or((rest.trim(), ""));
                    metadata.step = Some(Step {
                        index: index.parse().unwrap_or(0),
                        event: event.into(),
                    });
                }
                "process_path" => metadata.process_path = Some(rest.into()),
                "note" => notes.push(rest),
                "module" => {
                    let mut f = rest.splitn(3, ' ');
                    let address =
                        u64::from_str_radix(f.next().unwrap_or_default(), 16).unwrap_or(0);
                    let size = u64::from_str_radix(f.next().unwrap_or_default(), 16).unwrap_or(0);
                    modules.push(ModuleInfo {
                        name: f.next().unwrap_or_default().into(),
                        address,
                        size,
                        path: None,
                    });
                }
                "range" => {
                    let mut f = rest.split(' ');
                    let address =
                        u64::from_str_radix(f.next().unwrap_or_default(), 16).unwrap_or(0);
                    let size = u64::from_str_radix(f.next().unwrap_or_default(), 16).unwrap_or(0);
                    let flags = f.next().unwrap_or_default().parse().unwrap_or(0);
                    let readable = !matches!(f.next(), Some("-") | None);
                    ranges.push(MemoryRange {
                        address,
                        size,
                        flags,
                    });
                    stored.push((address, size, readable));
                }
                _ => {}
            }
        }
        metadata.notes = notes.join("\n");
        stored.sort_unstable_by_key(|&(address, _, _)| address);

        // A delta names its base as a sibling directory, so a whole sequence
        // moves or is copied as one unit.
        let base = match &metadata.base {
            Some(name) => {
                let parent = dir.parent().unwrap_or(Path::new("."));
                Some(Box::new(Snapshot::open(parent.join(name)).map_err(
                    |e| {
                        io::Error::new(
                            e.kind(),
                            format!(
                                "{} stores differences against {name}, which could not be \
                             opened: {e}. A delta is unreadable without its base.",
                                dir.display()
                            ),
                        )
                    },
                )?))
            }
            None => None,
        };

        let raw = std::fs::read(dir.join(CHUNKS)).unwrap_or_default();
        let mut chunks: Vec<Chunk> = raw
            .as_chunks::<24>()
            .0
            .iter()
            .map(|record| Chunk {
                address: u64::from_le_bytes(record[0..8].try_into().unwrap()),
                len: u64::from_le_bytes(record[8..16].try_into().unwrap()),
                offset: u64::from_le_bytes(record[16..24].try_into().unwrap()),
            })
            .collect();
        chunks.sort_unstable_by_key(|c| c.address);

        Ok(Self {
            blob,
            chunks,
            stored,
            base,
            directory_name: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            modules,
            ranges,
            metadata,
        })
    }

    /// The chunk holding `address`, if this capture has one.
    fn chunk_at(&self, address: u64) -> Option<Chunk> {
        let index = match self.chunks.binary_search_by_key(&address, |c| c.address) {
            Ok(index) => index,
            Err(0) => return None,
            Err(index) => index - 1,
        };
        let chunk = self.chunks[index];
        (address < chunk.address + chunk.len).then_some(chunk)
    }

    /// Where the next chunk after `address` begins, for sizing a read that has
    /// to fall through to the base.
    fn next_chunk_after(&self, address: u64) -> Option<u64> {
        let index = self.chunks.partition_point(|c| c.address <= address);
        self.chunks.get(index).map(|c| c.address)
    }

    /// Satisfies a read from this capture's own bytes and its base's, without
    /// re-checking range containment -- the outermost capture has done that.
    fn read_through(&self, address: u64, buf: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < buf.len() {
            let at = address + done as u64;
            let remaining = buf.len() - done;
            match self.chunk_at(at) {
                Some(chunk) => {
                    let available = (chunk.address + chunk.len - at) as usize;
                    let take = available.min(remaining);
                    let offset = chunk.offset + (at - chunk.address);
                    if self
                        .blob
                        .read_exact_at(&mut buf[done..done + take], offset)
                        .is_err()
                    {
                        return false;
                    }
                    done += take;
                }
                None => {
                    let Some(base) = &self.base else {
                        return false;
                    };
                    // Only up to the next chunk we do hold, so a read spanning
                    // changed and unchanged memory is stitched from both.
                    let boundary = self.next_chunk_after(at);
                    let take = boundary
                        .map(|next| (next - at) as usize)
                        .unwrap_or(remaining)
                        .min(remaining);
                    if !base.read_through(at, &mut buf[done..done + take]) {
                        return false;
                    }
                    done += take;
                }
            }
        }
        true
    }

    /// The captured process, ready to hand to a [`World`](crate::World).
    pub fn process(self) -> FakeProcess {
        let mut process =
            FakeProcess::new(self.metadata.pid as u64, self.metadata.process_name.clone());
        process.path = self.metadata.process_path.clone();
        process.modules = self.modules.clone();
        process.ranges = self.ranges.clone();
        process.memory = Box::new(self);
        process
    }

    /// Bytes this capture holds itself, excluding anything inherited.
    pub fn captured_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.len).sum()
    }

    /// How many captures deep the chain runs, this one included.
    pub fn chain_length(&self) -> usize {
        1 + self.base.as_ref().map_or(0, |b| b.chain_length())
    }
}

impl Memory for Snapshot {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        // Range containment first, and against *this* capture's range table:
        // a read spanning two mappings fails as a whole, which is what the real
        // runtime does. Where the bytes then come from -- here or a base -- is
        // a storage question, not a semantic one.
        let index = match self
            .stored
            .binary_search_by_key(&address, |&(start, _, _)| start)
        {
            Ok(index) => index,
            Err(0) => return false,
            Err(index) => index - 1,
        };
        let (start, size, readable) = self.stored[index];
        if !readable || address - start + buf.len() as u64 > size {
            return false;
        }
        self.read_through(address, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::flags;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tb-snapshot-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write_sample(dir: &Path) {
        let metadata = Metadata {
            game_version: "1.1.2.4-52e959e-sw".into(),
            build_id: "25096761".into(),
            label: "sample".into(),
            captured_at: "0".into(),
            process_name: "Unity Main Thre".into(),
            process_path: Some("/games/Timberborn.exe".into()),
            pid: 4242,
            notes: "folktails\nday 3".into(),
            frozen: true,
            satisfies: vec!["run-finished".into(), "main-menu".into()],
            ..Default::default()
        };
        let mut writer = Writer::create(dir.to_path_buf(), metadata).unwrap();
        writer.add_modules(vec![ModuleInfo {
            name: "mono-2.0-bdwgc.dll".into(),
            address: 0x7000_0000,
            size: 0x30_0000,
            path: None,
        }]);
        writer
            .add_range(
                MemoryRange {
                    address: 0x1000,
                    size: 4,
                    flags: flags::HEAP,
                },
                Some(&[1, 2, 3, 4]),
            )
            .unwrap();
        // Deliberately out of address order: the reader must sort.
        writer
            .add_range(
                MemoryRange {
                    address: 0x9000,
                    size: 4,
                    flags: flags::READ,
                },
                Some(&[9, 9, 9, 9]),
            )
            .unwrap();
        writer
            .add_range(
                MemoryRange {
                    address: 0x5000,
                    size: 8,
                    flags: flags::HEAP,
                },
                Some(&[5, 6, 7, 8, 9, 10, 11, 12]),
            )
            .unwrap();
        // Listed but unreadable, which really happens during a capture.
        writer
            .add_range(
                MemoryRange {
                    address: 0x7000,
                    size: 16,
                    flags: flags::READ,
                },
                None,
            )
            .unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn round_trips_metadata_and_modules() {
        let dir = temp_dir("metadata");
        write_sample(&dir);
        let snapshot = Snapshot::open(&dir).unwrap();

        assert_eq!(snapshot.metadata.game_version, "1.1.2.4-52e959e-sw");
        assert_eq!(snapshot.metadata.build_id, "25096761");
        assert_eq!(snapshot.metadata.pid, 4242);
        assert_eq!(snapshot.metadata.process_name, "Unity Main Thre");
        assert_eq!(snapshot.metadata.notes, "folktails\nday 3");
        // What searching runs on, so it has to survive the round trip.
        assert!(snapshot.metadata.frozen);
        assert_eq!(snapshot.metadata.satisfies, ["run-finished", "main-menu"]);
        assert_eq!(snapshot.modules.len(), 1);
        assert_eq!(snapshot.modules[0].address, 0x7000_0000);
        assert_eq!(snapshot.ranges.len(), 4);
        assert_eq!(snapshot.captured_bytes(), 16);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn serves_the_bytes_back_at_their_addresses() {
        let dir = temp_dir("read");
        write_sample(&dir);
        let snapshot = Snapshot::open(&dir).unwrap();

        let mut buf = [0u8; 4];
        assert!(snapshot.read(0x1000, &mut buf));
        assert_eq!(buf, [1, 2, 3, 4]);

        // A range written after a higher one still reads correctly, which is
        // the whole point of sorting on load.
        assert!(snapshot.read(0x9000, &mut buf));
        assert_eq!(buf, [9, 9, 9, 9]);

        // Partway into a range.
        let mut buf = [0u8; 2];
        assert!(snapshot.read(0x5002, &mut buf));
        assert_eq!(buf, [7, 8]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_reads_it_cannot_honour() {
        let dir = temp_dir("refuse");
        write_sample(&dir);
        let snapshot = Snapshot::open(&dir).unwrap();
        let mut buf = [0u8; 4];

        assert!(!snapshot.read(0x0, &mut buf), "below every range");
        assert!(!snapshot.read(0x2000, &mut buf), "in a gap between ranges");
        assert!(!snapshot.read(0xF000, &mut buf), "above every range");
        assert!(
            !snapshot.read(0x1002, &mut buf),
            "running off the end of a range, rather than reading a neighbour's bytes"
        );
        assert!(
            !snapshot.read(0x7000, &mut buf),
            "a range that was listed but could not be captured"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod delta_tests {
    use super::*;
    use crate::memory::flags;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tb-delta-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn metadata(label: &str) -> Metadata {
        Metadata {
            game_version: "1.1.2.4-52e959e-sw".into(),
            build_id: "25096761".into(),
            label: label.into(),
            frozen: true,
            satisfies: vec!["run-finished".into()],
            ..Default::default()
        }
    }

    /// One range, spanning several chunks, so a delta can change some and not
    /// others.
    fn body(fill: u8, mutate: &[(usize, u8)]) -> Vec<u8> {
        let mut bytes = vec![fill; (CHUNK * 3) as usize];
        for &(at, value) in mutate {
            bytes[at] = value;
        }
        bytes
    }

    const AT: u64 = 0x10_0000;

    fn write(dir: &Path, bytes: &[u8], base: Option<Snapshot>, label: &str) {
        let mut writer = Writer::create(dir.to_path_buf(), metadata(label)).unwrap();
        if let Some(base) = base {
            writer = writer.with_base(base);
        }
        writer
            .add_range(
                MemoryRange {
                    address: AT,
                    size: bytes.len() as u64,
                    flags: flags::HEAP,
                },
                Some(bytes),
            )
            .unwrap();
        writer.finish().unwrap();
    }

    /// The point of the whole exercise: a step stores only what changed, and
    /// still reads back as the complete address space.
    #[test]
    fn a_delta_stores_only_what_changed_and_reads_back_whole() {
        let parent = dir("chain");
        std::fs::create_dir_all(&parent).unwrap();

        let first = body(0xAA, &[]);
        write(&parent.join("base"), &first, None, "base");

        // One byte, in the middle chunk.
        let second = body(0xAA, &[(CHUNK as usize + 5, 0x42)]);
        let base = Snapshot::open(parent.join("base")).unwrap();
        write(&parent.join("step1"), &second, Some(base), "step1");

        let step = Snapshot::open(parent.join("step1")).unwrap();
        assert_eq!(step.chain_length(), 2);
        assert_eq!(
            step.captured_bytes(),
            CHUNK,
            "only the one changed chunk should have been stored"
        );

        // Reads come out whole regardless of which capture holds them.
        let mut buf = vec![0u8; second.len()];
        assert!(step.read(AT, &mut buf));
        assert_eq!(buf, second);

        std::fs::remove_dir_all(&parent).unwrap();
    }

    /// A read crossing the boundary between a changed chunk and an inherited
    /// one has to be stitched from both. This is the case a naive
    /// "chunk or base" lookup gets wrong.
    #[test]
    fn a_read_spanning_stored_and_inherited_bytes_is_stitched() {
        let parent = dir("stitch");
        std::fs::create_dir_all(&parent).unwrap();

        let first = body(0x11, &[]);
        write(&parent.join("base"), &first, None, "base");
        let second = body(0x11, &[(CHUNK as usize, 0x99)]);
        let base = Snapshot::open(parent.join("base")).unwrap();
        write(&parent.join("step1"), &second, Some(base), "step1");

        let step = Snapshot::open(parent.join("step1")).unwrap();
        let mut buf = vec![0u8; 16];
        // Straddles the end of chunk 0 (inherited) and the start of chunk 1
        // (stored).
        assert!(step.read(AT + CHUNK - 8, &mut buf));
        assert_eq!(&buf[..8], &[0x11; 8]);
        assert_eq!(buf[8], 0x99);
        assert_eq!(&buf[9..], &[0x11; 7]);

        std::fs::remove_dir_all(&parent).unwrap();
    }

    /// Deltas stack, so a recorded sequence is one full capture and a tail of
    /// small ones.
    #[test]
    fn deltas_chain_through_several_steps() {
        let parent = dir("stack");
        std::fs::create_dir_all(&parent).unwrap();

        write(&parent.join("base"), &body(0x00, &[]), None, "base");
        let one = body(0x00, &[(0, 1)]);
        write(
            &parent.join("step1"),
            &one,
            Some(Snapshot::open(parent.join("base")).unwrap()),
            "step1",
        );
        let two = body(0x00, &[(0, 1), (CHUNK as usize * 2 + 3, 2)]);
        write(
            &parent.join("step2"),
            &two,
            Some(Snapshot::open(parent.join("step1")).unwrap()),
            "step2",
        );

        let last = Snapshot::open(parent.join("step2")).unwrap();
        assert_eq!(last.chain_length(), 3);
        assert_eq!(last.captured_bytes(), CHUNK, "step2 changed one chunk");

        let mut buf = vec![0u8; two.len()];
        assert!(last.read(AT, &mut buf));
        assert_eq!(buf, two, "the first step's change must survive the second");

        std::fs::remove_dir_all(&parent).unwrap();
    }

    /// A delta without its base is not a capture with holes, it is unreadable,
    /// and it has to say so rather than serving stale bytes.
    #[test]
    fn a_delta_refuses_to_open_without_its_base() {
        let parent = dir("orphan");
        std::fs::create_dir_all(&parent).unwrap();
        write(&parent.join("base"), &body(0x7, &[]), None, "base");
        write(
            &parent.join("step1"),
            &body(0x7, &[(1, 9)]),
            Some(Snapshot::open(parent.join("base")).unwrap()),
            "step1",
        );
        std::fs::remove_dir_all(parent.join("base")).unwrap();

        let error = match Snapshot::open(parent.join("step1")) {
            Err(error) => error,
            Ok(_) => panic!("opened a delta whose base is gone"),
        };
        assert!(
            error.to_string().contains("stores differences against"),
            "unhelpful error: {error}"
        );

        std::fs::remove_dir_all(&parent).unwrap();
    }
}
