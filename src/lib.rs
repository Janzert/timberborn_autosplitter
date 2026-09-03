#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod collections;
mod probe;
mod scan;
mod service;
mod singletons;
mod status;

use alloc::{format, vec::Vec};

use asr::{
    future::{next_tick, retry},
    game_engine::unity::mono::Module,
    settings::Gui,
    timer::{self, TimerState},
    Address, Process,
};

/// Which splits are enabled. The set and order follow the mod-based splitter
/// this replaces, except that the advanced science split covers both factions'
/// buildings and the run end is named for the Congratulations screen.
#[derive(Gui)]
struct Settings {
    /// Start the run when the overlay appears after naming the settlement
    #[default = true]
    start: bool,

    /// Split when the Forester is finished
    #[default = true]
    forester: bool,

    /// Split when the Gear Workshop is finished
    #[default = true]
    gear_workshop: bool,

    /// Split when the Tapper's Shack is finished
    #[default = true]
    tappers_shack: bool,

    /// Split when the faction's advanced science building is finished
    ///
    /// The Observatory for Folktails, the Numbercruncher for Iron Teeth. The
    /// two factions are separate categories, so only one can ever fire in a
    /// given run.
    #[default = true]
    advanced_science: bool,

    /// Split when both the Smelter and Wood Workshop are finished
    #[default = true]
    smelter_woodworkshop: bool,

    /// Split when the wonder is unlocked with science
    #[default = true]
    unlock_wonder: bool,

    /// Split when the Congratulations screen appears
    ///
    /// The category rules end the run here, which is roughly 0.5 in-game hours
    /// after the wonder is activated -- not at activation itself. The wonder is
    /// the prerequisite; this screen is the official end time.
    #[default = true]
    congratulations_screen: bool,
}

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

/// Ticks per second asked for while a game is attached.
///
/// asr's own default, and what everything timing-critical is calibrated
/// against: the splits are polled every tick, so this is the resolution of a
/// split. See `docs/DESIGN.md`, *Split latency*.
const ATTACHED_TICK_RATE: f64 = 120.0;

/// Ticks per second asked for while there is no game to watch.
///
/// The search loop does nothing but list processes, and a game appears on a
/// human timescale, so polling it 120 times a second buys nothing and costs a
/// wakeup every 8ms for as long as LiveSplit is open -- which, for a runner who
/// leaves it open, is most of the day. One a second is CryZe's idiom and is
/// still far quicker than a game can start.
const DETACHED_TICKS_PER_SEC: u32 = 1;

/// Seconds converted to ticks at the detached rate, for the constants used by
/// the search loop only.
const fn detached_ticks(secs: u32) -> u32 {
    secs * DETACHED_TICKS_PER_SEC
}

// The "~Ns" figures on the attached constants below are at 120 ticks/s, which
// is what asr asks the host for and what asr-debugger delivers. LiveSplit
// measures at ~107/s (9.4ms) with no game attached, and ~89/s (11.3ms) while
// watching a loaded save: its update timer is stopped for the duration of each
// step and restarted afterwards, so the period is the interval plus our own
// per-tick work, and that work scales with the settlement. Every duration below
// is therefore a lower bound, ~12% to ~35% longer in LiveSplit than the figure
// in its comment. None of them are timing-critical -- they are retry and log
// intervals, and the splits themselves are polled every tick.
//
// The constants written as `detached_ticks(N)` are the exception: they are used
// only while detached, where the rate is ours rather than the host's, so they
// are exact seconds.

/// When to first say we are still looking: 15s. Silence and failure looked
/// identical before this, which cost an evening of forensics.
const FIRST_SEARCH_NOTICE_TICKS: u32 = detached_ticks(15);

/// How often to repeat it: every 5 minutes. Frequent enough to be visible in a
/// log, rare enough not to bury anything during a session with no game open.
const REPEAT_SEARCH_NOTICE_TICKS: u32 = detached_ticks(300);

/// How long to wait after the game closes before looking again: 5s.
const PROCESS_GONE_DELAY_TICKS: u32 = detached_ticks(5);

/// How often to retry resolving the wonder completion service, in ticks (~1s).
const WONDER_RESOLVE_TICKS: u32 = 120;

/// How often to retry resolving GameInitializer, in ticks (~2s). Each attempt
/// costs a scan.
///
/// This is the recovery path only, for an initializer that failed validation
/// while being watched. It is far too slow to catch a run start: that has to be
/// bound during the scene load, because the window between the load ending and
/// the overlay is about one scan wide. See `RunStart::resolve_during_load`.
const RUN_START_RESOLVE_TICKS: u32 = 240;

/// How often to forget which candidates were ruled out: 10s. Pids are reused,
/// and a process rejected once may since have mapped the game.
const FORGET_RULED_OUT_TICKS: u32 = detached_ticks(10);

/// How often to re-examine ambiguous candidates: 1s. Checking one means
/// attaching to it, which the runtime logs, so doing it too often turns a dying
/// game into a stream of attach/detach churn. At the detached rate that is
/// every tick, and the rate itself is the throttle.
const AMBIGUOUS_RETRY_TICKS: u32 = detached_ticks(1);

/// Ticks to wait before rescanning after a scan comes up empty. Without this
/// the retry is a hot loop; the object we are waiting for appears on a human
/// timescale anyway.
const RESCAN_DELAY_TICKS: u32 = 120;

/// How many conclusive empty scans it takes to give up skipping the instance
/// just left.
///
/// Skipping it is right when the outgoing game's clock is still alive. It is
/// wrong when the clock was found during the load and belongs to the game
/// coming in -- and then the skip locks out the only instance there is, which
/// was observed as a permanent spin of "No DayNightCycle" with a fully loaded
/// game on screen.
const GIVE_UP_SKIPPING_AFTER: u32 = 3;


async fn main() {
    asr::print_message("Timberborn auto splitter.");
    // A message from a previous session survives a module reload, so start
    // from a blank one rather than showing a stale warning about a game that
    // is no longer running.
    status::clear();
    let mut settings = Settings::register();

    loop {
        // `attach` drops the tick rate while it looks; back up to full for the
        // splits, which are polled every tick.
        let process = attach().await;
        asr::set_tick_rate(ATTACHED_TICK_RATE);
        settings.update();
        process.until_closes(run(&process, &mut settings)).await;
        // Back down before the wait below, which is counted in detached ticks.
        asr::set_tick_rate(DETACHED_TICKS_PER_SEC as f64);
        // A process on its way out stays attachable for several seconds, and
        // each re-attach is logged, so wait longer here than between rescans.
        for _ in 0..PROCESS_GONE_DELAY_TICKS {
            next_tick().await;
        }
    }
}

