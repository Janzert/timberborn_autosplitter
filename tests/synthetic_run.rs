//! The splitter driven against a world built from the fixture.
//!
//! The snapshot suite does this against a capture of a real game, and cannot
//! run anywhere without one. These ask the same questions of a world assembled
//! from committed facts, so they run in `cargo test`, on any machine, in
//! milliseconds rather than half a minute.
//!
//! Both suites stay: a disagreement between them is the signal that the
//! fixture no longer describes the game, and `tests/fixture_vs_snapshot.rs` is
//! where that gets checked directly.
//!
//! What these can never establish is that the world is shaped like the game's.
//! It is shaped like what we believe the game's is — see
//! `fixtures/README.md`.

use test_harness::{fixture, fixture::game::Scene, World};

/// Ticks to run. The synthetic heap is a few hundred KiB, so a sweep of it
/// costs one slice rather than the hundreds a real 5 GiB heap needs.
const TICKS: usize = 400;

/// `InitializationState.Finished`, the state a game that is up and running
/// sits at. The splitter's own copy of this is private, and duplicating it is
/// the point: a test that imported the constant could not notice the game
/// renumbering the enum, which is exactly the kind of change worth catching.
const FINISHED: i32 = 5;

/// A loaded game with the services the splitter looks for, and nothing
/// happening: the mid-run attach case, which is the one a still world models
/// honestly.
fn game_in_progress(fixture: &fixture::Fixture, day: i32) -> World {
    let mut scene = Scene::new(fixture);

    let clock = scene.service("Timberborn.TimeSystem", "DayNightCycle");
    scene.set_i32(&clock, "DayNumber", day);

    scene.service("Timberborn.ScienceSystem", "BuildingUnlockingService");
    scene.service(
        "Timberborn.GameWonderCompletion",
        "WonderCompletionCountdownStarter",
    );
    // Not located by scanning: the splitter reaches these through the
    // container, so they need no event bus to be recognised by.
    let game_over = scene.object("Timberborn.GameOver", "GameOverChecker");
    scene.register(&game_over);

    let population = scene.object("Timberborn.Population", "PopulationService");
    scene.register(&population);
    let data = scene.object("Timberborn.Population", "PopulationData");
    scene.set_i32(&data, "NumberOfAdults", 12);
    scene.set_i32(&data, "NumberOfChildren", 3);
    scene.set_ptr(&population, "GlobalPopulationData", data.address);

    // The initializer the run start binds to. Already past the overlay, which
    // is what "attached to a game in progress" means and why no timer should
    // start from it.
    let initializer = scene.service("Timberborn.GameStartup", "GameInitializer");
    scene.set_i32(&initializer, "_initializationState", FINISHED);

    // Loaded, not loading: the splitter attaching to a game already running.
    let loader = scene.scene_loader(false, true);
    scene.register(&loader);

    World::new().with_process(scene.finish())
}

/// Drives every committed fixture and hands each run's log to `check`.
fn for_each_fixture(build: impl Fn(&fixture::Fixture) -> World, check: impl Fn(&str, &World)) {
    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let world = build(fixture);
        let world = test_harness::drive(world, timberborn_autosplitter::main(), TICKS);
        check(&fixture.game_version, &world);
    }
}

fn require(world: &World, version: &str, needle: &str) {
    assert!(
        world.logged(needle),
        "{version}: expected a log line containing {needle:?}; log was {:#?}",
        world.log
    );
}

/// The whole path from nothing to a bound service: attach to the process,
/// attach to Mono, resolve the event bus, sweep the heap for the scene loader,
/// find the DI container, and pull services out of it.
///
/// Reaching the last of those means every one before it worked, which is why
/// this is one test rather than six.
#[test]
fn finds_the_games_singleton_container_and_services() {
    for_each_fixture(
        |fixture| game_in_progress(fixture, 5),
        |version, world| {
            require(world, version, "Attached to the Mono runtime.");
            require(world, version, "Watching scene loads at");
            require(world, version, "The game's singleton container is at");
            require(world, version, "Found DayNightCycle at");
        },
    );
}

/// Attaching to a game already in progress must not start a timer. A runner
/// would see a run that silently began at the wrong time, which is the exact
/// failure this splitter exists to remove.
#[test]
fn refuses_to_start_a_timer_for_a_game_already_in_progress() {
    for_each_fixture(
        |fixture| game_in_progress(fixture, 5),
        |version, world| {
            require(world, version, "Not starting the timer");
            assert_eq!(
                world.timer.splits(),
                0,
                "{version}: a still world produced a split"
            );
        },
    );
}

/// Every name the design depends on resolves against the synthetic world.
///
/// The same assertion the snapshot suite makes, which is the point: the probe
/// is the splitter's own version check, and a fixture that could not satisfy
/// it would be describing a game the splitter cannot read.
#[test]
fn every_name_the_design_depends_on_resolves() {
    for_each_fixture(
        |fixture| game_in_progress(fixture, 5),
        |version, world| {
            require(world, version, "probe: ALL RESOLVED");
            assert!(
                !world.logged("MISSING"),
                "{version}: a name the design depends on did not resolve; log was {:#?}",
                world.log
            );
        },
    );
}
