# grasp — Syntax

The surface form: what a program is made of, and the grammar that accepts it.
What the forms *mean* is [`semantics.md`](semantics.md); what their types are is
[`types.md`](types.md).

## Lexical structure

### Tokens

```
keywords     not  and  or  input  true  false  NONE
types        boolean  i64  f64  string  json  optional  record  array  dict
declarations relation  function
aggregators  sum  count  min  max  avg
operators    +  -  *  /  %  ++
comparison   =  !=  <  <=  >  >=
binding      :=
annotation   ::
arrow        <-
returns      ->
dict entry   =>
punctuation  (  )  [  ]  {  }  :  ,  .  _
patterns     *  **
```

`*` and `**` are the unnest prefixes on the right of `:=` and the rest-markers
in a destructure pattern. They are never binary operators — `*` as
multiplication is told apart by position, since a pattern marker only ever
follows `:=` or sits inside a bracket pattern.

### Identifiers

```
identifier ::= [a-zA-Z_][a-zA-Z0-9_]*  (":" [a-zA-Z][a-zA-Z0-9_]*)*
```

Colon-separated segments qualify a name with a namespace: `string:length`.
Variables are always single-segment; relation and callable names may be
qualified. A leading `_` is an ordinary identifier character, but `_` alone is
the wildcard (below).

### Literals

| kind | form | examples |
|---|---|---|
| integer | `[0-9]+` | `42`, `0` |
| float | `[0-9]+ "." [0-9]+ ([eE] [+-]? [0-9]+)?` | `3.14`, `1.0`, `2.5e-3` |
| string | `"` characters and escapes `"` | `"hello"`, `"say \"hi\""` |
| boolean | keyword | `true`, `false` |
| absence | keyword | `NONE` |

Numbers are **unsigned tokens**: `-5` is unary minus applied to `5`. String
escapes are `\"`, `\\`, `\n`, `\t`, `\r`; a backslash before anything else is an
error, so a typo is reported rather than silently dropped.

### The wildcard

`_` matches any value and binds nothing. It is legal only in an atom's argument
position, where "there is a column here I do not care about" is the whole
meaning. Two `_` in one atom are unrelated.

### Comments and whitespace

`#` begins a comment running to the end of the line. Spaces and tabs separate
tokens.

**Indentation** delimits a multiline rule body, and nothing else. The width is
set by the first body statement; every later statement in that body must be
indented at least as far. Blank lines and comment-only lines may appear between
statements and do not end the body. A line indented less than the first
statement ends the rule.

## Grammar

```
program        ::= (spec | fn_spec | rule)*

spec           ::= relation_name "::" "relation" "(" [col_types] ")"
col_types      ::= col_type ("," col_type)* [","]
col_type       ::= name ":" type

fn_spec        ::= callable_name "::" "function" NEWLINE (INDENT variant NEWLINE)+
variant        ::= "(" [params] ")" "->" type
params         ::= type ("," type)* ("," name ":" type)*
                 | name ":" type ("," name ":" type)*

rule           ::= fact | single_line_rule | multiline_rule

fact           ::= relation_name "(" [kv_args] ")"
single_line_rule ::= head "<-" body_stmt
multiline_rule ::= head "<-" NEWLINE (INDENT body_stmt NEWLINE)+
head           ::= relation_name "(" [kv_args] ")"

kv_args        ::= kv_arg ("," kv_arg)* [","]
kv_arg         ::= name ":" expr        -- explicit
                 | name ":"             -- shorthand for `name: name`
                 | name ":" "_"         -- wildcard: matches, binds nothing

body_stmt      ::= positive_atom
                 | negated_atom
                 | match
                 | filter
                 | type_assertion
                 | input_stmt

positive_atom  ::= relation_name "(" [kv_args] ")"
negated_atom   ::= "not" positive_atom
filter         ::= expr                 -- must be boolean
type_assertion ::= variable "::" type
input_stmt     ::= "input"

match          ::= variable ":=" expr
                 | variable ":=" aggregate
                 | unnest_pattern ":=" ("*" | "**") expr
                 | destructure ":=" expr

unnest_pattern ::= "(" variable ("," variable)* ")"

destructure    ::= array_pattern | dict_pattern | record_pattern
array_pattern  ::= "[" [pat_elems] ["," rest] "]"
dict_pattern   ::= "{" [pat_fields] ["," dict_rest] "}"
record_pattern ::= "record" "(" [pat_fields] ["," dict_rest] ")"
pat_elems      ::= variable ("," variable)*
pat_fields     ::= pat_field ("," pat_field)*
pat_field      ::= dict_key ":" [variable]   -- omitted: the key names it
dict_key       ::= name | STRING        -- quoted only when not an identifier
rest           ::= "*" [variable]       -- ignore, or bind, the remainder
dict_rest      ::= "**" [variable]

aggregate      ::= aggregator "<" [expr] ">"
aggregator     ::= "sum" | "count" | "min" | "max" | "avg"
```

