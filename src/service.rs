//! Locating game services and reading fields off them.
//!
//! Wraps the raw scan in [`crate::scan`] with the two things every split needs:
//! resolving a class by name, and validating that a match is a real instance.
//!
//! Nearly every Timberborn service holds an `_eventBus` pointing at the single
//! `EventBus`, which makes one shared validation target work for all of them.

extern crate alloc;

use asr::{
    game_engine::unity::mono::{Class, Image, Module},
    Address, Process,
};

use alloc::vec::Vec;

use crate::{
    scan::{self, Validator},
    table::ReferenceTable,
};

/// What a search turned up.
pub struct Found {
    /// Instances that passed validation.
    pub instances: Vec<Address>,
    /// Whether an empty result actually means "not present". Never true for a
    /// search through the reference table, which can only say "not among the
    /// objects the runtime is holding a reference to".
    pub conclusive: bool,
    /// Ranges holding a pointer to the anchor a sweep was asked to notice in
    /// passing, for identifying the reference table off the back of a sweep
    /// that was happening anyway. Empty unless one was asked for.
    pub table_candidates: Vec<(Address, u64)>,
}

impl Found {
    /// The first instance, for the singleton case.
    pub fn one(&self) -> Option<Address> {
        self.instances.first().copied()
    }
}

/// The field almost every Timberborn service has, and the class it points at.
/// Not universal: `ComponentCache` and other non-services have no `_eventBus`.
const EVENT_BUS_FIELD: &str = "_eventBus";
const EVENT_BUS_CLASS: (&str, &str) = ("Timberborn.SingletonSystem", "EventBus");

/// Resolves a class's vtable, for use as a validation target.
///
/// Returns `None` until the class has been constructed at least once, since
/// Mono fills in a vtable lazily.
pub fn class_vtable(
    process: &Process,
    module: &Module,
    image_name: &str,
    class_name: &str,
) -> Option<Address> {
    let image: Image = module.get_image(process, image_name)?;
    image
        .get_class(process, module, class_name)?
        .get_vtable(process, module)
}

/// Resolves the shared validation target used by most services.
pub fn event_bus_vtable(process: &Process, module: &Module) -> Option<Address> {
    let (image_name, class_name) = EVENT_BUS_CLASS;
    class_vtable(process, module, image_name, class_name)
}

/// Address of a static field, without needing an instance.
///
/// [`Locatable`] deliberately requires an `_eventBus` so it can validate the
/// instances it finds, but that is a requirement of *finding objects*, not of
/// reading statics. Classes like `WorldDataService` are not DI services and
/// have no `_eventBus`, yet their statics are perfectly readable.
pub fn static_field(
    process: &Process,
    module: &Module,
    image_name: &str,
    class_name: &str,
    field: &str,
) -> Option<Address> {
    let image: Image = module.get_image(process, image_name)?;
    let class = image.get_class(process, module, class_name)?;
    let table = class.get_static_table(process, module)?;
    let offset = class.get_field_offset(process, module, field)?;
    Some(table.add(offset as u64))
}

/// Length of a .NET string, or `None` if the reference is null.
///
/// `MonoString` is a 16-byte object header, then an `i32` length, then UTF-16
/// characters. Only the length is needed to tell "set" from "unset".
pub fn string_len(process: &Process, reference: Address) -> Option<i32> {
    let pointer = Address::new(process.read::<u64>(reference).ok()?);
    if pointer.is_null() {
        return None;
    }
    process.read::<i32>(pointer.add(0x10)).ok()
}

/// Offset of a named field on a class named at compile time.
///
/// For classes we never need to locate an instance of -- the offset comes from
/// metadata, so it resolves whether or not one has ever been constructed.
pub fn field_offset(
    process: &Process,
    module: &Module,
    image_name: &str,
    class_name: &str,
    field: &str,
) -> Option<u32> {
    let image: Image = module.get_image(process, image_name)?;
    image
        .get_class(process, module, class_name)?
        .get_field_offset(process, module, field)
}

/// Offset of a named field on an object whose class is discovered at runtime.
///
/// For objects reached by dereference rather than by scanning, where there is
/// no need to build a [`Locatable`] for the class.
pub fn field_of(process: &Process, module: &Module, object: Address, field: &str) -> Option<u32> {
    Class::of_object(process, module, object)?.get_field_offset(process, module, field)
}

/// A class whose instances we can find in memory.
#[derive(Clone)]
pub struct Locatable {
    class: Class,
    vtable: Address,
    validator: Validator,
    name: &'static str,
}

