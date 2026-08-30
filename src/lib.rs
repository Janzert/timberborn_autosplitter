#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod probe;
mod scan;
mod service;

use alloc::{format, vec::Vec};

use asr::{
    future::{next_tick, retry},
    game_engine::unity::mono::Module,
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

/// When to first say we are still looking, in ticks. The runtime ticks at
/// 120/s, so this is 15 seconds. Silence and failure looked identical before
/// this, which cost an evening of forensics.
const FIRST_SEARCH_NOTICE_TICKS: u32 = 1800;

/// How often to repeat it: every 5 minutes. Frequent enough to be visible in a
/// log, rare enough not to bury anything during a session with no game open.
const REPEAT_SEARCH_NOTICE_TICKS: u32 = 36_000;

/// How long to wait after the game closes before looking again, in ticks (~5s).
const PROCESS_GONE_DELAY_TICKS: u32 = 600;

/// How often to retry resolving the Wonder class, in ticks (~1s). It does not
/// exist until a wonder has been built.
const WONDER_RESOLVE_TICKS: u32 = 120;

/// How often to rescan for wonder instances while none are known, in ticks
/// (~2s). Only the scan is throttled: once an instance is located its flag is
/// read every tick, because this split ends the run and its latency is the
/// splitter's timing error.
const WONDER_RESCAN_TICKS: u32 = 240;

/// How often to forget which candidates were ruled out, in ticks. Pids are
/// reused, and a process rejected once may since have mapped the game.
const FORGET_RULED_OUT_TICKS: u32 = 1200;

/// How often to re-examine ambiguous candidates, in ticks. Checking one means
/// attaching to it, which the runtime logs, so doing it every tick turns a
/// dying game into a stream of attach/detach churn.
const AMBIGUOUS_RETRY_TICKS: u32 = 60;

/// Ticks to wait before rescanning after a scan comes up empty. Without this
/// the retry is a hot loop; the object we are waiting for appears on a human
/// timescale anyway.
const RESCAN_DELAY_TICKS: u32 = 120;


async fn main() {
    asr::print_message("Timberborn auto splitter (heap scan spike).");

    loop {
        let process = attach().await;
        process.until_closes(spike(&process)).await;
        // A process on its way out stays attachable for several seconds, and
        // each re-attach is logged, so wait longer here than between rescans.
        for _ in 0..PROCESS_GONE_DELAY_TICKS {
            next_tick().await;
        }
    }
}

async fn attach() -> Process {
    let mut waited = 0u32;
    let mut next_notice = FIRST_SEARCH_NOTICE_TICKS;
    // Candidates already ruled out. A game on its way out lingers for seconds,
    // and re-checking it means re-attaching to it, which the runtime logs.
    let mut ruled_out: Vec<asr::ProcessId> = Vec::new();

    loop {
        for name in EXACT_NAMES {
            if let Some(process) = Process::attach(name) {
                asr::print_message(&format!("Attached to {name}."));
                return process;
            }
        }

        if waited.is_multiple_of(AMBIGUOUS_RETRY_TICKS) {
            for name in AMBIGUOUS_NAMES {
                for pid in Process::list_by_name(name).unwrap_or_default() {
                    if ruled_out.contains(&pid) {
                        continue;
                    }
                    let Some(process) = Process::attach_by_pid(pid) else {
                        continue;
                    };
                    if is_timberborn(&process) {
                        asr::print_message(&format!(
                            "Attached to pid {pid:?}, which reports as \"{name}\" \
                             but has {GAME_MODULE} mapped."
                        ));
                        return process;
                    }
                    ruled_out.push(pid);
                }
            }
        }

        waited += 1;
        if waited.is_multiple_of(FORGET_RULED_OUT_TICKS) {
            ruled_out.clear();
        }
        if waited >= next_notice {
            asr::print_message("Still looking for Timberborn...");
            next_notice = waited.saturating_add(REPEAT_SEARCH_NOTICE_TICKS);
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

/// The spike, now exercising the pieces the real splits will use.
///
/// Locates `DayNightCycle` and watches `DayNumber`, and alongside it watches
/// for the wonder being activated -- the first actual split condition, chosen
/// because `Wonder.IsActive` is a plain bool and needs no knowledge of BCL
/// collection layouts.
async fn spike(process: &Process) {
    let module = Module::wait_attach_auto_detect(process).await;
    asr::print_message("Attached to the Mono runtime.");

    // Everything hangs off this: it is the shared validation target, and it
    // only exists once the game has constructed an EventBus.
    asr::print_message("Waiting for the game to load...");
    let event_bus_vtable = retry(|| service::event_bus_vtable(process, &module)).await;

    let clock = retry(|| {
        service::Locatable::new(
            process,
            &module,
            "Timberborn.TimeSystem",
            "DayNightCycle",
            event_bus_vtable,
        )
    })
    .await;

    let Some(day_number) = clock.field(process, &module, "DayNumber") else {
        asr::print_message("FAIL: DayNightCycle has no DayNumber. Renamed?");
        return;
    };

    let mut probed = false;
    loop {
        let found = clock.find_one(process).await;
        let Some(instance) = found.first else {
            asr::print_message(if found.conclusive {
                "No DayNightCycle -- no game loaded. Waiting."
            } else {
                "No DayNightCycle, but the scan was incomplete, so this is not \
                 conclusive. Waiting."
            });
            for _ in 0..RESCAN_DELAY_TICKS {
                next_tick().await;
            }
            continue;
        };
        asr::print_message(&format!("Found {} at {instance}.", clock.name()));

        if !probed {
            probe::run(process, &module);
            probed = true;
        }

        watch(process, &module, &clock, instance, day_number, event_bus_vtable).await;
        asr::print_message("Scene change. Rescanning.");
    }
}

/// Watches the loaded game until the scene changes.
async fn watch(
    process: &Process,
    module: &Module,
    clock: &service::Locatable,
    instance: Address,
    day_number: u32,
    event_bus_vtable: Address,
) {
    let mut last_day = None;
    let mut wonder: Option<Wonder> = None;
    let mut completion: Option<WonderCompletion> = None;
    let mut activated = false;
    let mut ended = false;
    let mut ticks = 0u32;

    let mut run_start = RunStart::resolve(process, module, event_bus_vtable).await;

    loop {
        if !clock.still_valid(process, instance) {
            return;
        }

        if let Some(start) = &mut run_start {
            if start.poll(process) {
                asr::print_message("SPLIT would fire: RUN START -- overlay shown.");
            }
        }

        if let Ok(day) = process.read::<i32>(instance.add(day_number as u64)) {
            if last_day != Some(day) {
                asr::print_message(&format!("DayNumber = {day}"));
                last_day = Some(day);
            }
        }

        // Neither class exists until a wonder has been built, so keep trying
        // rather than giving up at load time.
        if ticks.is_multiple_of(WONDER_RESOLVE_TICKS) {
            if wonder.is_none() {
                wonder = Wonder::resolve(process, module, event_bus_vtable);
            }
            if completion.is_none() {
                completion = WonderCompletion::resolve(process, module, event_bus_vtable).await;
            }
        }

        if let Some(w) = &mut wonder {
            w.update(process, ticks).await;
            if !activated && w.is_active(process) {
                let day = completion.as_ref().and_then(|c| c.unlock_day(process));
                asr::print_message(&format!(
                    "Wonder activated (NOT the run end). Completion due on day {day:?}."
                ));
                activated = true;
            }
        }

        // The actual run end: the Congratulations screen. Read every tick.
        if let Some(c) = &mut completion {
            c.report_length(process, module, clock, instance);
            if !ended && c.finished(process) {
                asr::print_message("SPLIT would fire: RUN END -- wonder completion countdown finished.");
                ended = true;
            }
        }

        ticks = ticks.wrapping_add(1);
        next_tick().await;
    }
}

/// Tracks the wonder buildings and whether any has been activated.
///
/// A wonder is a building rather than a singleton, so there may be several, and
/// none exist until one is built. Locating them is a full scan; checking them
/// afterwards is one byte each, which is why the two are separated.
struct Wonder {
    class: service::Locatable,
    is_active: u32,
    instances: alloc::vec::Vec<Address>,
}

impl Wonder {
    fn resolve(process: &Process, module: &Module, event_bus_vtable: Address) -> Option<Self> {
        let class = service::Locatable::new(
            process,
            module,
            "Timberborn.Wonders",
            "Wonder",
            event_bus_vtable,
        )?;
        let is_active = class.field(process, module, "IsActive")?;
        // Resolving only proves the class has been constructed once, which a
        // prefab does at load. Whether a wonder is actually built is decided by
        // the scan in update().
        asr::print_message("Wonder class resolved; scanning for built wonders.");
        Some(Self {
            class,
            is_active,
            instances: alloc::vec::Vec::new(),
        })
    }

    /// Rescans only when we have nothing to watch, or when what we were
    /// watching stopped being valid -- a wonder demolished and rebuilt, say.
    async fn update(&mut self, process: &Process, ticks: u32) {
        let stale = self
            .instances
            .iter()
            .any(|&i| !self.class.still_valid(process, i));

        if (self.instances.is_empty() || stale) && ticks.is_multiple_of(WONDER_RESCAN_TICKS) {
            let found = self.class.find_all(process).await;
            if !found.all.is_empty() || found.conclusive {
                self.instances = found.all;
            }
        }
    }

    /// One byte per wonder. Cheap enough to run every tick.
    fn is_active(&self, process: &Process) -> bool {
        self.instances.iter().any(|&i| {
            process
                .read::<u8>(i.add(self.is_active as u64))
                .is_ok_and(|active| active != 0)
        })
    }
}

/// The run-end condition, per the category rules: the "Congratulations!"
/// screen, not the wonder being activated.
///
/// Activating a wonder starts a countdown of `UnlockOffsetInHours` in-game
/// hours. Only when that finishes does `WonderCompletedEvent` fire and
/// `WonderCompletionPanel` -- the Congratulations screen -- appear. So
/// `CountdownFinished` is the signal, and `Wonder.IsActive` is strictly
/// earlier. Both are watched so the gap between them can be measured.
struct WonderCompletion {
    class: service::Locatable,
    countdown_finished: u32,
    unlock_day: u32,
    instance: Address,
    /// Whether the real-time length of the countdown has been reported yet.
    /// The clock reads zero during load, so this is retried until it is sane.
    reported_length: bool,
    /// The value when we first saw it. A save that already completed its wonder
    /// loads with this true, and that must not read as the run ending.
    was_finished_on_arrival: bool,
}

impl WonderCompletion {
    async fn resolve(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
    ) -> Option<Self> {
        let class = service::Locatable::new(
            process,
            module,
            "Timberborn.GameWonderCompletion",
            "WonderCompletionCountdownStarter",
            event_bus_vtable,
        )?;



        let countdown_finished = class.field(process, module, "CountdownFinished")?;
        let unlock_day = class.field(process, module, "_unlockDay")?;
        let instance = class.find_one(process).await.first?;

        let already = process
            .read::<u8>(instance.add(countdown_finished as u64))
            .is_ok_and(|done| done != 0);
        asr::print_message(&format!(
            "Watching wonder completion at {instance} (run end per the rules). \
             Already finished in this save: {already}."
        ));

        Some(Self {
            class,
            countdown_finished,
            unlock_day,
            instance,
            reported_length: false,
            was_finished_on_arrival: already,
        })
    }

    /// Reports how far activation precedes the run end, once the clock has
    /// values to compute it from.
    fn report_length(
        &mut self,
        process: &Process,
        module: &Module,
        clock: &service::Locatable,
        clock_instance: Address,
    ) {
        if self.reported_length {
            return;
        }
        self.reported_length =
            report_countdown_length(process, module, &self.class, clock, clock_instance);
    }

    /// True only on a transition we actually observed.
    fn finished(&self, process: &Process) -> bool {
        !self.was_finished_on_arrival
            && process
                .read::<u8>(self.instance.add(self.countdown_finished as u64))
                .is_ok_and(|done| done != 0)
    }

    fn unlock_day(&self, process: &Process) -> Option<f32> {
        process.read::<f32>(self.instance.add(self.unlock_day as u64)).ok()
    }
}

/// Works out how far the run end sits behind wonder activation, in real time.
///
/// The countdown is expressed in in-game hours, and `DayNightCycle` knows how
/// long a day is in both hours and real seconds, so the gap can be computed
/// from any save rather than waiting to observe a completion -- which is
/// otherwise a whole run, and only happens once per map.
fn report_countdown_length(
    process: &Process,
    module: &Module,
    countdown: &service::Locatable,
    clock: &service::Locatable,
    clock_instance: Address,
) -> bool {
    let read = |offset: Option<u32>| -> Option<f32> {
        process.read::<f32>(clock_instance.add(offset? as u64)).ok()
    };

    let Some(hours) = countdown
        .static_field(process, module, "UnlockOffsetInHours")
        .and_then(|addr| process.read::<f32>(addr).ok())
    else {
        return false;
    };

    let day_seconds = read(clock.field(process, module, "DayLengthInSeconds"));
    let daytime = read(clock.field(process, module, "DaytimeLengthInHours"));
    let nighttime = read(clock.field(process, module, "NighttimeLengthInHours"));

    match (day_seconds, daytime, nighttime) {
        (Some(day_seconds), Some(daytime), Some(nighttime)) if daytime + nighttime > 0.0 => {
            let seconds = hours * (day_seconds / (daytime + nighttime));
            asr::print_message(&format!(
                "Countdown: {hours} in-game hours = {seconds:.1}s real time at 1x \
                 (day is {daytime}+{nighttime}h in {day_seconds}s). This is how far \
                 activation precedes the run end."
            ));
            true
        }
        // The clock reads zero until the game finishes loading; try again.
        _ => false,
    }
}

/// The run-start condition, per the category rules: "starts when the overlay
/// appears after choosing your settlement name".
///
/// `GameInitializer` steps through an `InitializationState` enum, whose members
/// name the sequence exactly:
///
/// ```text
/// 0 Waiting  1 SpawnBeavers  2 PostSpawnBeavers  3 UnpauseGame  4 ShowUI  5 Finished
/// ```
///
/// It sits on `Waiting` while the settlement-name dialog is up and steps
/// through the rest within a few ticks of confirming it. `ShowUI` is the step
/// that puts the overlay on screen, so that is the split.
///
/// `SpeedManager.CurrentSpeed` was the other candidate and is unusable: it goes
/// to 1 at `UnpauseGame`, one step early, and then toggles every time the
/// player pauses.
struct RunStart {
    instance: Address,
    offset: u32,
    /// Whether a state before `ShowUI` has been seen. Attaching to a game
    /// already in progress must not count as a start.
    seen_before_ui: bool,
    fired: bool,
    last: Option<i32>,
}

/// `InitializationState.ShowUI`.
const SHOW_UI: i32 = 4;

impl RunStart {
    async fn resolve(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
    ) -> Option<Self> {
        let class = service::Locatable::new(
            process,
            module,
            "Timberborn.GameStartup",
            "GameInitializer",
            event_bus_vtable,
        )?;
        let offset = class.field(process, module, "_initializationState")?;
        let instance = class.find_one(process).await.first?;
        asr::print_message(&format!("Watching run start at {instance}."));
        Some(Self {
            instance,
            offset,
            seen_before_ui: false,
            fired: false,
            last: None,
        })
    }

    /// One read. Called every tick, because run start is as timing-critical as
    /// run end.
    fn poll(&mut self, process: &Process) -> bool {
        let Ok(state) = process.read::<i32>(self.instance.add(self.offset as u64)) else {
            return false;
        };

        if self.last != Some(state) {
            asr::print_message(&format!("initializationState = {state}{}", name_of(state)));
            self.last = Some(state);
        }

        if state < SHOW_UI {
            self.seen_before_ui = true;
            // A fresh game start after a previous one, so allow firing again.
            self.fired = false;
        } else if self.seen_before_ui && !self.fired {
            self.fired = true;
            return true;
        }
        false
    }
}

fn name_of(state: i32) -> &'static str {
    match state {
        0 => " (Waiting)",
        1 => " (SpawnBeavers)",
        2 => " (PostSpawnBeavers)",
        3 => " (UnpauseGame)",
        4 => " (ShowUI)",
        5 => " (Finished)",
        _ => "",
    }
}
