//! A Timberborn-shaped world on top of the synthetic Mono heap.
//!
//! [`Builder`](super::Builder) knows about Mono and nothing about the game: it
//! can place an object of a class and say where the class's vtable is. This
//! knows about the game and nothing about Mono: the DI container holds every
//! service in one array, a service is recognised by its `_eventBus`, the scene
//! loader is recognised by its `_assetLoader`. Everything here exists because
//! the splitter walks it.
//!
//! ```text
//!   SingletonRepository ─► SingletonListener ─► object[] ─┬─► DayNightCycle
//!                                                         ├─► EventBus
//!                                                         └─► ...
//!   SceneLoader ─┬─► AssetLoader          (what validates the loader)
//!                └─► GameSceneParameters  (new game, or a save)
//! ```
//!
//! Fields are written **by name**, resolved through the fixture, so a scene
//! built here is laid out the way the game lays one out. A field the fixture
//! does not carry is a panic rather than a silent write to offset zero: the
//! test is asking for something the fixture cannot answer, and the fix is to
//! add it to `src/probe.rs` and regenerate.
//!
//! # What this is not
//!
//! A world here is *still*. The real thing changes under the splitter — a
//! scene loads, a building finishes, a container is replaced — and two of the
//! bugs that cost recorded runs were things that vary live and had been frozen
//! by a test. Driving a scene through changes is what a scenario does with it;
//! see `Scene::set_*`, which stay usable right up until [`Scene::finish`].

use super::{Builder, Fixture};
use crate::memory::{FakeProcess, SharedMemory};

/// `MonoString`: object header, then the length, then UTF-16 characters.
///
/// Part of the Mono ABI rather than of any game version, like the object
/// header itself. `src/collections.rs` reads the same two numbers.
const STRING_LENGTH: u64 = 0x10;
const STRING_CHARS: u64 = 0x14;

/// `MonoArray`: object header, then bounds, then length, then the elements.
const ARRAY_LENGTH: u64 = 0x18;
const ARRAY_DATA: u64 = 0x20;

/// An object on the synthetic heap, and the class it is an instance of.
///
/// Carries its class so fields can be written by name: `set_ptr(&bus, ...)`
/// rather than repeating which class the offset should come from.
#[derive(Clone, Debug)]
pub struct Object {
    pub address: u64,
    image: String,
    class: String,
}

impl Object {
    /// The address, for a test asserting on what the splitter logged.
    pub fn address(&self) -> u64 {
        self.address
    }
}

/// One loaded game, as the splitter would find it.
pub struct Scene {
    fixture: Fixture,
    builder: Builder,
    /// Everything the DI container will hold, in the order it was created.
    singletons: Vec<u64>,
    /// The shared validation target every service points at.
    event_bus: Object,
    /// Whether the DI container gets an entry in the reference table. See
    /// [`Scene::container_unreferenced`].
    container_referenced: bool,
}

impl Scene {
    /// An empty scene: an event bus, and nothing else yet.
    ///
    /// The bus comes first because it is what makes anything else findable —
    /// almost every service is validated by its `_eventBus` pointing here.
    pub fn new(fixture: &Fixture) -> Self {
        let mut builder = fixture.builder();
        let address = builder.new_object("Timberborn.SingletonSystem", "EventBus", 0);
        let event_bus = Object {
            address,
            image: "Timberborn.SingletonSystem".into(),
            class: "EventBus".into(),
        };
        let mut scene = Self {
            fixture: fixture.clone(),
            builder,
            singletons: Vec::new(),
            event_bus,
            container_referenced: true,
        };
        scene.singletons.push(address);
        scene
    }

    /// The event bus every service is validated through.
    pub fn event_bus(&self) -> &Object {
        &self.event_bus
    }

    /// The builder underneath, for anything this does not model.
    pub fn builder(&mut self) -> &mut Builder {
        &mut self.builder
    }

