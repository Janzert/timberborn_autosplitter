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
//! **Permissions.** Reading another process's memory needs ptrace rights. The
//! grant is a file capability on an installed copy, which persists across
//! reboots and leaves `kernel.yama.ptrace_scope` alone for the rest of the
//! machine:
//!
//! ```text
//! cargo install --path test-harness --bin tb-dump --root ~/.local
//! sudo setcap cap_sys_ptrace+ep ~/.local/bin/tb-dump
//! ```
//!
//! `cap_sys_ptrace` would let this binary read any process the user owns, so
//! [`is_the_game`] refuses every process with nothing mapped from the
//! Timberborn install. That is a guard against a mistyped `--pid` rather than a
//! security boundary, but it means a slip cannot read a browser.

use std::{
    io::{self, Write},
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
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        label: "unlabelled".into(),
        notes: String::new(),
        pid: None,
        dry_run: false,
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
                    "pid {pid} has nothing mapped from {}, so it is not Timberborn.\n                            This build refuses to read anything else.",
                    install_dir().display()
                ));
            }
            pid
        }
        None => find_game()?,
    };

    let memory = LiveMemory::open(pid).map_err(|e| {
        format!(
            "cannot read /proc/{pid}/mem: {e}.\n       \
             Reading another process needs ptrace rights. Grant them to an \
             installed copy:\n       \
             cargo install --path test-harness --bin tb-dump --root ~/.local\n       \
             sudo setcap cap_sys_ptrace+ep ~/.local/bin/tb-dump\n       \
             Rebuilding drops the capability, so a reinstall needs the setcap again."
        )
    })?;

    let mappings = live::mappings(pid).map_err(|e| format!("cannot read /proc/{pid}/maps: {e}"))?;
    let modules = live::modules(&mappings);

    let capturable: Vec<_> = mappings
        .iter()
        .filter(|m| m.readable() && !m.is_special())
        .collect();
    let total: u64 = capturable.iter().map(|m| m.size).sum();

    println!("pid {pid}");
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

    let dir = snapshot::default_store().join(format!("{}-{}", build_id(), args.label));
    if dir.exists() {
        return Err(format!(
            "{} already exists; remove it or pass a different --label",
            dir.display()
        ));
    }

    let metadata = Metadata {
        build_id: build_id(),
        label: args.label.clone(),
        captured_at: timestamp(),
        process_name: comm(pid),
        process_path: std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned)),
        pid,
        notes: args.notes.clone(),
    };

    let mut writer = Writer::create(dir, metadata).map_err(|e| format!("cannot create: {e}"))?;
    writer.add_modules(modules);

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
    println!();

    let skipped = writer.skipped;
    let written = writer.bytes_written();
    let dir = writer.finish().map_err(|e| format!("finishing: {e}"))?;

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
        0 => Err(format!(
            "no process found with anything mapped from {}.\n       \
             Start the game first, or pass --pid.",
            install_dir().display()
        )),
        1 => Ok(found[0]),
        _ => Err(format!(
            "several candidate processes ({found:?}); pass --pid to choose"
        )),
    }
}

/// Whether a process is Timberborn, and the guard on what this may read.
///
/// `/proc/<pid>/exe` is no use: under Proton it points at Wine's preloader
/// rather than at the game. What is reliable is that the game has the install
/// directory mapped -- its assemblies, `mono-2.0-bdwgc.dll`, `Timberborn.exe`.
fn is_the_game(pid: u32) -> bool {
    let install = install_dir();
    let Ok(mappings) = live::mappings(pid) else {
        return false;
    };
    mappings
        .iter()
        .filter_map(|m| m.path.as_deref())
        .any(|path| path.starts_with(&install))
}

/// Where Steam put the game, from its own app manifest.
///
/// Canonicalized, because `~/.steam/steam` is a symlink to the real Steam
/// directory and `/proc/<pid>/maps` reports the resolved path. Comparing the
/// two unresolved matches nothing, and presents as "no Timberborn process
/// found" while the game is plainly running.
fn install_dir() -> std::path::PathBuf {
    let install = manifest_value("installdir").unwrap_or_else(|| "Timberborn".into());
    let path = steamapps().join("common").join(install);
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim_end().to_owned())
        .unwrap_or_default()
}

/// The installed build, straight from Steam's app manifest, so a snapshot is
/// never left guessing which version it is of.
fn build_id() -> String {
    manifest_value("buildid").unwrap_or_else(|| "unknown".into())
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
