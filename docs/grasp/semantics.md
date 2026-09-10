# grasp — Semantics

What a program means. The forms are in [`syntax.md`](syntax.md), their types in
[`types.md`](types.md), and how they are compiled in
[`compilation.md`](compilation.md).

## Programs and relations

A program is a set of **specs** and **rules**. A rule derives tuples for one
relation; several rules may derive for the same one. A relation's value at a
transaction is every tuple derivable by any of its rules, and nothing else —
[`Time`](#time) says what a transaction is and why the value has one.

**Relations are sets.** A tuple derived twice — by two rules, or by two ways
through one rule, or asserted twice as a fact — appears once. This is the one
place grasp and its target differ by default: grasp-dbsp's `plus` adds weights
and its `constant` sums the weights of identical rows, so
[`mapping.md`](mapping.md#rules-and-unions) emits a `distinct` to restore set
semantics. It lands in three places: at the relation, at an
[input](#time) — where the outside world writes the weights — and wherever else
multiplicity would be *observable*, which is exactly an
[aggregate](#aggregation).

**Order does not exist.** Rules may be written in any order, body statements may
be written in any order, and neither changes the program. A relation has no row
order, and nothing observes one.

## Time

A program does not run once. It runs against a sequence of **transactions**: in
each one, rows arrive from outside, every relation is brought up to date
together, and the results are reported. The logical clock advances between
transactions and not within them, so a transaction is the unit in which a
program's answer is well defined.

**A row arrives with a weight, and a transaction is unordered.** `+1` asserts a
row, `-1` retracts it. A transaction may carry several weights for one row; they
are summed, so `+1, -1, +1` and `+1` are the same transaction. That is the same
"order does not exist" as above, applied to data instead of to statements.

**An input relation is the rows whose accumulated weight is positive**, summed
over every transaction so far. So `+1, +1, -1` leaves the row in — the total is
still one. `-1, -1, +1` leaves it out, and so does the next `+1`: the total is
zero, and only the one after that puts the row in. A retraction of a row never
asserted removes nothing and is not an error, but it does leave a debt, and
assertions that merely pay it off produce nothing. This is "relations are sets"
applied to data arriving over time rather than to two rules deriving one tuple,
and it is the same `distinct` that enforces both.

**Everything derived follows.** A relation's value *at a transaction* is the
least fixpoint of its rules over the input relations as of that transaction.
Recursion, negation and aggregation all read the relations of the transaction
they are in; nothing reads across one.

**What is observable is change.** Running a transaction reports, per relation,
which rows entered and which left since the previous one — not the relation's
contents. A program that wants the contents accumulates the changes. A row that
enters and leaves within one transaction is reported neither way.

Facts are unaffected by all of this: they are program text, delivered once, and
[cannot be retracted](#facts).

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
`string:length("abc")` are facts too. They cannot be anything else: a variable there
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

What `<- input` supplies is a stream of weighted rows, and the relation is the
rows whose accumulated weight is positive — see [`Time`](#time).

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
n     := string:length(name)
```

A match binds, so it satisfies [safety](#safety) for the variable on its left.

### `v :: T` asserts

An assertion states a variable's type. It is a **compile-time check** where the
value is already known to be a `T`, and a **runtime filter** where a check could
settle it — dropping the rows that do not match and making `v` a `T`. Which one
it is depends on the tables in [`types.md`](types.md#runtime-filters).

```grasp
named(name: n, len: k) <-
    person(name: n)      # n : optional(string)
    n :: string          # drops the absent rows
    k := string:length(n)  # n is a string here
```

**It narrows for the whole rule, not for what follows it.** A body is a set, so
an assertion is a claim about the rule; the three statements above may be
written in any order and mean the same thing. That is the same rule everything
else in a body obeys, and typespecs are not the exception they look like.

Two assertions on one variable are one claim, composed. Two that cannot compose
are a mistake, since neither is then the truth.

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
- **Operators and calls** compute; the set is in [the standard library](#the-standard-library), and
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

**An aggregate stands in two places and no others**: the right-hand side of a
match, and a head argument. The second is sugar for the first —

```grasp
payroll(dept: d, total: sum<r>) <-
    emp(dept: d, sal: r)
```

means the rule above it, because `total:` already means `total: total` and this
is that shorthand applied one step further: the column names the variable. So a
head that already binds `total` in its body is the collision the
[aggregate scope rule](inference.md#aggregate-scope-checked-here) reports, and a
column whose name a variable may not have — `sum:` — is refused for the same
reason the shorthand refuses it.

An aggregate is not an expression. It cannot appear in a filter, in an atom's
argument (which is one of the things it folds), or in a fact (which has no body
to fold).

**Grouping is implicit: the group is the head's non-aggregate columns** — the
*variables* those columns read, rather than the columns themselves. The
difference shows: `q(tag: string:length(d), total: s)` groups by `d`, while binding
`t := string:length(d)` first and writing `q(tag: t, …)` groups by `t`, so two
departments whose names are the same length give two rows in the first and one
in the second. Above,
`dept` — so one row per department. This is what makes aggregation read like the
rest of the language: there is no `group by`, because the head already says what
the result is keyed by.

**An aggregate ranges over the set of satisfying assignments to the rule's body
variables.** Not over derivations, and not over rows of any relation: an
assignment gives every variable the body binds a value, and two assignments are
the same when they agree everywhere.

That settles the two questions "rows in the group" leaves open. A column an atom
does not name binds nothing, so `r(b: 1)` over `r(b, c)` is *there is some `c`* —
a proposition, which holds or does not, and cannot hold three times. And a
variable that *is* bound but read nowhere else is still part of the assignment,
so two employees differing only in a `name` the rule never uses are two
assignments, and their salaries are summed twice.

The consequence is worth stating outright, because it surprises. There is no way
to say "once per row of `emp`" except to name enough columns to tell the rows
apart — and `_` is the existential written down, so it does not help:

```grasp
# distinct salaries: `dept` is not named, so it is existential
total(all: s) <-
    emp(sal: r)
    s := sum<r>

# per (dept, salary): naming `dept` tells the rows apart
total(all: s) <-
    emp(dept: d, sal: r)
    s := sum<r>
```

This is where grasp parts company with SQL, whose row identity is every column
of every table named in `from`, so SQL's answer to the first is the second's.
grasp's reading is the one that composes: under SQL's, adding a column to a
relation's spec silently changes the answer of every aggregate rule that does
not mention it.

- **The argument may be an expression**: `sum<p * q>` aggregates the product.
  An expression with no answer for some assignment — `sum<a / b>` where `b` is
  zero — derives no row for it, so that assignment is not in the group at all,
  and a second aggregate in the same rule does not see it either.
- **`count<>` takes no argument** and counts the assignments in the group. Over
  an unnest that means *distinct* elements: `(t) := *["x", "x"]` binds `t` to
  `"x"`, once, however many elements produced it. `array:length(arr)` is what counts
  elements.
- **Several aggregates in one rule** share the group: `min<r>` and `max<r>`
  together give one row per group with both.
- **An empty group produces no row.** A group exists because some row is in it;
  there is no set of keys to produce zeros against. A count that must show zero
  needs a second rule supplying it for the keys with nothing to count — which is
  an ordinary negated atom, and says what it means better than a default would.
- **`count` is the only aggregator whose result type is fixed.** It is an `i64`;
  every other one gives back its argument's type, `avg` included. So the mean of
  `i64`s is an `i64`, and integer division **truncates toward zero**: the mean of
  `-1` and `-2` is `-1`, not `-2`. That is the direction `/` takes everywhere
  else in the language, which is the reason to prefer it.
- **`sum` and `avg` need a numeric argument**, and **`min` and `max` need a
  scalar one** — the same rule `<` obeys, for the same reason:
  [ordering](types.md#comparison-and-ordering) is defined on scalars only, and any invention
  would be arbitrary in a way that silently decides which row wins.
- **An `optional` argument is looked through.** Absence is skipped rather than
  folded, and the wrapper survives into the result: `sum` over an
  `optional(i64)` is an `optional(i64)`, absent only for a group with nothing to
  add. `avg` averages the values that are there, over how many there were rather
  than how many assignments the group has.

  That group is not the empty one. It has assignments; none of them has a value,
  so it reports with every aggregate absent — where an empty group produces no
  row at all. And since the aggregates share one row, they cannot disagree about
  whether it exists.

**Where an aggregate's result has a value, only the group does too.** An
aggregate folds a whole group into one value, so a variable that varies within
the group has no value beside it: `s > r` is refused rather than answered, and
so is anything else that reads an aggregate result together with a variable
outside the group. [`inference.md`](inference.md#aggregate-scope-checked-here)
states the four shapes this rejects and the one workaround it needs.

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

> ``` `p` and `q` are mutually recursive, and `q` is negated at line N — negation
> cannot cross a recursive cycle ```

The strata are also the evaluation order: everything in a lower stratum is
finished before a higher one starts, which is what lets a negated atom be read
as a completed set. [`compilation.md`](compilation.md#scc-analysis) uses the same
components to decide what becomes a fixpoint.

## Desugaring

Several surface forms stand for others. Desugaring happens after parsing and
before the join graph, so everything downstream sees the smaller core.

| written | means |
|---|---|
| `x:` in an atom, a head or a pattern | `x: x` |
| `x: agg<e>` in a head | `x: x`, and `x := agg<e>` in the body |
| `a ++ b` | `string:concat(a, b)` |
| `{a: v}` | `{"a" => v}` |
| `{"a": v}` | `{"a" => v}` |
| `s.f` | `record:get(s, field: "f")` |
| `s.a.b` | `record:get(record:get(s, field: "a"), field: "b")` |
| `not e` in an expression | `boolean:not(e)` |

The **patterns** stand for a binding plus the checks that make the pattern
exact. Each generated check is an ordinary filter, so the optimizer places it
like any other. Written in the shorthand, since that is how they are written.

| written | means |
|---|---|
| `[x, y] := arr` | `array:length(arr) = 2`, `x := array:at(arr, index: 0)`, `x :: E`, and so on |
| `[x, y, *] := arr` | `array:length(arr) >= 2`, then the two bindings |
| `[x, y, *r] := arr` | as above, and `r` is the elements from position 2 on |
| `{a:} := d` | `dict:length(d) = 1`, `a := dict:get(d, key: "a")` |
| `{a:, **} := d` | `a := dict:get(d, key: "a")` — no size check |
| `{a:, **e} := d` | as above, and `e := dict:without_keys(d, ["a"])` |
| `record(a:) := s` | `a := record:get(s, field: "a")`; `s`'s type must have exactly that field |
| `record(a:, **) := s` | `a := record:get(s, field: "a")` — extra fields allowed |
| `record(a:, **e) := s` | as above, and `e` is a record of the fields left |

`dict:get` has no answer for a key that is not there, so the row is not derived
— nothing has to be unwrapped, and that is the exactness the pattern promises.

**A dict and a record are exact in different senses**, and the difference is
worth stating because both spellings look alike. A dict's size is *data*, so
`{a:, b:} := d` is a filter and a row whose dict has three entries is simply not
derived — the ordinary [body-has-an-answer](#a-body-must-have-an-answer) case. A
record's fields are its *type*, so `record(a:) := s` over a two-field `s` is a
**compile error**; there is no row to drop, because every `s` has the same shape.
That is also why `**e` works over a record and not over a dict: a record's
remaining fields are known before the program runs, so the remainder is a literal
built from them, while a dict's would need entries subtracted at runtime.

A pattern's fields are named rather than positional, so `{a:, b:}` and `{b:, a:}`
are one pattern, and the compiler puts them in one order before anything reads
them.

The expansion happens **after** inference rather than in desugaring, which is
where the rest of this section's table is performed. Two of the three parts need
types: the narrowing after a dict `get` has to name `V`, and a record remainder
has to know which fields are left. So the patterns reach
[`inference.md`](inference.md#where-a-constraint-comes-from), which already gives
their typing rules, and are expanded once it has answered.

The **unnests** do not desugar to filters — they are generative, and become a
`flat_map` in the computation DAG:

| written | means |
|---|---|
| `(v) := *arr` | one row per element of `arr`, with `v` bound to it |
| `(i, v) := *arr` | as above, and `i` is the element's 0-based position |
| `(k, v) := **d` | one row per entry of `d`, with `k` and `v` bound |

`(k, v) := **d` iterates entries **in key order**, because a dict is stored
sorted — so the rows it produces are the same set every time, which is what a
relation requires.

There are three shapes and no others: a value, an index and a value, or a key
and a value. The marker says which container is being taken apart and the arity
says what is wanted from it, so `(k) := **d` is not a shorter dict unnest but a
mistake — write `(k, _v)`.

An index is an `i64` and an ordinary operand. It also tells equal elements
apart: `["x", "x"]` unnests to one row without one and to two rows with it,
which is the [set](#programs-and-relations) rule doing what it always does
rather than an exception to it.

Every row above works. The two remainders differ in *when* they are known, which
is why they became different things: a record's fields are fixed by its type, so
`**e` there is a literal built at compile time, while a dict's entries are data,
so `**e` there is a subtraction the program performs.

**An array pattern is positional**, so its variables are not reordered and
`[x, y]` and `[y, x]` are two patterns — unlike a dict's or a record's, which
name what they take and are one pattern in any order. Its size check is `=` or
`>=` where a dict's is only `=`, and both are filters: an array's length is data,
so a row whose array is the wrong length is simply not derived.

## The standard library

**Every function is namespaced, and the namespace is the type family it belongs
to.** `string:length` and `array:length` are two functions, not one asked to
guess from its argument — so every name is monomorphic, and type-based
overloading is not something the language has to have.

`integer:` and `float:` do not merge into a `number:`. Float semantics are not
integer semantics, and `numeric` — arbitrary-precision decimal, when it arrives —
is a third thing again. `float:floor` exists and `integer:floor` does not, being
the identity on an `i64`.

The library is [`stdlib.grasp`](stdlib.grasp), written in grasp and parsed as
grasp, and it declares the **whole** library rather than the part that works.
Calling a function this compiler has not got is `not implemented: <name>` rather
than "there is no callable": the language has the function, and what is left of
it is a row of the test suite's burn-down rather than a promise in a document
nobody counts.

One callable is not in it and cannot be. `record:get(r, field: "f")`, what `r.f`
desugars to, gives back the *named field's* type — which depends on the value of
an argument rather than on its type, so no signature says what it returns.
Inference reads the field literal instead. The file declares what it can rather
than declaring a lie.

What follows is the shape the library takes.

**Access by position is `at`; access by key is `get`.** `array:at(a, index: 2)`,
`dict:get(d, key: k)`, `record:get(r, field: "f")`.

**A partial function derives no row.** `dict:get` on a missing key and
`array:at` past the end have no answer, and [a rule derives a row only where
every step of its body has one](#a-body-must-have-an-answer) — the same rule
division and every destructure obey. Absence is something to ask about,
`dict:has`, rather than something to unwrap at every call.

**The subject is positional and everything else is a keyword.**
`array:slice(a, start: 0, stop: 3)`, `dict:without(d, keys: ks)`. A binary
operation over one type stays positional: `string:concat(a, b)`.

**Overloads are by shape, not by type.** A name may have several variants, and
the one a call means is settled by [its shape](syntax.md#resolving-a-call) — the
arguments given by position and the set of keyword names given — before any type
is looked at. That is what lets `array:slice` be eight variants over `start:`
`stop:` `step:` rather than one signature with a defaulting rule, and it is the
other half of why namespacing keeps every name monomorphic: two functions over
different types are two names, and two ways of calling one function are two
shapes.

**Operators are sugar for functions.** `a ++ b` is `string:concat(a, b)`, `not e`
is `boolean:not(e)`, `r.f` is `record:get(r, field: "f")`, `d[k]` is
`dict:get(d, key: k)`. A reader who knows the library knows the operators.

**There is no `if`.** A conditional does not belong in a Datalog: a rule that
holds for some rows and not others is what a rule *is*, so the answer is two
rules with complementary filters. Nor is there a `json:` family — a document is
narrowed to what it holds, `d :: dict(string, json)`, and read with `dict:`.

Two things grasp-dbsp has are deliberately not here: **an explicit conversion**
(`cast(x, T)`) and **a default for an absent value** (`coalesce(x, d)`). A
document is read by the [runtime filter](types.md#runtime-filters) `v :: T`, and
an absent value is *dropped* by that same filter rather than defaulted — which is
the whole shape of [a body having an answer](#a-body-must-have-an-answer), and
the reason `coalesce` reads as the odd one out even though it is ordinary below.
What is genuinely missing is an arbitrary change of type. It is wanted; it will
not arrive by copying grasp-dbsp's, which is how `optional` division briefly got
in.

The namespaces are held whether or not anything lives in them yet: `string`,
`array`, `dict`, `record`, `json`, `boolean`, `integer`, `float`, `numeric`,
`bytes`, `bits`, `temporal`, `crypto`, `agg`. A name under one is a callable and
never a relation, which is what lets body-statement position tell an atom from a
filter without a lookahead.

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
