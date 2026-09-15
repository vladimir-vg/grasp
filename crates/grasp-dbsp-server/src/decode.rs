//! Rows out of a JSON body, for every transport that delivers one.
//!
//! Over HTTP the body is a request; from Kafka it is a message's payload. Both
//! are split and decoded here, the same way, so a row that ingests over one
//! ingests over the other.

use crate::wire::parse_error;
use grasp_dbsp::json::{Format, decode_delta, decode_delta_insert_delete};
use grasp_dbsp::value::{DynValue, TypeDesc};
use serde_json::Value as J;

/// The rows a body holds, and a Feldera-shaped description of each value that
/// did not decode.
///
/// Feldera splits a body into values with a brace-depth scanner and accepts
/// whitespace, newlines or nothing between them (`format/json/input.rs:426-
/// 454`). `StreamDeserializer` accepts exactly the same shape, which is why
/// `lines=single` and `lines=multiple` are both honoured by doing nothing
/// differently.
///
/// A value that is well-formed JSON and not a row is skipped and reported, and
/// the values around it still decode. A syntax error ends the body where it
/// stands, since nothing says where the next value would begin — which is also
/// where Feldera's parser stops (`format/json/input.rs:382-392`).
#[allow(clippy::type_complexity)]
pub fn rows(
    text: &str,
    row_type: &TypeDesc,
    format: Format,
    array: bool,
) -> (Vec<(DynValue, dbsp::ZWeight)>, Vec<J>) {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for (i, value) in serde_json::Deserializer::from_str(text)
        .into_iter::<J>()
        .enumerate()
    {
        let n = i + 1;
        let mut one = |value: &J| {
            let decoded = match format {
                Format::Weighted => decode_delta(value, row_type),
                Format::InsertDelete => decode_delta_insert_delete(value, row_type),
            };
            match decoded {
                Ok(row) => rows.push(row),
                Err(e) => errors.push(parse_error(n, e.0, &value.to_string())),
            }
        };
        match value {
            Ok(J::Array(items)) if array => items.iter().for_each(&mut one),
            Ok(value) => one(&value),
            Err(e) => {
                errors.push(parse_error(n, e.to_string(), ""));
                break;
            }
        }
    }
    (rows, errors)
}
