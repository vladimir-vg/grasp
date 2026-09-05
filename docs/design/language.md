# DBSP Runner — Language

The source language is declarative: a program is a flat list of declarations
that name streams and describe how they are derived from inputs. The grammar is
borrowed from grasp-dbsp; the operators and types it refers to come from
`dbsp`.

## Program structure

A program is a sequence of declarations. Declarations may appear in any order;
names are resolved after the whole program is parsed (forward references are
allowed).

```
program      := declaration*
declaration  := node_def | typespec | comment

node_def     := NAME ":=" op_call
typespec     := NAME "::" batch_type
comment      := "#" [^\n]*

op_call      := OP "(" [arg ("," arg)*] ")"
arg          := NAME                    # a stream declared elsewhere
              | op_call                 # a nested operator, except `input`
              | STRING                  # a table name, for `input`
              | AGGREGATOR              # min | max | count | sum | avg
              | fun
```

`OP` is one of the operators listed under [Operators](#operators); `AGGREGATOR`
is accepted only as the second argument of `aggregate`, so a bare name is never
ambiguous with a stream reference.

Node definitions assign a name to a derived stream. A `typespec` attaches a
type to a name; it is required only for `input` nodes and optional (but checked)
everywhere else.

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
value_type := builtin | "sql" "." sql_type | record_type

builtin    := bool | i64 | f64 | String | optional "(" value_type ")"

sql_type   := SqlString

record_type := "record" "(" field ("," field)* ")"
field       := FIELD_NAME ":" value_type
```

The set above is what is implemented. The namespaces below are designed to hold
more — the other integer widths, `f32`, `Vec`, `Tup*`, and the rest of the
`sql.*` types — and those are listed under future work in
[`overview.md`](overview.md).

Two namespaces are deliberately separated:

- **Plain builtins** (`i64`, `f64`, `bool`, `String`, `optional`) are Rust
  primitive/std types `dbsp` works with directly.
- **`sql.*` types** mirror the Feldera SQL value types from `feldera-sqllib`
  (`SqlString`, `Date`, `SqlDecimal`, …). The `sql.` prefix says "this is a
  `sqllib` type, not a plain Rust type". For example `sql.Array(T)`
  would be `Arc<Vec<T>>` rather than `Vec<T>`, and `sql.SqlDecimal(p,s)` a
  fixed-point type rather than `f64`.

The two namespaces do not overlap. `feldera-sqllib` has no integer, float or
boolean types of its own — in Feldera, SQL `BIGINT` *is* `i64` and SQL `DOUBLE`
*is* `f64` — so those are spelled with the plain builtin names and there is
exactly one way to write them. Where both namespaces appear to offer the same
thing they are genuinely different runtime types: `String` is `std::String`
while `sql.SqlString` is a cheaply-cloned `ArcStr`, and `Vec(T)` is `Vec<_>`
while `sql.Array(T)` is `Arc<Vec<_>>`.

**Absence is `optional(T)`, in both namespaces.** There is no separate nullable
flag; a SQL string that may be missing is `optional(sql.SqlString)`.

`record(f: T, …)` is a named-field record. Field names are bare identifiers; quote a name that is not a valid
identifier (`record("total count": i64)`).

Note that `record` is spelled the same way in type position and in expression
position — the type names the fields, and the literal fills them:

```
r :: zset(record(id: i64, name: sql.SqlString))     # the type
map(s, fun((row) -> record(id: row.id, name: row.name)))   # a value
```

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
operator's input batch shape(s), its result batch shape, and the `dbsp` method
it lowers to. `X` means the shape is preserved.

| operator | signature | `dbsp` method |
|---|---|---|
| `input("t")` | → `zset(T)` | `add_input_zset` |
| `map(s, f)` | `zset(T) → zset(U)`, `f : T → U` | `map` |
| `filter(s, f)` | `X → X`, `f : T → bool` | `filter` |
| `flat_map(s, f)` | `zset(T) → zset(U)`, `f : T → [U, …]` | `flat_map` |
| `map_index(s, f)` | `zset(T) → indexed_zset(K,V)`, `f : T → (K,V)` | `map_index` |
| `flat_map_index(s, f)` | `zset(T) → indexed_zset(K,V)`, `f : T → [(K,V), …]` | `flat_map_index` |
| `join(l, r, f)` | `indexed_zset(K,V₁) × indexed_zset(K,V₂) → zset(OV)`, `f : (K,V₁,V₂) → OV` | `join` |
| `join_index(l, r, f)` | as `join`, but `f : (K,V₁,V₂) → (OK,OV)` → `indexed_zset(OK,OV)` | `join_index` |
| `antijoin(l, r)` | `indexed_zset(K,V) × indexed_zset(K,V₂) → indexed_zset(K,V)` | `antijoin` |
| `distinct(s)` | `X → X` (deduplicated) | `distinct` |
| `aggregate(s, agg, f)` | `indexed_zset(K,V) → indexed_zset(K,A)` | see [Aggregators](#aggregators) |
| `weighted_count(s)` | `zset(T) → indexed_zset(T, i64)` | `weighted_count` |
| `neg(s)` | `X → X` | `neg` |
| `plus(a, b)` | `X × X → X` | `plus` |
| `minus(a, b)` | `X × X → X` | `minus` |
| `sum(a, b, …)` | `X⁺ → X` | `sum` |
| `integrate(s)` | `X → X` (running sum) | `integrate` |
| `differentiate(s)` | `X → X` | `differentiate` |
| `delay(s)` | `X → X` | `delay` |

Operator arity follows `dbsp`: `plus` and `minus` are binary, `sum` is n-ary.

**`filter`'s function takes the stream's element.** For a flat stream that is
one row; for an indexed stream it is the `(key, value)` pair, so the function
takes two parameters — `fun((k, v) -> …)` — matching `dbsp`'s `ItemRef` for
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

## Functions and expressions

Operators that transform rows (`map`, `filter`, `flat_map`, `map_index`,
`flat_map_index`, `join`, `join_index`, `aggregate`) take a function argument:

```
fun        := "fun" "(" "(" params ")" "->" expr ")"
params     := NAME ("," NAME)*

expr       := literal
            | NAME                                        # a bound parameter
            | expr "." FIELD_NAME                         # record field
            | "record" "(" FIELD_NAME ":" expr ("," FIELD_NAME ":" expr)* ")"
            | "(" expr "," expr ")"                       # a (key, value) pair
            | "[" expr ("," expr)* "]"                    # a list of output rows
            | unop expr
            | expr binop expr
            | BUILTIN "(" [expr ("," expr)*] ")"

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
fun((row) -> row)
fun((row) -> row.id)
fun((row) -> record(id: row.id, name: row.name))
fun((row) -> (row.dept_id, record(id: row.id)))
fun((row) -> row.salary > 100000 and row.dept_id == 3)
fun((k, e, d) -> record(name: e.name, dname: d.dname))
```

Duplicate field names within one `record(...)` literal are a parse error.

### Pairs and lists are syntax, not values

`(key, value)` and `[…]` describe *what a node does*; they are not values and
never flow through a stream. There is no pair type and no list type, and the
runtime has no variant for either.

- A `(key, value)` pair is legal only as the body of `map_index` or
  `join_index`, or as an element of a `flat_map_index` list. The type checker
  splits it into two independent expressions.
- A `[…]` list is legal only as the body of `flat_map` or `flat_map_index`,
  where its length fixes how many rows the operator emits per input row. The
  checker splits it into one expression per row.

The consequence worth knowing: **fan-out is fixed by the source, not the data.**
Exploding a column holding many values into a variable number of rows needs a
real list value, and is future work along with `Vec(T)`.

### Builtins

Free functions callable from any expression. They compose with the operators
above and with each other.

| builtin | signature | result |
|---|---|---|
| `coalesce` | `(x, y)` | `x` if present, else `y` |
| `abs` / `floor` / `ceil` / `round` | `(x)` | numeric |
| `length` | `(x)` | length of a string, list or array |
| `concat` | `(x, y)` | string concatenation |
| `lower` / `upper` / `trim` | `(x)` | string |

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

**`null` is not the absence literal.** It is reserved for the JSON null *value*
inside `sql.Variant` — a distinct thing, once `sql.Variant` is implemented — so
that JSON pasted into source keeps its meaning. On the wire it is unchanged: a
JSON `null` in a data position still decodes to absence for an `optional(T)`
column, and absence still encodes as JSON `null`.

## Circuits

A `circuit` is a named, parameterised block of declarations. It is expanded at
the call site: the body becomes ordinary nodes, reachable as
`<instance>.<node>`.

```
circuit normalize(src: s) {
    big     := filter(s, fun((r) -> r.v > 1))
    doubled := map(big, fun((r) -> record(v: r.v * 2)))
}

n   := normalize(src: a)
out := n.doubled
```

`label: internal` maps the keyword the caller uses to the name the body uses —
here the caller passes `src:` and the body refers to `s`. Every parameter must
be supplied, each exactly once, and the body may not define another circuit.

`out := n.doubled` is an **alias**: a right-hand side that is just a reference
adds no node, it only gives an existing one another name. That is how a body
node becomes selectable as a program output, since only declared names can be.

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
    path :: indexed_zset(i64, record(src: i64))
    step := join_index(p, f, fun((k, a, e) -> (e.dst, record(src: a.src))))
    path := plus(b, step)
}

fp      := fixpoint(tc(base: base, fwd: fwd, path: empty()))
closure := fp.path
```

- **A recursive stream starts empty**, so the call site passes `empty()`. There
  is no seeding: the base case belongs in the body, as `plus(b, step)` here.
- **A recursive body node needs a typespec.** Its type cannot be inferred,
  because the body that computes it also consumes it — `join_index` needs the
  parameter's value type before `plus` determines it.
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

### Reserved words

These may not name a node or a `fun` parameter: the 19 operator names, the 5
aggregator names, the builtin names, the type constructors (`bool`, `i64`,
`f64`, `String`, `optional`, `record`, `sql`, `zset`, `indexed_zset`), and
`true`, `false`, `NONE`, `null`, `fun`, `and`, `or`, `not`, `if`, `then`,
`else`, `circuit`, `fixpoint`.

`if`, `then` and `else` are reserved although there are no conditionals yet, so
adding them later will not break existing programs. Record *field* names are
unrestricted — they are their own namespace and can be quoted.

### Aggregators

`aggregate(s, agg, f)` takes an aggregator name and a projection `f : V → A`
applied to each value in the group. Aggregators are bare names:

```
aggregate(idx, max, fun((v) -> v.salary))
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

## Example

```
emp := input("emp")
emp :: zset(record(id: i64, name: sql.SqlString,
                      dept_id: i64, salary: i64))

dept := input("dept")
dept :: zset(record(id: i64, dname: sql.SqlString))

high_paid := filter(emp, fun((row) -> row.salary > 100000))

emp_idx  := map_index(emp,  fun((row) ->
                (row.dept_id, record(id: row.id, name: row.name, salary: row.salary))))
dept_idx := map_index(dept, fun((row) -> (row.id, record(dname: row.dname))))

joined   := join(emp_idx, dept_idx, fun((k, e, d) -> record(name: e.name, dname: d.dname)))

by_dept  := aggregate(emp_idx, max, fun((v) -> v.salary))
```

Both sides of the `join` are keyed by `i64`, so the key types are equal and the
join typechecks. Running this program with `joined` and `by_dept` named as
outputs emits their deltas; `high_paid` is still constructed, but not observed.
