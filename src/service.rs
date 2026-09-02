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

use crate::scan::{self, HotRanges, Validator};

/// What a scan turned up.
pub struct Found {
    /// Instances that passed validation.
    pub instances: Vec<Address>,
    /// The range each instance came from, so the caller can feed [`HotRanges`]
    /// and make later scans cheap. Same length as `instances`.
    pub ranges: Vec<(Address, u64)>,
    /// Whether an empty result actually means "not present". Never true for a
    /// scan restricted to known ranges, which can only say "not where we
    /// looked".
    pub conclusive: bool,
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
pub fn field_of(
    process: &Process,
    module: &Module,
    object: Address,
    field: &str,
) -> Option<u32> {
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
    pub async fn find_one(&self, process: &Process, hot: &HotRanges) -> Found {
        self.hot_then_full(process, Some(1), hot, |found| !found.instances.is_empty())
            .await
    }

    /// One sweep: the known ranges if there are any, otherwise everything.
    async fn sweep(
        &self,
        process: &Process,
        limit: Option<usize>,
        hot: &HotRanges,
        on_tick: impl FnMut(),
    ) -> Found {
        let mut scan = scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .restricted_to(hot);
        if let Some(n) = limit {
            scan = scan.limit(n);
        }
        let restricted = !hot.is_empty();
        asr::print_message(&alloc::format!(
            "[scan] {} starting ({}).",
            self.name,
            if restricted { "known ranges" } else { "full" }
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
            ranges: scan.found_ranges,
            conclusive,
        }
    }

    /// Sweeps the known ranges first and falls back to the whole address space
    /// if that did not settle it.
    ///
    /// The fallback is what keeps the shortcut honest: the known set is only
    /// ever a hint, so a miss costs an extra pass over 21% of memory rather
    /// than a wrong answer.
    async fn hot_then_full(
        &self,
        process: &Process,
        limit: Option<usize>,
        hot: &HotRanges,
        settled: impl Fn(&Found) -> bool,
    ) -> Found {
        if !hot.is_empty() {
            let found = self.sweep(process, limit, hot, || {}).await;
            if settled(&found) {
                return found;
            }
        }
        self.sweep(process, limit, &HotRanges::default(), || {}).await
    }

    /// Finds one instance the caller is willing to accept.
    ///
    /// Scanning during a scene load can turn up the outgoing world's object
    /// alongside the incoming one's, and neither address is known ahead of
    /// time, so excluding a known address has nothing to exclude. A predicate
    /// on the object's own state can still tell them apart.
    pub async fn find_matching(
        &self,
        process: &Process,
        limit: usize,
        hot: &HotRanges,
        accept: impl Fn(Address) -> bool,
    ) -> Found {
        let picked = |found: &Found| {
            found
                .instances
                .iter()
                .position(|&address| accept(address))
        };
        let found = self
            .hot_then_full(process, Some(limit), hot, |f| picked(f).is_some())
            .await;
        match picked(&found) {
            Some(i) => Found {
                instances: alloc::vec![found.instances[i]],
                ranges: found.ranges.get(i).copied().into_iter().collect(),
                conclusive: found.conclusive,
            },
            None => Found {
                instances: Vec::new(),
                ranges: Vec::new(),
                conclusive: found.conclusive,
            },
        }
    }

    /// Every instance the scan finds, up to `limit`, polling between slices.
    ///
    /// For classes that are legitimately multi-instance and have to be told
    /// apart by what they contain rather than by address -- the DI containers,
    /// of which several are alive at once.
    pub async fn find_all_polling(
        &self,
        process: &Process,
        limit: usize,
        hot: &HotRanges,
        on_tick: impl FnMut(),
    ) -> Found {
        self.sweep(process, Some(limit), hot, on_tick).await
    }

    /// Whether a previously located instance is still the object we think it
    /// is. Two reads, cheap enough to do every tick.
    pub fn still_valid(&self, process: &Process, instance: Address) -> bool {
        self.validator.accepts(process, instance)
    }
}

/// Maps the ranges the managed heap occupies, using `vtable` as a probe.
///
/// Wants a class with many instances: measured, `ComponentCache` had 5916
/// spread over 139 ranges, which is the heap's whole footprint -- 1497 MiB of a
/// 6906 MiB address space. Every later scan can then skip the other 79%, which
/// is Unity's native memory and cannot hold a managed object.
///
/// Free-standing rather than a [`Locatable`] method because mapping needs only
/// a vtable, and `ComponentCache` is not a DI service: it has no `_eventBus`,
/// so no `Locatable` can be built for it. No validator either, deliberately --
/// an over-match only adds a range to look at later, costing a little time,
/// and the scans that use the map validate their own hits anyway.
/// Returns the map and whether it is complete. An aborted map is still sound
/// -- every range in it really does hold objects -- just not exhaustive, so the
/// caller should keep it and try again rather than trust it as the last word.
pub async fn map_heap(
    process: &Process,
    vtable: Address,
    name: &str,
    keep_going: impl FnMut() -> bool,
) -> (HotRanges, bool) {
    let scan = scan::Scan::new(process, vtable)
        .mapping()
        .run_polling_while(process, scan::MAP_BUDGET, keep_going)
        .await;
    let total = scan.stats.bytes_total;
    let mut hot = HotRanges::default();
    hot.remember(&scan.found_ranges);
    asr::print_message(&alloc::format!(
        "[scan] heap mapped via {}: {} of {} ranges hold managed objects, {} MiB of {} MiB ({}%). Later scans read only those.",
        name,
        hot.len(),
        scan.stats.ranges_scanned,
        hot.bytes() >> 20,
        total >> 20,
        hot.bytes().saturating_mul(100).checked_div(total).unwrap_or(0),
    ));
    if scan.aborted() {
        asr::print_message("[scan] heap map cut short by a scene load; will finish it later.");
    }
    let complete = !scan.aborted();
    (hot, complete)
}
