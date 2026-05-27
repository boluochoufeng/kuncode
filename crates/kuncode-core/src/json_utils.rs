pub mod stringified_json {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &serde_json::Value, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = value.to_string();
        serializer.serialize_str(&s)
    }

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

/// 反序列化一个 provider 可能发成 JSON `null`（而非数组）的 `Vec<T>`：
/// `null` 归一成空 vec。
///
/// 搭配字段上的 `#[serde(default)]` 使用——本函数处理「键在但为 null」，
/// `default` 处理「键缺失」（键缺失时 serde 根本不会调用 `deserialize_with`）。
/// 两者一起才同时覆盖这两种形态。
pub fn null_or_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let opt = <Option<Vec<T>> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}
