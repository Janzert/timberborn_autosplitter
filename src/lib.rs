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

/// Process names to try, in order. Windows is what runners use; the Linux name
/// is here so the splitter can be exercised on a native or Proton install.
const PROCESS_NAMES: &[&str] = &["Timberborn.exe", "Timberborn.x86_64"];

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
    loop {
        for name in PROCESS_NAMES {
            if let Some(process) = Process::attach(name) {
                asr::print_message(&format!("Attached to {name}."));
                return process;
            }
        }
        next_tick().await;
    }
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
