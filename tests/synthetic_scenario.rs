//! A whole wonder run, played against a world built from the fixture.
//!
//! `tests/scenario_run.rs` does this by replaying two recorded games, which
//! costs 55s and captures this repository cannot ship. This plays the same
//! category — the timer starting, then all seven splits — against a world
//! assembled from committed facts, in milliseconds, on any machine.
//!
//! Both suites stay. This one can only ever be as right as the fixture, and
//! the recording is what says whether the fixture is right; see
//! `tests/fixture_vs_snapshot.rs` and TEST_HARNESS_PLAN.md.
//!
//! # The world changes, because a frozen one lies
//!
//! Two of the bugs that cost recorded runs were things that vary in a live
//! game and had been frozen by a test. So nothing here is still: the load
//! starts and ends, the initializer crosses the overlay, buildings are placed
//! and then finish, the unlock set grows. What *cannot* change is the set of
//! objects — a process's memory map is fixed once it exists, exactly as the
//! game's is while a run is played — so everything the run will need is placed
//! up front and then revealed, an entity list whose `_size` grows being how a
//! building gets built.

use std::{cell::RefCell, rc::Rc};

use test_harness::{
    fixture::{
        self,
        game::{reached_by, Entity, Live, Object, Scene},
    },
    timer::TimerEvent,
    World,
};

/// `InitializationState`, as the game numbers it. Duplicated from the splitter
/// rather than imported: a test that took the splitter's own constants could
/// not notice the game renumbering the enum.
const WAITING: i32 = 0;
const FINISHED: i32 = 5;

/// `BlockObjectState._state` for a building that is finished rather than a
/// construction site.
const BUILT: i32 = 1;

/// The Folktails wonder, by template name. Not the similar-sounding Tribute to
/// Ingenuity, which is a monument — a confusion that has already cost this
/// project a debugging session, and one a synthetic world would happily
/// reproduce if the name here were guessed rather than taken from the
/// splitter's own list.
const WONDER: &str = "EarthRecultivator.Folktails";

/// The six buildings that drive the five building splits, in the order the
/// category builds them. The last two share one split, which fires when both
/// are up.
const BUILDINGS: &[&str] = &[
    "Forester.Folktails",
    "GearWorkshop.Folktails",
    "TappersShack.Folktails",
    "Observatory.Folktails",
    "Smelter.Folktails",
    "WoodWorkshop.Folktails",
];

/// Ticks between one thing happening and the next.
///
/// Generous: the splitter spreads work over ticks on purpose — the container
/// is read a chunk at a time, entities are walked in chunks, scans yield — so
/// a step that fires the moment the last one landed would be testing a
/// schedule the real game never offers.
const SETTLE: usize = 60;

/// Everything a scenario reaches back into once the world is running.
struct Run {
    live: Live,
    loader: Object,
    initializer: Object,
    countdown: Object,
    clock: Object,
    entities: u64,
    unlocked: u64,
    /// Every entity, placed but not yet revealed.
    placed: Vec<Entity>,
}

/// Builds the world a wonder run happens in, with nothing yet done in it.
fn wonder_run(fixture: &fixture::Fixture) -> (World, Run) {
    let mut scene = Scene::new(fixture);

    let clock = scene.service("Timberborn.TimeSystem", "DayNightCycle");
    scene.set_i32(&clock, "DayNumber", 1);
    // The day lengths the countdown diagnostic divides by. Plausible rather
    // than measured: nothing splits on them, and a zero here would print a
    // completion day of 5.6e-47 instead of a number.
    scene.set_f32(&clock, "DayLengthInSeconds", 900.0);
    scene.set_f32(&clock, "DaytimeLengthInHours", 16.0);
    scene.set_f32(&clock, "NighttimeLengthInHours", 8.0);

    let unlocking = scene.service("Timberborn.ScienceSystem", "BuildingUnlockingService");
    // Every name the set will ever hold is placed now; `_count` and
    // `_lastIndex` start at zero, so as far as anything reading it is
    // concerned the set is empty until the run unlocks the wonder.
    let unlocked = scene.hash_set(reached_by::UNLOCKED_SET, &[WONDER]);
    scene.set_ptr(&unlocking, "_unlockedBuildings", unlocked);
    let unlocked_object = unlocked;

    let countdown = scene.service(
        "Timberborn.GameWonderCompletion",
        "WonderCompletionCountdownStarter",
    );

    // The entity registry, reached the way the splitter reaches it: through
    // the one singleton that holds it.
    let placed: Vec<Entity> = BUILDINGS
        .iter()
        .map(|template| scene.entity(template, 0))
        .collect();
    let addresses: Vec<u64> = placed
        .iter()
        .map(|entity| entity.component.address)
        .collect();
    let entities = scene.list(reached_by::ENTITY_LIST, &addresses);

    let registry = scene.object("Timberborn.EntitySystem", "EntityRegistry");
    scene.set_ptr(&registry, "_entitiesInInstantiationOrder", entities);
    let game_over = scene.object("Timberborn.GameOver", "GameOverChecker");
    scene.set_ptr(&game_over, "_entityRegistry", registry.address);
    scene.register(&game_over);

    let population = scene.object("Timberborn.Population", "PopulationService");
    scene.register(&population);
    let data = scene.object("Timberborn.Population", "PopulationData");
    scene.set_ptr(&population, "GlobalPopulationData", data.address);

    let initializer = scene.service("Timberborn.GameStartup", "GameInitializer");
    scene.set_i32(&initializer, "_initializationState", WAITING);

    // A new game, and the load still running: the state the splitter has to
    // bind the run start in, and the only window it gets.
    let loader = scene.scene_loader(true, true);
    scene.register(&loader);

    let (process, live) = scene.finish_live();
    // Nothing has happened yet. Both collections were built full, because
    // memory cannot be added to once the process exists -- so the run begins
    // by emptying them, and every step after this reveals what is already
    // there. No entities placed, and nothing unlocked.
    live.set_instance_i32(reached_by::ENTITY_LIST, entities, "_size", 0);
    live.set_instance_i32(reached_by::UNLOCKED_SET, unlocked_object, "_count", 0);
    live.set_instance_i32(reached_by::UNLOCKED_SET, unlocked_object, "_lastIndex", 0);

    (
        World::new().with_process(process),
        Run {
            live,
            loader,
            initializer,
            countdown,
            clock,
            entities,
            unlocked: unlocked_object,
            placed,
        },
    )
}

