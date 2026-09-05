//! The game scene's DI container, and finding services inside it.
//!
//! Every service the splitter watches is a singleton built by Timberborn's DI
//! container, and the container keeps all of them in one array. Reaching that
//! array costs a heap scan; every service in it is then a lookup rather than a
//! scan of its own. That is the whole point of this module: a game used to cost
//! five scans -- clock, run start, wonder unlock, run end, entities -- and now
//! costs two.
//!
//! The path is `SingletonRepository._singletonListener` ->
//! `SingletonListener._allSingletons`, an `ImmutableArray<object>`. A field of
//! that type is a struct wrapping one `object[]`, so the field slot holds the
//! array reference directly.
//!
//! Measured against the live game, one loaded save:
//!
//! | repository | singletons | holds |
//! |---|---|---|
//! | one | 20 | none of ours |
//! | another | 103 | none of ours -- the main menu's |
//! | another | 612 | every service the splitter watches |
//!
//! So there is more than one container alive at a time and they have to be told
//! apart, which is what `required` in [`Registry::resolve`] is for.
//!
//! # What this is not
//!
//! It is **not** an "is a game loaded" signal. The game's container is still
//! alive at the same address after exiting to the main menu, holding the same
//! `DayNightCycle` -- measured, not assumed. It lingers exactly like the loose
//! objects do. Knowing whether a game is loaded remains the scene loader's job;
//! see *Lifecycle* in `docs/DESIGN.md`.
//!
//! # Why the contents are cached, and re-read a chunk at a time
//!
//! A container held 612 singletons, and identifying one means reading each
//! entry's vtable -- one round trip through Wine apiece. Doing that per lookup,
//! inside a single tick, is hundreds of blocking reads against the wineserver
//! the game itself depends on, and the game visibly stops responding. So the
//! contents are read once into [`Registry::entries`], a chunk per tick, and
//! every lookup after that is free.
//!
//! `_allSingletons` is an `ImmutableArray`, so registering another singleton
//! replaces the array rather than growing it: the snapshot can go stale, and
//! services are resolved on a retry interval precisely because they do not all
//! exist at once. [`Registry::refresh`] retakes it, and the retry path calls
//! that before giving up on anything.

use alloc::{format, vec, vec::Vec};

use asr::{future::next_tick, game_engine::unity::mono::Module, Address, Process};

use crate::{collections, service, table::ReferenceTable};

/// Refuse to walk an array longer than this. The game's container held 612;
/// anything on a different scale means the field was misread rather than that
/// the game grew.
const MAX_SINGLETONS: i64 = 8192;

/// How many containers a scan will collect.
///
/// This is a safety valve, not a working limit. Every abandoned game leaves a
/// container behind until it is collected, so they accumulate over a session,
/// and a freshly built one is not necessarily among the first found: the scan
/// runs in address order and new allocations are not always at the end. A tight
/// limit truncated the search, the new game's container fell off it, and the
/// splitter bound the previous game's services instead -- which read as a game
/// with the wonder already unlocked.
const MAX_CONTAINERS: usize = 256;

/// The outcome of looking for the container.
pub struct Search {
    pub registry: Option<Registry>,
    /// Ranges holding a pointer to the anchor, when the sweep was asked to
    /// notice one in passing. The caller identifies the table from these; see
    /// [`crate::table::ReferenceTable::identify`].
    pub table_candidates: Vec<(Address, u64)>,
    /// Whether an empty result actually means "not there". A scan proves
    /// absence only if it read everything it set out to read, and memory goes
    /// transiently unreadable during a scene teardown -- which is exactly when
    /// this runs.
    pub conclusive: bool,
}

impl Search {
    /// Nothing found, and nothing proven: a name did not resolve, so no scan
    /// happened at all.
    fn inconclusive() -> Self {
        Self {
            registry: None,
            table_candidates: Vec::new(),
            conclusive: false,
        }
    }
}

/// How many entries to read before yielding. The reads go through the same
/// wineserver the game does, so a whole container in one tick stalls the game.
const CHUNK: usize = 96;

/// One game scene's DI container.
pub struct Registry {
    /// Kept so the container can be re-validated, the same way every other
    /// held instance is.
    class: service::Locatable,
    repository: Address,
    listener_offset: u32,
    all_singletons_offset: u32,
    /// Every singleton it has built, as (vtable, instance).
    entries: Vec<(Address, Address)>,
}

