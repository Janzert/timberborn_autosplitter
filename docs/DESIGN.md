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

### Matches have to be validated

A raw scan over-matches. The vtable address also appears inside Mono's own
metadata — including the `domain_vtables[0]` slot that `Class::get_vtable`
reads it out of. Measured against the game, `DayNightCycle` produced six
matches: one real object and five words in Mono metadata clustered around the
vtable itself.

So each match is checked by reading a reference-typed field off it and
confirming the object that field points at has the vtable of the class the
field is declared to hold. `DayNightCycle._eventBus` must lead to an `EventBus`.
This is decisive in practice — one of the rejects had a *non-null* `_eventBus`
that simply did not lead to an `EventBus`, so a null check alone would have
admitted it.

Because validation is reliable, a scan for a singleton stops at the first match
that survives it. Classes that are legitimately multi-instance —
`DistrictBuildingRegistry` is per-district — need the full scan instead.

The scan runs on attach and on scene change, not per tick. A located instance is
revalidated each tick, which is two reads and detects a scene change directly
rather than waiting for reads to start failing.

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

| Split | Path | Type |
|---|---|---|
| Wonder activated | instances of `Timberborn.Wonders.Wonder` → `IsActive` | `bool` |
| Research Earth Recultivator | `BuildingUnlockingService._unlockedBuildings` | **`HashSet<string>`** |
| Buildings finished | `DistrictBuildingRegistry._finishedBuildings` → `EntityComponentRegistry._registeredComponents` → per component `BaseComponent._componentCache` → `ComponentCache._components` → `TemplateSpec.TemplateName` | `EntityComponentRegistry`, then `Dictionary<Type, List<IRegisteredComponent>>`, `List<object>`, `string` |
| Population / day | `PopulationService.GlobalPopulationData` → `NumberOfAdults`; `DayNightCycle.DayNumber` | `int` |

Field types were read out of the assemblies offline (`devtools/metadata.py`),
which settles how much machinery each split needs:

- **Wonder activated needs none.** `IsActive` is a plain `bool`. A wonder is a
  building rather than a singleton, so all instances are scanned and any active
  one counts.
- **Research is a string membership test**, not an object graph walk —
  `_unlockedBuildings` holds template names directly. It does need a
  `HashSet<string>` reader.
- **Buildings finished is the expensive one**, three collection types deep.
  `ComponentCache._name` is a `string`, so the shortcut is still open: if it
  holds the template name, the last two hops collapse to one read.

### Split latency

Run start (initial load) and run end (wonder activated) are the two splits whose
accuracy actually matters; the intermediate ones only affect segment times.

There is **no watchpoint mechanism**. The sandbox offers read-only memory access
— no ptrace, no debug registers, no write traps — so a change can only be
noticed by looking again.

That is fine, because scanning and reading are different costs. Locating an
object is a multi-GiB scan; re-reading a known address is one byte. So anything
timing-critical is located once and then polled **every tick**, and only the
scan is throttled. Getting this wrong is easy: the first version rescanned every
2 seconds, which would have put 2 seconds of error on the run-end split.

Resulting error is one tick, worst case:

| host | tick rate | worst-case error |
|---|---|---|
| asr-debugger | 120/s (measured) | ~8 ms |
| LiveSplit (Auto Splitting Runtime) | ~66/s — the component drives it from a `Timer { Interval = 15 }` | ~15 ms |

Average error is half of that. `asr::runtime::set_tick_rate` can ask for more,
but the host's own polling interval is the real ceiling, so raising it does
nothing in LiveSplit. ~15 ms is comfortably inside speedrun.com's 0.01 s
display precision and is the best achievable from outside the process.

The same treatment is needed for run start once its signal is settled.

### Reading BCL collections

`HashSet<T>`, `List<T>` and `Dictionary<K, V>` layouts belong to Unity's Mono
BCL, not to Timberborn, so they move on a Unity upgrade rather than a game
patch. They should be resolved by name like everything else rather than
hardcoded.

That needs something the fork does not expose yet: given an arbitrary object we
find at runtime, there is no way to turn its `MonoClass` pointer back into an
`mono::Class` to call `get_field_offset` on. `Class`'s address field is
`pub(super)` and there is no constructor from an address. Adding one is a small
addition to the same fork that already carries `get_vtable`, and it is needed
for both the research and buildings splits.

Open question for the dumper: does `ComponentCache._name` already hold the
template name (e.g. `Forester.Folktails`)? If so the buildings split collapses
from a component-array walk to one string read per building.

### Run start

