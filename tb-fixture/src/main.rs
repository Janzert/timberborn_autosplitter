//! Writes a fixture: the committed facts a synthetic world is built from.
//!
//! The facts come from two places and neither can produce the other. The
//! assemblies hold names, declared types and static flags, and no offsets at
//! all — Mono assigns an offset when it lays a class out, so offsets exist
//! only in a running process. A memory snapshot is a running process kept, so
//! this resolves them there, through the same `Module` the splitter uses.
//!
//! ```text
//! cargo fixture --managed ~/.../Timberborn_Data/Managed
//! ```
//!
//! Both halves have to describe the same build, and a mismatch would be
//! invisible in the output. So the install's own version string is read and
//! matched against the snapshot's: the wrong pairing fails rather than writing
//! a fixture whose names come from one game and whose offsets come from
//! another.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::Command,
};

use asr::{game_engine::unity::mono::Module, Process, ProcessId};
use test_harness::{
    fixture::{ClassFacts, FieldFacts, Fixture, InstanceFacts, Sources},
    snapshot::Snapshot,
    World,
};

/// The state a fixture is generated from.
///
/// Any capture with a save loaded would do for most of the classes, but Mono
/// loads assemblies lazily: at the main menu half of these are not present at
/// all. A finished run has everything the splitter ever touches resolved.
const STATE: &str = "run-finished";

fn main() {
    if let Err(message) = run() {
        eprintln!("tb-fixture: {message}");
        std::process::exit(1);
    }
}

struct Args {
    managed: PathBuf,
    state: String,
    out: Option<PathBuf>,
    facts: Option<PathBuf>,
}

const HELP: &str = "\
Writes fixtures/<game version>.json from a game install and a memory snapshot.

    cargo fixture --managed <Timberborn_Data/Managed>

    --managed <dir>   the install's Managed directory; names and types come
                      from the assemblies in it
    --state <id>      which captured state to resolve offsets against
                      (default: run-finished)
    --facts <file>    use this `metadata.py facts` output instead of running it
    --out <file>      write here instead of fixtures/<game version>.json
";

fn parse_args() -> Result<Args, String> {
    let mut managed = None;
    let mut state = STATE.to_owned();
    let mut out = None;
    let mut facts = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("--{what} needs a value"));
        match arg.as_str() {
            "--managed" => managed = Some(PathBuf::from(value("managed")?)),
            "--state" => state = value("state")?,
            "--out" => out = Some(PathBuf::from(value("out")?)),
            "--facts" => facts = Some(PathBuf::from(value("facts")?)),
            "-h" | "--help" => return Err("help".into()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    Ok(Args {
        managed: managed.ok_or("--managed is required; see --help")?,
        state,
        out,
        facts,
    })
}

fn run() -> Result<(), String> {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) if message == "help" => {
            print!("{HELP}");
            return Ok(());
        }
        Err(message) => return Err(message),
    };

    // `Managed` sits two levels below the game directory, which is where the
    // version string lives.
    let install = args
        .managed
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("{} is not inside a game install", args.managed.display()))?;
    let version = test_harness::install::version(install).ok_or_else(|| {
        format!(
            "cannot read the game's version from {}. Without it there is no \
             saying which build these facts describe.",
            install.display()
        )
    })?;

    let dir = snapshot_of(&args.state, &version)?;
    println!("install  {version} ({})", install.display());
    println!("snapshot {}", dir.display());

    let names = facts(&args)?;
    let Resolved { offsets, instances } = resolve(&dir, &names)?;
    let snapshot = Snapshot::open(&dir).map_err(|e| format!("opening {}: {e}", dir.display()))?;

    let mut classes = Vec::new();
    for class in names {
        let mut fields = Vec::new();
        for field in class.fields {
            let key = (class.image.clone(), class.name.clone(), field.name.clone());
            let offset = offsets.get(&key).copied().ok_or_else(|| {
                format!(
                    "{}/{}: the assemblies declare {}, but it did not resolve against the \
                     snapshot.\n       Either they are of different builds, or Mono had not \
                     laid the class out.",
                    class.image, class.name, field.name
                )
            })?;
            fields.push(FieldFacts { offset, ..field });
        }
        classes.push(ClassFacts { fields, ..class });
    }

    let fixture = Fixture {
        instances,
        game_version: snapshot.metadata.game_version.clone(),
        build_id: snapshot.metadata.build_id.parse().ok(),
        sources: Sources {
            snapshot: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            managed: args.managed.display().to_string(),
        },
        classes,
    };

    let out = args.out.unwrap_or_else(|| {
        test_harness::fixture::default_dir().join(format!("{}.json", fixture.game_version))
    });
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(&out, fixture.to_json())
        .map_err(|e| format!("writing {}: {e}", out.display()))?;

    println!(
        "wrote    {} ({} classes, {} fields, {} instance layouts)",
        out.display(),
        fixture.classes.len(),
        fixture
            .classes
            .iter()
            .map(|c| c.fields.len())
            .sum::<usize>(),
        fixture.instances.len(),
    );
    Ok(())
}

