# grasp — Type inference

Every variable in every rule, and every column of every relation, gets a type.
Almost none of them are written down.

This is the one part of grasp where inference is **load-bearing** rather than a
convenience: a rule body names variables, not types, and requiring an annotation
per variable would make the language unusable for what it is for. Where
inference genuinely cannot reach, an annotation resolves it — but that is the
exception, and each one below says what it is.

The rules being applied are in [`types.md`](types.md). This document is the
algorithm.

## Shape

Four phases, run inside a fixpoint over the program:

```
1. per rule      constraints on each variable, composed
2. across rules  one relation, several rules, one column type
3. literals      untyped numbers take a type from context
4. resolution    builtin overloads; runtime filters inserted
```

Phases 1–2 iterate: typing a relation's columns may unlock a rule that mentions
it. Phases 3–4 run once the shapes have settled.

**Types are erased afterwards.** Nothing downstream carries a grasp type; the
only trace is the record shapes the emitted program declares.

## The fixpoint

Relations are typed in dependency order, which the program does not give
directly — a rule may mention a relation defined later, and recursive relations
mention each other.

```
known = { r : columns(r) for each r with a `:: relation(...)` spec }

repeat
    for each rule whose body atoms all reference relations in `known`
        run phases 1 and 2 for it
        add or refine its head relation in `known`
until no relation's type changed
```

**A relation declared `<- input` starts known**, from its required spec. That is
what seeds the loop; a program with no input relations and no specs has nothing
to start from, and every relation in it is reported unknown.

**Recursive relations settle in this loop like any other.** A recursive rule's
body mentions the very relation being defined, so it cannot be processed first —
but a recursive relation always has at least one non-recursive rule, or it is
empty and useless. That rule types the head, and the recursive rules are then
checked against it. Mutual recursion works the same way, one member at a time.

> ``relation `r` has no non-recursive rule and no typespec, so nothing gives it
> a type``

## Phase 1: per rule

For each variable in a rule, collect every constraint on it, then compose them.

### Where a constraint comes from

| source | the constraint |
|---|---|
| positive atom `rel(col: v)` | `v` is the column's type |
| literal in an atom `rel(col: 3)` | the column's type must accept the literal |
| match `v := expr` | `v` is `expr`'s type |
| unnest `(v) := *arr` | `arr` is `array(E)`; `v` is `E` |
| unnest `(i, v) := *arr` | and `i` is `i64`, whatever `E` is |
| unnest `(k, v) := **d` | `d` is `dict(K,V)`; `k` is `K`, `v` is `V` |
| destructure `[x, y] := arr` | `arr` is `array(E)`; `x` and `y` are `E` |
| destructure `{a:} := d` | `d` is `dict(string, V)`; `a` is `V`, definite |
| destructure `record(a:) := s` | `s` is a record with field `a`; `a` is its type |
| destructure `record(a:, **e) := s` | and `e` is a record of `s`'s other fields |
| dict literal `{k => v}` | one `K` over the keys, one `V` over the values |
| dict literal `{a: v}` | `K` is `string`; `v` contributes to `V` |
| aggregate `v := sum<e>` | `v` is the aggregator's result for `e`'s type |
| field access `e.f` | `e` is a record with field `f` |
| assertion `v :: T` | `v` is `T`, or is filtered to `T` — see phase 4 |
| head `r(col: v)` | `v` must be assignable to the column, if `r` is declared |
| filter `expr` | `expr` is `boolean` |

An atom is the only thing that gives a variable a type *from outside* the rule.
Everything else relates variables to each other.

A destructure's variable is **definite** where the source is not: `dict:get`
yields `optional(V)` and the pattern promises the key is there, so `a` is a `V`
and the row whose dict lacked the key is not derived. That is why the patterns
are typed here rather than expanded before this pass — the narrowing has to name
`V`, and nothing knows it earlier. `record(a:)` needs no narrowing at all: a
record's fields are its type, so the field is definite already, and what the
pattern claims about the fields it did *not* name is checked here too.

### Composing constraints

Given several constraints on one variable:

- **Identical types** compose to that type.
- **`T` and `optional(T)`** compose to `optional(T)`. A variable that some path
  leaves absent is optional everywhere.
- **Same constructor** — two `array`, two `dict`, two `record` — compose
  covariantly, recursing on element, key/value, and same-named field types. Two
  records with different field *names* do not compose.
- **An untyped literal** composes with anything it can inhabit, and defers; see
  phase 3.
