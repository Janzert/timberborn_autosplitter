//! The splitter driven against a capture of a *finished* run.
//!
//! Characterization, not specification: these say what the splitter did against
//! build 25096761 as it actually sat in memory. A game update can turn one red
//! with nothing wrong in the code, which is why they are behind
//! `--features snapshot-tests` — `cargo snapshot-tests`.
//!
//! The capture is one instant of a save where everything has already happened:
//! every tracked building finished, the wonder unlocked and launched, the
//! Congratulations screen already shown. So what it exercises is not a run but
//! the **attach-mid-run** path — the splitter reconstructing what it missed and
//! declining to start a timer for a run that is already over.

use std::sync::OnceLock;

use test_harness::{snapshot::Snapshot, timer::TimerEvent, World};

/// The state these need, by description rather than by filename.
const RUN_FINISHED: &str = "run-finished";

/// Ticks to run. The three full heap sweeps are budget-limited to 32 MiB each,
/// and the capture holds 4678 MiB of scannable range, so this is a few hundred
/// ticks of scanning before anything interesting is reachable.
const TICKS: usize = 2000;

/// Driven once and shared: a pass costs ~25s, nearly all of it sweeping the
/// heap, and paying that per test would make the suite unusable.
struct Outcome {
    version: String,
    log: Vec<String>,
    timer: Vec<TimerEvent>,
}

/// One pass per captured game version, driven once and shared.
///
/// A pass costs ~25s, nearly all of it sweeping the heap, so paying it per test
/// would make the suite unusable. Running every version is the point of having
/// captured more than one: the splitter's whole claim is that it resolves names
/// at runtime and so survives a game update, and this is where that stops being
/// an assertion. The two builds here are further apart than a patch -- Unity
/// 6000.3 against 6000.5, with a different Mono.
fn finished_runs() -> &'static [Outcome] {
    static OUTCOMES: OnceLock<Vec<Outcome>> = OnceLock::new();
    OUTCOMES.get_or_init(|| {
        let dirs = test_harness::snapshot::find_per_version(RUN_FINISHED)
            .unwrap_or_else(|e| panic!("{e}"));
        dirs.iter()
            .map(|dir| {
                let snapshot = Snapshot::open(dir).expect("opening the snapshot");
                let version = snapshot.metadata.game_version.clone();
                let world = World::new().with_process(snapshot.process());
                let world = test_harness::drive(world, timberborn_autosplitter::main(), TICKS);
                Outcome {
                    version,
                    log: world.log,
                    timer: world.timer.events,
                }
            })
            .collect()
    })
}

/// Asserts against every captured version, naming the one that failed.
fn require(needle: &str) {
    for run in finished_runs() {
        assert!(
            run.log.iter().any(|line| line.contains(needle)),
            "{}: expected a log line containing {needle:?}; log was {:#?}",
            run.version,
            run.log
        );
    }
}

fn refuse(needle: &str, why: &str) {
    for run in finished_runs() {
        assert!(
            !run.log.iter().any(|line| line.contains(needle)),
            "{}: {why}; log was {:#?}",
            run.version,
            run.log
        );
    }
}

/// The runtime half of the version check, which until now had only ever been
/// run by hand with the game open. `metadata.py check` answers the same
/// question offline; this answers it against real memory, including the vtables
/// that only exist once a class has been constructed.
#[test]
fn every_name_the_design_depends_on_resolves() {
    require("probe: ALL RESOLVED");
    refuse("MISSING", "a name the design depends on did not resolve");
}

/// The whole path: scan, container, services. Reaching the probe at all means
/// each of these worked.
#[test]
fn finds_the_games_singleton_container_and_services() {
    require("The game's singleton container is at");
    require("Found DayNightCycle at");
    require("Watching wonder completion at");
    require("Watching building unlocks at");
}

/// Attaching to a run already in progress must not start a timer. The runner
/// would see a run that silently began at the wrong time, which is the exact
/// failure this splitter exists to remove.
#[test]
fn refuses_to_start_a_timer_for_a_run_already_over() {
    require("Not starting the timer");
    require("Game already in progress");

    for run in finished_runs() {
        let controlling: Vec<_> = run
            .timer
            .iter()
            .filter(|e| !matches!(e, TimerEvent::SetVariable { .. }))
            .collect();
        assert!(
            controlling.is_empty(),
            "{}: nothing should reach the timer; got {controlling:#?}",
            run.version
        );
    }
}