impl Run {
    /// The load finishes. The game is not up yet: the settlement-name dialog
    /// is still on screen and the initializer has not reached the overlay.
    ///
    /// A separate step from [`overlay`](Self::overlay) because the gap between
    /// them is what the run start *is*. Collapsing the two — ending the load
    /// and crossing the overlay in one instant — makes the splitter report a
    /// start it could not have timed, which is exactly what it does when it
    /// binds too late in a real game.
    fn load_ends(&self) {
        self.live.set_u8(&self.loader, "_isLoading", 0);
    }

    /// The overlay appears. This is the run start.
    fn overlay(&self) {
        self.live
            .set_i32(&self.initializer, "_initializationState", FINISHED);
    }

    /// Places a building — it exists as an entity from the moment it is put
    /// down, unfinished — and then finishes it.
    fn place(&self, index: usize) {
        self.live.set_instance_i32(
            reached_by::ENTITY_LIST,
            self.entities,
            "_size",
            index as i32 + 1,
        );
    }

    fn finish(&self, index: usize) {
        self.live
            .set_i32(&self.placed[index].block_state, "_state", BUILT);
    }

    /// Science reaches the wonder: the set of unlocked buildings gains it.
    fn unlock_wonder(&self) {
        self.live
            .set_instance_i32(reached_by::UNLOCKED_SET, self.unlocked, "_count", 1);
        self.live
            .set_instance_i32(reached_by::UNLOCKED_SET, self.unlocked, "_lastIndex", 1);
    }

    /// The wonder is activated, which starts the countdown. **Not** the end of
    /// the run: the Congratulations screen is about half an in-game hour
    /// later, and splitting here would cost every runner that half hour.
    fn activate_wonder(&self) {
        self.live.set_i32(&self.clock, "DayNumber", 40);
        self.live.set_i32(&self.countdown, "_unlockDay", 40);
    }

    /// The countdown runs out and the Congratulations screen appears, which is
    /// where the category ends.
    fn congratulations(&self) {
        self.live.set_i32(&self.countdown, "CountdownFinished", 1);
    }
}

/// How many splits had fired by the time each step was applied.
type SplitsAtStep = Rc<RefCell<Vec<usize>>>;

/// One thing happening in the run, applied between ticks.
type Step = Box<dyn Fn(&Run)>;

/// Drives a run, doing each step in turn with ticks in between, and gives back
/// the world.
fn play(fixture: &fixture::Fixture) -> (World, SplitsAtStep) {
    let (world, run) = wonder_run(fixture);

    // What happens, in order. The splitter is given room between each.
    let steps: Vec<Step> = vec![
        Box::new(|r: &Run| r.load_ends()),
        Box::new(|r: &Run| r.overlay()),
        Box::new(|r: &Run| {
            r.place(0);
            r.finish(0);
        }),
        Box::new(|r: &Run| {
            r.place(1);
            r.finish(1);
        }),
        Box::new(|r: &Run| {
            r.place(2);
            r.finish(2);
        }),
        Box::new(|r: &Run| {
            r.place(3);
            r.finish(3);
        }),
        Box::new(|r: &Run| {
            r.place(4);
            r.finish(4);
        }),
        Box::new(|r: &Run| {
            r.place(5);
            r.finish(5);
        }),
        Box::new(|r: &Run| r.unlock_wonder()),
        Box::new(|r: &Run| r.activate_wonder()),
        Box::new(|r: &Run| r.congratulations()),
    ];

    // How long the load runs before the game comes up. Modelled on a real
    // one rather than on how long the splitter happens to need: Timberborn
    // takes seconds to load a map, which at the splitter's ~100 ticks a second
    // is many hundreds of ticks. Binding the run start inside that window is
    // the claim being tested, so the window has to be the game's, not one
    // sized to whatever passes.
    const LEAD_IN: usize = 800;
    let ticks = LEAD_IN + steps.len() * SETTLE + SETTLE;
    let mut next = LEAD_IN;
    let mut step = 0;
    // Sampled just before each step, so a test can ask what had happened by
    // the time the world changed -- which is the only way to say "activating
    // the wonder did not split" about a run that does split, later.
    let splits: SplitsAtStep = Default::default();
    let recorded = splits.clone();

    let world = test_harness::drive_with(
        world,
        timberborn_autosplitter::main(),
        ticks,
        move |tick, world| {
            if tick >= next && step < steps.len() {
                recorded.borrow_mut().push(world.timer.splits());
                steps[step](&run);
                step += 1;
                next = tick + SETTLE;
            }
            true
        },
    );
    (world, splits)
}

