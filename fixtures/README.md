# fixtures — the game's layout, as facts

A fixture is what one build of Timberborn's memory is *shaped* like: the
classes the splitter looks up, the fields it reads off them, each field's
declared type, and the offset Mono gave it.

**These are committed**, unlike snapshots. They are a few hundred lines of
JSON, they can be read in a diff, and they hold no game data — no save, no map,
no bytes out of the game's own address space. From one, the test harness builds
a synthetic process that asr's Mono support can attach to, so the whole default
suite runs on a machine that has never had Timberborn installed.

```bash
cargo test          # builds a world from every fixture here
```

## What is in one

```json
{
  "format": 1,
  "game_version": "1.1.2.4-52e959e-sw",
  "build_id": 25096761,
  "sources": {
    "snapshot": "1.1.2.4-52e959e-sw-run-complete-frozen",
    "managed": ".../Timberborn_Data/Managed"
  },
  "classes": [
    {
      "image": "Timberborn.TimeSystem",
      "namespace": "Timberborn.TimeSystem",
      "name": "DayNightCycle",
      "fields": [
        { "name": "<DayNumber>k__BackingField", "requested": "DayNumber",
          "type": "int", "static": false, "offset": 72 }
      ]
    }
  ]
}
```

`name` is what Mono holds; `requested` is what the splitter asks for, when an
auto-property makes those differ. The builder writes the mangled name into
memory, so a lookup goes through asr's backing-name path exactly as it does
against the game.

The class list is not the whole game. It is exactly what `src/probe.rs` names,
plus the classes the splitter validates an instance through — the set whose
disappearance would break something. A class with no fields is here because
finding the class at all is what the splitter needs.

## Making one

Two halves, from two places, and neither can produce the other:

- **Names, types and static flags** come from the assemblies, parsed by
  `devtools/metadata.py facts`. ECMA-335 metadata has no offsets in it at all.
- **Offsets** are assigned by Mono when it lays a class out, so they exist only
  in a running process — or in a capture of one. They are resolved against a
  snapshot, through the same `Module` the splitter uses.

`tb-fixture` does both and merges them:

```bash
cargo fixture --managed ~/.steam/steam/steamapps/common/Timberborn/Timberborn_Data/Managed
```

It needs the game **installed** (for the assemblies) and a `run-finished`
snapshot **of that same build** (for the offsets) — see
[../snapshots/README.md](../snapshots/README.md). It refuses to pair an install
with a capture of a different version, because a fixture whose names came from
one build and whose offsets came from another would be wrong in exactly the way
nothing else here can detect.

A finished run rather than any capture with a save loaded: Mono loads
assemblies lazily, and at the main menu half of these classes are not present
at all.

## What a fixture deliberately does not record

**Vtable addresses**, and the vtable size that positions a static table after
one. Those are allocation results, not layout — they differ between two runs of
the same build, so recording them would be recording one process's luck. The
builder assigns its own and keeps them consistent.

**Anything about the game's data.** A fixture says a `List<T>` field sits at
+0x18; it says nothing about what was in the list.

**Inherited structure.** A field the splitter asks a class for may be declared
on a parent, and asr's lookup walks the chain without saying where it stopped.
A fixture records the flattened view — which is the view the splitter consumes
— so the synthetic class hierarchy is one level deep.

## Checking one against the game

A fixture and the builder can agree with each other perfectly while both being
wrong about Timberborn — and by this project's record, "we misunderstood the
game" is the expensive kind of bug. So the capture is the oracle:

```bash
cargo snapshot-tests          # includes tests/fixture_vs_snapshot.rs
```

That puts the captured process and the synthetic one in the same world, attaches
asr to each, and asks both every question the splitter ever asks Mono — the
classes, every field offset, the namespaces, the vtables, the static tables. A
disagreement means the fixture no longer describes the game.

It needs a `run-finished` capture of the same build the fixture names, and says
so if there is not one. Nothing in the default suite needs a capture; this is
the one place the two suites meet.

## The layouts with no name

`HashSet<string>` and `List<EntityComponent>` are *inflated generics*. Each
instantiation is a class of its own with its own field offsets, none is in an
image's class cache under a name a lookup could use, and asr cannot read the
name of one at all — so neither half of the usual process can produce them.

`tb-fixture` walks the captured heap to a real instance instead, the way the
splitter does: sweep for the DI container, follow a singleton to the field
wanted, and ask the object what class it is. A layout is then recorded under
the **path walked to reach it**, which is the only identity available and also
the only one a test needs — it asks for "the list the entity registry holds":

```json
"instances": [
  {
    "reached_by": "Timberborn.GameOver/GameOverChecker -> GameOverChecker._entityRegistry -> EntityRegistry._entitiesInInstantiationOrder",
    "role": "list",
    "fields": [ { "name": "_items", "offset": 16 }, { "name": "_size", "offset": 24 } ]
  }
]
```

A fixture can legitimately carry none of these. Mono fills an inflated
generic's field table in lazily and may never do it at all — measured against a
live game, `Class::of_object` resolved the entity list's class and then
reported *none* of its fields while the object was plainly a list. A capture of
such a session yields nothing here, and `tb-fixture` says which and why.
`src/collections.rs` has a shape-checked fallback for exactly that case.

## Adding a build

Keep the old ones. Two fixtures a game update apart are what turn "the splitter
resolves names at runtime, so it survives updates" from a claim into a test:
the suite builds a world from each, and a change that only works against the
newest layout fails against the other. The two committed here already differ —
`ComponentCache._name` moved from 0x68 to 0x60 between 1.0.13.1 and 1.1.2.4.

An older build does not need reinstalling over the current one. A version saved
by `steam_versions/tbver.py` has the same directory layout, so point `--managed`
straight at it:

```bash
cargo fixture --managed ../steam_versions/1.0.13.1-b769e88-sw/Timberborn/Timberborn_Data/Managed
```

Both halves are offline, so nothing has to be running. What is needed is a
`run-finished` capture of that same build, which has to have been taken while
it *was* installed.

A fixture is **faction-independent**: Folktails and Iron Teeth differ in
template-name strings in the game's data, not in classes or field offsets, so
one capture serves both. The faction only appears in a scenario, which supplies
the names itself.
