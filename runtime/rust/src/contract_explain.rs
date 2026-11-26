//! Optional, bounded developer explanations for contract syntax checks.
//! Acceptance remains controlled by the existing external contract parser.
//! Paths use only fixed schema keys and numeric indices, never input names.
use super::{external_contract, json, json_input, terminal_unsafe, write_terminal_line, Json};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::ExitCode;

const MAX_DEPTH: usize = 8;
const MAX_NODES: usize = 32;
const MAX_FIELDS: usize = 16;
const MAX_FIELD_BYTES: usize = 64;
const MAX_ITEMS: i64 = 20;
const MAX_TEXT_BYTES: i64 = 4096;

#[derive(Debug, PartialEq, Eq)]
struct Diagnosis { code: &'static str, path: String }

fn error(code: &'static str, path: &str) -> Diagnosis {
    Diagnosis { code, path: path.to_string() }
}

fn field_path(path: &str, name: &str) -> String { format!("{path}.{name}") }

fn shape(value: &Json, count: usize) -> bool {
    value.is_object() && value.len().ok() == Some(count)
}

fn integer(value: &Json, name: &str) -> Option<i64> {
    value.get(name).ok().and_then(|child| child.as_int().ok())
}

fn text_value(value: &Json, name: &str) -> Option<String> {
    value.get(name).ok().and_then(|child| child.as_text().ok())
}

fn diagnose_schema(value: &Json, depth: usize, nodes: &mut usize, path: &str) -> Option<Diagnosis> {
    if depth > MAX_DEPTH || *nodes >= MAX_NODES { return Some(error("SCHEMA_LIMIT", path)); }
    if !value.is_object() { return Some(error("INVALID_SCHEMA_SHAPE", path)); }
    *nodes += 1;
    let kind = match text_value(value, "type") {
        Some(kind) => kind,
        None => return Some(error("INVALID_SCHEMA_TYPE", &field_path(path, "type"))),
    };
    match kind.as_str() {
        "int" => {
            if !shape(value, 3) { return Some(error("INVALID_SCHEMA_SHAPE", path)); }
            let min = match integer(value, "min") {
                Some(number) => number,
                None => return Some(error("INVALID_INTEGER_BOUND", &field_path(path, "min"))),
            };
            let max = match integer(value, "max") {
                Some(number) => number,
                None => return Some(error("INVALID_INTEGER_BOUND", &field_path(path, "max"))),
            };
            if min > max { return Some(error("INVALID_RANGE", path)); }
            None
        }
        "text" => {
            if !shape(value, 2) { return Some(error("INVALID_SCHEMA_SHAPE", path)); }
            match integer(value, "max_bytes") {
                Some(number) if (0..=MAX_TEXT_BYTES).contains(&number) => None,
                _ => Some(error("INVALID_TEXT_LIMIT", &field_path(path, "max_bytes"))),
            }
        }
        "bool" => {
            if shape(value, 1) { None } else { Some(error("INVALID_SCHEMA_SHAPE", path)) }
        }
        "array" => {
            if !shape(value, 3) { return Some(error("INVALID_SCHEMA_SHAPE", path)); }
            match integer(value, "max_items") {
                Some(number) if (0..=MAX_ITEMS).contains(&number) => {},
                _ => return Some(error("INVALID_ARRAY_LIMIT", &field_path(path, "max_items"))),
            }
            let item_path = field_path(path, "items");
            match value.get("items") {
                Ok(item) => diagnose_schema(item, depth + 1, nodes, &item_path),
                Err(_) => Some(error("INVALID_SCHEMA_SHAPE", &item_path)),
            }
        }
        "object" => {
            if !shape(value, 2) { return Some(error("INVALID_SCHEMA_SHAPE", path)); }
            let fields_path = field_path(path, "fields");
            let entries = match value.get("fields").and_then(|v| v.items()) {
                Ok(entries) if entries.len() <= MAX_FIELDS => entries,
                _ => return Some(error("INVALID_FIELDS", &fields_path)),
            };
            let mut names = HashSet::new();
            for (index, entry) in entries.iter().enumerate() {
                let entry_path = format!("{fields_path}[{index}]");
                if !shape(entry, 2) { return Some(error("INVALID_SCHEMA_SHAPE", &entry_path)); }
                let name_path = field_path(&entry_path, "name");
                let name = match text_value(entry, "name") {
                    Some(name) if !name.is_empty() && name.len() <= MAX_FIELD_BYTES &&
                        !name.chars().any(terminal_unsafe) => name,
                    _ => return Some(error("INVALID_FIELD_NAME", &name_path)),
                };
                if !names.insert(name) { return Some(error("DUPLICATE_FIELD_NAME", &name_path)); }
                let nested_path = field_path(&entry_path, "schema");
                let nested = match entry.get("schema") {
                    Ok(value) => value,
                    Err(_) => return Some(error("INVALID_SCHEMA_SHAPE", &nested_path)),
                };
                if let Some(problem) = diagnose_schema(nested, depth + 1, nodes, &nested_path) {
                    return Some(problem);
                }
            }
            None
        }
        _ => Some(error("INVALID_SCHEMA_TYPE", &field_path(path, "type"))),
    }
}

// The actual parser makes the accept/reject decision. The explanation is
// advisory and falls back to a generic error if it cannot identify a cause.
fn explain_source(source: &str) -> Option<Diagnosis> {
    if external_contract::parse(source).is_ok() { return None; }
    let value = match json_input::parse(source) {
        Ok(value) => value,
        Err(_) => return Some(error("INVALID_JSON", "$")),
    };
    if !shape(&value, 2) { return Some(error("INVALID_ROOT", "$")); }
    if integer(&value, "version") != Some(1) { return Some(error("INVALID_VERSION", "$.version")); }
    let schema = match value.get("schema") {
        Ok(schema) => schema,
        Err(_) => return Some(error("INVALID_SCHEMA_SHAPE", "$.schema")),
    };
    let mut nodes = 0;
    Some(diagnose_schema(schema, 1, &mut nodes, "$.schema")
        .unwrap_or_else(|| error("INVALID_CONTRACT", "$")))
}

fn read_source(filename: &str) -> std::result::Result<String, Diagnosis> {
    let mut bytes = Vec::new();
    let file = File::open(filename).map_err(|_| error("CONTRACT_UNREADABLE", "$"))?;
    file.take((json::MAX_BYTES + 1) as u64).read_to_end(&mut bytes)
        .map_err(|_| error("CONTRACT_UNREADABLE", "$"))?;
    if bytes.len() > json::MAX_BYTES { return Err(error("CONTRACT_TOO_LARGE", "$")); }
    String::from_utf8(bytes).map_err(|_| error("INVALID_UTF8", "$"))
}

fn message(code: &str) -> &'static str {
    match code {
        "CONTRACT_UNREADABLE" => "Cannot read the contract file",
        "CONTRACT_TOO_LARGE" => "Contract exceeds the byte limit",
        "INVALID_UTF8" => "Contract must be UTF-8",
        "INVALID_JSON" => "Contract is not supported JSON",
        "INVALID_ROOT" => "Contract root must contain version and schema",
        "INVALID_VERSION" => "Contract version must be integer 1",
        "INVALID_SCHEMA_TYPE" => "Schema type is missing or unsupported",
        "INVALID_SCHEMA_SHAPE" => "Schema has missing or extra properties",
        "INVALID_INTEGER_BOUND" => "Integer bounds must be integers",
        "INVALID_RANGE" => "Integer minimum exceeds maximum",
        "INVALID_TEXT_LIMIT" => "Text max_bytes is outside its allowed range",
        "INVALID_ARRAY_LIMIT" => "Array max_items is outside its allowed range",
        "INVALID_FIELDS" => "Object fields are missing or exceed the limit",
        "INVALID_FIELD_NAME" => "Object field name is invalid",
        "DUPLICATE_FIELD_NAME" => "Object field names must be unique",
        "SCHEMA_LIMIT" => "Schema exceeds a depth or node limit",
        _ => "Invalid contract",
    }
}

