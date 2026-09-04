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
batch_type := "OrdZSet" "(" value_type ")"
            | "OrdIndexedZSet" "(" value_type "," value_type ")"
```

- `OrdZSet(T)` — a flat weighted set of `T`.
- `OrdIndexedZSet(K, V)` — a keyed set: each key `K` maps to a weighted set of
  values `V`.

These names are the `dbsp` type names, so a reader can map them directly to
`dbsp::OrdZSet` / `dbsp::OrdIndexedZSet`.

### Value types

```
value_type := builtin | "sql" "." sql_type | record_type

builtin    := u8 | u16 | u32 | u64 | i8 | i16 | i32 | i64 | f32 | f64 | bool
            | String | Vec "(" value_type ")" | Option "(" value_type ")"
            | Tup0 | Tup1 | ... | Tup10  (each with the matching number of type arguments)

sql_type   := SqlString | ByteArray | SqlDecimal "(" precision "," scale ")"
            | Date | Time | Timestamp | TimestampTz
            | LongInterval | ShortInterval
            | Uuid | Variant
            | Array "(" value_type ")" | Map "(" value_type "," value_type ")"

record_type := "record" "(" field ("," field)* ")"
field       := FIELD_NAME ":" value_type
```

Two namespaces are deliberately separated:

- **Plain builtins** (`u64`, `i64`, `String`, `Vec`, `Option`, `Tup*`) are Rust
  primitive/std types `dbsp` works with directly.
- **`sql.*` types** mirror the Feldera SQL value types from `feldera-sqllib`
  (`SqlString`, `Date`, `SqlDecimal`, …). The `sql.` prefix says "this is a
  `sqllib` type, not a plain Rust type". `sql.Array(T)` is `Arc<Vec<T>>` (not
  `Vec<T>`); `sql.SqlDecimal(p,s)` is a fixed-point type (not `f64`).

The two namespaces do not overlap. `feldera-sqllib` has no integer, float or
boolean types of its own — in Feldera, SQL `BIGINT` *is* `i64` and SQL `DOUBLE`
*is* `f64` — so those are spelled with the plain builtin names and there is
exactly one way to write them. Where both namespaces appear to offer the same
thing they are genuinely different runtime types: `String` is `std::String`
while `sql.SqlString` is a cheaply-cloned `ArcStr`, and `Vec(T)` is `Vec<_>`
while `sql.Array(T)` is `Arc<Vec<_>>`.

**Nullability is `Option(T)`, in both namespaces.** There is no separate nullable
flag; a nullable SQL string is `Option(sql.SqlString)`.

`record(f: T, …)` is a named-field record. `Tup*` is the positional
alternative. Field names are bare identifiers; quote a name that is not a valid
identifier (`record("total count": i64)`).

Note that `record` is spelled the same way in type position and in expression
position — the type names the fields, and the literal fills them:

```
r :: OrdZSet(record(id: i64, name: sql.SqlString))     # the type
map(s, fun((row) -> record(id: row.id, name: row.name)))   # a value
```

### Typing rules

- An **`input` node requires a `typespec`**, always `OrdZSet(record(...))`
  (a table is a set of named-field rows). The record's field order is the
  deterministic column order used by the JSON codec.
- **All other nodes are inferred** from the operators applied to them. An
  explicit `typespec` elsewhere is checked, not used to drive inference.
- **Scalar value types are allowed** anywhere a `value_type` is expected
  (for example `OrdZSet(i64)` for a projected single column); only the
  `input` node is restricted to `record`.

## Operators

Operators are `dbsp` primitives exposed as-is. The reference below gives each
operator's input batch shape(s), its result batch shape, and the `dbsp` method
it lowers to. `X` means the shape is preserved.

| operator | signature | `dbsp` method |
|---|---|---|
| `input("t")` | → `OrdZSet(T)` | `add_input_zset` |
| `map(s, f)` | `OrdZSet(T) → OrdZSet(U)`, `f : T → U` | `map` |
| `filter(s, f)` | `X → X`, `f : T → bool` | `filter` |
| `flat_map(s, f)` | `OrdZSet(T) → OrdZSet(U)`, `f : T → Vec(U)` | `flat_map` |
| `map_index(s, f)` | `OrdZSet(T) → OrdIndexedZSet(K,V)`, `f : T → (K,V)` | `map_index` |
| `flat_map_index(s, f)` | `OrdZSet(T) → OrdIndexedZSet(K,V)`, `f : T → Vec((K,V))` | `flat_map_index` |
| `join(l, r, f)` | `OrdIndexedZSet(K,V₁) × OrdIndexedZSet(K,V₂) → OrdZSet(OV)`, `f : (K,V₁,V₂) → OV` | `join` |
| `join_index(l, r, f)` | as `join`, but `f : (K,V₁,V₂) → (OK,OV)` → `OrdIndexedZSet(OK,OV)` | `join_index` |
| `antijoin(l, r)` | `OrdIndexedZSet(K,V) × OrdIndexedZSet(K,V₂) → OrdIndexedZSet(K,V)` | `antijoin` |
| `distinct(s)` | `X → X` (deduplicated) | `distinct` |
| `aggregate(s, agg, f)` | `OrdIndexedZSet(K,V) → OrdIndexedZSet(K,A)` | see [Aggregators](#aggregators) |
| `weighted_count(s)` | `OrdZSet(T) → OrdIndexedZSet(T, i64)` | `weighted_count` |
| `neg(s)` | `X → X` | `neg` |
| `plus(a, b)` | `X × X → X` | `plus` |
| `minus(a, b)` | `X × X → X` | `minus` |
| `sum(a, b, …)` | `X⁺ → X` | `sum` |
| `integrate(s)` | `X → X` (running sum) | `integrate` |
| `differentiate(s)` | `X → X` | `differentiate` |
| `delay(s)` | `X → X` | `delay` |

Operator arity follows `dbsp`: `plus` and `minus` are binary, `sum` is n-ary.

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
            | expr "[" INT "]"                            # tuple element
            | "record" "(" FIELD_NAME ":" expr ("," FIELD_NAME ":" expr)* ")"
            | "(" expr ("," expr)* ")"                    # tuple
            | "[" [expr ("," expr)*] "]"                  # list
            | unop expr
            | expr binop expr
            | BUILTIN "(" [expr ("," expr)*] ")"
            | "if" expr "then" expr "else" expr

unop       := "-" | "not"
binop      := "+" | "-" | "*" | "/" | "%"
            | "==" | "!=" | "<" | "<=" | ">" | ">="
            | "and" | "or"
literal    := INT | FLOAT | STRING | "true" | "false" | "null"
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

### Builtins

Free functions callable from any expression. They compose with the operators
above and with each other.

| builtin | signature | result |
|---|---|---|
| `is_null` / `is_not_null` | `(x)` | a boolean |
| `coalesce` | `(x, y)` | `x` if non-null, else `y` |
| `abs` / `floor` / `ceil` / `round` | `(x)` | numeric |
| `length` | `(x)` | length of a string, list or array |
| `concat` | `(x, y)` | string concatenation |
| `lower` / `upper` / `trim` | `(x)` | string |
| `cast` | `(x, T)` | `x` converted to value type `T` |

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
emp :: OrdZSet(record(id: i64, name: sql.SqlString,
                      dept_id: i64, salary: i64))

dept := input("dept")
dept :: OrdZSet(record(id: i64, dname: sql.SqlString))

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
