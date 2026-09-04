//! The committed facts a synthetic world is built from.
//!
//! A snapshot shows what the game's memory *was*; a fixture says what its
//! layout *is* — class names, field names, declared types, and the offset Mono
//! gave each field. Facts rather than game bytes: a few hundred lines of JSON
//! that can be read in a diff, carrying nothing that could not be published.
//!
//! # Where the facts come from
//!
//! Two places, and neither half can produce the other:
//!
//! - **Names, types and static flags** come from the assemblies, read by
//!   `devtools/metadata.py facts`. ECMA-335 metadata holds no offsets at all.
//! - **Offsets** are assigned by Mono when it lays a class out, so they exist
//!   only in a running process — or in a capture of one. `tb-fixture` resolves
//!   them against a snapshot through the same `Module` the splitter uses.
//!
//! `tb-fixture` merges the two and writes `fixtures/<game version>.json`.
//!
//! # What is deliberately not a fact
//!
//! Vtable addresses, and the `vtable_size` that positions a static table after
//! one. Those are allocation results rather than layout: they differ between
//! two runs of the same build, so a fixture recording them would be recording
//! one process's luck. [`build`] assigns its own and keeps them consistent.
//!
//! Field offsets are the opposite case. The builder synthesises Mono's
//! metadata as well as its heap, so it could invent offsets and the splitter
//! would read them back happily — and the suite would then be testing itself.
//! Using the game's real offsets is what makes a fixture a model of the game,
//! and what gives the builder-versus-snapshot comparison anything to compare.
//!
//! # What a fixture flattens
//!
//! A field the splitter asks a class for may actually be declared on a parent;
//! `Class::get_field_offset` walks the chain and does not say where it stopped.
//! A fixture therefore records the *flattened* view — every field on the class
//! that answers for it — which is exactly the view the splitter consumes. The
//! synthetic class hierarchy is one level deep as a result.

use std::{fmt::Write as _, fs, path::Path, path::PathBuf};

use serde_json::{json, Value};

mod build;
pub use build::{Builder, ClassLayout};

/// The schema version of `fixtures/*.json`.
///
/// Bumped when an older file could be misread rather than merely be missing
/// something. [`Fixture::from_json`] refuses anything else, so a stale fixture
/// says so instead of building a subtly wrong world.
pub const FORMAT: u64 = 1;

/// Everything one game build's layout is, as committed.
#[derive(Clone, Debug)]
pub struct Fixture {
    /// The game's own version string, as the snapshot manifest records it.
    pub game_version: String,
    /// Steam's build id for that version, when the snapshot knew it.
    pub build_id: Option<u64>,
    /// What the facts were read out of, for a reader asking "says who".
    pub sources: Sources,
    pub classes: Vec<ClassFacts>,
}

/// Where a fixture's two halves came from.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    /// The snapshot the offsets were resolved against, by directory name.
    pub snapshot: String,
    /// The `Managed` directory the names and types were read out of.
    pub managed: String,
}

/// One class, and the fields of it the splitter depends on.
///
/// Not every field the class has: a fixture carries what `src/probe.rs` names,
/// which is the set whose disappearance would break something. A class with an
/// empty field list is here because the splitter validates an instance by
/// finding the class at all.
#[derive(Clone, Debug)]
pub struct ClassFacts {
    /// Assembly name, as Mono knows it and as `Image` is looked up by.
    pub image: String,
    pub namespace: String,
    pub name: String,
    pub fields: Vec<FieldFacts>,
}

/// One field: what it is called, what it holds, and where it sits.
#[derive(Clone, Debug)]
pub struct FieldFacts {
    /// The name Mono holds, which for an auto-property is the mangled
    /// `<Name>k__BackingField`. The builder writes *this* into memory, so a
    /// lookup goes through asr's backing-name path exactly as it does against
    /// the game.
    pub name: String,
    /// The name the splitter asks for, when it differs from `name`.
    pub requested: Option<String>,
    /// The declared type, rendered from the field's signature. Documentation
    /// and diff bait: a field that quietly changed type is worth seeing.
    pub declared_type: String,
    /// A static field's offset is from the start of the class's static table
    /// rather than from an instance.
    pub is_static: bool,
    pub offset: u32,
}

