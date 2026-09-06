# DBSP Runner — Expressions (design)

> **Status: design, not implemented.** The other documents in this directory
> describe what the runtime does today; this one describes a rewrite of the
> expression language that has been designed but not built. When it lands, this
> content folds into [`language.md`](language.md)'s "Functions and expressions"
> section and this file goes away.
>
> It arrived together with [`json.md`](json.md), which motivated it: making
> `json` a real type meant conversion had to become pattern matching, which
> needed a `match` expression, which needed functions with more than one
> expression in them.

The language is a **compilation target**. It does not have to be convenient and
it does not need syntactic sugar, but it does have to be explicit and uniform —
a higher-level compiler should be able to emit it mechanically, and a reader
should be able to tell what a construct does without knowing what surrounds it.

## What `dbsp` constrains — almost nothing

Worth stating first, because it is easy to assume otherwise. The bound on an
operator's function is an ordinary Rust closure — `F: Fn(&K) -> OK + Clone +
'static` for `map` over a flat stream, `F: Fn(&K, &V, &V2) -> OV + Clone +
'static` for `join` (`dbsp/src/mono.rs:837` and `:143`). It is **opaque** — `dbsp`
never inspects it, offers no expression IR, and has no conditional operator to
lean on. Feldera's SQL compiler emits Rust `if` because it is a code generator;
this runtime interprets, so every construct here is our own.

The real constraints come from incremental semantics rather than the API:

1. **Determinism.** A retraction is the same row at weight −1, so `f` runs on it
   again and must return what it returned for the insertion. If it doesn't, the
   two never annihilate and the Z-set silently accumulates — the same failure
   mode as invariant 2 in [`mapping.md`](mapping.md).
2. **Totality.** A panic inside `f` kills the worker thread and the circuit with
   it. There is no per-row error channel; `dbsp` has nowhere to put "this row
   failed". This is why the evaluator already returns `NONE` rather than
   trapping on division by zero and integer overflow (`expr.rs`).
3. **`Clone + Send + 'static`**, because `Runtime::init_circuit` runs the
   constructor once per worker. That is why `TypedExpr` sits behind an `Arc` in
   `PlanOp`.

Note `Fn`, not `FnMut`: no mutable state may be captured across rows.

## Functions

`fun` is removed. There is one construct, with two spellings:

```
function foo_bar(a, b, named_arg1: arg1) {
    d := a * 2
    x :: i64
    x := (a + b + d) / 2
    return match(arg1, NONE -> 0, v::f64 -> v, _ -> 3)
}

function((r) -> r.id)              # anonymous — sugar for { return r.id }
```

```
function_def := "function" NAME "(" [params] ")" "{" statement* "}"
anon_fn      := "function" "(" "(" [params] ")" "->" expr ")"

params       := param ("," param)*
param        := NAME                      # positional
              | NAME ":" NAME             # named — caller name : body name

statement    := "return" expr
              | NAME "::" value_type
              | NAME ":=" expr
```

Anonymous **multiline** — `function(r) { … }` — is illegal. An operator argument
that needs more than one statement must be a named function. That keeps the
anonymous form to exactly one shape.

### Statements

There are exactly three, and **a bare expression is not a statement**. A
function that computed something and discarded it would have written a no-op,
since expressions have no effects.

**Exactly one `return`, and its position does not matter.** Assignments are
order-independent, so making the result positionally significant would have been
the one line whose placement changed the meaning — moving it would silently
alter the function while moving anything else did nothing. Convention puts it
last; the checker only requires that there is one.

### Assignments

Order-independent, and they may reference each other:

```
function f(a, b) {
    x := (a + b + d) / 2      # d is defined below
    d := a * 2
    return x
}
```

Names resolve across the whole body, exactly as node declarations resolve across
a program. **Cycles are a check-time error**, detected syntactically over the
dependency graph — including bindings nothing reads:

```
function f(a) {
    x := y
    y := x        # error, even though neither is ever demanded
    return a
}
```

It has to be the static pass. Demand-driven evaluation would never touch a
binding nothing needs, so a dead cycle would sit undetected until some later
edit made it reachable — and then it would be a per-row infinite loop with
nowhere to report itself. `check_circuit_cycles` in `typecheck/mod.rs` is the
existing model.

Cycles between *nodes* are a different matter: they are meaningful, and they are
what `fixpoint` is for. There is no fixpoint semantics for a scalar binding.

Dead bindings that are not cyclic are fine and are never evaluated. An emitter
produces them routinely.

