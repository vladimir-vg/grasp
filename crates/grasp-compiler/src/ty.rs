//! Types while inference is still running, and the four things done to them.
//!
//! [`Ty`] is [`crate::ast::Type`] with holes. `docs/grasp/inference.md` types a
//! program by a fixpoint over its relations, so a column's type spends the
//! middle of that loop only partly known — and a literal spends it deliberately
//! unknown, because "an integer literal is not an `i64` until something says
//! so".
//!
//! **The holes go all the way down.** `grasp-dbsp-runner` has an analogous
//! lattice whose open cases sit only at the root, which is enough for a language
//! that declares its types; it cannot express `[1, 2, 3]`, an array whose
//! *element* is what has no type yet. grasp reaches that in the first fixture
//! that writes an array literal, so [`Ty`] is open at every level.
//!
//! The four operations are genuinely four, and conflating any two is the bug
//! this module exists to prevent:
//!
//! - [`compose`] — `inference.md`'s "composing constraints": what one variable
//!   constrained twice must be. Symmetric.
//! - [`assignable`] — `types.md`'s assignability, `S → T`. **Not** symmetric,
//!   and not composition: `i64 → f64` is *no*, while composing `i64` with `f64`
//!   fails for a different reason and says something different.
//! - [`impose`] — phase 3, top-down: fill a type's holes from the context it
//!   sits in.
//! - [`settle`] — what is left when the holes should be gone, or why one is not.

use crate::ast::Type;
use std::fmt;

/// A type with holes.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// No constraint yet. `[]` is `Array(Unknown)`, `{}` is
    /// `Dict(Unknown, Unknown)` and `NONE` is `Optional(Unknown)` — every open
    /// case but the numeric one is this, under a constructor.
    Unknown,
    /// An integer literal, which `i64` and `f64` both admit.
    ///
    /// It has no default. `inference.md` is explicit that settling one on its
    /// own is an error "not a silent default to `i64`", because a default
    /// "would put a type nobody chose into a relation's schema, where every
    /// later rule would then have to agree with it".
    Int,
    /// Already reported wrong.
    ///
    /// It composes with everything and settles to nothing, which is what keeps
    /// one mistake to one diagnostic — the harness matches an exact set, so a
    /// second complaint about the same fault fails the case.
    Error,

    Boolean,
    I64,
    F64,
    String,
    Json,
    Optional(Box<Ty>),
    /// Sorted by name: fields are a set.
    Record(Vec<(String, Ty)>),
    Array(Box<Ty>),
    Dict(Box<Ty>, Box<Ty>),
    Date,
    Time,
    Timestamp,
    Interval,
}

impl Ty {
    /// The type of a value that already knows what it is.
    pub fn known(t: &Type) -> Ty {
        match t {
            Type::Boolean => Ty::Boolean,
            Type::I64 => Ty::I64,
            Type::F64 => Ty::F64,
            Type::String => Ty::String,
            Type::Json => Ty::Json,
            Type::Date => Ty::Date,
            Type::Time => Ty::Time,
            Type::Timestamp => Ty::Timestamp,
            Type::Interval => Ty::Interval,
            Type::Optional(inner) => Ty::Optional(Box::new(Ty::known(inner))),
            Type::Array(inner) => Ty::Array(Box::new(Ty::known(inner))),
            Type::Dict(k, v) => Ty::Dict(Box::new(Ty::known(k)), Box::new(Ty::known(v))),
            Type::Record(fields) => Ty::Record(sorted(
                fields.iter().map(|(n, t)| (n.clone(), Ty::known(t))),
            )),
            // A type variable is a function typespec's, and a typespec does not
            // reach the core: desugaring drops it, having nothing to lower.
            Type::Var(name) => {
                unreachable!("`{name}` is a type variable, which only a typespec holds")
            }
        }
    }

    /// Whether anything here is still open. `Error` counts as closed: it has
    /// been reported, and asking again would report it twice.
    pub fn has_holes(&self) -> bool {
        match self {
            Ty::Unknown | Ty::Int => true,
            Ty::Error => false,
            Ty::Optional(t) | Ty::Array(t) => t.has_holes(),
            Ty::Dict(k, v) => k.has_holes() || v.has_holes(),
            Ty::Record(fields) => fields.iter().any(|(_, t)| t.has_holes()),
            _ => false,
        }
    }

