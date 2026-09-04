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
//! seam the tests use, paced at whatever tick rate it asks for. A moment is any
//! tick in which it **said something that was not routine progress**, or touched
//! the timer.
//!
//! Timer events alone are not enough, which took a wasted run to learn. They are
//! the splitter's *conclusions*, and its correctness depends on the states it
//! passes through to reach them: the run start is only bound while the scene is
//! still loading, and the timer only starts when `initializationState` reaches
//! `ShowUI` having been watched from below. A recording that jumped from the
//! main menu straight to "the overlay is up" showed the splitter a game already
//! in progress, and it correctly refused to start a timer -- reproducing, from a
//! recording of a perfectly good run, a bug that never happened.
//!
//! So the trigger is the splitter's log minus its own noise. That is deliberately
//! a filter rather than a list of interesting phrases: a list would have to be
//! kept in step with messages that change, and the failure mode of falling
//! behind is a recording that looks complete and is not.
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

/// Log lines that are progress rather than events. Everything else is a moment.
///
/// The scans report every slice, the entity walk reports its progress, and the
/// probe dumps a line per class -- hundreds of lines that say only "still
/// working". Capturing on those would fill a disk without adding a state.
const NOISE: &[&str] = &["[scan]", "[entities]", "[collections]", "--- probe"];

/// Refuse to record more than this many steps. A future splitter that logs more
/// freely should stop a recording, not quietly fill the disk.
const MAX_STEPS: u32 = 80;

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
    // `following`, not a table read once here: the game's mappings change as it
    // runs, and loading a save takes it from 1.5 GiB over 624 ranges to 5 GiB
    // over 2098. A splitter shown the table from the main menu scans a third of
    // the heap, finds nothing, and reads addresses that have been unmapped
    // since -- which is what the first recorded run produced.
    let process = test_harness::memory::FakeProcess::new(pid as u64, comm(pid))
        .with_memory(memory)
        .following(pid);

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
    /// watched, and captures if this tick was a moment.
    fn after_tick(&mut self, world: &World) -> bool {
        let fresh: Vec<&String> = world.log[self.seen_log..].iter().collect();
        for line in &fresh {
            println!("  | {line}");
        }
        let said = fresh
            .iter()
            .find(|line| !NOISE.iter().any(|n| line.contains(n)) && !is_probe_body(line))
            .map(|line| summarise(line));
        if !fresh.is_empty() {
            self.seen_log = world.log.len();
            self.last_activity = Instant::now();
        }

        let did = world.timer.events[self.seen_events..]
            .iter()
            .filter_map(describe)
            .next();
        self.seen_events = world.timer.events.len();

        // One capture a tick at most. What the splitter *did* names it in
        // preference to what it said, since a timer event is the thing a test
        // will assert on.
        if let Some(reason) = did.or(said) {
            self.last_activity = Instant::now();
            if self.step >= MAX_STEPS {
                self.error = Some(format!(
                    "stopping at {MAX_STEPS} steps. The splitter is logging more \
                     than this was built for; widen NOISE or raise MAX_STEPS."
                ));
                return false;
            }
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
        // A recording is one directory with a step per subdirectory, so it
        // moves, is deleted, or is copied to another machine as a unit -- and
        // a store holding several recordings and a few single captures reads
        // as a handful of entries rather than as sixty.
        let label = format!("step{:02}", self.step);
        let recording =
            snapshot::default_store().join(format!("{}-{}", self.version, self.scenario));
        let dir = recording.join(&label);
        if dir.exists() {
            return Err(format!(
                "{} already exists; move the old recording aside or pass a \
                 different --label",
                dir.display()
            ));
        }
        std::fs::create_dir_all(&recording)
            .map_err(|e| format!("creating {}: {e}", recording.display()))?;

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

/// A line of the probe's per-class dump, which is progress, not an event.
fn is_probe_body(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("ok ") || trimmed.starts_with("MISSING")
}

/// A log line, shortened enough to name a step by.
fn summarise(line: &str) -> String {
    let text: String = line
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    let slug: Vec<&str> = text.split_whitespace().take(5).collect();
    slug.join("-").to_lowercase()
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