impl Locatable {
    /// Resolves a class and the validator for it.
    ///
    /// Returns `None` until the class has been instantiated at least once,
    /// since Mono only fills in a vtable at that point. For a class that only
    /// exists in a loaded game, that absence is itself meaningful.
    pub fn new(
        process: &Process,
        module: &Module,
        image_name: &str,
        class_name: &'static str,
        event_bus_vtable: Address,
    ) -> Option<Self> {
        Self::with_validator(
            process,
            module,
            image_name,
            class_name,
            EVENT_BUS_FIELD,
            event_bus_vtable,
        )
    }

    /// As [`new`](Self::new), but validating through a different field.
    ///
    /// Classes that are not DI services have no `_eventBus`; `ComponentCache`
    /// points at its `ComponentCacheService` instead.
    pub fn with_validator(
        process: &Process,
        module: &Module,
        image_name: &str,
        class_name: &'static str,
        validation_field: &str,
        expected_vtable: Address,
    ) -> Option<Self> {
        let image: Image = module.get_image(process, image_name)?;
        let class = image.get_class(process, module, class_name)?;
        Some(Self {
            class,
            vtable: class.get_vtable(process, module)?,
            validator: Validator {
                field_offset: class.get_field_offset(process, module, validation_field)?,
                expected_vtable,
            },
            name: class_name,
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The class's vtable, which is what identifies one of its instances in a
    /// heap scan or in the singleton container.
    pub fn vtable(&self) -> Address {
        self.vtable
    }

    /// Offset of a field, by name. Resolved from metadata, so it works whether
    /// or not any instance exists.
    pub fn field(&self, process: &Process, module: &Module, name: &str) -> Option<u32> {
        self.class.get_field_offset(process, module, name)
    }

    /// Address of a static field, for values that live on the class rather
    /// than on an instance.
    pub fn static_field(&self, process: &Process, module: &Module, name: &str) -> Option<Address> {
        let table = self.class.get_static_table(process, module)?;
        let offset = self.class.get_field_offset(process, module, name)?;
        Some(table.add(offset as u64))
    }

    /// Finds the single instance of a singleton service.
    ///
    /// An empty result carries `conclusive: false` when the scan could not read
    /// everything, which happens while a scene is being torn down. Absence is
    /// only meaningful when the search was complete.
    pub async fn find_one(&self, process: &Process, table: Option<&ReferenceTable>) -> Found {
        self.table_then_sweep(process, Some(1), table, |found| !found.instances.is_empty())
            .await
    }

    /// The whole address space, a slice at a time.
    ///
    /// Public because a caller that has to judge candidates asynchronously --
    /// the DI container search walks each one's contents -- drives the two
    /// passes itself rather than through [`find_all_polling`](Self::find_all_polling).
    pub async fn sweep(
        &self,
        process: &Process,
        limit: Option<usize>,
        why: &str,
        anchor: Option<Address>,
        on_tick: impl FnMut(),
    ) -> Found {
        let mut scan = scan::Scan::new(process, self.vtable).validating(self.validator);
        if let Some(n) = limit {
            scan = scan.limit(n);
        }
        // Only when there is no table to lose: it costs a few percent of the
        // sweep, and buys the chance of not needing the next one.
        if let Some(anchor) = anchor {
            scan = scan.also_finding(anchor);
        }
        // Why, not just that: a sweep is the expensive path, and a log that
        // only says one happened leaves whoever reads it guessing whether the
        // table was missing, unreadable, or simply did not have the object.
        asr::print_message(&alloc::format!(
            "[scan] {} starting (full sweep -- {why}).",
            self.name
        ));
        let scan = scan
            .run_polling(process, scan::DEFAULT_BUDGET, on_tick)
            .await;
        let s = &scan.stats;
        asr::print_message(&alloc::format!(
            "[scan] {} done: {} slices, {} of {} MiB over {} ranges ({} skipped), \
             {} chunk read failures, {} KiB unreadable, {} found, {} rejected.",
            self.name,
            s.slices,
            s.bytes_scanned >> 20,
            s.bytes_total >> 20,
            s.ranges_scanned,
            s.ranges_skipped,
            s.read_failures,
            s.bytes_unreadable >> 10,
            scan.found.len(),
            scan.rejected.len(),
        ));
        let conclusive = scan.is_conclusive();
        Found {
            instances: scan.found,
            conclusive,
            table_candidates: scan.anchor_ranges,
        }
    }

    /// The same search, through the reference table.
    ///
    /// `None` when there is no table or it could not be read, which is not an
    /// answer about the game and leaves the caller to sweep. An empty result
    /// *is* an answer, just never a conclusive one -- see [`ReferenceTable`].
    pub async fn search_table(
        &self,
        process: &Process,
        limit: Option<usize>,
        table: Option<&ReferenceTable>,
        on_tick: impl FnMut(),
    ) -> Option<Found> {
        let table = table?;
        let search = table
            .instances(
                process,
                self.vtable,
                self.validator,
                limit.unwrap_or(usize::MAX),
                on_tick,
            )
            .await;
        if !search.readable {
            return None;
        }
        // Only when it found something. An empty result is either followed by
        // a sweep, whose reason line carries the count, or by another attempt
        // -- and the run-start bind makes a dozen of those per load.
        if !search.instances.is_empty() {
            asr::print_message(&alloc::format!(
                "[table] {}: {} live instances.",
                self.name,
                search.instances.len()
            ));
        }
        Some(Found {
            instances: search.instances,
            conclusive: false,
            table_candidates: Vec::new(),
        })
    }

    /// Asks the reference table first and sweeps only if that did not settle
    /// it.
    ///
    /// The sweep behind it is what keeps the shortcut honest. The table holds
    /// what the runtime is keeping alive, which is not provably everything the
    /// heap holds, so a miss costs one wasted pass over a couple of megabytes
    /// rather than a wrong answer.
    async fn table_then_sweep(
        &self,
        process: &Process,
        limit: Option<usize>,
        table: Option<&ReferenceTable>,
        settled: impl Fn(&Found) -> bool,
    ) -> Found {
        let why = match self.search_table(process, limit, table, || {}).await {
            Some(found) if settled(&found) => {
                if let Some(table) = table {
                    table.answered();
                }
                return found;
            }
            Some(found) if found.instances.is_empty() => {
                "the reference table has no instance of it".into()
            }
            Some(found) => alloc::format!(
                "the reference table held {} instance(s), none of them acceptable",
                found.instances.len()
            ),
            None if table.is_some() => "the reference table could not be read".into(),
            None => "no reference table found yet".into(),
        };
        // No anchor: this path only runs when there is a table already, and a
        // sweep behind a table that exists is not the moment to go looking for
        // another one.
        let swept = self.sweep(process, limit, &why, None, || {}).await;
        // The table said no and the heap said yes. That pairing is the only
        // thing that tells a table which has gone wrong from a question that
        // simply has no answer -- a sweep finding nothing either says nothing
        // about the table, so it is left alone rather than counted.
        if let Some(table) = table {
            if settled(&swept) {
                table.was_missing();
            }
        }
        swept
    }

    /// Finds one instance the caller is willing to accept.
    ///
    /// Scanning during a scene load can turn up the outgoing world's object
    /// alongside the incoming one's, and neither address is known ahead of
    /// time, so excluding a known address has nothing to exclude. A predicate
    /// on the object's own state can still tell them apart.
    /// `may_sweep` false asks only the reference table. For a caller that can
    /// afford to be told "not yet" and ask again -- the run-start bind, which
    /// has a whole scene load to work in and whose object reaches the heap
    /// before the runtime holds a reference to it.
    pub async fn find_matching(
        &self,
        process: &Process,
        limit: usize,
        table: Option<&ReferenceTable>,
        may_sweep: bool,
        accept: impl Fn(Address) -> bool,
    ) -> Found {
        let picked = |found: &Found| found.instances.iter().position(|&address| accept(address));
        let found = if may_sweep {
            self.table_then_sweep(process, Some(limit), table, |f| picked(f).is_some())
                .await
        } else {
            self.search_table(process, Some(limit), table, || {})
                .await
                .unwrap_or_else(|| Found {
                    instances: Vec::new(),
                    conclusive: false,
                    table_candidates: Vec::new(),
                })
        };
        match picked(&found) {
            Some(i) => Found {
                instances: alloc::vec![found.instances[i]],
                conclusive: found.conclusive,
                table_candidates: found.table_candidates,
            },
            None => Found {
                instances: Vec::new(),
                conclusive: found.conclusive,
                table_candidates: found.table_candidates,
            },
        }
    }

    /// Whether a previously located instance is still the object we think it
    /// is. Two reads, cheap enough to do every tick.
    pub fn still_valid(&self, process: &Process, instance: Address) -> bool {
        self.validator.accepts(process, instance)
    }
}
