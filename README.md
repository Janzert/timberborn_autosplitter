# Timberborn Auto Splitter

A LiveSplit auto splitter for [Timberborn](https://store.steampowered.com/app/1062090/Timberborn/),
built for the sandboxed WebAssembly auto splitting runtime.

Unlike the mod-based splitter it replaces, **nothing runs inside the game**. It
reads game state directly from process memory, so it is usable for runs
submitted to speedrun.com, which does not allow mods.

## Status

Early. The scaffolding attaches to the process and the Mono runtime; service
location and the splits themselves are not implemented yet. See
[docs/DESIGN.md](docs/DESIGN.md).

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