There is no `NewGameInitializedEvent` equivalent to observe from outside.

**Measured caveat: the presence of a service instance is not a reliable "in a
game" signal.** After exiting to the main menu, the `DayNightCycle` instance was
still found at the same address ~20 seconds later, still reading the last
in-game `DayNumber`. It is either uncollected or deliberately retained; either
way a splitter watching for the object to disappear would think a run was still
in progress. Absence is meaningful at process start — the class has no vtable
until it is first instantiated, confirmed by a 41 second wait on the main menu —
but not after a game has been loaded once.

So run start needs an authoritative signal rather than an inferred one. The plan
is to combine:

- `DayNightCycle` instance identity changing on scene load, plus `DayNumber == 1`
  — necessary but, per the above, not sufficient on its own, and
- `Timberborn.ErrorReporting.WorldDataService.SourceFileName` — a **static**
  holding the save file being loaded, empty on a new game. This is reachable
  with no scanning at all and gives us new-game vs. loaded-save discrimination
  nearly for free.

This is the fiddliest part of the splitter and needs the most testing.

## Risks

1. ~~**Heap scan performance**~~ — **answered, see Spike results.** A full
   scan of 3.3 GiB takes 1–2s, and with early stop a singleton lookup does not
   need anything like the full sweep. The scan is resumable, so it never stalls
   the splitter's update loop.
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

## Spike results

Run against the game (Timberborn under Proton, long-running save loaded,
`asr-debugger`). The approach is confirmed end to end.

**Everything resolves by name.** Mono attach, image, class, and field offsets
all resolved without a single hardcoded offset: `DayNumber` at `+0x48`,
`_eventBus` at `+0x10`, found by name at runtime. `DayNumber` read live and
ticked 546 -> 547 as a day passed in game.

**Six matches, one real.** The vtable sat at `7cbcd7a8`; four of the five
rejects clustered in `7c8x`–`7cbf`, i.e. in Mono metadata immediately around it,
and one was a lone stray much higher up. The real object was at `7753b850`, in a
separate band — the managed heap. Validation rejected all five, in both runs.

**Timing.** A full sweep of 3321.6 MiB took 1–2s across 107 slices, roughly
10–19ms per slice, which sits about at the host's update interval. Spreading the
work across slices cost essentially nothing in wall time versus the earlier
blocking version, while removing the stall. ~1500 chunk reads failed out of
~53,000, consistently across runs — the map shifts mid-scan, and Wine adds
noise. Harmless; worth watching only if it climbs.

These numbers are from Proton, so the range layout (1492 scanned, 1809 skipped)
and read costs are Wine's, not Windows'. Worth re-measuring natively before
tuning anything against them.

### Lifecycle, learned the hard way

A full-lifecycle run (start splitter, launch game, load save, exit to menu, quit)
turned up three things worth keeping written down:

- **A class has no vtable until it is first instantiated.** Mono fills in
  `domain_vtables[0]` lazily, so `Class::get_vtable` returns `None` in the main
  menu and during load. That is not a failure, it is "not yet" — and it doubles
  as a cheap signal that a game has actually been loaded.
- **Memory goes transiently unreadable during a scene teardown.** A scan taken
  just after exiting to the menu could not read ~193 MiB, found nothing, and the
  object turned up one second later in a clean scan. Retrying failed chunks page
  by page recovered only ~7 MiB of that, so this is whole regions being remapped,
  not isolated guard pages. The page-level retry is kept because it is cheap, but
  the thing that matters is `Scan::is_conclusive()`: **an empty result only means
  "absent" if the scan read everything it set out to read.** A dying process
  reports no ranges at all, which is why zero bytes scanned counts as
  inconclusive too, not as a clean negative.
- **Nothing may return into a bare retry loop.** Bailing out on a transient
  condition and immediately re-attaching produced 6,098 log lines in 33 seconds.
  A process on its way out stays attachable for several seconds after it starts
  exiting, so the outer attach loop needs a delay as well.

The address space also grows substantially during load — 1.2 GiB mid-load versus
3.5 GiB in game — so scan cost depends on when it runs.

### Finding the process

The runtime matches processes on the name the OS reports. On Linux that is
`/proc/<pid>/comm`, **capped at 15 characters**, via `sysinfo`.

Unity 6.5 names its main thread "Unity Main Thread", so a Proton install reports
`Unity Main Thre` and `Process::attach("Timberborn.exe")` can never match, even
though the command line is `...\Timberborn.exe`. Unity 6.3 did not do this,
which is why this only appeared on the experimental branch — and it presents as
the splitter sitting silent, indistinguishable from the game not running.

