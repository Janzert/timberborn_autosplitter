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

/// Classes whose vtable may legitimately be missing from a capture.
///
/// Mono fills a class's vtable in when the class is first *instantiated*, so a
/// missing one is a statement about what the captured session did rather than
/// about the build. These are the scene-parameters classes, and a session only
/// constructs one by loading that kind of scene. Measured across two captures
/// of the same category:
///
/// | capture | main menu | map editor |
/// |---|---|---|
/// | 1.0.13.1 run-finished | absent | absent |
/// | 1.1.2.4 run-complete-frozen | present | absent |
///
/// So this cannot be a fixed expectation either way, which is why it is a
/// permission rather than a prediction: everything *not* listed here must have
/// a vtable, and a service without one could never be found by the heap sweep
/// that is the splitter's only way to reach it.
///
/// **The splitter is safe against the absence, for a reason worth writing
/// down.** It compares a scene's parameters object against these vtables to
/// classify the scene — and the object it is holding *is* an instance of one
/// of them, so by the time any comparison happens Mono has constructed that
/// class and its vtable exists. A class is missing here precisely when no
/// scene of its kind was ever loaded, which is when nothing asks.
const MAY_BE_UNCONSTRUCTED: &[&str] = &[
    "Timberborn.MainMenuSceneLoading/MainMenuSceneParameters",
    "Timberborn.MapEditorSceneLoading/MapEditorSceneParameters",
];

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

/// Every class has a vtable in the synthetic world, and in the capture too
/// unless it is one no session need ever construct.
///
/// A vtable is what makes a class identifiable on the heap: an object's first
/// pointer is its class's, and the splitter has no static root to walk from, so
/// a heap sweep is the only way it finds a service at all. The addresses
/// themselves are allocation results and cannot be compared -- what can is
/// whether the relationship exists.
///
/// The exceptions are listed in [`MAY_BE_UNCONSTRUCTED`] with the reason, and
/// listed narrowly: a *service* without a vtable is still a failure, because
/// nothing could ever find it.
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
            assert!(
                captured.has_vtable || MAY_BE_UNCONSTRUCTED.contains(&name.as_str()),
                "{}: {name} was never constructed in the captured session, so \
                 nothing that sweeps the heap for it could have worked. Only \
                 the scene-parameters classes may legitimately be missing; see \
                 MAY_BE_UNCONSTRUCTED in this file.",
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
