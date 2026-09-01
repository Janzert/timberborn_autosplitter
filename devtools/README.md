# devtools — development only

**Nothing in this directory is ever shipped to runners, and nothing here
touches a running game.**
speedrun.com does not allow mods to be running for a submitted run, and the
whole point of this auto splitter is that it needs none.

## `metadata.py`

Reads .NET metadata straight out of the game's assemblies. No mod, no running
game, no mono or ilspy — it parses the ECMA-335 tables directly.

Since the splitter resolves everything by name, "did a game update rename
something" is answerable entirely offline:

```bash
./metadata.py check ~/.steam/steam/steamapps/common/Timberborn/Timberborn_Data/Managed
```

That checks every name `src/probe.rs` depends on. Run it after switching Steam
branches: a clean result means any `MISSING` the runtime probe reports is a real
change rather than a typo in the probe.

`./metadata.py dump <assembly.dll>` lists every class and field in an assembly,
with each field's declared type, which is how the split sources in
`docs/DESIGN.md` were found in the first place:

```
Timberborn.SingletonSystem.SingletonListener  _allSingletons  ImmutableArray<object>  instance
```

The type is what says which reader a field needs, and guessing it is a good way
to lose an evening. `_allSingletons` being an `ImmutableArray<object>` rather
than a keyed collection is why services are found by walking it and comparing
vtables; `_typeCache` sitting next to it is a `Dictionary<Type, bool>`, which
looks like a lookup table until you read the type and see it answers "is this a
singleton", not "which one".

Signatures are decoded from the `#Blob` heap: primitives, classes, value types,
arrays and generic instantiations. Every field in the current install decodes;
anything it cannot render says so in place rather than silently rendering
something plausible.

## Why there is nothing else here

Field offsets were once going to come from a purpose-built mod. They do not
need to: `metadata.py` reads the names with the game closed, and
`src/probe.rs` resolves them against the live process from outside. Between
them there is nothing left for a mod to tell us, so none was written.