### Evaluation is demand-driven

A binding is evaluated when something needs it, at most once, through a
maintained slot table:

```
function f(r) {
    d := expensive(r.x)
    return match(r.k, 0 -> 0, _ -> d)
}
```

`expensive` does not run when `r.k` is 0. Eager evaluation would be *safe* —
everything is total — but it would compute every binding on every row, and an
emitted body with twenty bindings would pay for all twenty to reach three.

### Parameters are untyped

A function is a **template**. It is type-checked once per invocation, against
the types at that call site, and the same body may be used at different types:

```
function to_int(v) {
    return match(v, x::f64 -> x, _ -> 0)
}

a := map(nums, to_int)      # v : f64
b := map(docs, to_int)      # v : json
```

Two consequences to take deliberately:

- **A function nobody calls is never checked.** This is the C++/Zig template
  bargain.
- **Errors in a body are caused by a call site.** A diagnostic pointing only at
  the body is not actionable, so per-instantiation errors need a span *chain* —
  "in `to_int`, instantiated at 12:5". `Diagnostic` holds a single
  `Option<Span>` today (`diag.rs`), so this is a change to the diagnostic model
  and to every fixture asserting `line`/`column`.

A body may carry typespecs for its own bindings (`x :: i64`), which are checked
rather than used to drive inference — the same rule node typespecs follow.

### Recursion is prohibited

```
function f(a) { return match(a, 0 -> 0, _ -> f(a - 1)) }     # error
```

Non-terminating at runtime, and under per-invocation instantiation it does not
terminate at *compile* time either. Mutual recursion is the same and needs a
call-graph acyclicity check. Recursive computation is what `fixpoint` is for.

Because the call graph is therefore a DAG, **functions are fully inlined at
check time** — the same thing `circuit` instantiation already does, and with the
same consequence: a function has no runtime form. What reaches `lower.rs` is one
`TypedExpr` tree per operator, with one flat slot table rather than a call
stack, and `PlanOp`'s structural deduplication keeps working unchanged because
it compares the fully-instantiated tree.

Instantiation needs a memo keyed by `(function, argument types)`, or a diamond
in the call graph instantiates exponentially.

### Named parameters

Named parameters use the same convention as `circuit`: the caller's name, then
the body's.

```
function helper(a, extra: e) { return a + e }
function top(r)              { return helper(r.x, extra: 2) }

o := map(s, top)              # fine — top has arity 1, all positional
o := map(s, helper)           # error — helper has a named parameter
```

Operators supply their arguments positionally and expect a fixed arity, so a
function passed directly to one must be all-positional and correctly sized.
**There is no partial application.** Named parameters are a call-site
readability feature with no semantic weight, available to functions that are
only ever called from other functions.

### Scoping

Body bindings are function-scoped and order-independent. Pattern bindings are
arm-scoped and exist only under their arm:

```
function f(v) {
    d := 10
    return match(v,
        x::f64 -> x + d,      # x here only
        _      -> d)          # x not in scope
}
```

**Shadowing is prohibited.** A pattern binding may not reuse a body binding's
name — shadowing inside a scope whose declarations have no order is genuinely
ambiguous to read.

## `match`

Sequential, first match wins, and every `match` must be exhaustive.

```
match_expr := "match" "(" scrutinee "," arm ("," arm)* ")"
scrutinee  := expr | "(" expr ("," expr)+ ")"
arm        := pattern "->" expr
```

The scrutinee may be a **list** of expressions, matched against a
correspondingly wide pattern:

```
match((a, b),
    (NONE,   NONE)   -> 0,
    (x::f64, NONE)   -> x,
    (NONE,   y::f64) -> y,
    (x::f64, y::f64) -> x + y)
```

That list is *not* a tuple value — there is no tuple type and nothing is
constructed. `match((a), …)` is illegal; a single scrutinee is written
`match(a, …)`.

`return` is a statement and arms are expressions, so `return` cannot appear
inside an arm:

```
match(x, 0 -> return 1, _ -> return 2)      # error
return match(x, 0 -> 1, _ -> 2)            # the form that works
```

### Patterns

| form | binds | notes |
|---|---|---|
| `NONE`, `null`, `0`, `""`, `true` | — | value patterns |
| `x::f64`, `x::string`, `x::bool` | the value | type patterns |
| `record(id: i64, name: string, **)` | a constructed record | `**` means "and other fields" |
| `dict(string, json)`, `array(json)` | the value | shape tests |
| `{key: pattern, **}` | nested bindings | structural, `json` only |
| `[first, *]` and friends | elements | structural, `json` only |
| `_` | — | wildcard |