async fn attach() -> Process {
    // Nothing here is timing-critical, and this is where the module spends
    // every hour LiveSplit is open without the game. The host re-reads the rate
    // each step, so this takes effect from the next tick and is undone by the
    // caller on a successful attach.
    asr::set_tick_rate(DETACHED_TICKS_PER_SEC as f64);

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
async fn run(process: &Process, settings: &mut Settings) {
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
        status::warn("Game version not supported: DayNumber missing");
        return;
    };

    // Which memory ranges hold managed objects. Empty until the heap is mapped,
    // and every scan is a full sweep until then.
    let mut hot = scan::HotRanges::default();
    let mut heap_mapped = false;

    let mut scene = loop {
        if let Some(scene) = SceneLoad::resolve(process, &module, &mut hot).await {
            break scene;
        }
        for _ in 0..RESCAN_DELAY_TICKS {
            next_tick().await;
        }
    };

    let mut probed = false;
    // The DI container of the game just left, skipped for the same reason its
    // objects are: it stays alive, and it holds a clock that is not this
    // game's.
    let mut previous_registry: Option<Address> = None;
    // The initializer of the game just left, skipped for the same reason the
    // container is: freed, its memory reads as a plausible pre-overlay
    // state and binding to it loses the run start entirely.
    let mut previous_run_start: Option<Address> = None;
    let mut empty_scans = 0;
    loop {
        // Nothing found during a load belongs to the world that comes out of
        // it, so wait the load out first. The parameters the loader holds are
        // sampled throughout, since on the rising edge they are still the
        // previous load's.
        let mut run_start = None;
        while scene.is_loading(process).unwrap_or(false) {
            // We are here for this one, whatever it turns out to be.
            scene.observed = true;
            let pending = scene.pending(process, &module);
            if pending != Scene::Unknown {
                scene.loaded = pending;
            }
            // Bind the run start here rather than after the load: once the
            // load has finished, the settlement-name dialog is the only thing
            // left before the overlay, and that window is about one heap scan
            // wide. Retried until it takes, since the initializer may not
            // exist yet on the first passes.
            if run_start.is_none() {
                if let Scene::Game { new_game } = scene.loaded {
                    run_start = RunStart::resolve_during_load(
                        process,
                        &module,
                        event_bus_vtable,
                        Some(new_game),
                        // Inside this loop, so by definition watched.
                        true,
                        previous_run_start,
                        &hot,
                    )
                    .await;
                }
            }
            next_tick().await;
        }

        // Run start is the timing-critical watcher, and the settlement-name
        // dialog is the entire window for binding it, so it is resolved before
        // anything else. Queued behind the clock scan it routinely arrived
        // after the overlay had been shown, and the start was simply missed.
        if let Scene::Game { new_game } = scene.loaded {
            // Binding happens inside the loop above, where the loader may still
            // be holding the *previous* load's parameters -- so what the
            // watcher was told about this game can be a load out of date. Now
            // that the load has finished the answer is final, so correct it.
            // Bound during a save load and then told "new game", it announced
            // a run start was not one.
            if let Some(start) = run_start.as_mut() {
                start.expect_new_game(new_game);
            }
            // A fresh game is a fresh slate: a warning about the last one must
            // not sit on screen through this one.
            status::clear();
            asr::print_message(&format!(
                "A game scene loaded ({}), run start {}.",
                if new_game { "new game" } else { "loaded save" },
                if run_start.is_some() {
                    "bound during the load"
                } else {
                    "not bound during the load"
                },
            ));
        }
        // Only reached when binding during the load did not happen: the load
        // was already over when it was watched, or no load was watched at all.
        let mut run_start = match (run_start, scene.loaded) {
            (Some(run_start), _) => Some(run_start),
            (None, Scene::Game { new_game }) => {
                RunStart::resolve(
                    process,
                    &module,
                    event_bus_vtable,
                    Some(new_game),
                    scene.observed,
                    previous_run_start,
                    &hot,
                )
                .await
            }
            (None, Scene::MainMenu | Scene::MapEditor) => None,
            // Attached with no load watched yet, so fall back to the save name
            // static and to requiring the pre-overlay states to be seen.
            (None, Scene::Unknown) => {
                RunStart::resolve(
                    process,
                    &module,
                    event_bus_vtable,
                    None,
                    scene.observed,
                    previous_run_start,
                    &hot,
                )
                .await
            }
        };

        // Not in a game, so there is nothing to watch. The containers still
        // lying around belong to games already left, and looking for one here
        // only finds a dead one -- which is what "no longer skipping it" then
        // latched onto, binding the previous game's services on the menu.
        if matches!(scene.loaded, Scene::MainMenu | Scene::MapEditor) {
            // The one safe moment to map the heap: no run is in progress, so a
            // scan that takes a while costs nothing but time. It could not be
            // done any earlier anyway -- Mono fills a class's vtable in lazily,
            // so ComponentCache has none until a game has built one.
            if !heap_mapped {
                if let Some(vtable) = service::class_vtable(
                    process,
                    &module,
                    "Timberborn.BaseComponentSystem",
                    "ComponentCache",
                ) {
                    // A load starting underneath this has to win: the map is
                    // seconds long, and blocking through a load would miss the
                    // very run start it exists to protect.
                    let (mut mapped, complete) =
                        service::map_heap(process, vtable, "ComponentCache", || {
                            !scene.is_loading(process).unwrap_or(false)
                        })
                        .await;
                    if !mapped.is_empty() {
                        // Keep anything already learned: the persistent objects
                        // found before a game was ever loaded live in a range
                        // the map may not cover.
                        mapped.remember(hot.ranges());
                        hot = mapped;
                    }
                    // Only a complete map retires the job. A partial one is
                    // kept and improved on the next visit to the menu.
                    heap_mapped = complete;
                }
            }
            for _ in 0..RESCAN_DELAY_TICKS {
                next_tick().await;
            }
            continue;
        }

        // The one scan a loaded game costs. Everything the splitter watches is
        // a singleton in this container, so finding it once replaces a scan
        // apiece for the clock, the wonder unlock, the run end and the
        // entities.
        //
        // The container of the game just left stays alive and holds a clock of
        // its own -- measured, still at the same address after exiting to the
        // menu -- so it is skipped, exactly as the loose clock instance used to
        // be.
        //
        // The run start is polled through the scan. Bound during the load but
        // first read only afterwards, it saw `Finished` every time: the whole
        // ShowUI transition happens inside one scan.
        let found = singletons::Registry::resolve(
            process,
            &module,
            previous_registry,
            clock.vtable(),
            &mut hot,
            || {
                if let Some(start) = run_start.as_mut() {
                    if start.poll(process) {
                        start_timer(settings);
                    }
                }
            },
        )
        .await;
        let Some(mut registry) = found.registry else {
            asr::print_message(if found.conclusive {
                "No singleton container with a clock in it -- no game loaded. Waiting."
            } else {
                "No singleton container found, but the scan was incomplete, so \
                 this is not conclusive. Waiting."
            });
            // Repeatedly finding nothing else means the skipped container is
            // the only one there is, so it cannot be a game we left: it is this
            // game's, found early. Keeping the skip would refuse to bind
            // anything for the rest of the session. Only a conclusive scan
            // counts: an incomplete one says nothing.
            if found.conclusive && previous_registry.is_some() {
                empty_scans += 1;
                if empty_scans >= GIVE_UP_SKIPPING_AFTER {
                    empty_scans = 0;
                    previous_registry = None;
                    asr::print_message(
                        "Nothing found but the container just left, so it \
                         belongs to this game after all. No longer skipping it.",
                    );
                }
            }
            for _ in 0..RESCAN_DELAY_TICKS {
                next_tick().await;
            }
            continue;
        };
        empty_scans = 0;
        previous_registry = Some(registry.address());

        let Some(instance) = registry.lookup(clock.vtable()) else {
            asr::print_message("The container lost its clock between scans. Waiting.");
            for _ in 0..RESCAN_DELAY_TICKS {
                next_tick().await;
            }
            continue;
        };
        asr::print_message(&format!("Found {} at {instance}.", clock.name()));

        if !probed {
            if !probe::run(process, &module) {
                status::warn("Game version may not be supported -- see the log");
            }
            probed = true;
        }

        // Remembered before it is handed over, so the next game does not bind
        // to this one's freed initializer.
        if let Some(start) = &run_start {
            previous_run_start = Some(start.instance);
        }

        watch(
            process,
            &module,
            &clock,
            &mut registry,
            instance,
            day_number,
            event_bus_vtable,
            &mut scene,
            run_start,
            &mut hot,
            &mut heap_mapped,
            settings,
        )
        .await;

        if !scene.still_valid(process) {
            asr::print_message("The scene loader is gone. Re-resolving it.");
            scene = loop {
                if let Some(scene) = SceneLoad::resolve(process, &module, &mut hot).await {
                    break scene;
                }
                for _ in 0..RESCAN_DELAY_TICKS {
                    next_tick().await;
                }
            };
        }
        asr::print_message("A scene is loading. Everything will be resolved again.");
    }
}

