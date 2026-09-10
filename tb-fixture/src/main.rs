//! Writes a fixture from an install and a snapshot. See `linux.rs`.
//!
//! Linux only, for now: it reads a snapshot, and the snapshot format is gated
//! with the live reader it sits on, not for anything of its own. Elsewhere this
//! builds to a `main` that says so, only so `cargo --workspace` compiles.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("tb-fixture: Linux only -- snapshots are, for now.");
    std::process::exit(1);
}
