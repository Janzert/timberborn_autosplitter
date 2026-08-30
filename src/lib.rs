#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod probe;
mod scan;

use alloc::format;

use asr::{
    future::{next_tick, retry},
    game_engine::unity::mono::{Class, Image, Module},
    Address, Process,
};

asr::async_main!(stable);
asr::panic_handler!();

/// Names that identify the game on their own. Windows reports the executable
/// name, which is what runners will hit.
const EXACT_NAMES: &[&str] = &["Timberborn.exe", "Timberborn.x86_64"];

/// Names that are *not* specific to this game and must be confirmed before use.
///
/// The runtime matches on the name the OS reports, which on Linux is
/// `/proc/<pid>/comm`, capped at 15 characters. Unity 6.5 names its main thread
/// "Unity Main Thread", so a Proton install reports `Unity Main Thre` and never
/// matches the executable name. Unity 6.3 did not do this, which is why it only
/// started failing on the experimental branch.
///
/// Any Unity 6.5 game would match, so a candidate is only accepted once we can
/// see Timberborn's own module in it.
const AMBIGUOUS_NAMES: &[&str] = &["Unity Main Thre"];

/// The module that confirms an ambiguous match really is Timberborn.
const GAME_MODULE: &str = "Timberborn.exe";

/// How often to say we are still looking, in ticks. Silence and failure looked
/// identical before this, which cost an evening of forensics.
const SEARCHING_NOTICE_TICKS: u32 = 1800;

/// Ticks to wait before rescanning after a scan comes up empty. Without this
/// the retry is a hot loop; the object we are waiting for appears on a human
/// timescale anyway.
const RESCAN_DELAY_TICKS: u32 = 120;


async fn main() {
    asr::print_message("Timberborn auto splitter (heap scan spike).");

    loop {
        let process = attach().await;
        process.until_closes(spike(&process)).await;
        // A process on its way out can still be attachable for a while. Without
        // this the loop re-attaches to it once per tick.
        for _ in 0..RESCAN_DELAY_TICKS {
            next_tick().await;
        }
    }
}

async fn attach() -> Process {
    let mut waited = 0u32;
    loop {
        for name in EXACT_NAMES {
            if let Some(process) = Process::attach(name) {
                asr::print_message(&format!("Attached to {name}."));
                return process;
            }
        }

        for name in AMBIGUOUS_NAMES {
            for pid in Process::list_by_name(name).unwrap_or_default() {
                let Some(process) = Process::attach_by_pid(pid) else {
                    continue;
                };
                if is_timberborn(&process) {
                    asr::print_message(&format!(
                        "Attached to pid {pid:?}, which reports as \"{name}\" but \
                         has {GAME_MODULE} mapped."
                    ));
                    return process;
                }
            }
        }

        waited += 1;
        if waited.is_multiple_of(SEARCHING_NOTICE_TICKS) {
            asr::print_message("Still looking for Timberborn...");
        }
        next_tick().await;
    }
}

/// Confirms a process really is the game, for names that could be any Unity
/// title. The executable is mapped into the process even under Proton, where
/// the process path itself points at the Wine loader rather than the game.
fn is_timberborn(process: &Process) -> bool {
    if process.get_module_address(GAME_MODULE).is_ok() {
        return true;
    }
    process
        .get_path()
        .is_ok_and(|path| path.contains("Timberborn"))
}

/// The spike.
///
/// Locates `DayNightCycle` by scanning for the instance of its class, then
/// watches `DayNumber` to prove the pointer is live and the offset is right.
///
/// Runs for as long as the process lives, riding out scene changes: the class
/// has no vtable until it is first instantiated, and no instance exists in the
/// main menu, so both are waited on rather than treated as failures.
async fn spike(process: &Process) {
    let module = Module::wait_attach_auto_detect(process).await;
    asr::print_message("Attached to the Mono runtime.");

    let time_system: Image = module
        .wait_get_image(process, "Timberborn.TimeSystem")
        .await;
    let singleton_system: Image = module
        .wait_get_image(process, "Timberborn.SingletonSystem")
        .await;
    let day_night_cycle: Class = time_system
        .wait_get_class(process, &module, "DayNightCycle")
        .await;
    let event_bus: Class = singleton_system
        .wait_get_class(process, &module, "EventBus")
        .await;

    // Mono only fills in a class's vtable once the class is first
    // instantiated, so this doubles as "wait until a game is actually loaded".
    asr::print_message("Waiting for DayNightCycle to be instantiated...");
    let vtable = retry(|| day_night_cycle.get_vtable(process, &module)).await;
    let event_bus_vtable = retry(|| event_bus.get_vtable(process, &module)).await;

    // Now a save is loaded, so lazily-loaded assemblies are present.
    probe::run(process, &module);

    let (Some(day_number), Some(event_bus_field)) = (
        day_night_cycle.get_field_offset(process, &module, "DayNumber"),
        day_night_cycle.get_field_offset(process, &module, "_eventBus"),
    ) else {
        asr::print_message("FAIL: a field is missing. Has it been renamed?");
        return;
    };

    asr::print_message(&format!(
        "DayNightCycle vtable {vtable}, DayNumber +0x{day_number:X}, \
         _eventBus +0x{event_bus_field:X} (EventBus vtable {event_bus_vtable})."
    ));

    let validator = scan::Validator {
        field_offset: event_bus_field,
        expected_vtable: event_bus_vtable,
    };

    loop {
        let scan = scan::Scan::new(process, vtable)
            .validating(validator)
            .stop_at_first()
            .run(process, scan::DEFAULT_BUDGET)
            .await;

        let s = scan.stats;
        asr::print_message(&format!(
            "Scan: {:.1} of {:.1} MiB ({:.1}%) over {} slices | {} rejected \
             | {} chunk retries | {:.1} MiB unreadable",
            s.bytes_scanned as f64 / (1024.0 * 1024.0),
            s.bytes_total as f64 / (1024.0 * 1024.0),
            100.0 * s.bytes_scanned as f64 / s.bytes_total.max(1) as f64,
            s.slices,
            scan.rejected.len(),
            s.read_failures,
            s.bytes_unreadable as f64 / (1024.0 * 1024.0),
        ));

        let Some(&instance) = scan.found.first() else {
            asr::print_message(if scan.is_conclusive() {
                "No instance -- no game loaded. Waiting."
            } else {
                "No instance, but the scan was incomplete, so this negative is \
                 not conclusive. Waiting."
            });
            for _ in 0..RESCAN_DELAY_TICKS {
                next_tick().await;
            }
            continue;
        };

        asr::print_message(&format!("Found DayNightCycle at {instance}. Watching DayNumber."));
        watch(process, instance, day_number, &validator).await;
        asr::print_message("Instance no longer valid -- scene change. Rescanning.");
    }
}

/// Reports `DayNumber` as it changes, until the instance stops validating.
async fn watch(process: &Process, instance: Address, day_number: u32, validator: &scan::Validator) {
    let mut last = None;
    loop {
        // Revalidating is two reads and detects a scene change directly, rather
        // than waiting for the read to start failing.
        if !validator.accepts(process, instance) {
            return;
        }
        if let Ok(day) = process.read::<i32>(instance.add(day_number as u64)) {
            if last != Some(day) {
                asr::print_message(&format!("DayNumber = {day}"));
                last = Some(day);
            }
        }
        next_tick().await;
    }
}
