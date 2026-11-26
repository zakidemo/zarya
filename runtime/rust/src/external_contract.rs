//! Experimental bounded external response contract.
//! A trusted operator selects a policy independently of the guest program.
//! This checks only JSON shape and stated bounds, not content secrecy,
//! authorization, semantic relationships, or operating-system containment.
use std::collections::HashSet;
use super::{json_input, terminal_unsafe, Json};

const MAX_CONTRACT_NODES: usize = 32;
const MAX_CONTRACT_DEPTH: usize = 8;
const MAX_FIELDS: usize = 16;
const MAX_FIELD_BYTES: usize = 64;
const MAX_ITEMS: i64 = 20;
const MAX_TEXT_BYTES: i64 = 4096;

pub enum Schema {
    Int { min: i64, max: i64 },
    Text { max_bytes: usize },
    Bool,
    Array { max_items: usize, item: Box<Schema> },
    Object { fields: Vec<(String, Schema)> },
}

pub struct Policy { schema: Schema }

fn exactly_object(value: &Json, size: usize) -> std::result::Result<(), ()> {
    if value.is_object() && value.len().map_err(|_| ())? == size { Ok(()) } else { Err(()) }
}

fn field<'a>(value: &'a Json, name: &str) -> std::result::Result<&'a Json, ()> {
    value.get(name).map_err(|_| ())
}

fn bounded_number(value: &Json, name: &str, upper: i64) -> std::result::Result<usize, ()> {
    let num = field(value, name)?.as_int().map_err(|_| ())?;
    if num < 0 || num > upper { return Err(()); }
    Ok(num as usize)
}

fn compile(value: &Json, depth: usize, count: &mut usize) -> std::result::Result<Schema, ()> {
    if depth > MAX_CONTRACT_DEPTH || *count >= MAX_CONTRACT_NODES || !value.is_object() { return Err(()); }
    *count += 1;
    let kind = field(value, "type")?.as_text().map_err(|_| ())?;
    match kind.as_str() {
        "int" => {
            exactly_object(value, 3)?;
            let min = field(value, "min")?.as_int().map_err(|_| ())?;
            let max = field(value, "max")?.as_int().map_err(|_| ())?;
            if min > max { return Err(()); }
            Ok(Schema::Int { min, max })
        }
        "text" => {
            exactly_object(value, 2)?;
            Ok(Schema::Text { max_bytes: bounded_number(value, "max_bytes", MAX_TEXT_BYTES)? })
        }
        "bool" => { exactly_object(value, 1)?; Ok(Schema::Bool) }
        "array" => {
            exactly_object(value, 3)?;
            let max_items = bounded_number(value, "max_items", MAX_ITEMS)?;
            let item = compile(field(value, "items")?, depth + 1, count)?;
            Ok(Schema::Array { max_items, item: Box::new(item) })
        }
        "object" => {
            exactly_object(value, 2)?;
            let entries = field(value, "fields")?.items().map_err(|_| ())?;
            if entries.len() > MAX_FIELDS { return Err(()); }
            let mut names = HashSet::new();
            let mut fields = Vec::with_capacity(entries.len());
            for entry in entries {
                exactly_object(entry, 2)?;
                let name = field(entry, "name")?.as_text().map_err(|_| ())?;
                if name.is_empty() || name.len() > MAX_FIELD_BYTES ||
                    name.chars().any(terminal_unsafe) || !names.insert(name.clone()) { return Err(()); }
                let schema = compile(field(entry, "schema")?, depth + 1, count)?;
                fields.push((name, schema));
            }
            Ok(Schema::Object { fields })
        }
        _ => Err(()),
    }
}

pub fn parse(source: &str) -> std::result::Result<Policy, ()> {
    let value = json_input::parse(source).map_err(|_| ())?;
    exactly_object(&value, 2)?;
    if field(&value, "version")?.as_int().map_err(|_| ())? != 1 { return Err(()); }
    let mut count = 0;
    Ok(Policy { schema: compile(field(&value, "schema")?, 1, &mut count)? })
}

impl Schema {
    fn matches(&self, value: &Json) -> bool {
        match self {
            Self::Int { min, max } => value.as_int().map_or(false, |n| n >= *min && n <= *max),
            Self::Text { max_bytes } => value.as_text().map_or(false, |s| s.len() <= *max_bytes),
            Self::Bool => value.as_bool().is_ok(),
            Self::Array { max_items, item } => value.items().map_or(false, |rows|
                rows.len() <= *max_items && rows.iter().all(|row| item.matches(row))),
            Self::Object { fields } => value.is_object() && value.len().map_or(false, |n| n == fields.len()) &&
                fields.iter().all(|(name, schema)| value.get(name).map_or(false, |item| schema.matches(item))),
        }
    }
}

