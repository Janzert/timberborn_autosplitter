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
| Buildings finished | instances of `BlockSystem.BlockObjectState` → `_state`, and `_componentCache` → `ComponentCache._name` | `State` enum, `string` |
| Population / day | `PopulationService.GlobalPopulationData` → `NumberOfAdults`; `DayNightCycle.DayNumber` | `int` |

Field types were read out of the assemblies offline (`devtools/metadata.py`),
which settles how much machinery each split needs:

- **Wonder activated needs none.** `IsActive` is a plain `bool`. A wonder is a
  building rather than a singleton, so all instances are scanned and any active
  one counts.
- **Research is a string membership test**, not an object graph walk —
  `_unlockedBuildings` holds template names directly. It does need a
  `HashSet<string>` reader.
- **Buildings finished turned out cheap.** `ComponentCache._name` holds the
  template name outright — sampled from a live game it reads
  `"WoodWorkshop.Folktails"`, `"TappersShack.Folktails"`, and
  `"BlueberryBush.<guid>"` for natural entities. That removes the component-list
  walk and the `TemplateSpec` lookup entirely.

  Finished state comes from `BlockSystem.BlockObjectState`, which carries
  `_state` (`Unfinished`, `Finished`, `Preview`) and, being a component,
  inherits `_componentCache`. It also has an `_eventBus`, so the ordinary
  validator finds it. A finished building of a given type is therefore one scan
  plus two derefs.

  The registry route through `DistrictBuildingRegistry._finishedBuildings` —
  `EntityComponentRegistry` holding a `Dictionary<Type, List<IRegisteredComponent>>` —
  is no longer needed, and with it the need for a `Dictionary` reader.

### A class can only be located through a field it has

`Locatable` validates a scan hit by dereferencing a field and checking what it
points at. Most Timberborn services hold an `_eventBus`, so that is the default —
but it is not universal, and a class validated through a field it does not have
can **never** be located. The scan finds it, the validator rejects every hit,
and the result is indistinguishable from "not present".

This has bitten twice: `WorldDataService` (not a DI service, statics only) and
`DistrictBuildingRegistry` (validated through `_entityComponentRegistryFactory`
instead). Both failed silently — the second cost a full test run in which the
building splits simply never appeared.

Two defences:

- `devtools/metadata.py check` reads every `Locatable::new` and
  `with_validator` call out of the source and verifies the class really has the
  field it is validated through. It catches this with the game closed.
- Resolution failures explain themselves on the first attempt rather than
  retrying in silence. "Not constructed yet" is normal during a load, so later
  attempts stay quiet.

### Faction-specific template names

The wonder is a different building per faction, and the names are not
guessable:

| faction | wonder template |
|---|---|
| Folktails | `EarthRecultivator.Folktails` |
| Iron Teeth | `EarthRepopulator.IronTeeth` |

`TributeToIngenuity.IronTeeth` reads like a wonder and is not one — it is a
monument, alongside `FarmerMonument.Folktails` and `LaborerMonument.IronTeeth`.
Guessing it cost an Iron Teeth test run, during which the research split simply
never fired. The confirming evidence is the pair of localisation keys
`Buildings.Wonder.EarthRecultivator` and `Buildings.Wonder.EarthRepopulator`.

A wrong name fails **silently** — the split just never happens. So reaching the
run end without ever having seen the wonder in the unlocked set now logs a
warning naming that as the likely cause.

A more robust alternative exists and is not implemented: rather than matching
names, find the building spec carrying a `Wonder` component. That would cover
future factions without changes, at the cost of walking building specs.

### Watch cheap fields, not expensive ones

Wonder activation was originally detected by scanning for `Wonder` instances and
reading `IsActive`. A wonder is a building rather than a singleton, so that
meant a **full heap scan every two seconds for the length of a run** — around
180 scans in a six-minute test — and it never actually found one.

`WonderCompletionCountdownStarter._unlockDay` is set at the moment of
activation, and that object is already located for the run-end split. Watching
it costs two reads instead of a scan, and needs no `Wonder` tracking at all.

The general point: prefer a field on an object already held over locating a new
one, especially for anything polled. Scans are for finding things once.

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

### Category rules, and what they mean in memory

From the category rules:

> Starts when the overlay appears after choosing your settlement name.
> Ends when the "Congratulations!" screen appears.

**The end condition is not wonder activation.** Activating the wonder starts a
countdown, and only when it finishes does the Congratulations screen appear:

```
Wonder.IsActive = true
  -> WonderCompletionCountdownStarter sets _unlockDay
     from UnlockOffsetInHours (static float, in-game hours)
  -> CountdownFinished = true
  -> WonderCompletedEvent
  -> WonderCompletionPanel  (WonderCompletedLocKey -- "Congratulations!")
```

So the run-end signal is `WonderCompletionCountdownStarter.CountdownFinished`
(`bool`, on a singleton with `_eventBus`, so the ordinary locator finds it).
`Wonder.IsActive` is strictly earlier by the countdown.

