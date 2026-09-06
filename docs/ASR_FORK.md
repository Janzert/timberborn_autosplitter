# Working on the vendored asr

`vendor/asr` is a git submodule. The crate depends on upstream asr by its git
URL, and `Cargo.toml` redirects that to the submodule:

```toml
[patch."https://github.com/LiveSplit/asr"]
asr = { path = "vendor/asr" }
```

The exact revision is pinned by the submodule gitlink, so a clone with
`--recurse-submodules` always builds against the same asr.

## What the fork carries

Two accessors the splitter needs, and one bug fix in the signature scanner.

## The accessors, and why they are needed

`mono::Class` stores its address in a `pub(super)` field, and `get_name` is
`pub(super)`. Locating a service by scanning for instances of its class
(see `DESIGN.md`) needs the class address as an identity handle, and there is
no public way to obtain it.

The change is a small accessor. It has **no security implications**: the
sandbox boundary is enforced by the WASM host, not by asr's API surface. asr is
guest-side code compiled into a module the runtime already treats as untrusted,
and a splitter can already `read` any address, enumerate every memory range via
`memory_ranges()`, and reimplement all of asr's Mono parsing itself. Exposing a
`MonoClass*` as an `Address` grants no new capability — asr already returns raw
addresses from `get_static_table()`, `UnityPointer::deref_offsets()` and
`MemoryRange::address()`.

## The page tail carried across a boundary

Found by reading `signature.rs` closely, and verified by running it. Scans go a
page at a time, with the last N - 1 bytes of the previous
page placed in front of the next so a signature lying across the boundary is
still found. The offset that tail is taken from assumes the previous page was a
full 4 KiB -- true of every page but two, the first page of a range that does
not start on a boundary and the last page of one that does not end on one.

- A range ending part way into a page asks for more bytes than the page held and
  **panics** in `copy_from_slice`.
- A range starting part way into a page carries bytes that were never read, and
  **silently misses** a signature across its first boundary.

### Which of those anything actually reaches

Checked, because the two are not equally real.

Every scan asr itself performs is over either a module range or a memory range,
except for a handful of the form `(some symbol, 0x100)` -- five of them in
`mono::Module::attach`, two in il2cpp, and a few in the PS1 and PS2 retroarch
backends.

Module and memory ranges are page-aligned at both ends, so they reach neither
bug. That is not an assumption: across a captured Timberborn process, all 272
modules had a page-aligned base *and* a page-aligned size. It is also why asr
has not been panicking everywhere.

That leaves the `(symbol, 0x100)` scans, which are aligned at neither end.
**Those reach the panic**, whenever the symbol lands within 256 bytes of a page
boundary.

It is worth being precise about what kind of risk that is, because "luck" is
misleading. Module bases are page-aligned, so the symbol's offset *within* a
page is `RVA & 0xfff` -- a property of the shipped `mono-2.0-bdwgc.dll` and
nothing else. Measured here:

| build | scan starts at | page offset |
|---|---|---|
| 1.0.13.1, Unity 6000.3 | `0x6ffffaa15390` | `0x390` |
| 1.1.2.4, Unity 6000.5 | `0x6ffff93e5790` | `0x790` |

Two separate launches of 1.1.2.4 -- different sessions, different pids -- gave
not just the same page offset but the same absolute address, so nothing is being
rebased between runs either.

So the dice are rolled once per Unity release, not once per launch. Functions
are 16-byte aligned, and 15 of the 256 aligned slots in a page put the scan
across the boundary (`0xf00` exactly is safe -- the range ends on the boundary
rather than crossing it), so about 6% of mono builds. A bad one would not be an
intermittent fault: it would panic on attach for every asr splitter against
every game built with that Unity version, every time. This project is two
draws clear of it, and gets a fresh draw whenever Unity ships a new mono.

**Nothing reaches the silent miss.** It needs a range that begins part way into
a page and then runs a full page further, and asr never builds one -- its
unaligned scans are 0x100 or 0x200 bytes long, which hits the panic first if it
straddles anything. It is a defect in a public API rather than a live bug, and
it is fixed here because it is the same three lines.

Fixed on `timberborn` by tracking how much the last page actually held. Two
regression tests come with it, `tests/signature_page_boundary.rs`. This is the
only thing the fork still carries beyond the accessors.

