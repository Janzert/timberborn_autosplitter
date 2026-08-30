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

    let mut start_signals = StartSignals::resolve(process, module, event_bus_vtable).await;

    loop {
        if !clock.still_valid(process, instance) {
            return;
        }

        start_signals.poll(process);

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
                completion =
                    WonderCompletion::resolve(process, module, event_bus_vtable, clock, instance)
                        .await;
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
        if let Some(c) = &completion {
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
        asr::print_message("A wonder exists; watching for activation.");
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
    countdown_finished: u32,
    unlock_day: u32,
    instance: Address,
    /// The value when we first saw it. A save that already completed its wonder
    /// loads with this true, and that must not read as the run ending.
    was_finished_on_arrival: bool,
}

impl WonderCompletion {
    async fn resolve(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
        clock: &service::Locatable,
        clock_instance: Address,
    ) -> Option<Self> {
        let class = service::Locatable::new(
            process,
            module,
            "Timberborn.GameWonderCompletion",
            "WonderCompletionCountdownStarter",
            event_bus_vtable,
        )?;

        report_countdown_length(process, module, &class, clock, clock_instance);


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
            countdown_finished,
            unlock_day,
            instance,
            was_finished_on_arrival: already,
        })
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
) {
    let read = |offset: Option<u32>| -> Option<f32> {
        process.read::<f32>(clock_instance.add(offset? as u64)).ok()
    };

    let Some(hours) = countdown
        .static_field(process, module, "UnlockOffsetInHours")
        .and_then(|addr| process.read::<f32>(addr).ok())
    else {
        return;
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
        }
        _ => asr::print_message(&format!(
            "Countdown: {hours} in-game hours (could not read day length to convert)."
        )),
    }
}

/// Candidate signals for run start, logged for comparison against the rules:
/// "starts when the overlay appears after choosing your settlement name".
///
/// Neither is confirmed yet. `GameInitializer._initializationState` is an enum
/// that should step through startup, and `SpeedManager.CurrentSpeed` should go
/// from paused to running when the naming dialog closes. Both live on
/// singletons that exist *while* the dialog is up, so they can be located in
/// advance and then polled every tick -- which is what run-start accuracy
/// needs.
struct StartSignals {
    init_state: Option<(Address, u32)>,
    speed: Option<(Address, u32)>,
    last: Option<(i32, f32)>,
}

impl StartSignals {
    async fn resolve(process: &Process, module: &Module, event_bus_vtable: Address) -> Self {
        let mut signals = Self {
            init_state: None,
            speed: None,
            last: None,
        };

        if let Some(class) = service::Locatable::new(
            process,
            module,
            "Timberborn.GameStartup",
            "GameInitializer",
            event_bus_vtable,
        ) {
            if let (Some(offset), Some(instance)) = (
                class.field(process, module, "_initializationState"),
                class.find_one(process).await.first,
            ) {
                signals.init_state = Some((instance, offset));
            }
        }

        if let Some(class) = service::Locatable::new(
            process,
            module,
            "Timberborn.TimeSystem",
            "SpeedManager",
            event_bus_vtable,
        ) {
            if let (Some(offset), Some(instance)) = (
                class.field(process, module, "CurrentSpeed"),
                class.find_one(process).await.first,
            ) {
                signals.speed = Some((instance, offset));
            }
        }

        asr::print_message(&format!(
            "Start signals: GameInitializer {}, SpeedManager {}.",
            if signals.init_state.is_some() { "found" } else { "MISSING" },
            if signals.speed.is_some() { "found" } else { "MISSING" },
        ));
        signals
    }

    /// Logs on change, so a new game start shows the exact transition.
    fn poll(&mut self, process: &Process) {
        let state = self
            .init_state
            .and_then(|(a, o)| process.read::<i32>(a.add(o as u64)).ok())
            .unwrap_or(-1);
        let speed = self
            .speed
            .and_then(|(a, o)| process.read::<f32>(a.add(o as u64)).ok())
            .unwrap_or(-1.0);

        if self.last != Some((state, speed)) {
            asr::print_message(&format!(
                "start signals: initializationState={state} currentSpeed={speed}"
            ));
            self.last = Some((state, speed));
        }
    }
}
