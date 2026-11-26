//! Immutable JSON values with bounded representation; no raw-fragment insertion.
use std::collections::BTreeMap;

pub const MAX_BYTES: usize = 16_384;
pub const MAX_NODES: usize = 128;
pub const MAX_DEPTH: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct Json {
    data: Data,
    bytes: usize,
    nodes: usize,
    depth: usize,
}

#[derive(Clone, Debug, PartialEq)]
enum Data {
    Scalar(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

fn limits(bytes: usize, nodes: usize, depth: usize) -> Result<(), String> {
    if bytes > MAX_BYTES { return Err("JSON byte limit exceeded".into()); }
    if nodes > MAX_NODES { return Err("JSON node limit exceeded".into()); }
    if depth > MAX_DEPTH { return Err("JSON depth limit exceeded".into()); }
    Ok(())
}

fn quoted(text: &str) -> Result<String, String> {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
        if out.len() >= MAX_BYTES { return Err("JSON byte limit exceeded".into()); }
    }
    out.push('"');
    Ok(out)
}

pub fn quoted_bytes(text: &str) -> usize {
    2 + text.chars().map(|c| match c {
        '"' | '\\' | '\n' | '\r' | '\t' => 2, c if c < ' ' => 6, c => c.len_utf8(),
    }).sum::<usize>()
}

impl Json {
    pub fn is_object(&self) -> bool { matches!(&self.data, Data::Object(_)) }
    pub fn get(&self, key: &str) -> Result<&Self, String> {
        match &self.data {
            Data::Object(items) => items.get(key).ok_or_else(|| "JSON object key not found".into()),
            _ => Err("Expected JSON object".into()),
        }
    }
    pub fn items(&self) -> Result<&[Self], String> {
        match &self.data { Data::Array(items) => Ok(items), _ => Err("Expected JSON array".into()) }
    }
    pub fn at(&self, index: i64) -> Result<&Self, String> {
        let items = self.items()?;
        usize::try_from(index).ok().and_then(|i| items.get(i)).ok_or_else(|| "JSON array index out of bounds".into())
    }
    pub fn len(&self) -> Result<usize, String> {
        match &self.data {
            Data::Array(items) => Ok(items.len()), Data::Object(items) => Ok(items.len()),
            _ => Err("Expected JSON array or object".into()),
        }
    }
    pub fn as_text(&self) -> Result<String, String> {
        match &self.data {
            Data::Scalar(s) if s.starts_with('"') => super::json_input::decode_text(s),
            _ => Err("Expected JSON string".into()),
        }
    }
    pub fn as_int(&self) -> Result<i64, String> {
        match &self.data {
            Data::Scalar(s) => s.parse().map_err(|_| "Expected JSON integer".into()),
            _ => Err("Expected JSON integer".into()),
        }
    }
    pub fn as_bool(&self) -> Result<bool, String> {
        match &self.data {
            Data::Scalar(s) if s == "true" => Ok(true), Data::Scalar(s) if s == "false" => Ok(false),
            _ => Err("Expected JSON boolean".into()),
        }
    }
    pub fn is_null(&self) -> bool { matches!(&self.data, Data::Scalar(s) if s == "null") }
    pub fn byte_len(&self) -> usize { self.bytes }
    // Fixed node/entry charges include logical container overhead. This policy
    // does not claim to measure allocator capacity, fragmentation or process RAM.
    pub fn memory_bytes(&self) -> usize {
        128 + match &self.data {
            Data::Scalar(text) => text.len(),
            Data::Array(items) => items.iter().map(Self::memory_bytes).sum(),
            Data::Object(items) => items.iter().map(|(key, value)| 128 + key.len() + value.memory_bytes()).sum(),
        }
    }
    fn scalar(encoded: String) -> Self {
        Self { bytes: encoded.len(), data: Data::Scalar(encoded), nodes: 1, depth: 1 }
    }
    pub fn text(text: &str) -> Result<Self, String> { Ok(Self::scalar(quoted(text)?)) }
    pub fn int(value: i64) -> Self { Self::scalar(value.to_string()) }
    pub fn boolean(value: bool) -> Self { Self::scalar(value.to_string()) }
    pub fn null() -> Self { Self::scalar("null".into()) }

