//! Synthesising a process asr's Mono support can attach to.
//!
//! Not Mono. Only asr's model of Mono, which is very much smaller: a handful
//! of pointer chases defined by one row of
//! `vendor/asr/src/game_engine/unity/mono/offsets.rs`. Everything here exists
//! because something in `Module::attach`, `get_image`, `get_class` or
//! `get_field_offset` reads it.
//!
//! ```text
//!   UnityPlayer.dll ─ VS_FIXEDFILEINFO major 6000 ──────────► Version::V3
//!
//!   mono-2.0-bdwgc.dll
//!     export "mono_assembly_foreach" ─► 48 8B 0D <rel32> ──► assembly root
//!                                                                │
//!   assembly root ─► node[data,next] ─► MonoAssembly ─► MonoImage│
//!                                          │ aname        │ class cache
//!                                          ▼              ▼
//!                                       "Timberborn.X"  bucket[] ─► MonoClass
//!                                                                     │
//!            name · namespace · fields[] · runtime_info ─► vtable ─────┘
//! ```
//!
//! # The one row of offsets
//!
//! PE / `Version::V3` / 64-bit, which is what Timberborn is on both platforms
//! it runs on — under Proton the game is still a Windows binary. The constants
//! below are that row, repeated here rather than imported because this crate
//! deliberately does not depend on asr: the builder writing the same numbers
//! asr reads is the thing being tested, and taking them from asr would make
//! the test agree with itself by construction.
//!
//! # Sizes are not layout
//!
//! Structures here are only as large as the last field asr reads, and the
//! bytes between fields are zero. That is not what Mono's structures look
//! like, and it is deliberate: a builder that guessed at the rest would be
//! inventing facts. Anything asr does not read cannot matter, and if that ever
//! stops being true, the read lands on a zero and fails loudly.

use std::collections::HashMap;

use super::{ClassFacts, Fixture};
use crate::memory::{flags, FakeProcess, MemoryRange, SparseMemory};

/// `MonoAssembly`, as asr reads it.
mod assembly {
    pub const ANAME: u64 = 0x10;
    pub const IMAGE: u64 = 0x60;
    pub const SIZE: u64 = 0x68;
}

/// `MonoImage`, and the `MonoInternalHashTable` its class cache is.
mod image {
    pub const CLASS_CACHE: u64 = 0x4D0;
    pub const HASH_SIZE: u64 = CLASS_CACHE + 0x18;
    pub const HASH_TABLE: u64 = CLASS_CACHE + 0x20;
    pub const SIZE: u64 = HASH_TABLE + 0x8;
}

/// `MonoClass`.
mod class {
    pub const PARENT: u64 = 0x30;
    pub const IMAGE: u64 = 0x40;
    pub const NAME: u64 = 0x48;
    pub const NAMESPACE: u64 = 0x50;
    pub const VTABLE_SIZE: u64 = 0x5C;
    pub const FIELDS: u64 = 0x98;
    pub const RUNTIME_INFO: u64 = 0xD0;
    pub const FIELD_COUNT: u64 = 0x100;
    pub const NEXT_CLASS_CACHE: u64 = 0x108;
    pub const SIZE: u64 = NEXT_CLASS_CACHE + 0x8;
}

/// `MonoClassField`. The stride is what makes a field array walkable.
mod field {
    pub const NAME: u64 = 0x8;
    pub const OFFSET: u64 = 0x18;
    pub const STRIDE: u64 = 0x20;
}

/// `MonoVTable`: where the vtable's own function slots begin, and so where the
/// static-table pointer sits once `vtable_size` of them have gone by.
const VTABLE_FUNCTIONS: u64 = 0x48;

/// How many function slots each synthetic vtable claims.
///
/// Any number would do — nothing reads the slots — but zero would put the
/// static table exactly where the last read function pointer is, and a
/// builder bug that landed there would be invisible. A non-zero gap of known
/// size makes an off-by-one land in the padding instead.
const VTABLE_SIZE: u32 = 4;

