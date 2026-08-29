#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod scan;

use alloc::{format, vec::Vec};

use asr::{
    future::next_tick,
    game_engine::unity::mono::{Image, Module},
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

/// The spike proper.
///
/// Answers one question: can we locate a service by scanning the heap for the
/// instance of its class, and is that fast enough to be usable? `DayNightCycle`
/// is the test subject because it is a singleton with a trivially checkable
/// field -- `DayNumber` should be >= 1 in a loaded game and should tick over as
/// days pass.
///
/// Watch the tick timing in asr-debugger while this runs; that is the actual
/// measurement, since wasm32-unknown-unknown gives us no clock of our own.
async fn spike(process: &Process) {
    let module = Module::wait_attach_auto_detect(process).await;
    asr::print_message("Attached to the Mono runtime.");

    let image: Image = module
        .wait_get_image(process, "Timberborn.TimeSystem")
        .await;
    asr::print_message("Found the Timberborn.TimeSystem image.");

    let class = image
        .wait_get_class(process, &module, "DayNightCycle")
        .await;

    let Some(vtable) = class.get_vtable(process, &module) else {
        asr::print_message("FAIL: could not read the class vtable.");
        return;
    };
    asr::print_message(&format!("DayNightCycle vtable at {vtable}."));

    let Some(day_number) = class.get_field_offset(process, &module, "DayNumber") else {
        asr::print_message("FAIL: no DayNumber field. Has it been renamed?");
        return;
    };
    asr::print_message(&format!("DayNumber is at instance offset 0x{day_number:X}."));

    // A singleton should produce exactly one hit. Collect a few more than that
    // so the log distinguishes "one instance" from "many, this class is not a
    // singleton after all".
    let mut hits = Vec::new();
    let stats = scan::find_instances(process, vtable, &mut hits, 16);

    asr::print_message(&format!(
        "Scan: {} hits | {} ranges scanned, {} skipped | {:.1} MiB read | {} read failures{}",
        hits.len(),
        stats.ranges_scanned,
        stats.ranges_skipped,
        stats.bytes_scanned as f64 / (1024.0 * 1024.0),
        stats.read_failures,
        if stats.truncated { " | TRUNCATED" } else { "" },
    ));

    match hits.len() {
        0 => {
            asr::print_message(
                "FAIL: no instance found. Is a save actually loaded? \
                 DayNightCycle only exists in a loaded game, not in the main menu.",
            );
            return;
        }
        1 => asr::print_message("Exactly one instance, as expected for a singleton."),
        n => asr::print_message(&format!(
            "NOTE: {n} instances. Disambiguation needed before this class can be used.",
        )),
    }

    // Prove the pointer is live and the field is the right one: log DayNumber
    // whenever it changes. In game this should count up as days pass.
    let instance = hits[0];
    asr::print_message(&format!("Watching DayNumber on the instance at {instance}."));

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
