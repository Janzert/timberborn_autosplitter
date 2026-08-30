//! A version-compatibility check.
//!
//! Everything the splitter reads is resolved by name at runtime, which should
//! make it survive game updates. This probe is how that claim gets evidence
//! rather than assertion: it resolves every class and field the design depends
//! on and reports what did and did not resolve.
//!
//! Run it against a new game version -- the Steam `experimental` branch, or a
//! release the day it lands -- and anything that moved shows up immediately,
//! with a name, instead of as a mysterious failure mid-run.
//!
//! Field offsets come from Mono metadata and resolve whether or not the class
//! has ever been instantiated. A vtable does not: Mono fills that in lazily, so
//! a missing vtable means "not constructed yet in this session", not "broken".

use alloc::{format, string::String, vec::Vec};

use asr::{
    game_engine::unity::mono::Module,
    string::ArrayWString,
    Address, Process,
};

use crate::service;

/// A class we depend on, and the fields we read off it.
struct Subject {
    /// Assembly name, as Mono knows it (no `.dll`).
    image: &'static str,
    class: &'static str,
    fields: &'static [&'static str],
    /// What breaks if this is not found.
    used_for: &'static str,
}

const SUBJECTS: &[Subject] = &[
    Subject {
        image: "Timberborn.TimeSystem",
        class: "DayNightCycle",
        fields: &["DayNumber", "_eventBus"],
        used_for: "day counter, run start",
    },
    Subject {
        image: "Timberborn.SingletonSystem",
        class: "EventBus",
        fields: &[],
        used_for: "instance validation",
    },
    Subject {
        image: "Timberborn.ScienceSystem",
        class: "BuildingUnlockingService",
        fields: &["_unlockedBuildings", "_eventBus"],
        used_for: "split: research Earth Recultivator",
    },
    Subject {
        image: "Timberborn.Wonders",
        class: "Wonder",
        fields: &["IsActive", "_eventBus"],
        used_for: "split: wonder activated",
    },
    Subject {
        image: "Timberborn.GameDistricts",
        class: "DistrictBuildingRegistry",
        fields: &["_finishedBuildings", "_instantFinishedBuildings"],
        used_for: "split: buildings finished",
    },
    Subject {
        image: "Timberborn.BaseComponentSystem",
        class: "BaseComponent",
        fields: &["_componentCache"],
        used_for: "split: buildings finished",
    },
    Subject {
        image: "Timberborn.BaseComponentSystem",
        class: "ComponentCache",
        // _name may already hold the template name. If so the buildings split
        // is one string read instead of a component walk.
        fields: &["_components", "_name"],
        used_for: "split: buildings finished",
    },
    Subject {
        image: "Timberborn.TemplateSystem",
        class: "TemplateSpec",
        fields: &["TemplateName"],
        used_for: "split: buildings finished",
    },
    Subject {
        image: "Timberborn.Population",
        class: "PopulationService",
        fields: &["GlobalPopulationData"],
        used_for: "population",
    },
    Subject {
        image: "Timberborn.Population",
        class: "PopulationData",
        fields: &["NumberOfAdults", "NumberOfChildren"],
        used_for: "population",
    },
    Subject {
        image: "Timberborn.ErrorReporting",
        class: "WorldDataService",
        // Static, and empty on a new game: the most promising authoritative
        // new-game-vs-loaded-save signal.
        fields: &["SourceFileName"],
        used_for: "run start",
    },
];