    /// Whether a reported mistake is anywhere in here.
    ///
    /// `Error` absorbs everything it composes with, so a variable that once
    /// conflicted stays `Error` for the rest of the pass — which is what makes
    /// the mistake speak once. Anything that carries a type from one pass into
    /// the next has to drop these, or the pass that would report the conflict
    /// starts out already absorbing it and says nothing at all.
    pub fn is_poisoned(&self) -> bool {
        match self {
            Ty::Error => true,
            Ty::Optional(t) | Ty::Array(t) => t.is_poisoned(),
            Ty::Dict(k, v) => k.is_poisoned() || v.is_poisoned(),
            Ty::Record(fields) => fields.iter().any(|(_, t)| t.is_poisoned()),
            _ => false,
        }
    }
}

fn sorted(fields: impl IntoIterator<Item = (String, Ty)>) -> Vec<(String, Ty)> {
    let mut v: Vec<(String, Ty)> = fields.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // These are never quoted in a diagnostic — a message about a hole
            // says what is missing, not that "Unknown" was found — but a
            // rendering that panicked would be worse than one that is dull.
            Ty::Unknown => f.write_str("an unknown type"),
            Ty::Int => f.write_str("an integer literal"),
            Ty::Error => f.write_str("an invalid type"),
            Ty::Boolean => f.write_str("boolean"),
            Ty::I64 => f.write_str("i64"),
            Ty::F64 => f.write_str("f64"),
            Ty::String => f.write_str("string"),
            Ty::Json => f.write_str("json"),
            Ty::Date => f.write_str("date"),
            Ty::Time => f.write_str("time"),
            Ty::Timestamp => f.write_str("timestamp"),
            Ty::Interval => f.write_str("interval"),
            Ty::Optional(t) => write!(f, "optional({t})"),
            Ty::Array(t) => write!(f, "array({t})"),
            Ty::Dict(k, v) => write!(f, "dict({k}, {v})"),
            Ty::Record(fields) => {
                f.write_str("record(")?;
                for (i, (name, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                f.write_str(")")
            }
        }
    }
}

/// Two constraints on one thing that cannot both hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub left: Ty,
    pub right: Ty,
}

/// `inference.md`, "Composing constraints".
///
/// **Composition is not widening.** `i64` and `f64` conflict rather than
/// meeting at `f64`, because `types.md` says numbers do not widen and a
/// variable used as both is a mistake rather than a promotion.
pub fn compose(a: &Ty, b: &Ty) -> Result<Ty, Conflict> {
    let conflict = || {
        Err(Conflict {
            left: a.clone(),
            right: b.clone(),
        })
    };
    Ok(match (a, b) {
        // A reported mistake absorbs everything after it, so it speaks once.
        (Ty::Error, _) | (_, Ty::Error) => Ty::Error,
        (Ty::Unknown, t) | (t, Ty::Unknown) => t.clone(),

        // "An untyped literal composes with anything it can inhabit."
        (Ty::Int, Ty::Int) => Ty::Int,
        (Ty::Int, Ty::I64) | (Ty::I64, Ty::Int) => Ty::I64,
        (Ty::Int, Ty::F64) | (Ty::F64, Ty::Int) => Ty::F64,
        // An integer literal under an optional column is still an integer
        // literal — the openness has to survive the wrapper.
        (Ty::Int, Ty::Optional(t)) | (Ty::Optional(t), Ty::Int) => {
            Ty::Optional(Box::new(compose(&Ty::Int, t)?))
        }
        (Ty::Int, _) | (_, Ty::Int) => return conflict(),

        // "`T` and `optional(T)` compose to `optional(T)`. A variable that some
        // path leaves absent is optional everywhere." `optional` does not nest,
        // so this never wraps twice.
        (Ty::Optional(x), Ty::Optional(y)) => Ty::Optional(Box::new(compose(x, y)?)),
        (Ty::Optional(x), other) | (other, Ty::Optional(x)) => {
            Ty::Optional(Box::new(compose(x, other)?))
        }

        // "Same constructor composes covariantly, recursing on element,
        // key/value, and same-named field types."
        (Ty::Array(x), Ty::Array(y)) => Ty::Array(Box::new(compose(x, y)?)),
        (Ty::Dict(k1, v1), Ty::Dict(k2, v2)) => {
            Ty::Dict(Box::new(compose(k1, k2)?), Box::new(compose(v1, v2)?))
        }
        (Ty::Record(x), Ty::Record(y)) => {
            // "Two records with different field *names* do not compose." Not a
            // merge: a record's fields are its identity, so disagreeing on them
            // is disagreeing on the type.
            if x.len() != y.len() || x.iter().zip(y).any(|(a, b)| a.0 != b.0) {
                return conflict();
            }
            Ty::Record(
                x.iter()
                    .zip(y)
                    .map(|((n, a), (_, b))| Ok((n.clone(), compose(a, b)?)))
                    .collect::<Result<_, Conflict>>()?,
            )
        }

        (x, y) if x == y => x.clone(),
        _ => return conflict(),
    })
}

