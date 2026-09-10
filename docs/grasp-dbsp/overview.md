# grasp-dbsp — Overview

grasp-dbsp is a small purpose-built language for declarative dataflow programs.
A program in it is instantiated as a DBSP circuit using the
[`dbsp`](https://github.com/feldera/feldera/tree/main/crates/dbsp) crate and
executed incrementally: input changes stream through the circuit and output
changes are emitted as they are produced.

## What it is

- Executed by one Rust crate, `grasp-dbsp-runner`, linking against `dbsp` and
  `feldera-macros`, for the `IsNone` derive `dbsp`'s `DBData` bound requires.
  It shares a workspace with `grasp-compiler`, which compiles
  [grasp](../grasp/overview.md) down to this language; the two meet at
  [`language.md`](language.md) and nowhere else.
- A **runtime interpreter**: the program is parsed at startup and the circuit
  is assembled at startup, through `dbsp`'s ordinary operator API instantiated
  at a single universal value type. There is **no code generation and no Rust
  toolchain at runtime** — assembling the circuit is still a build step, but it
  needs no compiler.
- A **thin layer over `dbsp`**. DBSP defines the computational model (Z-sets,
  incremental operators, weights, epochs, state); grasp-dbsp adds the surface
  syntax, the value model, and the mapping from one onto the other. It does
  not reimplement or re-describe DBSP's model.

## Design principles

The language is a **compilation target**: it is emitted by a compiler frontend
or by an agent, not written by hand. That audience decides most of the
trade-offs below, so it is worth stating what it wants.

- **Explicit is free.** An emitter always knows the type it intends, so it can
  always write it down. Inference is never load-bearing: wherever it falls
  short, saying the type resolves it — a `cast`, or the node's own `::`
  typespec — and inference is not made cleverer to avoid one. A typespec
  supplies a type only where inference has none, as for an empty container
  literal; it never overrides one.
- **Regeneration is the norm.** An agent re-emits whole programs — different
  whitespace, ordering and intermediate names, same computation. Nothing may be
  keyed to source position; identity is content. See
  [`mapping.md`](mapping.md).
- **Diagnostics are the interface.** Programs get written by emit → check →
  repair, so no reachable path may produce an internal error, and every
  rejection says what to write instead.
- **The docs are the spec.** They are what an emitting agent is given, so a doc
  that disagrees with the implementation produces wrong programs
  deterministically. Every documented rule is pinned by a fixture in
  `tests/cases/`.
- **Exactly one way to write each thing**, and no implicit conversion. A
  redundancy is a decision an emitter must make with no information.
- **Every construct is a value in every position.** A construct legal only in
  certain syntactic slots cannot be composed.
- **A declared type is a promise the runtime cannot break.**

Two of these were bought rather than assumed. `sql.SqlString` was withdrawn and
implicit numeric promotion deleted, both to satisfy the third and fifth.

## Scope

- The source language described in [`language.md`](language.md).
- `circuit` definitions, instantiated either by expansion at the call site —
  including a circuit instantiated inside another — or by `fixpoint`.
- Named `function` definitions, which are **templates**: parameters carry no
  types, the body is checked per call site, and the whole thing is inlined. One
  arithmetic helper therefore serves every numeric type it works at.
- Content-addressed nodes: identical operator, inputs and parameters means one
  node, however many times it is written — and that same address is the node's
  `persistent_id` and the name it can be observed by.
- The operator set listed there: inputs and constants; the mapping family (`map`, `filter`,
  `flat_map`, `map_index`, `flat_map_index`); the join family (`join`,
  `join_index`, `antijoin`); `distinct`; `aggregate` over
  `min`/`max`/`sum`/`avg`/`count`; `weighted_count`; the algebraic operators
  (`neg`, `plus`, `minus`, `sum`); and `integrate`, `differentiate`, `delay`.
  Every operator in the mapping family takes either stream shape, so `map`
  flattens an indexed stream and an outer join is a pattern rather than an
  operator.
- Expressions in function bodies: literals, parameters, record field access,
  `record(...)` and `[…]` construction, arithmetic, comparison and logic,
  `if(cond, a, b)`, `cast(x, T)`, `map_array` and `filter_array` over an array,
  and a small builtin library. Those two are the only constructs that bind a
  name inside an expression, and `map_array` is what lets a `flat_map`'s
  function build the rows it fans out to rather than only choosing an array
  already in the row.
- Documents, as the `json` type. There is deliberately no pattern language:
  `cast` converts one out and `get` reaches inside one, so the type costs no new
  grammar at all.
- The type system described in [`language.md`](language.md) (batch types and
  value types). The value vocabulary is `bool`, `i64`, `f64`, `string`,
  `date`, `time`, `timestamp`, `interval`, `optional(T)`, `record(...)`,
  `array(T)`, `dict(K,V)` and `json`. A dict's entries are sorted and
  deduplicated by construction, and its keys are the scalars — the ones with a
  JSON object-key spelling.

  There are **no timezones and no months**. A timestamp is always UTC and an
  interval is microseconds, so a day is exactly 86400 seconds and each of the
  four is one number — which is what makes them order, key a dict and compare
  structurally like everything else here.
- The value model described in [`mapping.md`](mapping.md).
- Feldera-native JSON input and output, emitting deltas — `weighted` by default
  here because it represents a Z-set delta exactly, `insert_delete` for
  compatibility with Feldera's own default.

One level of `fixpoint` is supported, and that is deliberate rather than
pending: a `fixpoint` inside a `fixpoint` is rejected by the type checker, with
a diagnostic. `dbsp` allows circuits at arbitrary depth, but each level is a
distinct Rust circuit type needing its own instantiation of the lowering, and
one level covers the recursive queries this language is for.

Outputs are **not** part of the source language. A program declares streams; the
set of nodes to observe is supplied when the runner starts, so anything can
become an output — by declared name, or by content id for a node that has none.

## Future work

- **Checkpoint and restore.** The ids are in place — every operator carries its
  node's content id as a `persistent_id` — but nothing takes or restores a
  checkpoint yet. See [`mapping.md`](mapping.md).
- **Multi-worker execution.** `Runtime::init_circuit` is called with one worker.
  Sharding is by key hash, so this depends on the hashing invariants in
  [`mapping.md`](mapping.md).
- **`left_join` is *not* planned.** `dbsp` has one, but its right-hand input is
  `OrdIndexedZSet<K, Option<V2>>` — a second Rust batch type, in a design whose
  leverage is that there is exactly one. It is not needed: a left join is
  `join ∪ (antijoin × null)`, three operators that already exist, now that the
  mapping family accepts an indexed stream. See [`language.md`](language.md).
- **`consolidate`.** Applies to trace-carrying streams. Recursion has landed
  and this still does not apply, because no operator in the language produces a
  stream carrying a trace rather than a batch. See [`mapping.md`](mapping.md).
- **Windowing and ranking operators** — `window`, `waterline`, `topk`, `rank`,
  `row_number`, `lag`, `asof_join`, `star_join`. All exist in `dbsp`; none are
  implemented.
- **The rest of the value vocabulary.** The other integer widths and `f32`; and
  the Feldera `sql.*` types still absent — `ByteArray`, `SqlDecimal`, `Uuid`.
  Shallow but wide: each needs a `DynValue` variant, a `TypeDesc` variant, a
  parser name, JSON coding and an ordering that upholds the invariants in
  [`mapping.md`](mapping.md), and the decimal type additionally needs
  `SqlSerdeConfig` for its JSON format.

  The temporal types have landed — `date`, `time`, `timestamp` and `interval`,
  on `Date`, `Time`, `Timestamp` and `ShortInterval`. Feldera's `TimestampTz`
  and `LongInterval` are **not** wanted: the first stores exactly what
  `Timestamp` does and differs only in printing, and the second is months, which
  [`language.md`](language.md) declines to measure.

  Adding a variant shifts `DynValue`'s archived discriminant, which is a
  persisted storage format. Nothing is persisted yet, so the variant order is
  still free to settle — which stops being true after the first stored batch.
  That freedom is what let `SqlString` be removed rather than deprecated.

- **Document odds and ends.** A `shape(doc)` builtin — a program that must
  branch on what a document holds attempts casts in order today, which works and
  reads poorly. A serialisation builtin: `cast(d, optional(string))` *extracts* a
  string document, so writing one out as JSON text needs its own name
  (`FlatVariant::to_json_string` exists). And `dynamic` — `json` plus the
  temporal and decimal tags `FlatVariant` already carries — which is a one-line
  addition once there is something in the language that can produce one.

- **Body bindings in a `function`** — `x := …` statements alongside `return`,
  with a flat slot table so an argument used twice is evaluated once. Inlining
  is substitution today, so a diamond in the call graph duplicates work.

- **The `raw` JSON format** (a bare object meaning insert) and the `update`
  operation for keyed partial updates, which needs primary keys the language
  does not have. Both are Feldera-native; neither is implemented.

- **A richer builtin set** — a fuller arithmetic/string/temporal library.
- **The CLI / HTTP surface** — the current `validate` / `run` / `serve`
  commands are placeholders and subject to change.

## Relationship to other projects

- **grasp-dbsp (Erlang)** — an Erlang DBSP runtime with its own language, and
  the source of this one's name. **Not the language specified here**: the
  language in these documents borrows only the *grammar* (declaration forms,
  `name := op(...)`, `name :: type`, `function((params) -> ...)`, and
  `record(...)` where the Erlang project writes `struct(...)`). Operators,
  types, and semantics come from `dbsp`. Where the rest of these documents say
  grasp-dbsp unqualified, they mean the language specified here; the Erlang
  project is always named as such.
- **dbsp** — the computational engine. `grasp-dbsp-runner` relies on `dbsp` for
  all execution and state management. The design documents describe only *how
  the language is mapped onto* `dbsp`.
- **Feldera's SQL compiler** — solves the same lowering problem by generating
  Rust. `grasp-dbsp-runner` does the same lowering at runtime instead, so the
  two agree on operator vocabulary and on the shape of aggregation, but share no
  code.

## Architecture

`grasp-dbsp-runner` has these logical layers:

| module | responsibility |
|---|---|
| `lang` | lexing and parsing the source language |
| `typecheck` | type inference/checking; produces the schema (`TypeDesc`) and each node's content id |
| `value` | the runtime value model (`DynValue`, `TypeDesc`) |
| `diag` | one `Diagnostic` type for every pass, with severity, pass and span |
| `expr` | type-checked expressions and their tree-walking evaluator |
| `json` | the Feldera JSON codec (`weighted`, `insert_delete`) |
| `lower` | mapping the program onto `dbsp` operators; also circuit construction, input/output handles and transactions (`Runner`) |
| `serve` / CLI | *not implemented* — the crate's `main.rs` is a placeholder. YAML fixtures are the surface for now |

The layered mapping is: source text → AST → typed AST (`TypeDesc`) → `dbsp`
circuit, with values flowing through a single runtime value type.

