//! Phase 0 only: proves the crate compiles for a native target and that the
//! `rlib` is consumable from a test binary.
//!
//! It deliberately does not *call* into the splitter. Every entry point reaches
//! `asr`'s runtime imports, which are undefined symbols off wasm until the test
//! harness supplies them; standing those up is phase 1. Nothing here should be
//! read as evidence that the splitter runs natively yet -- referencing `main`
//! as a value links the async shim, not its body.

#[test]
fn crate_is_usable_from_a_native_test() {
    let entry: fn() -> _ = timberborn_autosplitter::main;
    let _ = &entry;
}