Windows reports the executable name, so runners on the stable branch are
unaffected. Linux and Steam Deck players are not, so it is handled rather than
worked around:

- exact names (`Timberborn.exe`, `Timberborn.x86_64`) are trusted outright
- ambiguous ones (`Unity Main Thre`) would match *any* Unity 6.5 game, so
  candidates are enumerated with `list_by_name`, attached by pid, and only
  accepted once `Timberborn.exe` is visible as a mapped module. Under Proton the
  process path points at the Wine loader, so the module is the reliable check,
  not the path.

The lesson generalises: the name a process reports is not stable across engine
versions, and anything that can silently find nothing needs to say so
periodically.

### Measured offsets

Recorded as evidence, not relied on: everything is resolved by name at runtime.

| class | field | stable 6000.3.6f1 | experimental 6000.5.5f1 |
|---|---|---|---|
| `DayNightCycle` | `DayNumber` / `_eventBus` | `+0x48` / `+0x10` | same |
| `BuildingUnlockingService` | `_unlockedBuildings` / `_eventBus` | `+0x48` / `+0x30` | same |
| `Wonder` | `IsActive` / `_eventBus` | `+0x48` / `+0x38` | same |
| `DistrictBuildingRegistry` | `_finishedBuildings` / `_instantFinishedBuildings` | `+0x48` / `+0x50` | same |
| `BaseComponent` | `_componentCache` | `+0x10` | same |
| `ComponentCache` | `_components` / `_name` | `+0x48` / **`+0x68`** | `+0x48` / **`+0x60`** |
| `TemplateSpec` | `TemplateName` | `+0x20` | same |
| `PopulationService` | `GlobalPopulationData` | `+0x10` | same |
| `PopulationData` | `NumberOfAdults` / `NumberOfChildren` | `+0x10` / `+0x14` | same |
| `WorldDataService` | `SourceFileName` (static) | `+0x0` | same |

**`ComponentCache._name` moved by 8 bytes between the two versions.** That is
the justification for the whole name-resolution approach, measured rather than
argued: a hardcoded offset would have silently read the adjacent field, and
`_name` is on the path for the buildings split. Everything else held, so the
churn is real but narrow.

Mono reports as V3, 64-bit on both. The runtime ticks at **120/s**, confirmed
by a 1800-tick timer firing every 15 seconds.

## Checking a new game version

Everything the splitter reads is resolved by name, which is what should make it
survive game updates. Two halves check that, and neither takes long:

- **Offline**, with the game closed:
  `devtools/metadata.py check <Timberborn_Data/Managed>` verifies every name
  `src/probe.rs` depends on against the installed assemblies. This is the fast
  answer to "did an update rename something".
- **At runtime**, `src/probe.rs` resolves the same set against the live process
  and logs each one's offset, plus the Mono version and pointer size. It runs
  once a save is loaded, because Mono loads assemblies lazily and some are
  absent in the main menu.

A missing *vtable* in the probe output is not a failure — Mono fills those in
lazily, so it only means the class has not been constructed yet this session.
Missing *classes or fields* are the real signal.

Worth running against the Steam `experimental` branch to get advance warning of
what the next release breaks. Old release branches (0.6, 0.7) are also allowed
for submitted runs, though nobody appears to be using them, so supporting them
is worth having only if it turns out to be close to free.

## Running the spike

`src/scan.rs` plus `spike()` in `src/lib.rs` locate `DayNightCycle` and watch
`DayNumber` — a singleton with a trivially checkable field, which should be >= 1
in a loaded game and tick over as days pass.

```bash
cargo build --release
```

Load `target/wasm32-unknown-unknown/release/timberborn_autosplitter.wasm` in
[asr-debugger](https://github.com/LiveSplit/asr-debugger), then **load a save**.
`DayNightCycle` does not exist in the main menu, so a scan from there correctly
finds nothing. Re-running after a code change is a "restart" in the debugger;
the game can keep running.

Expect roughly:

```
DayNightCycle vtable 7cbcd7a8, DayNumber +0x48, _eventBus +0x10 (EventBus vtable 7241d688).
Scan: 412.0 of 3321.6 MiB (12.4%) over 13 slices | 0 rejected | 180 read failures
Found DayNightCycle at 7753b850. Watching DayNumber.
DayNumber = 546
```

The percentage is the early stop working: the scan should finish well short of
the full sweep. `rejected` counts Mono-metadata matches encountered *before* the
real one, so it depends on where the object falls in range order and may be
zero.