The names and literals those productions rest on, and the type grammar the two
annotation forms take. What each type *means* is [`types.md`](types.md); this is
only its shape.

```
relation_name  ::= identifier          -- under no reserved namespace
callable_name  ::= identifier          -- under a reserved namespace
name           ::= identifier          -- a column or field name
variable       ::= [a-zA-Z_][a-zA-Z0-9_]*   -- single segment: no namespace

literal        ::= INTEGER | FLOAT | STRING | "true" | "false" | "NONE"

type           ::= "boolean" | "i64" | "f64" | "string" | "json"
                 | "date" | "time" | "timestamp"
                 | "optional" "(" type ")"
                 | "record" "(" [type_fields] ")"
                 | "array" "(" type ")"
                 | "dict" "(" key_type "," type ")"
                 | type_var                    -- only in a variant
type_fields    ::= name ":" type ("," name ":" type)* [","]
key_type       ::= "boolean" | "i64" | "f64" | "string"
                 | "date" | "time" | "timestamp"
type_var       ::= [A-Z][a-zA-Z0-9]*
```

A **type variable** — `T`, `K`, `V` — is legal in a function typespec and
nowhere else: it says that `array:at`'s result is the array's element type
without naming one. An initial capital is the whole of the rule, no type's own
name being spelled that way, so a variable needs no declaration to be told from
a type.

### Expressions

```
expr        ::= or_expr
or_expr     ::= and_expr ("or" and_expr)*
and_expr    ::= not_expr ("and" not_expr)*
not_expr    ::= "not" not_expr | comparison
comparison  ::= cat_expr [cmp_op cat_expr]      -- non-associative
cat_expr    ::= add_expr ("++" add_expr)*
add_expr    ::= mul_expr (("+" | "-") mul_expr)*
mul_expr    ::= unary (("*" | "/" | "%") unary)*
unary       ::= "-" postfix | postfix
postfix     ::= primary ("." name | "[" subscript "]")*
subscript   ::= expr                                -- a lookup
              | [expr] ":" [expr] [":" [expr]]      -- a slice
primary     ::= literal
              | variable
              | call
              | array_literal
              | dict_literal
              | record_literal
              | "(" expr ")"

cmp_op      ::= "=" | "!=" | "<" | "<=" | ">" | ">="

call           ::= identifier "(" [args] ")"
args           ::= expr ("," expr)* ("," name ":" expr)*
                 | name ":" expr ("," name ":" expr)*

array_literal  ::= "[" [expr ("," expr)*] [","] "]"
dict_literal   ::= "{" [dict_entry ("," dict_entry)*] [","] "}"
dict_entry     ::= expr "=>" expr       -- any key type
                 | dict_key ":" expr    -- a string key
record_literal ::= "record" "(" [rec_field ("," rec_field)*] [","] ")"
rec_field      ::= dict_key ":" expr
```

### Subscripts

