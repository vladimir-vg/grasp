# grasp — Language

A grasp program is a set of **rules**. A rule names a relation and says how its
tuples are derived. Rules may appear in any order, they may refer to relations
defined later, and a relation may be defined by several rules at once.

This describes the language as designed. Where it is narrower than the Erlang
implementation it is ported from, [`overview.md`](overview.md) says why, and
[`mapping.md`](mapping.md) records the differences.

## Program structure

```
program ::= (spec | rule)*

spec    ::= relation_name "::" "relation" "(" [col_types] ")"
rule    ::= fact | single_line_rule | multiline_rule
```

Blank lines are ignored. `#` begins a comment that runs to the end of the line.

A **spec** declares a relation's columns and their types:

```grasp
emp  :: relation(name: string, dept: i64, sal: i64)
edge :: relation(src: i64, dst: i64)
```

A spec is required for relations that come from outside the program (below), and
optional elsewhere — a derived relation's column types are inferred from the
rules that define it. Writing one anyway is checked against what is inferred,
so it documents and constrains at the same time rather than overriding.

## Rules

### Facts

A relation name with parenthesised key-value arguments and no body. Every value
is a literal.

```grasp
edge(src: 1, dst: 2)
emp(name: "Alice", dept: 1, sal: 100)
```

### Rules with bodies

A head, `<-`, and a body. The body is either a single statement on the same
line, or a sequence of statements each on its own indented line:

```grasp
path(src: x, dst: y) <- edge(src: x, dst: y)

result(name: n, title: t) <-
    student(id: s, name: n, age: a)
    a > 20
    course(id: c, title: t)
    not enrolled(student: s, course: c)
```

