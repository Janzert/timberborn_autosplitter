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

use test_harness::{snapshot::Snapshot, World};

/// A capture taken at the main menu, before any save is loaded.
const MAIN_MENU: &str = "main-menu";

fn world(label: &str) -> World {
    let dir = test_harness::snapshot::locate(label).expect("snapshot");
    let snapshot = Snapshot::open(&dir).expect("opening the snapshot");
    World::new().with_process(snapshot.process())
}

/// The whole chain, end to end: a real capture, through the fake runtime, into
/// the splitter's own attach path.
#[test]
fn attaches_to_a_captured_process() {
    let world = test_harness::drive(world(MAIN_MENU), timberborn_autosplitter::main(), 50);

    assert!(
        world.logged("Attached to pid"),
        "the splitter should recognise the capture as Timberborn; log was {:#?}",
        world.log
    );
    assert!(!world.logged("Still looking for Timberborn..."));
}

/// The step that would break first if the capture were incomplete: `Module`
/// resolves Mono by reading the PE header, walking exports for
/// `mono_assembly_foreach`, and following a RIP-relative displacement out of
/// its code. All of that lives in mappings Wine leaves anonymous, so it only
/// works if the capture took the unnamed ranges as well as the named ones.
#[test]
fn resolves_the_mono_runtime() {
    let world = test_harness::drive(world(MAIN_MENU), timberborn_autosplitter::main(), 400);

    assert!(
        world.logged("Attached to the Mono runtime."),
        "log was {:#?}",
        world.log
    );
}

/// A capture is one instant, so the splitter can only ever be part-way through
/// its startup. Nothing should reach the timer from a still frame -- and
/// nothing should crash trying.
#[test]
fn a_still_frame_starts_no_run() {
    let world = test_harness::drive(world(MAIN_MENU), timberborn_autosplitter::main(), 400);

    assert_eq!(
        world.timer.run_control().count(),
        0,
        "events were {:#?}",
        world.timer.events
    );
}
