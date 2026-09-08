//! The splitter driven against a capture of a settlement that has made a bot.
//!
//! Characterization, not specification, like the rest of the snapshot suite:
//! this says what the splitter read out of one real game as it actually sat in
//! memory. It is behind `--features snapshot-tests` -- `cargo snapshot-tests`.
//!
//! What it is for is the one thing the synthetic world cannot establish. The
//! Timberbot run end is `BotPopulation.BotCreated`, and every offline test
//! reads that flag out of a heap this repository wrote: a synthetic world
//! would agree with the splitter about a field the game does not have, or
//! about a meaning the game does not give it. This capture is a settlement
//! with eleven adults, two children and exactly one bot, so the flag reading
//! true here -- and the live list holding one -- is the game saying so.
//!
//! The other half of the claim, that the flag is *false* before the first bot,
//! is covered by every other capture in the store: none of them ever made one,
//! and the splitter binds the same watcher in all of them.

use test_harness::{snapshot::Snapshot, World};

/// The state this needs, by description rather than by filename.
const FIRST_BOT: &str = "first-bot";

/// Ticks to run. The sweeps are budget-limited to 32 MiB apiece against a
/// capture of several gigabytes, so most of this is scanning before anything
/// is reachable.
const TICKS: usize = 2000;

fn worlds() -> Vec<(String, World)> {
    let dirs =
        test_harness::snapshot::find_per_version(FIRST_BOT).unwrap_or_else(|e| panic!("{e}"));
    dirs.iter()
        .map(|dir| {
            let snapshot = Snapshot::open(dir).expect("opening the snapshot");
            let version = snapshot.metadata.game_version.clone();
            let world = World::new().with_process(snapshot.process());
            (
                version,
                test_harness::drive(world, timberborn_autosplitter::main(), TICKS),
            )
        })
        .collect()
}

/// The bot population is found, and says what the game showed on screen.
///
/// One assertion rather than three, because the interesting failure is not
/// "the count was wrong" but "the class resolved and the flag did not mean
/// what we think it means". A capture of a settlement with one bot that reads
/// `false` here would say exactly that.
#[test]
fn reads_the_bot_population_the_game_had() {
    for (version, world) in worlds() {
        assert!(
            world.logged("A bot has already been created in this save: true. Bots alive now: 1."),
            "{version}: the capture is of a settlement with one bot, so the \
             splitter should have read one. Log was {:#?}",
            world.log
        );
    }
}

/// A capture is one instant, so the flag was already true when the splitter
/// arrived -- which must not read as the run ending. This is the same
/// suppression `run-finished` checks for the wonder's countdown.
#[test]
fn a_bot_already_made_is_not_a_run_ending() {
    for (version, world) in worlds() {
        assert_eq!(
            world.timer.splits(),
            0,
            "{version}: attaching to a settlement that already has a bot split \
             something. Log was {:#?}",
            world.log
        );
        assert!(
            !world.logged("the first Timberbot was created"),
            "{version}: a bot made before the splitter attached was reported as \
             one made during a run. Log was {:#?}",
            world.log
        );
    }
}