/// The capture to resolve offsets against: one of the named state, of the
/// installed build.
///
/// Refusing to fall back to another version is the point. A fixture whose
/// names came from one build and whose offsets came from another would build a
/// world that is wrong in exactly the way nothing else here can detect.
fn snapshot_of(state: &str, version: &str) -> Result<PathBuf, String> {
    let dirs = test_harness::snapshot::find_all(state)?;
    let mut versions = Vec::new();
    for dir in &dirs {
        let Ok(snapshot) = Snapshot::open(dir) else {
            continue;
        };
        if snapshot.metadata.game_version == version {
            return Ok(dir.clone());
        }
        versions.push(snapshot.metadata.game_version);
    }
    versions.sort();
    versions.dedup();
    Err(format!(
        "no {state:?} snapshot of {version}, which is the installed build.\n       \
         There are captures of: {}.\n       \
         Either capture one of this build, or switch the install to a version there \
         is a capture of -- see steam_versions/ in the parent repository.",
        if versions.is_empty() {
            "nothing".to_owned()
        } else {
            versions.join(", ")
        }
    ))
}

/// The assemblies' half, by running `devtools/metadata.py facts`.
///
/// Shelled out to rather than reimplemented: the ECMA-335 parser already
/// exists there, is already what `metadata.py check` uses, and having one
/// reader of the assemblies means a fixture and a version check can never
/// disagree about what an assembly says.
fn facts(args: &Args) -> Result<Vec<ClassFacts>, String> {
    let text = match &args.facts {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?
        }
        None => {
            let script = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("the crate always has a parent directory")
                .join("devtools/metadata.py");
            let output = Command::new("python3")
                .arg(&script)
                .arg("facts")
                .arg(&args.managed)
                .output()
                .map_err(|e| format!("running {}: {e}", script.display()))?;
            if !output.status.success() {
                return Err(format!(
                    "{} facts failed:\n{}",
                    script.display(),
                    String::from_utf8_lossy(&output.stderr).trim_end()
                ));
            }
            String::from_utf8(output.stdout).map_err(|e| format!("metadata.py facts: {e}"))?
        }
    };

    // The same reader the harness uses for a whole fixture, given the half
    // that has no offsets yet.
    Fixture::from_json(&with_placeholders(&text))
        .map(|fixture| fixture.classes)
        .map_err(|e| format!("metadata.py facts: {e}"))
}

/// Dresses `metadata.py facts` output up as a fixture so one parser reads
/// both. The offsets are the thing being filled in, and any that survived
/// would be a bug, so they go in as an unmistakable value.
fn with_placeholders(text: &str) -> String {
    let mut value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => return text.to_owned(),
    };
    if let Some(classes) = value.get_mut("classes").and_then(|v| v.as_array_mut()) {
        for class in classes {
            if let Some(fields) = class.get_mut("fields").and_then(|v| v.as_array_mut()) {
                for field in fields {
                    field["offset"] = serde_json::Value::from(u32::MAX);
                }
            }
        }
    }
    value["format"] = serde_json::Value::from(test_harness::fixture::FORMAT);
    value["game_version"] = serde_json::Value::from("(from the assemblies)");
    value.to_string()
}

/// Everything the capture had to say.
struct Resolved {
    /// Field offsets, keyed by image, class and the name Mono holds.
    offsets: std::collections::HashMap<(String, String, String), u32>,
    /// The layouts that have no name to look them up by.
    instances: Vec<InstanceFacts>,
}

