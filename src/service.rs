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

use crate::scan::{self, Validator};

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
    pub async fn find_one(&self, process: &Process) -> Option<Address> {
        scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .limit(1)
            .run(process, scan::DEFAULT_BUDGET)
            .await
            .found
            .first()
            .copied()
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
        accept: impl Fn(Address) -> bool,
    ) -> Option<Address> {
        scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .limit(limit)
            .run(process, scan::DEFAULT_BUDGET)
            .await
            .found
            .iter()
            .copied()
            .find(|&address| accept(address))
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
        on_tick: impl FnMut(),
    ) -> (alloc::vec::Vec<Address>, bool) {
        let scan = scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .limit(limit)
            .run_polling(process, scan::DEFAULT_BUDGET, on_tick)
            .await;
        let conclusive = scan.is_conclusive();
        (scan.found, conclusive)
    }

    /// Whether a previously located instance is still the object we think it
    /// is. Two reads, cheap enough to do every tick.
    pub fn still_valid(&self, process: &Process, instance: Address) -> bool {
        self.validator.accepts(process, instance)
    }
}
