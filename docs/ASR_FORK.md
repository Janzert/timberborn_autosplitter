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

Two accessors the splitter needs, and two bug fixes.

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

Fixed on `class-vtable` by tracking how much the last page actually held. Two
regression tests come with it, in `tests/`.

**This one is not on the pull request branch.** It is a different bug in code
the dangling-buffer fix does not touch, and pairing them would hold up whichever
review is slower. The pull request body offers it as a follow-up.

## Where the submodule points

`.gitmodules` points at the fork, <https://github.com/Janzert/asr>, and inside
the submodule:

| remote | |
|---|---|
| `origin` | the fork — fetch over https, push over ssh |
| `upstream` | <https://github.com/LiveSplit/asr> — pull from it to stay current |

The two accessors and both scanner fixes live on the `class-vtable` branch,
which is what the parent repo's gitlink points at. `mono-class-vtable` carries the
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
an API proposal would hold up whichever of the two is slower. It is prepared and
pushed on the fork as `signature-scan-dangling-buffer`, based on
`upstream/master` -- upstream's default branch is `master`, and its
`signature.rs` is byte-identical to the pre-fix state, so the bug is live there.
The PR has not been opened.

Upstream's `master` is green under its own CI commands -- `cargo test
--all-features` (22 doctests), `cargo clippy --all-features` without
`-D warnings`, and a `cargo fmt` step that ends in `|| true`. A red check on
the pull request would therefore mean something. (Plain `cargo test` and
`cargo clippy -- -D warnings` do fail on pristine master, but neither is what
CI runs.)

That CI has a **Test (Host)** job, which today runs nothing but doctests --
there is no `#[test]` anywhere in the crate. A `tests/` file added by the pull
request is picked up by it with no CI change, which is where the branch's second
commit puts the regression test: `tests/signature_scan_buffer.rs`, which fails
on unmodified upstream and passes with the fix.

Upstream may prefer a higher-level API (e.g. `Image::find_instances(&class)`)
over exposing the raw address. That is a nicer contribution but more surface to
get reviewed, which is why it is worth doing after the splitter works.

When it lands, delete the `[patch]` stanza and the submodule.