impl Registry {
    /// Finds the container that holds `required`, ignoring `skip`.
    ///
    /// `required` is the vtable of a class only the game scene's container has
    /// -- the clock -- which is what separates it from the menu's container and
    /// from the bootstrap one. `skip` is the container of the game just left,
    /// which stays alive and holds a clock of its own, so it would otherwise
    /// match first.
    pub async fn resolve(
        process: &Process,
        module: &Module,
        skip: Option<Address>,
        required: Address,
        table: Option<&ReferenceTable>,
        // Something already located, for the sweep to notice references to on
        // its way past. Only worth passing when there is no table: it costs a
        // few percent of the sweep and saves a whole one.
        anchor: Option<Address>,
        mut on_tick: impl FnMut(),
    ) -> Search {
        // Not a DI service itself, so no _eventBus: validated through the
        // listener it holds, the same way SceneLoader is through its loader.
        let Some(listener_vtable) = service::class_vtable(
            process,
            module,
            "Timberborn.SingletonSystem",
            "SingletonListener",
        ) else {
            return Search::inconclusive();
        };
        let Some(class) = service::Locatable::with_validator(
            process,
            module,
            "Timberborn.SingletonSystem",
            "SingletonRepository",
            "_singletonListener",
            listener_vtable,
        ) else {
            return Search::inconclusive();
        };
        let Some(listener_offset) = class.field(process, module, "_singletonListener") else {
            return Search::inconclusive();
        };
        let Some(all_singletons_offset) = service::field_offset(
            process,
            module,
            "Timberborn.SingletonSystem",
            "SingletonListener",
            "_allSingletons",
        ) else {
            return Search::inconclusive();
        };

        // The reference table first. Whether a container is the right one takes
        // an async walk of its contents, so the two passes are driven from here
        // rather than inside the search: a table search that turns up
        // candidates none of which hold the clock is not an answer, and has to
        // fall through to the sweep.
        let mut why = alloc::borrow::Cow::Borrowed("no reference table found yet");
        if let Some(found) = class
            .from_table(process, Some(MAX_CONTAINERS), table, &mut on_tick)
            .await
        {
            why = alloc::borrow::Cow::Owned(if found.instances.is_empty() {
                "the reference table has no container in it".into()
            } else {
                format!(
                    "the reference table held {} container(s), none of them this game's",
                    found.instances.len()
                )
            });
            let picked = Self::pick(
                process,
                &class,
                &found,
                skip,
                required,
                listener_offset,
                all_singletons_offset,
            )
            .await;
            if picked.is_some() {
                if let Some(table) = table {
                    table.answered();
                }
                return Search {
                    registry: picked,
                    table_candidates: Vec::new(),
                    conclusive: found.conclusive,
                };
            }
        }

        if table.is_some() && why.starts_with("no reference") {
            why = alloc::borrow::Cow::Borrowed("the reference table could not be read");
        }
        let found = class
            .sweep(process, Some(MAX_CONTAINERS), &why, anchor, &mut on_tick)
            .await;
        let conclusive = found.conclusive;
        let registry = Self::pick(
            process,
            &class,
            &found,
            skip,
            required,
            listener_offset,
            all_singletons_offset,
        )
        .await;
        // The sweep answered what the table could not, which is what a table
        // that has stopped being the runtime's looks like from here.
        if let (Some(table), true) = (table, registry.is_some()) {
            table.was_missing();
        }
        Search {
            registry,
            table_candidates: found.table_candidates,
            conclusive,
        }
    }

    /// The first candidate that is not the one just left and does hold the
    /// required class. Costs a [`refresh`](Self::refresh) apiece, which is why
    /// it is not folded into the scan itself.
    async fn pick(
        process: &Process,
        class: &service::Locatable,
        found: &service::Found,
        skip: Option<Address>,
        required: Address,
        listener_offset: u32,
        all_singletons_offset: u32,
    ) -> Option<Self> {
        for &repository in &found.instances {
            if Some(repository) == skip {
                continue;
            }
            let mut registry = Self {
                class: class.clone(),
                repository,
                listener_offset,
                all_singletons_offset,
                entries: Vec::new(),
            };
            registry.refresh(process).await;
            if registry.lookup(required).is_some() {
                asr::print_message(&format!(
                    "The game's singleton container is at {repository}."
                ));
                return Some(registry);
            }
        }
        None
    }

    /// The instance of the class with this vtable, if the snapshot has one.
    ///
    /// Free: no reads at all. [`refresh`](Self::refresh) is what costs.
    pub fn lookup(&self, vtable: Address) -> Option<Address> {
        self.entries
            .iter()
            .find(|(found, _)| *found == vtable)
            .map(|(_, instance)| *instance)
    }

    /// Re-reads the container's contents, a chunk per tick.
    ///
    /// One bulk read gets every reference; the vtable that identifies each one
    /// then costs a read apiece, and those are what have to be spread out --
    /// several hundred blocking round trips in a single tick is enough to stop
    /// the game responding.
    pub async fn refresh(&mut self, process: &Process) {
        let Some(references) = self.references(process) else {
            return;
        };
        let mut entries = Vec::with_capacity(references.len());
        for chunk in references.chunks(CHUNK) {
            for &object in chunk {
                if let Some(vtable) = read_pointer_raw(process, object) {
                    entries.push((vtable, object));
                }
            }
            next_tick().await;
        }
        self.entries = entries;
    }

    /// Every non-null reference the container holds, in one read.
    fn references(&self, process: &Process) -> Option<Vec<Address>> {
        let listener = read_pointer(process, self.repository, self.listener_offset)?;
        let array = read_pointer(process, listener, self.all_singletons_offset)?;
        let length = process
            .read::<i64>(array.add(collections::ARRAY_LENGTH))
            .ok()?;
        if !(0..=MAX_SINGLETONS).contains(&length) {
            return None;
        }
        let mut references = vec![0u64; length as usize];
        process
            .read_into_buf(
                array.add(collections::ARRAY_DATA),
                bytemuck::cast_slice_mut(&mut references),
            )
            .ok()?;
        Some(
            references
                .into_iter()
                .map(Address::new)
                .filter(|object| !object.is_null())
                .collect(),
        )
    }

    /// Whether the container is still the object we think it is.
    pub fn still_valid(&self, process: &Process) -> bool {
        self.class.still_valid(process, self.repository)
    }

    /// The container's address, to be skipped when the next game is resolved.
    pub fn address(&self) -> Address {
        self.repository
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
        .filter(|address| !address.is_null())
}
