#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod scan;

use alloc::{format, string::String, vec::Vec};

use asr::{
    future::next_tick,
    game_engine::unity::mono::{Class, Image, Module},
    Address, Process,
};

asr::async_main!(stable);
asr::panic_handler!();

/// Process names to try, in order. Windows is what runners use; the Linux name
/// is here so the splitter can be exercised on a native or Proton install.
const PROCESS_NAMES: &[&str] = &["Timberborn.exe", "Timberborn.x86_64"];

/// Bytes to scan per tick. The whole address space is a few GiB, so this
/// spreads a scan over roughly a hundred ticks rather than blocking for
/// seconds. Tuned against Proton; revisit once there is a native Windows
/// number.
const SCAN_BUDGET: u64 = 32 << 20;

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
/// The previous run found six matches for a class that should be a singleton,
/// so this pass reports every candidate with the evidence needed to tell real
/// objects from stray words, rather than stopping at the first hit.
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

    let Some(vtable) = day_night_cycle.get_vtable(process, &module) else {
        asr::print_message("FAIL: could not read the DayNightCycle vtable.");
        return;
    };
    let Some(event_bus_vtable) = event_bus.get_vtable(process, &module) else {
        asr::print_message("FAIL: could not read the EventBus vtable.");
        return;
    };

    let Some(day_number) = day_night_cycle.get_field_offset(process, &module, "DayNumber") else {
        asr::print_message("FAIL: no DayNumber field. Has it been renamed?");
        return;
    };
    let Some(event_bus_field) = day_night_cycle.get_field_offset(process, &module, "_eventBus")
    else {
        asr::print_message("FAIL: no _eventBus field. Has it been renamed?");
        return;
    };

    asr::print_message(&format!(
        "DayNightCycle vtable {vtable}, DayNumber +0x{day_number:X}, \
         _eventBus +0x{event_bus_field:X} (EventBus vtable {event_bus_vtable})."
    ));

    // Scan the whole space rather than stopping early: we still need to see
    // what the extra matches are.
    let mut scan = scan::Scan::new(process, vtable);
    asr::print_message(&format!(
        "Scanning {} ranges ({} skipped)...",
        scan.stats.ranges_scanned, scan.stats.ranges_skipped
    ));
    while !scan.step(process, SCAN_BUDGET) {
        next_tick().await;
    }

    let s = scan.stats;
    asr::print_message(&format!(
        "Scan done: {} candidates | {:.1} MiB over {} ticks | {} read failures",
        scan.hits.len(),
        s.bytes_scanned as f64 / (1024.0 * 1024.0),
        s.ticks,
        s.read_failures,
    ));

    // Report every candidate with the evidence, then pick the validated one.
    let mut validated: Vec<Address> = Vec::new();
    for (i, &hit) in scan.hits.iter().enumerate() {
        let ok = scan::validate(process, hit, event_bus_field, event_bus_vtable);
        let bus = process
            .read::<u64>(hit.add(event_bus_field as u64))
            .map(|v| format!("{}", Address::new(v)))
            .unwrap_or_else(|_| String::from("<unreadable>"));
        let day = process
            .read::<i32>(hit.add(day_number as u64))
            .map(|v| format!("{v}"))
            .unwrap_or_else(|_| String::from("<unreadable>"));

        asr::print_message(&format!(
            "  [{i}] {hit} | _eventBus {bus} | DayNumber {day} | {}",
            if ok { "VALID" } else { "rejected" },
        ));
        if ok {
            validated.push(hit);
        }
    }

    match validated.len() {
        0 => {
            asr::print_message("FAIL: no candidate validated. The check is wrong, or the layout is.");
            return;
        }
        1 => asr::print_message("Exactly one candidate validated, as expected for a singleton."),
        n => asr::print_message(&format!(
            "NOTE: {n} candidates validated. Still ambiguous -- needs a stronger check."
        )),
    }

    let instance = validated[0];
    asr::print_message(&format!("Watching DayNumber on {instance}."));

    let mut last = None;
    loop {
        match process.read::<i32>(instance.add(day_number as u64)) {
            Ok(day) => {
                if last != Some(day) {
                    asr::print_message(&format!("DayNumber = {day}"));
                    last = Some(day);
                }
            }
            Err(_) => {
                asr::print_message("Lost the instance -- most likely a scene change. Rescanning.");
                return;
            }
        }
        next_tick().await;
    }
}
