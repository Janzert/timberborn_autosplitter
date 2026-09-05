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

    // Already past the overlay, which is what "a game in progress" means and
    // why no timer should start from it.
    let services = scene.core_services(FINISHED);
    scene.set_i32(&services.clock, "DayNumber", day);

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

/// The runtime's reference table is found, and found by content.
///
/// The synthetic table is at an address of the fixture's choosing, and the
/// real ones are at two different addresses again, so a splitter that found
/// this one by looking anywhere in particular would be finding the wrong
/// thing for the right reason.
#[test]
fn finds_the_reference_table() {
    for_each_fixture(
        |fixture| game_in_progress(fixture, 5),
        |version, world| {
            require(world, version, "[table] found at");
            require(world, version, "object headers in it");
        },
    );
}

/// Having found the table, the splitter uses it: the container is resolved
/// without sweeping the address space for it.
///
/// This is the whole point of the change. A run that still sweeps has fallen
/// back, which is safe but is not what the table was added for.
#[test]
fn resolves_the_container_without_sweeping_for_it() {
    for_each_fixture(
        |fixture| game_in_progress(fixture, 5),
        |version, world| {
            require(world, version, "[table] SingletonRepository:");
            assert!(
                !world.logged("[scan] SingletonRepository starting"),
                "{version}: swept for the container despite finding a table; log was {:#?}",
                world.log
            );
        },
    );
}

/// An object the table does not know about is still found, by sweeping.
///
/// The table holds what the runtime is holding, which a capture showed is not
/// provably everything the heap holds: a `SingletonRepository` that still
/// validated and still had its 103 singletons had no entry at all. Whether it
/// was dead or merely unheld cannot be told from a memory image, so the
/// splitter must not need the answer -- and this is the test that says so.
#[test]
fn sweeps_for_what_the_table_does_not_hold() {
    for_each_fixture(
        |fixture| {
            let mut scene = fixture::game::Scene::new(fixture).container_unreferenced();
            let services = scene.core_services(FINISHED);
            scene.set_i32(&services.clock, "DayNumber", 5);
            let loader = scene.scene_loader(false, true);
            scene.register(&loader);
            World::new().with_process(scene.finish())
        },
        |version, world| {
            // The table was found and asked, and had nothing.
            require(world, version, "[table] found at");
            require(world, version, "[table] SingletonRepository: 0 live instances");
            // And the sweep behind it got there anyway.
            require(world, version, "[scan] SingletonRepository starting");
            require(world, version, "The game's singleton container is at");
        },
    );
}

/// With no reference table at all, everything still works — by sweeping.
///
/// A game or engine version where the table cannot be found is not a failure,
/// it is the behaviour this splitter had before the table existed. What would
/// be a failure is not noticing, so the fallback says why it is sweeping.
#[test]
fn works_with_no_reference_table_at_all() {
    for_each_fixture(
        |fixture| {
            let mut scene = fixture::game::Scene::new(fixture).without_reference_table();
            let services = scene.core_services(FINISHED);
            scene.set_i32(&services.clock, "DayNumber", 5);
            let loader = scene.scene_loader(false, true);
            scene.register(&loader);
            World::new().with_process(scene.finish())
        },
        |version, world| {
            require(world, version, "no candidate range was one");
            require(world, version, "full sweep -- no reference table found yet");
            require(world, version, "The game's singleton container is at");
            require(world, version, "DayNumber = 5");
        },
    );
}

/// Looking for the table is itself a sweep, so a version where it cannot be
/// found must not go on looking on the menu's one-second loop.
///
/// This is the pathological case: sitting on the main menu with no table
/// findable. Unbounded, it is a full sweep every second — a second apiece on
/// Linux and half a minute on Windows — for as long as the runner sits there.
#[test]
fn gives_up_looking_for_a_table_rather_than_sweeping_forever() {
    // Long enough for several turns of the menu loop, which waits ~120 ticks
    // between passes.
    const MENU_TICKS: usize = 1200;
    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let mut scene = fixture::game::Scene::new(fixture).without_reference_table();
        // The clock has to exist or the splitter waits for a game and never
        // reaches the menu branch at all — which is itself why the menu branch
        // is only reachable once a game has been loaded at some point.
        let services = scene.core_services(FINISHED);
        scene.set_i32(&services.clock, "DayNumber", 1);
        let loader = scene.scene_loader_for(
            false,
            "Timberborn.MainMenuSceneLoading",
            "MainMenuSceneParameters",
        );
        scene.register(&loader);
        let world = World::new().with_process(scene.finish());
        let world = test_harness::drive(world, timberborn_autosplitter::main(), MENU_TICKS);

        let looked = world
            .log
            .iter()
            .filter(|line| line.contains("[table] looking for"))
            .count();
        assert_eq!(
            looked, 3,
            "{}: looked for the table {looked} times in {MENU_TICKS} ticks; log was {:#?}",
            fixture.game_version, world.log
        );
        assert!(
            world.logged("no reference table after three tries"),
            "{}: gave up without saying so; log was {:#?}",
            fixture.game_version,
            world.log
        );
    }
}