**Type patterns are the only conversion mechanism in the language.** There is no
`cast`. `x::f64` against an `optional(f64)` unwraps it, and against a `json`
converts it — so the same body handles both, with the `NONE` arm simply dead in
the first case:

```
match(v, NONE -> 0, x::f64 -> x)
```

There is no conversion between two *known* types. `floor`/`ceil`/`round` are
type-preserving, and nothing turns an `f64` into an `i64`. See
[`json.md`](json.md) for what that means for numbers coming out of documents.

#### Open records

`record(id: i64, name: string, **)` matches a record or document that has *at
least* those fields, and binds a **closed** record of exactly the named ones —
the extras are tested against nothing and discarded, which is visible in the
bound variable's type.

**`**` is pattern-only. It is never a type.** An open record type would break
the invariant that makes the value model work: [`mapping.md`](mapping.md) states
that field names live only in `TypeDesc` and never in the value, so a value
carrying fields the schema does not name would either need names back in every
row or would silently lose them.

Because `record` patterns extract and convert rather than merely test, they are
**parses**: each field is a lookup, a conversion and an allocation, failure is
all-or-nothing, and a pattern that fails on its last field has already paid for
the earlier ones. **Arm order is a performance decision as well as a semantic
one.**

#### Structural patterns

`{…}` and `[…]` patterns are for `json` scrutinees only, and their nested
positions hold further structural patterns — **not** type ascriptions:

```
{user: {id: x, **}, **}          # legal — x : json
{user: {id: x::f64, **}, **}     # illegal — no type ascriptions inside
```

Navigation and conversion never happen in one pattern. They compose through a
nested `match`, which is more verbose and keeps each construct doing one thing:

```
match(doc,
    {user: {id: x, **}, **} -> match(x, n::f64 -> n, _ -> 0),
    _ -> 0)
```

Array patterns allow **at most one `*`**, in any position:

```
[first, *]           # at least one
[*, last]            # at least one
[first, *, last]     # at least two — element 0 and element n−1
[a, b]               # exactly two
[a, b, *]            # at least two
```

There is no `*name` or `**name`. A binding for the *rest* of a container is the
only pattern form that would force a copy — a middle run of elements has no byte
range that is itself a container, so binding one means building a new document,
O(n) per row per arm tried. Dropping it keeps every pattern's cost proportional
to what was written. The cost is that object-spread in a source language has no
lowering.

#### Binding copies, and matching is two-phase

A binding **copies**. Views into the source document would be cheaper — an `Arc`
clone and a range — but a small field extracted from a large payload would then
retain the whole payload for as long as the derived Z-set lives, invisibly.
Lazy materialization with escape analysis is future work.

Because bindings cost something, matching runs in two phases: **test the whole
pattern, then materialize bindings only on success.** Otherwise a pattern that
binds a large sub-document and then fails on a later position pays for a copy it
throws away, once per row per arm.

### Arm unification

The type of a `match` is its arms' types unified. This is the existing `unify`
(`typecheck/infer.rs`) folded across the arms — the machinery is already there,
including the part that matters most: `Ty::None` is the polymorphic `NONE`, and
`unify(Known(t), Ty::None)` already yields `optional(t)`.

```
match(x, n::f64 -> n, _ -> NONE)        # optional(f64)
```

Where the fold lands on `Ty::None` — every arm is `NONE` — the type is genuinely
ambiguous and needs a typespec. Since `return match(…)` has nowhere to put one,
that means binding it to a name:

```
r :: optional(f64)
r := match(x, _ -> NONE)
return r
```

**Arm unification does not promote numerics**, unlike arithmetic. So the two
rules disagree, visibly, a few characters apart:

```
n + f                                     # i64 + f64 → f64, promotes
match(x, n::i64 -> n, f::f64 -> f, …)     # i64 vs f64 → error
```

That is deliberate: one is a computation whose result is a number, the other a
choice between alternatives that must agree on what they are. `unify` currently
serves arithmetic, comparison and `coalesce`, so it has to split, and each
existing call site needs checking for which rule it wanted.

### Exhaustiveness and dead arms

