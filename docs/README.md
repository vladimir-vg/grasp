# Design documents

This workspace holds two languages, one directory each. They are designed
separately and meet at exactly one file. A third document,
[`grasp-dbsp/serving.md`](grasp-dbsp/serving.md), specifies not a language but
the server that runs one.

## [`grasp-dbsp/`](grasp-dbsp/) — the target language

Declarative dataflow, executed incrementally as a DBSP circuit. Implemented by
[`grasp-dbsp`](../crates/grasp-dbsp), which parses it and builds
the circuit at startup with no code generation, and served over HTTP by
[`grasp-dbsp-server`](../crates/grasp-dbsp-server).

- [`overview.md`](grasp-dbsp/overview.md) — goals, design principles, scope, future work, architecture
- [`language.md`](grasp-dbsp/language.md) — the language as implemented: types, operators, expressions
- [`mapping.md`](grasp-dbsp/mapping.md) — how the language maps onto the `dbsp` crate

## [`grasp/`](grasp/) — the source language

A Datalog dialect that compiles to grasp-dbsp. Implemented by
[`grasp-compiler`](../crates/grasp-compiler). It has no runtime of its own —
rules become grasp-dbsp, and grasp-dbsp is what runs.

- [`overview.md`](grasp/overview.md) — goals, design principles, scope, future work
- [`syntax.md`](grasp/syntax.md) — lexical structure, the grammar, the AST
- [`types.md`](grasp/types.md) — the type catalog, assignability, runtime filters
- [`inference.md`](grasp/inference.md) — how every type is found
- [`semantics.md`](grasp/semantics.md) — what a program means: rules, recursion, negation, aggregation, safety, stratification
- [`compilation.md`](grasp/compilation.md) — join graph, optimizer, computation DAG, strata
- [`mapping.md`](grasp/mapping.md) — how grasp is emitted as grasp-dbsp

## Where the two meet

[`grasp-dbsp/language.md`](grasp-dbsp/language.md), and nowhere else. It is what
`grasp-compiler` emits and what `grasp-dbsp` accepts, which is why these
documents live at the workspace root rather than inside either crate. A change
to that file is a change to the contract between them.

grasp-dbsp is a deliberate *compilation target* — explicit and uniform rather
than convenient, because it is written by a compiler or an agent and not by
hand. [`grasp-dbsp/overview.md`](grasp-dbsp/overview.md) records the principles
that follow from this, and they are worth reading before emitting it.

**A note on the name.** `grasp-dbsp` here always means the language specified in
[`grasp-dbsp/`](grasp-dbsp/) — including where a crate shares the name, as
`grasp-compiler` shares `grasp`: the crate implements the language, and which
one is meant is never in doubt from the sentence it appears in. There is also an
unrelated Erlang project called grasp-dbsp, which is where the grammar was
borrowed from; these documents always name it explicitly. See
[`grasp-dbsp/overview.md`](grasp-dbsp/overview.md#relationship-to-other-projects).
