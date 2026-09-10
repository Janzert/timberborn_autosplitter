//! The shipped artifact: the splitter, wrapped as the module the auto splitting
//! runtime loads. Everything it does is in the parent crate; this only exports
//! `update` and supplies what a `no_std` wasm module needs to exist.
//!
//! Empty anywhere but wasm, so `cargo --workspace` can build it for the host
//! without a runtime to link against. See Cargo.toml.
#![cfg(target_family = "wasm")]
#![no_std]

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

use splitter::main;

asr::async_main!(stable);
asr::panic_handler!();
