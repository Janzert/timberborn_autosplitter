# Changelog

What changed between releases, from a runner's point of view. Dates are the
release date.

## Unreleased

### Added

- **A splits file for Iron Teeth**,
  [`examples/Timberborn-Wonder-IronTeeth.lss`](examples/Timberborn-Wonder-IronTeeth.lss),
  alongside the Folktails one. Same seven segments; the faction specific two
  carry the Numbercruncher and the Earth Repopulator, with that faction's
  icons. Both are attached to the release, so pick the one for the faction you
  run.

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
