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

Two accessors the splitter needs, and one bug fix. There were two fixes; the
scanner's dangling buffer is upstream now, and the fork was rebased onto a
`master` containing it, so only the page-boundary fix is still carried.

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

## The signature scanner's dangling buffer

`Signature::scan_iter` zero-initialised a `Buffer<N>` as a local, took a
`&mut [u8]` over it through `slice::from_raw_parts_mut`, and moved *that slice*
into the `iter::from_fn` closure it returned. The buffer itself stayed a local
of `scan_iter`, so every poll of the returned iterator read and wrote a stack
frame that had already been given back — 4 KiB of it, in a scan that runs on
every `Module::attach`.

It is the sort of unsoundness that behaves for years: the stale address is
usually still slack stack. It stops behaving when the caller is deeper than
whatever ran before, which is how it turned up here — replaying a recording
reads through a chain of twenty delta captures, and the scan's 256-byte write
landed on one of those frames' return addresses.

The fix is to move the buffer into the closure and take the slice inside each
call, where the storage is live for as long as the pointer is used. It is a
soundness fix rather than an API change, and nothing about it is
Timberborn-specific.

**This one is upstream, and no longer carried here.**
<https://github.com/LiveSplit/asr/pull/158>, merged 2026-09-05 as `12375fc` on
`master`; the fork was rebased onto that and its own copy dropped. What is
described above is now upstream's code, kept in this document because the
reasoning behind it is not obvious from the diff and the measurements below
were expensive to make.

### What it meant for the shipped `.wasm`

Almost certainly nothing, which is worth writing down so this is not
remembered as a released bug.

Checked rather than assumed, by disassembling the release wasm built from the
commit before the fix. `scan_iter` is inlined into every caller there, so its
4 KiB buffer is allocated in the frame that then drives the iterator, and there
is no popped frame for the pointer to dangle into. Each of
`scan_process_range`'s instantiations opens with a 4240-, 4224- or 4256-byte
`__stack_pointer` adjustment, which is that buffer. The splitter never calls
`scan_iter` itself.

**What decides it is `opt-level`, not LTO** — measured against unmodified
upstream, sweeping both. `opt-level = 0` reproduces with LTO either way; 1, 2
and 3 do not, and
`-C llvm-args=--inline-threshold=0` does not bring it back at 3. So the crash
needed a build where `scan_iter` is a real frame that gets popped, which is the
unoptimised one the tests run — and every debug build is exposed, not just this
project's.

So this is a latent bug, not a live one — but latent by codegen accident rather
than by anything guaranteeing it, and wasm has no guard page: the day inlining
went the other way it would corrupt linear memory silently instead of
segfaulting. That is the argument for fixing it rather than noting it.

## The page tail carried across a boundary

Found by reading the rest of `signature.rs` after the one above, and verified by
running it. Scans go a page at a time, with the last N - 1 bytes of the previous
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

**This one was kept off the pull request branch**, being a different bug in
code the dangling-buffer fix does not touch, and pairing them would have held up
whichever review was slower. #158's body offers it as a follow-up. Nothing has
been sent yet -- and when it is, it goes **without** the two regression tests,
for the reason under [Upstreaming](#upstreaming).

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
happened to land on it first; it was `class-vtable` until the rebase.
`mono-class-vtable` carries the accessors shaped for upstream review --
retitled to the house style, with the dedup and the doc comments a reviewer
asked for in advance.

**The gitlink tracks `timberborn`, never `mono-class-vtable`.** A PR branch is
rewritten as review proceeds -- #158's was, to drop its test commit -- and a
gitlink pointing at a commit that a later force-push orphans cannot be fetched
at all: every fresh clone breaks. That is the one rule here worth more than the
convenience of a single branch. Keep the
two at the same commit while they agree, and let them diverge if review asks
for something the splitter does not need.

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

Two pull requests, deliberately separate.

The scanner's dangling buffer was
<https://github.com/LiveSplit/asr/pull/158>, from
`signature-scan-dangling-buffer`, based on `upstream/master` -- upstream's
default branch is `master`, and its `signature.rs` was byte-identical to the
pre-fix state, so the bug was live there. **Merged 2026-09-05** as `12375fc`.

The accessors are <https://github.com/LiveSplit/asr/pull/157>, from the
`mono-class-vtable` branch on the fork. Still open.

They were kept apart because the scanner fix is a soundness bug in code the
accessors do not touch, and reviewing it alongside an API proposal would have
held up whichever of the two was slower. That is borne out: one is merged and
the other is still in review.

### Upstream does not want tests right now

#158 went in as the fix commit alone. Its second commit was a regression test,
and the maintainer asked for it to be dropped -- not on its merits, but because
several open pull requests are adding tests at once and he wants to settle a
testing strategy for the crate before any of them land.

Treat that as standing until upstream says otherwise: **send fixes without
their tests**, and offer the tests separately in the body. That applies to the
page-boundary follow-up, whose two regression tests stay on `timberborn` and
do not go with it.

The dropped test was `tests/signature_scan_buffer.rs`. It is not lost -- commit
`4a2fdd5`, still reachable from #158's own commit list on GitHub -- if the
strategy question is ever settled.

Removing it was a force-push over the published branch rather than a revert
commit, which was safe here for reasons worth checking again next time: the test
was the tip commit, it touched only its own new file, and the PR had no
submitted reviews and no inline comments anchored to it, so nothing was
orphaned. `--force-with-lease` pinned the expected remote tip.

### What upstream's CI actually runs

Upstream's `master` is green under its own CI commands -- `cargo test
--all-features` (22 doctests), `cargo clippy --all-features` without
`-D warnings`, and a `cargo fmt` step that ends in `|| true`. A red check on a
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
`[patch]` stanza and the submodule then, not before: a merged #158 alone does
not get us there.