/// The snapshot's half: what Mono actually laid each class out as.
///
/// Resolved through asr rather than by reading the capture directly, so the
/// offsets a fixture records are the ones the splitter would have got. A class
/// whose fields cannot be resolved here is left out entirely, and the caller
/// fails on it by name.
fn resolve(dir: &Path, wanted: &[ClassFacts]) -> Result<Resolved, String> {
    let snapshot = Snapshot::open(dir).map_err(|e| format!("opening {}: {e}", dir.display()))?;
    let pid = snapshot.metadata.pid;

    let found = RefCell::new(std::collections::HashMap::new());
    let instances = RefCell::new(Vec::new());
    let world = World::new().with_process(snapshot.process());

    // One poll: everything here is synchronous, and the fake runtime only
    // exists while `drive` is running.
    test_harness::drive(
        world,
        async {
            let Some(process) = Process::attach_by_pid(ProcessId(pid.into())) else {
                asr::print_message("could not attach to the captured process");
                return;
            };
            let Some(module) = Module::attach_auto_detect(&process) else {
                asr::print_message("the capture has no Mono module asr recognises");
                return;
            };
            for class in wanted {
                let Some(image) = module.get_image(&process, &class.image) else {
                    continue;
                };
                let Some(resolved) = image.get_class(&process, &module, &class.name) else {
                    continue;
                };
                for field in &class.fields {
                    if let Some(offset) = resolved.get_field_offset(&process, &module, &field.name)
                    {
                        found.borrow_mut().insert(
                            (class.image.clone(), class.name.clone(), field.name.clone()),
                            offset,
                        );
                    }
                }
            }
            // Only after the named classes: the walk to an instance needs
            // their offsets to follow a field at all.
            *instances.borrow_mut() = instances::walk(&process, &module, &found.borrow());
        },
        1,
    );

    Ok(Resolved {
        offsets: found.into_inner(),
        instances: instances.into_inner(),
    })
}

/// The layouts that cannot be looked up by name.
///
/// `HashSet<string>` and `List<EntityComponent>` are *inflated generics*: each
/// instantiation is a class of its own with its own field offsets, and none of
/// them is in an image's class cache under a name anything could ask for. The
/// only way to a one is through an object that is an instance of it, which is
/// why the splitter reaches them with `Class::of_object` — and why this walks
/// the captured heap to a real one rather than looking anything up.
///
/// The walk is the splitter's own path, without the budgets and the yielding a
/// live game needs: sweep for the DI container, take the services out of it,
/// and follow the field whose target is wanted.
mod instances {
    use asr::{
        game_engine::unity::mono::{Class, Module},
        Address, MemoryRangeFlags, Process,
    };

    use std::collections::HashMap;

    use test_harness::fixture::{InstanceFacts, InstanceField};

    /// Field offsets as the capture reported them, keyed by image, class and
    /// field. The names half cannot be used here: its offsets are placeholders
    /// until this same pass has filled them in.
    pub type Offsets = HashMap<(String, String, String), u32>;

    /// `MonoArray`: header, bounds, then the length and the elements.
    const ARRAY_LENGTH: u64 = 0x18;
    const ARRAY_DATA: u64 = 0x20;