    /// Builds the DI container with no entry in the runtime's reference table.
    ///
    /// The object is still on the heap and still validates -- only the
    /// runtime's reference to it is missing. That is the one discrepancy a
    /// capture actually showed, and it is what the sweep behind the table
    /// exists for, so a world that can express it is how the fallback stays
    /// tested rather than assumed.
    pub fn container_unreferenced(mut self) -> Self {
        self.container_referenced = false;
        self
    }

    /// Builds a process with no reference table at all, so every search the
    /// splitter makes falls back to sweeping. See
    /// [`Builder::without_reference_table`].
    pub fn without_reference_table(mut self) -> Self {
        self.builder = self.builder.without_reference_table();
        self
    }

    /// Reports the reference table short at first and at full size later,
    /// which is what the real one does. See
    /// [`Builder::reference_table_grows`].
    pub fn reference_table_grows(mut self) -> Self {
        self.builder = self.builder.reference_table_grows();
        self
    }

    /// The switch that grows it, for a test to flip mid-run.
    pub fn growth(&self) -> Option<std::rc::Rc<std::cell::Cell<bool>>> {
        self.builder.growth()
    }

    /// Drops an object's entry from the reference table, leaving the object
    /// itself alone.
    pub fn forget(&mut self, object: &Object) {
        self.builder.forget(object.address);
    }

    /// A bare object of a class: no fields set, not in the container.
    pub fn object(&mut self, image: &str, class: &str) -> Object {
        let address = self.builder.new_object(image, class, 0);
        Object {
            address,
            image: image.into(),
            class: class.into(),
        }
    }

    /// A DI service: an object with its `_eventBus` wired up, registered in
    /// the container.
    ///
    /// Both halves matter and for different reasons. The bus is what makes a
    /// heap scan accept the object as a real instance rather than as loose
    /// memory that happens to start with the right pointer. The container is
    /// how the splitter reaches it without scanning for each one separately.
    pub fn service(&mut self, image: &str, class: &str) -> Object {
        let object = self.object(image, class);
        self.set_ptr(&object, "_eventBus", self.event_bus.address);
        self.singletons.push(object.address);
        object
    }

    /// Puts an existing object in the DI container.
    ///
    /// For the ones that are singletons but are not services with an event bus
    /// of their own.
    pub fn register(&mut self, object: &Object) {
        self.singletons.push(object.address);
    }

    /// The scene loader, and what it says was last loaded.
    ///
    /// `new_game` distinguishes a fresh settlement from a loaded save, which
    /// the splitter reads off the parameters object rather than off any
    /// process-wide state — the two `k__BackingField`s, exactly one of which
    /// is set.
    pub fn scene_loader(&mut self, loading: bool, new_game: bool) -> Object {
        // Not a DI service, so it is validated through the asset loader it
        // holds rather than through an event bus.
        let asset_loader = self.object("Timberborn.AssetSystem", "AssetLoader");
        let params = self.object("Timberborn.GameSceneLoading", "GameSceneParameters");
        let field = if new_game {
            "<NewGameConfiguration>k__BackingField"
        } else {
            "<SaveReference>k__BackingField"
        };
        // Any non-null reference will do: the splitter asks whether the field
        // is set, not what it points at.
        let configuration = self.builder.alloc(0x20);
        self.set_ptr(&params, field, configuration);

        let loader = self.object("Timberborn.SceneLoading", "SceneLoader");
        self.set_ptr(&loader, "_assetLoader", asset_loader.address);
        self.set_ptr(&loader, "_sceneParameters", params.address);
        self.set_u8(&loader, "_isLoading", u8::from(loading));
        loader
    }

