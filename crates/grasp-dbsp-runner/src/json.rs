//! The Feldera-native JSON codec, in the `weighted` format.
//!
//! `{"weight": 2, "data": {…}}` represents a Z-set delta exactly, including
//! weights whose magnitude is greater than one. `insert_delete` cannot say
//! "weight 3" except by repeating the row, which is why `weighted` is the
//! default here; see `docs/grasp-dbsp/mapping.md`.
//!
//! Encoding is schema-driven: a [`TypeDesc`] supplies record field names on the
//! way out and the expected variant on the way in. That is what lets record
//! values stay positional.

use crate::lower::Delta;
use crate::value::{BatchType, DynValue, TypeDesc};
use dbsp::ZWeight;
use feldera_sqllib::FlatVariant;
use serde::Deserialize;
use serde_json::{Map, Value as J};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError(pub String);

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for JsonError {}

type JResult<T> = Result<T, JsonError>;

fn bad<T>(msg: impl Into<String>) -> JResult<T> {
    Err(JsonError(msg.into()))
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

pub fn encode_value(v: &DynValue, ty: &TypeDesc) -> JResult<J> {
    // The mirror of `decode_value`'s check, and for the same reason: a declared
    // type is a promise the runtime cannot break. Emitting `null` here would
    // produce output this codec would refuse to read back.
    if v.is_none() {
        return if ty.is_optional() {
            Ok(J::Null)
        } else {
            bad(format!(
                "NONE where `{ty}` was expected; the column is not optional"
            ))
        };
    }
    let ty = ty.non_null();
    Ok(match (v, ty) {
        (DynValue::Bool(b), TypeDesc::Bool) => J::Bool(*b),
        (DynValue::I64(n), TypeDesc::I64) => J::from(*n),
        // NaN and the infinities have no JSON spelling, so they write as `null`
        // — which is what `serde_json`'s own `serialize_f64` does, and what
        // Feldera therefore emits for the same query.
        //
        // This is not the refusal above it, and the difference is the point:
        // `NONE` is not an `f64` at all, so writing it into a definite column
        // would contradict the type. NaN *is* an `f64`; JSON simply cannot spell
        // it. The cost is real and stated in the docs — such a row does not
        // decode back into the same schema.
        (DynValue::F64(f), TypeDesc::F64) => serde_json::Number::from_f64(f.into_inner())
            .map(J::Number)
            .unwrap_or(J::Null),
        (DynValue::String(s), TypeDesc::String) => J::String(s.clone()),
        // The written form, which is what `decode_value` reads back and what a
        // dict key already uses.
        (DynValue::Date(_), TypeDesc::Date)
        | (DynValue::Time(_), TypeDesc::Time)
        | (DynValue::Timestamp(_), TypeDesc::Timestamp) => J::String(
            v.dict_key_string()
                .expect("a temporal value has a written form"),
        ),
        (DynValue::Record(fields), TypeDesc::Record(schema)) => {
            if fields.len() != schema.len() {
                return bad(format!(
                    "record has {} field(s) but its type has {}",
                    fields.len(),
                    schema.len()
                ));
            }
            let mut obj = Map::new();
            for (value, (name, fty)) in fields.iter().zip(schema) {
                obj.insert(name.clone(), encode_value(value, fty)?);
            }
            J::Object(obj)
        }
        (DynValue::Array(items), TypeDesc::Array(elem)) => J::Array(
            items
                .iter()
                .map(|v| encode_value(v, elem))
                .collect::<JResult<Vec<_>>>()?,
        ),
        // An object, always — a dict has one wire shape whatever its key type,
        // and the key type is what says how to spell the key.
        (DynValue::Dict(entries), TypeDesc::Dict(kt, vt)) => {
            let mut obj = Map::new();
            for (k, v) in entries {
                obj.insert(encode_key(k, kt)?, encode_value(v, vt)?);
            }
            J::Object(obj)
        }
        // `TAG_SQL_NULL` and `TAG_VARIANT_NULL` both write as `null`, so absence
        // and JSON null are indistinguishable on the wire. Round-trip is still
        // stable: a `json` column reads `null` back as JSON null.
        (DynValue::Json(fv), TypeDesc::Json) => match serde_json::to_value(fv) {
            Ok(v) => v,
            Err(e) => return bad(format!("encoding a json document: {e}")),
        },
        (v, t) => return bad(format!("cannot encode a {} as `{t}`", v.type_name())),
    })
}

pub fn decode_value(j: &J, ty: &TypeDesc) -> JResult<DynValue> {
    // The type decides what a bare `null` means, so it is consulted first. For
    // every type but one, `null` is absence and is refused where absence is not
    // allowed; a `json` column holds JSON null as a *value*, which is the same
    // split Feldera makes between a nullable `VARIANT` and a `VARIANT NOT NULL`.
    if j.is_null() {
        return match ty {
            _ if ty.is_optional() => Ok(DynValue::None),
            TypeDesc::Json => Ok(DynValue::Json(FlatVariant::variant_null())),
            _ => bad(format!(
                "null where `{ty}` was expected; the column is not optional"
            )),
        };
    }
    let ty = ty.non_null();
    Ok(match ty {
        TypeDesc::Bool => match j.as_bool() {
            Some(b) => DynValue::Bool(b),
            None => return bad(format!("expected a bool, found `{j}`")),
        },
        TypeDesc::I64 => match j.as_i64() {
            Some(n) => DynValue::I64(n),
            None => return bad(format!("expected an integer, found `{j}`")),
        },
        TypeDesc::F64 => match j.as_f64() {
            Some(f) => DynValue::F64(dbsp::algebra::F64::new(f)),
            None => return bad(format!("expected a number, found `{j}`")),
        },
        TypeDesc::String => match j.as_str() {
            Some(s) => DynValue::String(s.to_string()),
            None => return bad(format!("expected a string, found `{j}`")),
        },
        // A temporal value travels as a string, in the one spelling its type
        // reads back — the same round trip a dict key rests on, and the same
        // function on both sides of it.
        TypeDesc::Date | TypeDesc::Time | TypeDesc::Timestamp => {
            let Some(s) = j.as_str() else {
                return bad(format!("expected a `{ty}` as a string, found `{j}`"));
            };
            match ty.parse_dict_key(s) {
                Some(v) => v,
                None => return bad(format!("`{s}` is not a `{ty}`")),
            }
        }
        TypeDesc::Record(schema) => {
            let Some(obj) = j.as_object() else {
                return bad(format!("expected an object, found `{j}`"));
            };
            let mut fields = Vec::with_capacity(schema.len());
            for (name, fty) in schema {
                // Absent and present-but-`null` are different things, and
                // collapsing them — as reading a missing key as `null` did —
                // makes the second case unreachable for a `json` field. An
                // omitted column is absence, which only an optional type
                // accepts; an explicit `null` is whatever the type says.
                let value = match obj.get(name) {
                    Some(raw) => decode_value(raw, fty)
                        .map_err(|e| JsonError(format!("field `{name}`: {e}")))?,
                    None if fty.is_optional() => DynValue::None,
                    None => {
                        return bad(format!(
                            "field `{name}` is missing, and `{fty}` is not optional"
                        ));
                    }
                };
                fields.push(value);
            }
            if let Some(extra) = obj.keys().find(|k| !schema.iter().any(|(n, _)| n == *k)) {
                return bad(format!("unknown field `{extra}`"));
            }
            DynValue::Record(fields)
        }
        TypeDesc::Array(elem) => {
            let Some(items) = j.as_array() else {
                return bad(format!("expected an array, found `{j}`"));
            };
            DynValue::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        decode_value(v, elem).map_err(|e| JsonError(format!("element {i}: {e}")))
                    })
                    .collect::<JResult<Vec<_>>>()?,
            )
        }
        TypeDesc::Dict(kt, vt) => {
            let Some(obj) = j.as_object() else {
                return bad(format!("expected an object, found `{j}`"));
            };
            let mut entries = std::collections::BTreeMap::new();
            for (k, v) in obj {
                let key = decode_key(k, kt)?;
                let value =
                    decode_value(v, vt).map_err(|e| JsonError(format!("key `{k}`: {e}")))?;
                entries.insert(key, value);
            }
            DynValue::Dict(entries)
        }
        // `FlatVariant`'s own deserializer builds the byte encoding directly,
        // canonicalising map key order on the way in.
        TypeDesc::Json => match FlatVariant::deserialize(j) {
            Ok(fv) => DynValue::Json(fv),
            Err(e) => return bad(format!("expected a JSON document, found `{j}`: {e}")),
        },
        TypeDesc::Optional(_) => unreachable!("stripped by non_null"),
    })
}

