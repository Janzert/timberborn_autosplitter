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
    fixture::{ClassFacts, FieldFacts, Fixture, Sources},
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
    let offsets = offsets(&dir, &names)?;
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
        "wrote    {} ({} classes, {} fields)",
        out.display(),
        fixture.classes.len(),
        fixture
            .classes
            .iter()
            .map(|c| c.fields.len())
            .sum::<usize>(),
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

/// The snapshot's half: what Mono actually laid each class out as.
///
/// Resolved through asr rather than by reading the capture directly, so the
/// offsets a fixture records are the ones the splitter would have got. A class
/// whose fields cannot be resolved here is left out entirely, and the caller
/// fails on it by name.
#[allow(clippy::type_complexity)]
fn offsets(
    dir: &Path,
    wanted: &[ClassFacts],
) -> Result<std::collections::HashMap<(String, String, String), u32>, String> {
    let snapshot = Snapshot::open(dir).map_err(|e| format!("opening {}: {e}", dir.display()))?;
    let pid = snapshot.metadata.pid;

    let found = RefCell::new(std::collections::HashMap::new());
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
        },
        1,
    );

    Ok(found.into_inner())
}
