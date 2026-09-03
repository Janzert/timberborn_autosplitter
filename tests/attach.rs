//! The splitter's search loop, driven with no game to find.
//!
//! Phase 1 of TEST_HARNESS_PLAN.md: enough of a fake runtime to run the
//! splitter natively. Nothing here reads memory yet -- these cases are about
//! whether it looks, what it says while looking, and what it declines to do.

use harness::{
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
    let world = harness::drive(World::new(), timberborn_autosplitter::main(), 1);

    assert!(world.logged("Timberborn auto splitter."));
    assert_eq!(
        world.tick_rate,
        Some(1.0),
        "the search loop should drop to the detached rate rather than spin at 120/s"
    );
}

#[test]
fn says_so_when_there_is_no_game() {
    let world = harness::drive(
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
    let world = harness::drive(World::new(), timberborn_autosplitter::main(), 1);

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
    let world = harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

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
    let world = harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

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
    let world = harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

    assert!(world.logged("Attached to pid"), "log was {:#?}", world.log);
    assert!(!world.logged("Still looking for Timberborn..."));
}

#[test]
fn attaches_to_the_executable_name_directly() {
    let world = World::new().with_process(FakeProcess::new(300, "Timberborn.x86_64"));
    let world = harness::drive(world, timberborn_autosplitter::main(), PAST_FIRST_NOTICE);

    assert!(!world.logged("Still looking for Timberborn..."));
}

/// The splitter registers eight splits, all defaulting on.
#[test]
fn registers_its_settings() {
    let world = harness::drive(World::new(), timberborn_autosplitter::main(), 1);

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
        ]
    );
    assert!(world
        .registered_settings
        .iter()
        .all(|(_, default)| *default));
}
