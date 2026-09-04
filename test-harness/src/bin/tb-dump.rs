//! Captures a snapshot of Timberborn's memory, for offline testing.
//!
//! ```text
//! cargo dump -- --label folktails-loaded --notes "save loaded, day 3, nothing built"
//! ```
//!
//! Reads the game from outside and changes nothing about it. It is a
//! development tool: nothing here ships to runners, and it is never run during
//! a submitted run.
//!
//! **Permissions.** This needs none of its own. When a direct open of
//! `/proc/<pid>/mem` is refused, [`LiveMemory::open`] runs `tb-ptrace-open`,
//! which holds `cap_sys_ptrace` and passes the descriptor back. That is also
//! where the "is this really Timberborn" check lives, since a check in the
//! caller is one the caller can skip.

use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use test_harness::{
    live::{self, LiveMemory},
    memory::Memory,
    snapshot::{self, Metadata, Writer},
};

/// Names the game may report. Under Proton `/proc/<pid>/comm` is capped at 15
/// characters and Unity 6.5 names its main thread, so the game usually appears
/// as `Unity Main Thre` -- the same trap the splitter has to work around.
const CANDIDATE_NAMES: &[&str] = &["Timberborn.x86_64", "Timberborn.exe", "Unity Main Thre"];

/// Read in chunks rather than whole ranges: a range can be hundreds of MiB and
/// a single failing page should not discard all of it.
const CHUNK: usize = 1 << 20;

struct Args {
    label: String,
    notes: String,
    pid: Option<u32>,
    dry_run: bool,
    freeze: bool,
    states: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        label: "unlabelled".into(),
        notes: String::new(),
        pid: None,
        dry_run: false,
        freeze: false,
        states: Vec::new(),
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--label" => args.label = argv.next().ok_or("--label needs a value")?,
            "--notes" => args.notes = argv.next().ok_or("--notes needs a value")?,
            "--pid" => {
                args.pid = Some(
                    argv.next()
                        .ok_or("--pid needs a value")?
                        .parse()
                        .map_err(|_| "--pid must be a number")?,
                )
            }
            "--dry-run" => args.dry_run = true,
            "--freeze" => args.freeze = true,
            "--state" => {
                let id = argv.next().ok_or("--state needs a value")?;
                // Checked here rather than at capture time: a typo would
                // otherwise produce a 5 GiB file no test will ever look at.
                if test_harness::requirement::get(&id).is_none() {
                    return Err(format!(
                        "no such state {id:?}. Known states:\n{}",
                        test_harness::requirement::listing()
                    ));
                }
                args.states.push(id);
            }
            "--help" | "-h" => return Err("help".into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dump: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) if message == "help" => {
            println!("{}", include_str!("tb-dump_help.txt"));
            println!("Known states:\n{}", test_harness::requirement::listing());
            return Ok(());
        }
        Err(message) => return Err(message),
    };

    let pid = match args.pid {
        // Checked even when named explicitly: the capability this runs with is
        // wider than the job, so the narrowing has to cover every path in.
        Some(pid) => {
            if !is_the_game(pid) {
                return Err(format!(
                    "pid {pid} has no Timberborn data mapped, so it is not the game.\n       \
                     This build refuses to read anything else."
                ));
            }
            pid
        }
        None => find_game()?,
    };

    let dir_of_game = game_dir(pid)
        .ok_or_else(|| format!("cannot tell where pid {pid} is running the game from"))?;
    let version = test_harness::install::version(&dir_of_game).ok_or_else(|| {
        format!(
            "cannot read the game's version from {}. Without it a capture cannot \n       \
             say which version it is of, which makes it worthless later.",
            dir_of_game.display()
        )
    })?;

    let memory = LiveMemory::open(pid).map_err(|e| format!("{e}"))?;

    let mappings = live::mappings(pid).map_err(|e| format!("cannot read /proc/{pid}/maps: {e}"))?;
    let modules = live::modules(&mappings);

    let capturable: Vec<_> = mappings
        .iter()
        .filter(|m| m.readable() && !m.is_special())
        .collect();
    let total: u64 = capturable.iter().map(|m| m.size).sum();

    println!("pid {pid}");
    println!("  {version} ({})", dir_of_game.display());
    println!("  {} mappings, {} modules", mappings.len(), modules.len());
    println!(
        "  {} readable ranges, {:.1} GiB to capture",
        capturable.len(),
        total as f64 / (1 << 30) as f64
    );
    if args.dry_run {
        println!("  --dry-run: nothing written");
        return Ok(());
    }

    let label = match (args.label.as_str(), args.states.first()) {
        ("unlabelled", Some(state)) => state.clone(),
        _ => args.label.clone(),
    };
    let dir = snapshot::default_store().join(format!("{version}-{label}"));
    if dir.exists() {
        return Err(format!(
            "{} already exists; remove it or pass a different --label",
            dir.display()
        ));
    }

    let metadata = Metadata {
        game_version: version.clone(),
        build_id: build_id(&dir_of_game),
        label: label.clone(),
        captured_at: timestamp(),
        process_name: comm(pid),
        process_path: std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned)),
        pid,
        notes: args.notes.clone(),
        frozen: args.freeze,
        satisfies: args.states.clone(),
        ..Default::default()
    };

    if args.states.is_empty() {
        println!(
            "note: no --state given, so no test will find this capture. Known states:\n{}",
            test_harness::requirement::listing()
        );
    }

    let mut writer = Writer::create(dir, metadata).map_err(|e| format!("cannot create: {e}"))?;
    writer.add_modules(modules);

    // Held for the whole read, so every range comes from one instant. Dropped
    // at the end of `run`, which covers the error paths as well as this one.
    let _frozen = if args.freeze {
        println!("stopping pid {pid} for the read; if this is interrupted, resume it with:");
        println!("  kill -CONT {pid}");
        Some(test_harness::freeze::Frozen::stop(pid)?)
    } else {
        println!(
            "not stopping the game, so ranges will be read seconds apart. \
             Pass --freeze for a consistent capture."
        );
        None
    };

    let started = Instant::now();
    let mut buffer = vec![0u8; CHUNK];
    let mut done = 0u64;
    for mapping in &capturable {
        let mut bytes = Vec::with_capacity(mapping.size as usize);
        let mut readable = true;
        let mut offset = 0u64;
        while offset < mapping.size {
            let want = CHUNK.min((mapping.size - offset) as usize);
            if !memory.read(mapping.address + offset, &mut buffer[..want]) {
                readable = false;
                break;
            }
            bytes.extend_from_slice(&buffer[..want]);
            offset += want as u64;
        }
        writer
            .add_range(mapping.to_range(), readable.then_some(bytes.as_slice()))
            .map_err(|e| format!("writing {:#x}: {e}", mapping.address))?;

        done += mapping.size;
        progress(done, total, started);
    }
    if io::stdout().is_terminal() {
        println!();
    }

    let skipped = writer.skipped;
    let written = writer.bytes_written();
    let dir = writer.finish().map_err(|e| format!("finishing: {e}"))?;

    if args.freeze {
        println!(
            "resuming pid {pid}, stopped for {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }
    println!(
        "captured {:.1} GiB into {}",
        written as f64 / (1 << 30) as f64,
        dir.display()
    );
    if skipped > 0 {
        println!(
            "  {skipped} ranges were listed but unreadable; recorded without bytes so \
             replay reports them as unreadable rather than absent"
        );
    }
    Ok(())
}

/// Picks the game's process, refusing rather than guessing when it is unclear.
fn find_game() -> Result<u32, String> {
    let mut found: Vec<u32> = Vec::new();
    for name in CANDIDATE_NAMES {
        for pid in live::find_by_comm(name).unwrap_or_default() {
            if is_the_game(pid) && !found.contains(&pid) {
                found.push(pid);
            }
        }
    }
    match found.len() {
        0 => Err("no process found with Timberborn_Data mapped. \
                  Start the game first, or pass --pid."
            .into()),
        1 => Ok(found[0]),
        _ => Err(format!(
            "several candidate processes ({found:?}); pass --pid to choose"
        )),
    }
}

/// Whether a process is Timberborn, and the guard on what this may read.
///
/// `/proc/<pid>/exe` is no use: under Proton it points at Wine's preloader
/// rather than at the game. What is reliable is what the process has *mapped*.
///
/// Identified by shape rather than by location, deliberately. Matching against
/// Steam's install directory refuses a perfectly real game running from
/// anywhere else -- and running an old build straight out of a version store,
/// where Steam cannot update it, is precisely how this project keeps more than
/// one version usable. `Timberborn_Data` is specific enough to the game that a
/// browser cannot match it, which is all the guard is for.
fn is_the_game(pid: u32) -> bool {
    let Ok(mappings) = live::mappings(pid) else {
        return false;
    };
    mappings
        .iter()
        .filter_map(|m| m.path.as_deref())
        .any(|path| {
            let path = path.to_string_lossy();
            path.contains("/Timberborn_Data/") || path.ends_with("/Timberborn/Timberborn.exe")
        })
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim_end().to_owned())
        .unwrap_or_default()
}