- **Anything else** is a conflict.

Composition is **not** widening. Two distinct concrete types do not meet in some
third type — there is no numeric tower here for them to meet in, which is what
makes this rule short.

> ``variable `x` is used as `i64` here and as `string` at line N``

### Collecting them is a fixpoint

A constraint can name a variable that another constraint types, and a rule body
is a set, so "collect every constraint" cannot mean one walk from the top:

```grasp
q(v: n) <-
    n := a * 2
    r(a: a)
```

`n := a * 2` is read before anything says what `a` is. So the collection runs
again on what it learned, until nothing moves — which for a chain of matches
takes as many rounds as the chain is long, and is bounded the same way the
per-program loop is. A rule that does not settle falls out with its variables
open, which phase 3 reports.

The rounds before the last are **silent**, for the reason the per-program loop's
are: a conflict seen in an early round may be an artefact of what that round had
not yet read. A reported type does not carry into the next round either, since it
absorbs everything it composes with and would make the conflict compose cleanly
the second time.

### Safety, checked here

Every variable in the head, and every variable a negated atom, filter or
assertion mentions, must be **bound** — produced by a positive atom or a match
somewhere in the body. This is Datalog's safety condition, and
[`semantics.md`](semantics.md#safety) says why it is what makes a rule finite.

> ``variable `x` appears in the head but nothing in the body binds it``

### Aggregate scope, checked here

An aggregate folds a whole group into one value, so where its result has a value
the only other things that do are **the group** and the other aggregates'
results. A variable that varies *within* the group has none, and reading one is
refused rather than answered.

The group is the variables the head's non-aggregate columns read
([`semantics.md`](semantics.md#aggregation)). Everything computed from an
aggregate result is in the group's scope with it, transitively — the rule is
closed under *reading*, not over statement order, because a rule body is a set.

Four rejections, and none of them is about where a line was written:

- a statement, **or the head**, that reads an aggregate result and also a
  variable outside the group;
- an **aggregate** whose argument reads another aggregate's result — it folds the
  body's assignments, and a result is not one of them;
- a **positive atom** that mentions one — an atom is among the things being
  folded, so joining a grouped result against a relation takes a second rule,
  which is the ordinary Datalog shape;
- a variable **bound both** by the body and from an aggregate result.

> ``variable `r` is not in the group, so it has no value where the aggregate `s` does``

The rule is on variables rather than values, so a binding that happens to be
constant within the group is refused too — `m := concat(d, "x")` followed by
`s > length(m)`, where `m` varies with `d` alone. SQL refuses the same thing.
The way out is always available and mechanical: inline the definition, since it
reads only group variables.

## Phase 2: across rules

A relation defined by several rules gets one column type per column. **A fact
counts as a rule here** — it constrains its relation's columns exactly as a rule
head does, and its arguments are closed, so each contributes the type of a
literal rather than of a variable.

That closedness is also what limits it. A fact's arguments have no body to take
a type from, so an untyped literal in one has exactly two sources: the
relation's `:: relation(...)` spec, or another rule for the same relation. A
relation whose only definitions are facts of untyped literals has neither, and
is reported by [phase 3](#phase-3-literals-take-their-type-from-context) rather
than defaulted.

```grasp
reachable(node: 1)          # `1` from the rule below, not from a spec
reachable(node: n) <-
    reachable(node: m)
    edge(src: m, dst: n)
```

That is the one place a fact's literal is settled across phase 2 instead of from
a spec, and it is why the two phases run inside one fixpoint rather than in
sequence.

- **With a `:: relation(...)` spec**, each rule's inferred column type must be
  assignable to the declared one. The spec is the answer; a rule that disagrees
  is the error.
- **Without one**, the column type is composed across rules by the phase 1
  rules. `T` in one rule and `optional(T)` in another gives `optional(T)`; two
  incompatible types are an error naming both rules.

A spec never *overrides* what a rule infers — it constrains it. A rule producing
`string` for a column declared `i64` is rejected, not coerced.

> ``relation `r` column `c` is `i64` at line N and `string` at line M``
> ``rule at line N gives `r.c` type `string`, but it is declared `i64` ``

A relation declared `<- input` must have at least one column. A relation with
none is a proposition, and rows pushed into one could only be counted rather than
collected — so it has to be derived, not supplied. See
[`types.md`](types.md#a-relation-with-no-columns).

> ``relation `ready` has no columns and cannot be an input``

A rule head must name **every** column of a declared relation. Omitting one is
an error rather than an implicit absence, because a column that is sometimes
absent should say so in its type.

> ``rule head is missing column `c` of relation `r` ``

## Phase 3: literals take their type from context

An integer literal is not an `i64` until something says so. `42` can be `i64` or
`f64`; `[]` can be an `array(T)` for any `T`. Both wait.

- A literal used with a typed value takes that type: in `x + 1` where `x` is
  `f64`, the `1` is `f64`.
- A literal in an atom's argument takes the column's type.
- A literal with nothing to take from **is an error naming the variable**, not a
  silent default to `i64`. A default here would put a type nobody chose into a
  relation's schema, where every later rule would then have to agree with it.

An empty `[]` or `{}` is the same case one level up: a complete value with an
open type, resolved by context or reported.

> ``the literal at line N has no type here; nothing determines whether it is
> `i64` or `f64` ``

## Phase 4: overloads and filters

**Builtin overloads** resolve against the now-concrete operand types. Each
builtin has a fixed set of signatures — see
[`semantics.md`](semantics.md#builtins) — and exactly one must match.

> ``no version of `length` takes `i64` ``

**Runtime filters** are inserted where an assertion or a head column requires a
type the value is not assignable to, but a check could settle it — the table in
[`types.md`](types.md#runtime-filters). The filter becomes an ordinary body
statement, placed where the variable is bound, and narrows the variable for
everything after it.

This is the one place the compiler adds a statement the program did not write.
It is worth the exception: the alternative is rejecting `n :: string` on an
`optional(string)`, which is the most common thing a program wants to say.

Where no filter applies, it is an error — and specifically an error saying the
filter would be empty, since a check that can never pass is a relation that is
always empty and a bug that would otherwise be silent.

## Worked example

```grasp
edge :: relation(src: i64, dst: i64)
edge(src:, dst:) <- input

heavy(src: x, total: t) <-
    edge(src: x, dst: y)
    weight(edge: y, w: w)
    w > 10
    t := sum<w>
```

**Fixpoint.** `edge` is known from its spec. `weight` must be known too — from
its own spec or its own rules — or this rule waits, and is reported if it never
becomes known.

**Phase 1.** `edge(src: x, dst: y)` gives `x : i64` and `y : i64`.
`weight(edge: y, w: w)` gives `y : i64` again — composing identically, which is
also the join — and `w` its column type, say `i64`. The filter `w > 10` requires
`w` comparable and `10` to inhabit `w`'s type. `t := sum<w>` gives `t` the
result of `sum` over `i64`.

**Safety.** `x` and `t` appear in the head; `x` comes from an atom, `t` from a
match. Both bound.

**Phase 2.** `heavy` has no spec, so its columns are what this rule inferred:
`src: i64`, `total: i64`.

**Phase 3.** `10` takes `i64` from `w`.

**Phase 4.** `>` resolves at `i64`. No filter needed.

## Diagnostics

Every rejection this pass can produce, in the phase that produces it.

| rejection | phase |
|---|---|
| ``variable `x` is used as A here and as B at line N`` | 1 |
| ``variable `x` appears in the head but nothing in the body binds it`` | 1 |
| ``variable `x` is not in the group, so it has no value where the aggregate `s` does`` | 1 |
| ``` `f` folds the body's assignments, and `s` is an aggregate result rather than one of them ``` | 1 |
| ``atom `r` mentions the aggregate result `s`, but an atom is one of the things `s` is folded over`` | 1 |
| ``variable `x` is bound by the body and again from an aggregate result`` | 1 |
| ```e` is not a record, so it has no field `f` `` | 1 |
| ``` `count` takes no argument ``` | 1 |
| ``` `f` needs an argument: the expression to fold ``` | 1 |
| ``` `f` needs a numeric argument, found A ``` | 1 |
| ``ordering is not defined on A, so `f` has no meaning over it`` | 1 |
| ``relation `r` is not defined and has no typespec`` | 2 |
| ``relation `r` column `c` is A at line N and B at line M`` | 2 |
| ``rule at line N gives `r.c` type B, but it is declared A`` | 2 |
| ``rule head is missing column `c` of relation `r` `` | 2 |
| ``relation `r` has no non-recursive rule and no typespec`` | 2 |
| ``the literal at line N has no type here`` | 3 |
| ``no version of `f` takes A`` | 4 |
| ``this assertion would discard every row`` | 4 |
