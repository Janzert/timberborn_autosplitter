# Testing

The splitter has almost no pure logic. `SceneLoad::pending`, `Buildings::poll`,
`RunStart::bind` and `WonderCompletion::finished` all take a `&Process` and read
memory directly, so for most of this project's life every behaviour was verified
by launching the game and playing — a twenty-minute session per iteration, and
one each for the awkward cases: a second run in the same session, attaching
mid-run, a scene change while a scan is in flight.

The flip side of that same fact was the opportunity. *Because* everything goes
through `Process`, faking the memory makes the whole codebase testable with no
refactoring at all. There is one seam, and it is underneath everything.

```
      the splitter (unchanged)
              │
         asr::Process
              │
   test-harness: 71 extern "C" stubs
              │
        Memory (the seam)
         ╱          ╲
   Snapshot        Fixture
   captured        committed facts
   game memory     → synthesised heap
```

## Two suites

```bash
cargo test              # the offline suite: no game, no capture, ~0.5s
cargo snapshot-tests    # replays real captured memory: ~2 minutes
```

**The offline suite gates commits.** It builds a synthetic Mono process out of
the layout facts in [`fixtures/`](../fixtures/README.md) — assemblies, images,
class caches, classes, fields, vtables — and drives the real splitter against
it. It covers a whole wonder run as both factions, the mid-run attach, the timer
states a runner sets by hand rather than the splitter causing, and the edges of
a session: the game starting after the splitter, closing under it, dying
slowly, or being the second game of the evening. It runs on a machine that
has never had Timberborn installed, which is why CI can run it.

**The snapshot suite is the oracle**, not the deliverable. It replays real
captured game memory from [`snapshots/`](../snapshots/README.md), including
whole runs recorded split by split, and — in `tests/fixture_vs_snapshot.rs` —
compares the synthetic world against a capture class by class. It needs
captures this repository cannot ship, so it is behind a Cargo feature and CI
does not run it.

Both are kept deliberately. A fixture and the builder can agree with each other
perfectly while both being wrong about Timberborn.

The synthetic world models the runtime's reference table as well as the heap —
every object placed gets an entry, in a range of its own holding pointers and
nothing else. `Scene::container_unreferenced` leaves the DI container out of it
while leaving the object on the heap, which is the one discrepancy a capture
actually showed and the reason the sweep behind the table is tested rather than
assumed. See [DESIGN.md](DESIGN.md) under *Reading fewer bytes*.

## What this catches, and what it does not

Worth being explicit, because a harness like this is easy to oversell.

**It catches** lifecycle and state-machine bugs — the cold two-cycle case,
mid-run attach, rescan-after-scene-change, the `SKIP_GIVE_UP` counter that once
locked out the only clock there was. It lets error paths be exercised at all: a
renamed class, a container over `MAX_SINGLETONS`, the collections fallback that
a whole platform pass could not provoke even once.

**It does not catch a wrong belief about the game.** A fixture is built from
what we think the layout is, so a bug of the form "we misunderstood the game"
tests green — the districts' finished-building registries not listing every
building, `TributeToIngenuity` not being the Iron Teeth wonder, Mono's
lazily-filled field tables. By this project's own record those are the expensive
ones. That is the entire reason the snapshot suite exists and is checked
against.

**It caught one thing nobody was looking for.** Replaying a recording means
reading through a chain of twenty delta captures, which puts the harness twenty
stack frames deeper than a single capture does — deep enough to land on a buffer
asr's signature scanner had left dangling on the stack after the function that
declared it had returned. The scan then wrote 256 bytes of the game's code over
whatever was there, which on a shallower chain was nothing and on this one was a
return address. Fixed in the vendored fork; see [ASR_FORK.md](ASR_FORK.md).
Worth recording because the harness found it by *being* an unusual caller, which
is not something either suite was designed to do.

**It does not catch anything about timing** — the 29s Windows heap sweep, ~30µs
reads through Wine, slice stutter — **or the Proton prefix and start-order
constraints.** Those need the real thing; see [DESIGN.md](DESIGN.md) under
*Measurements*. What it does catch is which *path* answered a question, which
is the next best thing: both suites assert that the container is resolved
through the reference table rather than by sweeping for it, so a change that
quietly falls back shows up as a failure rather than as a slow run nobody
measures.

So it substitutes for the *repeat* runs, not the first one: fast iteration and a
regression net, not correctness assurance.

## Decisions, and why

So they are not re-litigated.