/// Whether a scene is being loaded.
///
/// `SceneLoader` loads every scene including the main menu, and outlives all of
/// them: measured across two new games, the load-game menu and deleting saves,
/// it stayed at one address throughout. That makes it the one thing that can be
/// asked "did the world just change?" rather than inferred from whether some
/// object still reads.
///
/// Inference was the previous approach and it does not work. A freed object
/// keeps returning values until its memory is reused, and then returns whatever
/// now owns it -- observed as a garbage `initializationState` followed by a
/// plausible `0`, which is indistinguishable from a game starting to load.
struct SceneLoad {
    class: service::Locatable,
    instance: Address,
    offset: u32,
    loading: bool,
    /// What the last load produced, or `Unknown` before one has been watched.
    loaded: Scene,
    /// Whether `loaded` came from watching a load happen, rather than from
    /// reading the loader's parameters on attach. Both give the same answer
    /// about *what* loaded; only the first means we were there for it.
    observed: bool,
}

/// Which scene a load is producing, and for a game, how it was started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scene {
    Game { new_game: bool },
    MainMenu,
    MapEditor,
    Unknown,
}

impl SceneLoad {
    async fn resolve(
        process: &Process,
        module: &Module,
        hot: &mut scan::HotRanges,
    ) -> Option<Self> {
        // Not a DI service, so no _eventBus: validated through the asset loader
        // it holds, the same way DistrictBuildingRegistry was.
        let asset_loader =
            service::class_vtable(process, module, "Timberborn.AssetSystem", "AssetLoader")?;
        let class = service::Locatable::with_validator(
            process,
            module,
            "Timberborn.SceneLoading",
            "SceneLoader",
            "_assetLoader",
            asset_loader,
        )?;
        let offset = class.field(process, module, "_isLoading")?;
        let found = class.find_one(process, hot).await;
        hot.remember(&found.ranges);
        let instance = found.one()?;
        let scene = Self {
            class,
            instance,
            offset,
            loading: false,
            loaded: Scene::Unknown,
            observed: false,
        };
        // The loader is persistent, so its parameters still describe the last
        // load even though we were not watching when it happened. Without this
        // an attach starts from Unknown, and Unknown has to assume a game may
        // be loaded -- which at the main menu is wrong, because the previous
        // game's objects are still alive and readable there. The splitter
        // announced "Game already in progress" while sitting on the menu.
        let scene = Self {
            loaded: scene.pending(process, module),
            ..scene
        };
        asr::print_message(&format!(
            "Watching scene loads at {instance}. Last load: {:?}.",
            scene.loaded
        ));
        Some(scene)
    }

    /// One byte per tick. `None` if the read failed, which happens while a
    /// scene is being torn down and means nothing on its own.
    /// Every observation is remembered, so whoever asks next sees an edge
    /// against what was last actually true. Reading it somewhere that did not
    /// record it left the flag stuck across a whole load, and the next load's
    /// edge was then computed against a stale `true` and missed entirely.
    fn is_loading(&mut self, process: &Process) -> Option<bool> {
        let loading = process
            .read::<u8>(self.instance.add(self.offset as u64))
            .ok()
            .map(|byte| byte != 0)?;
        self.loading = loading;
        Some(loading)
    }

    /// What is being loaded, read off the parameters object the loader holds.
    ///
    /// The parameters' class says which scene, and a game scene's parameters
    /// say whether it is a new game or a save -- which beats the process-wide
    /// `WorldDataService.SourceFileName` static, since these belong to this
    /// load and cannot hold a value left over from an earlier one.
    ///
    /// Sampled repeatedly while a load runs: on the tick the load starts, the
    /// field still holds the *previous* load's parameters.
    fn pending(&self, process: &Process, module: &Module) -> Scene {
        let Some(params) = self
            .class
            .field(process, module, "_sceneParameters")
            .and_then(|offset| read_pointer(process, self.instance, offset))
        else {
            return Scene::Unknown;
        };
        let Some(vtable) = read_pointer_raw(process, params) else {
            return Scene::Unknown;
        };
        let is = |image, class| {
            service::class_vtable(process, module, image, class) == Some(vtable)
        };
        if is("Timberborn.MainMenuSceneLoading", "MainMenuSceneParameters") {
            return Scene::MainMenu;
        }
        if is("Timberborn.MapEditorSceneLoading", "MapEditorSceneParameters") {
            return Scene::MapEditor;
        }
        if !is("Timberborn.GameSceneLoading", "GameSceneParameters") {
            return Scene::Unknown;
        }
        let set = |field| {
            service::field_of(process, module, params, field)
                .and_then(|offset| read_pointer(process, params, offset))
                .is_some()
        };
        Scene::Game {
            new_game: set("<NewGameConfiguration>k__BackingField")
                && !set("<SaveReference>k__BackingField"),
        }
    }

    /// Whether a load has started since the last observation.
    fn started_loading(&mut self, process: &Process) -> bool {
        let was_loading = self.loading;
        matches!(self.is_loading(process), Some(true)) && !was_loading
    }

    fn still_valid(&self, process: &Process) -> bool {
        self.class.still_valid(process, self.instance)
    }
}

/// The timer side of a run start.
///
/// Needed from two places: the watch loop, and the clock scan that runs before
/// it -- during which the overlay can come and go.
fn start_timer(settings: &Settings) {
    if !settings.start {
        return;
    }
    // Only from a stopped timer: never restart a run in progress.
    if timer::state() == TimerState::NotRunning {
        asr::print_message("Run start: overlay shown. Starting the timer.");
        timer::start();
    } else {
        asr::print_message("Run start seen, but the timer is already running.");
    }
}

