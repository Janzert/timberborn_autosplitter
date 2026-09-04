//! The synthetic world, checked against the thing that has to believe it.
//!
//! The builder writes Mono's structures from memory of what asr reads, and
//! nothing about that is self-evidently right — a wrong constant produces a
//! world that is perfectly self-consistent and that asr cannot attach to at
//! all. So these ask asr directly, rather than inferring it from the
//! splitter's behaviour further up.
//!
//! What they do **not** establish is that the fixture matches the game. A
//! builder and a fixture can agree with each other perfectly while both being
//! wrong about Timberborn; that is what the snapshot suite is the oracle for,
//! and what phase 5 of TEST_HARNESS_PLAN.md compares.
//!
//! These run in the default suite: a fixture is committed, so nothing here
//! needs a game, a capture, or a machine that has ever run Timberborn.

use asr::{
    game_engine::unity::mono::{Class, Module, Version},
    PointerSize, Process, ProcessId,
};
use test_harness::{
    fixture::{self, Fixture},
    World,
};

/// Runs `check` against every committed fixture, naming the one that failed.
///
/// Every fixture rather than the newest: the splitter's claim is that it
/// resolves names at runtime and so survives a game update, and a suite that
/// only ever builds the latest build's layout is not testing that claim.
fn for_each_fixture(check: impl Fn(&Fixture, &Process, &Module)) {
    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let process = fixture.builder().finish();
        let pid = ProcessId(process.pid);
        let world = World::new().with_process(process);

        // One poll: everything here is synchronous, and the fake runtime only
        // answers while `drive` is running.
        test_harness::drive(
            world,
            async {
                let process = Process::attach_by_pid(pid)
                    .unwrap_or_else(|| panic!("{}: cannot attach", fixture.game_version));
                let module = Module::attach_auto_detect(&process).unwrap_or_else(|| {
                    panic!(
                        "{}: asr could not attach to the synthetic Mono module",
                        fixture.game_version
                    )
                });
                check(fixture, &process, &module);
            },
            1,
        );
    }
}

/// The whole of `Module::attach`: the Unity version resource, the export
/// table, the RIP-relative displacement into the assembly list. Reaching a
/// version at all means every one of those was right.
#[test]
fn asr_attaches_to_the_synthetic_mono() {
    for_each_fixture(|fixture, _process, module| {
        assert_eq!(
            module.get_version(),
            Version::V3,
            "{}: Timberborn is Unity 6, which is V3",
            fixture.game_version
        );
        assert_eq!(
            module.get_pointer_size(),
            PointerSize::Bit64,
            "{}",
            fixture.game_version
        );
    });
}

/// Every assembly and class the fixture names is reachable by name, which is
/// the only way the splitter ever looks anything up.
#[test]
fn every_class_in_the_fixture_resolves_by_name() {
    for_each_fixture(|fixture, process, module| {
        for class in &fixture.classes {
            let image = module
                .get_image(process, &class.image)
                .unwrap_or_else(|| panic!("{}: no image {}", fixture.game_version, class.image));
            assert!(
                image.get_class(process, module, &class.name).is_some(),
                "{}: {} has no class {}",
                fixture.game_version,
                class.image,
                class.name
            );
        }
    });
}

/// The point of recording real offsets: what the fixture says a field is at is
/// what asr reads back.
///
/// Asked for by the name the *splitter* uses, which for an auto-property is
/// the plain one while memory holds `<Name>k__BackingField`. That difference
/// is why this is a test and not an identity — it goes through asr's
/// backing-name path exactly as it does against the game.
#[test]
fn every_field_resolves_to_the_offset_the_fixture_records() {
    for_each_fixture(|fixture, process, module| {
        for class in &fixture.classes {
            let resolved = module
                .get_image(process, &class.image)
                .and_then(|image| image.get_class(process, module, &class.name))
                .unwrap_or_else(|| {
                    panic!(
                        "{}: no {}/{}",
                        fixture.game_version, class.image, class.name
                    )
                });

            for field in &class.fields {
                let asked = field.requested_name();
                assert_eq!(
                    resolved.get_field_offset(process, module, asked),
                    Some(field.offset),
                    "{}: {}/{}.{asked}",
                    fixture.game_version,
                    class.image,
                    class.name,
                );
            }
        }
    });
}

