//! The splitter and the game process: appearing, going, and coming back.
//!
//! Everything else in the suite hands the splitter a process that exists for
//! the whole run. Real sessions do not look like that, and the paths that only
//! exist at the edges are exactly the ones that have cost time before — the
//! cold two-cycle case took a play session per iteration to test by hand.
//!
//! The four edges, all of which a synthetic world can express and a recording
//! cannot:
//!
//! - the splitter started **before** the game, waiting and finding nothing;
//! - the game **closing** under it, and the splitter going back to looking;
//! - a **second game** in the same session, which is the cold two-cycle;
//! - attaching to a **dying** process, which stays attachable for several
//!   seconds after its memory has gone.
//!
//! This is the one place the recorded and synthetic suites do not line up:
//! `tb-record` stops when the game does, so a recording has no way to say "and
//! then there was no process".

use test_harness::{
    fixture::{self, game::Scene},
    memory::EmptyMemory,
    timer::TimerEvent,
    World,
};

/// `InitializationState`. Duplicated from the splitter rather than imported: a
/// test that took the splitter's own constants could not notice the game
/// renumbering the enum.
const WAITING: i32 = 0;
const FINISHED: i32 = 5;

/// Long enough for the splitter to attach, resolve Mono, sweep for the scene
/// loader and read the container. A real load is longer than this.
const SETTLE: usize = 400;

/// The splitter's detached tick rate is one a second, and it says it is still
/// looking after fifteen. Duplicated for the same reason the states are.
const SEARCH_NOTICE_TICKS: usize = 15;

/// A process running a game that is already up: nothing to time, everything to
/// find.
fn game_in_progress(fixture: &fixture::Fixture, pid: u64) -> test_harness::memory::FakeProcess {
    let mut scene = Scene::new(fixture);
    scene.core_services(FINISHED);
    let loader = scene.scene_loader(false, true);
    scene.register(&loader);
    let mut process = scene.finish();
    process.pid = pid;
    process
}

/// A process running a game that is still loading: a run start about to
/// happen.
fn game_loading(
    fixture: &fixture::Fixture,
    pid: u64,
) -> (test_harness::memory::FakeProcess, Loading) {
    let mut scene = Scene::new(fixture);
    let services = scene.core_services(WAITING);
    let loader = scene.scene_loader(true, true);
    scene.register(&loader);
    let (mut process, live) = scene.finish_live();
    process.pid = pid;
    (
        process,
        Loading {
            live,
            loader,
            initializer: services.initializer,
        },
    )
}

/// The handles a loading game is brought up through.
struct Loading {
    live: fixture::game::Live,
    loader: fixture::game::Object,
    initializer: fixture::game::Object,
}

impl Loading {
    /// The load finishes, then the overlay appears. Two moments, because the
    /// gap between them is what the run start is.
    fn load_ends(&self) {
        self.live.set_u8(&self.loader, "_isLoading", 0);
    }

    fn overlay(&self) {
        self.live
            .set_i32(&self.initializer, "_initializationState", FINISHED);
    }
}

/// Everything the timer was told, ignoring the variables set alongside.
fn controlling(world: &World) -> Vec<&TimerEvent> {
    world
        .timer
        .events
        .iter()
        .filter(|event| !matches!(event, TimerEvent::SetVariable { .. }))
        .collect()
}

/// Whether a log line appears at or after `from`.
fn logged_after(world: &World, from: usize, needle: &str) -> bool {
    world.log[from.min(world.log.len())..]
        .iter()
        .any(|line| line.contains(needle))
}

/// Runs `body` against every committed fixture.
fn for_each_fixture(body: impl Fn(&fixture::Fixture)) {
    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        body(fixture);
    }
}

/// Started before the game: the splitter waits, says so, and touches nothing.
///
/// The "touches nothing" half is the one worth having. A splitter that drove
/// the timer while no game was running would be actively harmful — it is
/// running in LiveSplit for the whole session, including during someone else's
/// run.
#[test]
fn waits_for_a_game_that_is_not_running_yet() {
    for_each_fixture(|fixture| {
        let world = test_harness::drive(World::new(), timberborn_autosplitter::main(), 60);
        assert!(
            world.logged("Still looking for Timberborn..."),
            "{}: the splitter did not report that it was waiting; log was {:#?}",
            fixture.game_version,
            world.log
        );
        assert!(
            controlling(&world).is_empty(),
            "{}: the timer was driven with no game running: {:?}",
            fixture.game_version,
            controlling(&world)
        );
    });
}

/// The game starts after the splitter does, and is picked up.
///
/// Not the same test as the one above with an extra assertion: what this
/// covers is the *transition*. The splitter's search remembers pids it has
/// ruled out, and a process appearing into that state is how it goes wrong.
#[test]
fn attaches_to_a_game_that_starts_later() {
    for_each_fixture(|fixture| {
        const APPEARS: usize = 30;
        let mut process = Some(game_in_progress(fixture, 900));

        let world = test_harness::drive_with(
            World::new(),
            timberborn_autosplitter::main(),
            APPEARS + SETTLE,
            move |tick, world| {
                if tick == APPEARS {
                    world.add_process(process.take().expect("added once"));
                }
                true
            },
        );

        assert!(
            world.logged("Attached to the Mono runtime."),
            "{}: the splitter never picked the game up; log was {:#?}",
            fixture.game_version,
            world.log
        );
        assert!(
            world.logged("The game's singleton container is at"),
            "{}: attached, but never got as far as the container; log was {:#?}",
            fixture.game_version,
            world.log
        );
    });
}