/// Watches the loaded game until the scene changes.
#[allow(clippy::too_many_arguments)] // Everything the loop watches, passed in.
async fn watch(
    process: &Process,
    module: &Module,
    clock: &service::Locatable,
    registry: &mut singletons::Registry,
    instance: Address,
    day_number: u32,
    event_bus_vtable: Address,
    scene: &mut SceneLoad,
    mut run_start: Option<RunStart>,
    hot: &mut scan::HotRanges,
    heap_mapped: &mut bool,
    settings: &mut Settings,
) {
    let mut ticks = 0u32;
    let mut last_day = None;
    let mut completion: Option<WonderCompletion> = None;
    let mut unlock: Option<WonderUnlock> = None;
    let mut buildings: Option<Buildings> = None;
    let mut explained_buildings = false;
    let mut ended = false;
    // Whether a run start was observed while watching this scene. Anything
    // resolved afterwards belongs to a run already under way, so "already done
    // when we arrived" is not a reason to stay quiet.
    // Already true when the start fired during the clock scan that got here:
    // every watcher below is then resolved into a run already under way.
    let mut run_began = run_start.as_ref().is_some_and(|start| start.fired);

    // Resolved before the loop where the scene said a game was coming. Still
    // retried here, because on a fresh load GameInitializer exists before its
    // dependencies are injected and an early attempt fails validation.
    // A GameInitializer that was being watched and then failed validation.
    let mut lost_run_start: Option<Address> = None;

    let expect_new_game = match scene.loaded {
        Scene::Game { new_game } => Some(new_game),
        _ => None,
    };

    // Nothing to start a run in, so do not go looking for a GameInitializer.
    // The previous game's is still alive and readable on the menu, and binding
    // to it reads Finished and reports a game already in progress -- which the
    // runner saw on screen while sitting on the main menu.
    let want_run_start = !matches!(scene.loaded, Scene::MainMenu | Scene::MapEditor);
    let watched_load = scene.observed;

    // Resolved once: the probe the heap map is built from. Only present after a
    // game has been loaded, since Mono fills a class's vtable in lazily.
    let cache_vtable = service::class_vtable(
        process,
        module,
        "Timberborn.BaseComponentSystem",
        "ComponentCache",
    );
    let mut map: Option<scan::Scan> = None;

    loop {
        // The authoritative end of this world. Everything held here belongs to
        // the scene being replaced, whether or not it still reads.
        if scene.started_loading(process) {
            return;
        }

        // Map the heap a slice at a time while there is time to spare. This
        // loop is the only place that reliably has any: exiting to the main
        // menu does not always register as a scene load, so the menu branch
        // above it cannot be counted on to run at all.
        //
        // A slice per tick rather than awaiting the whole map, so every split
        // check below keeps running -- awaiting it would stop them for the best
        // part of a minute. Abandoning it costs nothing: the `return` above
        // drops the partial scan when a load starts, which is exactly when the
        // splitter has better things to do.
        if !*heap_mapped {
            if map.is_none() {
                if let Some(vtable) = cache_vtable {
                    map = Some(scan::Scan::new(process, vtable).mapping());
                }
            }
            if map.as_mut().is_some_and(|scan| {
                scan.step(process, scan::MAP_BUDGET)
            }) {
                if let Some(scan) = map.take() {
                    hot.remember(&scan.found_ranges);
                    *heap_mapped = true;
                    asr::print_message(&format!(
                        "[scan] heap mapped: {} ranges hold managed objects, {} MiB of {} MiB.                          Later scans read only those.",
                        hot.len(),
                        hot.bytes() >> 20,
                        scan.stats.bytes_total >> 20,
                    ));
                }
            }
        }

        // Before the state is read, not after: a freed GameInitializer keeps
        // returning values from whatever now owns the memory, and one of those
        // could look like a game starting.
        if run_start.as_ref().is_some_and(|r| !r.still_valid(process)) {
            asr::print_message(
                "The GameInitializer being watched is gone. Re-resolving run start.",
            );
            // Skipped from here on: its memory is free to be reused as
            // something that reads like a game starting.
            lost_run_start = run_start.as_ref().map(|r| r.instance);
            run_start = None;
        }

        if want_run_start && run_start.is_none() && ticks.is_multiple_of(RUN_START_RESOLVE_TICKS) {
            run_start = RunStart::resolve(
                process,
                module,
                event_bus_vtable,
                expect_new_game,
                watched_load,
                lost_run_start,
                hot,
            )
            .await;
        }
        settings.update();

        if let Some(start) = &mut run_start {
            if start.poll(process) {
                // A run is beginning, so nothing the other watchers hold
                // belongs to it: they were bound to whatever came before.
                // Re-resolving from here also means anything they then find
                // already done really was done during this run.
                run_began = true;
                unlock = None;
                completion = None;
                buildings = None;

                // Whatever went wrong before, it was about a previous game.
                status::clear();
                start_timer(settings);
            }
        }

        if let Ok(day) = process.read::<i32>(instance.add(day_number as u64)) {
            if last_day != Some(day) {
                asr::print_message(&format!("DayNumber = {day}"));
                last_day = Some(day);
            }
        }

        // Nothing may sample game state until initialization is done. The save
        // restorer sets CountdownFinished partway through a load, so resolving
        // earlier captures a stale "false" baseline and the restore then looks
        // like the run ending.
        let initialized = run_start.as_ref().is_some_and(|s| s.initialized());

        // Looking anything up in a container that is no longer a container
        // would read whatever now owns the memory. The scene loader, not this,
        // is what says the game has ended -- this only says the lookups cannot
        // be trusted meanwhile.
        if initialized
            && ticks.is_multiple_of(WONDER_RESOLVE_TICKS)
            && registry.still_valid(process)
        {
            // The snapshot is only as new as the last time it was taken, and
            // these are retried precisely because the services do not all
            // exist at once. Registering one replaces the container's array,
            // so nothing short of re-reading it will show the new arrival.
            if completion.is_none() || unlock.is_none() || buildings.is_none() {
                registry.refresh(process).await;
            }
            if completion.is_none() {
                completion =
                    WonderCompletion::resolve(process, module, event_bus_vtable, registry);
                if run_began {
                    if let Some(c) = &mut completion {
                        c.arrived_mid_run();
                    }
                }
            }
            if unlock.is_none() {
                unlock = WonderUnlock::resolve(process, module, event_bus_vtable, registry);
                if run_began {
                    if let Some(u) = &mut unlock {
                        u.arrived_mid_run();
                    }
                }
            }
            if buildings.is_none() {
                buildings =
                    Buildings::resolve(process, module, registry, !explained_buildings).await;
                explained_buildings = true;
                if run_began {
                    if let Some(b) = &mut buildings {
                        b.arrived_mid_run();
                    }
                }
            }
        }

        // Each watcher owns a different object with its own lifetime, so the
        // clock still validating does not mean these do. Reading through a
        // torn-down object produced a denormal float in one log, and the same
        // read on CountdownFinished could fire a spurious run end.
        if unlock.as_ref().is_some_and(|u| !u.still_valid(process)) {
            unlock = None;
        }
        if completion.as_ref().is_some_and(|c| !c.still_valid(process)) {
            completion = None;
        }
        if buildings.as_ref().is_some_and(|b| !b.still_valid(process)) {
            buildings = None;
        }

        if let Some(b) = &mut buildings {
            for label in b.poll(process) {
                let enabled = match label {
                    "Forester" => settings.forester,
                    "Gear Workshop" => settings.gear_workshop,
                    "Tapper's Shack" => settings.tappers_shack,
                    "Observatory / Numbercruncher" => settings.advanced_science,
                    _ => settings.smelter_woodworkshop,
                };
                if enabled && timer::state() == TimerState::Running {
                    asr::print_message(&format!("Split: {label} finished."));
                    timer::split();
                } else {
                    asr::print_message(&format!("{label} finished, but not splitting."));
                }
            }
        }

        if let Some(u) = &mut unlock {
            if u.poll(process, module) {
                if settings.unlock_wonder && timer::state() == TimerState::Running {
                    asr::print_message("Split: wonder unlocked.");
                    timer::split();
                } else {
                    asr::print_message("Wonder unlocked, but not splitting.");
                }
            }
        }

        // The actual run end: the Congratulations screen. Read every tick.
        if let Some(c) = &mut completion {
            c.report_length(process, module, clock, instance);
            c.report_activation(process);
            if !ended && c.finished(process) {
                ended = true;
                // A wrong template name shows up as the unlock split never
                // firing, which is otherwise silent. Reaching the end of a run
                // without it is the symptom, so say so.
                if unlock.as_ref().is_some_and(|u| !u.ever_matched()) {
                    asr::print_message(
                        "WARNING: the run ended but the wonder was never seen \
                         unlocked. The template name for this faction is \
                         probably wrong.",
                    );
                }
                if settings.congratulations_screen && timer::state() == TimerState::Running {
                    asr::print_message("Run end: Congratulations screen. Splitting.");
                    timer::split();
                } else {
                    asr::print_message(
                        "Congratulations screen reached, but the timer is not \
                         running so nothing was split.",
                    );
                }
            }
        }

        ticks = ticks.wrapping_add(1);
        next_tick().await;
    }
}

/// A building split: the first time any of `templates` is finished.
struct BuildingSplit {
    label: &'static str,
    templates: &'static [&'static str],
}