/// Refusing is only half of it: the runner has to be *told*, in the status
/// variable rather than only in a log nobody reads.
#[test]
fn tells_the_runner_why_through_the_status_variable() {
    for run in finished_runs() {
        let said = run.timer.iter().any(|e| {
            matches!(e, TimerEvent::SetVariable { key, value }
                if key == "Timberborn Autosplitter" && value.contains("already in progress"))
        });
        assert!(said, "{}: timer events were {:#?}", run.version, run.timer);
    }
}

/// State from before the splitter attached has to be reconstructed, or the
/// first poll would read every already-true condition as having just happened
/// and fire every split at once.
#[test]
fn reconstructs_what_it_missed() {
    require("Wonder already unlocked in this save: true");
    require("Already finished in this save: true");

    // Against the one line that lists them, not against the log as a whole:
    // "Forester" appears in several lines, so a bare substring search over
    // everything would pass without the reconstruction having happened.
    for run in finished_runs() {
        let listed = run
            .log
            .iter()
            .find(|line| line.contains("Already finished in this save: Forester"))
            .unwrap_or_else(|| panic!("{}: log was {:#?}", run.version, run.log));
        for building in [
            "Forester",
            "Gear Workshop",
            "Tapper's Shack",
            "Observatory / Numbercruncher",
            "Smelter",
            "Wood Workshop",
        ] {
            assert!(
                listed.contains(building),
                "{}: {building:?} missing from {listed:?}",
                run.version
            );
        }
    }
}

/// Buildings are found through the global entity registry rather than the
/// districts' finished-building registries, which do not list every building.
/// That bug was silent and cost real debugging; a walk that finds nothing would
/// look like a game with nothing built.
#[test]
fn walks_the_global_entity_registry() {
    require("[entities] opening walk starting over");
    require("entities for 6 tracked buildings");
}

/// The reference table is what makes every search after the first cheap, so a
/// run that never finds one has quietly fallen back to sweeping for everything.
///
/// Asserting that it was found and then used, not which address it was at:
/// the address differs per build -- 0x1f0000000 on one, 0x230000000 on another
/// -- which is the whole reason it is discovered rather than tabulated.
#[test]
fn finds_the_reference_table_and_searches_it() {
    require("[table] found at");
    require("[table] SingletonRepository:");
}

/// The container is found through the table, not by sweeping for it.
///
/// The sweep is still there as a fallback and a run may legitimately use it,
/// but a *loaded game* whose container came from a sweep means the table was
/// found and then did not contain the one object it most needs to.
#[test]
fn the_container_does_not_cost_a_sweep() {
    for run in finished_runs() {
        let swept = run
            .log
            .iter()
            .any(|line| line.contains("[scan] SingletonRepository starting"));
        assert!(
            !swept,
            "{}: the container was swept for despite a table being found; log was {:#?}",
            run.version, run.log
        );
    }
}

/// Reads either succeed or are reported. A capture with holes in it would show
/// up here rather than as a mysterious failure to find an object.
#[test]
fn the_capture_has_no_holes() {
    // Every scan reports its own read failures, so an incomplete capture shows
    // up here rather than as a mysterious failure to find an object.
    for run in finished_runs() {
        let sweeps: Vec<&String> = run
            .log
            .iter()
            .filter(|line| line.contains("chunk read failures"))
            .collect();
        assert!(
            !sweeps.is_empty(),
            "{}: no sweep reported; log was {:#?}",
            run.version,
            run.log
        );
        for sweep in sweeps {
            assert!(
                sweep.contains("0 chunk read failures, 0 KiB unreadable"),
                "{}: the capture has holes in it: {sweep:?}",
                run.version
            );
        }
    }
}

/// The BCL collection readers resolve `ImmutableArray`, `Dictionary` and
/// `HashSet` field names rather than falling back to a hardcoded layout. The
/// fallback is insurance, and a silent one: it was only made to log at all
/// after a session could not tell whether it was load-bearing.
#[test]
fn reads_collections_by_name_rather_than_by_the_known_layout() {
    refuse(
        "[collections]",
        "the fallback was taken, so a field name did not resolve",
    );
}

/// Guards the guard: covering one version when two are captured would look
/// identical from outside and quietly halve the evidence.
#[test]
fn reports_which_versions_are_being_used() {
    let versions: Vec<&str> = finished_runs().iter().map(|r| r.version.as_str()).collect();
    println!("run-finished versions in use: {versions:?}");
    assert!(!versions.is_empty());
}