// ---------------------------------------------------------------------------
// Deltas
// ---------------------------------------------------------------------------

/// Decodes one input delta in [`Format::InsertDelete`]: `{"insert": {…}}` is
/// weight `+1`, `{"delete": {…}}` is `-1`. The format cannot express any other
/// magnitude, so a row with weight 2 arrives as two records.
/// A dict key as a JSON object key.
///
/// Object keys are strings, so every key type needs one spelling that
/// [`decode_key`] can parse back. `TypeDesc::is_dict_key` is what restricts the
/// key types to those that have one.
fn encode_key(k: &DynValue, ty: &TypeDesc) -> JResult<String> {
    if k.type_name() != type_key_name(ty) {
        return bad(format!(
            "cannot encode a {} as a `{ty}` dict key",
            k.type_name()
        ));
    }
    // `None` here means a non-finite float: it writes as `null` in value
    // position, and `null` is not an object key. Refusing says so rather than
    // inventing a spelling that would not parse back.
    k.dict_key_string().ok_or_else(|| {
        JsonError(format!(
            "this {ty} cannot be a dict key: it has no JSON spelling"
        ))
    })
}

/// The variant name a key of this type must have, so a mismatch is reported as
/// a type error rather than silently taking the value's own spelling.
fn type_key_name(ty: &TypeDesc) -> &'static str {
    match ty {
        TypeDesc::String => "string",
        TypeDesc::I64 => "i64",
        TypeDesc::Bool => "bool",
        TypeDesc::F64 => "f64",
        TypeDesc::Date => "date",
        TypeDesc::Time => "time",
        TypeDesc::Timestamp => "timestamp",
        _ => "",
    }
}