/// `types.md`, "Assignability" — whether an `S` may stand where a `T` is wanted.
///
/// One-directional, and deliberately narrow: there is no implicit conversion,
/// numbers do not widen, and nothing is assignable out of `json`.
pub fn assignable(from: &Ty, to: &Ty) -> bool {
    match (from, to) {
        (Ty::Error, _) | (_, Ty::Error) => true,
        // A hole is assignable to anything: it has not been settled yet, and
        // settling is what decides. Phase 3 catches one that never was.
        (Ty::Unknown, _) => true,
        (Ty::Int, Ty::I64 | Ty::F64 | Ty::Int) => true,
        (Ty::Int, Ty::Optional(t)) => assignable(&Ty::Int, t),
        (Ty::Int, _) => false,

        // "`T → optional(T)` yes; `optional(T) → T` no — use the `v :: T`
        // filter." Absence is a value, and dropping it is a decision the
        // program has to write down.
        (f, Ty::Optional(t)) if !matches!(f, Ty::Optional(_)) => assignable(f, t),
        (Ty::Optional(f), Ty::Optional(t)) => assignable(f, t),
        (Ty::Optional(_), _) => false,

        // Into `json`: the types a document can hold, and no others. `i64` is
        // refused rather than widened to `f64`, because a 64-bit identifier
        // does not survive the trip.
        (Ty::String | Ty::F64 | Ty::Boolean | Ty::Json, Ty::Json) => true,
        (Ty::Array(e), Ty::Json) => assignable(e, &Ty::Json),
        (Ty::Dict(k, v), Ty::Json) => **k == Ty::String && assignable(v, &Ty::Json),
        (_, Ty::Json) => false,
        // "Nothing is assignable out of `json`."
        (Ty::Json, _) => false,

        (Ty::Array(f), Ty::Array(t)) => assignable(f, t),
        (Ty::Dict(fk, fv), Ty::Dict(tk, tv)) => assignable(fk, tk) && assignable(fv, tv),
        // "Field names equal as sets — no width subtyping."
        (Ty::Record(f), Ty::Record(t)) => {
            f.len() == t.len()
                && f.iter()
                    .zip(t)
                    .all(|((fa, fv), (ta, tv))| fa == ta && assignable(fv, tv))
        }

        (f, t) => f == t,
    }
}

/// Phase 3, top-down: fill `ty`'s holes from the type of the place it sits in.
///
/// The one rewrite inference performs on a value is the consequence of this:
/// an integer literal under an `f64` context becomes a float. Everything else
/// here is a check that the context and the type agree.
pub fn impose(ty: &mut Ty, expected: &Ty) -> Result<(), Conflict> {
    match (&mut *ty, expected) {
        (_, Ty::Unknown) | (Ty::Error, _) | (_, Ty::Error) => Ok(()),
        (Ty::Unknown, t) => {
            *ty = t.clone();
            Ok(())
        }
        (Ty::Int, Ty::I64 | Ty::F64) => {
            *ty = expected.clone();
            Ok(())
        }
        (Ty::Int, Ty::Optional(inner)) => {
            let mut it = Ty::Int;
            impose(&mut it, inner)?;
            *ty = Ty::Optional(Box::new(it));
            Ok(())
        }
        (Ty::Optional(inner), Ty::Optional(exp)) => impose(inner, exp),
        // A definite value under an optional context stays definite; it is
        // assignable, and widening it here would lose that it can never be
        // absent.
        (_, Ty::Optional(exp)) => impose(ty, exp),
        (Ty::Array(inner), Ty::Array(exp)) => impose(inner, exp),
        (Ty::Dict(k, v), Ty::Dict(ek, ev)) => {
            impose(k, ek)?;
            impose(v, ev)
        }
        (Ty::Record(fields), Ty::Record(exp)) if fields.len() == exp.len() => {
            for ((fname, fty), (ename, ety)) in fields.iter_mut().zip(exp) {
                if fname != ename {
                    return Err(Conflict {
                        left: ty.clone(),
                        right: expected.clone(),
                    });
                }
                impose(fty, ety)?;
            }
            Ok(())
        }
        (have, want) if have == want => Ok(()),
        _ => Err(Conflict {
            left: ty.clone(),
            right: expected.clone(),
        }),
    }
}

