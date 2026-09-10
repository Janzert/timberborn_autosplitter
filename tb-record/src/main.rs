//! Records a scenario against the live game. See `linux.rs`.
//!
//! Linux only: it reads the game through `/proc`, and the capture format sits
//! on that. Elsewhere this builds to a `main` that says so, only so
//! `cargo --workspace` compiles.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("tb-record: Linux only -- it reads the game through /proc.");
    std::process::ExitCode::FAILURE
}
