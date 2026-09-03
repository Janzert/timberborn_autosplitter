# Snapshots

A snapshot is a capture of Timberborn's memory taken from outside the running
game. Nothing here is committed: they run to gigabytes, and they are a copy of
the game's own data.

They are the **oracle**, not the deliverable. A synthesized fixture is built
from what we believe the game's layout to be, and cannot tell us that a belief
is wrong; a snapshot can, by being what the game actually had in memory. See
`TEST_HARNESS_PLAN.md` in the parent repository.

## Tests ask for a state, not for a file

A test needs the game in a particular condition, not a particular capture on
one machine. So each test names a **state** — `main-menu`, `run-finished` — a
capture records which states it is of, and a test searches the store for one
that matches. Nothing searches on the directory name.

The consequence worth having: a missing capture fails with the steps for
producing it rather than with a path that means nothing to whoever hit it.

```text
No snapshot satisfies "run-finished" (a finished wonder run, Congratulations
screen already shown).

To make one:
  - Start a new game as either faction.
  - Build every split-triggering building: Forester, Gear Workshop, ...
  ...
  - then, with the game left in that state:
      tb-dump --freeze --state run-finished --notes '<what you did>'
```

The states are defined in `test-harness/src/requirement.rs`, which is also
where a new one is added. `tb-dump` refuses a `--state` that is not listed
there, so a typo cannot produce gigabytes that no test will ever look at.

## Capturing one

The game must already be running, and left in the state you are capturing.

```bash
tb-dump --dry-run
tb-dump --freeze --state run-finished --notes "folktails, developer mode"
```

Run `tb-dump --help` for the list of known states. A capture with no `--state`
is still written, and says so, but no test will find it.

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

Measured once, on a finished run sitting idle: a frozen and an unfrozen capture
of the same state drove the splitter to **identical conclusions**, all 50 log
lines matching once addresses and counts were blanked. So tearing was not
affecting anything the splitter read. That state was idle, though, with nothing
being built and no simulation pressure, and it says nothing about a capture
taken during active play -- where `--freeze` stops being optional.

The comparison was a one-off and its pair of captures has been deleted; this
paragraph is the result. What that pair also measured, and what matters for
scenarios, is how much of memory actually changes between two moments seconds
apart on an idle save:

| granularity | differs | records |
|---|---|---|
| 4 KiB pages | 278 MiB of 5132 (5.4%) | 71,255 |
| 64 KiB chunks | 757 MiB (14.7%) | 12,140 |
| 1 MiB chunks | 1310 MiB (25.5%) | 1,408 |

Only 244 of 2163 ranges were touched at all. A sequence of captures can
therefore be stored as one full capture plus deltas of a few hundred MiB, rather
than gigabytes apiece.

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

The manifest records `frozen yes|no` and one `satisfies <state>` line per state
the capture is of. Those two are what searching uses; everything else in it is
provenance.

A range that was listed but could not be read is recorded with `-` in place of
its offset, so replay can say "unreadable" rather than "absent".