/// An object placed on the synthetic heap resolves back to its own class.
///
/// This is how the splitter recognises a service: it has no static root to
/// walk from, so it sweeps the heap and asks each candidate what it is. A
/// builder whose vtables did not point back at their classes would leave that
/// whole path untestable, and it is the path the splitter is built around.
#[test]
fn an_object_on_the_heap_knows_its_own_class() {
    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let mut builder = fixture.builder();
        let expected = builder.expect_class("Timberborn.TimeSystem", "DayNightCycle");
        let object = builder.new_object("Timberborn.TimeSystem", "DayNightCycle", 0);

        let process = builder.finish();
        let pid = ProcessId(process.pid);
        let world = World::new().with_process(process);

        test_harness::drive(
            world,
            async {
                let process = Process::attach_by_pid(pid).expect("attaching");
                let module = Module::attach_auto_detect(&process).expect("attaching to Mono");

                let class =
                    Class::of_object(&process, &module, object.into()).unwrap_or_else(|| {
                        panic!(
                            "{}: an object at {object:#x} did not resolve to a class",
                            fixture.game_version
                        )
                    });
                assert_eq!(
                    class.get_vtable(&process, &module),
                    Some(expected.vtable.into()),
                    "{}: the object resolved to the wrong class",
                    fixture.game_version
                );
                assert_eq!(
                    class.get_field_offset(&process, &module, "DayNumber"),
                    fixture
                        .class("Timberborn.TimeSystem", "DayNightCycle")
                        .and_then(|c| c.field("DayNumber"))
                        .map(|f| f.offset),
                    "{}: reached the class but not its fields",
                    fixture.game_version
                );
            },
            1,
        );
    }
}

/// A static field's offset is into the class's static table, and asr finds
/// that table by stepping over the vtable's function slots. Getting
/// `vtable_size` wrong lands it in the middle of them, silently.
#[test]
fn a_static_field_is_read_out_of_the_static_table() {
    const IMAGE: &str = "Timberborn.ErrorReporting";
    const CLASS: &str = "WorldDataService";
    const FIELD: &str = "SourceFileName";
    const SENTINEL: u64 = 0xABCD_0123_4567_8000;

    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let facts = fixture
            .class(IMAGE, CLASS)
            .and_then(|c| c.field(FIELD))
            .unwrap_or_else(|| {
                panic!(
                    "{}: the fixture has no {IMAGE}/{CLASS}.{FIELD}",
                    fixture.game_version
                )
            });
        assert!(
            facts.is_static,
            "{}: {FIELD} is what makes this test worth having",
            fixture.game_version
        );

        let mut builder = fixture.builder();
        let table = builder
            .expect_class(IMAGE, CLASS)
            .static_table
            .unwrap_or_else(|| panic!("{}: no static table", fixture.game_version));
        builder.write_u64(table + u64::from(facts.offset), SENTINEL);

        let process = builder.finish();
        let pid = ProcessId(process.pid);
        let world = World::new().with_process(process);

        test_harness::drive(
            world,
            async {
                let process = Process::attach_by_pid(pid).expect("attaching");
                let module = Module::attach_auto_detect(&process).expect("attaching to Mono");
                let class = module
                    .get_image(&process, IMAGE)
                    .and_then(|image| image.get_class(&process, &module, CLASS))
                    .expect("the class");

                let table = class
                    .get_static_table(&process, &module)
                    .expect("the static table");
                let offset = class
                    .get_field_offset(&process, &module, FIELD)
                    .expect("the field");
                assert_eq!(
                    process.read::<u64>(table + u64::from(offset)).ok(),
                    Some(SENTINEL),
                    "{}: the static table is not where asr looks for it",
                    fixture.game_version
                );
            },
            1,
        );
    }
}