/// The split set and order. Each entry lists both factions' template names,
/// which are the same building under different names where the factions
/// differ.
const BUILDING_SPLITS: &[BuildingSplit] = &[
    BuildingSplit {
        label: "Forester",
        templates: &["Forester.Folktails", "Forester.IronTeeth"],
    },
    BuildingSplit {
        label: "Gear Workshop",
        templates: &["GearWorkshop.Folktails", "GearWorkshop.IronTeeth"],
    },
    BuildingSplit {
        label: "Tapper's Shack",
        templates: &["TappersShack.Folktails", "TappersShack.IronTeeth"],
    },
    // The two factions' advanced science buildings share one split: they are
    // separate categories, so only one of these can be built in a given run.
    BuildingSplit {
        label: "Observatory / Numbercruncher",
        templates: &["Observatory.Folktails", "Numbercruncher.IronTeeth"],
    },
];

/// The Smelter + Wood Workshop split needs both, in either order.
const SMELTER: &[&str] = &["Smelter.Folktails", "Smelter.IronTeeth"];
const WOOD_WORKSHOP: &[&str] = &["WoodWorkshop.Folktails", "WoodWorkshop.IronTeeth"];

/// Where a `T[]`'s elements start, past the object header, bounds and length.
/// Part of the Mono ABI rather than of any managed type, so there is no named
/// field to resolve it from.
const ENTRY_DATA: u64 = 0x20;

/// Finished buildings, discovered through the global entity registry.
///
/// Every entity in the game is in `EntityRegistry._entitiesInInstantiationOrder`,
/// which is reached from `EntityService._entityRegistry` -- so no scan of its
/// own, and no dependence on districts. An entity's `_componentCache` gives its
/// template name (`_name`), and the `BlockObjectState` among its components
/// gives whether it is finished.
///
/// Buildings exist as entities from the moment they are placed, so a tracked
/// one is found while it is still unfinished and then watched. The per-tick
/// cost is one read for the entity count plus one per watched building -- a
/// handful -- and the split fires on the tick `_state` becomes `Finished`.
///
/// This replaces reading the districts' finished-building registries, which
/// turned out not to see every building: on an Iron Teeth endgame save with two
/// districts, four built Numbercrunchers appeared in no district registry at
/// all. See `docs/DESIGN.md`.
struct Buildings {
    /// `_entitiesInInstantiationOrder`, with the offsets of its `_size` and
    /// `_items` so neither the class nor the backing array has to be resolved
    /// again. `_items` is re-read each time, since a growing list reallocates.
    entities: Address,
    size_offset: u32,
    items_offset: u32,
    /// `BaseComponent._componentCache`, then `ComponentCache._name` and
    /// `ComponentCache._components`.
    cache_offset: u32,
    name_offset: u32,
    components_offset: u32,
    /// How a `BlockObjectState` is recognised among an entity's components,
    /// and where its `_state` sits.
    block_state_vtable: Address,
    state_offset: u32,

    /// Tracked buildings found so far: the address of the building's
    /// `BlockObjectState`, and which split slot it belongs to. Small -- only
    /// tracked templates go in here.
    watched: Vec<(Address, usize)>,
    /// How much of the list has been inspected, as a count of leading entries.
    ///
    /// Entities are appended, so ordinarily only the tail is new. A removal
    /// shifts everything after it down by one, which can slide an uninspected
    /// entity below this mark -- so a shrink of `k` rewinds it by `k`, and the
    /// re-inspected window costs churn, not the length of the list. Inspecting
    /// the same entity twice is harmless: watches are keyed by address.
    inspected: i32,
    /// Entity count as of the last poll, to spot growth and shrinkage.
    last_count: i32,

    finished: Vec<bool>,
    on_arrival: Vec<bool>,
    /// The combined split fires once, on whichever of the two comes second.
    combined_fired: bool,
}

impl Buildings {
    /// Four single-building splits, plus smelter and wood workshop tracked
    /// separately for the combined one.
    const TRACKED: usize = 6;
    const SMELTER_INDEX: usize = 4;
    const WOOD_WORKSHOP_INDEX: usize = 5;

    /// `BlockObjectState.State`: `Unfinished`, `Finished`, `Preview`.
    const FINISHED: i32 = 1;

    /// Longest component list looked at. Entities have a handful of
    /// components; anything near this is a misread rather than a real list.
    const MAX_COMPONENTS: i32 = 256;

    /// Entities inspected per tick while catching up. Each costs about five
    /// reads, and a read through Wine is ~30us, so this stays around a
    /// millisecond in the worst case.
    const CHUNK: i32 = 32;

    /// Entities inspected per tick during the opening baseline walk. That one
    /// runs while the game is still loading, so it can afford more.
    const BASELINE_CHUNK: i32 = 256;

    /// Guards against walking a pathological registry.
    const MAX_ENTITIES: i32 = 500_000;

    /// `explain` makes the first attempt say why it failed. Later attempts stay
    /// quiet, since "not constructed yet" is normal during a load.
    async fn resolve(
        process: &Process,
        module: &Module,
        container: &singletons::Registry,
        explain: bool,
    ) -> Option<Self> {
        macro_rules! need {
            ($value:expr, $what:literal) => {
                match $value {
                    Some(value) => value,
                    None => {
                        if explain {
                            asr::print_message(concat!(
                                "buildings: cannot resolve yet -- ",
                                $what
                            ));
                        }
                        return None;
                    }
                }
            };
        }

        // Every entity is a component, so the cache offset comes from
        // BaseComponent rather than from any particular component type.
        let cache_offset = need!(
            service::field_offset(
                process,
                module,
                "Timberborn.BaseComponentSystem",
                "BaseComponent",
                "_componentCache",
            ),
            "BaseComponent._componentCache"
        );
        let name_offset = need!(
            service::field_offset(
                process,
                module,
                "Timberborn.BaseComponentSystem",
                "ComponentCache",
                "_name",
            ),
            "ComponentCache._name"
        );
        let components_offset = need!(
            service::field_offset(
                process,
                module,
                "Timberborn.BaseComponentSystem",
                "ComponentCache",
                "_components",
            ),
            "ComponentCache._components"
        );
        let state_offset = need!(
            service::field_offset(
                process,
                module,
                "Timberborn.BlockSystem",
                "BlockObjectState",
                "_state",
            ),
            "BlockObjectState._state"
        );
        // A vtable, unlike a field offset, only exists once the class has been
        // constructed -- so this one is a "not yet", not a rename.
        let block_state_vtable = need!(
            service::class_vtable(process, module, "Timberborn.BlockSystem", "BlockObjectState"),
            "BlockObjectState not constructed yet"
        );

        // EntityRegistry has no _eventBus, so it cannot be located directly,
        // and EntityService -- which holds it -- turns out not to be a
        // singleton and so is not in the container either. GameOverChecker is,
        // and holds the same registry, so it is one hop instead of a scan.
        //
        // Nothing else about the game-over feature is used or wanted here; it
        // is simply a singleton with a reference to the registry. If a game
        // update removes it, this fails by name like everything else and the
        // probe says so.
        let checker_vtable = need!(
            service::class_vtable(process, module, "Timberborn.GameOver", "GameOverChecker"),
            "GameOverChecker not constructed yet"
        );
        let checker = need!(
            container.lookup(checker_vtable),
            "GameOverChecker is not in the container yet"
        );
        let registry_field = need!(
            service::field_of(process, module, checker, "_entityRegistry"),
            "GameOverChecker._entityRegistry"
        );
        let entity_registry = need!(
            read_pointer(process, checker, registry_field),
            "GameOverChecker._entityRegistry is null"
        );
        let order_field = need!(
            service::field_of(
                process,
                module,
                entity_registry,
                "_entitiesInInstantiationOrder"
            ),
            "EntityRegistry._entitiesInInstantiationOrder"
        );
        let entities = need!(
            read_pointer(process, entity_registry, order_field),
            "the entity list is null"
        );
        let (size_offset, items_offset) = need!(
            collections::List::offsets(process, module, entities),
            "the entity list is not a List"
        );

        let mut buildings = Self {
            entities,
            size_offset,
            items_offset,
            cache_offset,
            name_offset,
            components_offset,
            block_state_vtable,
            state_offset,
            watched: Vec::new(),
            inspected: 0,
            last_count: 0,
            finished: alloc::vec![false; Self::TRACKED],
            on_arrival: alloc::vec![false; Self::TRACKED],
            combined_fired: false,
        };

        // The opening walk covers every entity in the save -- tens of thousands
        // on a long-running one -- so it yields between chunks rather than
        // stalling the runtime for a second or more.
        let total = buildings.count(process);
        asr::print_message(&format!(
            "[entities] opening walk starting over {total} entities."
        ));
        let mut index = 0;
        while index < total {
            let end = (index + Self::BASELINE_CHUNK).min(total);
            buildings.inspect_range(process, index, end);
            index = end;
            next_tick().await;
        }
        buildings.inspected = total;
        buildings.last_count = total;

        // Anything already finished when we arrived is not a split.
        buildings.poll_watched(process);
        buildings.on_arrival = buildings.finished.clone();

        asr::print_message(&format!(
            "Watching {} entities for {} tracked buildings. Already finished in \
             this save: {}.",
            total,
            buildings.watched.len(),
            buildings.describe_finished(),
        ));
        Some(buildings)
    }