    /// Where to walk to, and what to record when we get there.
    ///
    /// `reached_by` is the identity: an inflated generic has no name asr can
    /// read, so a layout is recorded by the field it belongs to rather than by
    /// what it is called. That is also the only identity a test needs — it
    /// asks for "the list the entity registry holds", not for `List<T>`.
    struct Wanted {
        /// Where the walk starts: a class that is in the DI container.
        image: &'static str,
        class: &'static str,
        /// Fields to follow from there. Not everything wanted hangs directly
        /// off a singleton -- the entity registry is held by
        /// `GameOverChecker`, and a component cache is held by an entity that
        /// is itself inside a list.
        path: &'static [Step],
        role: &'static str,
        fields: &'static [&'static str],
    }

    enum Step {
        /// Follow a named field of a named class.
        Field(&'static str, &'static str, &'static str),
        /// Take the first element of the list at this address, using the list
        /// layout this same walk discovered.
        FirstOfList,
    }

    const WANTED: &[Wanted] = &[
        Wanted {
            image: "Timberborn.GameOver",
            class: "GameOverChecker",
            path: &[
                Step::Field("Timberborn.GameOver", "GameOverChecker", "_entityRegistry"),
                Step::Field(
                    "Timberborn.EntitySystem",
                    "EntityRegistry",
                    "_entitiesInInstantiationOrder",
                ),
            ],
            role: "list",
            fields: &["_items", "_size"],
        },
        Wanted {
            image: "Timberborn.ScienceSystem",
            class: "BuildingUnlockingService",
            path: &[Step::Field(
                "Timberborn.ScienceSystem",
                "BuildingUnlockingService",
                "_unlockedBuildings",
            )],
            role: "hash-set",
            fields: &["_slots", "_count", "_lastIndex"],
        },
        Wanted {
            image: "Timberborn.GameOver",
            class: "GameOverChecker",
            path: &[
                Step::Field("Timberborn.GameOver", "GameOverChecker", "_entityRegistry"),
                Step::Field(
                    "Timberborn.EntitySystem",
                    "EntityRegistry",
                    "_entitiesInInstantiationOrder",
                ),
                Step::FirstOfList,
                Step::Field(
                    "Timberborn.BaseComponentSystem",
                    "BaseComponent",
                    "_componentCache",
                ),
                Step::Field(
                    "Timberborn.BaseComponentSystem",
                    "ComponentCache",
                    "_components",
                ),
            ],
            role: "component-list",
            fields: &["_items", "_size"],
        },
    ];

    /// Every layout that could be reached, and a line each about the rest.
    ///
    /// Missing ones are reported rather than fatal. Mono fills an inflated
    /// generic's field table in lazily and may never do it at all — measured
    /// against a live game, `Class::of_object` resolved the entity list's
    /// class and then reported *none* of its fields, while the object was
    /// plainly a list. A capture where that happened cannot yield these, and
    /// that is a fact about the capture rather than an error.
    pub fn walk(process: &Process, module: &Module, offsets: &Offsets) -> Vec<InstanceFacts> {
        let Some(container) = find_container(process, module, offsets) else {
            eprintln!(
                "  note: no DI container found in the capture, so no collection \
                 layouts were recorded"
            );
            return Vec::new();
        };

        let mut out: Vec<InstanceFacts> = Vec::new();
        for wanted in WANTED {
            let Some(start) = container
                .iter()
                .copied()
                .find(|&object| is_a(process, module, object, wanted.image, wanted.class))
            else {
                eprintln!(
                    "  note: {}/{} is not in the container, so {} was not recorded",
                    wanted.image, wanted.class, wanted.role
                );
                continue;
            };

            let Some(target) = follow(process, offsets, &out, start, wanted) else {
                continue;
            };
            let Some(class) = Class::of_object(process, module, target) else {
                continue;
            };

            let mut fields = Vec::new();
            for name in wanted.fields {
                match class.get_field_offset(process, module, name) {
                    Some(offset) => fields.push(InstanceField {
                        name: (*name).to_owned(),
                        offset,
                    }),
                    None => {
                        eprintln!(
                            "  note: the class reached by {} did not resolve {name}. Mono \
                             fills an inflated generic's field table in lazily, and this \
                             capture is of a session where it had not.",
                            describe(wanted)
                        );
                        fields.clear();
                        break;
                    }
                }
            }
            if fields.is_empty() {
                continue;
            }
            out.push(InstanceFacts {
                reached_by: describe(wanted),
                role: wanted.role.to_owned(),
                fields,
            });
        }
        out
    }

    /// Walks a path of dereferences from a starting object.
    fn follow(
        process: &Process,
        offsets: &Offsets,
        found: &[InstanceFacts],
        start: Address,
        wanted: &Wanted,
    ) -> Option<Address> {
        let mut at = start;
        for step in wanted.path {
            at = match step {
                Step::Field(image, class, field) => {
                    let offset = offset_of(offsets, image, class, field)?;
                    match read_pointer(process, at.add(offset)) {
                        Some(next) => next,
                        None => {
                            eprintln!("  note: {image}/{class}.{field} is null in the capture");
                            return None;
                        }
                    }
                }
                // Uses the list layout an earlier entry recorded, which is why
                // WANTED is ordered: the entity list has to be measured before
                // anything inside it can be reached.
                Step::FirstOfList => {
                    let list = found.iter().find(|i| i.role == "list")?;
                    let items =
                        read_pointer(process, at.add(u64::from(list.field("_items")?.offset)))?;
                    let size = process
                        .read::<i32>(at.add(u64::from(list.field("_size")?.offset)))
                        .ok()?;
                    if size <= 0 {
                        eprintln!("  note: the entity list is empty in the capture");
                        return None;
                    }
                    read_pointer(process, items.add(ARRAY_DATA))?
                }
            };
        }
        Some(at)
    }

    /// How a layout is identified: the path that was walked to reach it.
    fn describe(wanted: &Wanted) -> String {
        let mut parts = vec![format!("{}/{}", wanted.image, wanted.class)];
        for step in wanted.path {
            parts.push(match step {
                Step::Field(_, class, field) => format!("{class}.{field}"),
                Step::FirstOfList => "[0]".to_owned(),
            });
        }
        parts.join(" -> ")
    }

    /// Every object the game scene's DI container holds.
    ///
    /// The container is found the way the splitter finds it -- sweep for a
    /// `SingletonRepository`, follow it to the singleton array -- except that
    /// the right one is picked by size rather than by which clock it holds. An
    /// offline tool can afford to try them all and keep the fullest, and the
    /// game scene's container held 612 against the menu's 103.
    fn find_container(
        process: &Process,
        module: &Module,
        offsets: &Offsets,
    ) -> Option<Vec<Address>> {
        let vtable = vtable_of(
            process,
            module,
            "Timberborn.SingletonSystem",
            "SingletonRepository",
        )?;
        let listener_offset = offset_of(
            offsets,
            "Timberborn.SingletonSystem",
            "SingletonRepository",
            "_singletonListener",
        )?;
        let singletons_offset = offset_of(
            offsets,
            "Timberborn.SingletonSystem",
            "SingletonListener",
            "_allSingletons",
        )?;

        let mut best: Vec<Address> = Vec::new();
        for candidate in sweep(process, vtable) {
            let Some(entries) = read_pointer(process, candidate.add(listener_offset))
                .and_then(|listener| read_pointer(process, listener.add(singletons_offset)))
                .and_then(|array| read_array(process, array))
            else {
                continue;
            };
            if entries.len() > best.len() {
                best = entries;
            }
        }
        (!best.is_empty()).then_some(best)
    }

    /// Whether an object is an instance of a named class.
    fn is_a(process: &Process, module: &Module, object: Address, image: &str, class: &str) -> bool {
        vtable_of(process, module, image, class)
            .is_some_and(|vtable| read_pointer(process, object) == Some(vtable))
    }

    fn vtable_of(process: &Process, module: &Module, image: &str, class: &str) -> Option<Address> {
        module
            .get_image(process, image)?
            .get_class(process, module, class)?
            .get_vtable(process, module)
    }

    fn offset_of(offsets: &Offsets, image: &str, class: &str, field: &str) -> Option<u64> {
        offsets
            .get(&(image.to_owned(), class.to_owned(), field.to_owned()))
            .map(|&offset| u64::from(offset))
    }

    fn read_pointer(process: &Process, at: Address) -> Option<Address> {
        process
            .read::<u64>(at)
            .ok()
            .map(Address::new)
            .filter(|address| !address.is_null())
    }

    /// The non-null references a `MonoArray` of objects holds.
    fn read_array(process: &Process, array: Address) -> Option<Vec<Address>> {
        let length = process.read::<i64>(array.add(ARRAY_LENGTH)).ok()?;
        // The same bound the splitter uses: a container on a different scale
        // means the field was misread, not that the game grew.
        if !(0..=8192).contains(&length) {
            return None;
        }
        Some(
            (0..length as u64)
                .filter_map(|i| read_pointer(process, array.add(ARRAY_DATA + 8 * i)))
                .collect(),
        )
    }

    /// Every address in the capture holding `needle`.
    ///
    /// The splitter's scan, minus everything that exists for a live game: no
    /// budget, no yielding, no page-level retry. This runs once, offline, and
    /// a capture does not stop responding while it is read.
    fn sweep(process: &Process, needle: Address) -> Vec<Address> {
        const WORDS: usize = 8 * 1024;

        let mut found = Vec::new();
        let mut buf = vec![0u64; WORDS];
        for range in process.memory_ranges() {
            let wanted = MemoryRangeFlags::READ | MemoryRangeFlags::WRITE;
            if !range
                .flags()
                .is_ok_and(|f| f.contains(wanted) && !f.contains(MemoryRangeFlags::EXECUTE))
            {
                continue;
            }
            let Ok((start, size)) = range.range() else {
                continue;
            };
            let mut offset = 0;
            while offset < size {
                let words = WORDS.min(((size - offset) / 8) as usize);
                if words == 0 {
                    break;
                }
                let at = start.add(offset);
                if process
                    .read_into_buf(at, bytemuck::cast_slice_mut(&mut buf[..words]))
                    .is_ok()
                {
                    for (index, &word) in buf[..words].iter().enumerate() {
                        if word == needle.value() {
                            found.push(at.add((index * 8) as u64));
                        }
                    }
                }
                offset += (words * 8) as u64;
            }
        }
        found
    }
}
