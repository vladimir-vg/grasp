# grasp — Syntax

The surface form: what a program is made of, and the grammar that accepts it.
What the forms *mean* is [`semantics.md`](semantics.md); what their types are is
[`types.md`](types.md).

## Lexical structure

### Tokens

```
keywords     not  and  or  input  true  false  NONE
types        boolean  i64  f64  string  json  optional  record  array  dict
             relation
aggregators  sum  count  min  max  avg
operators    +  -  *  /  %  ++
comparison   =  !=  <  <=  >  >=
binding      :=
annotation   ::
arrow        <-
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
program        ::= (spec | rule)*

spec           ::= relation_name "::" "relation" "(" [col_types] ")"
col_types      ::= col_type ("," col_type)* [","]
col_type       ::= name ":" type

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
pat_fields     ::= name ":" variable ("," name ":" variable)*
rest           ::= "*" [variable]       -- ignore, or bind, the remainder
dict_rest      ::= "**" [variable]

aggregate      ::= aggregator "<" [expr] ">"
aggregator     ::= "sum" | "count" | "min" | "max" | "avg"
```

The names and literals those productions rest on, and the type grammar the two
annotation forms take. What each type *means* is [`types.md`](types.md); this is
only its shape.

```
relation_name  ::= identifier
name           ::= identifier          -- a column or field name
variable       ::= [a-zA-Z_][a-zA-Z0-9_]*   -- single segment: no namespace

literal        ::= INTEGER | FLOAT | STRING | "true" | "false" | "NONE"

type           ::= "boolean" | "i64" | "f64" | "string" | "json"
                 | "optional" "(" type ")"
                 | "record" "(" [type_fields] ")"
                 | "array" "(" type ")"
                 | "dict" "(" key_type "," type ")"
type_fields    ::= name ":" type ("," name ":" type)* [","]
key_type       ::= "boolean" | "i64" | "f64" | "string"
```

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
postfix     ::= primary ("." name)*
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
dict_literal   ::= "{" [expr "=>" expr ("," expr "=>" expr)*] [","] "}"
record_literal ::= "record" "(" [name ":" expr ("," name ":" expr)*] [","] ")"
```

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

### Dict literals use `=>`, patterns use `:`

A dict literal's keys are **expressions**; a dict pattern's keys are **field
names**. They need different spellings, because with one spelling `{name: n}`
would mean "the key that variable `name` holds" in an expression and "the key
`\"name\"`" in a pattern — the same text meaning two things by position.

```grasp
d := {"name" => n, k => 1}   # literal: keys are expressions
{name: n} := d               # pattern: `name` is the key
```

`=>` is also what grasp-dbsp spells a dict entry with, so the literal survives
lowering unchanged.

### Where a call is, and is not, a call

At **body-statement** level, `name(` always begins an atom. A function call is
only ever a subexpression:

```grasp
result(v: n) <-
    person(name: s)
    n := length(s)          # a call, inside a match
    length(s) > 3           # a call, inside a filter
```

There is no statement that is a bare call, so nothing has to disambiguate.

Inside an expression, `name(` is a call. An atom cannot appear there, so the two
never compete.

## Name resolution

**A name is either a relation or a callable, never both.** The compiler enforces
this when the program loads: if a program defines a relation `total`, `total`
cannot also name a builtin, and vice versa. This is what lets body-statement
position resolve `name(` without lookahead.

Column names, variable names and namespace-qualified names are separate scopes
and unaffected.

### Reserved words

These may not name a relation, a variable or a column:

- **keywords** — `not`, `and`, `or`, `input`, `true`, `false`, `NONE`
- **type names** — `boolean`, `i64`, `f64`, `string`, `json`, `optional`,
  `record`, `array`, `dict`, `relation`
- **aggregators** — `sum`, `count`, `min`, `max`, `avg`

Builtin names are *not* reserved as such; they are unavailable as relation names
by the one-name rule above, but a column may be called `length`.

### Reserved namespace prefixes

`string:`, `array:`, `dict:`, `record:`, `json:`, `agg:` and `temporal:` belong
to the language. User code may not define names under them. They are held now so
that the standard library can grow into them without taking names a program was
already using — see [`overview.md`](overview.md#future-work).

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
| ``a name cannot be both a relation and a callable`` | the one-name rule |
| ``the wildcard `_` binds nothing and cannot be used here`` | `_` outside an atom argument |
