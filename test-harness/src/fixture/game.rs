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
use crate::memory::FakeProcess;

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

    /// Writes a single byte, by name. A `bool` in the game is one.
    pub fn set_u8(&mut self, object: &Object, field: &str, value: u8) {
        let offset = self.offset(object, field);
        self.builder.write(object.address + offset, &[value]);
    }

    /// Where a field sits, from the fixture.
    ///
    /// Panics naming the field. A test writing to a field the fixture does not
    /// carry is asking for something no fixture can answer, and writing it to
    /// offset zero instead would put it on top of the object's vtable pointer
    /// — which would make the object stop being an instance of its class, some
    /// distance from the line that caused it.
    fn offset(&self, object: &Object, field: &str) -> u64 {
        let facts = self
            .fixture
            .class(&object.image, &object.class)
            .unwrap_or_else(|| {
                panic!("the fixture has no class {}/{}", object.image, object.class)
            });
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

    /// Builds the DI container over everything registered, and hands over the
    /// process.
    ///
    /// The container comes last because `_allSingletons` is an array, and an
    /// array has to know its length. That is true of the game as well: it is
    /// an `ImmutableArray`, so registering another singleton replaces the array
    /// rather than growing it.
    pub fn finish(mut self) -> FakeProcess {
        let singletons = std::mem::take(&mut self.singletons);
        let array = self.array(&singletons);

        let listener = self.object("Timberborn.SingletonSystem", "SingletonListener");
        self.set_ptr(&listener, "_allSingletons", array);

        let repository = self.object("Timberborn.SingletonSystem", "SingletonRepository");
        self.set_ptr(&repository, "_singletonListener", listener.address);

        self.builder.finish()
    }
}
