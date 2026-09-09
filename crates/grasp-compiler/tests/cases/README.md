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
program means and what it computes; `inference/` asks what the compiler concluded
about it.** A semantic rejection and an execution case are both answers to the
second question — one says a program is wrong, the other says what a correct one
does — so they live together rather than being split again by how far down the
pipeline they get.

`inference/` is the exception to that, and it earns it by asking a different
question. Its cases reach into the middle of the compiler: they read the types
`infer` settled on, or the rejections only `infer` can make. Nothing there is
about what a program computes, and nothing in `programs/` can see a type.

Where a case goes follows from what it asserts:

| a case asserting | goes to |
|---|---|
| a `pass: parse` diagnostic | `syntax/` |
| `expected_types`, or a `pass: infer` diagnostic | `inference/` |
| a diagnostic from any later pass | `programs/` |
| `equivalent_to`, or an output mode | `programs/` |
| `expected_ok` | wherever its topic lives |

The last row is the one that needs saying. `expected_ok` is **not** a
parser-level assertion — it compiles the program and hands the result to
`grasp-dbsp-runner` — so it belongs with the subject it illustrates rather than
with a stage. An operator-precedence case that happens to be accepted is a
syntax case; a case about what a fact means is a program case.

**And a rejection written to contrast with an acceptance stays beside it**, which
is the same principle one row up. `relations.yaml` keeps its `pass: infer`
diagnostics for that reason: *"a relation with a typespec and no rules is
declared"* is accepted and *"a relation needs a non-recursive rule or a typespec"*
is rejected, and the pair is the lesson — filing them apart by stage would leave
two halves of an argument in different directories. `syntax/lexical.yaml` holds
the corpus's one `pass: desugar` diagnostic on the same grounds: the case exists
to show the text *parses*.

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
| `expected_types` | inference gave these relations and rules these types |

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

`expected_types` is blocked by a *shorter* pipeline, because it reads inference's
output and stops there: parse, desugar and infer can block it, and the stages
after them cannot. That distinction is not cosmetic — every program is
unimplemented at `plan` today, so a case judged against the whole pipeline would
be pending forever and pass while asserting nothing.

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

## `expected_types`

```yaml
- name: transitive closure infers i64 throughout
  source: |
    edge :: relation(src: i64, dst: i64)
    edge(src:, dst:) <- input

    path(dst: d) <- edge(dst: d)
    path(dst: d) <-
        path(dst: m)
        edge(src: m, dst: d)
  expected_types:
    path:   {dst: i64}
    path:0: {d: i64}
    path:1: {d: i64, m: i64}
```

The only mode that can see what inference concluded. Every other one reads what
the compiler **emitted** or **rejected**, and both erase types — two programs
that differ only in the type a variable was given are the same program to them.

**Keys.** A bare name is a relation, and its map is column to type. `rel:N` is
the Nth rule, **counting from zero in source order**, whose head names `rel`, and
its map is variable to type. No relation can be called `rel:0` — a namespace
segment needs a letter after the colon — so the two never collide, and the key
needs no quoting in YAML.

**Variables are rule-scoped.** `path:0` and `path:1` above have their own maps,
and that is the point of the ordinal: a relation's column may be `optional(i64)`
while the variable filling it in one rule is plainly `i64`.

**Types are compared as strings**, against the canonical spelling a diagnostic
would quote — `dict(string, i64)`, `record(age: i64, name: string)` with the
fields sorted, a space after every `,` and `:`.

**A non-canonical spelling cannot pass.** The comparison is byte equality against
that spelling, with no normalising and no parsing the expectation back into a
type, so this mode pins how a type is *written* as well as which type it is.
Nothing else in the corpus does, and a fixture's own `source:` cannot: the type
parser accepts `dict(string,i64)` and an unsorted `record`, and only `Display`
decides the one spelling out of those that is canonical.

Two things follow for anyone writing a case. Spacing is checked when the file is
read rather than when the types are compared, because a case that goes pending
never reaches the comparison, and a typo sitting unnoticed inside one is the
silence this mode exists to remove. And **a compound type is quoted, a bare name
is not** — `i64` plain, `"optional(i64)"` and `"dict(string, i64)"` quoted. That
is a rule rather than a taste: these maps are written in YAML flow style, where an
unquoted `{m: dict(string, i64)}` quietly becomes *two* entries and surfaces as a
parse error blaming the file.

**Naming is partial; a name is not.** A column or variable the case does not
mention is not checked, and neither is a relation it does not mention, so a case
may pin one variable of one rule and say nothing else. But naming something that
does not exist is a failure: leaving a key out is a choice, getting one wrong is
a typo.

Two things it is not for:

- **It is not a way to un-pend a case.** If the claim is observable in the
  output, write the output case — this mode reaches into the middle of the
  compiler, and that is only worth doing for something the ends cannot see.
- **It asserts what inference *concluded*, not what the program *declared*.**
  Pinning the columns of a relation that has a spec, on its own, tests the
  parser: a declared relation is never widened by its rules, so those columns are
  the spec read back. Pin the rules, or a relation that has no spec.

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
a rule with it as head, a fact, or `<- input`. So does a fact's untyped literal
need somewhere to take a type from.

Both were once checked by a `tests/declared.rs` that existed only because
`infer` did not: 124 of 185 cases had drifted into breaking the first rule, and
nothing would have caught them. `infer` now produces both diagnostics itself —
the declaration one *is* its fixpoint's stall report — so that file is gone and
the rules are enforced by the compiler rather than beside it.

What survives from it is `assert_every_directory_was_walked`, now in
[`tests/common/mod.rs`](../common/mod.rs) and called from the harness: a case
that is never read is a case that never fails, and a new subdirectory the walk
does not reach would hide silently.

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
the slot: grasp has no implicit numeric conversion, and an `expected_ok` case
must typecheck.

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
| `expressions.yaml` | what a body expression computes, and where it has no answer |
| `desugar.yaml` | the desugaring table, as `equivalent_to` pairs |
| `normalization.yaml` | statement and rule order do not change the emission — including the two component-ordering cases the optimizer's forest must respect |
| `recursion.yaml` | the transitive closure worked example, end to end |
| `smoke.yaml` | the worked programs from the spec, end to end |

`inference/`, covering [`inference.md`](../../../../docs/grasp/inference.md):

| file | covers |
|---|---|
| `fixpoint.yaml` | schemas crossing relations that have no spec, and the round a recursive rule types on |
| `compose.yaml` | two rules giving one name two types, and where the column and the rule differ |
| `literals.yaml` | what an untyped literal settles as, and what told it |
| `overloads.yaml` | what a builtin or an aggregator gives back |
| `safety.yaml` | a variable the body does not bind |
| `assignability.yaml` | what a value of one type may be used as |

Still to write, a little ahead of the code that satisfies them — named here so a
spec change has an obvious fixture home. `syntax/` is as complete as the grammar
is, so these are all `inference/` or `programs/`:

- **desugar** — the `expected_core` half of `patterns.yaml`, in `programs/`.
- **infer** — `optional`, `json`, `runtime_filters`, `comparison`, `specs`, and
  more of `safety` and `assignability`, all in `inference/`.
- **plan** — `optimizer`, `stratification`, and the diagnostic halves of
  `negation`, `aggregation`, `input_relations`.
- **emit** — `rules`, `joins`, `builtins`, `unions`, more of `expressions`, and
  the end-to-end halves of everything above.
