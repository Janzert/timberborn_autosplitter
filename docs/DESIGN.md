# Design

## Why not the existing mod + ASL script

The current auto splitter is a Timberborn mod that writes `autosplitter_state.json`
every 0.5s, plus an ASL script that reads that file with `System.IO`.

Two problems:

1. speedrun.com does not allow mods to be running for a submitted run, so the
   mod-based splitter cannot be used for runs that are actually submitted.
2. LiveSplit's sandboxed WebAssembly auto splitting runtime has **no filesystem
   access**. It can attach to a process, read its memory, control the timer,
   expose settings, and log. That is all. The file-based design cannot be
   ported to it at any level of effort.

So the splitter has to read game state out of process memory directly.

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

### The asr change this needs

`mono::Class` keeps its address in a `pub(super)` field and `get_name` is
`pub(super)` too, so from outside the crate there is no way to get a class's
address to compare against. `vendor/asr` is a submodule so we can add the
accessor and pin it; see `docs/ASR_FORK.md`.

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
   multi-GB address space. Mitigated by filtering to committed private RW
   ranges and scanning once rather than per tick. This is the biggest unknown
   and should be the first thing prototyped.
2. `List<T>` / `HashSet<T>` internal layout is Unity's Mono BCL — stable across
   game patches, but moves on a Unity upgrade.
3. Class or field renames in a game update. Caught immediately by the dumper.
