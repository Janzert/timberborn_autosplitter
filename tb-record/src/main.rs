//! Records a scenario: the splitter driven against the live game, with a
//! capture taken every time it does something worth reproducing.
//!
//! ```text
//! tb-record --state wonder-run --notes "folktails, developer mode"
//! ```
//!
//! A single capture is one instant. It can say *the wonder was already
//! unlocked*, but never *the split fired when it became unlocked*, because a
//! split is a change and one frame has none. That is the last thing the offline
//! suite cannot test, and this is what closes it.
//!
//! # How a moment is chosen
//!
//! The real splitter is driven against the live game through the same `Memory`
//! seam the tests use, paced at whatever tick rate it asks for. Every time it
//! touches the timer -- starts, splits, resets -- that is a moment, and the game
//! is stopped and captured before it can move on. There is no separate list of
//! things to watch for: the splitter's own decisions are the definition, so a
//! scenario cannot drift out of step with what it actually does.
//!
//! Each step is a delta against the one before, so a run costs one full capture
//! and a few hundred MiB a split rather than 5 GiB apiece.
//!
//! # Interrupting it is safe
//!
//! Each step is written and closed as it is taken, so a recording stopped
//! halfway is a shorter scenario rather than a broken one.

use std::{
    io::{self, Write},
    process::ExitCode,
    time::{Duration, Instant},
};

use test_harness::{
    capture,
    live::{self, LiveMemory},
    snapshot::{self, Metadata, Snapshot},
    timer::TimerEvent,
    World,
};

/// Give up if the splitter has done nothing for this long. Recording is meant
/// to be watched; an unattended one that silently records nothing is worse than
/// one that stops and says so.
const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);

struct Args {
    state: String,
    label: Option<String>,
    notes: String,
    pid: Option<u32>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("tb-record: {message}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut state = None;
    let mut label = None;
    let mut notes = String::new();
    let mut pid = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--state" => state = Some(argv.next().ok_or("--state needs a value")?),
            "--label" => label = Some(argv.next().ok_or("--label needs a value")?),
            "--notes" => notes = argv.next().ok_or("--notes needs a value")?,
            "--pid" => {
                pid = Some(
                    argv.next()
                        .ok_or("--pid needs a value")?
                        .parse()
                        .map_err(|_| "--pid must be a number")?,
                )
            }
            "--help" | "-h" => return Err("help".into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let state = state.ok_or_else(|| {
        format!(
            "--state is required: it says what scenario is being recorded, and \
             is how a test finds it.\nKnown states:\n{}",
            test_harness::requirement::listing()
        )
    })?;
    if test_harness::requirement::get(&state).is_none() {
        return Err(format!(
            "no such state {state:?}. Known states:\n{}",
            test_harness::requirement::listing()
        ));
    }
    Ok(Args {
        state,
        label,
        notes,
        pid,
    })
}

fn run() -> Result<(), String> {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) if message == "help" => {
            println!("{}", include_str!("help.txt"));
            println!("Known states:\n{}", test_harness::requirement::listing());
            return Ok(());
        }
        Err(message) => return Err(message),
    };

    let pid = match args.pid {
        Some(pid) => pid,
        None => find_game()?,
    };
    let game_dir = game_dir(pid).ok_or_else(|| format!("cannot locate pid {pid}'s game"))?;
    let version = game_version(&game_dir)
        .ok_or_else(|| format!("cannot read the game's version from {}", game_dir.display()))?;

    let scenario = args.label.clone().unwrap_or_else(|| args.state.clone());
    println!("Recording {scenario} against {version} (pid {pid}).");
    println!("Play the run. Every split the splitter takes is captured; Ctrl-C to stop.");
    println!("Each step is written as it is taken, so stopping early keeps what it has.\n");

    // The splitter reads the live game through the same seam the tests use, so
    // what it decides here is what it would decide replaying the result.
    let memory = LiveMemory::open(pid).map_err(|e| {
        format!(
            "cannot read /proc/{pid}/mem: {e}. tb-record needs the same ptrace \
             rights as tb-dump; see snapshots/README.md."
        )
    })?;
    let mappings = live::mappings(pid).map_err(|e| format!("cannot read maps: {e}"))?;
    let mut process =
        test_harness::memory::FakeProcess::new(pid as u64, comm(pid)).with_memory(memory);
    process.modules = live::modules(&mappings);
    process.ranges = live::ranges(&mappings);

    let mut recorder = Recorder {
        pid,
        version,
        scenario,
        state: args.state,
        notes: args.notes,
        step: 0,
        previous: None,
        seen_events: 0,
        seen_log: 0,
        last_activity: Instant::now(),
        error: None,
    };

    // The state before anything happened, so the first split has something to
    // be a change from.
    recorder.take("begin")?;

    let world = World::new().with_process(process);
    test_harness::drive_with(
        world,
        timberborn_autosplitter::main(),
        usize::MAX,
        |_, world| recorder.after_tick(world),
    );

    match recorder.error.take() {
        Some(message) => Err(message),
        None => {
            println!("\nRecorded {} step(s).", recorder.step);
            Ok(())
        }
    }
}

struct Recorder {
    pid: u32,
    version: String,
    scenario: String,
    state: String,
    notes: String,
    step: u32,
    previous: Option<std::path::PathBuf>,
    seen_events: usize,
    seen_log: usize,
    last_activity: Instant,
    error: Option<String>,
}

