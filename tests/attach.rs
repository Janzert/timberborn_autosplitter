//! The splitter's search loop, driven with no game to find.
//!
//! The first thing the harness had to do: enough of a fake runtime to run the
//! splitter natively. Nothing here reads memory yet -- these cases are about
//! whether it looks, what it says while looking, and what it declines to do.

use test_harness::{
    memory::{FakeProcess, SparseMemory},
    timer::TimerEvent,
    World,
};

/// `STILL_LOOKING_AFTER` is 15 detached ticks, and the notice repeats every
/// `REPEAT_SEARCH_NOTICE_TICKS`. 20 ticks clears the first without reaching the
/// second.
const PAST_FIRST_NOTICE: usize = 20;

#[test]
fn announces_itself_and_starts_looking() {
    let world = test_harness::drive(World::new(), timberborn_autosplitter::main(), 1);

    assert!(world.logged("Timberborn auto splitter."));
    assert_eq!(
        world.tick_rate,
        Some(1.0),
        "the search loop should drop to the detached rate rather than spin at 120/s"
    );
}

#[test]
fn says_so_when_there_is_no_game() {
    let world = test_harness::drive(
        World::new(),
        timberborn_autosplitter::main(),
        PAST_FIRST_NOTICE,
    );

    assert!(
        world.logged("Still looking for Timberborn..."),
        "silence and failure looked identical before this message existed; log was {:#?}",
        world.log
    );
    assert_eq!(
        world.timer.run_control().count(),
        0,
        "there is nothing to time; events were {:#?}",
        world.timer.events
    );
}

/// A warning from a previous session survives a module reload, so the splitter
/// blanks the status variable on startup rather than leaving a stale message
/// about a game that is no longer running.
#[test]
fn clears_a_stale_status_on_startup() {
    let world = test_harness::drive(World::new(), timberborn_autosplitter::main(), 1);

    assert_eq!(
        world.timer.events,
        [TimerEvent::SetVariable {
            key: "Timberborn Autosplitter".into(),
            value: String::new(),
        }]
    );
}

#[test]
fn ignores_processes_that_are_not_the_game() {
    let world = World::new()
        .with_process(FakeProcess::new(100, "firefox"))
        .with_process(FakeProcess::new(101, "steam"));
    let world = test_harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

    assert!(world.logged("Still looking for Timberborn..."));
}

/// Unity 6.5 names its main thread "Unity Main Thread", so a Proton install
/// reports `Unity Main Thre` -- 15 characters of `/proc/<pid>/comm` -- and
/// never matches the executable name. Any Unity game would match that, so the
/// splitter must confirm Timberborn's own module before accepting one.
#[test]
fn refuses_an_ambiguous_name_without_the_game_module() {
    let world = World::new().with_process(FakeProcess::new(200, "Unity Main Thre").with_module(
        "UnityPlayer.dll",
        0x1000,
        0x1000,
    ));
    let world = test_harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

    assert!(
        world.logged("Still looking for Timberborn..."),
        "a Unity process with no Timberborn.exe is some other game; log was {:#?}",
        world.log
    );
}

#[test]
fn accepts_an_ambiguous_name_that_has_the_game_module() {
    let world = World::new().with_process(
        FakeProcess::new(200, "Unity Main Thre")
            .with_module("Timberborn.exe", 0x140000000, 0x10000)
            .with_memory(SparseMemory::new()),
    );
    let world = test_harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

    assert!(world.logged("Attached to pid"), "log was {:#?}", world.log);
    assert!(!world.logged("Still looking for Timberborn..."));
}

#[test]
fn attaches_to_the_executable_name_directly() {
    let world = World::new().with_process(FakeProcess::new(300, "Timberborn.x86_64"));
    let world = test_harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

    assert!(!world.logged("Still looking for Timberborn..."));
}

/// The splitter registers eleven splits: the eight wonder ones on, the three
/// Timberbot ones off.
///
/// The defaults are the whole of the interaction between the two routes.
/// Smelter and Smelter + Wood Workshop read the same building, so shipping
/// both on would give a runner who never opened the settings two splits out of
/// one Smelter. Asserting the values here, not just the keys, is what keeps
/// that from being reintroduced by a copied `#[default = true]`.
#[test]
fn registers_its_settings() {
    let world = test_harness::drive(World::new(), timberborn_autosplitter::main(), 1);

    let keys: Vec<&str> = world
        .registered_settings
        .iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "start",
            "forester",
            "gear_workshop",
            "tappers_shack",
            "advanced_science",
            "smelter_woodworkshop",
            "unlock_wonder",
            "congratulations_screen",
            "smelter",
            "bot_part_factory",
            "first_bot",
            "wellbeing_15",
        ]
    );
    let off: Vec<&str> = world
        .registered_settings
        .iter()
        .filter(|(_, default)| !*default)
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(
        off,
        ["smelter", "bot_part_factory", "first_bot", "wellbeing_15"]
    );
}

/// Every setting a recorded scenario asks `tb-record` to force is a setting
/// the splitter actually has.
///
/// The recorder turns these on so a scenario whose triggers ship off records
/// the splits it was made for. A renamed key would not fail anything: the
/// harness would file it under a name nothing reads, the splitter would keep
/// its default, and the recording would capture the run start and then go
/// quiet -- hours of playing for a snapshot missing the thing it is of, with
/// nothing about it looking wrong. So the catalogue is checked against the
/// register here, where it costs a millisecond.
#[test]
fn every_setting_a_scenario_forces_is_one_the_splitter_registers() {
    let world = test_harness::drive(World::new(), timberborn_autosplitter::main(), 1);
    let registered: Vec<&str> = world
        .registered_settings
        .iter()
        .map(|(key, _)| key.as_str())
        .collect();

    for requirement in test_harness::requirement::CATALOGUE {
        for (key, _) in requirement.settings {
            assert!(
                registered.contains(key),
                "the {:?} scenario forces the setting {key:?}, which the \
                 splitter does not register. Registered: {registered:?}.",
                requirement.id
            );
        }
    }
}