A trailing `_` establishes exhaustiveness. (Whether full enumeration of a finite
shape set also counts is [open](#open-questions).)

**Dead arms are allowed.** They are the point of templates — the same body is
exhaustive at one call site and not at another, and an arm that cannot match at
one instantiation is live at the next:

```
function norm(v) {
    return match(v,
        NONE      -> 0,
        x::f64    -> x,
        s::string -> length(s),
        _         -> 0)
}
```

At `v : f64` the `string` and `NONE` arms are dead; at `v : json` they are live.
Making that an error would make the template useless, so unreachable arms are
not diagnosed — with one exception.

**Notes are emitted for patterns unsatisfiable against a `json` scrutinee**, and
only there. That is the case where the author's mental model is most likely
wrong, because what a `json` can yield is deliberately narrower than the type
vocabulary:

```
match(doc, n::i64 -> n, _ -> 0)               # note: json numbers are f64
match(doc, r::record(id: i64, **) -> …, …)    # note: on the `id` field
```

The note recurses into record patterns and points at the *field*, since writing
`id: i64` out of habit is the likeliest mistake in the whole feature and a
head-only note would miss it. Notes are deduplicated by pattern span, so a
helper used at six call sites reports once.

Notes need plumbing that does not exist: `compile()` returns
`Result<Plan, Vec<Diagnostic>>`, so a diagnostic on a *successful* compile has
nowhere to go, and nothing in `src/` produces a `Severity::Note` today.

## Pairs become records

All three pair-producing operators take a record instead:

```
map_index(s,       function((r)       -> record(key: r.dept_id, value: r)))
join_index(l, r,   function((k, a, b) -> record(key: …, value: …)))
flat_map_index(s,  function((r)       -> [record(key: …, value: …), …]))
```

and `[a, b]` becomes an ordinary **array constructor**. Those two changes are
one change: once `[…]` builds a value, its elements must *be* values, and
`(k, v)` pairs explicitly are not.

The payoff is that `flat_map`'s function now returns an `array(T)`, so it emits
one row per element and **fan-out becomes data-dependent** — which
[`overview.md`](overview.md) currently lists as blocked on a runtime list value.
Existing `flat_map` fixtures keep working: `[r.a, r.b]` builds a 2-array and
produces two rows, exactly as the fan-out reading did.

After this, `(…)` in expression position means **only grouping**. There are no
pairs and no list-syntax left in the language, so `language.md`'s "Pairs and
lists are syntax, not values" section can be deleted rather than amended — the
value model gets simpler, paid for by touching every `map_index`, `join_index`
and `flat_map_index` body in the fixtures and docs.

## Reserved words

`fun` is removed. `function`, `match` and `return` are added to `KEYWORDS`;
`dict` and `array` join `TYPE_NAMES` (`lang.rs`).

## What this costs

- `PlanOp::FlatMap` and `FlatMapIndex` change from `Vec<Arc<TypedExpr>>` — one
  expression per output row, fixed at plan time — to a single expression.
- `TypedExpr` gains a `Match` node with binding slots, and `ExprKind::List`
  becomes a real array constructor rather than fan-out syntax.
- `Diagnostic` needs a span chain, which affects every fixture asserting
  `line`/`column`.
- `compile()`'s signature has to carry diagnostics out of a successful compile,
  which touches `lib.rs`, `lower.rs`, `tests/plan.rs`, `tests/reserved.rs` and
  `tests/yaml.rs`.
- The YAML harness enforces **exactly one** of `expected_output` /
  `expected_exact_output` / `expected_diagnostics` (`tests/yaml.rs`), so a case
  that compiles *and* emits a note cannot assert both. That rule has to relax,
  and `tests/cases/README.md` documents it.
- 145 `fun((` sites across 12 fixture files and 18 in the docs — a pure rename
  to `function((`. The `map_index`/`join_index`/`flat_map_index` body change is
  wider and not mechanical.

## Open questions

1. **Do statically-dead arms contribute their type to arm unification?**
   Excluding them is what makes templates work — otherwise `norm` above fails at
   every instantiation rather than none. But then an expression's type depends
   on which arms are statically reachable, so adding a field to a record can
   bring a dead arm to life and change a function's return type at that call
   site.
2. **Are `key` and `value` magic field names?** The pair-to-record change gives
   the operators a shape to recognise, and the language has no other place where
   a specific field name carries meaning — `language.md` says field names are
   their own namespace and unrestricted. Does `record(k: …, v: …)` fail, or do
   the operators take the first two fields positionally?
3. **Exhaustiveness beyond the trailing wildcard.** A trailing `_` is
   sufficient. Whether full enumeration of a finite shape set also counts is
   undecided — without it, `match(v, NONE -> 0, x::f64 -> x)` needs an
   unreachable `_` even though it is total.