/// What `v :: T` does, given what `v` already is — `types.md`, "Runtime
/// filters".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Narrowing {
    /// The value is already a `T`. A compile-time check: nothing is emitted.
    Already,
    /// A check could settle it, so a filter drops the rows that do not match.
    /// `cast` says whether the whole value has to be converted before it can be
    /// tested, and `shape` says what the check is *over*.
    Filter { cast: bool, shape: Shape },
    /// A check could settle it, but this compiler cannot emit one.
    ///
    /// One thing is: a check under *two* wrappers, as
    /// `optional(array(A)) :: optional(array(B))` is. Each layer alone is a
    /// shape emission writes; the two composed are a fifth, and nothing has
    /// wanted one. Reported as unsupported rather than as `Never`, which would
    /// call the program wrong for asking.
    Unsupported,
    /// No value of the source could ever be a `T`. An error, because a filter
    /// that can never pass is a silently empty relation.
    Never,
}

/// What a runtime filter tests, which the target type alone does not say:
/// `json :: array(i64)` and `array(json) :: array(i64)` want the same type and
/// check different things.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// The value itself, which is definite afterwards.
    Value,
    /// The value under its `optional`, which survives: absence is kept and only
    /// a present value that fails is dropped.
    Wrapper,
    /// Every element of an array.
    Elements,
    /// Every value of a dict.
    Values,
}

/// Which of the four `v :: T` is.
pub fn narrows(from: &Ty, to: &Ty) -> Narrowing {
    if assignable(from, to) {
        return Narrowing::Already;
    }
    match (from, to) {
        // "`optional(T)` :: `T` — drops the row when the value is absent." The
        // common one, and the way to drop absent rows before a join.
        (Ty::Optional(a), t) if !matches!(t, Ty::Optional(_)) => match narrows(a, t) {
            Narrowing::Already => Narrowing::Filter {
                cast: false,
                shape: Shape::Value,
            },
            Narrowing::Filter { .. } => Narrowing::Filter {
                cast: true,
                shape: Shape::Value,
            },
            other => other,
        },
        // "`json` :: `T` — drops the row when the document does not hold a
        // `T`." A document is converted by what is wanted, and every such
        // conversion is fallible, which is exactly the test.
        (Ty::Json, t) if holdable(t) => Narrowing::Filter {
            cast: true,
            shape: Shape::Value,
        },
        // "`optional(A)` :: `optional(B)` — drops the row when present and not
        // a `B`." The wrapper survives, so this is the inner check performed
        // under it, and absence is one of the answers rather than one of the
        // rows to drop.
        //
        // `Already` cannot appear here: it means the inner is assignable, and
        // then so is the pair, which `assignable` matched at the top. Every
        // other answer is the inner one — an inner `Unsupported` must stay
        // unsupported, or `optional(array(A)) :: optional(array(B))` would
        // become a filter over a check that does not exist.
        // The three rows that check *under* something. Each is the inner check
        // performed once per part, and each admits only a whole-value inner
        // one — `optional(array(A)) :: optional(array(B))` and
        // `array(array(A)) :: array(array(B))` are two of these composed, and
        // the composition is a shape emission does not write. Reported as
        // unsupported rather than as a mistake, because the program is not one.
        (Ty::Optional(a), Ty::Optional(b)) => under(narrows(a, b), true, Shape::Wrapper),
        // "`array(A)` :: `array(B)` — drops the row when any element is not a
        // `B`", and the dict row over its values. The check is per part and so
        // is the conversion; the whole value is converted nowhere, which is
        // what `cast: false` says.
        (Ty::Array(a), Ty::Array(b)) => under(narrows(a, b), false, Shape::Elements),
        (Ty::Dict(ka, a), Ty::Dict(kb, b)) if ka == kb => {
            under(narrows(a, b), false, Shape::Values)
        }
        _ => Narrowing::Never,
    }
}

