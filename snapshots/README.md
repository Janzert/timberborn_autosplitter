# Snapshots

A snapshot is a capture of Timberborn's memory taken from outside the running
game. Nothing here is committed: they run to gigabytes, and they are a copy of
the game's own data.

They are the **oracle**, not the deliverable. A synthesized fixture is built
from what we believe the game's layout to be, and cannot tell us that a belief
is wrong; a snapshot can, by being what the game actually had in memory. See
`TEST_HARNESS_PLAN.md` in the parent repository.

## Capturing one

The game must already be running.

```bash
tb-dump --dry-run
tb-dump --label folktails-loaded --notes "day 3, nothing built yet"
```

`cargo dump -- ...` runs it straight from the repo, which is convenient while
changing the tool but has no capability, so it can only report.

Reading another process's memory needs ptrace rights, which
`kernel.yama.ptrace_scope` withholds at its usual setting of `1` even from the
same user. The grant goes to an installed copy of the tool rather than to the
whole machine:

```bash
cargo install --path test-harness --bin tb-dump --root ~/.local
sudo setcap cap_sys_ptrace+ep ~/.local/bin/tb-dump
```

Then run `tb-dump` rather than `cargo dump`. The capability is an attribute of
the file, so it survives reboots and applies to nothing else on the system --
but it is lost whenever the binary is rebuilt, so **a reinstall needs the
setcap again**.

`cap_sys_ptrace` would let that binary read any process you own, so it refuses
any process with nothing mapped from the Timberborn install directory. That is
a guard against a mistyped `--pid`, not a security boundary.

Snapshots land here by default. `TIMBERBORN_SNAPSHOTS` moves them, which is
worth doing if this disk is small.

## Keep the assemblies too

A snapshot is only half of what a build needs. The other half is
`Timberborn_Data/Managed`, which is what `devtools/metadata.py` reads to
generate fixture facts. It is 47 MB against the snapshot's gigabytes:

```bash
cd ../../steam_versions && ./tbver.py save --managed-only --branch <build-id>
```

Snapshot plus assemblies is everything the harness will ever need for that
version, with no game install at all.

## Running the tests that use them

Snapshot tests are behind a feature so that `cargo test` neither compiles nor
counts them -- a test that silently skips reports green for work it did not do.

```bash
cargo test --features snapshot-tests --target x86_64-unknown-linux-gnu
```

## Format

A directory per capture, named `<build-id>-<label>`, holding:

- `manifest.txt` -- provenance and the range table, one record per line
- `memory.bin` -- the captured bytes, concatenated in manifest order

A range that was listed but could not be read is recorded with `-` in place of
its offset, so replay can say "unreadable" rather than "absent".
