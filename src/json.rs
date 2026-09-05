//! The Feldera-native JSON codec, in the `weighted` format.
//!
//! `{"weight": 2, "data": {…}}` represents a Z-set delta exactly, including
//! weights whose magnitude is greater than one. `insert_delete` cannot say
//! "weight 3" except by repeating the row, which is why `weighted` is the
//! default here; see `docs/design/mapping.md`.
//!
//! Encoding is schema-driven: a [`TypeDesc`] supplies record field names on the
//! way out and the expected variant on the way in. That is what lets record
//! values stay positional.

use crate::lower::Delta;
use crate::value::{BatchType, DynValue, TypeDesc};
use dbsp::ZWeight;
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
    if v.is_none() {
        return Ok(J::Null);
    }
    let ty = ty.non_null();
    Ok(match (v, ty) {
        (DynValue::Bool(b), TypeDesc::Bool) => J::Bool(*b),
        (DynValue::I64(n), TypeDesc::I64) => J::from(*n),
        (DynValue::F64(f), TypeDesc::F64) => serde_json::Number::from_f64(f.into_inner())
            .map(J::Number)
            .unwrap_or(J::Null),
        (DynValue::String(s), TypeDesc::String) => J::String(s.clone()),
        (DynValue::SqlString(s), TypeDesc::SqlString) => J::String(s.str().to_string()),
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
        (v, t) => return bad(format!("cannot encode a {} as `{t}`", v.type_name())),
    })
}

pub fn decode_value(j: &J, ty: &TypeDesc) -> JResult<DynValue> {
    if j.is_null() {
        return if ty.is_optional() {
            Ok(DynValue::None)
        } else {
            bad(format!("null where `{ty}` was expected; the column is not optional"))
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
        TypeDesc::SqlString => match j.as_str() {
            Some(s) => DynValue::str(s),
            None => return bad(format!("expected a string, found `{j}`")),
        },
        TypeDesc::Record(schema) => {
            let Some(obj) = j.as_object() else {
                return bad(format!("expected an object, found `{j}`"));
            };
            let mut fields = Vec::with_capacity(schema.len());
            for (name, fty) in schema {
                // A missing field is null, which only typechecks if the field is
                // nullable — so this reports the real problem rather than
                // silently defaulting.
                let raw = obj.get(name).unwrap_or(&J::Null);
                fields.push(
                    decode_value(raw, fty)
                        .map_err(|e| JsonError(format!("field `{name}`: {e}")))?,
                );
            }
            if let Some(extra) = obj.keys().find(|k| !schema.iter().any(|(n, _)| n == *k)) {
                return bad(format!("unknown field `{extra}`"));
            }
            DynValue::Record(fields)
        }
        TypeDesc::Optional(_) => unreachable!("stripped by non_null"),
    })
}

// ---------------------------------------------------------------------------
// Deltas
// ---------------------------------------------------------------------------

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
