#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod scan;

use alloc::format;

use asr::{
    future::next_tick,
    game_engine::unity::mono::{Class, Image, Module},
    Process,
};

asr::async_main!(stable);
asr::panic_handler!();

/// Process names to try, in order. Windows is what runners use; the Linux name
/// is here so the splitter can be exercised on a native or Proton install.
const PROCESS_NAMES: &[&str] = &["Timberborn.exe", "Timberborn.x86_64"];


async fn main() {
    asr::print_message("Timberborn auto splitter (heap scan spike).");

    loop {
        let process = attach().await;
        process.until_closes(spike(&process)).await;
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
/// The scan stops at the first match that passes validation, which is sound
/// because validation is reliable: measured against the game, one candidate
/// passed and five -- words inside Mono's own metadata -- were rejected.
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

    let (Some(vtable), Some(event_bus_vtable)) = (
        day_night_cycle.get_vtable(process, &module),
        event_bus.get_vtable(process, &module),
    ) else {
        asr::print_message("FAIL: could not read a class vtable.");
        return;
    };

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

    let scan = scan::Scan::new(process, vtable)
        .validating(validator)
        .stop_at_first()
        .run(process, scan::DEFAULT_BUDGET)
        .await;

    let s = scan.stats;
    asr::print_message(&format!(
        "Scan: {:.1} of {:.1} MiB ({:.1}%) over {} slices | {} rejected | {} read failures",
        s.bytes_scanned as f64 / (1024.0 * 1024.0),
        s.bytes_total as f64 / (1024.0 * 1024.0),
        100.0 * s.bytes_scanned as f64 / s.bytes_total.max(1) as f64,
        s.slices,
        scan.rejected.len(),
        s.read_failures,
    ));

    let Some(&instance) = scan.found.first() else {
        asr::print_message(
            "FAIL: no instance found. Is a save loaded? DayNightCycle only \
             exists in a loaded game, not in the main menu.",
        );
        return;
    };
    asr::print_message(&format!("Found DayNightCycle at {instance}. Watching DayNumber."));

    let mut last = None;
    loop {
        // Revalidating is two reads and detects a scene change directly, rather
        // than waiting for the read to start failing.
        if !validator.accepts(process, instance) {
            asr::print_message("Instance no longer valid -- scene change. Rescanning.");
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
