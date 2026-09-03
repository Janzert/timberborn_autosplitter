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
cargo install --path test-harness --bin tb-dump --root ~/.local \
  && sudo setcap cap_sys_ptrace+ep ~/.local/bin/tb-dump
```

Then run `tb-dump` rather than `cargo dump`. The capability is an attribute of
the file, so it survives reboots and applies to nothing else on the system --
but it is lost whenever the binary is rebuilt, so **a reinstall needs the
setcap again** -- which is why the two are written above as one command.

`cap_sys_ptrace` would let that binary read any process you own, so it refuses
any process with nothing mapped from the Timberborn install directory. That is
a guard against a mistyped `--pid`, not a security boundary.

Snapshots land here by default. `TIMBERBORN_SNAPSHOTS` moves them, which is
worth doing if this disk is small.

## Consistency

By default a capture walks the ranges of a process that keeps running, so the
last range is seconds younger than the first and the result can hold a
combination of values the game never had at one moment.

`--freeze` closes that: the game is stopped with `SIGSTOP` for the whole read
and resumed afterwards, making the capture a real instant. The cost is that the
game is frozen for as long as the capture takes -- about 6s for 5 GiB -- so it
is not something to do during a run that matters. Either way the manifest
records which it was, since a snapshot that cannot say is one nobody can trust
later.

Measured once, on a finished run sitting idle: a frozen and an unfrozen
capture of the same state drove the splitter to **identical conclusions**, all
50 log lines matching once addresses and counts are blanked
(`tests/snapshot_compare.rs`). So tearing is not currently affecting anything
the splitter reads. That state was idle, though, with nothing being built and
no simulation pressure -- it says nothing about a capture taken during active
play, which has far more opportunity to tear.

`SIGSTOP` rather than `ptrace` because the game has over a hundred threads and
ptrace stops them one at a time; rather than the cgroup v2 freezer because
Steam leaves the game in the login session's own scope, shared with the desktop
and with the terminal running the capture.

If the capture is interrupted it resumes the game on the way out, including on
Ctrl-C. The one hole is `SIGKILL`, which runs nothing -- so the resume command
is printed *before* stopping rather than after:

```bash
kill -CONT <pid>
```

## What a snapshot is not

It is one frame, not a run.

Asserting that a split *fires* needs before-and-after states, which a single
capture cannot provide -- that is what the recorded read traces of phase 3 are
for.

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
cargo snapshot-tests
```

## Format

A directory per capture, named `<build-id>-<label>`, holding:

- `manifest.txt` -- provenance and the range table, one record per line
- `memory.bin` -- the captured bytes, concatenated in manifest order

A range that was listed but could not be read is recorded with `-` in place of
its offset, so replay can say "unreadable" rather than "absent".
