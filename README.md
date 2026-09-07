# dbsp-runner

A runtime for declarative dataflow programs, executed incrementally as DBSP
circuits via the [Feldera `dbsp` crate](https://github.com/feldera/feldera).

A program is a flat list of stream declarations; the runner parses it, builds a
`dbsp` circuit at startup with no code generation, and streams input changes
through it, emitting output changes as they are produced.

Design documents live in [`docs/design/`](docs/design/):

- [`overview.md`](docs/design/overview.md) — goals, design principles, scope, future work, architecture
- [`language.md`](docs/design/language.md) — the source language
- [`mapping.md`](docs/design/mapping.md) — how the language maps onto `dbsp`

## Building

The Feldera crates are path dependencies resolved through `vendor/feldera`, a
gitignored symlink to a local checkout of
[feldera/feldera](https://github.com/feldera/feldera). Create it once:

```bash
git clone https://github.com/feldera/feldera ~/repos/feldera
```

```bash
mkdir -p vendor && ln -sfn ~/repos/feldera vendor/feldera
```

Then `cargo build` works normally, including offline.

The toolchain is pinned by `rust-toolchain.toml`. This is not a formality:
`dbsp` declares `rust-version = "1.93.1"`, and rustc 1.98.1 hits an internal
compiler error generating code for it. The pin records the working version so
that is not rediscovered.

The symlink keeps the checkout's location out of the tracked manifest, so the
build is not tied to one machine while still pointing at a working tree you can
edit alongside this crate. Git dependencies were considered and rejected: cargo
cannot resolve a git dependency offline even when `[patch]` redirects it to a
local path, which would break the ordinary edit-and-rebuild loop.

`dbsp` is published to crates.io, so this can become a version requirement once
a release carries the APIs this crate needs — the design documents cite `dbsp`
at revision `4a6744aa` (workspace version 0.343.0). `feldera-sqllib` supplies the
`FlatVariant` behind the `json` type, and is where the `sql.*` value types will
come from when they land.
