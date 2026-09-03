use test_harness::{scenario::Scenario, World};

#[test]
fn explore() {
    let scenario = Scenario::find("wonder-run").unwrap_or_else(|e| panic!("{e}"));
    println!("steps: {:?}", scenario.events());
    let expected = scenario.events().len();
    let (process, playhead) = scenario.into_process();

    let mut budget = 0usize;
    let mut seen = 0usize;
    let world = test_harness::drive_with(
        World::new().with_process(process),
        timberborn_autosplitter::main(),
        200_000,
        |_, world| {
            let now = world.timer.run_control().count();
            budget += 1;
            if now > seen || budget > 4000 {
                if now > seen {
                    println!(
                        "  step {} -> event #{now} after {budget} ticks",
                        playhead.position()
                    );
                    seen = now;
                }
                budget = 0;
                return playhead.advance();
            }
            true
        },
    );
    for line in &world.log {
        println!("  | {line}");
    }
    println!("timer: {:?}", world.timer.run_control().collect::<Vec<_>>());
    println!("splits: {} (recorded {})", world.timer.splits(), expected - 2);
}
