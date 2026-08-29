# Design

## Background

Autosplitting for Timberborn was established by MHVandborg's splitter
([timberborn_speedrun](https://github.com/MHVandborg/timberborn_speedrun)): a
game mod that exports live state to `autosplitter_state.json`, paired with an
ASL script that reads it. It works, and it did the genuinely hard part — working
out which in-game events the Wonder category should split on, and proving the
whole thing was worth having. This project would not exist without it.

We keep that split set exactly, in the same order, so existing `.lss` files stay
compatible.

The reason for a second implementation is a constraint rather than a
shortcoming: speedrun.com does not allow mods to be running for a submitted run.
Reading state out of the game's memory instead makes autosplitting available on
a stock install.

That constraint also rules out porting the existing design across as-is.
LiveSplit's sandboxed WebAssembly runtime has **no filesystem access** — it can
attach to a process, read its memory, control the timer, expose settings and
log, and that is all — so a file hand-off between game and splitter cannot be
reproduced there at any level of effort. State has to come from process memory.

## What the game gives us

Timberborn 1.x, Unity **6000.3.6f1** (Unity 6.3), **Mono** backend — not IL2CPP.
`Timberborn_Data/Managed/` contains 647 real .NET assemblies and
`MonoBleedingEdge/` is present.

This is the good case: asr's Mono support resolves classes and field offsets
**by name at runtime**, so there are no hardcoded struct offsets to maintain and
most game patches change nothing.

## The rooting problem

Timberborn uses Bindito for constructor-injection DI. Nothing is a static
singleton.

Parsing the CLI metadata of all 647 assemblies and decoding every field
signature turns up **exactly one** mutable reference-typed static field in the
entire game:

    Timberborn.ErrorReporting.WorldDataService.<Data>k__BackingField : ImmutableArray<T>

which holds save-file bytes for crash reports and is useless as an anchor.

There is therefore **no static root into the DI container**. This rules out
asr's `UnityPointer` helper, which resolves paths starting from a class's
static table.

## Approach: locate services by scanning the heap for their class

Every Timberborn service is a singleton, so there is exactly one instance of
each service class in the process.

On Mono, `object[0]` is a `MonoVTable*` and `vtable[0]` is a `MonoClass*`
(confirmed by asr's own `UnityPointer` traversal). So:

1. Resolve the target class by name via the Mono module to get its `MonoClass*`.
2. Find its `MonoVTable` (a vtable whose first pointer is that class).
3. Scan writable committed ranges from `Process::memory_ranges()` for a
   pointer-aligned word equal to that vtable address. That is the instance.
4. Cache it, revalidate cheaply each tick, rescan on scene change.

This needs **zero Unity-native offsets**, so it does not depend on asr's
`scene_manager` module working on Unity 6.3, and it stays name-resolved
end to end.

The scan runs once on attach (and on scene change), not per tick.

Unity ships Mono with the Boehm collector — `MonoBleedingEdge/EmbedRuntime/mono-2.0-bdwgc.dll`
— which **does not move objects**. So a located instance address stays valid for
the lifetime of that object, and a rescan is only needed when the object itself
is replaced, i.e. on a scene change. If Unity ever switched this build to SGen,
which is a compacting collector, cached pointers would need revalidating against
the vtable every tick.

### The asr change this needs

`mono::Class` keeps its address in a `pub(super)` field, so from outside the
crate there is no way to get a class identity handle to compare against.

The fix turned out smaller than expected. asr already computes the vtable
internally for static table lookups — `class.runtime_info` -> `domain_vtables[0]`
— so rather than exposing the raw class address, `vendor/asr` adds
`Class::get_vtable()`, which is exactly the value the scan compares against and
does not leak offset internals. That is a better upstreaming proposition too;
see `docs/ASR_FORK.md`.

## Split sources

All paths below were verified to exist in the shipped assemblies.

| Split | Path |
|---|---|
| Research Earth Recultivator | `BuildingUnlockingService._unlockedBuildings` |
| Wonder activated | instances of `Timberborn.Wonders.Wonder` → `<IsActive>k__BackingField` |
| Buildings finished | `DistrictBuildingRegistry._finishedBuildings` → `BaseComponent._componentCache` → `ComponentCache._components` → `TemplateSpec.<TemplateName>` |
| Population / day | `PopulationService.<GlobalPopulationData>` → `<NumberOfAdults>`; `DayNightCycle.<DayNumber>` |

Open question for the dumper: does `ComponentCache._name` already hold the
template name (e.g. `Forester.Folktails`)? If so the buildings split collapses
from a component-array walk to one string read per building.

### Run start

There is no `NewGameInitializedEvent` equivalent to observe from outside. The
plan is to combine:

- `DayNightCycle` instance identity changing on scene load, plus `DayNumber == 1`, and
- `Timberborn.ErrorReporting.WorldDataService.SourceFileName` — a **static**
  holding the save file being loaded, empty on a new game. This is reachable
  with no scanning at all and gives us new-game vs. loaded-save discrimination
  nearly for free.

This is the fiddliest part of the splitter and needs the most testing.

## Risks

1. **Heap scan performance** in the WASM sandbox — cross-process reads over a
   multi-GB address space. Mitigated by filtering to readable-writable
   non-executable ranges and scanning once rather than per tick. This is the
   biggest unknown. The spike in `src/scan.rs` exists to answer it and has not
   been run against the game yet.
2. `List<T>` / `HashSet<T>` internal layout is Unity's Mono BCL — stable across
   game patches, but moves on a Unity upgrade.
3. Class or field renames in a game update. Caught immediately by the dumper.

## Naming and distribution

### Desktop LiveSplit runs these, and auto-loads them

Worth stating plainly, since "the new WebAssembly runtime" sounds like it might
be LiveSplit One only. It is not:

- `LiveSplit.AutoSplittingRuntime` is a **git submodule of LiveSplit itself**,
  exactly like `LiveSplit.ScriptableAutoSplit` (the ASL engine).
- LiveSplit 1.8.37 ships both engines in the box. From the release zip:

      49,664  Components/LiveSplit.AutoSplittingRuntime.dll
      71,168  Components/LiveSplit.ScriptableAutoSplit.dll
   9,727,488  Components/x64/asr_capi.dll      <- the wasmtime host
     170,496  Components/x86/asr_capi.dll

- `AutoSplitter.cs` downloads the `.wasm` and activates it through
  `ComponentManager.ComponentFactories["LiveSplit.AutoSplittingRuntime.dll"]`,
  the same path ASL scripts take, from the same
  `LiveSplit.AutoSplitters.xml` index.

So the runner experience is identical to the current splitter: set the game
name, click Activate. Nothing extra to install.

The Auto Splitting Runtime component also accepts a local `.wasm` path, which is
how you point it at a development build, but that is not how runners get it.

### The runtime is selected by ScriptType, not by the file extension

This one is easy to get wrong and the failure mode is confusing, so it goes
first. Both ASL and WebAssembly entries are `<Type>Script</Type>`. What
distinguishes them is a separate `<ScriptType>` element — from
`AutoSplitterFactory.cs` in LiveSplit:

```csharp
autoSplitterType = scriptTypeElementText == "AutoSplittingRuntime"
    ? AutoSplitterType.AutoSplittingRuntimeScript
    : AutoSplitterType.Script;
```

So our index entry **must** carry it:

```xml
<AutoSplitter>
    <Games>
        <Game>Timberborn</Game>
    </Games>
    <URLs>
        <URL>https://github.com/OWNER/REPO/releases/latest/download/timberborn_autosplitter.wasm</URL>
    </URLs>
    <Type>Script</Type>
    <ScriptType>AutoSplittingRuntime</ScriptType>
    <Description>Auto start/split for Timberborn - Wonder. No mod required.</Description>
</AutoSplitter>
```

`<Description>` is what LiveSplit shows runners in the activation prompt, so it
is where the practical difference belongs.

Omit `<ScriptType>` and LiveSplit hands the `.wasm` to the ASL engine, which
fails in a way that does not obviously point at the cause. All 57 WebAssembly
entries currently in the index carry both the `.wasm` URL and the element, with
no mismatches — which is exactly why it is tempting to assume the extension is
what matters. It is not.

### Filename

There is no enforced convention; naming is for humans only.

Across the 57 WebAssembly splitters currently in the index, three families:

| Pattern | Examples |
|---|---|
| `<game>_autosplitter.wasm` / `_auto_splitter` | `pizza_tower_autosplitter`, `cosmic_shake_auto_splitter` |
| `livesplit_<game>.wasm` | `livesplit_sonicmania`, `livesplit_redfall` |
| `_wasm` / `_asr` suffix | `live_a_live_autosplitter_wasm`, `pseudoregalia_asr` |

We use the first and most common: `timberborn_autosplitter.wasm`. The `_wasm`
suffix is used by authors disambiguating *two repos of their own* — e.g.
`SonicSpeedrunning/LiveSplit.Sonic1Forever` alongside
`LiveSplit.Sonic1Forever_wasm` — which does not apply here, and `_wasm.wasm` is
redundant besides. If this ever moves into a repo that also holds the ASL
splitter, revisit and take the suffix then.

### The index has one entry per game

Worth knowing before planning a release: `LiveSplit.AutoSplitters.xml` has
**2,611 entries and zero games listed more than once**. It is strictly one auto
splitter per game name, and LiveSplit auto-downloads whichever one matches.

So this cannot ship as a second Timberborn entry alongside the existing one — it
would have to replace it. That makes release a coordination question rather than
a technical one:

- Re-pointing the `<Game>Timberborn</Game>` entry needs MHVandborg's agreement,
  or LiveSplit maintainer arbitration.
- On the next index refresh, *every* runner with Timberborn splits picks up
  whatever that entry points at. So it must be at least as good on day one, and
  the split set has to stay compatible — which is why we keep the same seven
  splits in the same order.

Worth opening that conversation early rather than at the end. A joint handoff is
a much better outcome than arriving with a finished competing implementation,
and it may be that the right home for this is `timberborn_speedrun` itself — in
which case the `_wasm` naming question above comes back.

## Running the spike

`src/scan.rs` plus the `spike()` function in `src/lib.rs` are a first cut at the
approach above, aimed squarely at the open performance question. The subject is
`DayNightCycle`: a singleton with a trivially checkable field, `DayNumber`,
which should be >= 1 in a loaded game and tick over as days pass.

```bash
cargo build --release
```

Load `target/wasm32-unknown-unknown/release/timberborn_autosplitter.wasm` in
[asr-debugger](https://github.com/LiveSplit/asr-debugger), then **load a save** —
`DayNightCycle` does not exist in the main menu, so a scan from there correctly
finds nothing.

What to look for:

- `Scan: 1 hits` — exactly one instance, as expected for a singleton. More than
  one means the class needs disambiguation before it can be used this way.
- The MiB figure, and asr-debugger's own tick timing display. wasm32-unknown-unknown
  gives us no clock, so the debugger's timing is the measurement. **This is the
  number the whole approach hinges on.**
- `DayNumber = N` lines appearing as days pass, which confirms the pointer is
  live and the field offset is right.

If the scan is too slow to run in one tick, the fix is to make it resumable —
scan N ranges per tick and carry the cursor across — rather than to abandon the
approach. `Stats` is already shaped to support that.
