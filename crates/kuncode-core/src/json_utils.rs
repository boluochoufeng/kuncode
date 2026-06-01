//! Small serde helpers reused by provider protocol mappings.

/// serde `with` adapter for a [`serde_json::Value`] transported as
/// stringified JSON.
///
/// Some providers, including DeepSeek's `function.arguments`, put JSON inside
/// a string field on the wire while the domain layer wants structured
/// [`serde_json::Value`]. Use as
/// `#[serde(with = "json_utils::stringified_json")]`.
pub mod stringified_json {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes structured JSON as one string field.
    pub fn serialize<S>(value: &serde_json::Value, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = value.to_string();
        serializer.serialize_str(&s)
    }

    /// Deserializes one string field as structured JSON.
    ///
    /// Empty or whitespace-only strings are treated as an empty object because
    /// some providers emit empty argument strings for no-argument tool calls.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.trim().is_empty() {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }

        serde_json::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Shallow-merges JSON object `b` into `a`, with `b`'s keys taking precedence
/// on collision. If either side is not an object the left-hand `a` is
/// returned unchanged.
pub fn merge(a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    match (a, b) {
        (serde_json::Value::Object(mut a_map), serde_json::Value::Object(b_map)) => {
            b_map.into_iter().for_each(|(key, value)| {
                a_map.insert(key, value);
            });
            serde_json::Value::Object(a_map)
        }
        (a, _) => a,
    }
}

/// Deserializes a `Vec<T>` from a field that providers may send as JSON
/// `null`.
///
/// Pair this with `#[serde(default)]` on the field: this function handles "key
/// present but null", while `default` handles "key missing" because serde does
/// not call `deserialize_with` for absent fields.
pub fn null_or_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let opt = <Option<Vec<T>> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}
