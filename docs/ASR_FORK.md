# Working on the vendored asr

`vendor/asr` is a git submodule. The crate depends on upstream asr by its git
URL, and `Cargo.toml` redirects that to the submodule:

```toml
[patch."https://github.com/LiveSplit/asr"]
asr = { path = "vendor/asr" }
```

The exact revision is pinned by the submodule gitlink, so a clone with
`--recurse-submodules` always builds against the same asr.

## Why we need a change at all

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

## Pointing the submodule at your fork

The submodule starts out pointing at upstream. Once you have a fork:

```bash
git submodule set-url vendor/asr https://github.com/YOURNAME/asr.git
git -C vendor/asr remote add upstream https://github.com/LiveSplit/asr.git
git submodule sync
```

Then `origin` is your fork (push branches and open PRs from there) and
`upstream` is LiveSplit's (pull from it to stay current).

## Making a change

```bash
git -C vendor/asr checkout -b class-address
# edit, then commit inside the submodule
git -C vendor/asr commit -am "expose Class address"
git -C vendor/asr push -u origin class-address
```

Then record the new revision in the parent repo — this is the pin, and it is
easy to forget:

```bash
git add vendor/asr && git commit -m "chore: bump vendored asr"
```

## Upstreaming

Ship on the fork first; open the PR once there is a working splitter to point
at as the motivating use case. Frame it generally rather than as a Timberborn
special case: Unity games using constructor-injection DI (Bindito, Zenject,
VContainer) frequently have no static roots at all, which makes `UnityPointer`
unusable and scanning for a singleton's instance the only option.

Upstream may prefer a higher-level API (e.g. `Image::find_instances(&class)`)
over exposing the raw address. That is a nicer contribution but more surface to
get reviewed, which is why it is worth doing after the splitter works.

When it lands, delete the `[patch]` stanza and the submodule.