    /// The entity count. One read, and the whole per-tick trigger.
    fn count(&self, process: &Process) -> i32 {
        process
            .read::<i32>(self.entities.add(self.size_offset as u64))
            .ok()
            .filter(|n| (0..Self::MAX_ENTITIES).contains(n))
            .unwrap_or(0)
    }

    /// Looks at entities `[from, to)` and remembers the tracked ones.
    ///
    /// The backing array is read once per call rather than per entity, and
    /// nothing here resolves a class: every offset was settled at resolve time.
    fn inspect_range(&mut self, process: &Process, from: i32, to: i32) {
        let Some(items) = read_pointer(process, self.entities, self.items_offset) else {
            return;
        };
        for i in from.max(0)..to {
            let Some(entity) = read_pointer_raw(process, items.add(ENTRY_DATA + i as u64 * 8))
            else {
                continue;
            };
            let Some(cache) = read_pointer(process, entity, self.cache_offset) else {
                continue;
            };
            let Some(name) = read_pointer(process, cache, self.name_offset) else {
                continue;
            };
            let Some(slot) = Self::slot_for(process, name) else {
                continue;
            };
            let Some(state) = self.block_state_of(process, cache) else {
                continue;
            };
            if !self.watched.iter().any(|&(address, _)| address == state) {
                self.watched.push((state, slot));
            }
        }
    }

    /// Which split slot an entity's template name belongs to, if any.
    fn slot_for(process: &Process, name: Address) -> Option<usize> {
        Self::all_templates().position(|templates| {
            templates
                .iter()
                .any(|t| collections::string_starts_with_segment(process, name, t))
        })
    }

    /// The entity's `BlockObjectState`, if it has one.
    ///
    /// An entity's components are a plain list, and the one we want is
    /// identified by its vtable -- there is no named field to reach it by.
    fn block_state_of(&self, process: &Process, cache: Address) -> Option<Address> {
        let components = read_pointer(process, cache, self.components_offset)?;
        // A List<T> of references has the same layout whatever T is, so the
        // offsets taken from the entity list serve here too.
        let size = process
            .read::<i32>(components.add(self.size_offset as u64))
            .ok()?;
        let items = read_pointer(process, components, self.items_offset)?;
        (0..size.min(Self::MAX_COMPONENTS)).find_map(|i| {
            let component = read_pointer_raw(process, items.add(ENTRY_DATA + i as u64 * 8))?;
            let vtable = read_pointer_raw(process, component)?;
            (vtable == self.block_state_vtable).then_some(component)
        })
    }

    /// Reads every watched building's state. Returns the slots that just
    /// reached `Finished`.
    ///
    /// One read per watched building, and a second only when one of them says
    /// it is finished: a demolished building leaves an address that the
    /// allocator may hand to something else, and the value there could then be
    /// anything. Confirming the vtable before firing costs nothing in the case
    /// that happens every tick, and makes a stale address unable to split.
    fn poll_watched(&mut self, process: &Process) -> Vec<usize> {
        let mut newly = Vec::new();
        for &(state, slot) in &self.watched {
            if self.finished[slot] {
                continue;
            }
            let finished = process
                .read::<i32>(state.add(self.state_offset as u64))
                .is_ok_and(|value| value == Self::FINISHED);
            if !finished {
                continue;
            }
            if read_pointer_raw(process, state) != Some(self.block_state_vtable) {
                continue;
            }
            self.finished[slot] = true;
            newly.push(slot);
        }
        newly
    }

    fn all_templates() -> impl Iterator<Item = &'static [&'static str]> {
        BUILDING_SPLITS
            .iter()
            .map(|s| s.templates)
            .chain([SMELTER, WOOD_WORKSHOP])
    }

    fn describe_finished(&self) -> alloc::string::String {
        let mut names = Vec::new();
        for (index, split) in BUILDING_SPLITS.iter().enumerate() {
            if self.finished[index] {
                names.push(split.label);
            }
        }
        if self.finished[Self::SMELTER_INDEX] {
            names.push("Smelter");
        }
        if self.finished[Self::WOOD_WORKSHOP_INDEX] {
            names.push("Wood Workshop");
        }
        if names.is_empty() {
            alloc::string::String::from("none")
        } else {
            names.join(", ")
        }
    }

    fn still_valid(&self, process: &Process) -> bool {
        process
            .read::<i32>(self.entities.add(self.size_offset as u64))
            .is_ok()
    }

    /// Bound part way through a run, so anything standing now was built during
    /// it and its split is still owed.
    fn arrived_mid_run(&mut self) {
        if self.finished.iter().any(|&done| done) {
            asr::print_message(&format!(
                "Bound after the run started, so treating these as built during \
                 it rather than already there: {}.",
                self.describe_finished()
            ));
        }
        self.on_arrival = alloc::vec![false; Self::TRACKED];
        self.finished = alloc::vec![false; Self::TRACKED];
    }

    /// Returns the labels that just completed.
    fn poll(&mut self, process: &Process) -> Vec<&'static str> {
        let count = self.count(process);

        // A removal slid part of the tail down past the mark, so give back as
        // much ground as was lost. Re-inspecting is cheap and idempotent.
        if count < self.last_count {
            self.inspected = (self.inspected - (self.last_count - count)).max(0);
        }
        self.last_count = count;

        // Catch up on anything not yet inspected, a chunk at a time so a large
        // backlog is spread over ticks instead of stalling one.
        if self.inspected < count {
            let end = (self.inspected + Self::CHUNK).min(count);
            self.inspect_range(process, self.inspected, end);
            self.inspected = end;
        } else {
            self.inspected = self.inspected.min(count);
        }

        let mut fired = Vec::new();
        for slot in self.poll_watched(process) {
            if self.on_arrival[slot] {
                continue;
            }
            if let Some(split) = BUILDING_SPLITS.get(slot) {
                fired.push(split.label);
            }
        }

        // The combined split fires on the later of the two, in either order.
        let both = self.finished[Self::SMELTER_INDEX] && self.finished[Self::WOOD_WORKSHOP_INDEX];
        let both_on_arrival =
            self.on_arrival[Self::SMELTER_INDEX] && self.on_arrival[Self::WOOD_WORKSHOP_INDEX];
        if both && !both_on_arrival && !self.combined_fired {
            self.combined_fired = true;
            fired.push("Smelter + Wood Workshop");
        }
        fired
    }
}