fn text(value: &str) -> Json { Json::text(value).expect("bounded contract explanation") }

fn report(problem: Option<Diagnosis>) -> Json {
    let ok = problem.is_none();
    let diagnostics = match problem {
        Some(problem) => vec![Json::object(vec![
            ("code".into(), text(problem.code)),
            ("message".into(), text(message(problem.code))),
            ("path".into(), text(&problem.path)),
            ("severity".into(), text("error")),
        ]).expect("bounded contract explanation")],
        None => Vec::new(),
    };
    Json::object(vec![
        ("schema_version".into(), Json::int(1)),
        ("version".into(), text(env!("CARGO_PKG_VERSION"))),
        ("phase".into(), text("contract_explain")),
        ("ok".into(), Json::boolean(ok)),
        ("diagnostics".into(), Json::array(diagnostics).expect("bounded diagnostics")),
    ]).expect("bounded contract explanation")
}

pub fn cli(filename: &str) -> ExitCode {
    let problem = match read_source(filename) {
        Ok(source) => explain_source(&source),
        Err(problem) => Some(problem),
    };
    let ok = problem.is_none();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if write_terminal_line(&mut output, &report(problem).serialize())
        .and_then(|_| output.flush()).is_err() {
        eprintln!("Zarya error: cannot write contract explanation");
        return ExitCode::from(1);
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnose(source: &str, code: &str, path: &str) {
        let actual = explain_source(source).expect("expected invalid contract");
        assert_eq!(actual.code, code);
        assert_eq!(actual.path, path);
    }

    #[test]
    fn valid_contract_has_no_diagnostic() {
        assert!(explain_source(r#"{"version":1,"schema":{"type":"bool"}}"#).is_none());
    }
    #[test]
    fn malformed_json_is_identified_without_echo() {
        diagnose(r#"{"version":1,"private":"PRIVATE_SENTINEL""#, "INVALID_JSON", "$");
    }
    #[test]
    fn invalid_root_is_identified() {
        diagnose("[]", "INVALID_ROOT", "$");
    }
    #[test]
    fn invalid_version_is_identified() {
        diagnose(r#"{"version":2,"schema":{"type":"bool"}}"#, "INVALID_VERSION", "$.version");
    }
    #[test]
    fn unknown_schema_type_is_identified() {
        diagnose(r#"{"version":1,"schema":{"type":"wat"}}"#, "INVALID_SCHEMA_TYPE", "$.schema.type");
    }
    #[test]
    fn invalid_range_has_schema_path() {
        diagnose(r#"{"version":1,"schema":{"type":"int","min":9,"max":3}}"#, "INVALID_RANGE", "$.schema");
    }
    #[test]
    fn invalid_integer_type_has_bound_path() {
        diagnose(r#"{"version":1,"schema":{"type":"int","min":0,"max":true}}"#, "INVALID_INTEGER_BOUND", "$.schema.max");
    }
    #[test]
    fn invalid_nested_range_has_indexed_path() {
        let source = r#"{"version":1,"schema":{"type":"object","fields":[{"name":"PRIVATE_SENTINEL","schema":{"type":"int","min":9,"max":3}}]}}"#;
        diagnose(source, "INVALID_RANGE", "$.schema.fields[0].schema");
        let rendered = report(explain_source(source)).serialize();
        assert!(!rendered.contains("PRIVATE_SENTINEL"));
    }
    #[test]
    fn duplicate_field_name_reports_only_index() {
        let source = r#"{"version":1,"schema":{"type":"object","fields":[{"name":"x","schema":{"type":"bool"}},{"name":"x","schema":{"type":"bool"}}]}}"#;
        diagnose(source, "DUPLICATE_FIELD_NAME", "$.schema.fields[1].name");
    }
    #[test]
    fn bad_text_and_array_limits_are_identified() {
        diagnose(r#"{"version":1,"schema":{"type":"text","max_bytes":4097}}"#, "INVALID_TEXT_LIMIT", "$.schema.max_bytes");
        diagnose(r#"{"version":1,"schema":{"type":"array","max_items":21,"items":{"type":"bool"}}}"#, "INVALID_ARRAY_LIMIT", "$.schema.max_items");
    }
    #[test]
    fn duplicate_json_keys_are_invalid_json() {
        diagnose(r#"{"version":1,"version":1,"schema":{"type":"bool"}}"#, "INVALID_JSON", "$");
    }
    #[test]
    fn structured_report_uses_current_runtime_version() {
        let value = report(None);
        assert!(value.get("ok").unwrap().as_bool().unwrap());
        assert_eq!(value.get("phase").unwrap().as_text().unwrap(), "contract_explain");
        assert_eq!(value.get("version").unwrap().as_text().unwrap(), env!("CARGO_PKG_VERSION"));
    }
}
