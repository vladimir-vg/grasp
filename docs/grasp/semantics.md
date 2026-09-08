# grasp — Semantics

What a program means. The forms are in [`syntax.md`](syntax.md), their types in
[`types.md`](types.md), and how they are compiled in
[`compilation.md`](compilation.md).

## Programs and relations

A program is a set of **specs** and **rules**. A rule derives tuples for one
relation; several rules may derive for the same one. A relation's value is every
tuple derivable by any of its rules, and nothing else.

**Relations are sets.** A tuple derived twice — by two rules, or by two ways
through one rule — appears once. This is the one place grasp and its target
differ by default: grasp-dbsp's `plus` adds weights, so
[`mapping.md`](mapping.md#rules-and-unions) emits a `distinct` to restore set
semantics.

**Order does not exist.** Rules may be written in any order, body statements may
be written in any order, and neither changes the program. A relation has no row
order, and nothing observes one.

## Facts

A relation name with arguments and no body asserts one tuple.

```grasp
edge :: relation(src: i64, dst: i64)
edge(src: 1, dst: 2)
```

The spec is not optional here. A fact's arguments are closed, so `1` has nothing
beside it to take a type from and [`inference.md`](inference.md#phase-2-across-rules)
reports it rather than defaulting — unless another rule for `edge` settles the
column instead.

The arguments are **closed expressions** — usually literals, but `1 + 1` and
`length("abc")` are facts too. They cannot be anything else: a variable there
would be one the head uses and the body does not bind, which
[safety](#safety) already rejects, and the shorthand `edge(src:)` means
`edge(src: src)` and is rejected the same way.

So a fact is computed once, when the program is compiled, and is thereafter part
of the relation for as long as the program runs. Facts cannot be retracted —
they are program text, not data.

A relation is defined by the program or comes from outside, not both: a relation
with an `<- input` rule may not also have facts.

> ``relation `edge` has both an input rule and facts``

## Rules

A head, `<-`, and a body of statements that must all hold. Free variables are
implicitly universally quantified: the rule derives one head tuple for every
assignment of variables that satisfies every body statement.

```grasp
result(name: n, title: t) <-
    student(id: s, name: n, age: a)
    a > 20
    course(id: c, title: t)
    not enrolled(student: s, course: c)
```

A variable appearing in two atoms **joins** them. A variable appearing twice in
one atom constrains those two columns to be equal. A literal in an argument
position constrains that column to equal it.

### Shorthand and the wildcard

`x:` means `x: x`. `_` matches any value and binds nothing, so two `_` in one
atom are unrelated and neither can be referred to.

```grasp
leaf(name: n) <-
    node(name: n)
    not parent(child: n, parent: _)
```

### Unions

Several rules for one relation contribute together — this is disjunction, and
the only form of it. There is no `or` between body statements; write two rules.

```grasp
parent(child: x, parent: y) <- father(child: x, parent: y)
parent(child: x, parent: y) <- mother(child: x, parent: y)
```

### Recursion

A relation that mentions itself is recursive. Nothing declares it.

```grasp
path(src: x, dst: y) <- edge(src: x, dst: y)
path(src: x, dst: y) <-
    path(src: x, dst: z)
    edge(src: z, dst: y)
```

The meaning is the **least fixpoint**: the smallest relation satisfying every
rule. Because the rules below are monotone in the recursive relation — negation
and aggregation cannot reach into a cycle, per stratification — that fixpoint
exists and is unique, and iterating from empty reaches it.

Mutual recursion, where several relations mention each other, is the same thing
over a set of relations.

### External relations

```grasp
edge :: relation(src: i64, dst: i64)
edge(src:, dst:) <- input
```

`input` must be the only body statement, every head column takes the shorthand
form, and the relation must have a spec — nothing else can say what its columns
are. Such a relation may not also be derived by rules, and may not take part in
recursion.

> ``relation `r` is both an input and derived by a rule``

## Matches, assertions and expressions

### `v := expr` binds

A match computes `expr` from variables already bound and binds the result to
`v`. It is not assignment and not a constraint: `v` is a new variable, and
writing the same `v` twice means both computations must agree — the second
becomes an equality check, which is what
[`compilation.md`](compilation.md#resolving-competing-producers) resolves.

```grasp
total := price * quantity
n     := length(name)
```

A match binds, so it satisfies [safety](#safety) for the variable on its left.

### `v :: T` asserts

An assertion states a variable's type. It is a **compile-time check** where the
value is already known to be a `T`, and a **runtime filter** where a check could
settle it — dropping the rows that do not match and narrowing `v` to `T` for
everything after it. Which one it is depends on the tables in
[`types.md`](types.md#runtime-filters).

```grasp
named(name: n) <-
    person(name: n)      # n : optional(string)
    n :: string          # drops the absent rows; n : string below
```

An assertion **binds nothing** — the variable must already exist — and where no
check could ever pass it is an error rather than a silently empty relation.

### Expressions

Expressions compute values from bound variables. They appear on the right of a
match, inside a filter, and as an atom's argument.

- **Literals** are values: `42`, `"text"`, `true`, `NONE`.
- **`[a, b]`** builds an array; **`record(f: e)`** builds a record. Both are
  ordinary values — a column can hold one, and `flat_map` turns an array into
  rows.
- **A dict** is built either way: `{k => v}` takes expression keys, so any key
  type, and `{name: v}` takes a bare string key. The second is sugar for the
  first, and the two may be mixed.
- **`e.f`** reads a record field, and chains.
- **Operators and calls** compute; the set is in [Builtins](#builtins), and
  their precedence in [`syntax.md`](syntax.md#operator-groups).

An empty `[]` or `{}` is a complete value with an open type — an empty array is
one value whatever its elements would have been — so its type comes from
context, exactly as `NONE`'s does. See
[`inference.md`](inference.md#phase-3-literals-take-their-type-from-context).

A dict literal's entries are **sorted by key and deduplicated**, so two literals
naming the same entries in different orders build one value, and a key written
twice keeps the last — whichever spelling wrote it.

## A body must have an answer

A rule derives a row only where **every step of its body has an answer**. Where
an operation has none for the values it was given, the row is not derived, and
nothing is reported: there is no error to raise, because a rule that holds for
some rows and not others is what a rule is.

Division is the only operation that currently has none — a zero divisor — so
`q := a / b` derives rows for the divisors that are not zero and no others.

This is not a new mechanism. It is what a filter does, what
[`v :: T`](types.md#runtime-filters) does when the value is not a `T`, and what
the size checks a [destructure](#desugaring) expands into do. Those are written
down and this one is not, which is the only difference between them.

grasp-dbsp does the opposite, deliberately: it gives division the type
`optional(T)` so that what can be missing says so. It is a compilation target
and being explicit costs it nothing, while grasp is written by people and a
Datalog rule that quietly holds for fewer rows is the ordinary case rather than
a surprise. [`mapping.md`](mapping.md#narrowing-and-dropping) is where the two
are reconciled.

## Safety

A rule is **safe** when every variable it uses is *bound* — produced by a
positive atom or by a match. Only those two bind; negated atoms, filters and
assertions constrain variables that must already exist.

Specifically:

- every variable in the **head** must be bound in the body;
- every variable a **negated atom** mentions must be bound by a positive atom or
  match elsewhere in the body;
- every variable a **filter** or **assertion** mentions must be bound.

> ``variable `x` appears in the head but nothing in the body binds it``
> ``variable `x` appears only in a negated atom, which binds nothing``

Safety is what makes a rule **finite**. An unbound head variable would assert
tuples for every value of a type — infinitely many for `i64` or `string`. An
unbound variable in a negation asks for tuples that do not match *anything*,
which is the same infinity. The check is not conservatism; there is no answer to
compute.

The `input` rule is the deliberate exception: its head variables are bound from
outside, which is what `input` means.

### A body need not contain an atom

Only positive atoms and matches bind, and a rule using nothing but matches is
safe:

```grasp
answer :: relation(v: i64)

answer(v: n) <-
    n := 6 * 7
```

The spec is not decoration: `6 * 7` is two literals with nothing beside them to
take a type from, and [`inference.md`](inference.md#phase-3-literals-take-their-type-from-context)
reports that rather than defaulting. A rule grounded on nothing has nothing to
infer from either, so it says what it derives.

Such a rule derives exactly one tuple, once — it depends on no relation, so
there is nothing that could make it derive another. A fact is the degenerate
case of the same thing, with the computation moved into the head.

The optimizer roots a rule's evaluation at a positive atom, so a body with none
is given one: an occurrence of the **unit relation**, which has no columns and
holds exactly one tuple. That is the whole of the special case, and the join
graph builder is where it happens, so nothing downstream has one —
[`compilation.md`](compilation.md#grounding-a-body-with-no-atom) says where.

The unit relation is not writable. It has no name in grasp and a program cannot
mention it; a relation of your own with no columns is an ordinary proposition,
described in [`types.md`](types.md#a-relation-with-no-columns).

## Negation

```grasp
not enrolled(student: s, course: c)
```

Rows with a match are discarded. Negation is **stratified**: the negated
relation must be fully computed before this rule runs, so it must live in a
strictly lower stratum. Negation inside a recursive component is rejected.

A relation with no columns is a **proposition** — it holds the empty tuple or
nothing — so an atom over one neither binds nor constrains any variable. It is a
guard:

```grasp
ready :: relation()
ready() <- config(mode: "on")

active(id: i) <-
    ready()
    account(id: i)
```

Every row passes while `ready` holds and none passes when it does not, and
`not ready()` is the other way round. Because such an atom shares no variable
with anything, it is always its own component of the join graph — see
[`compilation.md`](compilation.md#the-weighted-join-graph).

Stratified negation has a definite meaning where unrestricted negation does not.
`p <- not p` has no least fixpoint — neither `p` empty nor `p` full satisfies it
— so the language forbids the shape rather than picking one of the answers.

## Aggregation

```grasp
payroll(dept: d, total: s) <-
    emp(dept: d, sal: r)
    s := sum<r>
```

Aggregators are `sum`, `count`, `min`, `max` and `avg`.

**Grouping is implicit: the group is the head's non-aggregate columns.** Above,
`dept` — so one row per department. This is what makes aggregation read like the
rest of the language: there is no `group by`, because the head already says what
the result is keyed by.

- **The argument may be an expression**: `sum<p * q>` aggregates the product.
- **`count<>` takes no argument** and counts rows in the group.
- **Several aggregates in one rule** share the group: `min<r>` and `max<r>`
  together give one row per group with both.
- **An empty group produces no row.** A group exists because some row is in it;
  there is no set of keys to produce zeros against. A count that must show zero
  needs a rule supplying the keys and a `coalesce`.
- **`avg` yields `f64`** whatever it was given, since a mean is not an integer.
  Every other aggregator yields its argument's type.

Aggregation is **stratified on the same terms as negation**: the aggregated
relation must be in a strictly lower stratum. An aggregate over a relation still
being computed would read a partial value, and which partial value would depend
on evaluation order.

> ``aggregate over `r` is in the same recursive component as this rule``

## Stratification

The check both negation and aggregation rest on.

```
1. Build the dependency graph over relations: an edge r → s when a rule
   for r has s in its body. Mark the edge NEGATIVE when s appears under
   `not`, or is the subject of an aggregate.

2. Find the strongly connected components. Each is a set of mutually
   recursive relations; a relation in no cycle is its own component.

3. Condense: the graph of components is acyclic by construction.

4. Assign strata by topological order — a component's stratum is one more
   than the highest of its dependencies'.

5. Reject if any NEGATIVE edge is internal to a component.
```

Step 5 is the whole point. A negative edge inside a component is a relation
whose definition depends on the *absence* of something not yet computed.

> ```p` and `q` are mutually recursive, and `q` is negated at line N — negation
> cannot cross a recursive cycle``

The strata are also the evaluation order: everything in a lower stratum is
finished before a higher one starts, which is what lets a negated atom be read
as a completed set. [`compilation.md`](compilation.md#scc-analysis) uses the same
components to decide what becomes a fixpoint.

## Desugaring

Several surface forms stand for others. Desugaring happens after parsing and
before the join graph, so everything downstream sees the smaller core.

| written | means |
|---|---|
| `x:` in an atom or head | `x: x` |
| `a ++ b` | `concat(a, b)` |
| `{a: v}` | `{"a" => v}` |
| `{"a": v}` | `{"a" => v}` |
| `s.f` | `record:get(s, "f")` |
| `s.a.b` | `record:get(record:get(s, "a"), "b")` |
| `not e` in an expression | `boolean:not(e)` |

The **patterns** desugar to a binding plus the checks that make the pattern
exact. Each generated check is an ordinary filter, so the optimizer places it
like any other.

| written | means |
|---|---|
| `[x, y] := arr` | `length(arr) = 2`, `x := arr[0]`, `y := arr[1]` |
| `[x, y, *] := arr` | `length(arr) >= 2`, then the two bindings |
| `[x, y, *r] := arr` | as above, and `r := arr[2:]` |
| `{a: x} := d` | `length(d) = 1`, `x := dict:get(d, "a")`, `x :: V` |
| `{a: x, **} := d` | `x := dict:get(d, "a")`, `x :: V` — no size check |
| `{a: x, **e} := d` | as above, and `e := dict:without_keys(d, ["a"])` |
| `record(a: x) := s` | `x := record:get(s, "a")`, `s`'s type must have exactly that field |
| `record(a: x, **) := s` | `x := record:get(s, "a")` — extra fields allowed |

`dict:get` yields `optional(V)`, so the `x :: V` assertion is what makes a
missing key drop the row rather than bind absence. That is the exactness the
pattern promises.

The **unnests** do not desugar to filters — they are generative, and become a
`flat_map` in the computation DAG:

| written | means |
|---|---|
| `(v) := *arr` | one row per element of `arr`, with `v` bound to it |
| `(k, v) := **d` | one row per entry of `d`, with `k` and `v` bound |

`(k, v) := **d` iterates entries **in key order**, because a dict is stored
sorted — so the rows it produces are the same set every time, which is what a
relation requires.

Two forms named above are not yet available: `arr[i]` and `arr[2:]` need array
element access, and `dict:without_keys` needs its builtin. Both are
[future work](overview.md#future-work), and the patterns that depend on them —
array destructure, and `**e` with a binding — are rejected until then.

## Builtins

Each of these is here because grasp wants it. That every one also lowers to a
grasp-dbsp counterpart is a property worth keeping, not the reason for the
contents — a library chosen by reading the target's is a library nobody chose.

| builtin | signature |
|---|---|
| `abs`, `floor`, `ceil`, `round` | `T → T`, `T` numeric |
| `length` | `string → i64`, `array(T) → i64`, `dict(K,V) → i64` |
| `concat` | `string × string → string` (also written `++`) |
| `lower`, `upper`, `trim` | `string → string` |
| `coalesce` | `optional(T) × T → T` |
| `if` | `boolean × T × T → T` |
| `keys` | `json → optional(array(string))`, `dict(K,V) → array(K)` |
| `entries` | `dict(K,V) → array(record(key: K, value: V))` |

Two things grasp-dbsp has are deliberately not here. **A computed dict lookup**
— `get(d, k)` — and **an explicit conversion** — `cast(x, T)` — are its
builtins, not grasp's. Reading a dict by a key written down is what the
`{a: x} := d` pattern is for, and extracting from a `json` is what the
[runtime filter](types.md#runtime-filters) `v :: T` is for. Neither has a grasp
spelling for a *computed* key or an arbitrary conversion, and neither will
until the case for one is made on grasp's own terms.

The namespaced spellings (`string:length`, `agg:sum`) are reserved for when the
library outgrows bare names, and for the type-specific namespaces that arrive
with their types. Some are already in use for what desugaring writes and a
program cannot: `record:get`, `dict:get` and `boolean:not` appear in the table
below and nowhere a person types.

## Example

```grasp
edge :: relation(src: i64, dst: i64)
emp  :: relation(name: string, dept: i64, sal: i64)
dept :: relation(id: i64, title: string)

edge(src:, dst:) <- input
emp(name:, dept:, sal:) <- input
dept(id:, title:) <- input

# Transitive closure. Recursive, and nothing says so.
path(src: x, dst: y) <- edge(src: x, dst: y)
path(src: x, dst: y) <-
    path(src: x, dst: z)
    edge(src: z, dst: y)

# A cycle is a node reachable from itself.
cyclic(node: x) <- path(src: x, dst: x)

# Join, filter, negation and aggregation in one rule. The group is `title`,
# the head's only non-aggregate column.
payroll(title: t, total: s) <-
    emp(name: n, dept: d, sal: r)
    dept(id: d, title: t)
    r >= 150
    not terminated(emp: n)
    s := sum<r>

# A dict built with the string-key form, and an unnest over another.
profile(name: n, info: i) <-
    person(name: n, age: a, city: c)
    i := {age: a, city: c}

tagged(name: n, tag: k) <-
    person(name: n, tags: d)
    (k, _v) := **d
```
