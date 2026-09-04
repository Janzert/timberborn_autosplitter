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

use asr::{game_engine::unity::mono::Module, Process};

/// A class we depend on, and the fields we read off it.
pub struct Subject {
    /// Assembly name, as Mono knows it (no `.dll`).
    pub image: &'static str,
    pub class: &'static str,
    pub fields: &'static [&'static str],
    /// What breaks if this is not found.
    pub used_for: &'static str,
}

/// Every name the design depends on.
///
/// Read at runtime by [`run`], offline by `devtools/metadata.py`, and by the
/// offline suite, which checks that a fixture covers all of it.
pub const SUBJECTS: &[Subject] = &[
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
        used_for: "split: the wonder unlocked with science",
    },
    Subject {
        image: "Timberborn.GameWonderCompletion",
        class: "WonderCompletionCountdownStarter",
        fields: &["CountdownFinished", "_unlockDay", "_eventBus"],
        used_for: "run end: the Congratulations screen",
    },
    Subject {
        image: "Timberborn.SingletonSystem",
        class: "SingletonRepository",
        fields: &["_singletonListener"],
        used_for: "the DI container every service is looked up in",
    },
    Subject {
        image: "Timberborn.SingletonSystem",
        class: "SingletonListener",
        fields: &["_allSingletons"],
        used_for: "the DI container every service is looked up in",
    },
    Subject {
        image: "Timberborn.GameOver",
        class: "GameOverChecker",
        // Wanted only because it is a singleton holding the entity registry,
        // which EntityService is not.
        fields: &["_entityRegistry"],
        used_for: "split: buildings finished (reaching the entity registry)",
    },
    Subject {
        image: "Timberborn.EntitySystem",
        class: "EntityRegistry",
        fields: &["_entitiesInInstantiationOrder"],
        used_for: "split: buildings finished",
    },
    Subject {
        image: "Timberborn.BlockSystem",
        class: "BlockObjectState",
        fields: &["_state"],
        used_for: "split: buildings finished (is it finished yet)",
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
        // _name is the template name; _components is where the entity's
        // BlockObjectState is found.
        fields: &["_components", "_name"],
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
        image: "Timberborn.SceneLoading",
        class: "SceneLoader",
        fields: &["_isLoading", "_assetLoader", "_sceneParameters"],
        used_for: "the scene-load anchor the whole lifecycle hangs on",
    },
    Subject {
        image: "Timberborn.AssetSystem",
        class: "AssetLoader",
        fields: &[],
        used_for: "validating the SceneLoader instance",
    },
    Subject {
        image: "Timberborn.GameSceneLoading",
        class: "GameSceneParameters",
        // Which of the two is set says new game versus loaded save.
        fields: &[
            "<NewGameConfiguration>k__BackingField",
            "<SaveReference>k__BackingField",
        ],
        used_for: "run start: is the incoming game new or a save",
    },
    Subject {
        image: "Timberborn.MainMenuSceneLoading",
        class: "MainMenuSceneParameters",
        fields: &[],
        used_for: "classifying an incoming scene",
    },
    Subject {
        image: "Timberborn.MapEditorSceneLoading",
        class: "MapEditorSceneParameters",
        fields: &[],
        used_for: "classifying an incoming scene",
    },
    Subject {
        image: "Timberborn.ErrorReporting",
        class: "WorldDataService",
        // A process-wide static, so it can hold a value left over from an
        // earlier load. Only the fallback now, for attaching with no load
        // watched; the scene parameters above are the real answer.
        fields: &["SourceFileName"],
        used_for: "run start (fallback)",
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
