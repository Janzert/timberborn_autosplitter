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
cargo snapshot-tests    # replays real captured memory: ~3 minutes
```

**The offline suite gates commits.** It builds a synthetic Mono process out of
the layout facts in [`fixtures/`](../fixtures/README.md) — assemblies, images,
class caches, classes, fields, vtables — and drives the real splitter against
it. It covers a whole wonder run as both factions, the mid-run attach, and the
edges of a session: the game starting after the splitter, closing under it,
dying slowly, or being the second game of the evening. It runs on a machine that
has never had Timberborn installed, which is why CI can run it.

**The snapshot suite is the oracle**, not the deliverable. It replays real
captured game memory from [`snapshots/`](../snapshots/README.md), including
whole runs recorded split by split, and — in `tests/fixture_vs_snapshot.rs` —
compares the synthetic world against a capture class by class. It needs
captures this repository cannot ship, so it is behind a Cargo feature and CI
does not run it.

Both are kept deliberately. A fixture and the builder can agree with each other
perfectly while both being wrong about Timberborn.

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

**It does not catch anything about timing** — the 29s Windows heap scan, ~30µs
reads through Wine, `MAP_BUDGET` stutter — **or the Proton prefix and
start-order constraints.** Those need the real thing; see
[DESIGN.md](DESIGN.md) under *Measurements*.

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
- **The offline suite gated commits from day one**, when it held three tests.
  Snapshot tests are development scaffolding with a machine-local dependency;
  they are not the product.

## Risks that are still live

- **A separate suite is a forgettable suite.** `cargo test` will not mention it.
  It is now ~3 minutes, up from 54s, because three recordings are replayed
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

- **How much does a capture compress?** Three recordings and four single
  captures come to ~47 GB uncompressed, which is what sets the budget for how
  many builds and scenarios can be kept.
- **Retiring the duplicated snapshot assertions.** The offline suite now covers
  what `snapshot_run.rs` and `snapshot_attach.rs` assert, and both the layouts
  and the scenarios have agreed across a game build change — so pruning them is
  available, and the suite's runtime is an argument for it. It is still a
  deletion of the oracle's coverage, so it wants taking deliberately rather than
  drifting into. `fixture_vs_snapshot.rs` should stay for as long as captures do.
- **A timer state the splitter did not cause.** Every split is gated on
  `timer::state() == Running` and the late-bind warning on `NotRunning`, and
  nothing exercises the splitter reading a state a runner set by hand. Note this
  is *not* reset behaviour: the splitter never calls `timer::reset()`.