Indentation width is set by the first body statement; blank and comment lines
may appear between statements. Statement order does not matter — see
[Body statements](#body-statements).

**Shorthand.** In a head, fact or atom, `x:` means `x: x` — the column and the
variable share a name.

```grasp
active(name:) <- student(name:, enrolled: true)
```

### Unions

Several rules for one head relation contribute to it together:

```grasp
parent(child: x, parent: y) <- father(child: x, parent: y)
parent(child: x, parent: y) <- mother(child: x, parent: y)
```

The result is a set: a tuple derived by two rules appears once.

### Recursion

Recursion is a relation mentioning itself. There is no keyword.

```grasp
path(src: x, dst: y) <- edge(src: x, dst: y)
path(src: x, dst: y) <-
    path(src: x, dst: z)
    edge(src: z, dst: y)
```

Mutual recursion — several relations mentioning each other — works the same way.
The compiler finds strongly connected components and compiles each to one
fixpoint; nothing is declared. See [`compilation.md`](compilation.md).

### External relations

A relation whose data comes from outside the program is declared with `input` as
its only body statement, and needs a spec:

```grasp
edge :: relation(src: i64, dst: i64)
edge(src:, dst:) <- input
```

All head columns take the shorthand form: the values come entirely from outside,
so there is nothing to bind them to. Such a relation may not also be defined by
ordinary rules, and may not take part in recursion.

This is the whole of `input` for now. The larger mechanism — where the body
constrains a *candidate space* and a runtime chooses from it — is future work,
described in [`overview.md`](overview.md#future-work).

## Body statements

A rule body is a **set of constraints, not a sequence of steps**. Every
statement must hold. The compiler chooses the evaluation order, so reordering a
body does not change the program, and a variable may be used by a statement
written above the one that binds it.

```
body_stmt ::= positive_atom | negated_atom | match | filter | type_assertion
```

### Positive atom

A relation name with key-value arguments. It binds its variables and constrains
its literals.

```grasp
person(name: x, age: a)
student(name:, enrolled: true)
```

`enrolled: true` is an equality constraint on that column. A variable appearing
in two atoms is a join between them. A variable appearing twice within one atom
constrains those columns to be equal.

**Each mention is its own occurrence.** Two atoms of the same relation are two
independent things, which is all a self-join needs:

```grasp
two_hop(src: x, dst: z) <-
    edge(src: x, dst: y)
    edge(src: y, dst: z)
```

### Negated atom

```grasp
not enrolled(student: s, course: c)
```

Rows with a match are discarded. Negation is **stratified**: the negated
relation must belong to a strictly lower stratum, so it is fully computed before
this rule runs. Negation inside a recursive component is rejected.

Every variable in a negated atom must be bound elsewhere in the body — a negated
atom constrains, it never binds. `_` matches any value without binding.

### Match

`:=` binds the left side to the value of the right side.

```grasp
total := price * quantity
n     := length(name)
```

**Aggregate.** The right side may be an aggregate over the rule's other
bindings, written with angle brackets:

```grasp
s := sum<sal>
c := count<>
m := max<p * q>
```

Aggregators are `sum`, `count`, `min`, `max` and `avg`. Grouping is implicit:
the group is the head's non-aggregate columns. Aggregation is **stratified** on
the same terms as negation.

**Unnest** turns one row into many:

```grasp
(v) := *arr           # one row per element
(k, v) := **d         # one row per dict entry
```

**Destructure** takes a value apart, and discards the row if the shape does not
match:

```grasp
[x, y] := arr          # exactly two elements
[x, y, *] := arr       # at least two, ignore the rest
[x, y, *r] := arr      # at least two, bind the rest to r

{name: n} := d         # exactly these keys
{name: n, **} := d     # at least these, ignore the rest
{name: n, **e} := d    # at least these, bind the rest to e

record(a: x) := s      # exactly these fields
record(a: x, **) := s  # at least these, ignore the rest
record(a: x, **rest) := s
```

All three are exact by default; `*` or `**` makes them partial. For records the
type of `rest` is computed by subtracting the named fields from the record type.

### Filter

A standalone boolean expression. The row survives only if it is true.

```grasp
a > 20
a >= 18 and c = "NY"
length(t) >= 3
```

A function call is never a body statement on its own — at statement level,
`name(` always begins an atom. Calls appear inside expressions.

### Type assertion

`v :: T` states a variable's type. It is a compile-time check where the type is
already known to fit, and a runtime filter where it narrows — `n :: string` on
an `optional(string)` drops the rows where the value is absent.

```grasp
n :: string
```

Assertions may appear anywhere in the body; they are collected and checked
together.

## Expressions

### Operators

| group | operators | associativity |
|---|---|---|
| arithmetic | `*`, `/`, `%` then `+`, `-` | left |
| concatenation | `++` | left |
| comparison | `=`, `!=`, `<`, `<=`, `>`, `>=` | none |
| boolean | `not` then `and` then `or` | left |
| unary | `-`, `not` | prefix |

Within a group, precedence is as listed. **Between** groups, only these
relations are declared:

```
arithmetic    < comparison        a + b < c    →  (a + b) < c
concatenation < comparison        x ++ y = z   →  (x ++ y) = z
comparison    < boolean           x < 3 and y  →  (x < 3) and y
```

**Every other combination is a parse error**, and must be parenthesised. So
`a ++ b + c` and `a + b and c` do not parse. A comparison cannot chain:
`a < b < c` is an error, not `(a < b) < c`.

The same applies to unary operators adjacent to a binary one of another group:
`not a and b` and `-a * b` are errors; write `(not a) and b` and `(-a) * b`.

This is deliberate, and stricter than most languages. Precedence between
unrelated operator families is a thing people remember wrong, so grasp declines
to have an opinion and asks instead. The rule matters more than the current
table: the operator groups that make it earn its keep — bitwise and shift — are
not in this cut, because grasp-dbsp has no operators to lower them to. When they
arrive, they arrive as incomparable groups, and no existing program changes
meaning.

**Equality is `=`.** The corpus this is ported from spelled it `=` in some
places and `==` in others; this settles it as `=`. `:=` is binding, so there is
no ambiguity to protect against. grasp-dbsp spells the same operator `==` — the
compiler translates.

### Literals

| kind | examples |
|---|---|
| integer | `42`, `0` |
| float | `3.14`, `1.0` |
| string | `"hello"`, `"esc\"aped"` |
| boolean | `true`, `false` |
| absence | `NONE` |

Numbers are unsigned tokens; `-5` is unary minus applied to `5`. An untyped
numeric literal takes its type from context.

`NONE` denotes absence. Its type is `optional(T)` for whatever `T` the context
requires. The Erlang implementation spells this `ABSENT`; grasp-dbsp spells it
`NONE`, and so does grasp.

```grasp
x = NONE
y != NONE          # the usual way to keep only present values
```

### Compound literals

```grasp
[1, 2, 3]                        # array
{name: "Alice", age: 30}         # dict
record(id: 1, name: "Alice")     # record
```

A `[` directly after an expression is a subscript, not an array literal.

### Function calls

```grasp
length(t)
concat(a, b)
if(x > 0, x, 0)
```

Positional arguments come first, keyword arguments after. A call is
distinguished from an atom by position — calls only occur inside expressions —
and by its first argument: `name:` after the paren means an atom.

### Field access and subscript

`s.field` reads a record field. It chains, and binds tighter than any operator,
left-associatively — `s.a.b` is `(s.a).b`.

```grasp
s.a
s.a.b
```

Subscript (`arr[0]`, `d["key"]`, `arr[1:5]`) is **not in this cut** — grasp-dbsp
has no array element access yet. See [`overview.md`](overview.md#future-work).

## Types

### Value types

```
type ::= "boolean" | "i64" | "f64" | "string" | "json"
       | "optional" "(" type ")"
       | "record" "(" field ("," field)* ")"
       | "array" "(" type ")"
       | "dict" "(" type "," type ")"

field ::= name ":" type
```

That is the whole vocabulary, and it is exactly what
[grasp-dbsp](../grasp-dbsp/language.md#value-types) can carry — with one
exception noted below. The wider set the Erlang implementation offers is listed
under [future work](overview.md#future-work).

| type | meaning |
|---|---|
| `boolean` | `true` or `false` |
| `i64` | signed 64-bit integer |
| `f64` | 64-bit IEEE float |
| `string` | UTF-8 text |
| `optional(T)` | a `T`, or absent |
| `record(f: T, …)` | named fields, each with its own type |
| `array(T)` | a sequence of one element type |
| `dict(K,V)` | a key-value map |
| `json` | a document of any shape |

**`optional` does not nest.** `optional(optional(T))` is not a distinct type and
is rejected, following grasp-dbsp.

**Record field order is not part of the type.** `record(a: i64, b: string)` and
`record(b: string, a: i64)` are one type.

**`dict(K,V)` is blocked.** It is specified here, but grasp-dbsp has no dict type
yet, so nothing that uses one can be emitted. It is called `dict` rather than
`map` because `map` is an operator name in grasp-dbsp and one word should not be
both.

**`record`, not `struct`.** The Erlang implementation calls this `struct(...)`
and namespaces its operations `struct:`. grasp-dbsp calls it `record(...)`, and
so does grasp.

**`json`** is a document with no structure the type system describes — that is
the point of it. `get` reaches inside one and `cast` converts one out; there is
no pattern language.

### Relation types

```
relation(col: T, …)
```

A relation is a set of tuples with named, typed columns. It is a distinct thing
from a value type: relations are what rules define, and they cannot nest inside
values.

### Inference

Column types are inferred. The compiler seeds from specs and literals, then
iterates: a rule whose body atoms all have known relations gives types to its
variables, and those give the head's columns their types, which feed the next
round. Recursive relations settle in the same iteration.

An annotation is needed only where this cannot reach — which is what the `::`
spec and the `v :: T` assertion are for. Adding one is checked against what is
inferred rather than overriding it.

The details, including the phases and what each can fail on, are in
[`compilation.md`](compilation.md).

## Builtins

Every builtin lowers to a grasp-dbsp builtin, so the set is exactly what
grasp-dbsp provides. It is small, and it grows there first.

| grasp | on | notes |
|---|---|---|
| `abs`, `floor`, `ceil`, `round` | numbers | |
| `length` | `string`, `array(T)` | |
| `concat` | `string` | also written `++` |
| `lower`, `upper`, `trim` | `string` | |
| `coalesce(x, d)` | `optional(T)`, `T` | `x` if present, else `d` |
| `if(c, a, b)` | `boolean`, `T`, `T` | |
| `get(d, k)` | `json` | returns `optional(json)` |
| `keys(d)` | `json` | object keys |

Aggregators — `sum`, `count`, `min`, `max`, `avg` — are written `agg<expr>`
rather than called, and match grasp-dbsp's set exactly.

Namespaced spellings (`string:length`, `agg:sum`) are reserved for when the
library grows past the point where bare names are workable, and for the
type-specific namespaces that arrive with their types.

## Reserved words

Type names — `boolean`, `i64`, `f64`, `string`, `json`, `optional`, `record`,
`array`, `dict`, `relation` — and the keywords `not`, `and`, `or`, `true`,
`false`, `NONE`, `input`. Aggregator names — `sum`, `count`, `min`, `max`,
`avg` — may not name a relation.

A name is either a relation or a callable, never both.

## Example

```grasp
edge :: relation(src: i64, dst: i64)
emp  :: relation(name: string, dept: i64, sal: i64)
dept :: relation(id: i64, title: string)

edge(src:, dst:) <- input
emp(name:, dept:, sal:) <- input
dept(id:, title:) <- input

# Transitive closure — recursive, and nothing says so.
path(src: x, dst: y) <- edge(src: x, dst: y)
path(src: x, dst: y) <-
    path(src: x, dst: z)
    edge(src: z, dst: y)

# A cycle is a node reachable from itself.
cyclic(node: x) <- path(src: x, dst: x)

# Join, filter, negation and aggregation in one rule.
payroll(title: t, total: s) <-
    emp(name: n, dept: d, sal: r)
    dept(id: d, title: t)
    r >= 150
    not terminated(emp: n)
    s := sum<r>

# A binding and an unnest.
positive(val: x) <-
    data(items: arr)
    (x) := *arr
    x > 0
```