/// Every committed fixture, played through.
fn for_each_fixture(check: impl Fn(&str, &World)) {
    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let (world, _) = play(fixture);
        check(&fixture.game_version, &world);
    }
}

/// Everything the timer was told, in order, ignoring the variables the
/// splitter sets alongside.
fn controlling(world: &World) -> Vec<&TimerEvent> {
    world
        .timer
        .events
        .iter()
        .filter(|event| !matches!(event, TimerEvent::SetVariable { .. }))
        .collect()
}

/// The category: a start and seven splits, in that order and no others.
///
/// Asserting the *sequence* rather than a count is the point. Seven splits
/// firing at once would satisfy a count perfectly while being the worst
/// possible behaviour.
#[test]
fn the_whole_category_fires() {
    for_each_fixture(|version, world| {
        let events = controlling(world);
        let expected = std::iter::once("Start")
            .chain(std::iter::repeat_n("Split", 7))
            .collect::<Vec<_>>();
        let actual: Vec<&str> = events
            .iter()
            .map(|event| match event {
                TimerEvent::Start => "Start",
                TimerEvent::Split => "Split",
                TimerEvent::Reset => "Reset",
                other => {
                    // Named rather than lumped together: a suite that reported
                    // "other" would say a run went wrong without saying how.
                    Box::leak(format!("{other:?}").into_boxed_str())
                }
            })
            .collect();
        assert_eq!(
            actual, expected,
            "{version}: the timer was driven wrongly; log was {:#?}",
            world.log
        );
    });
}

/// The run start is bound while the scene is still loading.
///
/// Not a nicety: the gap between a load finishing and the overlay appearing is
/// about one heap scan wide, and binding after it was measured as always too
/// late. A splitter that only ever bound afterwards would still pass a test
/// that merely counted splits.
#[test]
fn the_run_start_is_bound_during_the_load() {
    for_each_fixture(|version, world| {
        assert!(
            world.logged("run start bound during the load"),
            "{version}: the run start was not bound during the load; log was {:#?}",
            world.log
        );
    });
}

/// Each split fires for the building it is for, and in the category's order.
///
/// The count and the timing can both be right with the wrong buildings behind
/// them. Asserted against the same strings `tests/scenario_run.rs` asserts
/// against a recording, so the two suites disagreeing is visible.
#[test]
fn the_splits_are_for_the_right_things_in_order() {
    for_each_fixture(|version, world| {
        let reasons: Vec<&str> = world
            .log
            .iter()
            .filter_map(|line| {
                line.strip_prefix("Split: ")
                    .or_else(|| line.strip_prefix("Run end: "))
            })
            .collect();
        assert_eq!(
            reasons,
            [
                "Forester finished.",
                "Gear Workshop finished.",
                "Tapper's Shack finished.",
                "Observatory / Numbercruncher finished.",
                "Smelter + Wood Workshop finished.",
                "wonder unlocked.",
                "Congratulations screen. Splitting.",
            ],
            "{version}: log was {:#?}",
            world.log
        );
    });
}

/// Activating the wonder is not the end of the run.
///
/// The Congratulations screen is about half an in-game hour later, and
/// splitting at activation is the easy mistake — it would cost every runner
/// that half hour, and a suite that only counted splits would never see it.
///
/// Asked as "what had fired by the time the countdown ran out": six splits,
/// the five buildings and the unlock, and the seventh only afterwards.
#[test]
fn activating_the_wonder_does_not_split() {
    /// Where `congratulations()` sits in the step list.
    const COUNTDOWN_FINISHES: usize = 10;

    let fixtures = fixture::load_all().unwrap_or_else(|e| panic!("{e}"));
    for fixture in &fixtures {
        let (world, splits) = play(fixture);
        let splits = splits.borrow();
        assert_eq!(
            splits.get(COUNTDOWN_FINISHES).copied(),
            Some(6),
            "{}: the run should stand at six splits when the countdown runs \
             out -- five buildings and the unlock -- with the run end still to \
             come. Steps were {splits:?}; log was {:#?}",
            fixture.game_version,
            world.log
        );
        assert_eq!(
            world.timer.splits(),
            7,
            "{}: the run end did not split",
            fixture.game_version
        );
    }
}
