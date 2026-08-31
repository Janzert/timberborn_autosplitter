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
"Congratulations!" screen. Verified on both factions: a complete Folktails run,
and every split condition on Iron Teeth:

| Split | Fires when |
|---|---|
| *(run start)* | the overlay appears after naming the settlement |
| Forester | a Forester is built |
| Gear Workshop | a Gear Workshop is built |
| Tapper's Shack | a Tapper's Shack is built |
| Observatory / Numbercruncher | the faction's advanced science building is built (Observatory for Folktails, Numbercruncher for Iron Teeth) |
| Smelter + Wood Workshop | both are built, in either order |
| Wonder Unlocked | the faction's wonder is unlocked in the science tree |
| Congratulations screen *(run end)* | the Congratulations screen appears |

Both factions are covered: where they build different things — the advanced
science building and the wonder itself — one split covers both, and only the
one belonging to the faction being played can fire.

One LiveSplit limitation worth noting: **Splits fire in whatever order the
player achieves them**, which need not match the order in a `.lss` file. So an
out of order split will get attributed to the wrong item.

See [docs/DESIGN.md](docs/DESIGN.md) for how it works and what has been
measured.

## Building

```bash
git clone --recurse-submodules <this repo>
cargo build --release
```

If you cloned without `--recurse-submodules`, you will need to get the
submodule with:

```bash
git submodule update --init --recursive
```

The output is `target/wasm32-unknown-unknown/release/timberborn_autosplitter.wasm`.
Point LiveSplit's Auto Splitting Runtime component at it, or use
[asr-debugger](https://github.com/LiveSplit/asr-debugger) while developing.

## Layout

| Path | |
|---|---|
| `src/` | the auto splitter |
| `vendor/asr` | submodule; see [docs/ASR_FORK.md](docs/ASR_FORK.md) |
| `devtools/` | offline development tooling — never shipped, never runs against the game |

## devtools

`devtools/metadata.py` reads .NET metadata straight out of the game's
assemblies — no mod, no running game, no mono or ilspy, just the ECMA-335
tables parsed directly.

```bash
./devtools/metadata.py check ~/.steam/steam/steamapps/common/Timberborn/Timberborn_Data/Managed
```

That checks every class and field name `src/probe.rs` depends on against an
install, which is the offline half of the version check — the fast answer to
"did an update rename something", with the game closed. `dump <assembly.dll>`
lists every class and field in an assembly, which is how the split sources in
[docs/DESIGN.md](docs/DESIGN.md) were found.

Nothing here is distributed to runners or touches a running game. See
[devtools/README.md](devtools/README.md).