/// The object header: every managed object begins with a pointer to its
/// class's vtable, which is how [`Class::of_object`] gets back to the class.
const OBJECT_VTABLE: u64 = 0x0;
/// Objects start with the vtable pointer and Mono's sync block.
const OBJECT_HEADER: u64 = 0x10;

/// Where the synthetic world's regions sit.
///
/// Chosen so the whole address space fits inside 2 GiB, which is not
/// cosmetic: `Module::attach` finds the assembly root through a `48 8B 0D`
/// RIP-relative instruction, whose displacement is a signed 32-bit number. A
/// builder that scattered its regions across the address space would fail
/// there, and only there.
const UNITY_BASE: u64 = 0x1000_0000;
const MONO_BASE: u64 = 0x1001_0000;
/// Mono's metadata: assemblies, images, classes, fields and their names.
const METADATA_BASE: u64 = 0x2000_0000;
/// The managed heap, which is where a scenario's objects go.
const HEAP_BASE: u64 = 0x4000_0000;

/// A block of memory being filled in, addressed as the process will see it.
///
/// One `Vec` per region rather than one per structure: [`SparseMemory`] fails
/// a read that spans two blocks, and Mono's structures are adjacent to each
/// other by construction.
struct Region {
    base: u64,
    bytes: Vec<u8>,
}

impl Region {
    fn new(base: u64) -> Self {
        Self {
            base,
            bytes: Vec::new(),
        }
    }

    /// Reserves `len` zeroed bytes, 8-aligned, and returns their address.
    fn alloc(&mut self, len: u64) -> u64 {
        let start = (self.bytes.len() as u64 + 7) & !7;
        self.bytes.resize((start + len) as usize, 0);
        self.base + start
    }

    /// Places a NUL-terminated string and returns its address, reusing one
    /// already placed. Mono interns its names, and so a fixture with twenty
    /// classes in six assemblies places six assembly names rather than twenty.
    fn cstr(&mut self, text: &str, interned: &mut HashMap<String, u64>) -> u64 {
        if let Some(&address) = interned.get(text) {
            return address;
        }
        let address = self.alloc(text.len() as u64 + 1);
        self.write(address, text.as_bytes());
        interned.insert(text.to_owned(), address);
        address
    }

    fn write(&mut self, address: u64, bytes: &[u8]) {
        let start = (address - self.base) as usize;
        assert!(
            start + bytes.len() <= self.bytes.len(),
            "write of {} bytes at {address:#x} runs past the region",
            bytes.len()
        );
        self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
    }

    fn write_u32(&mut self, address: u64, value: u32) {
        self.write(address, &value.to_le_bytes());
    }

