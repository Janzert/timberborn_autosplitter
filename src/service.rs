//! Locating game services and reading fields off them.
//!
//! Wraps the raw scan in [`crate::scan`] with the two things every split needs:
//! resolving a class by name, and validating that a match is a real instance.
//!
//! Nearly every Timberborn service holds an `_eventBus` pointing at the single
//! `EventBus`, which makes one shared validation target work for all of them.

use alloc::vec::Vec;

use asr::{
    game_engine::unity::mono::{Class, Image, Module},
    Address, Process,
};

use crate::scan::{self, Validator};

/// The field almost every service has, and the class it points at.
const VALIDATION_FIELD: &str = "_eventBus";
const VALIDATION_CLASS: (&str, &str) = ("Timberborn.SingletonSystem", "EventBus");

/// Resolves the shared validation target. Its vtable only exists once an
/// `EventBus` has been constructed, which is why this can fail early in a load.
pub fn event_bus_vtable(process: &Process, module: &Module) -> Option<Address> {
    let (image_name, class_name) = VALIDATION_CLASS;
    let image: Image = module.get_image(process, image_name)?;
    let class = image.get_class(process, module, class_name)?;
    class.get_vtable(process, module)
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
        let image: Image = module.get_image(process, image_name)?;
        let class = image.get_class(process, module, class_name)?;
        Some(Self {
            class,
            vtable: class.get_vtable(process, module)?,
            validator: Validator {
                field_offset: class.get_field_offset(process, module, VALIDATION_FIELD)?,
                expected_vtable: event_bus_vtable,
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

    /// Finds the single instance of a singleton service.
    pub async fn find_one(&self, process: &Process) -> Option<Address> {
        let scan = scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .stop_at_first()
            .run(process, scan::DEFAULT_BUDGET)
            .await;
        scan.found.first().copied()
    }

    /// Finds every instance. For classes that are legitimately multi-instance,
    /// where stopping at the first would be wrong.
    pub async fn find_all(&self, process: &Process) -> (Vec<Address>, bool) {
        let scan = scan::Scan::new(process, self.vtable)
            .validating(self.validator)
            .run(process, scan::DEFAULT_BUDGET)
            .await;
        // The second value says whether an empty result can be trusted.
        (scan.found.clone(), scan.is_conclusive())
    }

    /// Whether a previously located instance is still the object we think it
    /// is. Two reads, cheap enough to do every tick.
    pub fn still_valid(&self, process: &Process, instance: Address) -> bool {
        self.validator.accepts(process, instance)
    }
}
