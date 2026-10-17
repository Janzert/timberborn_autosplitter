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

Two accessors the splitter needs, rebased onto upstream `master` as of
2026-10-15. Nothing else: the scanner fix it used to carry is upstream now
(below).

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

## The page tail carried across a boundary, now upstream

The fork used to carry a fix to `Signature::scan_process_range`: the tail of
one page carried in front of the next was taken from where a *full* 4 KiB page
would have ended. A range ending part way into a page panicked in
`copy_from_slice` -- reachable from `mono::Module::attach`, whose
`(mono_assembly_foreach, 0x100)` scans straddle a page boundary whenever that
symbol lands within 256 bytes of one, so about one Unity mono build in sixteen
would panic on attach every time. A range starting part way into a page
silently missed a signature across its first boundary.

Upstream fixed the same bug independently in
<https://github.com/LiveSplit/asr/pull/178> (2026-10-03), after it was found
again from the IL2CPP side, whose x86 attach scans unaligned windows on every
Unity version. That superseded our own
<https://github.com/LiveSplit/asr/pull/159>. The scanner was then rewritten in
<https://github.com/LiveSplit/asr/pull/181> to read 64 KiB chunks.

Before the fork's fix was dropped, its two regression tests were run against
upstream's rewritten scanner, at both a 4 KiB and a 64 KiB boundary, and all
passed. They were not kept: upstream's own suite now includes signature tests
that cross chunk boundaries, including a brute-force comparison against a
naive scan.

## Where the submodule points

`.gitmodules` points at the fork, <https://github.com/Janzert/asr>, and inside
the submodule:

| remote | |
|---|---|
| `origin` | the fork — fetch over https, push over ssh |
| `upstream` | <https://github.com/LiveSplit/asr> — pull from it to stay current |

The accessors live on the **`timberborn`** branch,
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

That rule is why the rename left something behind, and the rebase after it. Rebasing rewrote every
commit `class-vtable` had, and deleting the branch would have orphaned the
gitlinks in this repo's own history -- every parent commit up to the rename
names a submodule commit that no branch reaches any more. The old tip is
therefore kept as the tag **`archive/class-vtable`** (`a231d3f`), pushed to the
fork. Do not delete it: checking out an older parent commit needs it. Do the
same for any future rewrite of this branch -- as was done when it was rebased
onto upstream on 2026-10-15, keeping the previous tip as
**`archive/timberborn-2026-10-15`** (`589341a`).

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

The accessors are <https://github.com/LiveSplit/asr/pull/157>, from the
`mono-class-vtable` branch on the fork.

### Upstream tests

In September 2026 the maintainer asked for a fix of ours to arrive without its
regression test, because a testing strategy for the crate was not yet settled;
the test was dropped and the fix merged. Since then upstream has grown a test
suite of its own -- a mock host, and `#[cfg(test)]` modules beside the code
they cover -- so a test that fits that pattern is reasonable to offer with a
fix. Ask first if in doubt.

### What upstream's CI actually runs

`cargo build` for wasm targets with no default features, default features and
all features; `cargo test --all-features`; `cargo clippy --all-features`
without `-D warnings`; and a `cargo fmt` step that ends in `|| true`. Upstream
`master` has 31 clippy warnings of its own, so compare a branch's count
against master's rather than expecting none.

### Notes on #157

It is framed generally rather than as a Timberborn special case: Unity games
using constructor-injection DI (Bindito, Zenject, VContainer) frequently have
no static roots at all, which makes `UnityPointer` unusable and scanning for a
singleton's instance the only option.

`get_vtable` is the first half of what upstream's static-table lookup already
did, so that lookup now calls it (`MonoRuntime::vtable_address` in
`managed/runtime.rs`) -- the diff reads as an extraction rather than an
addition. `of_object` is a thin wrapper over upstream's own internal
`Walk::object_class`, which already does the object -> vtable -> class reads;
what the pull request adds is making that public, and documenting the
lazily-filled field table, which is a real trap in the API being proposed and
cost a session to find.

The pull request's branch predates upstream's restructuring of the Mono code
into `managed/` and no longer applies; the rebased `timberborn` branch is what
it would need to become.

Upstream uses neither conventional commits nor an `Area:` prefix -- 93% of the
last 150 subjects are a bare imperative sentence -- so the commits are titled
to match rather than to match this repo.

Upstream may prefer a higher-level API (e.g. `Image::find_instances(&class)`)
over exposing the raw address. That is a nicer contribution but more surface to
get reviewed, which is why it is worth doing after the splitter works.

The submodule goes away when the fork has nothing left that upstream lacks --
which now means #157, or an equivalent, landing. Delete the `[patch]` stanza
and the submodule then, not before.