impl FieldFacts {
    /// The name the splitter asks for, mangled or not.
    pub fn requested_name(&self) -> &str {
        self.requested.as_deref().unwrap_or(&self.name)
    }
}

impl ClassFacts {
    pub fn field(&self, name: &str) -> Option<&FieldFacts> {
        self.fields
            .iter()
            .find(|f| f.name == name || f.requested_name() == name)
    }
}

impl Fixture {
    pub fn class(&self, image: &str, name: &str) -> Option<&ClassFacts> {
        self.classes
            .iter()
            .find(|c| c.image == image && c.name == name)
    }

    /// Builds the synthetic world this fixture describes.
    pub fn builder(&self) -> Builder {
        Builder::new(self)
    }
}

/// Where fixtures live by default: `<repo>/fixtures`, so a fresh clone works
/// with no configuration. `TIMBERBORN_FIXTURES` overrides it.
///
/// Unlike snapshots, these are committed — they are small, and they are facts
/// rather than a copy of the game's data.
pub fn default_dir() -> PathBuf {
    match std::env::var_os("TIMBERBORN_FIXTURES") {
        Some(path) => PathBuf::from(path),
        // The harness crate lives one directory below the repo root.
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the harness crate always has a parent directory")
            .join("fixtures"),
    }
}

/// Every committed fixture, oldest game version first by filename.
///
/// More than one is the point rather than an accident: the splitter's whole
/// claim is that it resolves names at runtime and so survives a game update,
/// and a suite that runs against two builds is where that stops being an
/// assertion.
///
/// # Errors
///
/// If the directory cannot be read, or if any fixture in it is malformed —
/// naming the file. A fixture that cannot be parsed is a broken commit, not a
/// reason to quietly test against one build fewer.
pub fn load_all() -> Result<Vec<Fixture>, String> {
    let dir = default_dir();
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("reading {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();

    if paths.is_empty() {
        return Err(format!(
            "no fixtures in {}.\n\nTo make one, with the game installed and a \
             snapshot captured:\n    cargo fixture --managed \
             <Timberborn_Data/Managed>\n\nSee fixtures/README.md.",
            dir.display()
        ));
    }

    paths.iter().map(|path| load(path)).collect()
}

/// Reads one fixture, naming the file if it will not parse.
pub fn load(path: &Path) -> Result<Fixture, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    Fixture::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))
}

// The schema is mapped by hand rather than derived. It is small, it is read
// far more often than it is written, and a fixture that is wrong should fail
// naming the class it stumbled on -- which a derived error cannot do.

fn field<'a>(value: &'a Value, key: &str, whose: &str) -> Result<&'a Value, String> {
    value
        .get(key)
        .ok_or_else(|| format!("{whose} has no {key:?}"))
}

fn string(value: &Value, key: &str, whose: &str) -> Result<String, String> {
    field(value, key, whose)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{whose}: {key:?} is not a string"))
}

impl Fixture {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;

        let format = field(&root, "format", "the fixture")?
            .as_u64()
            .ok_or("the fixture's \"format\" is not a number")?;
        if format != FORMAT {
            return Err(format!(
                "fixture format {format}, but this harness reads {FORMAT}; \
                 regenerate it with `cargo fixture`"
            ));
        }

