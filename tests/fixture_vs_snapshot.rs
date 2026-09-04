//! The synthetic world against the captured one, asked the same questions.
//!
//! This is the test that makes the synthetic suite worth having. Without it,
//! `fixture_world.rs` establishes only that the builder and the fixture agree
//! with each other — and they will agree just as happily about something we
//! misunderstood. By this project's record, "we misunderstood the game" is the
//! expensive kind of bug: the districts' finished-building registries not
//! listing every building, `TributeToIngenuity`, Mono's lazily-filled field
//! tables. None of those would have been caught by a suite that only asked
//! itself.
//!
//! So the capture is the oracle. Both processes go into one world, asr attaches
//! to each, and every question the splitter ever asks Mono is put to both:
//!
//! ```text
//!   snapshot ──► asr ──► class, offset, vtable ◄── asr ◄── synthetic
//!                             compare
//! ```
//!
//! A disagreement means the fixture no longer describes the game — which is
//! exactly the signal wanted, and the reason both suites stay running until
//! they have agreed across a game build change.
//!
//! ```text
//! cargo snapshot-tests
//! ```

use std::path::PathBuf;

use asr::{game_engine::unity::mono::Module, Process, ProcessId};
use test_harness::{
    fixture::{self, Fixture},
    snapshot::Snapshot,
    World,
};

/// The state to compare against: everything the splitter touches, resolved.
///
/// Mono loads assemblies lazily, so half of these classes do not exist at the
/// main menu. A finished run has all of them.
const STATE: &str = "run-finished";

/// A pid for the synthetic process that no captured one will have. Linux pids
/// stop well below this.
const SYNTHETIC_PID: u64 = 1 << 40;

/// Classes that a capture of a *wonder run* will not have constructed.
///
/// Mono fills a class's vtable in when the class is first instantiated, so
/// this is a statement about what the captured session did rather than about
/// the build. The map editor was never opened, so `MapEditorSceneParameters`
/// has no vtable -- and the splitter's map-editor arm compares against exactly
/// that vtable.
///
/// That arm is safe anyway, and for a reason worth writing down: the object
/// being classified *is* a `MapEditorSceneParameters`, so by the time the
/// comparison happens Mono has constructed the class and the vtable exists. It
/// is missing here precisely because nothing in this capture ever loaded that
/// scene.
const UNCONSTRUCTED: &[&str] = &["Timberborn.MapEditorSceneLoading/MapEditorSceneParameters"];

/// One class, as either world answered for it.
#[derive(Debug, PartialEq, Eq)]
struct Resolved {
    /// The offset of each field, in the fixture's order, asked for by the name
    /// the splitter uses.
    offsets: Vec<Option<u32>>,
    /// Whether the class has a vtable at all. Mono fills one in when the
    /// class is first constructed, so in a capture this is a statement about
    /// that session rather than about the build.
    has_vtable: bool,
    /// Whether a static table was reachable. Compared rather than asserted:
    /// only some of these classes have static fields.
    has_static_table: bool,
    /// Whether the class is also findable by its fully qualified name, which
    /// is the only check the fixture's recorded namespace ever gets.
    found_by_qualified_name: bool,
}

/// Every question this suite knows how to ask about a class, put to one world.
fn resolve(
    process: &Process,
    module: &Module,
    fixture: &Fixture,
) -> Vec<(String, Option<Resolved>)> {
    fixture
        .classes
        .iter()
        .map(|facts| {
            let name = format!("{}/{}", facts.image, facts.name);
            let Some(image) = module.get_image(process, &facts.image) else {
                return (name, None);
            };
            let Some(class) = image.get_class(process, module, &facts.name) else {
                return (name, None);
            };

            let resolved = Resolved {
                offsets: facts
                    .fields
                    .iter()
                    .map(|field| class.get_field_offset(process, module, field.requested_name()))
                    .collect(),
                has_vtable: class.get_vtable(process, module).is_some(),
                has_static_table: class.get_static_table(process, module).is_some(),
                found_by_qualified_name: image
                    .get_class(
                        process,
                        module,
                        &format!("{}.{}", facts.namespace, facts.name),
                    )
                    .is_some(),
            };
            (name, Some(resolved))
        })
        .collect()
}

/// The snapshot to compare a fixture against: the same build, nothing else.
fn snapshot_for(fixture: &Fixture) -> PathBuf {
    let dirs = test_harness::snapshot::find_all(STATE).unwrap_or_else(|e| panic!("{e}"));
    for dir in dirs {
        if Snapshot::open(&dir).is_ok_and(|s| s.metadata.game_version == fixture.game_version) {
            return dir;
        }
    }
    panic!(
        "no {STATE:?} capture of {}, which fixtures/{}.json describes.\n\n\
         A fixture and a capture of the same build are what this compares. Either \
         capture one -- see snapshots/README.md -- or, if that build is gone, retire \
         the fixture rather than leaving it unchecked.",
        fixture.game_version, fixture.game_version
    );
}