/// The inverse of [`encode_key`]: the key type says what to parse.
fn decode_key(k: &str, ty: &TypeDesc) -> JResult<DynValue> {
    ty.parse_dict_key(k)
        .ok_or_else(|| JsonError(format!("`{k}` is not a `{ty}` dict key")))
}

pub fn decode_delta_insert_delete(j: &J, row_type: &TypeDesc) -> JResult<(DynValue, ZWeight)> {
    let Some(obj) = j.as_object() else {
        return bad(format!("expected an insert/delete object, found `{j}`"));
    };
    let (weight, body) = match (obj.get("insert"), obj.get("delete")) {
        (Some(v), None) => (1, v),
        (None, Some(v)) => (-1, v),
        (Some(_), Some(_)) => return bad("a record has both `insert` and `delete`"),
        (None, None) => return bad(format!("expected `insert` or `delete`, found `{j}`")),
    };
    if obj.len() != 1 {
        return bad("an insert/delete record has exactly one key");
    }
    Ok((decode_value(body, row_type)?, weight))
}

/// Decodes one input delta: `{"weight": w, "data": {…}}`.
pub fn decode_delta(j: &J, row_type: &TypeDesc) -> JResult<(DynValue, ZWeight)> {
    let Some(obj) = j.as_object() else {
        return bad(format!("expected a delta object, found `{j}`"));
    };
    let Some(weight) = obj.get("weight").and_then(|w| w.as_i64()) else {
        return bad("a delta needs an integer `weight`");
    };
    let Some(data) = obj.get("data") else {
        return bad("a delta needs a `data` field");
    };
    Ok((decode_value(data, row_type)?, weight))
}

/// Which delta encoding to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `{"weight": 2, "data": {…}}` — represents a Z-set delta exactly.
    #[default]
    Weighted,
    /// `{"insert": {…}}` / `{"delete": {…}}`. Cannot express a weight, so a
    /// weight of magnitude *n* becomes *n* repeated records.
    InsertDelete,
}

/// Encodes one delta in [`Format::InsertDelete`], which needs one record per
/// unit of weight because the format has nowhere to put a magnitude.
pub fn encode_delta_insert_delete(d: &Delta, ty: &BatchType) -> JResult<Vec<J>> {
    let body = match (ty, &d.value) {
        (BatchType::ZSet(t), None) => encode_value(&d.key, t)?,
        (BatchType::IndexedZSet(kt, vt), Some(v)) => {
            let mut obj = Map::new();
            obj.insert("key".into(), encode_value(&d.key, kt)?);
            obj.insert("value".into(), encode_value(v, vt)?);
            J::Object(obj)
        }
        (BatchType::ZSet(_), Some(_)) => {
            return bad("a flat stream produced a delta with a value half");
        }
        (BatchType::IndexedZSet(..), None) => {
            return bad("an indexed stream produced a delta with no value half");
        }
    };
    let key = if d.weight >= 0 { "insert" } else { "delete" };
    let count = d.weight.unsigned_abs() as usize;
    Ok((0..count)
        .map(|_| {
            let mut obj = Map::new();
            obj.insert(key.into(), body.clone());
            J::Object(obj)
        })
        .collect())
}

/// Encodes one output delta.
///
/// A flat stream produces `{"weight", "data"}`, matching Feldera's `weighted`
/// format. An indexed stream has a key and a value, which that format has no
/// shape for, so it produces `{"weight", "key", "value"}`.
pub fn encode_delta(d: &Delta, ty: &BatchType) -> JResult<J> {
    let mut obj = Map::new();
    obj.insert("weight".into(), J::from(d.weight));
    match (ty, &d.value) {
        (BatchType::ZSet(t), None) => {
            obj.insert("data".into(), encode_value(&d.key, t)?);
        }
        (BatchType::IndexedZSet(kt, vt), Some(v)) => {
            obj.insert("key".into(), encode_value(&d.key, kt)?);
            obj.insert("value".into(), encode_value(v, vt)?);
        }
        (BatchType::ZSet(_), Some(_)) => {
            return bad("a flat stream produced a delta with a value half");
        }
        (BatchType::IndexedZSet(..), None) => {
            return bad("an indexed stream produced a delta with no value half");
        }
    }
    Ok(J::Object(obj))
}
