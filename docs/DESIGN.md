# Design

## Background

Autosplitting for Timberborn was established by MHVandborg's splitter
([timberborn_speedrun](https://github.com/MHVandborg/timberborn_speedrun)): a
game mod that exports live state to `autosplitter_state.json`, paired with an
ASL script that reads it. It works, and it did the genuinely hard part — working
out which in-game events the Wonder category should split on, and proving the
whole thing was worth having. This project would not exist without it.

The split set and order follow it closely, with two deliberate departures. The
advanced science split covers both factions' buildings (Observatory for
Folktails, Numbercruncher for Iron Teeth) rather than Folktails' alone, and the
final split is named for the Congratulations screen rather than for launching
the wonder, because that screen is the run end the rules describe and the
wonder is only its prerequisite.

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

This is how a service is found when nothing else knows where it is, and it is
the fallback rather than the usual path. Most services are reached through the
DI container -- see *Locating services through the DI container* below, which
turns four of the five scans a loaded game used to cost into lookups -- and the
container itself, the run start and every later search go through the runtime's
reference table, described below. A full sweep is what finds the scene loader
once at the main menu, and what stands behind the table whenever it does not
settle a question.

On Mono, `object[0]` is a `MonoVTable*` and `vtable[0]` is a `MonoClass*`
(confirmed by asr's own `UnityPointer` traversal). So:

1. Resolve the target class by name via the Mono module to get its `MonoClass*`.
2. Find its `MonoVTable` (a vtable whose first pointer is that class).
3. Look for a pointer-aligned word equal to that vtable address -- among the
   objects the reference table names, or failing that across every writable
   committed range from `Process::memory_ranges()`. That is the instance.
4. Cache it, revalidate cheaply each tick, re-resolve on scene change.

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
that survives it. Nothing the splitter reads now needs the multi-instance form:
the one class that did, `DistrictBuildingRegistry`, is no longer used, and its
scan is a cautionary tale in its own right — it reliably found four instances,
two of which were stale leftovers.

The scan runs on attach and on scene change, not per tick. A located instance is
revalidated each tick, which is two reads and detects a scene change directly
rather than waiting for reads to start failing.

Unity ships Mono with the Boehm collector — `MonoBleedingEdge/EmbedRuntime/mono-2.0-bdwgc.dll`
— which **does not move objects**. So a located instance address stays valid for
the lifetime of that object, and a rescan is only needed when the object itself
is replaced, i.e. on a scene change. If Unity ever switched this build to SGen,
which is a compacting collector, cached pointers would need revalidating against
the vtable every tick.

### The asr changes this needs

`mono::Class` keeps its address in a `pub(super)` field, so from outside the
crate there is no way to get a class identity handle to compare against, and no
way to go the other way either — from an object found at runtime back to a
`Class` whose field offsets can be looked up by name.

`vendor/asr` therefore carries two accessors, both deliberately narrow:

- `Class::get_vtable()` — asr already computes this internally for static table
  lookups (`class.runtime_info` -> `domain_vtables[0]`), so this exposes the
  exact value the scan compares against rather than the raw class address.
- `Class::of_object()` — reads an object's `MonoVTable` -> `MonoClass` and
  returns it as a `Class`. This is what makes BCL collections resolvable by
  name: a generic instantiation like `HashSet<string>` cannot be looked up in an
  image by name, but any instance points at its own class.

Neither leaks offset internals, which makes both better upstreaming
propositions than a raw address getter. See `docs/ASR_FORK.md`.

### Reading fewer bytes: the runtime's reference table

A sweep reads every readable-writable range in the process. Under Proton that is
cheap enough not to matter. On Windows it is not, and the gap is not one of
degree — measured against the same game version:

| | Proton | Windows |
|---|---|---|
| Address space scanned | 3.3 GiB | 6.9 GiB |
| A full sweep | 1–2s | 29s |
| Effective throughput | ~2 GiB/s | ~240 MiB/s |

That is fatal rather than merely slow, because `resolve_during_load` is a
*retry* loop: the initializer does not exist on the first passes, so the bind
is retried until it takes. At 1–2s a try it gets twenty or forty attempts
inside a load. At 29s it gets one, and a missed run start is the result. The
stutter a runner sees and the timer that fails to start are the same bug.

Slicing cannot fix it. Total sweep time is bandwidth-bound, so a smaller budget
makes the stalls shorter and more numerous and leaves the total where it was.
The only lever is reading fewer bytes.

**The lever is not to search memory at all.** Unity's native side holds the
managed objects it cares about through a table of references, and everything the
splitter has to locate has an entry in it. Reading that table gives a few
hundred thousand object addresses directly; checking which of them are instances
of the class wanted costs only the pages those objects sit on, because managed
objects cluster. Measured across three recordings on two game builds:

| moment | through the table | a full sweep |
|---|---|---|
| main menu | 3 MiB | 811 MiB |
| first game | 20 MiB | 3916 MiB |
| second game of the session | 70 MiB | 5195 MiB |

Note the last row. The sweep grows as the process does — 3916 MiB to 5195 MiB
between two games of one session — so the cost this removes is at its worst
exactly when a runner is resetting for another attempt.

It is also *cleaner* than a sweep. Sweeping for `SingletonRepository` reports
"3 found, 21 rejected": the rejects are words inside Mono's own metadata that
happen to equal the vtable. The table holds object pointers and nothing else,
so those never arise.

#### It is found, never tabulated

The table is at `0x1f0000000` on one game build, `0x230000000` on another, and
`0x18a40000000` on Windows -- three addresses across two builds and two
platforms, sharing not even a shape -- so a tabulated address would be wrong on
the next update, and on the other platform today — the same reason nothing else here is
a hardcoded offset. `ReferenceTable::find` in `src/table.rs` looks for it
instead:

1. Locate one long-lived object the ordinary way. The scene loader is ideal: it
   exists at the main menu and outlives every scene.
2. Sweep for words equal to that object's address. Around 28 come back, in
   roughly ten distinct ranges.
3. Pick out the range that is a table rather than a heap section.

Step 3 is decided by content. A heap section is full of *object headers* — words
pointing into the region where vtables live — and a table has none. Measured
across both builds: a table has 0 to 2 such words, and the sparsest heap section
competing with one has 2355. `MAX_HEADER_WORDS` sits at 64, a factor of thirty
from either side, and the count is checked as a candidate is read so a heap
section is abandoned after sixty-five headers rather than read to the end.

A *fraction* was tried instead, on the theory that the ranges differ in size by
two orders of magnitude, and it is worse in both directions at once: the heap
sections run from 0.7% to 30% object headers, and a threshold loose enough to
admit a real table was measured accepting a 2600 KiB heap section as one. The
absolute count is the one the data supports.

One candidate needs no statistics at all: the range the anchor itself sits in
holds an object by definition, so it is skipped outright. That is what makes the
rule exact in a synthetic world, where a heap with a dozen objects in it is
otherwise sparse enough to look like a table.

#### Both sweeps happen at the main menu, and the ordering is load-bearing

Finding the anchor and then finding the table are two full passes, and both run
before a game exists — on a process a fraction of the size it will reach.
Measured on Windows: 704 MiB at the menu against 7.5 GiB by the second game of
a session.

That only holds because nothing above them waits for a game. `SceneLoader` is
validated through its `AssetLoader`, which is instantiated at the menu, and
finding the table needs only something already located. **The event bus and the
clock are resolved below them for exactly this reason.** Mono fills a class's
vtable in lazily, so `retry` on either blocks until a game scene is being built;
with them above, both sweeps were pushed into the first load instead — which is
the one place the whole design is trying to keep clear.

That was live for a while and nothing offline could see it: every recording
still passed, because the sweeps still happened and still found everything, just
later than the comment above them claimed. A live Windows run is what showed it.
`both_sweeps_land_before_the_first_game` in `tests/scenario_run.rs` now pins the
ordering to the recordings, which do begin at the main menu.

Verified across all four recordings and both builds: the loader resolves at the
menu, and the table is found there — at `0x230000000` on 1.0.13.1 and
`0x1f0000000` on 1.1.2.4. The discriminator has *less* to get wrong there than
in a game: four or five candidate ranges at the menu against seven to ten once
a game is up.

#### It grows in place, so its size is never remembered

The table doubled from 2 MiB to 4 MiB across two games of one session, in
place: same base, a bigger mapping. Its own header records this -- an end
pointer that moved from `base+0x1fffe0` to `base+0x3fffe0`, with the old value
kept beside it and a counter going 1 to 2.

Everything registered after the growth lands past the old end, so a splitter
holding the size it first saw reads a range that no longer contains anything
current. **And nothing fails.** The reads still succeed, the range is still
mapped, so `still_mapped` is satisfied and the table is never looked for again;
every search simply misses and falls back. Measured live: two games into a
session the splitter was sweeping 7 GiB for both the container and the run
start, having been told by its own cached bound that the table did not have
them. The only clue was the fallback reasons in the log, which is the entire
reason those exist.

So [`ReferenceTable`] keeps the base and nothing else, and reads the extent
back from the process on every search. It is the one number here that is not
allowed to be remembered.

**What is read back is the mapped range, which is an upper bound and not the
table.** The range containing the base flaps during every scene load and
returns to 2048 KiB within a second or two each time. That is neighbouring
anonymous mappings coalescing and splitting as the runtime commits and releases
memory, with the range enumeration reporting whatever abuts the base at that
instant. It is not a small effect, and it is larger on Windows:

| | high-water during a load |
|---|---|
| Linux, six games | 28672 KiB |
| Windows, loading a 16171-entity save | **49152 KiB** -- a 24x swing |

Reading too much is harmless: the extra words become candidate object pointers,
and the vtable check and the validator reject them. It costs a few reads during
a load, against a sweep of gigabytes. Reading too *little* would matter, and has
not been observed -- the range has never been reported smaller than the table.

The two effects are worth keeping apart, because only one of them is the table:

| | a load's flap | real growth |
|---|---|---|
| Settles at | back to 2048 KiB within seconds | 4096 KiB, stably, mid-game |
| Entries past the old end | none | four `SingletonRepository` among them |
| The table's own end pointer | unmoved | `base+0x1fffe0` to `base+0x3fffe0` |

The header is the table's own account of its size and the mapping is not, which
is what settles it: four repository pointers beyond the 2 MiB mark are not a
coincidental neighbour.

That header would in fact be the exact answer, and it is deliberately not used.
Reading a length out of the third word of a structure nobody has identified is
the kind of hardcoded knowledge everything else here avoids -- it would work
until the day it silently did not, which is the failure mode this design exists
to stay away from. The mapping is name-free, self-correcting, and wrong only in
the direction that costs reads.

`notices_the_reference_table_growing` in `tests/synthetic_run.rs` pins it. The
world is a scene load in progress, because that is what opens a window wide
enough to express the bug: the table is found while the load runs, grows during
it, and the container is not looked for until the load ends. Re-introducing the
cached size fails it.

#### A sweep with no table looks for one on its way past

When there is no table, the next sweep is asked to notice references to the
scene loader as it goes. A sweep already reads every byte, so the ranges that
could be the table come back with it and identifying one costs no pass of its
own -- which matters most exactly when there is no table, because then
everything is having to sweep anyway.

It is not free, and the measurement is worth keeping. Folding a second
comparison into the scanner's inner loop cost 55% of the comparison loop
(23.6 GiB/s to 15.3 GiB/s over 512 MiB). Doing it as a separate pass over the
64 KiB chunk, which is still in cache from the read that filled it, costs about
40% of the comparison work -- but against reads of ~2 GiB/s under Proton that
is around 6% of a sweep, and against ~240 MiB/s on Windows well under 1%.

Two consequences follow, and both are in the code:

- **A scan with no anchor is untouched.** The anchor pass is a separate loop
  behind an `if let`, not a second test inside the hot one, so a sweep that did
  not ask for this pays nothing. Folding them together would have charged every
  sweep 6% whether it wanted an anchor or not.
- **Only the container's sweep asks.** The run-start bind sweeps inside the
  load window, which is the one place latency is the whole point, so it is left
  alone. The container's sweep happens after the load has finished and is the
  one that recurs, which makes it the right place to pay 6% for the chance of
  not sweeping next time.

#### What a healthy session looks like

Worth knowing before reading a log, because the failure mode is quiet. A
session that is working shows **one** sweep, before any game exists, and
nothing after it:

```text
[scan] SceneLoader starting (full sweep -- no reference table found yet).
[table] looking for the runtime's reference table.
[table] 28 references to 6b3fb4c0.
[table] found at 1f0000000, 2048 KiB, 2 object headers in it.
[table] GameInitializer: 1 live instances.
[table] SingletonRepository: 3 live instances.
```

Three lines say something has gone wrong, and none of them stops the splitter
working:

- **`[table] no candidate range was one`** -- the discriminator rejected every
  candidate. Every search will sweep.
- **any `full sweep` after the first game** -- the reason on the end says which
  case it is: no table yet, the table unreadable, or the table not holding the
  object.
- **`[table] no reference table after three tries`** -- the search bound giving
  up, which should not happen if a table was ever found.

A pair of `GameInitializer` lines a second or two apart during a load is not a
problem; it is the retry doing its job, the first look finding only the
outgoing game's initializer.

#### What it costs to ship

The artifact grows from 167 KB to 196 KB, a little under 17%. Most of it is the
sorting and grouping in `instances` -- entries have to be sorted for the page
grouping to work at all, and that pulls in sort machinery a splitter otherwise
never uses.

Recorded rather than defended: it is a once-downloaded file, and the thing it
buys is not having to read four gigabytes every time a runner starts a game.
Worth revisiting only if the file ever has to shrink.

#### And if it stops answering, it is found again

Growth is the case we caught. The one we cannot rule out is the table being
*replaced* rather than grown -- and it would look identical from here, because
the old memory stays mapped and stays readable. So rather than trusting that
growth is the only thing that happens, the table is judged by whether it still
answers: `MISSES_BEFORE_REFINDING` consecutive searches that the sweep behind it
had to answer, and it is thrown away and looked for again on the next visit to
the main menu.

The pairing matters. A search the table missed *and the sweep also missed* says
nothing about the table -- the object simply was not there -- so only a sweep
that succeeded counts, and any search the table answers clears the count. The
menu is where the re-finding happens because it costs a sweep and there is no
run to disturb.

The counting policy has unit tests in `src/table.rs`. Two things around it are
verified by the log at runtime rather than offline, and it is worth being
straight about which:

- **Which searches report a hit and which a miss.** A synthetic world cannot
  easily produce a run of genuine misses, because the splitter's own
  skip-the-instance-just-left rules assume a process with several containers in
  it and a fixture has one.
- **The sweep finding a table on its way past.** Producing that offline needs a
  world where the table exists but cannot be discovered at first and can be
  later, which is a third knob on the fixture for one path.

Both fail safe: if either never fires, the splitter sweeps, which is what it
did before any of this.

#### What it is not

**It is not a superset of the heap.** A capture taken while a second game loaded
held a `SingletonRepository` that validated cleanly and still had its 103
singletons, with no entry in the table at all — a main-menu container whose scene
had already been torn down, with two references left to it against the seven each
live container had. Its predecessor from the first menu visit was in the table
throughout the whole time it was in use, and by then had been freed outright.

The reading that fits every observation is that an entry goes when the object
dies and the heap keeps the corpse until it is collected — which would make this
the *live* set, and so better than what a sweep returns: the stale leftovers that
`skip` exists to work around would never be offered in the first place. That
cannot be proven from a memory image, so nothing depends on it.

**So a table search never proves absence.** `Found::conclusive` is false for one
however cleanly it read, and an unsettled result falls through to the sweep.
That matters concretely: the give-up-skipping rule counts *conclusive* empty
searches, and would otherwise be talked into binding the previous game's
container.

Every sweep says in the log why it is happening -- no table yet, the table
unreadable, or the table simply not having the object -- because a line that
only records that a sweep happened leaves whoever reads it guessing, and the
whole point of the change is that sweeps should be rare and accounted for.

**Looking for the table is itself a sweep, so looking is bounded.**
`TABLE_SEARCH_ATTEMPTS` is 3. The retry lives in the main loop's menu branch,
which turns over about once a second, and unbounded that is a full sweep a
second for as long as a runner sits on the menu -- measured at eleven of them
in eleven seconds, which on Windows would be eleven half-minute sweeps. Three
attempts still absorb a transient failure, since memory goes briefly unreadable
during a scene teardown. The count resets when the scene loader is re-resolved,
because that means the process changed underneath us. Giving up is announced,
and leaves the splitter doing exactly what it did before the table existed.

**The fallback is not decoration.** During the *first* scene load of a session
the incoming `GameInitializer` can be on the heap before the runtime has a
reference to it: the table reports zero instances and the sweep finds it. That
is once per session and early, while the process is still small — 894 MiB in
the recordings, against the 5195 MiB the same sweep would cost by the second
game — and by the second load there are two initializers in the table and no
sweep happens at all.

It is a race rather than a rule. Every recording replays it, and the live
Windows run did not hit it at all: the initializer was in the table on the first
game too. So it is timing-dependent — which is the argument for keeping the
sweep behind the table rather than for expecting to need it.

## Split sources

All paths below were verified to exist in the shipped assemblies.

| Split | Path | Type |
|---|---|---|
| Run start | `GameInitializer._initializationState` crossing into `ShowUI`, gated on the scene load saying a new game | enum, object graph |
| Buildings finished | `GameOverChecker._entityRegistry` → `EntityRegistry._entitiesInInstantiationOrder`, each entity's `_componentCache` → `ComponentCache._name`, and its `BlockObjectState._state` | `List<EntityComponent>`, `string`, `State` enum |
| Wonder unlocked | `BuildingUnlockingService._unlockedBuildings` | **`HashSet<string>`** |
| Wonder activated *(logged, not split)* | `WonderCompletionCountdownStarter._unlockDay` | `int` |
| Run end | `WonderCompletionCountdownStarter.CountdownFinished` | `bool` |
| Day | `DayNightCycle.DayNumber` | `int` |

Field types were read out of the assemblies offline (`devtools/metadata.py`),
which settled how much machinery each split needs:

- **The unlock is a string membership test**, not an object graph walk —
  `_unlockedBuildings` holds template names directly. It needs a
  `HashSet<string>` reader and nothing else. Timberborn has no research that
  runs over time: science is banked, and clicking a building in the science
  tree unlocks it instantly, so the set gains a name the moment the player
  spends. There is no progress to watch, only an event.
- **`ComponentCache._name` holds the template name outright** — sampled from a
  live game it reads `"WoodWorkshop.Folktails"`, `"TappersShack.Folktails"`,
  and `"BlueberryBush.<guid>"` for natural entities. That removes the
  `TemplateSpec` lookup the buildings split would otherwise need. On a live
  entity the name is suffixed `.EntityComponent`, while prefabs carry the bare
  template name, so matching is on a `.`-separated prefix — which also stops
  one template matching a longer one that merely starts the same way.
- **The run end is a single bool** on a located singleton. See *Category rules*
  below for why it is not wonder activation.

The buildings split reads `BlockObjectState._state` (`Unfinished`, `Finished`,
`Preview`) directly, but never scans for it: the entity is found first, and the
state is one of its components. See *Every building, not just the ones a
district happens to know about* below for why the districts' own
finished-building registries turned out not to be usable.

### Watch a count that moves when the thing you care about happens

The building splits once fired in batches: Forester and Gear Workshop together,
then the remaining three all at once when the wonder finished — buildings that
had completed minutes earlier.

The trigger was `_registeredComponents._count` on a district's finished-building
registry. That dictionary is `Dictionary<Type, List<IRegisteredComponent>>`, so
its count is the number of component **types**, not of buildings. It only moves
when a type is registered for the first time, and the rescan it triggered then
discovered every target finished since.

The lesson survives the redesign that followed: **poll a number that moves when
your event happens, not one that merely tends to.** The count now watched is
the length of the global entity list, which moves the moment anything is
created — and each building's own `_state` is read every tick once found, so
the split fires on the tick it changes.

### Every building, not just the ones a district happens to know about

The registries that replaced that count were `DistrictBuildingRegistry._finishedBuildings`,
one per district, and they are **not a complete list of finished buildings**.

Measured on an Iron Teeth endgame save with two districts:

| | |
|---|---|
| `DistrictBuildingRegistry` instances found | 4 (stable across repeated scans) |
| ...with a non-empty `_registeredComponents` | 2 |
| districts actually in the settlement | 2 |
| distinct building templates visible across those registries | 66 |
| Numbercrunchers built, and visible in them | 4, and **none** |

The two empty registries are stale objects — the same lingering-after-teardown
behaviour documented under *The presence of an object is not an "in a game"
signal*. The other two are live and hold 2,704 components, including every
other tracked building, but no Numbercruncher and no ScienceCounter, though
four and five of those respectively exist as live entities. Nothing about the
scan is at fault: it finds exactly four registries, twice, matching the four
`DistrictBuildingRegistry` objects in memory.

A wrong answer here is silent. The split simply never fires, which is
indistinguishable from the building never being built.

So the source is now the **global entity registry**, which has no district in
it at all:

```
GameOverChecker (a singleton, so the DI container has it -- see Locating services)
  -> _entityRegistry
     -> _entitiesInInstantiationOrder : List<EntityComponent>   (16,177 on that save)
        -> entity._componentCache -> _name          e.g. "Numbercruncher.IronTeeth.EntityComponent"
        -> entity._componentCache -> _components -> the BlockObjectState -> _state
```

`EntityRegistry` has no `_eventBus` of its own, so it has to be reached by
dereference from something that holds it, the same way `DistrictBuildingRegistry`
was reached through its factory. `EntityService` is the obvious holder and was
the original route, but it turns out **not** to be a singleton and so is not in
the DI container. `GameOverChecker` is, and holds the same registry.

Nothing about the game-over feature is used or wanted here; it is simply a
singleton with a reference to the registry, which is what saves a heap scan.
If a game update removes it, it fails by name like everything else.

Two properties make this cheap despite the list being tens of thousands long:

- **Buildings are entities from the moment they are placed**, not from the
  moment they finish. So a tracked building is discovered while it is still
  under construction, and from then on the split needs one read per tick of its
  own `_state`. On that save, 20 buildings are watched out of 16,177 entities.
- **Entities are appended**, so ordinarily only the tail is new. A removal
  shifts the tail down by one, which can slide an uninspected entity below the
  mark — so a shrink of `k` rewinds the mark by `k`. The work is proportional to
  churn, never to the length of the list. Re-inspecting an entity is harmless:
  watches are keyed by address.

That second point was learned by measurement. The first version re-walked all
16,000 entities whenever the count shrank, and the count on a live endgame save
changes by ±1 every few seconds — so a re-walk was running much of the time,
and the tick rate fell to 82/s. Rewinding by the shortfall instead brought it to
98.6/s, faster than the district registries it replaced (88.8/s) and close to
the 107/s floor of an idle splitter.

A watched building can also be demolished, leaving an address the allocator may
reuse. So the state is read every tick, but the object's vtable is confirmed
before anything splits — a check that costs nothing until something claims to
be finished.

Verified both ways on the live game. On the endgame save, all six tracked
buildings are found as already finished on arrival, Numbercruncher included, and
nothing splits. On a fresh Iron Teeth game, every condition fired once, in
order, on the day it happened rather than in a batch at the end: Forester (day
2), Numbercruncher and Gear Workshop (day 4), Tapper's Shack (day 5), Smelter +
Wood Workshop and the wonder unlock (day 8), then the wonder activating on day
11 -- logged as *not* the run end, with completion predicted for day 11.372 --
and the Congratulations screen firing separately when it arrived.

### A class can only be located through a field it has

`Locatable` validates a scan hit by dereferencing a field and checking what it
points at. Most Timberborn services hold an `_eventBus`, so that is the default —
but it is not universal, and a class validated through a field it does not have
can **never** be located. The scan finds it, the validator rejects every hit,
and the result is indistinguishable from "not present".

This has bitten twice: `WorldDataService` (not a DI service, statics only) and
`DistrictBuildingRegistry` (validated through `_entityComponentRegistryFactory`
instead). Both failed silently — the second cost a full test run in which the
building splits simply never appeared. `EntityRegistry`, which the buildings
split uses now, has the same shape and is handled the same way: it is reached
by dereference from `GameOverChecker` rather than located on its own.

Two defences:

- `devtools/metadata.py check` reads every `Locatable::new` and
  `with_validator` call out of the source and verifies the class really has the
  field it is validated through. It catches this with the game closed.
- Resolution failures explain themselves on the first attempt rather than
  retrying in silence. "Not constructed yet" is normal during a load, so later
  attempts stay quiet.

### Faction-specific template names

Where the factions build different things, the names are not guessable and one
split carries both — only the one belonging to the faction being played can
ever fire, since the two factions are separate categories:

| faction | wonder | advanced science building |
|---|---|---|
| Folktails | `EarthRecultivator.Folktails` | `Observatory.Folktails` |
| Iron Teeth | `EarthRepopulator.IronTeeth` | `Numbercruncher.IronTeeth` |

Note the lowercase `c` in `Numbercruncher`, and that Iron Teeth have no
Observatory at all.

Template names are checkable with the game closed, which is the cheap way to
settle one: `Timberborn_Data/StreamingAssets/Modding/Blueprints.zip` holds a
`*.blueprint.json` per building, each carrying its own `TemplateName`.

Guessing costs test runs. `TributeToIngenuity.IronTeeth` reads like a wonder and
is not one — it is a monument, alongside `FarmerMonument.Folktails` and
`LaborerMonument.IronTeeth` — and assuming it was cost an Iron Teeth run during
which the unlock split simply never fired. The confirming evidence for the
real pair is the localisation keys `Buildings.Wonder.EarthRecultivator` and
`Buildings.Wonder.EarthRepopulator`.

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

Run start (the overlay appearing) and run end (the Congratulations screen) are
the two splits whose accuracy actually matters; the intermediate ones only
affect segment times.

There is **no watchpoint mechanism**. The sandbox offers read-only memory access
— no ptrace, no debug registers, no write traps — so a change can only be
noticed by looking again.

That is fine, because scanning and reading are different costs. Locating an
object is a multi-GiB scan; re-reading a known address is one byte. So anything
timing-critical is located once and then polled **every tick**, and only the
scan is throttled. Getting this wrong is easy: the first version rescanned every
2 seconds, which would have put 2 seconds of error on the run-end split.

Resulting error is one tick, worst case. Both hosts were measured on this
machine, by counting ticks in the module against a wall clock:

| host | tick rate | worst-case error |
|---|---|---|
| asr-debugger | 120/s | ~8.3 ms |
| LiveSplit 1.8.29 (ASR, under Proton), no game attached | ~107/s | ~9.4 ms |
| LiveSplit, attached and watching a loaded endgame save | ~99/s | ~10.1 ms |

Average error is half of that. The bottom row is the one that matters, and it
is still an order of magnitude inside speedrun.com's 0.01 s display precision.

The LiveSplit figure is a **ceiling of 120/s that it does not quite reach**,
not a slower clock. `ASRComponent` runs a `System.Timers.Timer` whose interval
is initially 15 ms, but every `UpdateTimerElapsed` re-reads
`Runtime_tick_rate()` and assigns it to `Interval` — so after the first tick
the interval is whatever the module asked for, which is asr's default 8.33 ms.
The gap to 9.4 ms is the handler stopping the timer for the duration of the
step and restarting it afterwards: the period is the interval *plus* the work,
so a heavier step means a slower tick. Idle, with no game attached, that
overhead measured a steady ~1.0 ms.

**The splitter's own per-tick work is the rest of the gap**, ~0.8 ms per tick
on top of the idle 9.4 ms while watching that save. Most of it is one read per
watched building, so it scales with how many tracked buildings have been built
(20 on that save), not with the size of the settlement.

The district-registry design it replaced cost ~1.9 ms, and that one *did* scale
with the settlement — one read per per-type component list, 35 of them across
4 registries. Both numbers come from the same save, with the watcher disabled
for comparison.

Two further consequences:

- **Under scan load the rate drops further still.** A scan slice costs 10–19 ms.
  This is exactly why the splits themselves never wait on a scan, and why
  everything timing-critical is located before it is needed.
- **Every tick-count constant in `lib.rs` is a lower bound on wall time** —
  ~12% longer than its "~Ns" comment when idle, ~35% longer when watching this
  save. They are all retry and log intervals, so this does not matter, but it
  is why they are phrased as approximations.

`asr::set_tick_rate` can ask for a different rate and LiveSplit will honour it,
since it re-reads it every step. There is no reason to ask for **more**: that
would only shrink the interval the step time is added to, and the step time is
what is actually limiting us.

Asking for **less** is worth it in one place. While no game is attached the
module does nothing but list processes, and a game appears on a human
timescale, so the search loop drops to **1/s** and `main` puts it back to 120/s
the moment it attaches (`ATTACHED_TICK_RATE`, `DETACHED_TICKS_PER_SEC`). A
runner leaves LiveSplit open for hours with no game running; that is 120
wakeups a second buying nothing. It costs up to a second of extra attach
latency, against a game that takes tens of seconds to reach a menu.

The catch is that a tick is no longer a fixed slice of time, so the constants
the search loop counts in are written as `detached_ticks(secs)` and are exact
seconds — including `PROCESS_GONE_DELAY_TICKS`, which is why `main` lowers the
rate before that wait rather than inside `attach`. Everything counted while
attached is unchanged.

Verified 2026-09-03 across two start/quit cycles, in LiveSplit One Druid (which
logs the runtime's `New Tick Rate` line at `DEBUG`) and in a headless host that
counts ticks against a wall clock. Detached, measured 1.0 ticks/s; attached,
120.0. The two `New Tick Rate: 1` lines a quit produces are `main`'s and
`attach`'s, five seconds apart — the process-gone wait, in real seconds, which
is the part that would have silently become 40 ms had the constants stayed in
120 Hz ticks. "Still looking" arrived 14 s after each drop, as its 15 detached
ticks intend.

Both run start and run end get this treatment: `GameInitializer` is bound while
the scene is still loading and `WonderCompletionCountdownStarter` long before
the countdown ends, so each is located well in advance and then polled every
tick, with no scanning in either critical window.

### Reading BCL collections

`HashSet<T>`, `List<T>` and `Dictionary<K, V>` layouts belong to Unity's Mono
BCL, not to Timberborn, so they move on a Unity upgrade rather than a game
patch. They are resolved by name like everything else (`src/collections.rs`),
via `Class::of_object` — a generic instantiation such as `HashSet<string>`
cannot be looked up in an image by name, but any instance points at its own
class, and from there field offsets resolve normally.

That is the primary path and it usually works. It is not reliable, and the way
it fails is worth knowing: **Mono fills a class's field table in lazily, and
for an inflated generic nothing necessarily has.** Measured against a live
game, `Class::of_object` resolved the class of `_entitiesInInstantiationOrder`
and then reported *none* of `_size`, `_items` or `_version` — while the object
itself was plainly a list, 5032 entries in an 8192-slot array. The identical
lookup succeeded against a different process running the same build, so this is
runtime state and nothing a version check could catch.

Both collections the splitter reads hit this in one session: the entity
registry's `List<EntityComponent>`, which silently stopped every building
split, and `_unlockedBuildings`' `HashSet<string>`, which silently stopped the
wonder-unlock split. Neither is visible to either half of the version check —
`metadata.py check` reports ALL RESOLVED and `probe.rs` reports the fields
`ok`, because both verify the names on the *owning* class, not the fields of
the generic collection it points at. It presents as "the building splits
sometimes just do not work", which is exactly the sort of thing that gets
written off as flaky.

So each has a fallback to the layout, taken only when the object agrees with
it: for a list, `_items` must point at an array whose length covers `_size`;
for a set, `_slots` must be a real array with `_lastIndex` inside it and
`_count` inside that. A genuine BCL change fails there rather than quietly
producing a wrong count. Those layouts were read out of a live game rather than
assumed — `src/collections.rs` records the measurement.

What genuinely has to be hardcoded is the shape of the Mono runtime's own
object headers — `MonoArray`'s length and data offsets, `MonoString`'s length
and characters, and `Slot<T>`'s layout inside a `HashSet`. These are not
managed types and have no named fields to resolve. They are part of the Mono
ABI rather than of a BCL implementation, and correspondingly more stable: if
`Slot<T>` ever changed shape, its declared fields would have changed with it
and the name lookups would fail first, loudly.

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

That is why the split is named `congratulations_screen` rather than for the
wonder: the wonder is the prerequisite, the screen is the end time. Splitting on
activation would be early by the length of the countdown.

**Measured: 0.5 in-game hours, which is 9.6s of real time at 1x** (a day is
16+8 hours in 460.8s).

Treat 9.6s as an order of magnitude, not a constant. The countdown is in in-game
hours, so its real-time length scales with game speed — a run at 3x sees ~3.2s —
and stretches further if the simulation cannot keep up or if maximum
acceleration is capped as a settlement grows. `DayLengthInSeconds` itself
appears not to be population-scaled: it read 460.80002 in both a brand-new game
and a day-1511 save.

None of that affects the splitter, which reads `CountdownFinished` directly. The
conversion exists only to put a real-time size on the gap.

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

What is watched for is the *crossing* into it, not the state itself: `ShowUI`
can last less than a tick and be missed entirely. See *Binding the run start
before the overlay*, which is where the whole of this turned out to be harder
than it looks.

`SpeedManager.CurrentSpeed` was the other candidate and is unusable. It becomes
1 at `UnpauseGame`, one step early, and then toggles every time the player
pauses, so it is neither correctly placed nor monotonic.

`GameInitializer` is bound while the scene is still loading and then polled
every tick, including through the clock scan. Run start gets the same one-tick
accuracy as run end, with no scanning in the critical window. Binding it any
later does not work at all.

Like the run end, this fires only on an observed transition: a state below
`ShowUI` has to be seen on that same instance first, so attaching to a game
already in progress does not count as a start.

**Loading a save walks the identical sequence**, confirmed by measurement, so
`initializationState` alone cannot tell a load from a new game and would fire a
run start on both. The discriminator is the scene load's own parameters — see
*What is being loaded, and where it came from*. The static
`WorldDataService.SourceFileName` (null on a new game, set to the save being
loaded otherwise) was the original discriminator and remains the fallback for
attaching with no load watched.

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

Retries cost a scan, so they are spaced (~2s). That is fine for every watcher
except the run start, which cannot be resolved on a retry interval at all —
see *Binding the run start before the overlay*. It was once believed that the
settlement-name dialog left ample time to resolve before the state could leave
`Waiting`. Measurement says the opposite, and that belief cost several test
runs.

### Nothing may sample saved state before initialization finishes

Two false splits in one test run traced to the same mistake, so it is worth
stating as a rule.

Loading a save that had already completed its wonder fired a run *end* five
seconds into the load. `CountdownFinished` was sampled as `false` while the
scene was still loading, and `GameWonderCompletionRestorer` then restored the
saved `true` — which looks exactly like a completion happening.

The "only fire on an observed transition" guard is necessary but not
sufficient: the baseline has to be taken at the right time as well. So anything
reading persisted state waits for `_initializationState` reaching `Finished`. Run start
is the exception, and has to be, since it fires on the crossing into `ShowUI`,
one step earlier — but it reads only a state machine, never saved data.

### The presence of an object is not an "in a game" signal

Worth keeping in mind for any future condition, because it is tempting and
wrong. After exiting to the main menu, the `DayNightCycle` instance was still
found at the same address ~20 seconds later, still reading the last in-game
`DayNumber`. It is either uncollected or deliberately retained; either way a
splitter watching for the object to disappear would think a run was still in
progress.

Absence is meaningful at process start — a class has no vtable until it is
first instantiated, confirmed by a 41 second wait on the main menu — but not
after a game has been loaded once. Anything that needs to know whether a game
is in progress has to read a state machine, not infer it from object lifetime.
That is why run start reads `GameInitializer` rather than watching for
`DayNightCycle` to appear.

The same reasoning, applied to "is this still the same game?", is what replaced
object lifetime with the scene loader — see the next section. Inferring it from
whether a held object still read was the single longest-lived bug in this
project.

## Locating services through the DI container

A heap scan is 1-2s under Proton. A loaded game used to cost five of them --
clock, run start, wonder unlock, run end, entities -- because each service was
found on its own. It now costs two.

Timberborn builds its services with a DI container that keeps every singleton
it made in one array:

```
SingletonRepository._singletonListener
  -> SingletonListener._allSingletons : ImmutableArray<object>
```

`ImmutableArray<T>` is a struct wrapping one `T[]`, so the field slot holds the
array reference directly. Finding the container costs a scan; every service in
it is then a lookup, matching each entry's vtable against the class we want.

Measured against the live game, one save loaded, three containers alive at once:

| container | singletons | holds |
|---|---|---|
| one | 20 | none of ours |
| another | 103 | none of ours -- the main menu's |
| another | 612 | every service the splitter watches |

So the right one has to be picked out, which is what "the container that has a
`DayNightCycle` in it" does.

`EntityService` is **not** a singleton and is not in there. `GameOverChecker`
is, and holds the same `EntityRegistry` the buildings split actually wants, so
that split needs no scan of its own either.

### What it is not

It is **not** an "is a game loaded" signal. The game's container is still alive
at the same address after exiting to the main menu, holding the same
`DayNightCycle` -- measured, not assumed. It lingers exactly like the loose
objects do. That question belongs to the scene loader; see the next section.

For the same reason the container of the game just left is skipped when the
next one is resolved, exactly as the loose clock instance used to be.

### Two things that cost a test run

**The scan limit truncated the search.** A limit of 16 looked generous against
three containers. But every abandoned game leaves one behind until it is
collected, they accumulate over a session, and the scan runs in address order
with no guarantee a fresh allocation comes last. The new game's container fell
off the end of the limit and the splitter bound the *previous* game's services
-- presenting as a game that already had its wonder unlocked and its Forester
built. The limit is now a safety valve, not a working number.

**Reading the container has to yield.** Identifying a singleton means reading
its vtable, one round trip through Wine apiece, and 612 of them in a single tick
is enough to stop the game responding -- it went through the same `wineserver`
the game itself depends on, and Steam put up "not responding" during a load. The
contents are read once into a snapshot, a chunk per tick, and every lookup after
that is free. The snapshot is retaken on the retry path, because
`_allSingletons` is immutable: registering another singleton replaces the array,
and services are retried precisely because they do not all exist at once.

## Lifecycle: telling one game from the next

Everything above finds objects. This section is about knowing which *game* they
belong to — which is where the splitter's worst bugs have lived. Symptoms were
always the same shape: splits that fired on one run and silently did not on the
next, depending on nothing the runner could see.

### The scene loader is the anchor, not object lifetime

The splitter used to decide "are we still in the same game?" by checking whether
an object it held still read successfully. That does not work, and the way it
fails is instructive: a freed object keeps returning values until its memory is
reused, and then returns whatever now owns it. Captured in a log, the previous
game's `GameInitializer` read as a garbage number and then as a plausible `0` —
indistinguishable from a game beginning to load. Watchers stayed bound to the
*previous* game and its splits could never fire. Whether it broke at all
depended on GC timing, which is why it presented as "sometimes works".

`Timberborn.SceneLoading.SceneLoader` replaces that inference with an
observation. It loads every scene including the main menu and outlives all of
them — measured at one address across two new games, the load-game menu,
deleting saves, and a LiveSplit restart. Its `_isLoading` gives a positive edge,
so a scene change is *seen* rather than deduced.

It is not a DI service and has no `_eventBus`, so it is validated through the
`_assetLoader` it holds.

Three rules follow:

- A load starting ends the watch. Everything held belongs to the scene being
  replaced, whether or not it still reads.
- Nothing found during a load is trusted for the world coming out of it, with
  one deliberate exception below.
- A search skips the instance just left, since the outgoing game's objects can
  still be alive and a scan would happily hand one back.

### What is being loaded, and where it came from

`SceneLoader._sceneParameters` says which scene is coming: the parameters'
class distinguishes `MainMenuSceneParameters`, `MapEditorSceneParameters` and
`GameSceneParameters`, and for a game scene, whichever of
`<NewGameConfiguration>` and `<SaveReference>` is set says new game versus
loaded save.

That belongs to *this* load. `WorldDataService.SourceFileName`, which the
splitter used before, is a process-wide static that keeps a stale value — a
live suspect for the original Windows failure to start on a second game. It
survives only as the fallback for attaching with no load watched.

The field is sampled throughout the load, not on the rising edge: on the tick a
load starts, it still holds the *previous* load's parameters.

That same staleness is useful on attach. The loader is persistent, so its
parameters still describe the last load even though we were not watching when
it happened -- so attaching reads them once and starts from a real answer
instead of `Unknown`. This matters because `Unknown` has to assume a game may
be loaded, and on the main menu that is wrong: the previous game's objects are
still alive and readable there, so the splitter bound to the last game's
`GameInitializer`, read `Finished`, and announced "Game already in progress"
while the runner sat on the menu.

The same reasoning says not to go looking for a `GameInitializer` at all when
the scene is the menu or the map editor. The watch loop retries resolving one
on an interval, and that retry ignored the scene, so seeding the state was not
by itself enough to stop the message.

### Binding the run start before the overlay

Run start is the one watcher that cannot be resolved on a retry interval, and
it took four separate bugs to get right. It is worth spelling out, because
every one of them is a trap the next watcher could fall into.

**It must be bound during the load.** Resolving it after the load finished is
always too late. The window between the load ending and the overlay is about as
wide as one heap scan — settlement naming happens inside it — so the first read
said `Finished` every time. During the load there are seconds of slack, and the
incoming game's initializer already exists in a pre-overlay state. This is the
deliberate exception to "nothing found during a load is trusted".

**A pre-overlay state is not enough to identify it.** The initializer of the
game just left is freed, and its reused memory reads as a plausible `0` —
`Waiting`. That is the same failure the scene anchor exists to avoid, and it
was observed binding to the previous game's address on a second run. The
address just left is skipped, exactly as the clock's is.

**Binding early is not enough if it is not polled.** Bound during the load but
first read only once `watch()` started — after the clock scan — it saw
`Finished` every time, because `ShowUI` came and went inside that one scan. The
clock scan now polls the run start between slices.

**The firing rule is the crossing, not the state.** `ShowUI` can last less than
a tick: an instance watched from `Waiting` was seen going `1 → 3 → 5`, with
neither `2` nor `4` ever sampled. Waiting to observe `state == ShowUI` therefore
discards starts that were tracked perfectly. The test is instead that a
pre-overlay state was seen *on this instance* and the state is now past it,
which is accurate to one tick and is the only rule that can work at all. Never
having seen one is the genuinely late case: the watcher arrived after the
overlay, how late is unknowable, and it warns rather than starting the timer.

A run start also drops and re-binds the other watchers and clears their
"already done on arrival" state, so one bad bind cannot poison the session.
`WonderUnlock` is why: binding to a game where the wonder was already unlocked
latches `unlocked_on_arrival` and disables that split for good.

### Skipping the instance just left can lock out the only one there is

Skipping is right when the outgoing game's object is still alive. It is wrong
when the object was found *during* the load and belongs to the game coming in —
and then the skip excludes the only instance in the process, conclusively, on
every later scan. Observed as a permanent spin of "No DayNightCycle" with a
fully loaded game on screen.

So the skip is given up after three conclusive empty scans. Conclusive matters:
an inconclusive empty scan says nothing (see *Scanning and attaching, learned
the hard way*),
and counting those would give the skip up during an ordinary teardown.

### What this is verified against

One session, on Linux, against the live game: two consecutive menu → new game →
Forester → unlock cycles, both starting the timer and both splitting, plus a
save loaded afterwards correctly refusing to start. The second cycle is the case
the whole design exists for — the first game alone cannot exhibit any of the
stale-address bugs, because there is nothing stale yet.

**Not verified on Windows.**

## Saying something to the runner

`asr::print_message` goes to the host's log. In LiveSplit that is `Trace`, which
with no listener configured is nowhere at all -- so every warning the splitter
can produce was invisible in normal use. The one that matters is "bound too
late, not starting the timer": to the runner that is a timer that silently did
not start, which is the same symptom as the bug this splitter exists to fix.

`asr::timer::set_variable` is the way out. The chain, read out of the source
rather than assumed:

1. `timer_set_variable` reaches the ASR component's `setCustomVariable`
   delegate (`ComponentSettings.cs`).
2. That calls `model.CurrentState.Run.Metadata.SetCustomVariable(name, value)`.
3. LiveSplit's **Text** component displays it when "Custom Variable" is ticked
   and the variable's name is in the second box.

`src/status.rs` wraps this. Warnings only: a status line that usually says
something is a status line nobody reads.

### It does not reach the runner's splits file

Worth being certain about, since writing status strings into someone's `.lss`
would be unforgivable:

- `SetCustomVariable` goes through `GetOrAddCustomVariable`, which constructs
  the variable with `IsPermanent = false`.
- `XMLRunSaver` writes a custom variable only `if (entry.Value.IsPermanent)`.
- `SetCustomVariable` sets `HasChanged` only for permanent variables, so this
  does not even make LiveSplit think the splits need saving.

Only a variable the runner added by hand in the Run Editor is permanent, so the
one hazard is a name collision with one -- which is why the name is
`Timberborn Autosplitter` rather than something like `Status`.

livesplit-core agrees, for LiveSplit One and asr-debugger: its `CustomVariable`
documents auto splitter variables as temporary, and its `.lss` saver filters on
`is_permanent` the same way.

### Blank when there is nothing to say

Setting the variable to the empty string blanks it in desktop LiveSplit:
`CustomVariableValue` returns `""`, which is not null and so does not hit the
component's `?? DASH` fallback. Leaving the component's first text box empty as
well means the row draws nothing at all. It still occupies its height -- a
component cannot give that up -- but it is blank.

That reserved height is why the component sits under the split list rather than
under the title, where an always-empty row separating the title from the first
split was too conspicuous to live with.

livesplit-core hosts differ: their text component filters empty values
(`.filter(|value| !value.trim_start().is_empty())`) and substitute a dash, so
there an empty message shows `—`. No value is blank in both.

### Verified

Against LiveSplit under Proton: the warning renders in amber, and the shipping
configuration -- empty label, empty value -- draws a blank row. Both confirmed
by screenshot, along with a blank line on the main menu and through a full
menu -> new game -> Forester -> unlock cycle that started the timer and split
twice without saying anything.

The two ways of arriving after the overlay say different things -- `Run start
missed` and `Game already in progress` -- even though the runner's response to
both is the same. A screenshot in a bug report should say which happened
without needing the log.

Two things cost time getting there and are worth knowing:

- **The layout LiveSplit opens is the last entry in `RecentLayouts` in
  `settings.cfg`**, not whichever `.lsl` looks like the obvious one. Edits went
  to a file nothing was reading for several rounds.
- **A hand-written Text component node must carry `Font1` and `Font2`.** The
  installed build writes them back out even when `OverrideFont` is false, and
  `getFontHashCode` throws on null -- once per layout-hash check, so it spams
  rather than failing once. `SetSettings` tolerates their absence, so the
  component loads and renders correctly; only saving breaks.

## Risks

1. **Search performance** — settled, see *Reading fewer bytes* and
   *Measurements*. A session pays two full sweeps at the main menu, where the
   process is smallest, and every search after that reads the reference table
   and the pages it names: 20–70 MiB against a sweep of 4–5 GiB. Both paths are
   resumable, so neither stalls the splitter's update loop. The residual risk is
   a game or engine update that stops the table being findable, which costs
   speed rather than correctness — the sweep is still behind it, and the log
   says which path answered.
2. `List<T>` / `HashSet<T>` / `Dictionary<K, V>` internal layout is Unity's Mono
   BCL — stable across game patches, but moves on a Unity upgrade. Resolved by
   name, so a move shows up as a failed lookup rather than a bad read.
3. Class or field renames in a game update. Caught by `devtools/metadata.py
   check` offline, and by the probe at runtime — see *Checking a new game
   version*.
4. New template names for a future faction. These fail silently by nature and
   are the one thing name resolution cannot catch for us; see *Faction-specific
   template names*.

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
  whatever that entry points at. So it must be at least as good on day one. The
  split set stays close to the existing one for that reason; the two departures
  (see *Background*) change what a split is named and which factions it covers,
  not how many there are or their order, so an existing `.lss` still lines up.

Worth opening that conversation early rather than at the end. A joint handoff is
a much better outcome than arriving with a finished competing implementation,
and it may be that the right home for this is `timberborn_speedrun` itself — in
which case the `_wasm` naming question above comes back.

## Measurements

Taken against the game (Timberborn under Proton, `asr-debugger`), and kept
because they are the evidence behind decisions above rather than history.

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

### Windows, natively

Measured against the same game version (Unity 6000.3.6f1) with real LiveSplit
attached. The Proton numbers above do not carry over.

**A sweep is an order of magnitude dearer**: 6906 MiB across 3043 ranges in
29s, ~240 MiB/s. Read failures are not the reason — 83 chunks failed out of
~110,000 on one run, so the page-by-page fallback is not what costs. Volume is.

**Process size is the variable that matters.** A fresh process is 0.65 GB at
the main menu and 6.3 GB with a game loaded, and it keeps growing across games.
So how bad this gets depends on how long the process has lived, which is why a
first game can look fine and a second cannot, and why none of it reproduces on
a small process. It is also why the reference table matters most here: what it
reads does not grow with the process.

#### Linux, with the reference table

Six games in one process under Proton -- a new game, a save with 16171 entities
loaded to fill the table, then four more new games with a trip through the main
menu between each:

| | |
|---|---|
| Scene loads | 5 |
| **Full sweeps** | **1** -- the anchor, at startup, 329 of 857 MiB |
| Everything else | served from the table |

The run-start retry is visible on every single load, as a pair of lines a second
or two apart: the first look finds only the previous game's initializer, which
the skip predicate rejects, and the next finds the incoming one. Each of those
pairs was a multi-gigabyte sweep in the session before the retry existed.

`SingletonRepository` counts moved 3, 4, 3 across the games as containers were
built and collected, which is the live-set behaviour inferred from a capture,
now watched in motion.

The self-healing never fired, because the table was never lost. That is the
better of the two outcomes: under exactly the pressure expected to break it --
a 16k-entity save and six games in one process -- it stayed where it was found.

#### Windows, with the reference table

LiveSplit 1.8.37, game build 1.1.2.4. Two new games, an end-of-run save with
16171 entities, then a fourth new game, with a trip through the main menu
between each. Timings are arrival stamps from tailing the trace log, which
carries none of its own.

| | |
|---|---|
| Scene loads | 4 |
| **Full sweeps** | **1** -- the anchor, on the menu at startup, 529 of 626 MiB |
| Everything else | served from the table |
| Process by the end | 7.4 GiB |

The two startup costs, both now above `Waiting for the game to load...`: 1.65s
for the anchor sweep and 2.16s for the table search, 3.81s together, on an idle
menu with nothing pending. Before the ordering fix the same work landed 2.7s
into the first load.

The sweep is also *cheaper* than it was -- 626 MiB against 704 -- because it is
no longer racing a load that is already growing the process, and references to
the anchor are down from 27 to 10 for the same reason. Both were predicted from
the recordings, where the menu offers four or five candidate ranges against
seven to ten in a game.

No `[table] no candidate range was one`, no `no reference table after three
tries`, and not one sweep with a reason after it.

An earlier Windows session, before the fixes, is what the figures above it
describe: it swept 6151 MiB to find `SingletonRepository` and would have swept
~5 GiB again for the second game.

### Scanning and attaching, learned the hard way

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

Windows reports the executable name, so runners there match on it directly and
never reach the ambiguous path. That branch has only ever run against the game
on Windows, where a full Folktails run confirmed it. Linux and Steam Deck
players get the truncated name instead, so it is handled rather than worked
around:

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
| `GameOverChecker` | `_entityRegistry` | measured at runtime | not measured |
| `EntityRegistry` | `_entitiesInInstantiationOrder` | `+0x18` | not measured |
| `BlockObjectState` | `_state` | `+0x30` | not measured |
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

Mono reports as V3, 64-bit on both. asr-debugger ticks at **120/s**, confirmed
by a 1800-tick counter firing every 15 seconds; LiveSplit measures **~107/s**
idle and **~99/s** attached, see *Split latency*. Both were measured by counting
ticks in the module against a wall clock.

Doing that in LiveSplit means capturing its log output, which takes a little
setup: LiveSplit writes no log of its own, and `print_message` reaches
`Trace.TraceInformation`, which goes nowhere until a listener is configured.
Adding one inside `<configuration>` in `LiveSplit.exe.config` is enough, with
`autoflush` so lines are not left sitting in a buffer:

```xml
<system.diagnostics>
  <trace autoflush="true">
    <listeners>
      <add name="file" type="System.Diagnostics.TextWriterTraceListener"
           initializeData="asr.log" />
    </listeners>
  </trace>
</system.diagnostics>
```

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

## Running it in asr-debugger

```bash
cargo build --release
```

Load `target/wasm32-unknown-unknown/release/timberborn_autosplitter.wasm` in
[asr-debugger](https://github.com/LiveSplit/asr-debugger), then **load a save**.
`DayNightCycle` is located first and gates everything else; it does not exist in
the main menu, so a scan from there correctly finds nothing. Re-running after a
code change is a "restart" in the debugger; the game can keep running.

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
zero. The probe (see above) runs once at this point and logs every class and
field it resolved.
