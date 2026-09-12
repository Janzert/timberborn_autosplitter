# Changelog

What changed between releases, from a runner's point of view. Dates are the
release date.

## 0.5 — 2026-09-11

### Added

- **A split for an Unlock Iron Teeth run**: **Well-being 15**, which fires the
  moment the settlement's average well-being first reaches 15 — the number in
  the top bar, and the level at which the game unlocks the Iron Teeth. It
  fires whether or not they are already unlocked on your machine, and only
  once: dipping back under 15 and returning does not split again.
  [`examples/Timberborn-UnlockIronTeeth.lss`](examples/Timberborn-UnlockIronTeeth.lss)
  is a splits file for the category, a start and this one split.

  **It ships off, and nobody else need do anything** — the wonder and
  Timberbot splits behave exactly as before.

## 0.4 — 2026-09-09

### Added

- **Three splits for a Timberbot run**: **Smelter**, **Bot Part Factory** and
  **Produce a Timberbot**, the last of which fires the moment the first bot
  walks out of the assembler — when the population overlay starts showing a bot
  count. With the Gear Workshop, which the route shares with the wonder run,
  that is a four-segment category, and
  [`examples/Timberborn-Timberbot.lss`](examples/Timberborn-Timberbot.lss) is a
  splits file for it that serves both factions.

  **They ship off, and wonder runners need do nothing** — the seven wonder
  splits behave exactly as before. Turn on the ones your route hits; there is
  no category to pick, so you can mix them however you actually run.

  One pairing to know about: **Smelter** and **Smelter + Wood Workshop** read
  the same building, the first splitting when the Smelter is done and the
  second waiting for the Wood Workshop too. That is why the solo one ships off.
  Turning both on gives two splits out of one Smelter, which is allowed if that
  is what you want.

### Changed

- **LiveSplit now downloads the splitter for you, and keeps it up to date.**
  Timberborn is in LiveSplit's auto splitter list, so there is no
  `timberborn_autosplitter.wasm` to fetch and no **Script Path** to browse to.
  Open your splits, then **Edit Splits...** → **Activate** once, and LiveSplit
  handles the rest from then on — including every future release.

  **If you are upgrading, undo the old setup.** The previous instructions had
  you add an **Auto Splitting Runtime** component to your layout and point it
  at a downloaded `.wasm`. That component is unaffected by any of this and will
  keep running its own copy, so leaving it in place while activating the
  downloaded splitter runs **two** splitters and splits everything twice.
  Remove it from your layout: **Edit Layout...**, select **Auto Splitting
  Runtime**, `−`.

  `examples/Timberborn.lsl` no longer contains that component for the same
  reason. If you deliberately want to run a specific build rather than the
  current release, adding it back by hand is still how — see the README.

- **The example splits files now spell out every split setting**, rather than
  only the ones that differ from the shipped defaults. LiveSplit stores a
  setting in the `.lss` only once you change it, so the wonder examples used to
  carry no settings at all.

  That is worth knowing because an empty settings block is not "use the
  defaults" — LiveSplit skips it entirely and leaves whatever settings were
  already loaded in place. Opening a wonder splits file after a Timberbot one,
  in the same session, therefore left the three Timberbot splits switched on.
  Now each file says what its category wants and switching between them does
  the right thing.

  Nothing changes for a file you already use, and your own settings are still
  yours: this is only what the downloaded examples start out saying.

## 0.3 — 2026-09-05

Mostly about what the splitter costs while you play. Starting a game used to
mean a multi-second search of the whole process; it no longer does. Two bugs in
the scanning code underneath it are also fixed, one of which could take the
splitter down outright.

### Added

- **A splits file for Iron Teeth**,
  [`examples/Timberborn-Wonder-IronTeeth.lss`](examples/Timberborn-Wonder-IronTeeth.lss),
  alongside the Folktails one — closing the one known gap listed against 0.2.
  Same seven segments; the faction specific ones carry the Numbercruncher and
  the Earth Repopulator, with that faction's icons. Both files are attached to
  the release, so pick the one for the faction you run.

### Changed