/// The game directory a process is actually running from, taken from what it
/// has mapped rather than from where Steam thinks the game is.
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

/// The Steam build id for a directory, if it can be established.
///
/// Steam's manifest describes the *installed* copy, so it only applies when the
/// process is running that copy. A saved version carries its own `version.json`
/// beside the game directory, written when the version was saved off.
fn build_id(dir: &std::path::Path) -> String {
    if std::fs::canonicalize(dir).ok() == std::fs::canonicalize(install_dir()).ok() {
        return manifest_value("buildid").unwrap_or_else(|| "unknown".into());
    }
    let saved = dir.parent().map(|p| p.join("version.json"));
    if let Some(text) = saved.and_then(|p| std::fs::read_to_string(p).ok()) {
        if let Some(rest) = text.split("\"build_id\"").nth(1) {
            if let Some(value) = rest.split('"').nth(1) {
                return value.to_owned();
            }
        }
    }
    "unknown".into()
}

/// Where Steam put the game, for telling "the installed copy" from any other.
fn install_dir() -> std::path::PathBuf {
    let install = manifest_value("installdir").unwrap_or_else(|| "Timberborn".into());
    let path = steamapps().join("common").join(install);
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// A top-level key from the app manifest, which is VDF: `"key"<tab>"value"`.
fn manifest_value(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(steamapps().join("appmanifest_1062090.acf")).ok()?;
    text.lines().find_map(|line| {
        let mut quoted = line.split('"').skip(1).step_by(2);
        (quoted.next()? == key).then(|| quoted.next().map(str::to_owned))?
    })
}

fn steamapps() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".steam/steam/steamapps")
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn progress(done: u64, total: u64, started: Instant) {
    // Carriage returns are fine on a terminal and 30 KB of noise in a log.
    if !io::stdout().is_terminal() {
        return;
    }
    let percent = if total == 0 {
        100.0
    } else {
        done as f64 * 100.0 / total as f64
    };
    print!(
        "\r  {percent:5.1}%  {:.1} GiB  {:.0}s",
        done as f64 / (1 << 30) as f64,
        started.elapsed().as_secs_f64()
    );
    let _ = io::stdout().flush();
}