    fn write_u64(&mut self, address: u64, value: u64) {
        self.write(address, &value.to_le_bytes());
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// Where one class ended up, for a test that needs to put an object of it
/// somewhere.
#[derive(Clone, Copy, Debug)]
pub struct ClassLayout {
    /// The `MonoClass`, as `Image::get_class` would return it.
    pub class: u64,
    /// The class's `MonoVTable`. An object is an instance of this class if and
    /// only if its first pointer is this value — which is exactly how the
    /// splitter recognises a service on the heap.
    pub vtable: u64,
    /// The class's static table, if it has any static fields.
    pub static_table: Option<u64>,
    /// How large an instance is: past the last field the fixture knows of.
    /// Not the game's instance size, which a fixture does not record.
    pub instance_size: u64,
}

/// A synthetic process built from a [`Fixture`].
///
/// Built in two steps because the memory is fixed once the process exists:
/// [`Builder::new`] lays out Mono's metadata, then objects are placed on the
/// heap, and [`Builder::finish`] hands over the process.
pub struct Builder {
    metadata: Region,
    heap: Region,
    classes: HashMap<(String, String), ClassLayout>,
    game_version: String,
}

impl Builder {
    /// Lays out every class the fixture describes.
    pub fn new(fixture: &Fixture) -> Self {
        let mut metadata = Region::new(METADATA_BASE);
        let mut interned = HashMap::new();
        let mut classes = HashMap::new();

        // The global `mono_assembly_foreach` reads its list head out of. It
        // has to be first: the module's RIP-relative displacement is computed
        // against it, and everything else can move.
        let root = metadata.alloc(8);

        // Classes are grouped into their assemblies, keeping the fixture's
        // order so a diff of the file and a walk of the world agree.
        let mut images: Vec<(&str, Vec<&ClassFacts>)> = Vec::new();
        for facts in &fixture.classes {
            match images.iter_mut().find(|(name, _)| *name == facts.image) {
                Some((_, list)) => list.push(facts),
                None => images.push((&facts.image, vec![facts])),
            }
        }

        let mut previous_node: Option<u64> = None;
        for (image_name, image_classes) in images {
            let node = metadata.alloc(0x10);
            match previous_node {
                // `next` of the node before this one.
                Some(previous) => metadata.write_u64(previous + 8, node),
                None => metadata.write_u64(root, node),
            }
            previous_node = Some(node);

            let assembly = metadata.alloc(assembly::SIZE);
            metadata.write_u64(node, assembly);

            let name = metadata.cstr(image_name, &mut interned);
            metadata.write_u64(assembly + assembly::ANAME, name);

            let image = metadata.alloc(image::SIZE);
            metadata.write_u64(assembly + assembly::IMAGE, image);

            // One class per bucket. Mono hashes by name into a shared table
            // and chains collisions; asr walks every bucket and every chain,
            // so a table that never collides exercises the same code with
            // nothing left to get wrong.
            let buckets = metadata.alloc(8 * image_classes.len() as u64);
            metadata.write_u32(image + image::HASH_SIZE, image_classes.len() as u32);
            metadata.write_u64(image + image::HASH_TABLE, buckets);

            for (index, facts) in image_classes.iter().enumerate() {
                let layout = lay_out_class(&mut metadata, &mut interned, facts, image);
                metadata.write_u64(buckets + 8 * index as u64, layout.class);
                classes.insert((facts.image.clone(), facts.name.clone()), layout);
            }
        }

        Self {
            metadata,
            heap: Region::new(HEAP_BASE),
            classes,
            game_version: fixture.game_version.clone(),
        }
    }

    /// Where a class ended up, or `None` if the fixture never mentioned it.
    pub fn class(&self, image: &str, name: &str) -> Option<ClassLayout> {
        self.classes
            .get(&(image.to_owned(), name.to_owned()))
            .copied()
    }

    /// Same, but panicking with the name — for a test, where a missing class
    /// is a mistake in the test rather than a condition to handle.
    pub fn expect_class(&self, image: &str, name: &str) -> ClassLayout {
        self.class(image, name)
            .unwrap_or_else(|| panic!("the fixture has no class {image}/{name}"))
    }

    /// Places a zeroed instance of a class on the heap, with its vtable
    /// pointer set, and returns its address.
    ///
    /// `extra` reserves room past the fields the fixture knows about, for an
    /// object a test wants to write beyond them.
    pub fn new_object(&mut self, image: &str, name: &str, extra: u64) -> u64 {
        let layout = self.expect_class(image, name);
        let address = self.heap.alloc(layout.instance_size + extra);
        self.heap.write_u64(address + OBJECT_VTABLE, layout.vtable);
        address
    }

    /// Reserves `len` zeroed bytes on the heap — an array's storage, a string,
    /// anything without a class of its own.
    pub fn alloc(&mut self, len: u64) -> u64 {
        self.heap.alloc(len)
    }

    pub fn write(&mut self, address: u64, bytes: &[u8]) {
        self.region_for(address).write(address, bytes);
    }

    pub fn write_u32(&mut self, address: u64, value: u32) {
        self.region_for(address).write_u32(address, value);
    }

    pub fn write_u64(&mut self, address: u64, value: u64) {
        self.region_for(address).write_u64(address, value);
    }

    fn region_for(&mut self, address: u64) -> &mut Region {
        if address >= HEAP_BASE {
            &mut self.heap
        } else {
            &mut self.metadata
        }
    }

    /// The process, with the modules and ranges asr needs to attach.
    ///
    /// The name is the one Linux reports for the game — 15 characters of
    /// `/proc/<pid>/comm`, which is where the splitter's ambiguous-name
    /// handling comes from — so a synthetic world reaches the splitter down
    /// the same path a real one does.
    pub fn finish(self) -> FakeProcess {
        let unity = unity_player();
        let mono = mono_module(METADATA_BASE);

        let metadata_size = self.metadata.len();
        let mut memory = SparseMemory::new();
        memory.put(UNITY_BASE, unity.clone());
        memory.put(MONO_BASE, mono.clone());
        memory.put(METADATA_BASE, self.metadata.bytes);
        // A heap with nothing on it is still a heap; an empty block would make
        // every read of it fail rather than read a zero.
        let heap_size = self.heap.len().max(0x1000);
        let mut heap = self.heap.bytes;
        heap.resize(heap_size as usize, 0);
        memory.put(HEAP_BASE, heap);

        FakeProcess {
            path: Some(format!("/synthetic/{}/Timberborn.exe", self.game_version)),
            ranges: vec![
                MemoryRange {
                    address: UNITY_BASE,
                    size: unity.len() as u64,
                    flags: flags::READ | flags::EXECUTE | flags::PATH,
                },
                MemoryRange {
                    address: MONO_BASE,
                    size: mono.len() as u64,
                    flags: flags::READ | flags::EXECUTE | flags::PATH,
                },
                MemoryRange {
                    address: METADATA_BASE,
                    size: metadata_size,
                    flags: flags::HEAP,
                },
                MemoryRange {
                    address: HEAP_BASE,
                    size: heap_size,
                    flags: flags::HEAP,
                },
            ],
            ..FakeProcess::new(4242, "Unity Main Thre")
                .with_module("UnityPlayer.dll", UNITY_BASE, unity.len() as u64)
                .with_module("mono-2.0-bdwgc.dll", MONO_BASE, mono.len() as u64)
                .with_memory(memory)
        }
    }
}

/// One class, its name strings, its field array and its vtable.
fn lay_out_class(
    metadata: &mut Region,
    interned: &mut HashMap<String, u64>,
    facts: &ClassFacts,
    image: u64,
) -> ClassLayout {
    let class = metadata.alloc(class::SIZE);

    let name = metadata.cstr(&facts.name, interned);
    let namespace = metadata.cstr(&facts.namespace, interned);
    metadata.write_u64(class + class::NAME, name);
    metadata.write_u64(class + class::NAMESPACE, namespace);
    metadata.write_u64(class + class::IMAGE, image);
    // Null: a fixture records the flattened view, so nothing is inherited and
    // asr's walk up the chain ends here. See the module docs.
    metadata.write_u64(class + class::PARENT, 0);
    metadata.write_u32(class + class::VTABLE_SIZE, VTABLE_SIZE);

    metadata.write_u32(class + class::FIELD_COUNT, facts.fields.len() as u32);
    if !facts.fields.is_empty() {
        let fields = metadata.alloc(field::STRIDE * facts.fields.len() as u64);
        metadata.write_u64(class + class::FIELDS, fields);
        for (index, facts) in facts.fields.iter().enumerate() {
            let entry = fields + field::STRIDE * index as u64;
            let name = metadata.cstr(&facts.name, interned);
            metadata.write_u64(entry + field::NAME, name);
            metadata.write_u32(entry + field::OFFSET, facts.offset);
        }
    }

    // `runtime_info` is a `MonoClassRuntimeInfo`: a count, then one vtable per
    // domain. asr takes the first, at one pointer in.
    let runtime_info = metadata.alloc(0x10);
    metadata.write_u64(class + class::RUNTIME_INFO, runtime_info);

    // The vtable, then its function slots, then the pointer to the static
    // table. Mono lays it out exactly this way, which is why asr can find a
    // static table by skipping `vtable_size` pointers.
    let statics = facts.fields.iter().filter(|f| f.is_static);
    let static_size = statics.map(|f| u64::from(f.offset) + 8).max();
    let vtable = metadata.alloc(VTABLE_FUNCTIONS + 8 * u64::from(VTABLE_SIZE) + 8);
    metadata.write_u64(runtime_info + 8, vtable);
    // A vtable begins with its class, which is what turns an object back into
    // the class it is an instance of.
    metadata.write_u64(vtable, class);

    let static_table = static_size.map(|size| {
        let table = metadata.alloc(size);
        metadata.write_u64(
            vtable + VTABLE_FUNCTIONS + 8 * u64::from(VTABLE_SIZE),
            table,
        );
        table
    });

    let instance_size = facts
        .fields
        .iter()
        .filter(|f| !f.is_static)
        .map(|f| u64::from(f.offset) + 8)
        .max()
        .unwrap_or(OBJECT_HEADER)
        .max(OBJECT_HEADER);

    ClassLayout {
        class,
        vtable,
        static_table,
        instance_size,
    }
}

/// The smallest PE that `pe::FileVersion::read` will report a Unity version
/// out of. Major 6000 is Unity 6, which puts asr on `Version::V3`.
fn unity_player() -> Vec<u8> {
    const RESOURCES: u32 = 0x200;
    const TYPE_DIR: u32 = 0x40;
    const LANG_DIR: u32 = 0x80;
    const DATA_ENTRY: u32 = 0xC0;
    const VERSION_INFO: u32 = 0x300;

    let mut pe = pe_headers(0x1000);
    // Data directory 2 is the resource table.
    pe.dir(2, RESOURCES, 0x200);

    // Three nested resource directories: type, then language, then the data
    // entry itself. asr takes the RT_VERSION type and the first of each of the
    // rest, so one entry per level is enough.
    pe.dir_header(RESOURCES, 1);
    pe.resource_entry(RESOURCES + 0x10, 0x10, TYPE_DIR, true);
    pe.dir_header(RESOURCES + TYPE_DIR, 1);
    pe.resource_entry(RESOURCES + TYPE_DIR + 0x10, 1, LANG_DIR, true);
    pe.dir_header(RESOURCES + LANG_DIR, 1);
    pe.resource_entry(RESOURCES + LANG_DIR + 0x10, 0x409, DATA_ENTRY, false);

    // An IMAGE_RESOURCE_DATA_ENTRY: the address of the VS_VERSIONINFO.
    pe.u32(RESOURCES + DATA_ENTRY, VERSION_INFO);

    // VS_VERSIONINFO's fixed part sits 0x28 in, past the header and the
    // "VS_VERSION_INFO" string asr does not read.
    pe.u32(VERSION_INFO + 0x28, 0xFEEF_04BD);
    pe.u32(VERSION_INFO + 0x2C, 0x0001_0000);
    // major.minor packed as one word each, high half first: Unity 6000.3.
    pe.u32(VERSION_INFO + 0x30, (6000 << 16) | 3);
    pe.u32(VERSION_INFO + 0x34, 0);
    pe.bytes
}

/// A PE exporting `mono_assembly_foreach`, whose first instruction hands asr
/// the address of the assembly list.
fn mono_module(metadata_base: u64) -> Vec<u8> {
    const EXPORTS: u32 = 0x200;
    const EXPORTS_SIZE: u32 = 0x40;
    const FUNCTIONS: u32 = 0x300;
    const NAMES: u32 = 0x310;
    const ORDINALS: u32 = 0x320;
    const NAME: u32 = 0x330;
    const CODE: u32 = 0x400;

    let mut pe = pe_headers(0x1000);
    // Data directory 0 is the export table. Its size matters: asr discards any
    // function whose address falls inside it, as a forwarded export.
    pe.dir(0, EXPORTS, EXPORTS_SIZE);

    pe.u32(EXPORTS + 0x14, 1); // NumberOfFunctions
    pe.u32(EXPORTS + 0x18, 1); // NumberOfNames
    pe.u32(EXPORTS + 0x1C, FUNCTIONS);
    pe.u32(EXPORTS + 0x20, NAMES);
    pe.u32(EXPORTS + 0x24, ORDINALS);

    pe.u32(FUNCTIONS, CODE);
    pe.u32(NAMES, NAME);
    pe.write(ORDINALS, &0u16.to_le_bytes());
    pe.write(NAME, b"mono_assembly_foreach\0");

    // `mov rcx, [rip + rel32]`, which is how Mono's compiled
    // `mono_assembly_foreach` loads the list head. asr scans the first 0x100
    // bytes of the function for it and follows the displacement.
    let instruction = MONO_BASE + u64::from(CODE);
    let rel32_at = instruction + 3;
    let displacement = i64::try_from(metadata_base)
        .ok()
        .and_then(|target| i32::try_from(target - (rel32_at as i64 + 4)).ok())
        .expect("the metadata region must sit within 2 GiB of the mono module");
    pe.write(CODE, &[0x48, 0x8B, 0x0D]);
    pe.write(CODE + 3, &displacement.to_le_bytes());

    pe.bytes
}

/// A PE image being assembled, addressed by RVA.
///
/// Only the headers asr parses: a DOS header, a COFF header, a PE32+ optional
/// header and its data directories. Every RVA is an offset into this one
/// block, which is what a loaded image looks like anyway.
struct PeImage {
    bytes: Vec<u8>,
}

/// Where the COFF header goes. Anywhere past the DOS header's `e_lfanew` would
/// do; this is what a real linker emits.
const E_LFANEW: u32 = 0x80;
const OPTIONAL_HEADER: u32 = E_LFANEW + 24;
/// PE32+ puts the data directories 112 bytes into the optional header.
const DATA_DIRECTORIES: u32 = OPTIONAL_HEADER + 112;

fn pe_headers(size: u32) -> PeImage {
    let mut pe = PeImage {
        bytes: vec![0; size as usize],
    };
    pe.write(0, b"MZ");
    pe.u32(0x3C, E_LFANEW);
    pe.write(E_LFANEW, b"PE\0\0");
    pe.write(E_LFANEW + 4, &0x8664u16.to_le_bytes()); // machine: x86-64
    pe.write(E_LFANEW + 20, &0xF0u16.to_le_bytes()); // size of the optional header
    pe.write(OPTIONAL_HEADER, &0x20Bu16.to_le_bytes()); // PE32+
    pe.u32(OPTIONAL_HEADER + 56, size); // size of image
    pe
}

impl PeImage {
    fn write(&mut self, rva: u32, bytes: &[u8]) {
        let start = rva as usize;
        assert!(
            start + bytes.len() <= self.bytes.len(),
            "PE write at {rva:#x} runs past the image"
        );
        self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
    }

    fn u32(&mut self, rva: u32, value: u32) {
        self.write(rva, &value.to_le_bytes());
    }

    /// One entry of the optional header's data directory array.
    fn dir(&mut self, index: u32, rva: u32, size: u32) {
        self.u32(DATA_DIRECTORIES + index * 8, rva);
        self.u32(DATA_DIRECTORIES + index * 8 + 4, size);
    }

    /// An IMAGE_RESOURCE_DIRECTORY, of which asr reads only the two counts.
    fn dir_header(&mut self, rva: u32, id_entries: u16) {
        self.write(rva + 12, &0u16.to_le_bytes()); // named entries
        self.write(rva + 14, &id_entries.to_le_bytes());
    }

    /// An IMAGE_RESOURCE_DIRECTORY_ENTRY. The high bit of the offset says
    /// whether it points at another directory or at a leaf.
    fn resource_entry(&mut self, rva: u32, id: u32, offset: u32, directory: bool) {
        self.u32(rva, id);
        self.u32(
            rva + 4,
            if directory {
                offset | 0x8000_0000
            } else {
                offset
            },
        );
    }
}