This matters beyond this implementation: the existing ASL splitter's final split
is "Earth Recultivator (Launch)", which fires on activation. If the countdown is
non-zero, that splits early relative to the rules.

**Measured: 0.5 in-game hours, which is 9.6s of real time at 1x** (a day is
16+8 hours in 460.8s). So the existing ASL splitter ends its runs roughly that
much early relative to the rules.

Treat 9.6s as an order of magnitude, not a constant. The countdown is in in-game
hours, so its real-time length scales with game speed — a run at 3x sees ~3.2s —
and stretches further if the simulation cannot keep up or if maximum
acceleration is capped as a settlement grows. `DayLengthInSeconds` itself
appears not to be population-scaled: it read 460.80002 in both a brand-new game
and a day-1511 save.

None of that affects the splitter, which reads `CountdownFinished` directly. The
conversion exists only to size the discrepancy for discussion.

The gap does not have to be observed to be known. The countdown is in in-game
hours and `DayNightCycle` knows both the length of a day in hours
(`DaytimeLengthInHours + NighttimeLengthInHours`) and in real seconds
(`DayLengthInSeconds`), so:

    gap_seconds = UnlockOffsetInHours * DayLengthInSeconds / (Daytime + Nighttime)

which is computed and logged from any loaded save. That matters because
observing a real completion is expensive: the Congratulations screen only
appears on a map's first completion, so a save that has already finished its
wonder cannot produce another one.

The same fact is a correctness trap. `CountdownFinished` is persisted in the
save, so a completed save loads with it already `true`. The end split therefore
only fires on a **transition we observed**, never on a value that was already
set when we attached. A real run starts from a new game and can never hit this,
but a splitter that fires the run-end split on loading an old save would be
badly behaved.

**Start** is identified, and the game's own state machine uses the same concept
the rules do. `GameInitializer` steps through an `InitializationState` enum:

```
0 Waiting  1 SpawnBeavers  2 PostSpawnBeavers  3 UnpauseGame  4 ShowUI  5 Finished
```

Measured against a real new game: it sits on `Waiting` for as long as the
settlement-name dialog is up, then steps through 1-5 within a few ticks of
confirming the name. **`ShowUI` is the split** — the rules say the run starts
when the overlay appears, and that is the step that shows it.

`SpeedManager.CurrentSpeed` was the other candidate and is unusable. It becomes
1 at `UnpauseGame`, one step early, and then toggles every time the player
pauses, so it is neither correctly placed nor monotonic.

`GameInitializer` exists while the dialog is up, so it is located in advance and
then polled every tick. Run start gets the same one-tick accuracy as run end,
with no scanning in the critical window.

Like the run end, this fires only on an observed transition: a state below
`ShowUI` has to be seen first, so attaching to a game already in progress does
not count as a start.

**Loading a save walks the identical sequence**, confirmed by measurement, so
`initializationState` alone cannot tell a load from a new game and would fire a
run start on both. The discriminator is the static
`WorldDataService.SourceFileName`: null on a new game, set to the save being
loaded otherwise. It is read when `ShowUI` is reached, and a non-empty name
suppresses the start.

Reading it needs no instance and no validation. The service locator requires an
`_eventBus` so it can validate the objects it finds, but `WorldDataService` is a
crash-reporting helper rather than a DI service and has no such field — so the
locator cannot be built for it at all, even though its statics read fine. Static
access is therefore a separate path. The first attempt coupled the two and
silently could not read the field.

### Resolve lazily, and keep retrying

On a fresh load a singleton exists before its dependencies are injected, so a
scan finds the object, sees a null `_eventBus`, rejects it, and returns nothing.
That is correct behaviour — an object mid-construction is not yet the service —
but it means **the first attempt to resolve anything routinely fails**.

Everything must therefore be resolved lazily and retried on an interval, never
resolved once at load. `RunStart` was resolved once, failed that way, and run
start went unwatched for an entire session while every other watcher worked, so
the failure looked like a detection bug rather than a resolution one.

Retries cost a scan, so they are spaced (~2s). That is comfortable here because
the settlement-name dialog is open far longer than that, giving ample time to
resolve before the state can leave `Waiting`.

### Nothing may sample saved state before initialization finishes

Two false splits in one test run traced to the same mistake, so it is worth
stating as a rule.

Loading a save that had already completed its wonder fired a run *end* five
seconds into the load. `CountdownFinished` was sampled as `false` while the
scene was still loading, and `GameWonderCompletionRestorer` then restored the
saved `true` — which looks exactly like a completion happening.

The "only fire on an observed transition" guard is necessary but not
sufficient: the baseline has to be taken at the right time as well. So anything
reading persisted state waits for `initializationState == Finished`. Run start
is the exception, and has to be, since it fires at `ShowUI` one step earlier —
but it reads only a state machine, never saved data.

### Run start: earlier notes

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