/// Puts both worlds' answers side by side, for every committed fixture.
fn compare(check: impl Fn(&Fixture, &[(String, Option<Resolved>)], &[(String, Option<Resolved>)])) {
    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let dir = snapshot_for(fixture);
        let snapshot = Snapshot::open(&dir).expect("opening the snapshot");
        let captured_pid = ProcessId(snapshot.metadata.pid.into());

        let world = World::new()
            .with_process(snapshot.process())
            .with_process(fixture.builder().with_pid(SYNTHETIC_PID).finish());

        test_harness::drive(
            world,
            async {
                let captured = Process::attach_by_pid(captured_pid)
                    .expect("attaching to the captured process");
                let captured_module =
                    Module::attach_auto_detect(&captured).expect("the capture's Mono module");
                let synthetic = Process::attach_by_pid(ProcessId(SYNTHETIC_PID))
                    .expect("attaching to the synthetic process");
                let synthetic_module =
                    Module::attach_auto_detect(&synthetic).expect("the synthetic Mono module");

                check(
                    fixture,
                    &resolve(&captured, &captured_module, fixture),
                    &resolve(&synthetic, &synthetic_module, fixture),
                );
            },
            1,
        );
    }
}

/// Every class the fixture names is findable in both worlds, by the same name.
#[test]
fn both_worlds_have_the_same_classes() {
    compare(|fixture, captured, synthetic| {
        for ((name, captured), (_, synthetic)) in captured.iter().zip(synthetic) {
            assert!(
                captured.is_some(),
                "{}: the fixture names {name}, and the capture has no such class. \
                 The fixture describes a build this capture is not of, or the class \
                 was renamed.",
                fixture.game_version
            );
            assert!(
                synthetic.is_some(),
                "{}: {name} is in the fixture but the builder did not place it",
                fixture.game_version
            );
        }
    });
}

/// The comparison the whole thing is for: what Mono laid the class out as, and
/// what the synthetic world says it did, are the same numbers.
#[test]
fn every_field_is_at_the_same_offset_in_both() {
    compare(|fixture, captured, synthetic| {
        for ((name, captured), (_, synthetic)) in captured.iter().zip(synthetic) {
            let (Some(captured), Some(synthetic)) = (captured, synthetic) else {
                continue; // reported by both_worlds_have_the_same_classes
            };
            assert_eq!(
                captured.offsets, synthetic.offsets,
                "{}: {name} is laid out differently in the capture and in the \
                 world built from the fixture. Regenerate the fixture -- \
                 `cargo fixture` -- and read the diff before committing it.",
                fixture.game_version
            );
        }
    });
}

/// Every class has a vtable in both worlds, bar the ones the capture never
/// constructed.
///
/// A vtable is what makes a class identifiable on the heap: an object's first
/// pointer is its class's, and the splitter has no static root to walk from, so
/// a heap sweep is the only way it finds a service at all. The addresses
/// themselves are allocation results and cannot be compared -- what can is
/// whether the relationship exists.
///
/// The exceptions are listed in [`UNCONSTRUCTED`] with the reason, so a class
/// that stops being constructed shows up as a new failure rather than
/// disappearing into a tolerance.
#[test]
fn every_class_has_a_vtable_in_both() {
    compare(|fixture, captured, synthetic| {
        for ((name, captured), (_, synthetic)) in captured.iter().zip(synthetic) {
            let (Some(captured), Some(synthetic)) = (captured, synthetic) else {
                continue;
            };
            assert!(
                synthetic.has_vtable,
                "{}: the builder gave {name} no vtable",
                fixture.game_version
            );
            assert_eq!(
                captured.has_vtable,
                !UNCONSTRUCTED.contains(&name.as_str()),
                "{}: {name} has a vtable in the capture, or does not, and \
                 UNCONSTRUCTED in this file says the opposite. A class the \
                 splitter matches by vtable and that is never constructed can \
                 never match, so this is worth understanding rather than \
                 relisting.",
                fixture.game_version
            );
        }
    });
}

/// The recorded namespace is a fact from the assemblies that nothing else ever
/// checks against a runtime. asr matches a qualified name by namespace, so
/// asking for one is what tests it.
#[test]
fn the_recorded_namespace_is_the_runtime_namespace() {
    compare(|fixture, captured, synthetic| {
        for ((name, captured), (_, synthetic)) in captured.iter().zip(synthetic) {
            let (Some(captured), Some(synthetic)) = (captured, synthetic) else {
                continue;
            };
            assert!(
                captured.found_by_qualified_name,
                "{}: {name} is not in the namespace the fixture records for it",
                fixture.game_version
            );
            assert_eq!(
                captured.found_by_qualified_name, synthetic.found_by_qualified_name,
                "{}: {name} is qualified differently in the two worlds",
                fixture.game_version
            );
        }
    });
}

/// A class the fixture records a static field for has a static table in both
/// worlds.
///
/// Only that direction. A fixture holds the fields the splitter reads and no
/// others, so a class can perfectly well have statics in the game that the
/// fixture does not know about -- `AssetLoader` does, and is in the fixture at
/// all only because finding the class is how a `SceneLoader` instance gets
/// validated. A synthetic world without those is a partial model, not a wrong
/// one.
///
/// Where the fixture *does* record a static, the table has to be reachable in
/// both: that is the chain from vtable past `vtable_size` function slots, and
/// getting it wrong lands in the middle of them silently.
#[test]
fn a_recorded_static_field_has_a_table_in_both() {
    compare(|fixture, captured, synthetic| {
        for (((name, captured), (_, synthetic)), facts) in
            captured.iter().zip(synthetic).zip(&fixture.classes)
        {
            let (Some(captured), Some(synthetic)) = (captured, synthetic) else {
                continue;
            };
            if !facts.fields.iter().any(|field| field.is_static) {
                continue;
            }
            assert!(
                captured.has_static_table,
                "{}: the fixture records a static field on {name}, and the \
                 capture has no static table to read it out of",
                fixture.game_version
            );
            assert!(
                synthetic.has_static_table,
                "{}: the builder placed no static table for {name}",
                fixture.game_version
            );
        }
    });
}