`d[k]` reads a dict by key, `arr[i]` an array by position, and
`arr[1:5:2]` slices. Every part of a slice may be left out — `arr[:5]`,
`arr[2:]`, `arr[::2]`, `arr[:]` — and a negative step reverses.

A lookup is [sugar for a library function](semantics.md#the-standard-library),
and **which one is decided by the subject's type**: `dict:get` on a dict,
`array:at` on an array. A slice is always `array:slice`, a dict having no order
to take a run of.

**One hazard, and it is the lexer's.** `identifier ::= name (":" name)*` joins
namespace segments, so `arr[x:y]` lexes as `arr[` *one qualified name* `]`, and
`::` is a single token besides — `arr[::2]` would be an annotation. Inside a
subscript both are put back: `::` becomes two colons, and a qualified identifier
**not followed by `(`** is split into its segments.

Nothing is lost, because a qualified name is only ever a callable and a callable
is only ever followed by `(` — so `arr[string:length(s):n]` keeps its call and
splits its slice. It has to happen before the expression is parsed rather than
after: `arr[a+b:c]` joins `b` to `c`, and the tree that produces is `a + (b:c)`,
which no rewriting could turn back into `a+b` and `c`.

`arr[1:5]` and `arr[x:5]` were never at risk, a segment having to begin with a
letter.

### Operator groups

Precedence is defined **within** a group. Between groups only these relations
hold:

```
arithmetic    < comparison        a + b < c    →  (a + b) < c
concatenation < comparison        x ++ y = z   →  (x ++ y) = z
comparison    < boolean           x < 3 and y  →  (x < 3) and y
```

**Every other combination is a parse error** and must be parenthesised, so
`a ++ b + c` and `a + b and c` do not parse. Comparison does not chain: `a < b <
c` is an error, not `(a < b) < c`.

A unary operator adjacent to a binary one from another group is likewise an
error: write `(not a) and b` and `(-a) * b`.

This is deliberate and stricter than most languages. Precedence between
unrelated operator families is a thing people remember wrong, so grasp declines
to have an opinion and asks. The rule outlives the current table: the groups
that make it earn its keep — bitwise and shift — are not in this cut because
grasp-dbsp has nothing to lower them to, and when they arrive they arrive as
further incomparable groups, changing no existing program's meaning.

### The two dict forms

A dict literal has two spellings, and they do different jobs:

```grasp
d := {k => n, "literal" => x}   # `=>` : the key is an expression
e := {1 => x, 2 => y}          #        of any scalar type, one per dict
f := {name: n, age: 30}        # `:`  : a string key, written bare
g := {"key with spaces": v}    #        quoted when it is not an identifier
```

`{a: v}` **is** `{"a" => v}` — the `:` form is sugar for the common case, and
[`semantics.md`](semantics.md#desugaring) desugars it away before anything
downstream sees it. The two may be mixed in one literal; a `:` entry simply
constrains the dict's key type to `string`, like any other entry.

grasp-dbsp has only `{k => v}`, and deliberately: *"exactly one way to write
each thing"* is one of [its principles](../grasp-dbsp/overview.md#design-principles),
because it is written by a machine that already knows what it means. grasp is
written by people. That is the whole of why the sugar lives on this side of the
boundary and not the other.

### Patterns and literals share the `:` spelling

A dict **pattern** — the left of `:=` — uses `:` with the same meaning: the key
is the name written there.

```grasp
d := {name: n}      # literal:  builds a dict with key "name"
{name: n} := d      # pattern:  extracts the key "name"
```

The key means `"name"` in both. What differs is the right-hand side: a literal
evaluates it, a pattern binds it — the ordinary duality of any language with
destructuring.

`=>` is **not** allowed in a pattern. A pattern names the key it extracts, so
its keys are literal; looking one up by a computed key is `d[k]`, which is an
expression and belongs there.

A **subscript** is postfix, beside `.`, and reads a dict: `d[k]` where `k` is
any expression.

**A bracket that opens a line belongs to that line.** The body's statements are
delimited by line, so an expression may not reach across one to take the bracket
that begins the next:

```grasp
    n := d
    [x, y] := arr
```

is a match and then an array pattern, never `d[x, y]`. The same rule covers `(`:

```grasp
    n := a
    (v) := *arr
```

is a match and then an unnest, never the call `a(v)`. Everywhere else a `[`
directly after an expression is a subscript, a `(` directly after a name is a
call, and either one elsewhere begins an array literal or a grouping.

A brace form at body-statement level is parsed once and then read as a pattern
if `:=` follows it. That is also where a pattern's extra requirement is
enforced: its values must be variables, so `{a: f(1)} := d` is rejected there
rather than by the grammar.

> ``a pattern binds variables; `f(1)` is not one``

The variable may be left out, and then the key names it: `{a:} := d` is
`{a: a} := d`, and `record(a:, **) := r` is `record(a: a, **) := r`. It is the
same shorthand an atom's `r(a:)` has, and it reads the same way — the name is
already written, so writing it twice says nothing. A quoted key need not be a
name a variable can have, so there it is not available.

> ``` `key with spaces` is not a name a variable can have, so `key with spaces:`
> names none; write the variable ```

### Where a call is, and is not, a call

At **body-statement** level, `name(` always begins an atom. A function call is
only ever a subexpression:

```grasp
result(v: n) <-
    person(name: s)
    n := string:length(s)   # a call, inside a match
    string:length(s) > 3    # a call, inside a filter
```

There is no statement that is a bare call, so nothing has to disambiguate.

Inside an expression, `name(` is a call. An atom cannot appear there, so the two
never compete.

### Typespecs

A relation's is one line; a function's is a **block**, one variant per line,
indented past the name:

```grasp
array:slice :: function
    (array(T)) -> array(T)
    (array(T), stop: i64) -> array(T)
    (array(T), start: i64, stop: i64) -> array(T)
```

Written in bulk — one block per name, however many ways there are to call it —
so that a function's typespec is in one place. **There is one typespec per
name**, and a second is a conflict rather than an addition; so are two variants
of one [shape](#resolving-a-call) whose parameter types also agree. A function typespec declares a name in the
[standard library](semantics.md#the-standard-library) and must therefore be
namespaced: grasp has no user-defined functions.

`docs/grasp/stdlib.grasp` is the library written this way.

## Name resolution

**A name under a reserved namespace is a callable; every other name is a
relation's.** Every function in the library is namespaced, so the two can never
collide and body-statement position resolves `name(` without lookahead: a
qualified name under a held prefix is a call, and anything else begins an atom.
A relation may be called `length`.

Column names, variable names and namespace-qualified names are separate scopes
and unaffected.

### Resolving a call

A callable may have several variants, and the one a call means is decided first
by its **shape**: how many arguments it gives by position, and the *set* of
keyword names it gives. Erlang's dispatch rather than C++'s.

```
array:at(a, index: i)     shape (_, index:)
array:at(a, i)            shape (_, _)        — no such variant
```

Keywords are a set, so `f(a, start: 1, stop: 9)` and `f(a, stop: 9, start: 1)`
are one call, and two variants that differ only in the order they write their
keywords are two answers to the same one.

Shape resolution runs before any type is looked at. That is what lets
`array:slice` carry eight variants over `start:` `stop:` `step:` without a
defaulting mechanism, and it is why a call whose shape no variant has is
reported in the words of the call rather than as a type error about an argument.

**Where two variants share a shape, the argument's type decides**, and that tie
is broken in [inference](inference.md#a-subscripts-function-is-chosen-here)
rather than here, because it is the first pass that has types.

```
temporal:date("2024-01-15")   shape (_), a string     — parses
temporal:date(ts)             shape (_), a timestamp  — extracts
```

Two variants of one shape are a [conflicting typespec](#typespecs) only when
their parameter *types* also agree. Different types are an overload; the same
types twice are two answers to one call.

### Reserved words

These may not name a relation, a variable or a column:

- **keywords** — `not`, `and`, `or`, `input`, `true`, `false`, `NONE`
- **type names** — `boolean`, `i64`, `f64`, `string`, `json`, `optional`,
  `record`, `array`, `dict`, `date`, `time`, `timestamp`
- **declaration kinds** — `relation`, `function`; the word a typespec uses to
  say what it declares
- **aggregators** — `sum`, `count`, `min`, `max`, `avg`

Builtin names are *not* reserved as such; they are unavailable as relation names
by the one-name rule above, but a column may be called `length`.

### Reserved namespace prefixes

`string:`, `array:`, `dict:`, `record:`, `json:`, `boolean:`, `integer:`,
`float:`, `numeric:`, `bytes:`, `bits:`, `temporal:`, `crypto:` and `agg:`
belong to the language. User code may not define names under them.

They hold the [standard library](semantics.md#the-standard-library), every
function of which is namespaced — so a name under one of these is a callable and
a name under none of them is a relation's. That is what lets body-statement
position tell an atom from a filter without a lookahead, and it is why a
relation may be called `length`.

Several are held and empty, for families that arrive with their types. The rest
are in `stdlib.grasp`.

## AST

The parser produces a list of declarations. This is the shape the rest of the
pipeline consumes, and the last point at which source order matters.

```
Program      = [Decl]
Decl         = Spec { relation, columns: [(name, Type)] }
             | Fact { relation, args: [KvArg] }
             | Rule { head: Head, body: [Stmt] }

Head         = { relation, args: [KvArg] }
KvArg        = { column: name, value: Arg }
Arg          = Expr | Wildcard

Stmt         = Atom      { relation, args: [KvArg], negated: bool }
             | Match     { lhs: Pattern, rhs: Rhs }
             | Filter    { expr: Expr }
             | Assert    { variable, ty: Type }
             | Input

Pattern      = Var       { name }
             | Unnest    { vars: [name], kind: Array | Dict }
             | Array     { elems: [name], rest: Rest }
             | Dict      { fields: [(name, name)], rest: Rest }
             | Record    { fields: [(name, name)], rest: Rest }
Rest         = None | Ignore | Bind { name }

Rhs          = Expr
             | Aggregate { fn: Aggregator, arg: Expr? }

Expr         = Lit       { value }
             | Var       { name }
             | Field     { base: Expr, name }
             | Unary     { op, operand: Expr }
             | Binary    { op, lhs: Expr, rhs: Expr }
             | Call      { name, positional: [Expr], keyword: [(name, Expr)] }
             | ArrayLit  { elems: [Expr] }
             | DictLit   { entries: [(Expr, Expr)] }
             | RecordLit { fields: [(name, Expr)] }
```

Every node carries the source span it came from. Nothing downstream is allowed
to key on source *position* — spans exist for diagnostics and nothing else, so
that reformatting a program cannot change what it compiles to.

The AST is untyped: a `Field` still carries a name rather than an index, and a
`Lit` an unresolved number. [`inference.md`](inference.md) resolves both.

## Diagnostics

| rejection | when |
|---|---|
| `unexpected token` | the grammar above does not accept the input |
| ``operators `X` and `Y` cannot be mixed without parentheses`` | two incomparable operator groups meet |
| `comparison cannot be chained` | `a < b < c` |
| ``unknown escape `\X` `` | a backslash before anything but `"` `\` `n` `t` `r` |
| ``duplicate column `c` `` | one atom or head names a column twice |
| ``duplicate field `f` `` | one record or dict literal names a field twice |
| ``inconsistent indentation`` | a body statement is indented less than the first |
| ``the namespace `n:` is reserved for the language`` | the one-name rule: a held namespace holds callables |
| ``already has a relation typespec`` | two typespecs for one name |
| ``already has a variant called `(…)` `` | two variants of one shape |
| ``the wildcard `_` binds nothing and cannot be used here`` | `_` outside an atom argument |