impl Recorder {
    /// Runs between ticks. Mirrors the splitter's log so the run can be
    /// watched, and captures whenever it touches the timer.
    fn after_tick(&mut self, world: &World) -> bool {
        for line in &world.log[self.seen_log..] {
            println!("  | {line}");
        }
        if world.log.len() > self.seen_log {
            self.seen_log = world.log.len();
            self.last_activity = Instant::now();
        }

        let new = &world.timer.events[self.seen_events..];
        let reasons: Vec<String> = new.iter().filter_map(describe).collect();
        self.seen_events = world.timer.events.len();

        for reason in reasons {
            self.last_activity = Instant::now();
            if let Err(message) = self.take(&reason) {
                self.error = Some(message);
                return false;
            }
        }

        if self.last_activity.elapsed() > IDLE_LIMIT {
            println!("\nNothing has happened for 30 minutes; stopping.");
            return false;
        }

        // Pace to whatever the splitter asked for, so the game moves between
        // ticks as it really does. A tick that already overran -- a heap sweep
        // slice, say -- sleeps for nothing rather than going backwards.
        let rate = world.tick_rate.unwrap_or(120.0).max(1.0);
        std::thread::sleep(Duration::from_secs_f64(1.0 / rate));
        true
    }

    /// Stops the game and captures it, as a delta against the step before.
    fn take(&mut self, reason: &str) -> Result<(), String> {
        let label = format!("{}-step{:02}", self.scenario, self.step);
        let dir = snapshot::default_store().join(format!("{}-{label}", self.version));
        if dir.exists() {
            return Err(format!(
                "{} already exists; move the old recording aside or pass a \
                 different --label",
                dir.display()
            ));
        }

        let base = match &self.previous {
            Some(path) => Some(Snapshot::open(path).map_err(|e| format!("reopening base: {e}"))?),
            None => None,
        };

        let mut metadata = Metadata {
            game_version: self.version.clone(),
            label: label.clone(),
            captured_at: format!(
                "{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ),
            process_name: comm(self.pid),
            pid: self.pid,
            notes: self.notes.clone(),
            frozen: true,
            satisfies: vec![self.state.clone()],
            scenario: Some(self.scenario.clone()),
            ..Default::default()
        };
        metadata.step = Some(snapshot::Step {
            index: self.step,
            event: reason.to_owned(),
        });

        print!("\n[step {:02}] {reason}: capturing... ", self.step);
        let _ = io::stdout().flush();
        let report = capture::capture(self.pid, dir.clone(), metadata, base, true, |_, _| {})
            .map_err(|e| format!("capturing step {}: {e}", self.step))?;

        println!(
            "{:.0} MiB new, {:.0} MiB inherited, frozen {:.1}s",
            report.bytes_written as f64 / (1 << 20) as f64,
            report.inherited as f64 * snapshot::CHUNK as f64 / (1 << 20) as f64,
            report.frozen_for.as_secs_f64()
        );
        self.previous = Some(dir);
        self.step += 1;
        Ok(())
    }
}

/// What made a moment worth keeping. Writes to the status variable are the
/// splitter talking to the runner, not the run changing, so they are not one.
fn describe(event: &TimerEvent) -> Option<String> {
    match event {
        TimerEvent::Start => Some("start".into()),
        TimerEvent::Split => Some("split".into()),
        TimerEvent::Reset => Some("reset".into()),
        TimerEvent::UndoSplit => Some("undo-split".into()),
        TimerEvent::SkipSplit => Some("skip-split".into()),
        TimerEvent::SetVariable { .. }
        | TimerEvent::SetGameTime { .. }
        | TimerEvent::PauseGameTime
        | TimerEvent::ResumeGameTime => None,
    }
}

fn find_game() -> Result<u32, String> {
    let mut found = Vec::new();
    for name in ["Timberborn.x86_64", "Timberborn.exe", "Unity Main Thre"] {
        for pid in live::find_by_comm(name).unwrap_or_default() {
            if game_dir(pid).is_some() && !found.contains(&pid) {
                found.push(pid);
            }
        }
    }
    match found.len() {
        0 => Err("no Timberborn process found. Start the game first, or pass --pid.".into()),
        1 => Ok(found[0]),
        _ => Err(format!(
            "several candidates ({found:?}); pass --pid to choose"
        )),
    }
}

fn game_dir(pid: u32) -> Option<std::path::PathBuf> {
    live::mappings(pid)
        .ok()?
        .iter()
        .filter_map(|m| m.path.as_deref())
        .find_map(|path| {
            let text = path.to_string_lossy();
            let (before, _) = text.split_once("/Timberborn_Data/")?;
            Some(std::path::PathBuf::from(before))
        })
}

fn game_version(dir: &std::path::Path) -> Option<String> {
    let blob = std::fs::read(dir.join("Timberborn_Data").join("globalgamemanagers")).ok()?;
    let head = &blob[..blob.len().min(200_000)];
    let strings: Vec<&[u8]> = head
        .split(|b| !(0x20..0x7f).contains(b))
        .filter(|s| s.len() >= 4)
        .collect();
    let name = strings.iter().position(|s| *s == b"Timberborn")?;
    strings.iter().skip(name + 1).take(4).find_map(|s| {
        let text = std::str::from_utf8(s).ok()?;
        let parts: Vec<&str> = text.split('-').next()?.split('.').collect();
        (parts.len() >= 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
        .then(|| text.to_owned())
    })
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim_end().to_owned())
        .unwrap_or_default()
}
