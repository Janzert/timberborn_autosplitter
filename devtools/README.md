# devtools — development only

**Nothing in this directory is ever shipped to runners or loaded during a run.**
speedrun.com does not allow mods to be running for a submitted run, and the
whole point of this auto splitter is that it needs none.

This is a Timberborn mod used as a **test oracle** while developing the
splitter and when adding support for a new game version. It dumps ground truth
that the splitter's memory reads are diffed against:

- runtime addresses of each service instance, to check the heap scan found the
  right object
- resolved field offsets per game version, as a fixture to diff after an update
- the contents of `_allSingletons`, and the layout of `ComponentCache._components`
- whether `ComponentCache._name` already holds the template name (see DESIGN.md)
- a timestamped golden trace of a real run, to replay against the splitter offline

Adding support for a new game version becomes: run the game with this mod, dump,
diff the fixture, fix whatever got renamed.

Not implemented yet.