    /// A scene loader for a scene that is not a game: the main menu, or the
    /// map editor.
    pub fn scene_loader_for(&mut self, loading: bool, image: &str, params_class: &str) -> Object {
        let asset_loader = self.object("Timberborn.AssetSystem", "AssetLoader");
        let params = self.object(image, params_class);
        let loader = self.object("Timberborn.SceneLoading", "SceneLoader");
        self.set_ptr(&loader, "_assetLoader", asset_loader.address);
        self.set_ptr(&loader, "_sceneParameters", params.address);
        self.set_u8(&loader, "_isLoading", u8::from(loading));
        loader
    }

    /// A `MonoString`, for a template name or a save name.
    pub fn string(&mut self, text: &str) -> u64 {
        let chars: Vec<u16> = text.encode_utf16().collect();
        // A trailing NUL, as Mono stores: nothing here reads it, but a string
        // that is exactly as long as its characters is a shape the game never
        // has.
        let address = self
            .builder
            .alloc(STRING_CHARS + 2 * (chars.len() as u64 + 1));
        self.builder
            .write_u32(address + STRING_LENGTH, chars.len() as u32);
        for (index, unit) in chars.iter().enumerate() {
            self.builder.write(
                address + STRING_CHARS + 2 * index as u64,
                &unit.to_le_bytes(),
            );
        }
        address
    }

    /// A `MonoArray` of references.
    pub fn array(&mut self, elements: &[u64]) -> u64 {
        let address = self.builder.alloc(ARRAY_DATA + 8 * elements.len() as u64);
        self.builder
            .write_u64(address + ARRAY_LENGTH, elements.len() as u64);
        for (index, &element) in elements.iter().enumerate() {
            self.builder
                .write_u64(address + ARRAY_DATA + 8 * index as u64, element);
        }
        address
    }

    /// Writes a reference into a field, by name.
    pub fn set_ptr(&mut self, object: &Object, field: &str, value: u64) {
        let offset = self.offset(object, field);
        self.builder.write_u64(object.address + offset, value);
    }

    /// Writes a 32-bit field, by name. The day counter, a population count.
    pub fn set_i32(&mut self, object: &Object, field: &str, value: i32) {
        let offset = self.offset(object, field);
        self.builder
            .write_u32(object.address + offset, value as u32);
    }

    /// Writes a 32-bit float field, by name — the clock's day lengths.
    pub fn set_f32(&mut self, object: &Object, field: &str, value: f32) {
        let offset = self.offset(object, field);
        self.builder
            .write(object.address + offset, &value.to_le_bytes());
    }

    /// Writes a single byte, by name. A `bool` in the game is one.
    pub fn set_u8(&mut self, object: &Object, field: &str, value: u8) {
        let offset = self.offset(object, field);
        self.builder.write(object.address + offset, &[value]);
    }

    fn offset(&self, object: &Object, field: &str) -> u64 {
        offset_in(&self.fixture, object, field)
    }

    /// Builds the DI container over everything registered, and hands over the
    /// process.
    ///
    /// The container comes last because `_allSingletons` is an array, and an
    /// array has to know its length. That is true of the game as well: it is
    /// an `ImmutableArray`, so registering another singleton replaces the array
    /// rather than growing it.
    pub fn finish(self) -> FakeProcess {
        self.finish_live().0
    }

    /// The process, and a handle for changing it while the splitter runs.
    pub fn finish_live(mut self) -> (FakeProcess, Live) {
        let singletons = std::mem::take(&mut self.singletons);
        let array = self.array(&singletons);

        let listener = self.object("Timberborn.SingletonSystem", "SingletonListener");
        self.set_ptr(&listener, "_allSingletons", array);

        let repository = self.object("Timberborn.SingletonSystem", "SingletonRepository");
        self.set_ptr(&repository, "_singletonListener", listener.address);
        if !self.container_referenced {
            self.builder.forget(repository.address);
        }

        let (process, memory) = self.builder.finish_live();
        (
            process,
            Live {
                fixture: self.fixture,
                memory,
            },
        )
    }
}

