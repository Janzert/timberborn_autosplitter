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

/// The outcome of a search.
pub struct Found {
    pub first: Option<Address>,
    /// Whether an empty result actually means "not present". False when the
    /// scan could not read everything it set out to.
    pub conclusive: bool,
}

/// A class whose instances we can find in memory.
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
    pub async fn find_one(&self, process: &Process) -> Found {
        let scan = scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .limit(1)
            .run(process, scan::DEFAULT_BUDGET)
            .await;
        Found {
            first: scan.found.first().copied(),
            conclusive: scan.is_conclusive(),
        }
    }

    /// Finds up to `limit` instances. For classes with many instances, where
    /// a sample is enough.
    pub async fn find_upto(&self, process: &Process, limit: usize) -> alloc::vec::Vec<Address> {
        scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .limit(limit)
            .run(process, scan::DEFAULT_BUDGET)
            .await
            .found
    }

    /// Whether a previously located instance is still the object we think it
    /// is. Two reads, cheap enough to do every tick.
    pub fn still_valid(&self, process: &Process, instance: Address) -> bool {
        self.validator.accepts(process, instance)
    }
}