- **Not built on `livesplit-auto-splitting`.** Its `Process` is hardwired to
  `read-process-memory` and `proc-maps` against real OS pids, with no trait to
  substitute. Forking it would buy nothing.
- **Not built on wasmtime either.** asr has no native or mock mode, so its
  imports are undefined symbols on a native build — but supplying them as
  `#[no_mangle]` stubs in a test-only crate is mechanical, and then the splitter
  builds as a native `rlib` and runs under plain `cargo test`, with a debugger
  and `println!`. The tests exercise native codegen rather than the shipped
  `.wasm`, which is an acceptable trade: this crate has no wasm-specific
  behaviour beyond the allocator.
- **Two suites split by Cargo feature, not by `#[ignore]`.** A self-skipping
  test reports green for work it did not do, which is worse than not having it.
  `required-features = ["snapshot-tests"]` means `cargo test` does not compile,
  run, or print a line about them.
- **Snapshot test *code* is committed**, behind the gate. Only the multi-gigabyte
  blob is uncommittable, and anyone who captures their own should be able to run
  them.
- **The harness crate is shared by both suites.** The split is at the test
  targets; duplicating the driver is how the two would drift.
- **Snapshots live in this repo, gitignored**, rather than somewhere beside it.
  The store belongs with the code that reads it, and a repo-relative default
  path works for anyone who clones.
- **Tests ask for a *state*, never for a path.** A missing capture fails with
  the steps for producing it. Naming a file makes the suite unreproducible: the
  capture lives on one machine.
- **The store keeps recordings, not separate captures of the instants in them.**
  A `wonder-run` recording begins at the main menu and ends at the
  Congratulations screen by its own definition, so its first and last steps are
  captures of `main-menu` and `run-finished`, and the recorder marks them as
  such. Keeping single captures of those states as well costs gigabytes apiece
  for instants already held. The tests are unchanged: they ask for a state, and
  a state is what they get.
- **A recording exists for the case no run covers.** A `two-games` recording is
  a second game started in the same process, which is what a runner resetting
  for another attempt actually does, and the case any shortcut past a per-scene
  sweep has to survive. It is deliberately short — two overlays with a trip
  through the main menu between them, no buildings and no wonder — because
  what it is a recording *of* is the scene load, not the run.
- **The offline suite gated commits from day one**, when it held three tests.
  Snapshot tests are development scaffolding with a machine-local dependency;
  they are not the product.

## Risks that are still live

- **A separate suite is a forgettable suite.** `cargo test` will not mention it.
  It is now ~2 minutes, up from 54s, because four recordings are replayed
  rather than two — long enough that nobody will run it absent-mindedly. Keep a
  line for it wherever notes carry between sessions, or it becomes stale code
  nobody has run against a current capture. Partly mitigated: a missing capture fails with the steps for
  making one, so a stale suite says what it needs rather than just failing.
- **Snapshot-first could become snapshot-only.** Replay is cheap and real, and
  the incentive to stop there is strong. What guards against it is
  `fixture_vs_snapshot.rs`: the committable suite exists and is checked against
  the oracle.
- **Fixtures drift from the game.** Regenerating is cheap but needs both halves
  of the matching build present — the install and a `run-finished` capture of
  it. Old fixtures accumulate rather than being replaced: two builds under test
  is what makes the suite check the "resolved by name, so it survives updates"
  claim instead of asserting it.

## Where the detail lives

| | |
|---|---|
| [`fixtures/README.md`](../fixtures/README.md) | what a fixture is, how one is generated, what it deliberately does not record |
| [`snapshots/README.md`](../snapshots/README.md) | capturing memory, recording a run, the store's layout |
| `test-harness/src/fixture/build.rs` | how asr's model of Mono is synthesised, and the one row of offsets it needs |
| `test-harness/src/fixture/game.rs` | the Timberborn-shaped world built on top of it |
| [`ASR_FORK.md`](ASR_FORK.md) | the two accessors the vendored asr carries |

## Open questions

- **How much does a capture compress?** A recording of a whole run is tens of
  gigabytes uncompressed, which is what sets the budget for how many builds and
  scenarios can be kept.
- **Retiring the duplicated snapshot assertions.** The offline suite now covers
  what `snapshot_run.rs` and `snapshot_attach.rs` assert, and both the layouts
  and the scenarios have agreed across a game build change — so pruning them
  remains available. It stays available rather than taken: those are the
  oracle's reading of the run and the attach out of real memory. What the
  runtime argument actually bought was dropping duplicate *captures*, which cost
  gigabytes and held nothing the recordings do not.
