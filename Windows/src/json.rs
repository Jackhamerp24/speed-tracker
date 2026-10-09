use crate::time::{Time, MAX_UNIX_MS};
use serde_json::Value;

static NULL: Value = Value::Null;

/// Lenient readers for telemetry JSON: a missing or mistyped field reads as absent, never as zero.
pub trait Json {
    fn get_or_null(&self, name: &str) -> &Value;
    fn text(&self) -> Option<&str>;
    fn s(&self, name: &str) -> Option<&str>;
    fn number(&self) -> Option<f64>;
    fn n(&self, name: &str) -> Option<f64>;
    /// A non-negative integer that fits 32 bits. `1.0` and `1e2` are not integers here.
    fn int(&self) -> Option<i32>;
    fn i(&self, name: &str) -> Option<i32>;
    fn b(&self, name: &str) -> bool;
    fn items(&self) -> &[Value];
    fn date(&self) -> Option<Time>;
    /// The field exists, whatever its value; an explicit null counts.
    fn has(&self, name: &str) -> bool;
}

impl Json for Value {
    fn get_or_null(&self, name: &str) -> &Value {
        self.as_object()
            .and_then(|object| object.get(name))
            .unwrap_or(&NULL)
    }
    fn text(&self) -> Option<&str> {
        self.as_str()
    }
    fn s(&self, name: &str) -> Option<&str> {
        self.get_or_null(name).as_str()
    }
    fn number(&self) -> Option<f64> {
        self.as_f64().filter(|number| number.is_finite())
    }
    fn n(&self, name: &str) -> Option<f64> {
        self.get_or_null(name).number()
    }
    fn int(&self) -> Option<i32> {
        self.as_i64()
            .filter(|number| (0..=i32::MAX as i64).contains(number))
            .map(|number| number as i32)
    }
    fn i(&self, name: &str) -> Option<i32> {
        self.get_or_null(name).int()
    }
    fn b(&self, name: &str) -> bool {
        self.get_or_null(name).as_bool() == Some(true)
    }
    fn items(&self) -> &[Value] {
        self.as_array().map(Vec::as_slice).unwrap_or(&[])
    }
    fn date(&self) -> Option<Time> {
        if let Some(text) = self.as_str() {
            if let Some(time) = Time::parse(text) {
                return Some(time);
            }
        }
        // Epoch milliseconds.
        self.number()
            .filter(|number| *number > 0.0 && *number < MAX_UNIX_MS)
            .map(|number| Time::from_unix_ms(number as i64))
    }
    fn has(&self, name: &str) -> bool {
        self.as_object()
            .is_some_and(|object| object.contains_key(name))
    }
}

/// Adds token counts. Absent everywhere, or too large for the history schema, is absent.
pub fn add(first: Option<i32>, second: Option<i32>, third: Option<i32>) -> Option<i32> {
    if first.is_none() && second.is_none() && third.is_none() {
        return None;
    }
    if [first, second, third]
        .iter()
        .any(|value| value.is_some_and(|number| number < 0))
    {
        return None;
    }
    let sum = first.unwrap_or(0) as i64 + second.unwrap_or(0) as i64 + third.unwrap_or(0) as i64;
    i32::try_from(sum).ok()
}
