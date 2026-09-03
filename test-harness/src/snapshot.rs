//! Capturing a real process's memory, and serving it back offline.
//!
//! A snapshot is a directory holding a text manifest and one blob of raw bytes.
//! It is the *oracle* rather than the deliverable: it shows what the game's
//! memory actually looks like, which is what keeps a synthesized fixture from
//! enshrining a misunderstanding. See TEST_HARNESS_PLAN.md in the parent repo.
//!
//! Snapshots are never committed -- they are large, and they are a copy of the
//! game's own data. `snapshots/` in this repo ignores everything but its own
//! `.gitignore` and `README.md`.

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
const FORMAT_VERSION: u32 = 1;

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

/// What was captured, alongside enough provenance to know what it is a snapshot
/// *of*. A snapshot with no build id is worthless a month later.
#[derive(Default)]
pub struct Metadata {
    pub build_id: String,
    pub label: String,
    pub captured_at: String,
    pub process_name: String,
    pub process_path: Option<String>,
    pub pid: u32,
    /// Free text: which faction, how far into the run, what was on screen.
    pub notes: String,
}

/// A captured range and where its bytes sit in the blob.
struct Stored {
    range: MemoryRange,
    /// `None` when the range was reported but could not be read, which happens
    /// and is better recorded than silently dropped.
    offset: Option<u64>,
}

/// Writes a snapshot directory.
pub struct Writer {
    dir: PathBuf,
    blob: BufWriter<File>,
    written: u64,
    modules: Vec<ModuleInfo>,
    stored: Vec<Stored>,
    metadata: Metadata,
    pub skipped: u64,
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
            skipped: 0,
        })
    }

    pub fn add_modules(&mut self, modules: Vec<ModuleInfo>) {
        self.modules = modules;
    }

    /// Stores one range's bytes. A range that cannot be read is still recorded,
    /// without bytes, so replay reports "unreadable" rather than "unknown".
    pub fn add_range(&mut self, range: MemoryRange, bytes: Option<&[u8]>) -> io::Result<()> {
        let offset = match bytes {
            Some(bytes) => {
                let at = self.written;
                self.blob.write_all(bytes)?;
                self.written += bytes.len() as u64;
                Some(at)
            }
            None => {
                self.skipped += 1;
                None
            }
        };
        self.stored.push(Stored { range, offset });
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
        writeln!(manifest, "build_id {}", m.build_id).unwrap();
        writeln!(manifest, "label {}", m.label).unwrap();
        writeln!(manifest, "captured_at {}", m.captured_at).unwrap();
        writeln!(manifest, "pid {}", m.pid).unwrap();
        writeln!(manifest, "process_name {}", m.process_name).unwrap();
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
            match stored.offset {
                Some(offset) => writeln!(
                    manifest,
                    "range {:x} {:x} {} {:x}",
                    stored.range.address, stored.range.size, stored.range.flags, offset
                ),
                None => writeln!(
                    manifest,
                    "range {:x} {:x} {} -",
                    stored.range.address, stored.range.size, stored.range.flags
                ),
            }
            .unwrap();
        }
        std::fs::write(self.dir.join(MANIFEST), manifest)?;
        Ok(self.dir)
    }
}

/// A snapshot on disk, served as an address space.
pub struct Snapshot {
    blob: File,
    /// Sorted by address, so a read is a binary search.
    stored: Vec<(u64, u64, Option<u64>)>,
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
                "build_id" => metadata.build_id = rest.into(),
                "label" => metadata.label = rest.into(),
                "captured_at" => metadata.captured_at = rest.into(),
                "pid" => metadata.pid = rest.trim().parse().unwrap_or(0),
                "process_name" => metadata.process_name = rest.into(),
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
                    let offset = match f.next() {
                        Some("-") | None => None,
                        Some(hex) => u64::from_str_radix(hex, 16).ok(),
                    };
                    ranges.push(MemoryRange {
                        address,
                        size,
                        flags,
                    });
                    stored.push((address, size, offset));
                }
                _ => {}
            }
        }
        metadata.notes = notes.join("\n");
        stored.sort_unstable_by_key(|&(address, _, _)| address);

        Ok(Self {
            blob,
            stored,
            modules,
            ranges,
            metadata,
        })
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

    /// Total bytes actually captured.
    pub fn captured_bytes(&self) -> u64 {
        self.stored
            .iter()
            .filter(|(_, _, offset)| offset.is_some())
            .map(|&(_, size, _)| size)
            .sum()
    }
}

impl Memory for Snapshot {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        let index = match self
            .stored
            .binary_search_by_key(&address, |&(start, _, _)| start)
        {
            Ok(index) => index,
            Err(0) => return false,
            Err(index) => index - 1,
        };
        let (start, size, Some(offset)) = self.stored[index] else {
            return false;
        };
        let within = address - start;
        // A read spanning two ranges fails as a whole, which is what the real
        // runtime does: adjacent mappings are not one readable region.
        if within + buf.len() as u64 > size {
            return false;
        }
        self.blob.read_exact_at(buf, offset + within).is_ok()
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
            build_id: "25096761".into(),
            label: "sample".into(),
            captured_at: "0".into(),
            process_name: "Unity Main Thre".into(),
            process_path: Some("/games/Timberborn.exe".into()),
            pid: 4242,
            notes: "folktails\nday 3".into(),
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

        assert_eq!(snapshot.metadata.build_id, "25096761");
        assert_eq!(snapshot.metadata.pid, 4242);
        assert_eq!(snapshot.metadata.process_name, "Unity Main Thre");
        assert_eq!(snapshot.metadata.notes, "folktails\nday 3");
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
