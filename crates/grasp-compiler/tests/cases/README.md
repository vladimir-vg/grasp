# YAML fixtures

Every `.yaml` file here holds a **list of cases**, and each case becomes one
test named `<file stem>::<index>_<name>`:

```
cargo test -p grasp-compiler --test yaml                 # all of them
cargo test -p grasp-compiler --test yaml operators       # one file
cargo test -p grasp-compiler --test yaml operators::3_   # one case
cargo test -p grasp-compiler --test yaml -- --list       # the inventory
```

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
end yet. `IMPLEMENTED` in [`src/lib.rs`](../../src/lib.rs) says how far it does
reach.

A case needing a later pass than that is expected to **fail**, and is reported as
*pending* rather than as a failure. Nothing is `#[ignore]`d, so nothing quietly
stops being parsed, and each run ends with the debt:

```
312 pending (the pipeline reaches `parse`): desugar 12, infer 40, plan 18, emit 55
```

**The pass a case needs is derived from what it asserts, not annotated** — there
is no marker to forget, and none to leave behind:

| assertion | needs |
|---|---|
| `expected_diagnostics` | the latest `pass` it lists |
| `expected_ok` | `parse` — it means "accepted so far", so it is always live |
| `equivalent_to`, `expected_*output` | `emit` |

**A pending case that starts passing is a failure**, saying so. That is the whole
anti-rot mechanism: the day a stage lands, bumping `IMPLEMENTED` by one variant
turns every fixture for that stage live at once, and a fixture that passes early
is either a stage nobody recorded or a case asserting less than it claims to.

So: **write fixtures before the stage that satisfies them.** Write them one stage
ahead, though, not five — a fixture written against a pass nobody has designed is
wrong in ways nothing detects.

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

"This program is legal." Diagnostics from a pass the pipeline has not reached
yet are tolerated, which is what lets this be written today; as stages land the
same fixture demands more, and once emission works it also requires that
`grasp-dbsp-runner` accepts what was emitted.

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

**Facts have somewhere to go now, but no output case can use one yet.**
grasp-dbsp gained `constant` for exactly this, and
[`docs/grasp/mapping.md`](../../../../docs/grasp/mapping.md#facts) says what a
fact emits — but the pipeline stops well short of emission, so such a case would
be pending like any other. Until then, declare data with `:: relation(...)` and
`<- input`. Parsing facts is well defined and covered in `grammar.yaml`.

When emission does land: a fact-only program feeds nothing, so its `input` is
`[{}]` — one transaction, no rows — and the facts arrive in it.

## Always on

Whatever a case asserts, if it reaches emission the text is handed to
`grasp_dbsp_runner::compile` and the case fails if the runner rejects it. That
covers the whole class of "emitted something the target does not accept" without
any fixture having to ask, which is what an emission snapshot would really have
been guarding — and unlike a snapshot it cannot rot.

## The files

Live now, covering [`syntax.md`](../../../../docs/grasp/syntax.md):

| file | covers |
|---|---|
| `lexical.yaml` | literals, string escapes, comments, unsigned numbers vs. unary minus, qualified identifiers |
| `indentation.yaml` | body width, blank and comment lines inside a body, dedent ending a rule |
| `operators.yaml` | precedence within a group, the three cross-group relations, every other mixing rejected |
| `grammar.yaml` | specs, facts, the `kv_arg` forms, trailing commas, `<- input`, calls |
| `names.yaml` | reserved words and namespace prefixes, the one-name rule, wildcards, duplicates |
| `dict_literals.yaml` | the two spellings, mixed, quoted keys |
| `patterns.yaml` | unnest and destructure forms, and what a pattern may not be |

Pending, because the spec gives them verbatim:

| file | covers |
|---|---|
| `desugar.yaml` | the desugaring table, as `equivalent_to` pairs |
| `recursion.yaml` | the transitive closure worked example, end to end |
| `smoke.yaml` | the worked programs from the spec, end to end |

Still to write, one stage ahead of the pass that satisfies them — named here so
a spec change has an obvious fixture home:

- **desugar** — the `expected_core` half of `patterns.yaml`.
- **infer** — `assignability`, `optional`, `json`, `runtime_filters`,
  `comparison`, `inference_fixpoint`, `inference_compose`, `inference_literals`,
  `inference_overloads`, `specs`, `safety`.
- **plan** — `normalization`, `optimizer`, `stratification`, and the diagnostic
  halves of `negation`, `aggregation`, `input_relations`.
- **emit** — `joins`, `expressions`, `builtins`, `unions`, and the end-to-end
  halves of everything above.
