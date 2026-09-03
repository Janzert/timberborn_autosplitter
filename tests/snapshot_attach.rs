//! The splitter driven against a capture of the real game.
//!
//! These are **characterization** tests: they say what the splitter does
//! against one build of Timberborn as it actually was in memory, not what it
//! ought to do in principle. A game update can turn one red with nothing wrong
//! in the code, which is why they are behind `--features snapshot-tests` and
//! not part of the suite that gates a commit.
//!
//! Their real job is to be the oracle a synthesized fixture is checked against.
//! See TEST_HARNESS_PLAN.md in the parent repository.
//!
//! ```text
//! cargo snapshot-tests
//! ```

use std::path::PathBuf;

use test_harness::{snapshot::Snapshot, World};

/// The state these need. Asked for by what it is, not by which file holds it --
/// a missing one fails with the steps for producing it.
const MAIN_MENU: &str = "main-menu";

/// Runs `check` against **every** capture of the state, and says which one
/// failed.
///
/// Not just the first. Two captured game versions are only worth their disk if
/// the same assertions run against both; it is also what stops a test pinning
/// itself to one build's behaviour, which is exactly what happened here --
/// `attaches_to_a_captured_process` asserted the log line from the
/// ambiguous-name route, and 1.0.13.1 reports its name plainly and takes the
/// other one.
fn for_each_capture(state: &str, ticks: usize, check: impl Fn(&World, &PathBuf)) {
    let dirs = test_harness::snapshot::find_all(state).unwrap_or_else(|e| panic!("{e}"));
    for dir in dirs {
        let snapshot = Snapshot::open(&dir).expect("opening the snapshot");
        let world = World::new().with_process(snapshot.process());
        let world = test_harness::drive(world, timberborn_autosplitter::main(), ticks);
        check(&world, &dir);
    }
}

/// The whole chain, end to end: a real capture, through the fake runtime, into
/// the splitter's own attach path.
#[test]
fn attaches_to_a_captured_process() {
    for_each_capture(MAIN_MENU, 50, |world, dir| {
        // Deliberately not asserting *which* route attached. A build whose comm
        // is "Timberborn.exe" matches outright; one reporting "Unity Main Thre"
        // -- Unity 6.5 names its main thread, and /proc caps comm at 15
        // characters -- has to be confirmed by its module first. Both are
        // correct, and which one fires is a property of the game version.
        assert!(
            world.logged("Attached to"),
            "{}: the splitter should recognise the capture as Timberborn; log was {:#?}",
            dir.display(),
            world.log
        );
        assert!(
            !world.logged("Still looking for Timberborn..."),
            "{}: log was {:#?}",
            dir.display(),
            world.log
        );
    });
}

/// The step that would break first if the capture were incomplete: `Module`
/// resolves Mono by reading the PE header, walking exports for
/// `mono_assembly_foreach`, and following a RIP-relative displacement out of
/// its code. All of that lives in mappings Wine leaves anonymous, so it only
/// works if the capture took the unnamed ranges as well as the named ones.
#[test]
fn resolves_the_mono_runtime() {
    for_each_capture(MAIN_MENU, 400, |world, dir| {
        assert!(
            world.logged("Attached to the Mono runtime."),
            "{}: log was {:#?}",
            dir.display(),
            world.log
        );
    });
}

/// A capture is one instant, so the splitter can only ever be part-way through
/// its startup. Nothing should reach the timer from a still frame -- and
/// nothing should crash trying.
#[test]
fn a_still_frame_starts_no_run() {
    for_each_capture(MAIN_MENU, 400, |world, dir| {
        assert_eq!(
            world.timer.run_control().count(),
            0,
            "{}: events were {:#?}",
            dir.display(),
            world.timer.events
        );
    });
}

/// Guards the guard: `for_each_capture` running against one capture when two
/// exist would look identical from the outside, and quietly halve the coverage.
#[test]
fn reports_which_captures_are_being_used() {
    let dirs = test_harness::snapshot::find_all(MAIN_MENU).unwrap_or_else(|e| panic!("{e}"));
    let versions: Vec<String> = dirs
        .iter()
        .map(|d| {
            Snapshot::open(d)
                .expect("opening the snapshot")
                .metadata
                .game_version
        })
        .collect();
    println!("main-menu captures in use: {versions:?}");
    assert!(!versions.is_empty());
}
