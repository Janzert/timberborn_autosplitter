# devtools — development only

**Nothing in this directory is ever shipped to runners or loaded during a run.**
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
which is how the split sources in `docs/DESIGN.md` were found in the first
place.

## The oracle mod

Not implemented, and its remit is narrower than first planned.

Field offsets were originally going to come from a mod. They do not need to:
`src/probe.rs` resolves them from outside by name, and `metadata.py` gets the
names with the game closed. A mod is only worth building for what genuinely
cannot be seen from outside:

- ground-truth pointer addresses for each service, to confirm the heap scan
  found the right object rather than a plausible-looking wrong one
- a timestamped golden trace of a real run, to replay against the splitter
  offline instead of re-running the game for every change

Neither is blocking, which is why it does not exist yet.