/// Resolves every subject and logs the result. Returns `true` if everything
/// resolved.
///
/// Best run once a save is loaded, since Mono loads assemblies lazily and some
/// will not be present in the main menu.
pub fn run(process: &Process, module: &Module) -> bool {
    asr::print_message(&format!(
        "--- probe: mono {:?}, {:?} ---",
        module.get_version(),
        module.get_pointer_size(),
    ));

    let mut missing_classes = 0;
    let mut missing_fields = 0;

    for subject in SUBJECTS {
        let Some(image) = module.get_image(process, subject.image) else {
            missing_classes += 1;
            asr::print_message(&format!(
                "  MISSING IMAGE  {} ({})",
                subject.image, subject.used_for
            ));
            continue;
        };
        let Some(class) = image.get_class(process, module, subject.class) else {
            missing_classes += 1;
            asr::print_message(&format!(
                "  MISSING CLASS  {}/{} ({})",
                subject.image, subject.class, subject.used_for
            ));
            continue;
        };

        let mut parts: Vec<String> = Vec::new();
        for field in subject.fields {
            match class.get_field_offset(process, module, field) {
                Some(offset) => parts.push(format!("{field} +0x{offset:X}")),
                None => {
                    missing_fields += 1;
                    parts.push(format!("{field} MISSING"));
                }
            }
        }
        // Lazily populated, so absence here is a statement about this session,
        // not about the game version.
        parts.push(match class.get_vtable(process, module) {
            Some(vtable) => format!("vtable {vtable}"),
            None => String::from("vtable not yet constructed"),
        });

        asr::print_message(&format!(
            "  ok  {}/{}: {}",
            subject.image,
            subject.class,
            parts.join(" | "),
        ));
    }

    let ok = missing_classes == 0 && missing_fields == 0;
    asr::print_message(&format!(
        "--- probe: {} ({} classes unresolved, {} fields unresolved) ---",
        if ok { "ALL RESOLVED" } else { "MISMATCH" },
        missing_classes,
        missing_fields,
    ));
    ok
}

/// Samples `ComponentCache._name` to settle what it actually holds.
///
/// The buildings splits need a finished building's template name, e.g.
/// `Forester.Folktails`. The documented path is a walk:
///
/// ```text
/// building -> BaseComponent._componentCache -> ComponentCache._components
///          -> find TemplateSpec -> TemplateName
/// ```
///
/// If `_name` already carries that name, the last two hops collapse to one
/// string read per building. That matters here more than anywhere else,
/// because this is the one split that touches many objects rather than one.
///
/// `ComponentCache` is not a DI service and has no `_eventBus`, so it is
/// validated through `_componentCacheService` instead.
/// Returns false if the classes are not constructed yet, so the caller can
/// retry. Neither exists until the game has entities.
pub async fn sample_component_names(
    process: &Process,
    module: &Module,
    count: usize,
) -> bool {
    let Some(service_vtable) = service::class_vtable(
        process,
        module,
        "Timberborn.BaseComponentSystem",
        "ComponentCacheService",
    ) else {
        return false;
    };

    let Some(cache) = service::Locatable::with_validator(
        process,
        module,
        "Timberborn.BaseComponentSystem",
        "ComponentCache",
        "_componentCacheService",
        service_vtable,
    ) else {
        return false;
    };

    let Some(name_offset) = cache.field(process, module, "_name") else {
        asr::print_message("probe: ComponentCache has no _name.");
        return true;
    };

    let instances = cache.find_upto(process, count).await;
    asr::print_message(&format!(
        "probe: {} ComponentCache instances sampled, _name at +0x{name_offset:X}:",
        instances.len()
    ));

    for instance in instances {
        let text = read_string(process, instance.add(name_offset as u64))
            .unwrap_or_else(|| String::from("<unreadable>"));
        asr::print_message(&format!("  {instance}  _name = {text:?}"));
    }
    true
}

/// Reads a .NET string for display. Truncates rather than failing on long ones.
fn read_string(process: &Process, reference: Address) -> Option<String> {
    let pointer = Address::new(process.read::<u64>(reference).ok()?);
    if pointer.is_null() {
        return None;
    }
    let len = process.read::<i32>(pointer.add(0x10)).ok()?;
    if !(0..=256).contains(&len) {
        return None;
    }
    let chars = process.read::<ArrayWString<64>>(pointer.add(0x14)).ok()?;
    Some(String::from_utf16_lossy(chars.as_slice()))
}