/// The game closes under the splitter, which notices and goes back to looking.
///
/// Both halves matter. Not noticing leaves it reading a dead process's memory,
/// which is where "the previous game's container" bugs come from; not going
/// back to looking means the next game of the session is never picked up.
#[test]
fn notices_the_game_closing_and_looks_again() {
    for_each_fixture(|fixture| {
        const CLOSES: usize = SETTLE;
        let mut closed_at = None;

        let world = test_harness::drive_with(
            World::new().with_process(game_in_progress(fixture, 900)),
            timberborn_autosplitter::main(),
            CLOSES + SEARCH_NOTICE_TICKS * 4,
            |tick, world| {
                if tick == CLOSES {
                    closed_at = Some(world.log.len());
                    world.close_process(900);
                }
                true
            },
        );

        let closed_at = closed_at.expect("the process was closed");
        assert!(
            world.logged("The game's singleton container is at"),
            "{}: the game was never picked up in the first place; log was {:#?}",
            fixture.game_version,
            world.log
        );
        assert!(
            logged_after(&world, closed_at, "Still looking for Timberborn..."),
            "{}: the splitter did not go back to looking after the game closed; \
             log from the close was {:#?}",
            fixture.game_version,
            &world.log[closed_at.min(world.log.len())..]
        );
    });
}

/// A second game in the same session, which is the cold two-cycle case.
///
/// The first game is already in progress, so nothing is timed; the second is a
/// fresh load, so it is. Getting this wrong is not academic — an earlier
/// version bound the *previous* game's services after a scene change, which
/// read as a game with the wonder already unlocked.
#[test]
fn times_the_second_game_of_a_session() {
    for_each_fixture(|fixture| {
        const CLOSES: usize = SETTLE;
        const STARTS: usize = CLOSES + 20;
        const LOAD_ENDS: usize = STARTS + SETTLE;
        const OVERLAY: usize = LOAD_ENDS + 60;

        let first = game_in_progress(fixture, 900);
        let (second, loading) = game_loading(fixture, 901);
        let mut second = Some(second);

        let world = test_harness::drive_with(
            World::new().with_process(first),
            timberborn_autosplitter::main(),
            OVERLAY + 120,
            move |tick, world| {
                match tick {
                    CLOSES => world.close_process(900),
                    STARTS => world.add_process(second.take().expect("added once")),
                    LOAD_ENDS => loading.load_ends(),
                    OVERLAY => loading.overlay(),
                    _ => {}
                }
                true
            },
        );

        assert_eq!(
            controlling(&world)
                .iter()
                .filter(|event| matches!(event, TimerEvent::Start))
                .count(),
            1,
            "{}: the second game of the session should start the timer exactly \
             once; the timer saw {:?} and the log was {:#?}",
            fixture.game_version,
            controlling(&world),
            world.log
        );
        assert!(
            world.logged("run start bound during the load"),
            "{}: the second game's run start was not bound during its load; \
             log was {:#?}",
            fixture.game_version,
            world.log
        );
    });
}

/// A process on its way out stays attachable for several seconds under Wine,
/// with nothing readable in it. The splitter must not be wedged by one.
///
/// It will attach — there is nothing to tell it not to — and then find no Mono
/// module and wait. What matters is that it drives no timer while it waits,
/// and that it is still able to pick up the real game afterwards.
#[test]
fn a_dying_process_neither_splits_nor_wedges() {
    for_each_fixture(|fixture| {
        const DIES: usize = 40;
        const GONE: usize = DIES + 30;
        const REAL_GAME: usize = GONE + 10;

        let mut real = Some(game_in_progress(fixture, 901));
        let mut dead_at = None;

        let world = test_harness::drive_with(
            World::new().with_process(game_in_progress(fixture, 900)),
            timberborn_autosplitter::main(),
            REAL_GAME + SETTLE,
            |tick, world| {
                match tick {
                    // Still open, still named the game, nothing readable: the
                    // window Wine leaves open after the process is gone.
                    DIES => {
                        let process = world.process_by_pid(900).expect("the dying process");
                        process.memory = Box::new(EmptyMemory);
                        dead_at = Some(world.log.len());
                    }
                    GONE => world.close_process(900),
                    REAL_GAME => world.add_process(real.take().expect("added once")),
                    _ => {}
                }
                true
            },
        );

        let dead_at = dead_at.expect("the process died");
        assert!(
            controlling(&world).is_empty(),
            "{}: the timer was driven off a dying process: {:?}",
            fixture.game_version,
            controlling(&world)
        );
        assert!(
            logged_after(&world, dead_at, "The game's singleton container is at"),
            "{}: the splitter never recovered to find the real game; log from \
             the death was {:#?}",
            fixture.game_version,
            &world.log[dead_at.min(world.log.len())..]
        );
    });
}
