//! Passing an open descriptor between processes.
//!
//! The premise the privilege split rests on: `/proc/<pid>/mem` is
//! permission-checked when opened, so a descriptor opened by
//! `tb-ptrace-open` -- the one binary holding a capability -- keeps working in
//! a tool that has none.
//!
//! These test the plumbing, not the premise, and do it against an ordinary file
//! so they need no privilege and no running game. SCM_RIGHTS mistakes are
//! silent, which is why they are worth a test at all.

use std::{io::Read, process::Command};

/// The stand-in helper: the same protocol, none of the privilege.
const FIXTURE: &str = env!("CARGO_BIN_EXE_fd-source");

#[test]
fn takes_a_descriptor_opened_by_another_process() {
    let path = std::env::temp_dir().join(format!("tb-fd-{}", std::process::id()));
    std::fs::write(&path, b"handed over").unwrap();

    let mut file = test_harness::live::spawn_and_receive(
        &mut Command::new(FIXTURE),
        &[path.to_string_lossy().into_owned()],
    )
    .expect("receiving the descriptor");

    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "handed over");

    std::fs::remove_file(&path).unwrap();
}

/// A helper that failed must not look like one that succeeded. Its socket
/// closes either way, and a closed socket reads as EOF -- which is easy to
/// mistake for a valid but empty reply.
#[test]
fn reports_a_helper_that_failed() {
    let error =
        test_harness::live::spawn_and_receive(&mut Command::new(FIXTURE), &["--fail".into()])
            .expect_err("a failing helper should not yield a descriptor");
    assert!(
        error.to_string().contains("helper exited"),
        "unhelpful error: {error}"
    );
}

/// The normal case on a machine that has not installed the helper, so it has to
/// be an error rather than a panic.
#[test]
fn reports_a_helper_that_is_not_there() {
    let error = test_harness::live::spawn_and_receive(
        &mut Command::new("tb-ptrace-open-that-does-not-exist"),
        &["1".into()],
    )
    .expect_err("a missing helper should be an error");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
