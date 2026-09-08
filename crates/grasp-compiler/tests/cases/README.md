# YAML fixtures

Every `.yaml` file here holds a **list of cases**, and each case becomes one
test named `<path>::<index>_<name>`, the path being the file's place under
`tests/cases` without its extension:

```
cargo test -p grasp-compiler --test yaml                 # all of them
cargo test -p grasp-compiler --test yaml syntax/         # one directory
cargo test -p grasp-compiler --test yaml operators       # one file
cargo test -p grasp-compiler --test yaml operators::3_   # one case
cargo test -p grasp-compiler --test yaml -- --list       # the inventory
```

## Two directories

**`syntax/` asks whether the text is grasp at all; `programs/` asks what the
program means and what it computes.** A semantic rejection and an execution case
are both answers to the second question — one says a program is wrong, the other
says what a correct one does — so they live together rather than being split
again by how far down the pipeline they get.

Where a case goes follows from what it asserts:

| a case asserting | goes to |
|---|---|
| a `pass: parse` diagnostic | `syntax/` |
| a diagnostic from any later pass | `programs/` |
| `equivalent_to`, or an output mode | `programs/` |
| `expected_ok` | wherever its topic lives |

The last row is the one that needs saying. `expected_ok` is **not** a
parser-level assertion — it compiles the program and hands the result to
`grasp-dbsp-runner` — so it belongs with the subject it illustrates rather than
with a stage. An operator-precedence case that happens to be accepted is a
syntax case; a case about what a fact means is a program case.

The harness is [`tests/yaml.rs`](../yaml.rs). The language these exercise is
grasp, specified in [`docs/grasp/`](../../../../docs/grasp/).

## A case

Every case has `name`, `source`, and exactly one assertion. The `expected_`
prefix is the convention: if it asserts something, it is called
`expected_something`.

| key | asserts |
|---|---|
| `expected_ok` | the program is accepted by every stage that exists |
| `expected_diagnostics` | the program is rejected, with exactly these diagnostics |
| `equivalent_to` | another grasp program; both emit identical grasp-dbsp |
| `expected_output` | these output deltas appear; extras are ignored |
| `expected_exact_output` | the output deltas are exactly these |

`name` is required — at three hundred cases, `errors::47` tells you nothing.

Unknown keys are rejected, so a misspelled key fails loudly instead of silently
asserting nothing.

## Cases outrun the compiler

The pipeline is parse → desugar → infer → plan → emit, and it does not reach the
end yet. **The compiler says what it cannot do**, through a diagnostic carrying
`Diagnostic::unimplemented`, and the harness reads that:

- a case that fails because the compiler said so is **pending**;
- a case that fails any other way is a **failure**.

That is the whole rule. Nothing is `#[ignore]`d, so nothing quietly stops being
parsed, and each run ends with the debt, grouped by what is blocking it:

```
43 pending: aggregates 9, recursion 5, negation 4, the stages after parsing 25
```

which is a work queue rather than a census — the largest number is the feature
that would free the most fixtures.

`expected_ok` is the exception, and stays live: it claims only that nothing
*rejects* the program, and an unimplemented construct does not reject it. That is
what makes it the floor, and why it silently demands more as the compiler grows.

**There is nothing to remember.** A case goes live the moment the compiler stops
saying it cannot — no marker to add, none to remove, and so no check needed
against forgetting either. A pending case that starts failing is the compiler
being wrong, and says so.

So: **write fixtures before the code that satisfies them.** Write them a little
ahead, though, not five stages ahead — a fixture written against a pass nobody
has designed is wrong in ways nothing detects.

### `skip` is not the same thing

```yaml
skip: "array destructure needs element access — overview.md#future-work"
```