/// `Slot<T>` in Mono's `HashSet<T>`: `int _hashCode`, `int _next`, then the
/// reference, 8-aligned. A negative hash marks a free slot.
const SLOT_SIZE: u64 = 16;
const SLOT_VALUE: u64 = 8;

/// The paths the fixture records the nameless layouts under.
///
/// Written out rather than passed in because they are what a fixture *is*
/// keyed by: a test asking for "the list the entity registry holds" is asking
/// for this exact string, and a typo would otherwise reach the panic in
/// `new_instance` with nothing to compare against.
pub mod reached_by {
    /// `List<EntityComponent>`, the registry's instantiation-order list.
    pub const ENTITY_LIST: &str = "Timberborn.GameOver/GameOverChecker -> \
         GameOverChecker._entityRegistry -> EntityRegistry._entitiesInInstantiationOrder";
    /// `HashSet<string>` of unlocked building names.
    pub const UNLOCKED_SET: &str = "Timberborn.ScienceSystem/BuildingUnlockingService -> \
         BuildingUnlockingService._unlockedBuildings";
    /// `List<object>`, an entity's component cache.
    pub const COMPONENT_LIST: &str = "Timberborn.GameOver/GameOverChecker -> \
         GameOverChecker._entityRegistry -> EntityRegistry._entitiesInInstantiationOrder -> \
         [0] -> BaseComponent._componentCache -> ComponentCache._components";
}

impl Scene {
    /// Every service the splitter resolves on its way to a run.
    ///
    /// `initialization_state` is what `GameInitializer` starts at: a
    /// pre-overlay value for a game that is still loading, `Finished` for one
    /// already up. The splitter treats those completely differently, and
    /// getting it wrong is how a scenario accidentally tests the mid-run
    /// attach path while believing it is testing a run start.
    pub fn core_services(&mut self, initialization_state: i32) -> CoreServices {
        let clock = self.service("Timberborn.TimeSystem", "DayNightCycle");
        self.set_i32(&clock, "DayNumber", 1);
        // The day lengths the countdown diagnostic divides by. Plausible
        // rather than measured: nothing splits on them, and a zero here makes
        // the log report a completion day of 5.6e-47 instead of a number.
        self.set_f32(&clock, "DayLengthInSeconds", 900.0);
        self.set_f32(&clock, "DaytimeLengthInHours", 16.0);
        self.set_f32(&clock, "NighttimeLengthInHours", 8.0);

        let unlocking = self.service("Timberborn.ScienceSystem", "BuildingUnlockingService");
        let countdown = self.service(
            "Timberborn.GameWonderCompletion",
            "WonderCompletionCountdownStarter",
        );

        // Reached through the container rather than by scanning, so these need
        // no event bus for the splitter to recognise them.
        let game_over = self.object("Timberborn.GameOver", "GameOverChecker");
        self.register(&game_over);
        let population = self.object("Timberborn.Population", "PopulationService");
        self.register(&population);
        let data = self.object("Timberborn.Population", "PopulationData");
        self.set_ptr(&population, "GlobalPopulationData", data.address);

        let initializer = self.service("Timberborn.GameStartup", "GameInitializer");
        self.set_i32(&initializer, "_initializationState", initialization_state);

        CoreServices {
            clock,
            unlocking,
            countdown,
            game_over,
            population,
            initializer,
        }
    }

    /// A `List<T>` of references, laid out as the fixture measured one.
    ///
    /// `reached_by` says *which* list: each instantiation is its own class
    /// with its own offsets, and the fixture identifies one by the path walked
    /// to reach it rather than by a name, because asr cannot read the name of
    /// an inflated generic at all.
    pub fn list(&mut self, reached_by: &str, elements: &[u64]) -> u64 {
        let items = self.array(elements);
        let object = self.builder.new_instance(reached_by, 0);
        self.write_instance_field(reached_by, object, "_items", items);
        self.write_instance_i32(reached_by, object, "_size", elements.len() as i32);
        object
    }

