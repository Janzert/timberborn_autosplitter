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

Two accessors the splitter needs, and one bug fix.

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
soundness fix rather than an API change, and worth upstreaming on its own
account: nothing about it is Timberborn-specific.

## Where the submodule points

`.gitmodules` points at the fork, <https://github.com/Janzert/asr>, and inside
the submodule:

| remote | |
|---|---|
| `origin` | the fork — fetch over https, push over ssh |
| `upstream` | <https://github.com/LiveSplit/asr> — pull from it to stay current |

The two accessors and the scanner fix live on the `class-vtable` branch, which
is what the parent repo's gitlink points at. `mono-class-vtable` carries the
accessors shaped for upstream review -- retitled to the house style, with the
dedup and the doc comments a reviewer asked for in advance.

**The gitlink tracks `class-vtable`, never `mono-class-vtable`.** A PR branch is
rewritten as review proceeds, and a gitlink pointing at a commit that a later
force-push orphans cannot be fetched at all: every fresh clone breaks. That is
the one rule here worth more than the convenience of a single branch. Keep the
two at the same commit while they agree, and let them diverge if review asks
for something the splitter does not need.

Note that `git submodule sync` rewrites `origin`'s fetch URL from `.gitmodules`
but leaves the push URL alone, which is why the two differ. If the push URL is
ever lost:

```bash
git -C vendor/asr remote set-url --push origin git@github.com:Janzert/asr.git
```

## Making a change

```bash
git -C vendor/asr checkout class-vtable      # or a new branch off it
# edit, then commit inside the submodule
git -C vendor/asr commit -am "expose whatever it is"
git -C vendor/asr push origin class-vtable
```

Then record the new revision here — the gitlink is the pin, and it is easy to
forget:

```bash
git add vendor/asr && git commit -m "chore: bump vendored asr"
```

## Upstreaming

Open as <https://github.com/LiveSplit/asr/pull/157>, from the
`mono-class-vtable` branch on the fork.

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

The scanner fix belongs in its own pull request rather than in that one: it is
a soundness bug in code the accessors do not touch, and reviewing it alongside
an API proposal would hold up whichever of the two is slower.

Upstream may prefer a higher-level API (e.g. `Image::find_instances(&class)`)
over exposing the raw address. That is a nicer contribution but more surface to
get reviewed, which is why it is worth doing after the splitter works.

When it lands, delete the `[patch]` stanza and the submodule.
