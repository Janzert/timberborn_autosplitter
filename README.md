# Timberborn Auto Splitter

A LiveSplit auto splitter for
[Timberborn](https://store.steampowered.com/app/1062090/Timberborn/), built for
the sandboxed WebAssembly auto splitting runtime. This work was inspired by
[MHVandborg's autosplitter](https://github.com/MHVandborg/timberborn_speedrun/tree/master)
which uses a game mod to collect the split information.

For this autosplitter **nothing runs inside the game.** It reads state directly
from the game's memory, so it works on a stock, unmodified game.

## Status

Currently this is barely beyond prototype stage in a minimally working state.
Any feedback would certainly be appreciated, but be cautious if using this in
an actual run.

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

Both factions are covered: where they have faction specific buildings — the
advanced science building and the wonder itself — one split covers both, and
only the one belonging to the faction being played can fire.

One LiveSplit limitation worth noting: **Splits fire in whatever order the
player achieves them**, which need not match the order in a `.lss` file. So an
out of order split will get attributed to the wrong item.

See [docs/DESIGN.md](docs/DESIGN.md) for how it works and what has been
measured.

## Setup

No build needed to try it — the release carries a prebuilt module, and
[`examples/`](examples/) has a splits file and a layout to start from.

1. Download `timberborn_autosplitter.wasm` from the
   [latest release](https://github.com/Janzert/timberborn_autosplitter/releases/latest),
   and the two example files:
   [the splits](<examples/Timberborn - Wonder (Earth Recultivator).lss>) and
   [the layout](examples/Timberborn.lsl).
2. In LiveSplit, right-click → **Open Splits** → **From File...** and pick the
   `.lss`. Then right-click → **Open Layout** → **From File...** and pick the
   `.lsl`, which already has the **Auto Splitting Runtime** component in it.
3. Right-click → **Edit Layout...** → **Layout Settings** → the **Auto
   Splitting Runtime** tab. Use **Browse...** next to **Script Path** to pick
   the `.wasm` you downloaded.
4. The individual splits appear as checkboxes below it and can be turned off
   there. **Save Layout** and **Save Splits** when done.

The layout ships with **Script Path** deliberately empty, because the path is
stored in the layout and only you know where you put the file. For the same
reason, moving the `.wasm` afterwards breaks it until you browse to it again. A
relative path is resolved against LiveSplit's own working directory rather than
the layout's, so it only works if the `.wasm` sits next to `LiveSplit.exe`.

The Auto Splitting Runtime component ships with LiveSplit itself — this was
tested against 1.8.29 — so there is nothing else to install, and nothing is
added to the game.

### Setting it up by hand

If you would rather build the layout yourself: right-click → **Edit Layout...**
→ `+` → **Control** → **Auto Splitting Runtime**, then step 3 above.

The example splits are named for a Folktails route and carry the buildings'
icons. Order the seven splits to match the route **you** run rather than the
order they are listed in: each fires when you achieve it, so a `.lss` ordered
the way you actually play keeps every split attributed to the right segment.

On Linux, LiveSplit has to run **inside the game's Proton prefix**: reads of
another process's memory are served by that prefix's `wineserver`, which only
knows the processes belonging to it, so a LiveSplit in a prefix of its own can
see the game but never read it.

## Building

Only needed to work on it — see [Setup](#setup) to just use it.

```bash
git clone --recurse-submodules https://github.com/Janzert/timberborn_autosplitter.git
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

## License

MIT — see [LICENSE](LICENSE). The vendored asr submodule is separately
licensed; see `vendor/asr`.
