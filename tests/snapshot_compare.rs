//! Evidence that an unfrozen capture is good enough.
//!
//! Capture walks a process that is still running, so without `--freeze` the
//! last range is seconds younger than the first and the result can in principle
//! hold a combination of values the game never had. This asks whether that
//! matters in practice: two captures of the same game state, one frozen and one
//! not, driven through the splitter, should reach the same conclusions.
//!
//! **Needs a pair of captures** of one unchanged state, taken back to back --
//! one with `--freeze` and one without:
//!
//! ```text
//! tb-dump --state run-finished --notes '...'
//! tb-dump --state run-finished --freeze --notes '...'
//! ```
//!
//! One caveat on how far the result generalises: the state this was first run
//! against is idle -- a finished run, nothing being built, no simulation
//! pressure. A capture taken during active play has far more opportunity to
//! tear, and this says nothing about that case.

use test_harness::{snapshot::Snapshot, World};

const RUN_FINISHED: &str = "run-finished";

const TICKS: usize = 2000;

fn log_of(frozen: bool) -> Vec<String> {
    let dir =
        test_harness::snapshot::find(RUN_FINISHED, Some(frozen)).unwrap_or_else(|e| panic!("{e}"));
    let snapshot = Snapshot::open(&dir).expect("opening the snapshot");
    let world = World::new().with_process(snapshot.process());
    test_harness::drive(world, timberborn_autosplitter::main(), TICKS).log
}

/// Blanks anything numeric. Addresses, byte counts and slice counts differ
/// between any two captures and are not the point; what the splitter *decided*
/// is. A word counts as numeric only if it holds a digit, so ordinary words
/// that happen to be spelled from a-f are left alone.
fn normalise(log: &[String]) -> Vec<String> {
    log.iter()
        .map(|line| {
            line.split_whitespace()
                .map(|word| {
                    let numeric = word.chars().any(|c| c.is_ascii_digit())
                        && word
                            .chars()
                            .all(|c| c.is_ascii_hexdigit() || "+.,x".contains(c));
                    if numeric {
                        "<n>"
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[test]
fn freezing_the_game_changes_nothing_the_splitter_concludes() {
    let unfrozen = normalise(&log_of(false));
    let frozen = normalise(&log_of(true));

    let differences: Vec<String> = (0..unfrozen.len().max(frozen.len()))
        .filter(|&i| unfrozen.get(i) != frozen.get(i))
        .map(|i| {
            format!(
                "line {i}:\n  unfrozen: {:?}\n  frozen:   {:?}",
                unfrozen.get(i),
                frozen.get(i)
            )
        })
        .collect();

    assert!(
        differences.is_empty(),
        "the two captures disagree, so tearing is affecting what the splitter \
         reads:\n{}",
        differences.join("\n")
    );
    assert!(
        unfrozen.len() > 40,
        "only {} log lines, so this compared almost nothing",
        unfrozen.len()
    );
}