- **Starting a game no longer costs a multi-second sweep of the game's
  memory.** The splitter used to search every writable byte in the process to
  find what it needs at each scene change — 1–2s on Linux, up to 29s on
  Windows, and getting slower with every game started in one session, because
  the process keeps growing. It now finds Unity's own table of live objects
  once, at the main menu, and reads that instead: 20–70 MiB rather than 4–5 GiB,
  and it does not grow. Measured on Windows across four scene loads, including
  an end-of-run save with 16171 things in it: **one** search of memory in the
  whole session, on the main menu before any game existed, and none after. On
  Linux, six games and the same single search. If the table cannot be found or
  does not hold what is wanted, the old search still runs, so nothing depends
  on it working.

- **The splitter now idles at one tick a second while no game is running**,
  instead of polling 120 times a second for as long as LiveSplit is open. It
  goes back to full rate the moment it attaches, so nothing about split timing
  changes; the only cost is up to a second more before it notices the game has
  started.

### Fixed

Both of these are in the scanning library the splitter is built on, fixed in
the copy it vendors. Neither has been seen to bite a real run here, and neither
would have announced itself if it had.

- **A scan could crash the splitter outright, depending on where the game's
  code happened to land.** Scans work a page at a time and carry the tail of
  one page onto the front of the next, and the offset that tail came from was
  wrong for a memory range that does not begin or end on a page boundary. One
  case asks for more bytes than the page held and panics; the other quietly
  reads bytes it never fetched, so a search can miss what it is looking for.
  Which you get depends on the game build's layout — roughly one build in
  sixteen for the panicking half — not on anything a runner does.

- **Every scan read and wrote memory that was no longer in use.** The iterator
  a scan returns held a buffer belonging to a function that had already
  returned. Attaching to the game runs one of these, so it was on the path
  every session took. It behaved for as long as that memory stayed unclaimed,
  which is not a guarantee of anything.

### Verified

Game build 1.1.2.4 (Unity 6000.5), both platforms, in a single session each:

- **Windows**, LiveSplit 1.8.37 — four scene loads, two new games plus an
  end-of-run save with 16171 entities, then a fourth new game, with a trip
  through the main menu between each.
- **Linux** under Proton — six games in one process, including the same
  end-of-run save.

One memory search in each session, before any game existed, and none after.

## 0.2 — 2026-09-02

The first build verified on both platforms. **If you have v0, replace it**: the
bugs below are silent, so a v0 module gives no sign that any of them is
happening.

### Fixed

- **Building splits could stop firing, and take the wonder-unlock split with
  them.** Mono fills a class's field table in lazily, and for a generic like
  `List<EntityComponent>` or `HashSet<string>` it may never do so, leaving the
  splitter unable to read a collection it was looking straight at. It now falls
  back to the known layout when the object agrees with it. The failure was
  silent and retried forever, and it can happen on any platform.
- **The timer could fail to start on a second game in the same session.**
  Binding the run start is a retry loop that runs during the load, and a full
  heap scan cost 29s on Windows — one attempt per load instead of twenty. Scans
  now read only the memory ranges that can hold a managed object, which is 21%
  of the address space on Windows and about 36% under Proton.
- **The game could stop responding while the splitter searched for services.**
  Services are now looked up through the game's DI container instead of a
  separate scan for each.
- **Splits could bind to the wrong game's objects.** The lifecycle now follows
  scene loads rather than object lifetime, so leaving a game and starting
  another rebinds cleanly.

### Added

- The splitter says when something has gone wrong, instead of failing quietly.
- Example splits and a layout ship with the release, and the README walks
  through setting them up with screenshots.
- Diagnostic logging (`[scan]`, `[collections]`) for when a run misbehaves. It
  goes nowhere unless a trace listener is configured, so it costs nothing in
  normal use — see the README if you are asked for a log.

### Verified

Windows natively and Linux under Proton, against Timberborn buildid 23107127
(Unity 6000.3.6f1), LiveSplit 1.8.37: a cold two-game session, attaching to a
game already in progress, loading a save, and both Folktails and Iron Teeth.

### Known gaps

- Example splits are Folktails only. Iron Teeth is supported by the splitter —
  it is the splits file that does not exist yet.

## 0 — 2026-08-31

First testable build.
