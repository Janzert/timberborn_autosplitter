# Timberborn Auto Splitter

A LiveSplit auto splitter for [Timberborn](https://store.steampowered.com/app/1062090/Timberborn/),
built for the sandboxed WebAssembly auto splitting runtime.

**Nothing runs inside the game.** It reads state directly from the game's
memory, so it works on a stock, unmodified install and is usable for runs
submitted to speedrun.com.

## Status

All seven splits work, verified across a complete run from a new game to the
"Congratulations!" screen:

| Split | Fires when |
|---|---|
| *(run start)* | the overlay appears after naming the settlement |
| Forester | the Forester's Hut is finished |
| Gear Workshop | the Gear Workshop is finished |
| Tapper's Shack | the Tapper's Shack is finished |
| Observatory | the Observatory is finished |
| Smelter + Wood Workshop | both are finished, in either order |
| Research | the faction's wonder becomes researchable |
| *(run end)* | the wonder completes and the Congratulations screen appears |

Both factions are covered. Loading a save never splits: every condition fires
only on a change actually observed, so a save with the research already done or
the buildings already up stays quiet.

Two things worth knowing:

- **The run end is the wonder *completing*, not activating.** Those are about
  0.5 in-game hours apart — roughly 9.6 seconds of real time at 1x — and the
  category rules end the run at the Congratulations screen.
- **Splits fire in whatever order the player achieves them**, which need not
  match the order in a `.lss` file. In testing, Research came before Smelter +
  Wood Workshop.

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
