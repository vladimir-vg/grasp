# grasp — Overview

grasp is a dialect of Datalog. A program is a set of rules that derive relations
from other relations; the compiler turns them into a
[grasp-dbsp](../grasp-dbsp/language.md) program, which is what actually runs.

grasp has **no runtime of its own**. Everything it can express, it expresses by
emitting grasp-dbsp, and everything that executes is `grasp-dbsp-runner` over the
`dbsp` crate. That is the whole architecture: one compiler, no engine.

## What it is

- **A Datalog dialect**, with the usual shape — a rule has a head and a body,
  multiple rules for one relation are a union, and recursion is just a relation
  mentioning itself. There is no fixpoint keyword, no iteration construct, and
  no explicit ordering.
- **Compiled, not interpreted.** `grasp-compiler` reads a program, checks it, and
  emits grasp-dbsp. It does not execute anything, hold state, or exist at
  runtime.
- **Written by people.** This is the one place grasp and grasp-dbsp disagree by
  design, and most differences between them follow from it — see below.

## Design principles

grasp-dbsp is a compilation target: its
[principles](../grasp-dbsp/overview.md#design-principles) all follow from being
written by a machine that already knows what it means. grasp inverts that
premise, and several of the principles invert with it.

- **Inference is load-bearing.** A person writing a rule should not have to
  annotate what the compiler can see. Where grasp-dbsp says "explicit is free,
  because an emitter always knows the type it intends", grasp infers column types
  across rules and asks for an annotation only where inference genuinely cannot
  reach. This is a real obligation on the compiler, not a convenience.
- **A rule body is a set, not a sequence.** Body statements constrain; they do
  not sequence. The author writes what must hold, and the optimizer picks the
  evaluation order. Two rule bodies differing only in statement order are the
  same program and compile to the same circuit.
- **Recursion is not a construct.** A relation that mentions itself is
  recursive; a set of relations that mention each other is mutually recursive.
  The compiler finds the strongly connected components. Nothing in the surface
  language names a fixpoint.
- **Stratification is the discipline.** Negation and aggregation are both
  stratified: a negated atom or an aggregate may only reference relations from a
  strictly lower stratum. This is what makes them well-defined, and it is checked,
  not assumed.
- **Occurrences, not relations.** Each mention of a relation in a body is its own
  thing with its own variable bindings. A self-join is two occurrences of one
  relation and needs no special syntax. This starts as a compiler representation
  and surfaces as a language property.
- **Sugar belongs here, not below.** grasp-dbsp holds itself to *exactly one
  way to write each thing*, because a machine emitting it already knows what it
  means and a second spelling is a choice made with no information. People
  write grasp, so it may offer two where two read better — the dict literal's
  `{a: v}` beside `{k => v}` — provided the extra one desugars away before
  emission, leaving the target's rule intact.
- **One name per concept, shared with grasp-dbsp.** Where both languages have the
  same thing, they spell it the same way: `record`, `array`, `optional`, `json`,
  `NONE`, and the aggregator names. A concept that needs two spellings across the
  boundary is a translation step waiting to be got wrong.
- **Types are erased.** Checking happens in the compiler. The emitted grasp-dbsp
  carries no grasp type metadata, and nothing at runtime knows grasp exists.

## Scope

- Facts, rules, and unions of rules for one head relation.
- Recursion, including mutual recursion, found by SCC analysis rather than
  declared.
- Stratified negation (`not r(...)`) and stratified aggregation.
- Rule bodies of positive atoms, negated atoms, `:=` matches, filter
  expressions, and destructuring and unnest patterns.
- Expressions: arithmetic, comparison, logic, concatenation, function calls,
  array and dict literals, field access and subscript.
- The value types listed in [`types.md`](types.md) — `boolean`, `i64`,
  `f64`, `string`, `optional(T)`, `record(...)`, `array(T)`, `dict(K,V)` and
  `json` — and `relation(...)` over them.
- External relations, declared `r(cols:) <- input`.
- Running against a sequence of transactions: rows arrive weighted, a relation
  is the rows whose accumulated weight is positive, and what a transaction
  reports is what changed. See [`semantics.md`](semantics.md#time).
- Compilation through a join graph, a cost-based optimizer, a per-rule
  computation DAG and SCC stratification, described in
  [`compilation.md`](compilation.md), and emission described in
  [`mapping.md`](mapping.md).

These documents are meant to be **sufficient on their own**: the grammar is
complete, the type rules are stated as rules, and each pass says what it rejects
and why. Nothing here requires reading the Erlang implementation's documents,
which are referenced below only as history.

## Future work

This is the first cut, and it is narrower than the Erlang implementation the
design comes from. What is missing is missing on purpose.

- **Element access.** `arr[0]` and slicing have no grasp-dbsp equivalent — array
  element access is future work there too. Indexed unnest, `(i, v) := *arr`,
  follows it; plain `(v) := *arr` works today.

- **Partial dict destructure with a binding.** `{k: v, **rest} := d` needs the
  remaining entries, which means subtracting the named keys — a `without_keys`
  builtin grasp-dbsp does not have. The exact and ignore-the-rest forms,
  `{k: v} := d` and `{k: v, **} := d`, need only `get` and `length` and work.

- **The rest of the type vocabulary.** `dynamic` — the top type, and the one
  most likely to be wanted first, since it is what an untyped subset of the
  language would be built on. Then the narrower integers and `f32`, the
  arbitrary-precision `integer` and `numeric`, `bytes` and `bits`, the temporal
  types, string encodings, and general `enum(...)` beyond the `boolean` case.
  Each needs a grasp-dbsp value type underneath before grasp can offer it.

- **Input rules.** Today `<- input` only declares that a relation comes from
  outside. The full mechanism in the Erlang implementation is much larger: the
  rule body defines a *candidate space*, and a runtime chooses which candidates
  to emit and supplies values for variables the body leaves unbound —
  with `limit`, `offset`, `order_by` for deterministic top-N, `snapshot` for
  atomic all-or-nothing transfer, and re-choice when the candidates change.

  It decomposes cleanly into three ordinary relations — the candidates, the
  runtime's choices, and their join — so nothing about grasp-dbsp blocks it.
  What it needs is a runtime that does the choosing, and there is not one here
  yet.

- **Reference types.** `closure(P, T)` and `result_equivalent_closure(P, T)`
  carry opaque references instead of values, so a large blob can flow through a
  rule without being materialised. They are backed by a storage service in the
  Erlang implementation, which is what makes them work and what this workspace
  does not have.

  One consequence is worth naming, because it is a visible divergence: there,
  every `f64` operation produces a `result_equivalent_closure` to cage the
  machine-dependence of IEEE arithmetic, resolvable only through an explicit
  eval against a shared cache. Here `f64` arithmetic is just arithmetic.

- **A computed dict lookup, and conversions.** grasp-dbsp has `get(d, k)`,
  `cast(x, T)` and `coalesce(x, d)`; grasp has none of them. A dict is read by
  the `{a: x} := d` pattern, which names its key; a document by a runtime
  filter; and an absent value is dropped by that same filter rather than
  defaulted. So what has no spelling is a *computed* key and an arbitrary change
  of type. Both are wanted; neither has been designed on grasp's own terms, and
  taking grasp-dbsp's because it has one is how `optional` division briefly got
  in.

- **A wider standard library.** The builtins available are the ones grasp-dbsp
  provides, listed in [`semantics.md`](semantics.md#builtins). The namespaces for types that
  do not exist yet — temporal, bytes, bits, cryptographic hashing — arrive with
  those types.

- **User-defined functions.** All callables are builtins. grasp-dbsp has
  `function` templates and `circuit` definitions that a frontend could use to
  give grasp its own, but nothing in grasp emits them yet.

## Relationship to other projects

- **grasp-dbsp** — the compilation target, specified in
  [`../grasp-dbsp/`](../grasp-dbsp/). grasp is one of possibly several frontends;
  the two meet at [`language.md`](../grasp-dbsp/language.md) and nowhere else.
  Note that the name also belongs to an unrelated Erlang project, which these
  documents always name as such — see
  [that overview](../grasp-dbsp/overview.md#relationship-to-other-projects).

- **Gordeev Grasp (Erlang)** — the implementation this design is ported from,
  and the source of the language. It runs Grasp on a purpose-built Erlang DBSP
  runtime rather than on the `dbsp` crate, and around the language it grows a
  whole platform: a record store, services, subcircuits, checkpoints, commits,
  branchpoints, hot-reload and provenance tracking. None of that comes here. The
  language, the type system and the compiler middle-end do.

  Where this dialect diverges from that one, [`mapping.md`](mapping.md) records
  it.

- **Datalog** — the tradition. grasp takes stratified negation and stratified
  aggregation from it. It departs in having a real type system, an
  indentation-based body syntax, expressions and destructuring in rule bodies,
  and no `:-`.

## Architecture

`grasp-compiler` is a pipeline. Each stage is described in
[`compilation.md`](compilation.md) except the last, which is
[`mapping.md`](mapping.md).

| stage | produces | described in |
|---|---|---|
| parse | an AST of rules and specs | [`syntax.md`](syntax.md) |
| desugar | the core forms | [`semantics.md`](semantics.md#desugaring) |
| infer | a type for every variable and column | [`inference.md`](inference.md) |
| join graph | one unordered graph per rule — atoms as nodes, variables as wires | [`compilation.md`](compilation.md) |
| optimizer | an evaluation order, chosen against a cost model | [`compilation.md`](compilation.md) |
| computation DAG | one ordered, column-level DAG per rule | [`compilation.md`](compilation.md) |
| SCC analysis | rules grouped into strata; recursive components identified | [`compilation.md`](compilation.md) |
| emission | a grasp-dbsp program | [`mapping.md`](mapping.md) |

The rules each stage enforces are stated where the stage is, with the
diagnostic each produces — a safety error in `semantics.md`, a type error in
`types.md`, and so on. There is no separate error catalogue to drift from them.

The layering is: grasp text → AST → core → typed core → join graph →
computation DAG → grasp-dbsp text → (grasp-dbsp's own pipeline) → a `dbsp`
circuit.
