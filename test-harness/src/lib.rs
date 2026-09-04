//! A fake auto splitting runtime, so the splitter can be driven without a game.
//!
//! `asr` reaches the host through 71 `extern "C"` imports, which off wasm are
//! undefined symbols. This crate defines every one of them ([`imports`]) on top
//! of a [`World`] held in thread-local storage, so the splitter links and runs
//! natively and a test can inspect what it did.
//!
//! ```ignore
//! // `ignore` rather than `no_run`: this crate deliberately does not depend on
//! // the splitter, so a doctest here cannot name it. Real examples are in
//! // `tests/`, which does.
//! let world = World::new().with_process(FakeProcess::new(1234, "Timberborn.exe"));
//! let world = test_harness::drive(world, timberborn_autosplitter::main(), 200);
//! assert_eq!(world.timer.splits(), 0);
//! ```
//!
//! Not every import is implemented. The ones the splitter never calls -- most
//! of the settings API -- panic naming themselves, so if that ever changes it
//! says so instead of quietly returning a plausible value.

use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    pin::pin,
    ptr,
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

#[cfg(target_os = "linux")]
pub mod capture;
pub mod fixture;
#[cfg(target_os = "linux")]
pub mod freeze;
pub mod imports;
pub mod install;
#[cfg(target_os = "linux")]
pub mod live;
pub mod memory;
pub mod requirement;
#[cfg(target_os = "linux")]
pub mod scenario;
#[cfg(target_os = "linux")]
pub mod snapshot;
pub mod timer;

use memory::FakeProcess;
use timer::Timer;

/// Everything the fake runtime knows, and everything it recorded.
#[derive(Default)]
pub struct World {
    pub processes: Vec<FakeProcess>,
    pub timer: Timer,
    /// Every `runtime_print_message`, in order. This is the splitter's log.
    pub log: Vec<String>,
    /// The most recent `runtime_set_tick_rate`, which the splitter changes
    /// between its attached and detached rates.
    pub tick_rate: Option<f64>,
    /// What the runtime reports for `get_os` / `get_arch`.
    pub os: String,
    pub arch: String,
    /// Settings the test is forcing, by key. Anything absent takes the value
    /// the splitter declared as its default.
    pub settings: HashMap<String, bool>,
    /// Keys the splitter registered, in registration order, with their
    /// declared defaults.
    pub registered_settings: Vec<(String, bool)>,

    /// Attached process handles. Index + 1 is the handle; `None` once detached.
    pub(crate) attached: Vec<Option<usize>>,
    /// Live `SettingValue` handles.
    pub(crate) values: HashMap<u64, bool>,
    pub(crate) next_value: u64,
}

impl World {
    pub fn new() -> Self {
        Self {
            os: "linux".into(),
            arch: "x86_64".into(),
            next_value: 1,
            ..Default::default()
        }
    }

    pub fn with_process(mut self, process: FakeProcess) -> Self {
        self.processes.push(process);
        self
    }

    /// Forces a setting, overriding the default the splitter declares.
    pub fn with_setting(mut self, key: impl Into<String>, value: bool) -> Self {
        self.settings.insert(key.into(), value);
        self
    }

    /// Whether any logged line contains `needle`. The splitter's observable
    /// behaviour is mostly its log, so most assertions go through this.
    pub fn logged(&self, needle: &str) -> bool {
        self.log.iter().any(|line| line.contains(needle))
    }

    pub(crate) fn process(&self, handle: u64) -> Option<&FakeProcess> {
        self.processes.get(self.process_index(handle)?)
    }

    pub(crate) fn process_index(&self, handle: u64) -> Option<usize> {
        *self.attached.get(handle.checked_sub(1)? as usize)?
    }
}

thread_local! {
    static WORLD: RefCell<Option<World>> = const { RefCell::new(None) };
}

/// Runs `f` against the installed world.
///
/// # Panics
///
/// If called outside [`drive`]. That means the splitter reached the runtime
/// without a world to answer it, which is a bug in the test rather than
/// something to paper over with a default.
pub(crate) fn with_world<T>(f: impl FnOnce(&mut World) -> T) -> T {
    WORLD.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let world = borrow.as_mut().expect(
            "the auto splitting runtime was called with no world installed; \
             the splitter must be driven through test_harness::drive",
        );
        f(world)
    })
}

/// Uninstalls the world even if the splitter panics, so one failing test
/// cannot leave a world behind for the next one on the same thread.
struct Installed;

impl Drop for Installed {
    fn drop(&mut self) {
        WORLD.with(|cell| cell.borrow_mut().take());
    }
}

/// Polls `future` up to `ticks` times against `world`, and gives the world back.
///
/// One poll is one tick: `asr`'s `next_tick()` yields exactly once, so a single
/// poll advances the splitter from one `next_tick().await` to the next --
/// the same unit the real runtime drives it in, and the unit every tick-count
/// constant in the splitter is written against.
///
/// Returns early if the future completes, which for the splitter's `main` means
/// it fell out of its loop -- an outcome worth asserting on rather than hiding.
pub fn drive<F: Future<Output = ()>>(world: World, future: F, ticks: usize) -> World {
    drive_with(world, future, ticks, |_, _| true)
}

/// Drives the splitter, calling `after_tick` between polls with the world as it
/// stands. Returning false stops.
///
/// This is what a recorder watches through: the splitter's log and timer calls
/// are the only outward sign of what it has decided, and a moment worth
/// capturing is a moment it just did something. The hook runs while the world
/// is still installed, so it can take as long as it likes -- a capture takes
/// seconds -- without the splitter observing the pause.
pub fn drive_with<F: Future<Output = ()>>(
    world: World,
    future: F,
    ticks: usize,
    mut after_tick: impl FnMut(usize, &World) -> bool,
) -> World {
    WORLD.with(|cell| *cell.borrow_mut() = Some(world));
    let _installed = Installed;

    // Scoped so the future is dropped while the world is still installed.
    // `asr::Process` detaches on drop, so tearing the world down first turns
    // every attached run into a `process_detach` with nothing to answer it --
    // and a panic crossing `extern "C"` aborts rather than failing the test.
    {
        let mut future = pin!(future);
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        for tick in 0..ticks {
            if future.as_mut().poll(&mut context).is_ready() {
                break;
            }
            if !WORLD.with(|cell| {
                let mut borrow = cell.borrow_mut();
                let world = borrow.as_mut().expect("the world is installed");
                after_tick(tick, world)
            }) {
                break;
            }
        }
    }

    WORLD
        .with(|cell| cell.borrow_mut().take())
        .expect("the world was removed while the splitter was running")
}

fn noop_waker() -> Waker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    // SAFETY: The vtable's operations are all no-ops on a null pointer that is
    // never dereferenced.
    unsafe { Waker::from_raw(RawWaker::new(ptr::null(), &VTABLE)) }
}