`skip:` is only for constructs blocked on **named future work**, and its reason
must name a section of
[`docs/grasp/overview.md#future-work`](../../../../docs/grasp/overview.md#future-work).
A skipped case is not run at all, because the construct may not even parse.

Anything blocked merely on *time* is pending, not skipped. The difference is that
pending resolves itself and skip does not, which is why skip has to name the
thing that would unblock it.

## `expected_ok`

```yaml
- name: a rule body may span several lines
  source: |
    result(name: n, title: t) <-
        student(id: s, name: n)
        course(id: s, title: t)
  expected_ok: true
```

"This program is legal." A construct the compiler has not implemented is
tolerated — it is not a rejection — which is what lets this be written today.
Everything else is not: a diagnostic the compiler *means* fails the case.

So the claim tightens on its own as the compiler grows, from "nothing rejects
this" to "nothing rejects this and the emitted grasp-dbsp is accepted by
`grasp-dbsp-runner`", without the fixture changing.

There is no `expected_ok: false` — a case that expects rejection should say which
diagnostics it expects.

## `expected_diagnostics`

```yaml
- name: comparison cannot be chained
  source: |
    ok(v: a) <-
        r(a: a, b: b, c: c)
        a < b < c
  expected_diagnostics:
    - severity: error
      pass: parse
      message: "comparison cannot be chained"
      line: 3
```

**Only the fields a case writes are checked**, except `pass`, which is required.
Omitting `column` asserts nothing about the column; that is what lets the
diagnostic model grow — new fields, and eventually several diagnostics per
pass — without editing existing fixtures.

`message` is a substring match. Everything else is exact. `pass` is `parse`,
`desugar`, `infer`, `plan` or `emit`; `severity` is `error`, `warning` or `note`.

Matching is an **exact set, order-independent**: every listed diagnostic must
appear and nothing else may, since a spurious extra error is itself a bug.

`pass` is required for two reasons. It is what decides when the case goes live
(above). And it stops a fixture whose own source has a typo — which would also
fail to compile — from being satisfied by an unrelated parse error. Give each
case a `message` specific enough to do the same.

## `equivalent_to`

```yaml
- name: body statement order does not change the emission
  source: |
    rich(name: n) <-
        emp(name: n, sal: s)
        s > 100
  equivalent_to: |
    rich(name: n) <-
        s > 100
        emp(name: n, sal: s)
```

The two programs must emit **byte-identical** grasp-dbsp.

Byte equality is the assertion, not a weakening of one.
[`compilation.md`](../../../../docs/grasp/compilation.md) requires the optimizer
to be deterministic, and [`syntax.md`](../../../../docs/grasp/syntax.md) requires
that nothing downstream keys on source position — so two programs that mean the
same thing must emit the same text, and this mode holds the compiler to both.

It is how the desugaring table in
[`semantics.md`](../../../../docs/grasp/semantics.md#desugaring) is tested: one
case per row, sugar against its expansion, with no need to expose the core form
at all.

**Its weakness, which every file using it must cover: `equivalent_to` proves the
two programs agree, not that either is right.** A file that uses it must also
carry at least one `expected_ok` or output case over one of the two sources.

## Output cases

`input` is a list with one entry per transaction; `expected_*_output` is a list
of the same length. Both are keyed by **grasp relation name** — an external
relation emits as `r := input("r")` and a derived one as a node named `r`, so
there is nothing else it could be. Use `{}` for a transaction that produces
nothing.

A row is `[weight, value]`. A grasp relation is always a flat
`zset(record(...))` — indexed streams never correspond to one — so there is none
of the flat-versus-indexed arity distinction the runner's own fixtures have.

```yaml
- name: transitive closure, then retract the middle edge
  source: |
    edge :: relation(src: i64, dst: i64)
    edge(src:, dst:) <- input

    path(src: x, dst: y) <- edge(src: x, dst: y)
    path(src: x, dst: y) <-
        path(src: x, dst: z)
        edge(src: z, dst: y)

  input:
    - edge:
        - [1, {src: 1, dst: 2}]
        - [1, {src: 2, dst: 3}]
    - edge:
        - [-1, {src: 2, dst: 3}]

  expected_exact_output:
    - path:
        - [1, {src: 1, dst: 2}]
        - [1, {src: 2, dst: 3}]
        - [1, {src: 1, dst: 3}]
    - path:
        - [-1, {src: 2, dst: 3}]
        - [-1, {src: 1, dst: 3}]
```

Scalars are written as native YAML values (`src: 1`, not `src: "1"`). Values go
through `grasp-dbsp-runner`'s own codec, so there is no second decoder here and a
fixture cannot drift from the real wire format; a value that does not match the
declared column is an error rather than being quietly coerced.

**Prefer `expected_exact_output`.** Subset matching passes when the compiler
emits rows nobody expected, which is the bug class these tests exist to catch.

Retractions and incrementality need no special support: they are a negative
weight and a second epoch.

**Facts are specified now, but no output case can use one yet.** grasp-dbsp gained
`constant` for exactly this, and
[`docs/grasp/mapping.md`](../../../../docs/grasp/mapping.md#facts) says what a
fact emits — but the pipeline stops well short of emission, so such a case would
be pending like any other. Until then, declare data with `:: relation(...)` and
`<- input`. What facts *are* is covered in `relations.yaml`.

When emission does land: a fact-only program feeds nothing, so its `input` is
`[{}]` — one transaction, no rows — and the facts arrive in it.

## Every relation must be declared

A relation a program mentions needs a `:: relation(...)` spec or a definition —
a rule with it as head, a fact, or `<- input`. That is
[`inference.md`](../../../../docs/grasp/inference.md#diagnostics)'s rule, not the
suite's, but until `infer` exists nothing would catch a fixture breaking it, and
124 of 185 cases had. So [`tests/declared.rs`](../declared.rs) checks it here.

It exempts two kinds of case: one whose only assertion is a `pass: parse`
diagnostic, since parse short-circuits and inference never runs on it, and one
that *asserts* the rule's own diagnostic, since breaking the rule is its job.

The scaffolding a case needs is usually one line — a spec with no rules is a
declaration, and the relation is simply empty:

```yaml
- name: multiplication binds tighter than addition
  source: |
    s :: relation(a: i64, b: i64, c: i64)

    r(v: n) <-
        s(a: a, b: b, c: c)
        n := a + b * c
  expected_ok: true
```

Choose the column types to match how the variables are *used*, not just to fill
the slot: `expected_ok` will demand they typecheck once `infer` lands, and grasp
has no implicit numeric conversion.

`declared.rs` is scaffolding with an end — when `infer` implements the check,
every `expected_ok` case enforces it and the file should be deleted rather than
kept in step.

## Always on

Whatever a case asserts, if it reaches emission the text is handed to
`grasp_dbsp_runner::compile` and the case fails if the runner rejects it. That
covers the whole class of "emitted something the target does not accept" without
any fixture having to ask, which is what an emission snapshot would really have
been guarding — and unlike a snapshot it cannot rot.

## The files

`syntax/`, covering [`syntax.md`](../../../../docs/grasp/syntax.md):

| file | covers |
|---|---|
| `lexical.yaml` | literals, string escapes, comments, unsigned numbers vs. unary minus, qualified identifiers |
| `indentation.yaml` | body width, blank and comment lines inside a body, dedent ending a rule |
| `operators.yaml` | precedence within a group, the three cross-group relations, every other mixing rejected |
| `grammar.yaml` | specs, facts, the `kv_arg` forms, trailing commas, `<- input`, calls |
| `names.yaml` | reserved words and namespace prefixes, the one-name rule, wildcards, duplicates |
| `dict_literals.yaml` | the two spellings, mixed, quoted keys |
| `patterns.yaml` | unnest and destructure forms, and what a pattern may not be |

`programs/`:

| file | covers |
|---|---|
| `relations.yaml` | facts, rules with no atom, relations with no columns — and the rejections the spec states verbatim |
| `safety.yaml` | a variable the body does not bind |
| `assignability.yaml` | what a value of one type may be used as |
| `expressions.yaml` | what a body expression computes, and where it has no answer |
| `desugar.yaml` | the desugaring table, as `equivalent_to` pairs |
| `normalization.yaml` | statement and rule order do not change the emission — including the two component-ordering cases the optimizer's forest must respect |
| `recursion.yaml` | the transitive closure worked example, end to end |
| `smoke.yaml` | the worked programs from the spec, end to end |

Still to write, a little ahead of the code that satisfies them — named here so a
spec change has an obvious fixture home. All of them are `programs/`, since
`syntax/` is as complete as the grammar is:

- **desugar** — the `expected_core` half of `patterns.yaml`.
- **infer** — `optional`, `json`, `runtime_filters`, `comparison`,
  `inference_fixpoint`, `inference_compose`, `inference_literals`,
  `inference_overloads`, `specs`, and more of `safety` and `assignability`.
- **plan** — `optimizer`, `stratification`, and the diagnostic halves of
  `negation`, `aggregation`, `input_relations`.
- **emit** — `rules`, `joins`, `builtins`, `unions`, more of `expressions`, and
  the end-to-end halves of everything above.