    pub fn array(items: Vec<Json>) -> Result<Self, String> {
        let mut bytes = 2 + items.len().saturating_sub(1);
        let mut nodes = 1;
        let mut depth = 1;
        for item in &items {
            bytes += item.bytes;
            nodes += item.nodes;
            depth = depth.max(item.depth + 1);
            limits(bytes, nodes, depth)?;
        }
        limits(bytes, nodes, depth)?;
        Ok(Self { data: Data::Array(items), bytes, nodes, depth })
    }

    pub fn object(items: Vec<(String, Json)>) -> Result<Self, String> {
        let mut bytes = 2 + items.len().saturating_sub(1);
        let mut nodes = 1;
        let mut depth = 1;
        let mut object = BTreeMap::new();
        for (key, value) in items {
            if object.contains_key(&key) { return Err("Duplicate JSON object key".into()); }
            bytes += quoted(&key)?.len() + 1 + value.bytes;
            nodes += value.nodes;
            depth = depth.max(value.depth + 1);
            limits(bytes, nodes, depth)?;
            object.insert(key, value);
        }
        limits(bytes, nodes, depth)?;
        Ok(Self { data: Data::Object(object), bytes, nodes, depth })
    }

    pub fn serialize(&self) -> String {
        let mut out = String::with_capacity(self.bytes);
        self.write(&mut out);
        debug_assert_eq!(out.len(), self.bytes);
        out
    }

    fn write(&self, out: &mut String) {
        match &self.data {
            Data::Scalar(encoded) => out.push_str(encoded),
            Data::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 { out.push(','); }
                    item.write(out);
                }
                out.push(']');
            }
            Data::Object(items) => {
                out.push('{');
                for (i, (key, value)) in items.iter().enumerate() {
                    if i > 0 { out.push(','); }
                    // Keys were validated at construction and cannot be mutated.
                    out.push_str(&quoted(key).expect("validated JSON key"));
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_controls_and_delimiters() {
        assert_eq!(Json::text("\"\\\n\r\t\0\x1f").unwrap().serialize(), "\"\\\"\\\\\\n\\r\\t\\u0000\\u001f\"");
    }
    #[test]
    fn unicode_preserved() { assert_eq!(Json::text("مرحبا 🦀").unwrap().serialize(), "\"مرحبا 🦀\""); }
    #[test]
    fn empty_containers() {
        assert_eq!(Json::array(vec![]).unwrap().serialize(), "[]");
        assert_eq!(Json::object(vec![]).unwrap().serialize(), "{}");
    }
    #[test]
    fn object_order_is_deterministic() {
        let j = Json::object(vec![("z".into(), Json::int(1)), ("a".into(), Json::boolean(true))]).unwrap();
        assert_eq!(j.serialize(), "{\"a\":true,\"z\":1}");
    }
    #[test]
    fn duplicate_keys_rejected() {
        assert!(Json::object(vec![("x".into(), Json::int(1)), ("x".into(), Json::int(2))]).is_err());
    }
    #[test]
    fn depth_bounded() {
        let mut j = Json::null();
        for _ in 1..MAX_DEPTH { j = Json::array(vec![j]).unwrap(); }
        assert!(Json::array(vec![j]).is_err());
    }
    #[test]
    fn nodes_bounded() {
        assert!(Json::array(vec![Json::null(); MAX_NODES-1]).is_ok());
        assert!(Json::array(vec![Json::null(); MAX_NODES]).is_err());
    }
    #[test]
    fn encoded_bytes_bounded_after_escaping() {
        assert!(Json::text(&"\0".repeat(3000)).is_err());
        assert!(Json::text(&"x".repeat(MAX_BYTES - 2)).is_ok());
        assert!(Json::text(&"x".repeat(MAX_BYTES - 1)).is_err());
    }
    #[test]
    fn nested_size_count_is_exact() {
        let value = Json::object(vec![("name".into(), Json::text("نص").unwrap()),
            ("items".into(), Json::array(vec![Json::null(), Json::int(-4)]).unwrap())]).unwrap();
        assert_eq!(value.serialize().len(), value.bytes);
    }
    #[test]
    fn node_and_byte_accounting_includes_keys() {
        assert!(Json::object(vec![("x".repeat(MAX_BYTES), Json::null())]).is_err());
        let pairs = (0..MAX_NODES).map(|i| (i.to_string(), Json::null())).collect();
        assert!(Json::object(pairs).is_err());
    }
}