impl Policy {
    pub fn validate(&self, lines: &[String]) -> std::result::Result<String, ()> {
        if lines.len() != 1 { return Err(()); }
        let value = json_input::parse(&lines[0]).map_err(|_| ())?;
        if !self.schema.matches(&value) { return Err(()); }
        Ok(value.serialize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(schema: &str) -> std::result::Result<Policy, ()> {
        parse(&format!(r#"{{"version":1,"schema":{}}}"#, schema))
    }
    fn accepts(spec: &str, value: &str) -> bool {
        policy(spec).unwrap().validate(&[value.to_string()]).is_ok()
    }
    #[test]
    fn integers_respect_type_and_range() {
        let s = r#"{"type":"int","min":-5,"max":7}"#;
        for v in ["-5", "0", "7"] { assert!(accepts(s, v)); }
        for v in ["-6", "8", "true", "\"7\"", "1.0"] { assert!(!accepts(s, v)); }
    }
    #[test]
    fn text_limit_counts_utf8_bytes() {
        let s = r#"{"type":"text","max_bytes":4}"#;
        assert!(accepts(s, r#""éé""#));
        assert!(!accepts(s, r#""ééa""#));
        assert!(!accepts(s, "1"));
    }
    #[test]
    fn booleans_are_not_integers() {
        let s = r#"{"type":"bool"}"#;
        assert!(accepts(s, "true")); assert!(accepts(s, "false"));
        assert!(!accepts(s, "1")); assert!(!accepts(s, "null"));
    }
    #[test]
    fn array_checks_every_item_and_count() {
        let s = r#"{"type":"array","max_items":2,"items":{"type":"bool"}}"#;
        assert!(accepts(s, "[]")); assert!(accepts(s, "[true,false]"));
        assert!(!accepts(s, "[true,1]")); assert!(!accepts(s, "[true,false,true]"));
    }
    #[test]
    fn object_requires_exact_fields() {
        let s = r#"{"type":"object","fields":[{"name":"x","schema":{"type":"bool"}}]}"#;
        assert!(accepts(s, r#"{"x":true}"#));
        assert!(!accepts(s, r#"{}"#));
        assert!(!accepts(s, r#"{"x":true,"secret":1}"#));
        assert!(!accepts(s, r#"{"x":1}"#));
    }
    #[test]
    fn duplicate_and_unsafe_field_names_are_rejected() {
        let a = r#"{"type":"object","fields":[{"name":"x","schema":{"type":"bool"}},{"name":"x","schema":{"type":"bool"}}]}"#;
        let b = r#"{"type":"object","fields":[{"name":"x\u202ey","schema":{"type":"bool"}}]}"#;
        assert!(policy(a).is_err()); assert!(policy(b).is_err());
    }
    #[test]
    fn malformed_and_unknown_policy_properties_are_rejected() {
        for s in [r#"{"type":"wat"}"#, r#"{"type":"bool","extra":1}"#,
                  r#"{"type":"text","max_bytes":-1}"#, r#"{"type":"text","max_bytes":4097}"#,
                  r#"{"type":"int","min":10,"max":0}"#,
                  r#"{"type":"array","max_items":21,"items":{"type":"bool"}}"#] {
            assert!(policy(s).is_err());
        }
        assert!(parse(r#"{"version":2,"schema":{"type":"bool"}}"#).is_err());
        assert!(parse(r#"{"version":1,"version":1,"schema":{"type":"bool"}}"#).is_err());
    }
    #[test]
    fn nested_objects_and_arrays_work() {
        let s = r#"{"type":"object","fields":[{"name":"items","schema":{"type":"array","max_items":2,"items":{"type":"object","fields":[{"name":"name","schema":{"type":"text","max_bytes":20}}]}}}]}"#;
        assert!(accepts(s, r#"{"items":[{"name":"Book"}]}"#));
        assert!(!accepts(s, r#"{"items":[{"name":"Book","id":1}]}"#));
    }
    #[test]
    fn json_duplicates_and_multiple_outputs_are_rejected() {
        let p = policy(r#"{"type":"object","fields":[]}"#).unwrap();
        assert!(p.validate(&[]).is_err());
        assert!(p.validate(&["{}".into(), "{}".into()]).is_err());
        assert!(p.validate(&[r#"{"a":1,"a":2}"#.into()]).is_err());
    }
    #[test]
    fn malformed_versions_and_numbers_are_rejected() {
        assert!(parse(r#"{"version":true,"schema":{"type":"bool"}}"#).is_err());
        assert!(parse(r#"{"version":1,"schema":{"type":"int","min":0,"max":9223372036854775808}}"#).is_err());
        assert!(parse(r#"{"version":1,"schema":{"type":"bool"},"extra":1}"#).is_err());
    }
    #[test]
    fn empty_object_and_scalar_schemas_are_allowed() {
        assert!(accepts(r#"{"type":"object","fields":[]}"#, "{}"));
        assert!(accepts(r#"{"type":"text","max_bytes":0}"#, "\"\""));
        assert!(accepts(r#"{"type":"array","max_items":0,"items":{"type":"bool"}}"#, "[]"));
    }
    #[test]
    fn excessively_nested_contracts_are_rejected() {
        let mut schema = r#"{"type":"bool"}"#.to_string();
        for _ in 0..8 {
            schema = format!(r#"{{"type":"array","max_items":1,"items":{}}}"#, schema);
        }
        assert!(policy(&schema).is_err());
    }
    #[test]
    fn too_many_fields_are_rejected() {
        let fields = (0..17).map(|i| format!(r#"{{"name":"f{}","schema":{{"type":"bool"}}}}"#, i))
            .collect::<Vec<_>>().join(",");
        assert!(policy(&format!(r#"{{"type":"object","fields":[{}]}}"#, fields)).is_err());
    }
}