        let sources = root.get("sources").cloned().unwrap_or_else(|| json!({}));
        let mut classes = Vec::new();
        for (index, value) in field(&root, "classes", "the fixture")?
            .as_array()
            .ok_or("the fixture's \"classes\" is not an array")?
            .iter()
            .enumerate()
        {
            let whose = format!("class {index}");
            let image = string(value, "image", &whose)?;
            let name = string(value, "name", &whose)?;
            let whose = format!("{image}/{name}");

            let mut fields = Vec::new();
            for value in field(value, "fields", &whose)?
                .as_array()
                .ok_or_else(|| format!("{whose}: \"fields\" is not an array"))?
            {
                let field_name = string(value, "name", &whose)?;
                let whose = format!("{whose}.{field_name}");
                fields.push(FieldFacts {
                    requested: value
                        .get("requested")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    declared_type: string(value, "type", &whose)?,
                    is_static: field(value, "static", &whose)?
                        .as_bool()
                        .ok_or_else(|| format!("{whose}: \"static\" is not a boolean"))?,
                    offset: field(value, "offset", &whose)?
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or_else(|| format!("{whose}: \"offset\" is not a 32-bit number"))?,
                    name: field_name,
                });
            }

            classes.push(ClassFacts {
                namespace: string(value, "namespace", &whose)?,
                image,
                name,
                fields,
            });
        }

        Ok(Self {
            game_version: string(&root, "game_version", "the fixture")?,
            build_id: root.get("build_id").and_then(Value::as_u64),
            sources: Sources {
                snapshot: string(&sources, "snapshot", "the fixture's \"sources\"")
                    .unwrap_or_default(),
                managed: string(&sources, "managed", "the fixture's \"sources\"")
                    .unwrap_or_default(),
            },
            classes,
        })
    }

    /// The fixture as it is committed: stable key order, one field per line,
    /// so a regenerated file diffs against the old one readably.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\n");
        let _ = writeln!(out, "  \"format\": {FORMAT},");
        let _ = writeln!(
            out,
            "  \"game_version\": {},",
            Value::from(self.game_version.clone())
        );
        if let Some(build_id) = self.build_id {
            let _ = writeln!(out, "  \"build_id\": {build_id},");
        }
        let _ = writeln!(out, "  \"sources\": {{");
        let _ = writeln!(
            out,
            "    \"snapshot\": {},",
            Value::from(self.sources.snapshot.clone())
        );
        let _ = writeln!(
            out,
            "    \"managed\": {}",
            Value::from(self.sources.managed.clone())
        );
        let _ = writeln!(out, "  }},");

        let _ = writeln!(out, "  \"classes\": [");
        for (i, class) in self.classes.iter().enumerate() {
            let _ = writeln!(out, "    {{");
            let _ = writeln!(
                out,
                "      \"image\": {},",
                Value::from(class.image.clone())
            );
            let _ = writeln!(
                out,
                "      \"namespace\": {},",
                Value::from(class.namespace.clone())
            );
            let _ = writeln!(out, "      \"name\": {},", Value::from(class.name.clone()));
            if class.fields.is_empty() {
                let _ = writeln!(out, "      \"fields\": []");
            } else {
                let _ = writeln!(out, "      \"fields\": [");
                for (j, f) in class.fields.iter().enumerate() {
                    let mut parts = vec![format!("\"name\": {}", Value::from(f.name.clone()))];
                    if let Some(requested) = &f.requested {
                        parts.push(format!("\"requested\": {}", Value::from(requested.clone())));
                    }
                    parts.push(format!(
                        "\"type\": {}",
                        Value::from(f.declared_type.clone())
                    ));
                    parts.push(format!("\"static\": {}", f.is_static));
                    parts.push(format!("\"offset\": {}", f.offset));
                    let comma = if j + 1 == class.fields.len() { "" } else { "," };
                    let _ = writeln!(out, "        {{ {} }}{comma}", parts.join(", "));
                }
                let _ = writeln!(out, "      ]");
            }
            let comma = if i + 1 == self.classes.len() { "" } else { "," };
            let _ = writeln!(out, "    }}{comma}");
        }
        let _ = writeln!(out, "  ]");
        out.push_str("}\n");
        out
    }
}
