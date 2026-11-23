//! Converts protobuf Value/Struct to JSON.
//!
//! Protobuf numbers are f64. JSON schema constraints such as `minItems` must
//! be integers, so exact integer-valued floats are emitted as JSON integers
//! when they fit in `u64` / `i64` without saturating.

use prost_types::{Struct, Value as ProtoValue, value::Kind};
use serde_json::Value;

pub(crate) fn prost_json(value: &ProtoValue) -> Value {
    match value.kind.as_ref() {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::NumberValue(value)) => json_number(*value),
        Some(Kind::StringValue(value)) => Value::String(value.clone()),
        Some(Kind::BoolValue(value)) => Value::Bool(*value),
        Some(Kind::StructValue(value)) => prost_struct(value),
        Some(Kind::ListValue(value)) => Value::Array(value.values.iter().map(prost_json).collect()),
    }
}

pub(crate) fn prost_struct(value: &Struct) -> Value {
    Value::Object(
        value
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), prost_json(value)))
            .collect(),
    )
}

fn json_number(value: f64) -> Value {
    if let Some(int_value) = integral_u64(value) {
        return Value::from(int_value);
    }
    if let Some(int_value) = integral_i64(value) {
        return Value::from(int_value);
    }
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// 2^64: the first `f64` past `u64`'s range. Exactly representable.
const U64_LIMIT: f64 = 18_446_744_073_709_551_616.0;
/// -2^63 and 2^63: `i64`'s range. Exactly representable.
const I64_MIN_LIMIT: f64 = -9_223_372_036_854_775_808.0;
const I64_MAX_LIMIT: f64 = 9_223_372_036_854_775_808.0;

/// Whether `value` is finite with no fractional part. Compares bit patterns
/// instead of the value so the check is exact and `-0.0` counts as integral.
fn is_integral(value: f64) -> bool {
    value.is_finite() && value.trunc().to_bits() == value.to_bits()
}

/// Every integral `f64` inside `u64`'s range is exactly representable as `u64`,
/// so the guarded cast below is lossless.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "range-checked to [0, 2^64) and integral, so the cast is exact"
)]
pub(crate) fn integral_u64(value: f64) -> Option<u64> {
    if is_integral(value) && (0.0..U64_LIMIT).contains(&value) {
        Some(value as u64)
    } else {
        None
    }
}

/// Same reasoning as [`integral_u64`], for `i64`'s range.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "range-checked to [-2^63, 2^63) and integral, so the cast is exact"
)]
fn integral_i64(value: f64) -> Option<i64> {
    if is_integral(value) && (I64_MIN_LIMIT..I64_MAX_LIMIT).contains(&value) {
        Some(value as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_protobuf_integers_serialize_as_json_integers() {
        let value = prost_json(&ProtoValue {
            kind: Some(Kind::NumberValue(2.0)),
        });
        assert_eq!(value, serde_json::json!(2));
        assert_eq!(serde_json::to_string(&value).unwrap(), "2");
    }

    #[test]
    fn non_integer_protobuf_numbers_remain_json_floats() {
        let value = prost_json(&ProtoValue {
            kind: Some(Kind::NumberValue(2.5)),
        });
        assert_eq!(value, serde_json::json!(2.5));
        assert_eq!(serde_json::to_string(&value).unwrap(), "2.5");
    }
}