fn read_pointer(process: &Process, base: Address, offset: u32) -> Option<Address> {
    read_pointer_raw(process, base.add(offset as u64))
}

fn read_pointer_raw(process: &Process, at: Address) -> Option<Address> {
    process
        .read::<u64>(at)
        .ok()
        .map(Address::new)
        .filter(|a| !a.is_null())
}

/// The unlock split: the player spending science on the wonder.
///
/// Timberborn has no research that runs over time. Science is produced by
/// buildings and banked, and a building is unlocked by clicking it in the
/// science tree, which succeeds instantly if enough is banked. So this is an
/// event, not a progress bar, and it is the click that the split fires on.
///
/// `BuildingUnlockingService._unlockedBuildings` is a `HashSet<string>` of
/// template names, so this is a membership test rather than an object walk.
///
/// Both factions are covered. The ASL script this replaces only recognised the
/// Folktails wonder.
struct WonderUnlock {
    class: service::Locatable,
    instance: Address,
    pub set: Address,
    count_offset: u32,
    last_count: Option<i32>,
    /// Whether the wonder was already unlocked when we arrived. A loaded save
    /// may have it unlocked already, and that must not read as a fresh unlock.
    unlocked_on_arrival: bool,
    fired: bool,
}

impl WonderUnlock {
    /// Whether the wonder was ever seen in the unlocked set, however it got
    /// there. False at the end of a run means the template name is wrong.
    fn ever_matched(&self) -> bool {
        self.fired || self.unlocked_on_arrival
    }

    /// Bound part way through a run rather than on arriving at a loaded game,
    /// so whatever is unlocked now was unlocked during it.
    fn arrived_mid_run(&mut self) {
        if self.unlocked_on_arrival {
            asr::print_message(
                "The wonder reads as unlocked, but this watcher was bound after \
                 the run started, so that belongs to the game before it. \
                 Watching for the unlock anyway.",
            );
        }
        self.unlocked_on_arrival = false;
    }

    fn still_valid(&self, process: &Process) -> bool {
        self.class.still_valid(process, self.instance)
    }
}

/// Wonder template names, by faction. Both verified against a completed run --
/// the Iron Teeth wonder is the Earth Repopulator, not the similarly grand
/// sounding Tribute to Ingenuity, which is a monument.
const WONDER_TEMPLATES: &[&str] = &["EarthRecultivator.Folktails", "EarthRepopulator.IronTeeth"];

impl WonderUnlock {
    fn resolve(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
        registry: &singletons::Registry,
    ) -> Option<Self> {
        let class = service::Locatable::new(
            process,
            module,
            "Timberborn.ScienceSystem",
            "BuildingUnlockingService",
            event_bus_vtable,
        )?;
        let field = class.field(process, module, "_unlockedBuildings")?;
        let instance = registry.lookup(class.vtable())?;

        let set = process
            .read_pointer(instance.add(field as u64), module.get_pointer_size())
            .ok()
            .filter(|a| !a.is_null())?;
        let count_offset = collections::count_offset(process, module, set)?;

        let unlocked_on_arrival = Self::wonder_unlocked(process, module, set);
        asr::print_message(&format!(
            "Watching building unlocks at {set}. Wonder already unlocked in \
             this save: {unlocked_on_arrival}."
        ));

        Some(Self {
            class,
            instance,
            set,
            count_offset,
            last_count: None,
            unlocked_on_arrival,
            fired: false,
        })
    }

    fn wonder_unlocked(process: &Process, module: &Module, set: Address) -> bool {
        let Some(set) = collections::HashSet::read(process, module, set) else {
            return false;
        };
        WONDER_TEMPLATES
            .iter()
            .any(|name| set.contains_str(process, name))
    }

    /// One read per tick. The set is only walked when its count changes, which
    /// happens a handful of times a run rather than 120 times a second.
    fn poll(&mut self, process: &Process, module: &Module) -> bool {
        if self.fired || self.unlocked_on_arrival {
            return false;
        }
        let Ok(count) = process.read::<i32>(self.set.add(self.count_offset as u64)) else {
            return false;
        };
        if self.last_count == Some(count) {
            return false;
        }
        self.last_count = Some(count);

        if Self::wonder_unlocked(process, module, self.set) {
            self.fired = true;
            return true;
        }
        false
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
    /// `_unlockDay` as first seen. It is set when the wonder is activated, so a
    /// change means activation -- no scanning required.
    unlock_day_on_arrival: Option<f32>,
    reported_activation: bool,
    /// Whether the real-time length of the countdown has been reported yet.
    /// The clock reads zero during load, so this is retried until it is sane.
    reported_length: bool,
    /// The value when we first saw it. A save that already completed its wonder
    /// loads with this true, and that must not read as the run ending.
    was_finished_on_arrival: bool,
}

impl WonderCompletion {
    /// Bound part way through a run. A completion that reads as already done
    /// belongs to the game before this one, so do not let it suppress the end.
    fn arrived_mid_run(&mut self) {
        if self.was_finished_on_arrival {
            asr::print_message(
                "The countdown reads as already finished, but this watcher was \
                 bound after the run started, so that belongs to the game before \
                 it. Watching for the run end anyway.",
            );
        }
        self.was_finished_on_arrival = false;
    }