It is upstream as <https://github.com/LiveSplit/asr/pull/159>, opened
2026-09-05 and still in review. **The two regression tests did not go with
it**, for the reason under [Upstreaming](#upstreaming); they stay here.

## Where the submodule points

`.gitmodules` points at the fork, <https://github.com/Janzert/asr>, and inside
the submodule:

| remote | |
|---|---|
| `origin` | the fork — fetch over https, push over ssh |
| `upstream` | <https://github.com/LiveSplit/asr> — pull from it to stay current |

The accessors and the page-boundary fix live on the **`timberborn`** branch,
which is what the parent repo's gitlink points at. It is named for what it is
for -- the branch this project builds against -- rather than for whatever
happened to land on it first; it was `class-vtable` until 2026-09-05.
`mono-class-vtable` carries the accessors shaped for upstream review --
retitled to the house style, with the dedup and the doc comments a reviewer
asked for in advance.

**The gitlink tracks `timberborn`, never `mono-class-vtable`.** A PR branch is
rewritten as review proceeds -- a branch of this fork's has been, to drop a
commit a reviewer did not want -- and a gitlink pointing at a commit that a
later force-push orphans cannot be fetched at all: every fresh clone breaks.
That is the one rule here worth more than the convenience of a single branch.
Keep the two at the same commit while they agree, and let them diverge if
review asks for something the splitter does not need.

That rule is why the rename left something behind. Rebasing rewrote every
commit `class-vtable` had, and deleting the branch would have orphaned the
gitlinks in this repo's own history -- every parent commit up to the rename
names a submodule commit that no branch reaches any more. The old tip is
therefore kept as the tag **`archive/class-vtable`** (`a231d3f`), pushed to the
fork. Do not delete it: checking out an older parent commit needs it. Do the
same for any future rewrite of this branch.

Note that `git submodule sync` rewrites `origin`'s fetch URL from `.gitmodules`
but leaves the push URL alone, which is why the two differ. If the push URL is
ever lost:

```bash
git -C vendor/asr remote set-url --push origin git@github.com:Janzert/asr.git
```

## Making a change

```bash
git -C vendor/asr checkout timberborn        # or a new branch off it
# edit, then commit inside the submodule
git -C vendor/asr commit -am "expose whatever it is"
git -C vendor/asr push origin timberborn
```

Then record the new revision here — the gitlink is the pin, and it is easy to
forget:

```bash
git add vendor/asr && git commit -m "chore: bump vendored asr"
```

## Upstreaming

Two pull requests, deliberately separate, both open.

The accessors are <https://github.com/LiveSplit/asr/pull/157>, from the
`mono-class-vtable` branch on the fork.

The page-boundary fix is <https://github.com/LiveSplit/asr/pull/159>, from
`signature-page-tail-boundary`, based on `upstream/master` -- upstream's
default branch is `master`, and its `signature.rs` is the pre-fix code, so the
bug is live there.

They are apart because one is a bug fix in code the accessors do not touch and
the other is an API proposal; reviewing them together would hold up whichever
is slower. Keeping fixes on their own is what got the last one merged quickly.

### Upstream does not want tests right now

Asked for directly by the maintainer in September 2026, on a fix of ours that
had arrived with a regression test: several open pull requests are adding tests
at once, and he wants a testing strategy settled for the crate before any of
them land. The test was dropped and the fix merged without it.

Treat that as standing until upstream says otherwise: **send fixes without
their tests**, and offer the tests separately in the body. #159 was sent that
way, and its two regression tests stay on `timberborn`.

### What upstream's CI actually runs

Upstream's `master` is green under its own CI commands --
`cargo test --all-features` (22 doctests), `cargo clippy --all-features`
without `-D warnings`, and a `cargo fmt` step that ends in `|| true`. A red
check on a
pull request therefore means something. (Plain `cargo test` and
`cargo clippy -- -D warnings` do fail on pristine master, but neither is what
CI runs.)

Its **Test (Host)** job runs nothing but doctests -- there is no `#[test]`
anywhere in the crate. A file added under `tests/` would be picked up by that
job with no CI change needed, which is worth knowing for whenever tests are
welcome again.

### Notes on #157

It is framed generally rather than as a Timberborn special case: Unity games
using constructor-injection DI (Bindito, Zenject, VContainer) frequently have
no static roots at all, which makes `UnityPointer` unusable and scanning for a
singleton's instance the only option.

Two things were done to it that the fork branch did not need. `get_vtable` is
the first half of upstream's own `get_static_table_pointer`, so that function
now calls it -- the diff reads as an extraction rather than an addition. And
`of_object` documents the lazily-filled field table, which is a real trap in
the API being proposed and cost a session to find.

Upstream uses neither conventional commits nor an `Area:` prefix -- 93% of the
last 150 subjects are a bare imperative sentence -- so the commits are titled
to match rather than to match this repo.

Upstream may prefer a higher-level API (e.g. `Image::find_instances(&class)`)
over exposing the raw address. That is a nicer contribution but more surface to
get reviewed, which is why it is worth doing after the splitter works.

The submodule goes away when the fork has nothing left that upstream lacks --
which now means #157 landing, and the page-boundary fix after it. Delete the
`[patch]` stanza and the submodule then, not before -- which means both open
pull requests landing, not just one.