/// One of the three checks performed under a wrapper, given what the inner
/// check turned out to be.
fn under(inner: Narrowing, cast: bool, shape: Shape) -> Narrowing {
    match inner {
        Narrowing::Filter {
            shape: Shape::Value,
            ..
        } => Narrowing::Filter { cast, shape },
        // An inner check that is itself under something. Two levels is a fifth
        // emission shape and nothing has wanted one.
        Narrowing::Filter { .. } => Narrowing::Unsupported,
        // `Already` cannot reach here: it means the inner is assignable, and
        // then so is the pair, which `assignable` matched at the top.
        other => other,
    }
}

/// Whether a document can be converted to this — the `cast` rows out of `json`,
/// which are every scalar plus a record or an array asked for by name.
fn holdable(t: &Ty) -> bool {
    matches!(
        t,
        Ty::Boolean
            | Ty::I64
            | Ty::F64
            | Ty::String
            | Ty::Record(_)
            | Ty::Array(_)
            // A temporal value is a string in a document, read in the one
            // spelling its own type writes — so extracting one is the check the
            // codec already performs, and `d :: date` is the row that says so.
            | Ty::Date
            | Ty::Time
            | Ty::Timestamp
            | Ty::Interval
    )
}

/// Why a type could not be settled.
#[derive(Debug, Clone, PartialEq)]
pub enum Open {
    /// An integer literal with nothing to take a type from.
    Numeric,
    /// `NONE` with nothing to take a type from.
    Absence,
    /// An empty `[]` or `{}` — "a complete value with an open type".
    Container,
    /// Anything else still unknown.
    Nothing,
    /// Already reported. Say nothing.
    Reported,
}

/// What is left when the holes should be gone, or why one is not.
pub fn settle(ty: &Ty) -> Result<Type, Open> {
    Ok(match ty {
        Ty::Error => return Err(Open::Reported),
        Ty::Int => return Err(Open::Numeric),
        Ty::Unknown => return Err(Open::Nothing),
        Ty::Optional(t) if **t == Ty::Unknown => return Err(Open::Absence),
        Ty::Array(t) if **t == Ty::Unknown => return Err(Open::Container),
        Ty::Dict(k, v) if **k == Ty::Unknown || **v == Ty::Unknown => {
            return Err(Open::Container);
        }

        Ty::Boolean => Type::Boolean,
        Ty::I64 => Type::I64,
        Ty::F64 => Type::F64,
        Ty::String => Type::String,
        Ty::Json => Type::Json,
        Ty::Date => Type::Date,
        Ty::Time => Type::Time,
        Ty::Timestamp => Type::Timestamp,
        Ty::Interval => Type::Interval,
        Ty::Optional(t) => Type::Optional(Box::new(settle(t)?)),
        Ty::Array(t) => Type::Array(Box::new(settle(t)?)),
        Ty::Dict(k, v) => Type::Dict(Box::new(settle(k)?), Box::new(settle(v)?)),
        Ty::Record(fields) => Type::Record(
            fields
                .iter()
                .map(|(n, t)| Ok((n.clone(), settle(t)?)))
                .collect::<Result<Vec<_>, Open>>()?,
        ),
    })
}

/// The tables in `docs/grasp/types.md`, transcribed.
///
/// They live here rather than in a fixture because most of their rows are not
/// reachable from a grasp program the corpus contains — `i64 → json` is *no*
/// while `f64 → json` is *yes*, and nothing writes either — and a table
/// implemented from prose with nothing checking it is a table that drifts.
#[cfg(test)]
mod tests {
    use super::*;

    fn opt(t: Ty) -> Ty {
        Ty::Optional(Box::new(t))
    }
    fn arr(t: Ty) -> Ty {
        Ty::Array(Box::new(t))
    }
    fn rec(fields: &[(&str, Ty)]) -> Ty {
        Ty::Record(sorted(
            fields.iter().map(|(n, t)| (n.to_string(), t.clone())),
        ))
    }

    #[test]
    fn numbers_do_not_widen() {
        assert!(!assignable(&Ty::I64, &Ty::F64));
        assert!(!assignable(&Ty::F64, &Ty::I64));
        assert!(compose(&Ty::I64, &Ty::F64).is_err());
    }