    fn still_valid(&self, process: &Process) -> bool {
        self.class.still_valid(process, self.instance)
    }
}

impl WonderCompletion {
    fn resolve(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
        registry: &singletons::Registry,
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
        let instance = registry.lookup(class.vtable())?;

        let already = process
            .read::<u8>(instance.add(countdown_finished as u64))
            .is_ok_and(|done| done != 0);
        asr::print_message(&format!(
            "Watching wonder completion at {instance} (run end per the rules). \
             Already finished in this save: {already}."
        ));

        let unlock_day_on_arrival = process
            .read::<f32>(instance.add(unlock_day as u64))
            .ok();

        Some(Self {
            class,
            countdown_finished,
            unlock_day,
            instance,
            unlock_day_on_arrival,
            reported_activation: false,
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

    /// Notes when the wonder is activated, which is not the run end but is
    /// worth seeing in the log: the run ends one countdown later.
    ///
    /// Activation sets `_unlockDay`, so watching that costs two reads. The
    /// alternative -- scanning for `Wonder` instances and reading `IsActive` --
    /// meant a full heap scan every two seconds for the length of a run.
    fn report_activation(&mut self, process: &Process) {
        if self.reported_activation {
            return;
        }
        let Some(day) = self.unlock_day(process) else {
            return;
        };
        if Some(day) != self.unlock_day_on_arrival {
            asr::print_message(&format!(
                "Wonder activated (not the run end). Completion due on day {day}."
            ));
            self.reported_activation = true;
        }
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
    /// Kept so the instance can be re-validated. Without this the watcher
    /// cannot tell that its object has been freed: the address still reads,
    /// and returns whatever now occupies the memory.
    class: service::Locatable,
    pub instance: Address,
    pub offset: u32,
    /// Static `WorldDataService.SourceFileName`: the save being loaded, and
    /// null on a new game. `initializationState` alone cannot tell the two
    /// apart -- loading a save walks the same Waiting -> ShowUI sequence.
    source_file_name: Option<Address>,
    /// Whether a state before `ShowUI` has been seen. Attaching to a game
    /// already in progress must not count as a start. Not needed when the
    /// scene load already said a new game was starting.
    seen_before_ui: bool,
    /// What the scene load said, if one was watched.
    new_game: Option<bool>,
    /// Whether the load into this game was actually watched. Losing the race on
    /// a load we watched is a malfunction; arriving at a game that was already
    /// up is not, and the two need to be told apart in a bug report even though
    /// the runner does the same thing about both.
    watched_load: bool,
    warned_late: bool,
    fired: bool,
    last: Option<i32>,
}

/// `InitializationState.Finished`.
const FINISHED: i32 = 5;

/// `InitializationState.ShowUI`.
const SHOW_UI: i32 = 4;

impl RunStart {
    /// Whether the object being watched is still a `GameInitializer`.
    ///
    /// A freed one gets its memory reused, and the state field then reads as
    /// whatever the new occupant holds -- observed as a garbage number and
    /// then a plausible-looking `0`, which is indistinguishable from a game
    /// beginning to load.
    fn still_valid(&self, process: &Process) -> bool {
        self.class.still_valid(process, self.instance)
    }

    /// `new_game` is what the scene load said, when one was watched:
    /// `Some(true)` for a new game, `Some(false)` for a save, `None` when no
    /// load has been seen and the save-name static has to answer instead.
    async fn resolve(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
        new_game: Option<bool>,
        watched_load: bool,
        skip: Option<Address>,
        hot: &scan::HotRanges,
    ) -> Option<Self> {
        Self::bind(
            process,
            module,
            event_bus_vtable,
            new_game,
            watched_load,
            skip,
            hot,
            false,
        )
        .await
    }

    /// Binds while the scene load is still running, taking only an initializer
    /// that has not reached the overlay yet.
    ///
    /// Binding after the load finished was measured as always too late: the
    /// gap between the load ending and `ShowUI` is about as wide as one heap
    /// scan, and the first read said `Finished` every time. The incoming
    /// game's initializer already exists during the load, in a pre-overlay
    /// state, while the outgoing game's reads `Finished`.
    ///
    /// A pre-overlay state is not enough on its own. The initializer of the
    /// game just left is freed, and its reused memory reads as a plausible
    /// `0` -- `Waiting` -- which is the failure this whole approach exists to
    /// avoid, and it was observed binding to the previous game's address on a
    /// second run. `skip` is that address, and skipping it is what makes the
    /// state test safe.
    async fn resolve_during_load(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
        new_game: Option<bool>,
        watched_load: bool,
        skip: Option<Address>,
        hot: &scan::HotRanges,
    ) -> Option<Self> {
        Self::bind(
            process,
            module,
            event_bus_vtable,
            new_game,
            watched_load,
            skip,
            hot,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // Everything a bind has to weigh up.
    async fn bind(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
        new_game: Option<bool>,
        watched_load: bool,
        skip: Option<Address>,
        hot: &scan::HotRanges,
        require_pre_ui: bool,
    ) -> Option<Self> {
        let class = service::Locatable::new(
            process,
            module,
            "Timberborn.GameStartup",
            "GameInitializer",
            event_bus_vtable,
        )?;
        let offset = class.field(process, module, "_initializationState")?;
        let instance = class
            .find_matching(process, 4, hot, |address| {
                if Some(address) == skip {
                    return false;
                }
                !require_pre_ui
                    || process
                        .read::<i32>(address.add(offset as u64))
                        .is_ok_and(|state| state < SHOW_UI)
            })
            .await
            .one()?;

        // Static, so no instance and no validator are needed. WorldDataService
        // is not a DI service and has no _eventBus, so Locatable cannot be
        // built for it -- which is what broke the first attempt at this.
        let source_file_name = service::static_field(
            process,
            module,
            "Timberborn.ErrorReporting",
            "WorldDataService",
            "SourceFileName",
        );
        if source_file_name.is_none() {
            status::warn("Cannot tell a new game from a loaded save");
        }

        asr::print_message(&format!("Watching run start at {instance}."));
        Some(Self {
            class,
            instance,
            offset,
            source_file_name,
            seen_before_ui: false,
            fired: false,
            last: None,
            new_game,
            watched_load,
            warned_late: false,
        })
    }

    /// Corrects what this watcher was told about the game it is bound to.
    ///
    /// Binding happens during the load, when the scene loader can still be
    /// holding the previous load's parameters, so the answer given at bind time
    /// may be a load out of date. It is final once the load has finished.
    /// Ignored once the watcher has already decided, so a late correction
    /// cannot fire a start twice or retract one.
    fn expect_new_game(&mut self, new_game: bool) {
        if !self.fired && !self.warned_late {
            self.new_game = Some(new_game);
        }
    }

    /// Whether the game has finished loading. Anything that samples saved state
    /// must wait for this.
    fn initialized(&self) -> bool {
        self.last.is_some_and(|state| state >= FINISHED)
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
            return false;
        }

        if self.fired {
            return false;
        }

        if self.new_game == Some(false) {
            if !self.warned_late {
                self.warned_late = true;
                asr::print_message(
                    "Overlay shown, but this game was loaded from a save. \
                     Not a run start.",
                );
            }
            return false;
        }

        // The test is not "the state is ShowUI". That state can be over in
        // less than a tick -- observed going 1 -> 3 -> 5 with neither 2 nor 4
        // ever sampled, on an instance watched from Waiting -- so waiting to
        // see it discards starts that were tracked perfectly. What matters is
        // that a pre-overlay state was seen on *this* instance and the state
        // is now past it: the crossing then happened since the last poll,
        // which is one tick ago at worst.
        //
        // Never having seen one means the watcher arrived after the overlay,
        // and how late is unknowable, so the timer is not started.
        //
        // This covers two ways of getting here and warns about both, because
        // the consequence is the same: this game will not be timed. Either a
        // watched load said a new game was starting and binding lost the race,
        // or there was no load to watch and the game was already up when the
        // splitter attached. The second is not a malfunction, but the runner
        // still needs to know the timer is not going to start by itself.
        if !self.seen_before_ui {
            if !self.warned_late {
                self.warned_late = true;
                // Deliberately two different strings for what is, to the
                // runner, the same situation: start the timer yourself. They
                // differ so that a screenshot in a bug report says which of
                // the two happened, which the log would otherwise be the only
                // way to tell.
                let (log, message) = if self.watched_load {
                    (
                        "WARNING: a new game was loading, but this watcher was \
                         bound after the overlay had already appeared. Not \
                         starting the timer, since the start time would be wrong.",
                        "Run start missed",
                    )
                } else {
                    (
                        "WARNING: the overlay was already shown when this \
                         watcher bound, and the load into this game was not \
                         watched, so it was already up. Not starting the timer.",
                        "Game already in progress",
                    )
                };
                asr::print_message(log);
                // A timer that is already running makes this moot: the start
                // was not missed, this watcher just arrived after it. Saying
                // otherwise put "Run start missed" on screen during a run that
                // had started perfectly well.
                if timer::state() == TimerState::NotRunning {
                    status::warn(message);
                }
            }
            return false;
        }

        self.fired = true;
        if self.new_game == Some(true) {
            return true;
        }
        // No load was watched, so the save-name static has to say whether this
        // is a new game.
        match self.loaded_from_save(process) {
            Some(name_len) => {
                asr::print_message(&format!(
                    "Overlay shown, but this is a loaded save \
                     (SourceFileName is {name_len} chars). Not a run start."
                ));
                false
            }
            None => true,
        }
    }

    /// `Some(length)` when a save file name is set, i.e. this is a load rather
    /// than a new game.
    fn loaded_from_save(&self, process: &Process) -> Option<i32> {
        service::string_len(process, self.source_file_name?).filter(|&len| len > 0)
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
