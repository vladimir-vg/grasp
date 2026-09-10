# grasp-dbsp — Language

grasp-dbsp is declarative: a program is a flat list of declarations that name
streams and describe how they are derived from inputs. The grammar is borrowed
from the Erlang project of the same name — see
[`overview.md`](overview.md#relationship-to-other-projects) — while the
operators and types it refers to come from `dbsp`.

This describes the language as implemented.

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
              | op_call                 # a nested operator, except `input` and `constant`
              | STRING                  # a table name, for `input`
              | AGGREGATOR              # min | max | count | sum | avg
              | anon_function
              | array_literal           # the rows, for `constant`
```

`OP` is one of the operators listed under [Operators](#operators); `AGGREGATOR`
is accepted only as the second argument of `aggregate`, so a bare name is never
ambiguous with a stream reference.

Node definitions assign a name to a derived stream. A `typespec` attaches a
type to a name. It is required in four places — an `input` node, a `constant()`
node, a standalone `empty()`, and a recursive `fixpoint` stream whose type
inference cannot reach — and optional but checked everywhere else.

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
value_type := scalar | record_type | array_type | dict_type | "json"

scalar      := bool | i64 | f64 | string | temporal
                 | optional "(" value_type ")"
temporal    := date | time | timestamp | interval

record_type := "record" "(" [field ("," field)*] ")"
field       := FIELD_NAME ":" value_type

array_type  := "array" "(" value_type ")"
dict_type   := "dict" "(" key_type "," value_type ")"
key_type    := "bool" | "i64" | "f64" | "string"
                 | "date" | "time" | "timestamp" | "interval"
```

That is the whole vocabulary. It is deliberately narrow: the other integer
widths, `f32`, and the Feldera `sql.*` types are listed under future work in
[`overview.md`](overview.md).

**There is exactly one way to write each type.** An earlier cut had a second
string type, `sql.SqlString`, alongside `string`; it was withdrawn. Two
spellings of one thing is a choice an emitter must make with no information, and
these two were not even disjoint in practice — every string builtin returned one
of them whatever it was given. The `sql` namespace stays reserved, for types
that would be genuinely distinct.

**Absence is `optional(T)`.** There is no separate nullable flag, and an
`optional` never wraps another.

**The temporal types are `date`, `time` and `timestamp`**, and they are scalars:
each orders as the instant it names, so `<` means what a reader expects, `min`
and `max` fold them, and each may key a dict. A `timestamp` is **always UTC** —
there is no `timestamp_with_timezone`, which needs IANA tzdata and DST
semantics and is under future work in [`overview.md`](overview.md).

Each travels as a **string**, in the one spelling its own parser reads back —
`2024-01-15`, `14:30:00`, `2024-01-15 14:30:00`, with `.ffffff` where there is a
fraction. That single round trip is what the JSON codec, a dict key and
`cast(s, optional(date))` all use, so none of them can spell a value a different
way from the others. All three are **microsecond precision**: `time` counts
nanoseconds underneath, and a finer fraction is truncated where text enters
rather than kept and then lost on the way out.

**`interval` is a span of time, in microseconds.** One integer, because with no
timezone a day is exactly 86400 seconds and every unit below a month converts
into microseconds exactly. Months are the one quantity that does not, and they
are **not in this type**: a `month_interval` beside it is future work, and
`1 month = 30 days` is not a rule invented to avoid needing one.

It writes as an ISO 8601 duration — `P1DT1H1M1.5S`, `-PT1H`, `PT0S` — with no
year or month designator, so `P1M` is text this type cannot read. The form is
**canonical**, so `PT30H` reads back as `P1DT6H`: one value, one spelling.

`+` and `-` work on all four, and every case is total:

| | result | |
|---|---|---|
| `date - date`, `time - time`, `timestamp - timestamp` | `interval` | how far apart |
| `timestamp ± interval` | `timestamp` | exact |
| `time ± interval` | `time` | wraps at midnight |
| `date ± interval` | `date` | **truncates to whole days** |
| `interval ± interval` | `interval` | exact |

`interval + x` reads as well as `x + interval` and means the same thing.
Subtraction does not commute: a moment less a duration is a moment, and a
duration less a moment is nothing, so only the first order typechecks.

A `date` has no sub-day resolution, so a shift truncates toward zero — 30 hours
moves it a day, one hour leaves it alone, and so does minus one hour. That is
the one lossy rule here, and it is what keeps every case total. Two consequences
worth knowing: it is **not invertible** — `(d + iv) - iv` is `d` only for a whole
number of days — and **not associative over addition**, since two shifts of
twelve hours move nothing where one of a day moves a day.

`record(f: T, …)` is a named-field record. Field names are bare identifiers;
quote a name that is not a valid identifier (`record("total count": i64)`).

Note that `record` is spelled the same way in type position and in expression
position — the type names the fields, and the literal fills them:

```
r :: zset(record(id: i64, name: string))                        # the type
map(s, function((row) -> record(id: row.id, name: row.name)))   # a value
```

**`record()` is the record with no fields**, and so the type with exactly one
value. It is written the same way in both positions, and it is `{}` on the wire.

It exists because three things need a name for "no columns", and it is the same
name for all three:

- **A stream with no columns.** `zset(record())` carries a proposition: every row
  is the same key, so after a `distinct` the relation is present or absent and
  nothing else.
- **The key of a cross product.** `join` needs both sides indexed on one key
  type; indexing both on `record()` gives every row one partner, which is what a
  cross product is. There is no `cross` operator because there does not need to
  be one.
- **The key of an aggregate over everything.** An `aggregate` with nothing to
  group by is `map_index` to `record()` and then the fold.

One consequence worth stating rather than leaving to be found: `cast(doc, record())`
succeeds for **any** document. A record whose fields are all optional extracts
from a non-object as an all-absent record rather than failing, and a record with
no fields is vacuously all-optional, so there is nothing left to fail on.

`record()` and an empty `dict(K,V)` are both `{}` on the wire. Decoding is driven
by the declared type, so this is not an ambiguity — it is the same benign overlap
that already puts `record(a: i64)` and `dict(string, i64)` on the same encoding.

`array(T)` is a sequence of one element type, and an ordinary value: it can sit
in a record, in a column and in a stream, and `flat_map` turns one into rows.

`dict(K,V)` is a key-value map, and an ordinary value in the same way. Its
entries are **sorted by key and deduplicated**, so two dicts written with their
entries in different orders are one value, one hash and one Z-set key — the same
property `record` gets by sorting its fields and `json` gets from its encoding.

**A dict key is a scalar** — `bool`, `i64`, `f64` or `string`. A dict is a JSON
object on the wire and an object's keys are strings, so a key type has to have
one string spelling that its own type reads back; no composite type does. It is
called `dict` rather than `map` because `map` is an operator, and one word
should not be both.

**Field order is not part of a record's identity.** `record(a: i64, b: string)`
and `record(b: string, a: i64)` are one type, so a frontend building the same
record on two code paths need not canonicalise its own output first. Order is
still load-bearing at runtime — the value is positional, and the order is the
column order the codec writes — so the checker sorts fields by name, and sorts
the values with them.

`json` is a whole document of any shape. It has no structure the type system
describes — that is the point of it — so reaching inside one is `get` and
converting one is `cast`. See [Documents](#documents).

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
operator's input batch shape(s) and its result batch shape. Two conventions:

- **`X`** is either batch shape. `X → X` means the shape is preserved; `X` on
  the left with a named shape on the right means the operator accepts either and
  always produces that one.
- **`E`** is the stream's *element*: the row of a `zset(T)`, or the `(key,
  value)` pair of an `indexed_zset(K,V)` — which is why a function over an
  indexed stream takes two parameters. See the note below the table.

Which `dbsp` method each lowers to is in [`mapping.md`](mapping.md), which is
where that mapping is maintained.

| operator | signature |
|---|---|
| `input("t")` | → `zset(T)` |
| `constant([r, …])` | → `zset(T)` |
| `map(s, f)` | `X → zset(U)`, `f : E → U` |
| `filter(s, f)` | `X → X`, `f : E → bool` |
| `flat_map(s, f)` | `X → zset(U)`, `f : E → array(U)` |
| `map_index(s, f)` | `X → indexed_zset(K,V)`, `f : E → record(key: K, value: V)` |
| `flat_map_index(s, f)` | `X → indexed_zset(K,V)`, `f : E → array(record(key: K, value: V))` |
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

**A row-at-a-time function takes the stream's element**, written `E` above. For
a flat stream that is one row; for an indexed stream it is the `(key, value)`
pair, so the function takes two parameters — `function((k, v) -> …)` — matching
`dbsp`'s `ItemRef` for each shape. Every operator in the mapping family follows
this rule, so none of them cares which shape it was given.

The *result* shape comes from what the function returns, not from what it was
given. **So `map` over an indexed stream flattens it**, and that is the only
route out of an indexed shape other than a join. It is what makes an outer join
expressible: `antijoin` yields an indexed stream whose rows would otherwise be
stuck there. There is no `left_join` operator, because there need not be one:

```
matched   := join(emp_idx, dept_idx, function((k, e, d) ->
                 record(name: e.name, dname: cast(d.dname, optional(string)))))
unmatched := antijoin(emp_idx, dept_idx)
nulled    := map(unmatched, function((k, e) ->
                 record(name: e.name, dname: cast(NONE, optional(string)))))
out       := plus(matched, nulled)
```

**Operator calls nest.** A stream argument may be another operator call rather
than a name, so `weighted_count(map(emp, f))` is one declaration. A named node
then exists because it is worth naming — as an output, or as an intermediate
used more than once — rather than because the grammar insists.

**`input` and `constant` are the exceptions.** Both take their type from a `::`
typespec, which needs a name to attach to, so `map(input("emp"), f)` and
`plus(a, constant([record(v: 1)]))` are both rejected. Bind them first.

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

| aggregator | result |
|---|---|
| `min` / `max` | `A` — the smallest/largest projected value in the group |
| `sum` | the sum of the projected values |
| `avg` | their mean |
| `count` | the number of rows whose projected value is not absent, of whatever type |

**`count` is the only one whose result type is fixed.** It is an `i64`; every
other aggregator gives back the projection's own type, `A`, wrapper and all.

So the mean of `i64`s is an `i64`, and integer division **truncates toward
zero**: the mean of `-1` and `-2` is `-1`, not `-2`. That is the same direction
`/` takes everywhere else here, which is the reason to prefer it — a language
with two roundings in it has to say which one each place uses.

**Every aggregator ignores absent projections, as SQL does.** `min` and `max`
report the smallest and largest value that is there; `sum` adds the ones that
are there; `avg` averages them, over how many there were rather than how many
rows the group has; `count` counts them. A group in which *every* projection is
absent still reports — with `NONE`, rather than disappearing.

Which is the only way a result is absent, and it needs an optional projection to
arise at all: a group exists because a row is in it, so with a definite
projection every group has something to fold.

That is worth stating because it does not come for free. `NONE` sorts before
every value, so the obvious lowering of `min` would report absence for a group
that merely *contains* an absent row, while `max` would be unaffected. See
[`mapping.md`](mapping.md) for what each is lowered to and why they differ.

**`min` and `max` compare with the runtime value type's ordering**, which is
therefore load-bearing for query results and not merely for batch layout — see
the invariants section of [`mapping.md`](mapping.md). It is also why a `json`
projection is rejected for them.

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
            | "record" "(" [FIELD_NAME ":" expr ("," FIELD_NAME ":" expr)*] ")"
            | "{" [expr "=>" expr ("," expr "=>" expr)*] "}"   # a dict
            | "dict" "(" expr ")"                         # a dict, from an array
            | "[" expr ("," expr)* "]"                    # an array
            | "(" expr ")"                                # grouping, only
            | unop expr
            | expr binop expr
            | NAME "(" [expr ("," expr)*] ")"             # a builtin, or a named function
            | "map_array" "(" expr "," anon_function ")"     # one element per element
            | "filter_array" "(" expr "," anon_function ")"  # the elements it keeps

unop       := "-" | "not"
binop      := "+" | "-" | "*" | "/" | "%"
            | "==" | "!=" | "<" | "<=" | ">" | ">="
            | "and" | "or"
literal    := INT | FLOAT | STRING | "true" | "false" | "NONE"
```

A `STRING` is double-quoted, and its escapes are `\"`, `\\`, `\n`, `\t` and
`\r`; a backslash before anything else is an error, so a typo is reported rather
than silently dropped. That is the same set
[grasp](../grasp/syntax.md#literals) has, and it is the same set on purpose: a
string a source language can write and this one cannot is a hole in the pair.

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

A dict is built two ways, because they do different jobs: `{k => v, …}` fixes
its entries in the source, and `dict(a)` takes however many an
`array(record(key: K, value: V))` carries. `dict_entries` is the inverse of the
second.

```
d := {"a" => 1, "b" => row.n}
d := dict(row.pairs)                 # pairs : array(record(key:, value:))
```

Braces are unambiguous here: elsewhere they open a circuit or function body, and
both of those are declarations rather than expressions.

A key written twice keeps the last value.

### Empty containers take a type from context

`[]` and `{}` are complete values with open types — an empty array is one value
whatever its elements would have been — so neither carries a type of its own.
This is the rule `NONE` already follows, and `empty()` one level up.

Three things can supply the type, and the first that applies wins:

- **The node's typespec.** A `::` annotation says what the operator's function
  must return, and that flows into the body:

  ```
  out :: zset(record(tags: dict(string, i64)))
  out := map(t, function((r) -> record(tags: {})))
  ```

- **A sibling**, wherever two values must already meet under one type —
  `coalesce(m, {})`, `if(c, m, {})`, a comparison.

- **A `cast`**, written out where neither reaches:
  `cast({}, dict(string, i64))`. This is `cast(NONE, optional(i64))` for the
  same reason.

With none of them, it is an error naming all three rather than a guess: an
element type nobody chose would otherwise end up in the output schema.

**A typespec supplies a type only where inference has none. It never overrides
one** — a node whose inferred type disagrees with its annotation is still an
error, which is what keeps the annotation a check.

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
posts :: zset(record(id: i64, tags: array(string)))
tags  := flat_map(posts, function((r) -> r.tags))
```

### `map_array` and `filter_array`

`map_array(a, function((e, i) -> body))` evaluates `body` once per element of
the array `a` — with `e` bound to that element and `i` to its 0-based index —
and collects the results into an array. `filter_array` takes the same shape,
its `body` is a `bool`, and what comes back is the elements it kept, in order.

They are the **two constructs in the language that bind a name inside an
expression**, and the only two things that read an array element by element.

`map_array` exists for `flat_map`. That operator emits one row per element of
the array its function returns, so the output row *is* the element — and a row's
other columns cannot survive the fan-out unless the function can build an array
of whole rows. Nothing else could build one:

```
person :: zset(record(name: string, tags: array(string)))
tagged := flat_map(person, function((r) ->
    map_array(r.tags, function((e) -> record(name: r.name, tag: e)))))
```

`dict_entries` already yields `array(record(key: K, value: V))`, so a dict fans out
through the same shape, and `map_array` is what puts the row back beside each
entry.

`filter_array` is the length-changing one, which is a capability rather than a
second spelling: `map_array` cannot change an array's length, and the `filter`
*operator* works on rows rather than on an array inside one. The names say which
of the two levels each works at, rather than borrowing a second vocabulary a
reader would have to hold beside the operators' — the rule that keeps `dict` from
being called `map`.

With `dict_entries` and `dict` around it, it is how keys are subtracted from a
dict — including by a set of keys the data supplies, which is what `contains`
buys:

```
less := dict(filter_array(dict_entries(r.d), function((e) -> e.key != "b")))
fewer := dict(filter_array(dict_entries(r.d), function((e) -> not contains(r.ks, e.key))))
```

It can also take a run of elements by index, but only forwards: `slice` is what
covers the rest, being the one that can reverse.

- **The function is not a value.** It is written where it is used, the way
  `cast`'s second argument is a type rather than an expression. There is no
  function type and nothing may pass one around.
- **The body sees the enclosing function's parameters**, which is what makes it
  worth having: `r.name` above comes from the row, `e` from the element.
- **The index binder is optional to name, never to bind.** A function may be
  written `function((e) -> …)` or `function((e, i) -> …)`, and the second form
  is the only way to reach the index — but the element and the index take a
  frame slot each either way. That is what makes two bodies that ignore the
  index one expression however they were written, rather than two that happen to
  compute the same array.
- **A binder may not shadow a name already in scope.** There is one reading of a
  name, and no rule to remember about which one wins.
- **They are expressions.** `x := map_array(a, f)` is not a node definition; it
  belongs inside a function body.

One neighbour they do not have. **A `fold`** would be a new capability again —
nothing here reduces an array to a scalar. But it would subsume `length`, and
"exactly one way to write each thing" wants that answered rather than left:
either `length` goes, or the two coexist and an emitter is given a rule for
choosing. Nothing has needed a fold, so nothing has answered it.

### Builtins

Free functions callable from any expression. They compose with the operators
above and with each other.

| builtin | signature | result |
|---|---|---|
| `coalesce` | `optional(T) × T → T` | `x` if present, else `y` |
| `if` | `bool × T × T → T` | the taken arm |
| `abs` / `floor` / `ceil` / `round` | `T → T`, `T` numeric | type-preserving |
| `length` | `string → i64`, `array(T) → i64`, `dict(K,V) → i64` | element, entry or character count |
| `concat` | `string × string → string` | concatenation |
| `lower` / `upper` / `trim` | `string → string` | |
| `get` | `(json \| optional(json)) × (string \| i64) → optional(json)` | a member, by key or 0-based index |
| `get` | `dict(K,V) × K → optional(V)` | an entry, by key |
| `get` | `array(T) × i64 → optional(T)` | an element, by 0-based index |
| `keys` | `json → optional(array(string))` | an object's keys, `NONE` otherwise |
| `keys` | `dict(K,V) → array(K)` | a dict's keys, sorted |
| `dict_entries` | `dict(K,V) → array(record(key: K, value: V))` | a dict's entries, sorted by key |
| `contains` | `array(T) × T → bool` | whether the array holds the element |
| `slice` | `array(T) × optional(i64) × optional(i64) × i64 → array(T)` | Python's slice |
| `make_date` | `i64 × i64 × i64 → optional(date)` | year, month, day |
| `make_time` | `i64 × i64 × i64 × i64 → optional(time)` | hour, minute, second, microsecond |
| `make_timestamp` | `date × time → timestamp` | the two halves of an instant |
| `timestamp_from_micros` | `i64 → timestamp` | microseconds since the epoch |
| `epoch_micros` | `timestamp → i64` | the inverse |
| `epoch_days` | `date → i64` | days since the epoch |
| `year` / `month` / `day` | `date \| timestamp → i64` | a date's components |
| `hour` / `minute` / `second` / `microsecond` | `time \| timestamp → i64` | a time's components |
| `make_interval` | `i64 → interval` | microseconds |
| `total_days` … `total_microseconds` | `interval → i64` | the span in whole units of one size |

Every builtin but `coalesce` and `slice` rejects an `optional` argument, for the
reason under [Absence](#absence). Those two inspect absence rather than being
rejected for it, and they differ in what they do with it: `coalesce` replaces it,
and `slice` reads it as "no bound here".

There is no `+` on strings. `concat` is the one way to join them, and `+` is
arithmetic only.

`get` and `keys` each cover a document and a dict, which are the same idea at
two levels of typing. They differ where the types differ: a dict lookup is exact
rather than navigation, and `keys` on a dict is definite — a dict is always a
dict, so there is no "not an object" case to report as absence. `dict_entries` is
what turns a dict into rows, through `flat_map`.

`get` covers a typed **array** on the same terms: exact rather than navigation,
0-based, and `optional(T)` because an index may be past the end. Absence is the
answer for an index out of range at either end, which is what a document already
says for a member that is not there. It is the only way to read an element by
position — `map_array` and `filter_array` walk every element and cannot single
one out.

`contains` and `slice` are the two array operations that are neither navigation
nor a walk. `contains` could be written
`length(filter_array(a, function((e) -> e == x))) > 0`, which allocates an array
to answer a boolean; `slice` takes a run of elements, and **`filter_array`
cannot reverse**, so a negative step needs it whether or not the rest does.

`slice` is Python's. Negative indices count from the end, the bounds **clamp**
rather than fail — `slice(a, 0, 99, 1)` on a three-element array is those three —
and a negative step reverses. Its bounds are `optional(i64)` because "no bound
here" is not a number: with a positive step the missing start is `0` and the
missing stop is the length, and with a negative one they are the last index and
one before the first. `NONE` is the only faithful spelling, which is why Python's
own slice carries `None` there.

A step of zero selects nothing. Python raises; every expression here is total, so
the empty array is the answer, and it is the one place this differs from the
semantics it copies.

`make_date` and `make_time` are **`optional`**, because not every triple of
integers is a date and not every hour is an hour — the shape `/` already has.
`make_timestamp` is not: every date and time-of-day is one instant.

The components read **either type that holds them** — `year` a `date` or a
`timestamp`, `hour` a `time` or one — the way `length` reads a string, an array
or a dict, so an instant needs no conversion written around it. `microsecond` is
the **sub-second** part, unlike SQL's `EXTRACT(MICROSECOND)` which folds the
seconds in, so that it is the inverse of the argument `make_time` takes and the
two round-trip.

The `total_*` readers are named apart from the components because they answer a
different question: `minute(14:30)` is 30, and `total_minutes(PT100H)` is 6000.

Reserving `year` through `microsecond` takes six ordinary words out of the space
of node names. That is the cost of having no namespaces here, and it is paid
where the language is written by hand: a frontend's colliding name is mangled on
the way in.

`if` is the only branching construct, and the only builtin that does not
evaluate all of its arguments — the untaken arm does not run. Because every
expression here is total, that is a cost property and never a semantic one: an
untaken branch could not have produced an error to avoid. Its two arms meet
under the same rule everything else does, so `if(c, x, NONE)` widens to
`optional(T)` and `if(c, i64_val, f64_val)` is rejected like any other
mixed-type expression. Nested, it is how a SQL `CASE` lowers.

### `cast`

`cast(x, T)` converts between two known types. It is the one place a *type*
appears in expression position, which is why it is a construct rather than a
builtin.

**`cast(x, T)` yields exactly `T`.** Where a conversion has inputs the target
cannot hold, that is an error naming the fix rather than a silently optional
result:

```
cast(r.x, i64)             # error: can fail (NaN, infinity, or out of range)
cast(r.x, optional(i64))   # optional(i64)
```

That keeps a declared type a promise, and follows the rule division already set.

| from → to | `bool` | `i64` | `f64` | `string` | `json` |
|---|---|---|---|---|---|
| `bool`   | — | — | — | total | total |
| `i64`    | — | — | total | total | total |
| `f64`    | — | fallible | — | total | total |
| `string` | fallible | fallible | fallible | — | total |
| `record(…)` / `array(T)` | — | — | — | — | total |
| `json`   | fallible | fallible | fallible | fallible | — |
| `date` / `time` / `timestamp` | — | — | — | total | total |

Text converts **to** a temporal type as well, and fallibly — parsing is how one
arrives from outside, and `2024-13-45` is not a date. So `cast(s, date)` is an
error and `cast(s, optional(date))` is the way to write it, which is the rule
every other parse here follows.

A document also converts to a `record(…)` or an `array(T)`, and both are
fallible. Those two rows are not a matrix: a document is converted by what is
*wanted* rather than by what it happens to hold, so every extraction is one rule
and every construction is another.

A conversion to the same type is the identity. `NONE → optional(T)` is total for
any `T`: that is how a definite value's absent counterpart is written, and a
bare `NONE` in a record field has no type to infer without it.

An `optional` **target** is what admits an absent input, so absence needs no
propagation rule of its own — the written type says whether it is allowed
through. `optional(T) → optional(U)` is allowed whenever `T → U` is, and maps
absence to absence.

Records and arrays have no conversions.

`cast` exists because there is no implicit conversion: without it there would be
no path at all from `i64` to `f64`, and a query as ordinary as `a * 1.5` on an
integer column could not be written.

### Documents

A `json` is a whole document. The type system says nothing about its shape, so
there are two operations and no third:

- **`get`** reaches inside one. A missing key, an index past the end, or a
  document that is not a container yields `NONE`, and `get` accepts what it
  returns — so a chain needs no guard at any level:

  ```
  cast(get(get(r.payload, "user"), "id"), optional(i64))
  ```

  It is the one builtin whose argument may be absent. That is not the
  propagation arithmetic refuses: navigating into nothing has one sensible
  answer, whereas `+` on an unknown has none without inventing SQL's semantics.

- **`cast`** converts one out. Every extraction is fallible, because a document
  need not hold the shape asked of it, so the target is `optional`. A `record`
  target extracts the fields it names, converts each and ignores the rest;
  it fails as a whole if a named field is missing or holds the wrong shape:

  ```
  cast(r.payload, optional(record(id: i64, name: string)))
  ```

There is deliberately no pattern language. An earlier design had `match`, type
patterns, structural patterns and open records; `cast` and `get` cover every
case those did, and cost no new grammar.

**Numbers keep their tags, and both extractions exist.** `cast(d, optional(i64))`
takes an integer document exactly — a 64-bit key survives — while
`cast(d, optional(f64))` takes any number, so `5` and `5.0` both convert.

**Two kinds of nothing, and `get` tells them apart.** `NONE` means *there is no
such member*; a document that is null means *the member is there, holding
`null`*. So `get(d, "x") != NONE` is a presence test, and
`cast(get(d, "x"), optional(i64))` is absent for both a missing key and a null
one — which is usually what you want, and the comparison is there when it is
not.

The encoding has a third state, an absent sentinel distinct from JSON null, but
the language never shows it: `get` converts it to `NONE` at the boundary. It
would otherwise be a value that prints as `null` without *being* the null
document — a difference nothing could see.

On the wire the codec follows Feldera exactly:

| column | omitted | `null` |
|---|---|---|
| `T` | error | error |
| `optional(T)` | `NONE` | `NONE` |
| `json` | error | **the null document** |
| `optional(json)` | `NONE` | `NONE` |

`json` is the one type whose value set contains null, which is why a definite
`json` column may accept a bare `null` where a definite `i64` may not. The cost
is on the last row: in an `optional(json)` column a null document and absence
both write `null` and both read back as absence, so **a top-level null document
degrades to absence there**. Declare the column `json` to keep it. Nulls
*inside* a document are unaffected either way.

**Documents compare, but do not order.** `==` and `!=` are allowed: the encoding
is canonical — map keys are stored sorted and deduplicated, so `{"a":1,"b":2}`
and `{"b":2,"a":1}` are one value — which makes a document sound as an index or
join key. `<`, `<=`, `>`, `>=` and a `min`/`max` projection are **rejected**,
because the encoding sorts by type tag: every number would precede every string,
an order that is well defined and meaningless. Extract a value and compare that.

Equality does distinguish `5` from `5.0`, since documents keep their tags. Those
are two different documents, so this is a true statement about them rather than
a lie about one — but it is worth knowing before using a document as a key.

### SQL null semantics

SQL's `a + b` is `NULL` when either side is. This language's arithmetic rejects
an optional operand instead, because propagating silently is the thing `NONE`
exists to avoid — see [Absence](#absence). A frontend that wants SQL's behaviour
writes it out, once:

```
function add_null(a, b) {
    return if(a == NONE or b == NONE, NONE, coalesce(a, 0) + coalesce(b, 0))
}
```

That serves every numeric type, because `0` takes the type of whatever it is
coalesced against — which is the case named functions exist for.

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

That has one consequence at the output boundary: `f64` includes NaN and the
infinities, which arithmetic can reach (`1e308 * 10`) and JSON cannot spell.
They are written as `null`, which is what `serde_json` and therefore Feldera do
— so such a row does not decode back into the same schema. This is not the
codec refusing to write `NONE` into a definite column: `NONE` is not an `f64` at
all, while NaN is one that JSON has no syntax for.

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
inside a `json` document — see [Documents](#documents) — so that JSON pasted
into source keeps its meaning. On the wire it is unchanged:
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
  type rather than overriding it — it supplies one only where inference has none
  at all, as for an [empty container](#empty-containers-take-a-type-from-context).
- **Only recursive members leave the fixpoint.** `fp.path` works; other body
  nodes exist only inside the nested circuit.
- **`distinct` is applied for you** to each recursive stream, on every round.
  That is what makes the iteration terminate, and writing your own would lower
  a redundant second one.
- Every recursive stream in one `fixpoint` must have the same shape, since one
  nested batch type serves them all.

## `constant()`

`constant([r₁, r₂, …])` is a relation that is constantly those rows.

```
edge :: zset(record(src: i64, dst: i64))
edge := constant([record(src: 1, dst: 2), record(src: 2, dst: 3)])
```

**What that means as a stream of changes.** Every stream here carries a
*change*, so a relation that never changes is a batch delivered in the first
transaction and nothing after: `constant(X)` reports `X` once, and
`integrate(constant(X))` is `X` at every transaction. Operators downstream keep
their own state — `join`, `distinct` and `aggregate` are incremental — so they
go on seeing the rows long after the delta stops. That is what makes it a
relation rather than a one-off pulse.

**The argument is a closed expression.** Literals, arithmetic over them,
`record(…)`, `[…]`, `{…}`, `cast`, the builtins and named functions all work; a
parameter reference does not, because there is no row to read it from. The rows
are computed once, when the program is checked, so `constant([record(v: 1 + 1)])`
holds `record(v: 2)`.

**It needs a typespec, and it does not nest** — the same rule as `input`, for the
same reason: nothing in the argument says what type the rows are, so the
annotation is the only thing that can, and an annotation needs a name.

- **`zset(T)` only.** For an indexed constant, write
  `map_index(constant([…]), f)`. `T` need not be a `record`; only `input` is
  restricted that way.
- **The typespec is checked, not used to drive inference**, so a bare numeric
  literal still settles to its own type: `constant([record(v: 1)])` is
  `zset(record(v: i64))`, and an `f64` column wants `1.0` or `cast(1, f64)`.
  This is where that rule is most visible, because there is no operand beside
  the literal at all — see [Numbers](#numbers).
- **Rows may repeat**, and the second copy is a second unit of weight, as
  everywhere else.
- **Row order is not part of the relation**, so `constant([a, b])` and
  `constant([b, a])` are one node.
- **Not inside a `fixpoint` body.** A source there fires once per iteration
  rather than once per transaction. Declare it outside and pass it in as a
  circuit parameter, the way an input is passed in.

## `empty()`

`empty()` is a stream with no rows. It carries no type of its own — it takes one
from where it sits:

- beside a typed operand of `plus`, `minus` or `sum`, which require identical
  batch types anyway;
- as a circuit argument, from the body node the parameter feeds;
- on its own, from its `::` typespec.

Anywhere else it is an error saying so, rather than guessing.

`empty()` is the zero of the same family `constant()` populates, and the two
stay separate because their typing rules are opposites: `constant` *requires* a
typespec, since its rows cannot say what type they are, while `empty()`
deliberately has none and takes one from where it sits. So an empty constant is
written `empty()`, and `constant([])` is rejected — there is one way to write
each thing.

## Reserved words

These may not name a node, a function or a parameter: the 21 operator names,
the 5 aggregator names, the builtin names, the type constructors (`bool`,
`i64`, `f64`, `string`, `json`, `optional`, `record`, `array`, `dict`, `sql`,
`zset`, `indexed_zset`), `cast`, `map_array`, `filter_array`, and `true`,
`false`, `NONE`, `null`, `function`, `return`, `and`, `or`, `not`, `circuit`,
`fixpoint`.

`if` is reserved by being a builtin, like every other builtin name. `then` and
`else` are **not** reserved: the conditional is `if(cond, a, b)` and there is no
syntactic one to hold them for, so those words are free to name a node.

`sql` is reserved although the namespace is empty, so the Feldera value types
can arrive later without breaking a program that used the name meanwhile.

Record *field* names are unrestricted — they are their own namespace and can be
quoted.

## Example

```
emp := input("emp")
emp :: zset(record(id: i64, name: string, dept_id: i64, salary: i64))

dept := input("dept")
dept :: zset(record(id: i64, dname: string))

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