    #[test]
    fn an_integer_literal_inhabits_either_numeric_type() {
        assert_eq!(compose(&Ty::Int, &Ty::I64), Ok(Ty::I64));
        assert_eq!(compose(&Ty::Int, &Ty::F64), Ok(Ty::F64));
        assert_eq!(compose(&Ty::Int, &Ty::Int), Ok(Ty::Int));
        assert!(compose(&Ty::Int, &Ty::String).is_err());
        // And on its own it settles to nothing, rather than defaulting.
        assert_eq!(settle(&Ty::Int), Err(Open::Numeric));
    }

    #[test]
    fn optional_composes_but_does_not_nest() {
        assert_eq!(compose(&Ty::I64, &opt(Ty::I64)), Ok(opt(Ty::I64)));
        assert_eq!(compose(&opt(Ty::I64), &opt(Ty::I64)), Ok(opt(Ty::I64)));
        // The wrapper is applied once however many times it is composed.
        let twice = compose(&opt(Ty::I64), &Ty::I64).unwrap();
        assert_eq!(compose(&twice, &Ty::I64), Ok(opt(Ty::I64)));
    }

    #[test]
    fn assignability_into_optional_is_one_way() {
        assert!(assignable(&Ty::String, &opt(Ty::String)));
        // "use the `v :: T` filter" — dropping absence is written down.
        assert!(!assignable(&opt(Ty::String), &Ty::String));
    }

    #[test]
    fn the_json_table() {
        for t in [Ty::String, Ty::F64, Ty::Boolean] {
            assert!(assignable(&t, &Ty::Json), "{t} should reach json");
        }
        assert!(assignable(&arr(Ty::Json), &Ty::Json));
        assert!(assignable(
            &Ty::Dict(Box::new(Ty::String), Box::new(Ty::Json)),
            &Ty::Json
        ));
        // Refused rather than widened: a 64-bit identifier does not survive.
        assert!(!assignable(&Ty::I64, &Ty::Json));
        assert!(!assignable(&rec(&[("a", Ty::String)]), &Ty::Json));
        assert!(!assignable(&opt(Ty::String), &Ty::Json));
        // Nothing comes back out.
        for t in [Ty::String, Ty::I64, Ty::Boolean, arr(Ty::Json)] {
            assert!(!assignable(&Ty::Json, &t), "json should not reach {t}");
        }
    }

    #[test]
    fn records_have_no_width_subtyping() {
        let two = rec(&[("a", Ty::I64), ("b", Ty::String)]);
        let one = rec(&[("a", Ty::I64)]);
        assert!(!assignable(&two, &one));
        assert!(!assignable(&one, &two));
        assert!(compose(&one, &two).is_err());
        // Field order is not identity: both constructors sort.
        assert_eq!(
            rec(&[("b", Ty::String), ("a", Ty::I64)]),
            rec(&[("a", Ty::I64), ("b", Ty::String)])
        );
    }

    #[test]
    fn holes_go_all_the_way_down() {
        // `[1, 2, 3]` — the case the runner's root-only lattice cannot express.
        let mut literal = arr(Ty::Int);
        assert!(literal.has_holes());
        assert_eq!(settle(&literal), Err(Open::Numeric));
        impose(&mut literal, &arr(Ty::I64)).expect("an i64 array accepts it");
        assert_eq!(settle(&literal), Ok(Type::Array(Box::new(Type::I64))));
    }

    #[test]
    fn an_empty_container_says_which_hole_it_is() {
        assert_eq!(settle(&arr(Ty::Unknown)), Err(Open::Container));
        assert_eq!(settle(&opt(Ty::Unknown)), Err(Open::Absence));
        assert_eq!(settle(&Ty::Unknown), Err(Open::Nothing));
    }

    #[test]
    fn an_error_absorbs_and_stays_silent() {
        assert_eq!(compose(&Ty::Error, &Ty::String), Ok(Ty::Error));
        assert_eq!(compose(&Ty::String, &Ty::Error), Ok(Ty::Error));
        // So one mistake makes one diagnostic, which the exact-set matching
        // the harness does requires.
        assert_eq!(settle(&Ty::Error), Err(Open::Reported));
    }

    #[test]
    fn imposing_a_context_settles_a_literal() {
        let mut t = Ty::Int;
        impose(&mut t, &Ty::F64).unwrap();
        assert_eq!(t, Ty::F64);

        let mut d = Ty::Dict(Box::new(Ty::String), Box::new(Ty::Int));
        impose(&mut d, &Ty::Dict(Box::new(Ty::String), Box::new(Ty::I64))).unwrap();
        assert_eq!(settle(&d).unwrap().to_string(), "dict(string, i64)");
    }
}
