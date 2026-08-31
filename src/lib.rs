#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod collections;
mod probe;
mod scan;
mod service;

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

    /// Split when the faction's wonder research is completed
    #[default = true]
    research_wonder: bool,

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

// The "~Ns" figures on the constants below are at 120 ticks/s, which is what
// asr asks the host for and what asr-debugger delivers. LiveSplit measures at
// ~107/s (9.4ms) with no game attached, and ~89/s (11.3ms) while watching a
// loaded save: its update timer is stopped for the duration of each step and
// restarted afterwards, so the period is the interval plus our own per-tick
// work, and that work scales with the settlement. Every duration below is
// therefore a lower bound, ~12% to ~35% longer in LiveSplit than the figure in
// its comment. None of them are timing-critical -- they are retry and log
// intervals, and the splits themselves are polled every tick.

/// When to first say we are still looking, in ticks: ~15s at 120/s, ~17s in
/// LiveSplit. Silence and failure looked identical before this, which cost an
/// evening of forensics.
const FIRST_SEARCH_NOTICE_TICKS: u32 = 1800;

/// How often to repeat it: every 5 minutes. Frequent enough to be visible in a
/// log, rare enough not to bury anything during a session with no game open.
const REPEAT_SEARCH_NOTICE_TICKS: u32 = 36_000;

/// How long to wait after the game closes before looking again, in ticks (~5s).
const PROCESS_GONE_DELAY_TICKS: u32 = 600;

/// How often to retry resolving the wonder completion service, in ticks (~1s).
const WONDER_RESOLVE_TICKS: u32 = 120;

