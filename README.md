# grasp

Two languages for declarative dataflow, executed incrementally as DBSP circuits
via the [Feldera `dbsp` crate](https://github.com/feldera/feldera).

**grasp-dbsp** is the target language: a flat list of stream declarations. The
runner parses it, builds a `dbsp` circuit at startup with no code generation,
and streams input changes through it, emitting output changes as they are
produced.

**grasp** is a Datalog dialect that compiles to grasp-dbsp: rules, stratified
negation and aggregation, and recursion found by the compiler rather than
declared. It has no runtime of its own.

## Crates

| crate | |
|---|---|
| [`grasp-dbsp-runner`](crates/grasp-dbsp-runner) | parses grasp-dbsp and executes it as a `dbsp` circuit |
| [`grasp-compiler`](crates/grasp-compiler) | compiles grasp down to grasp-dbsp — parse, desugar, infer, plan, emit — and its fixtures run what it emits |

grasp-dbsp is the contract between them, which is why the design documents live
at the workspace root rather than inside either crate. They are in
[`docs/`](docs/), a directory per language — start at
[`docs/README.md`](docs/README.md):

- [`docs/grasp-dbsp/`](docs/grasp-dbsp/) — the target language: its goals and
  principles, the spec as implemented, and how it maps onto `dbsp`
- [`docs/grasp/`](docs/grasp/) — the Datalog dialect: its principles, the
  language, how it is compiled, and how it is emitted as grasp-dbsp

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
