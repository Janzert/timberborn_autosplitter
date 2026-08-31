# Timberborn Auto Splitter

A LiveSplit auto splitter for
[Timberborn](https://store.steampowered.com/app/1062090/Timberborn/), built for
the sandboxed WebAssembly auto splitting runtime. This work was inspired by
[MHVandborg's autosplitter](https://github.com/MHVandborg/timberborn_speedrun/tree/master)
which uses a game mod to collect the split information.

For this autosplitter **nothing runs inside the game.** It reads state directly
from the game's memory, so it works on a stock, unmodified game.

## Status

All seven splits defined by MHVandborg's splitter work, new game start to the
"Congratulations!" screen. Verified end to end on both factions: a complete
Folktails run, and every split condition on Iron Teeth — the buildings in build
order, the research unlock, and the wonder activating and then completing as two
separate events:

| Split | Fires when |
|---|---|
| *(run start)* | the overlay appears after naming the settlement |
| Forester | a Forester's Hut is built |
| Gear Workshop | a Gear Workshop is built |
| Tapper's Shack | a Tapper's Shack is built |
| Observatory / Numbercruncher | the faction's advanced science building is built (Observatory for Folktails, Numbercruncher for Iron Teeth) |
| Smelter + Wood Workshop | both are built, in either order |
| Wonder Research | the faction's wonder research is completed |
| Congratulations screen *(run end)* | the Congratulations screen appears |

Buildings are found through the global entity registry rather than through the
districts' own finished-building registries, which turned out not to list every
building — see [docs/DESIGN.md](docs/DESIGN.md). A building is picked up when it
is placed and its split fires on the tick it finishes.

Both factions are covered: where they build different things — the advanced
science building and the wonder itself — one split covers both, and only the
one belonging to the faction being played can fire. Loading a save never splits
on already completed items: every condition fires only on a change actually
observed, so a save with the research already done or the buildings already up
stays quiet.

Two things worth knowing:

- **The run end is the wonder *completing*, not activating.** Those are about
  0.5 in-game hours apart — roughly 9.6 seconds of real time at 1x — and the
  category rules end the run at the Congratulations screen.
- **Splits fire in whatever order the player achieves them**, which need not
  match the order in a `.lss` file. In testing, Research came before Smelter +
  Wood Workshop.

A full run has been verified in classic LiveSplit itself, not just
[asr-debugger](https://github.com/LiveSplit/asr-debugger): LiveSplit 1.8.29
running inside Timberborn's own Proton prefix split correctly all the way to the
Congratulations screen. See [../livesplit/](../livesplit/) for that setup.

Still to do: submitting to the auto splitter index, and testing on Windows —
development has been on Linux under Proton throughout.

See [docs/DESIGN.md](docs/DESIGN.md) for how it works and what has been
measured.

## Building

```bash
git clone --recurse-submodules <this repo>
cargo build --release
```

The output is `target/wasm32-unknown-unknown/release/timberborn_autosplitter.wasm`.
Point LiveSplit's Auto Splitting Runtime component at it, or use
[asr-debugger](https://github.com/LiveSplit/asr-debugger) while developing.

If you cloned without `--recurse-submodules`:

```bash
git submodule update --init --recursive
```

## Layout

| Path | |
|---|---|
| `src/` | the auto splitter |
| `vendor/asr` | submodule; see [docs/ASR_FORK.md](docs/ASR_FORK.md) |
| `devtools/` | **development only** — never shipped, never run during a run |

## devtools

`devtools/` holds a Timberborn mod used as a *test oracle* when adding support
for a new game version: it dumps ground-truth pointers, field offsets and a
golden trace of a run, which the splitter's own results are diffed against.

It is a build-time tool. It is never distributed to runners and must never be
loaded during a run.