/// How often to retry resolving GameInitializer, in ticks (~2s). Each attempt
/// costs a scan, and the settlement-name dialog is up for far longer than this,
/// so there is ample time to resolve before the state can move off Waiting.
const RUN_START_RESOLVE_TICKS: u32 = 240;

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
    asr::print_message("Timberborn auto splitter.");
    let mut settings = Settings::register();

    loop {
        let process = attach().await;
        settings.update();
        process.until_closes(run(&process, &mut settings)).await;
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

        watch(
            process,
            &module,
            &clock,
            instance,
            day_number,
            event_bus_vtable,
            settings,
        )
        .await;
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
    settings: &mut Settings,
) {
    let mut ticks = 0u32;
    let mut last_day = None;
    let mut completion: Option<WonderCompletion> = None;
    let mut research: Option<Research> = None;
    let mut buildings: Option<Buildings> = None;
    let mut explained_buildings = false;
    let mut ended = false;

    // Resolved lazily and retried: on a fresh load GameInitializer exists
    // before its dependencies are injected, so the first attempt finds an
    // object that fails validation and comes back empty. Resolving once and
    // giving up meant run start was never watched for that whole session.
    let mut run_start: Option<RunStart> = None;

    loop {
        if !clock.still_valid(process, instance) {
            return;
        }

        if run_start.is_none() && ticks.is_multiple_of(RUN_START_RESOLVE_TICKS) {
            run_start = RunStart::resolve(process, module, event_bus_vtable).await;
        }
        settings.update();

        if let Some(start) = &mut run_start {
            if start.poll(process) && settings.start {
                // Only from a stopped timer: never restart a run in progress.
                if timer::state() == TimerState::NotRunning {
                    asr::print_message("Run start: overlay shown. Starting the timer.");
                    timer::start();
                } else {
                    asr::print_message("Run start seen, but the timer is already running.");
                }
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

        if initialized && ticks.is_multiple_of(WONDER_RESOLVE_TICKS) {
            if completion.is_none() {
                completion = WonderCompletion::resolve(process, module, event_bus_vtable).await;
            }
            if research.is_none() {
                research = Research::resolve(process, module, event_bus_vtable).await;
            }
            if buildings.is_none() {
                buildings =
                    Buildings::resolve(process, module, event_bus_vtable, !explained_buildings)
                        .await;
                explained_buildings = true;
            }
        }

        // Each watcher owns a different object with its own lifetime, so the
        // clock still validating does not mean these do. Reading through a
        // torn-down object produced a denormal float in one log, and the same
        // read on CountdownFinished could fire a spurious run end.
        if research.as_ref().is_some_and(|r| !r.still_valid(process)) {
            research = None;
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

        if let Some(r) = &mut research {
            if r.poll(process, module) {
                if settings.research_wonder && timer::state() == TimerState::Running {
                    asr::print_message("Split: wonder research completed.");
                    timer::split();
                } else {
                    asr::print_message("Wonder research completed, but not splitting.");
                }
            }
        }

        // The actual run end: the Congratulations screen. Read every tick.
        if let Some(c) = &mut completion {
            c.report_length(process, module, clock, instance);
            c.report_activation(process);
            if !ended && c.finished(process) {
                ended = true;
                // A wrong template name shows up as the research split never
                // firing, which is otherwise silent. Reaching the end of a run
                // without it is the symptom, so say so.
                if research.as_ref().is_some_and(|r| !r.ever_matched()) {
                    asr::print_message(
                        "WARNING: the run ended but the wonder was never seen as \
                         researched. The template name for this faction is \
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
        event_bus_vtable: Address,
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

        // EntityRegistry has no _eventBus and so cannot be located directly.
        // EntityService is an ordinary DI service and holds it, which is the
        // same trick DistrictBuildingRegistry needed.
        let entity_service = need!(
            service::Locatable::new(
                process,
                module,
                "Timberborn.EntitySystem",
                "EntityService",
                event_bus_vtable,
            ),
            "EntityService"
        );
        let registry_field = need!(
            entity_service.field(process, module, "_entityRegistry"),
            "EntityService._entityRegistry"
        );
        let instance = need!(
            entity_service.find_one(process).await.first,
            "EntityService instance"
        );
        let registry = need!(
            read_pointer(process, instance, registry_field),
            "EntityService._entityRegistry is null"
        );
        let order_field = need!(
            service::field_of(process, module, registry, "_entitiesInInstantiationOrder"),
            "EntityRegistry._entitiesInInstantiationOrder"
        );
        let entities = need!(
            read_pointer(process, registry, order_field),
            "the entity list is null"
        );
        let size_offset = need!(
            collections::List::size_offset(process, module, entities),
            "the entity list is not a List"
        );
        let items_offset = need!(
            service::field_of(process, module, entities, "_items"),
            "the entity list has no _items"
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

/// The research split: the faction's wonder research completing, which is the
/// moment the wonder becomes buildable.
///
/// `BuildingUnlockingService._unlockedBuildings` is a `HashSet<string>` of
/// template names, so this is a membership test rather than an object walk.
///
/// Both factions are covered. The ASL script this replaces only recognised the
/// Folktails wonder.
struct Research {
    class: service::Locatable,
    instance: Address,
    set: Address,
    count_offset: u32,
    last_count: Option<i32>,
    /// Whether the wonder was already unlocked when we arrived. A loaded save
    /// has its research done, and that must not read as researching it now.
    unlocked_on_arrival: bool,
    fired: bool,
}

impl Research {
    /// Whether the wonder was ever seen in the unlocked set, however it got
    /// there. False at the end of a run means the template name is wrong.
    fn ever_matched(&self) -> bool {
        self.fired || self.unlocked_on_arrival
    }

    fn still_valid(&self, process: &Process) -> bool {
        self.class.still_valid(process, self.instance)
    }
}

/// Wonder template names, by faction. Both verified against a completed run --
/// the Iron Teeth wonder is the Earth Repopulator, not the similarly grand
/// sounding Tribute to Ingenuity, which is a monument.
const WONDER_TEMPLATES: &[&str] = &["EarthRecultivator.Folktails", "EarthRepopulator.IronTeeth"];

impl Research {
    async fn resolve(
        process: &Process,
        module: &Module,
        event_bus_vtable: Address,
    ) -> Option<Self> {
        let class = service::Locatable::new(
            process,
            module,
            "Timberborn.ScienceSystem",
            "BuildingUnlockingService",
            event_bus_vtable,
        )?;
        let field = class.field(process, module, "_unlockedBuildings")?;
        let instance = class.find_one(process).await.first?;

        let set = process
            .read_pointer(instance.add(field as u64), module.get_pointer_size())
            .ok()
            .filter(|a| !a.is_null())?;
        let count_offset = collections::count_offset(process, module, set)?;

        let unlocked_on_arrival = Self::wonder_unlocked(process, module, set);
        asr::print_message(&format!(
            "Watching research at {set}. Wonder already unlocked in this save: \
             {unlocked_on_arrival}."
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
    fn still_valid(&self, process: &Process) -> bool {
        self.class.still_valid(process, self.instance)
    }
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
    instance: Address,
    offset: u32,
    /// Static `WorldDataService.SourceFileName`: the save being loaded, and
    /// null on a new game. `initializationState` alone cannot tell the two
    /// apart -- loading a save walks the same Waiting -> ShowUI sequence.
    source_file_name: Option<Address>,
    /// Whether a state before `ShowUI` has been seen. Attaching to a game
    /// already in progress must not count as a start.
    seen_before_ui: bool,
    fired: bool,
    last: Option<i32>,
}

/// `InitializationState.Finished`.
const FINISHED: i32 = 5;

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
            asr::print_message(
                "WARNING: cannot read SourceFileName, so a loaded save cannot be \
                 told from a new game. Run start would fire on both.",
            );
        }

        asr::print_message(&format!("Watching run start at {instance}."));
        Some(Self {
            instance,
            offset,
            source_file_name,
            seen_before_ui: false,
            fired: false,
            last: None,
        })
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
        } else if self.seen_before_ui && !self.fired {
            self.fired = true;
            return match self.loaded_from_save(process) {
                Some(name_len) => {
                    asr::print_message(&format!(
                        "Overlay shown, but this is a loaded save \
                         (SourceFileName is {name_len} chars). Not a run start."
                    ));
                    false
                }
                None => true,
            };
        }
        false
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