    /// A `HashSet<string>` holding `values`, every slot occupied.
    ///
    /// No free slots and no hashing: the splitter walks the used region and
    /// compares strings, and never looks a value up by hash. Modelling the
    /// buckets would be modelling Mono rather than what is read.
    pub fn hash_set(&mut self, reached_by: &str, values: &[&str]) -> u64 {
        let strings: Vec<u64> = values.iter().map(|value| self.string(value)).collect();
        let slots = self
            .builder
            .alloc(ARRAY_DATA + SLOT_SIZE * strings.len() as u64);
        self.builder
            .write_u64(slots + ARRAY_LENGTH, strings.len() as u64);
        for (index, &string) in strings.iter().enumerate() {
            let slot = slots + ARRAY_DATA + SLOT_SIZE * index as u64;
            // A non-negative hash marks the slot as occupied; the value itself
            // is never hashed by anything that reads this.
            self.builder.write_u32(slot, index as u32);
            self.builder.write_u64(slot + SLOT_VALUE, string);
        }

        let object = self.builder.new_instance(reached_by, 0);
        self.write_instance_field(reached_by, object, "_slots", slots);
        self.write_instance_i32(reached_by, object, "_count", strings.len() as i32);
        self.write_instance_i32(reached_by, object, "_lastIndex", strings.len() as i32);
        object
    }

    /// One entity, as the splitter reads it: a `BaseComponent` whose component
    /// cache names the template and lists the components.
    ///
    /// `finished` is the `BlockObjectState._state` the splitter tests to tell a
    /// finished building from a construction site. The cache name is the live
    /// form, `<template>.EntityComponent` — prefabs use the bare template name,
    /// and the splitter accepts both, which is why this uses the one a running
    /// game has.
    pub fn entity(&mut self, template: &str, state: i32) -> Entity {
        let block_state = self.object("Timberborn.BlockSystem", "BlockObjectState");
        self.set_i32(&block_state, "_state", state);

        let components = self.list(reached_by::COMPONENT_LIST, &[block_state.address]);

        let cache = self.object("Timberborn.BaseComponentSystem", "ComponentCache");
        let name = self.string(&format!("{template}.EntityComponent"));
        self.set_ptr(&cache, "_name", name);
        self.set_ptr(&cache, "_components", components);

        let entity = self.object("Timberborn.BaseComponentSystem", "BaseComponent");
        self.set_ptr(&entity, "_componentCache", cache.address);
        Entity {
            component: entity,
            block_state,
        }
    }

    fn write_instance_field(&mut self, reached_by: &str, object: u64, field: &str, value: u64) {
        let offset = self.instance_offset(reached_by, field);
        self.builder.write_u64(object + offset, value);
    }

    fn write_instance_i32(&mut self, reached_by: &str, object: u64, field: &str, value: i32) {
        let offset = self.instance_offset(reached_by, field);
        self.builder.write_u32(object + offset, value as u32);
    }

    fn instance_offset(&self, reached_by: &str, field: &str) -> u64 {
        instance_offset_in(&self.fixture, reached_by, field)
    }
}

/// Where a field sits, from the fixture.
///
/// Panics naming the field. A test writing to a field the fixture does not
/// carry is asking for something no fixture can answer, and writing it to
/// offset zero instead would land on the object's vtable pointer — which would
/// make the object stop being an instance of its class, some distance from the
/// line that caused it.
fn offset_in(fixture: &Fixture, object: &Object, field: &str) -> u64 {
    let facts = fixture
        .class(&object.image, &object.class)
        .unwrap_or_else(|| panic!("the fixture has no class {}/{}", object.image, object.class));
    let field = facts.field(field).unwrap_or_else(|| {
        panic!(
            "the fixture has no {}/{}.{field}. Add it to SUBJECTS in \
             src/probe.rs and regenerate: `cargo fixture`.",
            object.image, object.class
        )
    });
    assert!(
        !field.is_static,
        "{}/{}.{} is a static field; it lives in the class's static table, \
         not on an instance",
        object.image, object.class, field.name
    );
    u64::from(field.offset)
}

