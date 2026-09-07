# DBSP Runner — Language

The source language is declarative: a program is a flat list of declarations
that name streams and describe how they are derived from inputs. The grammar is
borrowed from grasp-dbsp; the operators and types it refers to come from
`dbsp`.

This describes the language as implemented. [`json.md`](json.md) holds a
designed but unbuilt `json` type, and with it `match` and the pattern language
that conversion needs.

The language is a **compilation target**. It is written by a compiler frontend
or by an agent, not by hand, so it does not have to be convenient — but it does
have to be explicit and uniform. Where that principle decides something, the
text says so.

## Program structure

A program is a sequence of declarations. Declarations may appear in any order;
names are resolved after the whole program is parsed (forward references are
allowed).

```
program      := declaration*
declaration  := node_def | typespec | function_def | circuit_def | comment

node_def     := NAME ":=" op_call
typespec     := NAME "::" batch_type
comment      := "#" [^\n]*

op_call      := OP "(" [arg ("," arg)*] ")"
arg          := NAME                    # a stream, or a named function
              | op_call                 # a nested operator, except `input`
              | STRING                  # a table name, for `input`
              | AGGREGATOR              # min | max | count | sum | avg
              | anon_function
```

`OP` is one of the operators listed under [Operators](#operators); `AGGREGATOR`
is accepted only as the second argument of `aggregate`, so a bare name is never
ambiguous with a stream reference.

Node definitions assign a name to a derived stream. A `typespec` attaches a
type to a name. It is required in three places — an `input` node, a standalone
`empty()`, and a recursive `fixpoint` stream whose type inference cannot reach —
and optional but checked everywhere else.

**A typespec is always checked, never used to drive inference.** Inference is a
convenience; an emitter always knows the type it intends and can write it down.
So an ascription is the answer wherever inference falls short, and inference is
never made cleverer to avoid one.

A program declares streams only. **It does not say which of them are observed** —
the set of output nodes is supplied when the runner starts, by node name. See
[`mapping.md`](mapping.md) for how outputs are exposed.

## Types

Types describe both the *shape of the stream's batches* and the *values inside
them*. There are two layers.

### Batch types

A stream carries one batch per clock cycle, and the batch type is part of the
stream's type.

```
batch_type := "zset" "(" value_type ")"
            | "indexed_zset" "(" value_type "," value_type ")"
```

- `zset(T)` — a flat weighted set of `T`.
- `indexed_zset(K, V)` — a keyed set: each key `K` maps to a weighted set of
  values `V`.

These name the *shape*, which is all the language distinguishes. `dbsp` has
several storage representations of identical Z-set semantics — `Ord`, `Vec`,
`File`, `Fallback` — and choosing among them is its concern, not the
language's. [`mapping.md`](mapping.md) records which Rust types these become.

### Value types

```
value_type := scalar | record_type | array_type

scalar      := bool | i64 | f64 | String | optional "(" value_type ")"

record_type := "record" "(" field ("," field)* ")"
field       := FIELD_NAME ":" value_type

array_type  := "array" "(" value_type ")"
```

That is the whole vocabulary. It is deliberately narrow: the other integer
widths, `f32`, and the Feldera `sql.*` types are listed under future work in
[`overview.md`](overview.md).

**There is exactly one way to write each type.** An earlier cut had a second
string type, `sql.SqlString`, alongside `String`; it was withdrawn. Two
spellings of one thing is a choice an emitter must make with no information, and
these two were not even disjoint in practice — every string builtin returned one
of them whatever it was given. The `sql` namespace stays reserved, for types
that would be genuinely distinct.

**Absence is `optional(T)`.** There is no separate nullable flag, and an
`optional` never wraps another.

`record(f: T, …)` is a named-field record. Field names are bare identifiers;
quote a name that is not a valid identifier (`record("total count": i64)`).

Note that `record` is spelled the same way in type position and in expression
position — the type names the fields, and the literal fills them:

```
r :: zset(record(id: i64, name: String))                        # the type
map(s, function((row) -> record(id: row.id, name: row.name)))   # a value
```

`array(T)` is a sequence of one element type, and an ordinary value: it can sit
in a record, in a column and in a stream, and `flat_map` turns one into rows.

### Typing rules

- An **`input` node requires a `typespec`**, always `zset(record(...))`
  (a table is a set of named-field rows). The record's field order is the
  deterministic column order used by the JSON codec.
- **All other nodes are inferred** from the operators applied to them. An
  explicit `typespec` elsewhere is checked, not used to drive inference.
- **Scalar value types are allowed** anywhere a `value_type` is expected
  (for example `zset(i64)` for a projected single column); only the
  `input` node is restricted to `record`.

## Operators

Operators are `dbsp` primitives exposed as-is. The reference below gives each
operator's input batch shape(s) and its result batch shape; `X` means the shape
is preserved. Which `dbsp` method each lowers to is in
[`mapping.md`](mapping.md), which is where that mapping is maintained.

| operator | signature |
|---|---|
| `input("t")` | → `zset(T)` |
| `map(s, f)` | `zset(T) → zset(U)`, `f : T → U` |
| `filter(s, f)` | `X → X`, `f : T → bool` |
| `flat_map(s, f)` | `zset(T) → zset(U)`, `f : T → array(U)` |
| `map_index(s, f)` | `zset(T) → indexed_zset(K,V)`, `f : T → record(key: K, value: V)` |
| `flat_map_index(s, f)` | `zset(T) → indexed_zset(K,V)`, `f : T → array(record(key: K, value: V))` |
| `join(l, r, f)` | `indexed_zset(K,V₁) × indexed_zset(K,V₂) → zset(OV)`, `f : (K,V₁,V₂) → OV` |
| `join_index(l, r, f)` | as `join`, but `f : (K,V₁,V₂) → record(key: OK, value: OV)` → `indexed_zset(OK,OV)` |
| `antijoin(l, r)` | `indexed_zset(K,V) × indexed_zset(K,V₂) → indexed_zset(K,V)` |
| `distinct(s)` | `X → X` (deduplicated) |
| `aggregate(s, agg, f)` | `indexed_zset(K,V) → indexed_zset(K,A)`, `f : V → A` — see [Aggregators](#aggregators) |
| `weighted_count(s)` | `zset(T) → indexed_zset(T, i64)` |
| `neg(s)` | `X → X` |
| `plus(a, b)` | `X × X → X` |
| `minus(a, b)` | `X × X → X` |
| `sum(a, b, …)` | `X⁺ → X` |
| `integrate(s)` | `X → X` (running sum) |
| `differentiate(s)` | `X → X` |
| `delay(s)` | `X → X` |

Operator arity follows `dbsp`: `plus` and `minus` are binary, `sum` is n-ary.

**`filter`'s function takes the stream's element.** For a flat stream that is
one row; for an indexed stream it is the `(key, value)` pair, so the function
takes two parameters — `function((k, v) -> …)` — matching `dbsp`'s `ItemRef` for
each shape.

**Operator calls nest.** A stream argument may be another operator call rather
than a name, so `weighted_count(map(emp, f))` is one declaration. A named node
then exists because it is worth naming — as an output, or as an intermediate
used more than once — rather than because the grammar insists.

**`input` is the one exception.** Its schema comes from a `::` typespec, which
needs a name to attach to, so `map(input("emp"), f)` is rejected. Bind it first.

**Nodes are content-addressed.** Identical operator, identical inputs and
identical parameters means one node, however many times it is written. So
`plus(filter(a,f), filter(a,f))` builds one filter — and still doubles the
weights, because `plus` adds that stream to itself. Deduplication changes which
nodes are built, never what comes out.

It applies to `input` too: two declarations of one table are one node, and so
one input handle.

A nested node has no name, so it cannot be selected as an output. It is still a
node, and diagnostics about it are labelled by operator and position —
`filter@3:12`.

`join`, `join_index` and `antijoin` require equal key type `K` on both sides;
`plus`/`minus`/`sum` require identical batch types.

**`plus` and `sum` add weights — they are bag union, not set union.** Two streams
that each contain a row with weight 1 produce that row with weight 2. Follow them
with `distinct` for set semantics.

There is no `output` operator. Outputs are named when the runner starts.

## Aggregators

`aggregate(s, agg, f)` takes an aggregator name and a projection `f : V → A`
applied to each value in the group. Aggregators are bare names:

```
aggregate(idx, max, function((v) -> v.salary))
```

| aggregator | result | lowering |
|---|---|---|
| `min` / `max` | `A` — the minimum/maximum projected value in the group | `map_index` to re-project the value, then `aggregate(Min)` / `aggregate(Max)` |
| `sum` | the sum of the projected values | `aggregate_linear_postprocess` |
| `avg` | their mean | `aggregate_linear_postprocess` |
| `count` | the number of rows whose projected value is non-null | `aggregate_linear_postprocess` |

`sum` yields the projection's own type; `avg` always yields `f64`, so averaging
integers does not truncate. Both yield null for a group in which every
projection was null.

Two consequences worth stating explicitly:

- **`min` and `max` compare with the runtime value type's ordering.** That
  ordering is therefore load-bearing for query results, not merely for batch
  layout — see the invariants section of [`mapping.md`](mapping.md).
- **`sum`, `avg` and `count` are linear aggregates**, and a linear aggregate
  cannot distinguish "the group summed to zero" from "the group is empty". They
  carry an extra row counter for that reason; see [`mapping.md`](mapping.md).

To count rows regardless of nullability, use the `weighted_count` operator
rather than the `count` aggregator — it sums Z-weights directly and is exact.

## Functions and expressions

Operators that transform rows (`map`, `filter`, `flat_map`, `map_index`,
`flat_map_index`, `join`, `join_index`, `aggregate`) take a function argument:

```
function_def  := "function" NAME "(" [params] ")" "{" "return" expr "}"
anon_function := "function" "(" "(" [params] ")" "->" expr ")"
params        := NAME ("," NAME)*

expr       := literal
            | NAME                                        # a bound parameter
            | expr "." FIELD_NAME                         # record field
            | "record" "(" FIELD_NAME ":" expr ("," FIELD_NAME ":" expr)* ")"
            | "[" expr ("," expr)* "]"                    # an array
            | "(" expr ")"                                # grouping, only
            | unop expr
            | expr binop expr
            | NAME "(" [expr ("," expr)*] ")"             # a builtin, or a named function

unop       := "-" | "not"
binop      := "+" | "-" | "*" | "/" | "%"
            | "==" | "!=" | "<" | "<=" | ">" | ">="
            | "and" | "or"
literal    := INT | FLOAT | STRING | "true" | "false" | "NONE"
```

The parameter list binds the row(s) the operator feeds the function. `map`,
`filter`, `flat_map`, `map_index` and `flat_map_index` take one row; `join` and
`join_index` take three (key, left value, right value); `aggregate`'s function
takes one value.

```
function((row) -> row)
function((row) -> row.id)
function((row) -> record(id: row.id, name: row.name))
function((row) -> record(key: row.dept_id, value: record(id: row.id)))
function((row) -> row.salary > 100000 and row.dept_id == 3)
function((k, e, d) -> record(name: e.name, dname: d.dname))
```

Duplicate field names within one `record(...)` literal are a parse error.

### Named functions are templates

A `function` declaration names a body that can be called from an expression or
passed straight to an operator:

```
function scale(x)    { return x * 2 + 1 }
function positive(r) { return r.v > 0 }

kept   := filter(a, positive)
scaled := map(a, function((r) -> scale(r.v)))
```

**Parameters carry no types.** The body is checked afresh against the types at
each call site, so one definition serves every type it happens to work at:

```
ints   := map(a, function((r) -> scale(r.i)))   # i64 arithmetic
floats := map(a, function((r) -> scale(r.f)))   # f64 arithmetic
```

That works because the literals inside `scale` resolve against the parameter
type like any other operand — see [Numbers](#numbers). It is the reason a
function is a *macro* rather than a value.

Three consequences, taken deliberately:

- **A function nobody calls is never checked.** This is the C++/Zig template
  bargain.
- **An error in a body is caused by a call site**, so the diagnostic names it —
  `in `scale`, instantiated at 12:5: …` — while the span still points at the
  offending expression inside the body.
- **Recursion is rejected**, directly or mutually. A function is fully inlined
  at check time, so a cycle would not terminate while *compiling*. `fixpoint`
  is what recursion is for.

A function has no runtime form: what reaches the lowering is one expression tree
per operator. Inlining is substitution, so an argument used twice in a body is
evaluated twice; sharing it needs body bindings and a slot table, which are
future work.

A name means one thing — a function and a node may not share one.

### `key` and `value`

`map_index`, `join_index` and `flat_map_index` produce a keyed stream, so their
function returns a two-field record naming the halves:

```
map_index(s, function((r) -> record(key: r.dept_id, value: r)))
```

**These are the one place a field *name* carries meaning.** Everywhere else
field names are their own unrestricted namespace. Two things follow:

- The record has exactly `key` and `value`. A third field is an error, because
  there is nowhere for it to go.
- **Their order is not significant.** They are matched by name, so
  `record(value: …, key: …)` means the same thing — unlike every other record,
  where the literal's order defines the type. The match happens at check time,
  so it costs nothing at runtime.

An earlier cut wrote these as `(key, value)` pairs: a construct legal only in
those three positions, destructured by the type checker rather than being a
value. `[…]` was the same — fan-out syntax rather than an array. Both are gone,
because **every construct in this language is a value in every position**. A
construct legal only in certain syntactic slots is one an emitter cannot
compose; it would have to know where it is before knowing what it may write.

The payoff shows in `flat_map`, whose function returns an `array(T)`: it emits
one row per element, so **fan-out follows the data** rather than the source text.

```
posts :: zset(record(id: i64, tags: array(String)))
tags  := flat_map(posts, function((r) -> r.tags))
```

### Builtins

Free functions callable from any expression. They compose with the operators
above and with each other.

| builtin | signature | result |
|---|---|---|
| `coalesce` | `optional(T) × T → T` | `x` if present, else `y` |
| `abs` / `floor` / `ceil` / `round` | `T → T`, `T` numeric | type-preserving |
| `length` | `String → i64`, `array(T) → i64` | element or character count |
| `concat` | `String × String → String` | concatenation |
| `lower` / `upper` / `trim` | `String → String` | |

Every builtin but `coalesce` rejects an `optional` argument, for the reason
under [Absence](#absence). `coalesce` is the one that inspects absence rather
than being rejected for it.

There is no `+` on strings. `concat` is the one way to join them, and `+` is
arithmetic only.

### Numbers

**A numeric literal has no type of its own; it takes one from the operand
beside it.** An integer literal inhabits any numeric type, a float literal any
floating one. Standing alone, they settle to `i64` and `f64`.

```
r.i * 2       # i64, and `2` is an i64
r.f * 2       # f64, and the same `2` is an f64
2             # i64, since nothing else decides
```

This is the same mechanism `NONE` and `empty()` already use — a construct with
no type of its own, resolved from context — and it is what makes a function
usable at several numeric types without being written twice.

**There is no implicit conversion.** Arithmetic and comparison need operands of
one type; `i64` and `f64` do not meet:

```
r.i + r.f     # error: no implicit conversion
r.i + 1.5     # error: a float literal has no `i64` value
```

That is deliberate rather than austere. Implicit promotion would need a lattice
that gets worse with every numeric type added, and an emitter would have to
model it exactly to predict the type of its own output. One rule — operands
match, literals adapt — is computable in one bottom-up pass, by the emitter as
well as by the checker.

**Integer arithmetic wraps.** Overflow in `+`, `-`, `*`, unary `-` and `abs`
wraps rather than trapping or vanishing, so the result stays a definite value of
the type its column declares. Float arithmetic is IEEE.

**Division is the one arithmetic that can be absent.** A zero divisor has no
value to return, so `/` and `%` have type `optional(T)` at every numeric type:

```
r.a / r.b                    # optional(i64)
coalesce(r.a / r.b, 0)       # i64
```

An operation whose result may be missing says so in its type. That is the rule
the whole value model rests on — see [Absence](#absence) — and it is why these
two are typed differently from the rest.

### Absence

A value that may be missing has type `optional(T)`, and the literal for its
absence is `NONE`.

**`NONE` is a value, not SQL's `NULL`.** SQL propagates `NULL` because it
means *unknown*; `NONE` means *this field has no value*, so it behaves the way
Rust's `None` does:

| expression | result |
|---|---|
| `r.x == NONE` | `bool` — always decides |
| `r.a == r.b`, both none | `true` |
| `r.x > 5`, `r.x` none | `false` — `NONE` sorts before every value |
| `r.x + 1` | a type error — write `coalesce(r.x, 0) + 1` |

Every comparison yields a plain `bool`, so `filter` accepts one directly.
Arithmetic, `and`, `or` and `not` reject an `optional(T)` operand rather than
returning absence, because returning it would be propagation under another name;
`coalesce` supplies a definite value first.

`NONE` sorting first is the same order `min` and `max` use, so expressions and
aggregates agree about where absence sits.

**A declared type is a promise the runtime cannot break.** Nothing produces a
`NONE` in a column whose type forbids one: that is why `/` is typed
`optional`, why integer overflow wraps rather than vanishing, and why the string
builtins reject a non-string rather than returning absence for it. The JSON
codec enforces the same rule from the other side — it refuses to write a `null`
into a column that is not optional, which is exactly what it would refuse to
read back.

**`null` is not the absence literal.** It is reserved for the JSON null *value*
inside a `json` document — a distinct thing, once [`json.md`](json.md) lands —
so that JSON pasted into source keeps its meaning. On the wire it is unchanged:
a JSON `null` in a data position still decodes to absence for an `optional(T)`
column, and absence still encodes as JSON `null`.

## Circuits

A `circuit` is a named, parameterised block of declarations. It is expanded at
the call site: the body becomes ordinary nodes, reachable as
`<instance>.<node>`.

```
circuit normalize(src: s) {
    big     := filter(s, function((r) -> r.v > 1))
    doubled := map(big, function((r) -> record(v: r.v * 2)))
}

n   := normalize(src: a)
out := n.doubled
```

`label: internal` maps the keyword the caller uses to the name the body uses —
here the caller passes `src:` and the body refers to `s`. Every parameter must
be supplied, each exactly once, and the body may not define another circuit.

`out := n.doubled` is an **alias**: a right-hand side that is just a reference
adds no node, it only gives an existing one another name. It is a convenience,
not a requirement — a body node is registered as `<instance>.<node>` and can be
selected as an output by that dotted path directly.

A circuit body may instantiate another circuit, and its nodes are reached by a
longer path — `t.second.out` for the node `out` of the instance `second` inside
the instance `t`. Inside a body, a shorter path resolves against the enclosing
instance first, so `second.out` works there.

Definition-level cycles are rejected: expansion is inlining, so a circuit that
instantiates itself, directly or through others, would never finish. `fixpoint`
is the only recursion.

Expansion is per instantiation, but content addressing then merges whatever
turns out identical — two instantiations with the same arguments collapse
entirely.

### `fixpoint`

`fixpoint` iterates a circuit body to convergence. A parameter is
**self-referential** when a body node shares its label; inside the body its
internal name is the *previous round's* value.

```
circuit tc(base: b, fwd: f, path: p) {
    step := join_index(p, f, function((k, a, e) -> record(key: e.dst, value: record(src: a.src))))
    path := plus(b, step)
}

fp      := fixpoint(tc(base: base, fwd: fwd, path: empty()))
closure := fp.path
```

- **A recursive stream starts empty**, so the call site passes `empty()`. There
  is no seeding: the base case belongs in the body, as `plus(b, step)` here.
- **A recursive body node's type is usually inferred**, so the typespec above is
  optional. `path := plus(b, step)` works because `plus` *equates* its operands'
  types: `path` has `b`'s type whatever `step` turns out to be, even though
  `step` consumes `path`.

  Inference follows the operators whose result type is one of their operands —
  `plus`, `minus`, `sum`, and the shape-preserving `distinct`, `neg`, `filter`,
  `integrate`, `differentiate`, `delay` — through any nesting. It cannot follow
  `map`, `join` or `aggregate`, whose result type comes from a function applied
  to the very value type being solved for; a recursion defined only that way
  needs a typespec. A typespec, when given, is checked against the inferred
  type rather than overriding it.
- **Only recursive members leave the fixpoint.** `fp.path` works; other body
  nodes exist only inside the nested circuit.
- **`distinct` is applied for you** to each recursive stream, on every round.
  That is what makes the iteration terminate, and writing your own would lower
  a redundant second one.
- Every recursive stream in one `fixpoint` must have the same shape, since one
  nested batch type serves them all.

## `empty()`

`empty()` is a stream with no rows. It carries no type of its own — it takes one
from where it sits:

- beside a typed operand of `plus`, `minus` or `sum`, which require identical
  batch types anyway;
- as a circuit argument, from the body node the parameter feeds;
- on its own, from its `::` typespec.

Anywhere else it is an error saying so, rather than guessing.

## Reserved words

These may not name a node, a function or a parameter: the 20 operator names,
the 5 aggregator names, the builtin names, the type constructors (`bool`,
`i64`, `f64`, `String`, `optional`, `record`, `array`, `sql`, `zset`,
`indexed_zset`), and `true`, `false`, `NONE`, `null`, `function`, `return`,
`and`, `or`, `not`, `if`, `then`, `else`, `circuit`, `fixpoint`.

`sql` is reserved although the namespace is empty, so the Feldera value types
can arrive later without breaking a program that used the name meanwhile.

`if`, `then` and `else` are reserved although there are no conditionals yet, so
adding them later will not break existing programs. Record *field* names are
unrestricted — they are their own namespace and can be quoted.

## Example

```
emp := input("emp")
emp :: zset(record(id: i64, name: String, dept_id: i64, salary: i64))

dept := input("dept")
dept :: zset(record(id: i64, dname: String))

high_paid := filter(emp, function((row) -> row.salary > 100000))

emp_idx  := map_index(emp,  function((row) ->
                record(key: row.dept_id, value: record(id: row.id, name: row.name, salary: row.salary))))
dept_idx := map_index(dept, function((row) -> record(key: row.id, value: record(dname: row.dname))))

joined   := join(emp_idx, dept_idx, function((k, e, d) -> record(name: e.name, dname: d.dname)))

by_dept  := aggregate(emp_idx, max, function((v) -> v.salary))
```

Both sides of the `join` are keyed by `i64`, so the key types are equal and the
join typechecks. Running this program with `joined` and `by_dept` named as
outputs emits their deltas; `high_paid` is still constructed, but not observed.

A node may also be named by its **content id** — a hash of the computation it
performs, described in [`mapping.md`](mapping.md). That is what makes a nested
node observable, since it has no declared name of its own.
