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
same user. Only **one** small program needs them:

```bash
cargo install --path tb-ptrace-open --root ~/.local \
  && sudo setcap cap_sys_ptrace+ep ~/.local/bin/tb-ptrace-open
```

`tb-dump` and `tb-record` need no privilege of their own. When a direct open is
refused they run `tb-ptrace-open`, which opens `/proc/<pid>/mem` and passes the
descriptor back over an inherited socket. `/proc/<pid>/mem` is
permission-checked when it is *opened*, not on each read -- verified on this
kernel rather than assumed -- so the descriptor keeps working in a process with
no rights at all.

The point of the split is that a capability is an attribute of a file and is
lost on every rebuild. `tb-dump` and `tb-record` change constantly; the helper
does almost nothing and depends on nothing but libc, so it should need
re-granting only when its validation rule changes. It also means development
binaries are not run with more privilege than they need.

`tb-ptrace-open` does its own check and refuses any process with no Timberborn
data mapped, so a mistyped `--pid` cannot read something else. That check lives
there rather than in the callers because it is the piece holding the capability,
and a check in the caller is one whoever invokes it can skip.

Install the tools themselves however you like -- `cargo dump` and
`cargo record` run them straight from the repo:

```bash
cargo install --path test-harness --bin tb-dump --root ~/.local
cargo install --path tb-record --root ~/.local
```

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

## Recording a scenario

A single capture is one instant. It can say *the wonder was already unlocked*,
never *the split fired when it became unlocked* -- a split is a change, and one
frame has none. `tb-record` records the changes:

```bash
tb-record --state wonder-run --notes "folktails, developer mode"
```

It drives the real splitter against the live game through the same `Memory`
seam the tests use, and every time the splitter touches the timer -- starts,
splits, resets -- it stops the game and captures. There is no separate list of
things to watch for: the splitter's own decisions are the definition, so a
recording cannot drift out of step with what it actually does.

Each step is a delta against the one before, so a run costs one full capture and
a few hundred MiB a split rather than 5 GiB apiece. Each is written and closed
as it is taken, so stopping early -- Ctrl-C, or closing the game -- leaves a
shorter scenario rather than a broken one.

A moment is any tick in which the splitter said something that was not routine
progress, as well as any tick in which it touched the timer. Timer events alone
are not enough, and that cost a recorded run to learn: they are the splitter's
*conclusions*, and its correctness depends on the states it passes through to
reach them. The run start is only bound while the scene is still loading, and
the timer only starts when `initializationState` reaches `ShowUI` having been
watched from below. A recording that jumped from the main menu to "the overlay
is up" showed a game already in progress, and the splitter correctly refused to
start a timer -- reproducing, from a recording of a perfectly good run, a bug
that never happened.

Replay serves the steps in order, advancing between ticks, so the splitter sees
the world change. That substitution is closer to honest than it sounds: every
step is a capture of the same process, and Unity's Mono uses the Boehm
collector, which does not move objects. An object has the same address in every
step it is alive for, so swapping step *n* for *n+1* shows the splitter the same
addresses holding their new values -- what actually happened, minus the time in
between.

### One recording, one directory

A recording is a directory of steps rather than sixty directories sharing a
prefix:

```
snapshots/
  1.1.2.4-52e959e-sw-main-menu/            a single capture
  1.1.2.4-52e959e-sw-run-complete-frozen/  a single capture
  1.1.2.4-52e959e-sw-wonder-run/           a recording
    step00/
    step01/
    ...
  1.1.2.4-52e959e-sw-wonder-run-ironteeth/
    step00/
    ...
```

So a recording moves, is deleted, or is copied to another machine as one thing,
and a store holding a few of them still reads as a handful of entries. Which
kind a directory is needs no naming convention and no guessing: a capture is the
one with a `manifest.txt` in it, and the search looks one level down for the
rest.

A store written before this still reads. Its steps sit at the top level, where
they are found as single captures -- which is what they are to everything above
the search anyway. Nothing has to be migrated, and a store holding both layouts
is fine.

### More than one recording

The useful case, not an awkward one, and for two reasons.

A run as Folktails and one as Iron Teeth are the same category down different
code, since the splitter matches faction-suffixed template names. And the same
category recorded against two game builds is what says the splitter survived an
update, which is the claim the whole design rests on.

Steps are grouped by **game version and label together**, so neither can splice
two recordings into one nonsense run. That grouping is not cosmetic: a label
defaults to its state, so recording `--state wonder-run` on each of two builds
gives two sets of steps sharing one label, and grouping by label alone would
produce a "run" that starts twice and changes build half way through. The tests
replay every recording rather than the first.

### What the freezing costs

Each step stops the game for a second or three, so a full run is twenty-odd
brief freezes. That is noticeable while playing and is the price of each step
being a single instant.

It was briefly suspected of crashing the game's display after a recorded run.
It was not: this machine has a standing problem with that, unrelated to
anything here. Recorded so the suspicion is not formed again from the same
coincidence.

A recording with a gap in its steps is refused rather than replayed: a missing
step would show the splitter a jump that never happened.

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