fn instance_offset_in(fixture: &Fixture, reached_by: &str, field: &str) -> u64 {
    let facts = fixture
        .instance(reached_by)
        .unwrap_or_else(|| panic!("the fixture has no instance layout {reached_by:?}"));
    u64::from(
        facts
            .field(field)
            .unwrap_or_else(|| panic!("{reached_by:?} has no {field}"))
            .offset,
    )
}

/// The services every loaded game has, as the splitter looks for them.
///
/// Not a menu of options: the splitter resolves all of these on its way to a
/// run, so a world missing one is a world it spins in rather than a smaller
/// world. Which is why they are built together.
#[derive(Clone, Debug)]
pub struct CoreServices {
    /// `DayNightCycle`, the clock and the day counter.
    pub clock: Object,
    /// `BuildingUnlockingService`, whose set says whether the wonder is
    /// unlocked. Its `_unlockedBuildings` is left null; a scenario that cares
    /// gives it a set.
    pub unlocking: Object,
    /// `WonderCompletionCountdownStarter`, which holds the run end.
    pub countdown: Object,
    /// `GameOverChecker`, wanted only because it is the singleton that holds
    /// the entity registry. Its `_entityRegistry` is left null.
    pub game_over: Object,
    pub population: Object,
    /// `GameInitializer`, whose `_initializationState` is the run start.
    pub initializer: Object,
}

/// One entity, in the two pieces a scenario touches.
///
/// The splitter reads a building's identity off its component cache and its
/// completion off a `BlockObjectState` among its components, so finishing a
/// building means writing to a different object than the one in the registry's
/// list. Returning both is what stops a scenario writing "finished" onto the
/// entity itself, which is a field that does not exist there.
#[derive(Clone, Debug)]
pub struct Entity {
    /// What the entity registry's list holds.
    pub component: Object,
    /// Where `_state` says whether the building is finished.
    pub block_state: Object,
}

/// A built world that a scenario keeps changing.
///
/// The same writes [`Scene`] does, against the process the splitter is already
/// attached to. Only *changes* — nothing new can be placed, because a process's
/// memory map is fixed once it exists, exactly as the game's regions are while
/// a run is played. A scenario therefore builds everything the run will ever
/// need and then reveals it: an entity list whose `_size` starts at zero and
/// grows is how a building gets placed.
pub struct Live {
    fixture: Fixture,
    memory: SharedMemory,
}

impl Live {
    pub fn set_ptr(&self, object: &Object, field: &str, value: u64) {
        let offset = self.offset(object, field);
        self.memory
            .poke(object.address + offset, &value.to_le_bytes());
    }

    pub fn set_i32(&self, object: &Object, field: &str, value: i32) {
        let offset = self.offset(object, field);
        self.memory
            .poke(object.address + offset, &value.to_le_bytes());
    }

    pub fn set_u8(&self, object: &Object, field: &str, value: u8) {
        let offset = self.offset(object, field);
        self.memory.poke(object.address + offset, &[value]);
    }

    /// Sets a field of one of the nameless layouts — a list's `_size`, a set's
    /// `_count`. Growing those is how a scenario makes something appear.
    pub fn set_instance_i32(&self, reached_by: &str, object: u64, field: &str, value: i32) {
        let offset = self.instance_offset(reached_by, field);
        self.memory.poke(object + offset, &value.to_le_bytes());
    }

    fn offset(&self, object: &Object, field: &str) -> u64 {
        offset_in(&self.fixture, object, field)
    }

    fn instance_offset(&self, reached_by: &str, field: &str) -> u64 {
        instance_offset_in(&self.fixture, reached_by, field)
    }
}
