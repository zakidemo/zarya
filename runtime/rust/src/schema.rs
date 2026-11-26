//! Bounded declared JSON shapes and projection. Not new nominal value types.
use std::collections::HashMap;
use super::{Error, Expr, ExprKind, Json, Result, Span, Type, Value, memory, OwnedValue};

pub const MAX_SCHEMAS: usize = 32;
pub const MAX_FIELDS: usize = 16;
pub const MAX_NAME_BYTES: usize = 64;
pub type Fields = Vec<(String, Type)>;
pub type Registry = HashMap<String, Fields>;

pub fn builtin(name: &str) -> bool {
    matches!(name, "json_validate" | "json_records" | "json_validate_result" | "json_records_result")
}

pub fn literal_name<'a>(expr: &'a Expr, schemas: &Registry) -> Result<&'a str> {
    match &expr.kind {
        ExprKind::Literal(Value::Text(name)) if schemas.contains_key(name) => Ok(name),
        ExprKind::Literal(Value::Text(_)) => Err(Error::new("Unknown schema; declare it before use", expr.span)),
        _ => Err(Error::new("Schema name must be a literal Text naming a declared schema", expr.span)),
    }
}

pub fn result_bytes(name: &str, args: &[OwnedValue]) -> usize {
    match args.first().map(|arg| &arg.value) {
        Some(Value::Json(j)) if matches!(name, "json_validate_result" | "json_records_result") => memory::SLOT
            .saturating_add(j.memory_bytes()).saturating_add(2048),
        Some(Value::Json(j)) if name == "json_validate" => memory::SLOT + j.memory_bytes(),
        Some(Value::Json(j)) if name == "json_records" => j.items().map_or(memory::SLOT,
            |items| items.iter().fold(memory::SLOT, |n, item| n.saturating_add(memory::SLOT + item.memory_bytes()))),
        _ => memory::SLOT,
    }
}

fn kind_matches(value: &Json, typ: Type) -> bool {
    match typ {
        Type::Int => value.as_int().is_ok(), Type::Text => value.as_text().is_ok(),
        Type::Bool => value.as_bool().is_ok(), Type::Json => true,
        list => match list.element() {
            Some(element) => value.items().map_or(false, |items| items.iter().all(|item| kind_matches(item, element))),
            None => false,
        },
    }
}

fn project(value: &Json, fields: &Fields, context: &str, span: Span) -> Result<Json> {
    if !value.is_object() { return Err(Error::new(format!("{}: expected JSON object", context), span)); }
    let mut projected = Vec::with_capacity(fields.len());
    for (key, typ) in fields {
        let item = value.get(key).map_err(|_| Error::new(format!("{}: missing required field {:?}", context, key), span))?;
        if !kind_matches(item, *typ) {
            return Err(Error::new(format!("{}: field {:?} expected {}", context, key, typ.name()), span));
        }
        projected.push((key.clone(), item.clone()));
    }
    Json::object(projected).map_err(|message| Error::new(message, span))
}

// These fixed codes never contain the input's keys or values. The type checker
// still requires a literal, previously declared schema name. Failed resource,
// parse, memory or JSON-output limits remain interpreter errors, not validation
// results. Only the top-level fields declared by the schema are checked.
fn validation_failure_at(code: &str, index: Option<usize>, span: Span) -> Result<Json> {
    let code = Json::text(code).map_err(|message| Error::new(message, span))?;
    let mut fields = vec![("code".into(), code)];
    if let Some(index) = index {
        let index = i64::try_from(index).map_err(|_| Error::new("Record index out of range", span))?;
        fields.push(("index".into(), Json::int(index)));
    }
    let detail = Json::object(fields)
        .map_err(|message| Error::new(message, span))?;
    Json::object(vec![("ok".into(), Json::boolean(false)), ("error".into(), detail)])
        .map_err(|message| Error::new(message, span))
}

fn validation_failure(code: &str, span: Span) -> Result<Json> {
    validation_failure_at(code, None, span)
}

fn strict_validation_code(value: &Json, fields: &Fields, span: Span) -> Result<Option<&'static str>> {
    if !value.is_object() { return Ok(Some("EXPECTED_OBJECT")); }
    // Declared-field order determines which error wins if multiple fields fail.
    for (key, typ) in fields {
        let item = match value.get(key) {
            Ok(item) => item,
            Err(_) => return Ok(Some("MISSING_FIELD")),
        };
        if !kind_matches(item, *typ) { return Ok(Some("TYPE_MISMATCH")); }
    }
    let actual = value.len().map_err(|message| Error::new(message, span))?;
    if actual != fields.len() { return Ok(Some("EXTRA_FIELD")); }
    Ok(None)
}

fn strict_validation_result(value: &Json, fields: &Fields, span: Span) -> Result<Json> {
    if let Some(code) = strict_validation_code(value, fields, span)? {
        return validation_failure(code, span);
    }
    let projected = project(value, fields, "Schema", span)?;
    Json::object(vec![("ok".into(), Json::boolean(true)), ("value".into(), projected)])
        .map_err(|message| Error::new(message, span))
}

fn strict_records_result(value: &Json, fields: &Fields, span: Span) -> Result<Json> {
    let items = match value.items() {
        Ok(items) => items,
        Err(_) => return validation_failure("EXPECTED_ARRAY", span),
    };
    // Validate the complete batch before building any returned record.
    for (index, item) in items.iter().enumerate() {
        if let Some(code) = strict_validation_code(item, fields, span)? {
            return validation_failure_at(code, Some(index), span);
        }
    }
    let mut projected = Vec::with_capacity(items.len());
    for item in items {
        projected.push(project(item, fields, "Schema", span)?);
    }
    let array = Json::array(projected).map_err(|message| Error::new(message, span))?;
    Json::object(vec![("ok".into(), Json::boolean(true)), ("value".into(), array)])
        .map_err(|message| Error::new(message, span))
}

pub fn call(name: &str, args: Vec<Value>, schemas: &Registry, span: Span) -> Result<Value> {
    let (value, schema_name) = match args.as_slice() {
        [Value::Json(value), Value::Text(schema_name)] => (value, schema_name),
        _ => return Err(Error::new("Invalid schema builtin arguments", span)),
    };
    let fields = schemas.get(schema_name).ok_or_else(|| Error::new("Unknown schema", span))?;
    let context = format!("Schema {}", schema_name);
    match name {
        "json_validate" => project(value, fields, &context, span).map(Value::Json),
        "json_validate_result" => strict_validation_result(value, fields, span).map(Value::Json),
        "json_records_result" => strict_records_result(value, fields, span).map(Value::Json),
        "json_records" => {
            let items = value.items().map_err(|_| Error::new(format!("{}: expected JSON array of records", context), span))?;
            let mut values = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                values.push(Value::Json(project(item, fields, &format!("{} item {}", context, i), span)?));
            }
            super::lists::make(values, Type::JsonList, span)
        }
        _ => Err(Error::new("Unknown schema builtin", span)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{compile, run};

    #[test]
    fn validates_and_projects_without_echoing_extra_fields() {
        let source = r#"schema Course { id: Int; title: Text; price_minor: Int; }
            let data: Json = json_object("id",json_int(1),"title",json_text("نص 🦀"),"price_minor",json_int(2100),"private",json_text("SECRET"));
            print(json_validate(data,"Course")); print(data);"#;
        let lines = run(source).unwrap();
        assert_eq!(lines[0], r#"{"id":1,"price_minor":2100,"title":"نص 🦀"}"#);
        assert!(lines[1].contains("SECRET")); // Input itself is immutable and unmodified.
    }
    #[test]
    fn scalar_kinds_and_lists_have_no_implicit_conversion() {
        let prefix = "schema Row { id: Int; title: Text; active: Bool; tags: List<Text>; extra: Json; }";
        let good = r#"json_object("id",json_int(1),"title",json_text("hi"),"active",json_bool(false),"tags",json_array(json_text("a")),"extra",json_null())"#;
        assert!(run(&format!("{} print(json_validate({},\"Row\"));", prefix, good)).is_ok());
        for wrong in [good.replace("json_int(1)","json_text(\"1\")"), good.replace("json_text(\"hi\")","json_null()"),
            good.replace("json_bool(false)","json_int(0)"), good.replace("json_array(json_text(\"a\"))","json_array(json_int(1))"),
            good.replace("json_array(json_text(\"a\"))","json_text(\"a\")")] {
            assert!(run(&format!("{} print(json_validate({},\"Row\"));", prefix, wrong)).is_err());
        }
        let lists = "schema L {ints:List<Int>; flags:List<Bool>; any:List<Json>;}";
        let value = "json_object(\"ints\",json_array(json_int(1)),\"flags\",json_array(json_bool(false)),\"any\",json_array(json_null(),json_object()))";
        assert!(run(&format!("{} print(json_validate({},\"L\"));",lists,value)).is_ok());
        for bad in [value.replace("json_int(1)","json_bool(true)"), value.replace("json_bool(false)","json_int(0)")] {
            assert!(run(&format!("{} print(json_validate({},\"L\"));",lists,bad)).is_err());
        }
    }
    #[test]
    fn batch_validates_every_record_and_reports_zero_based_index() {
        assert_eq!(run("schema Row { id: Int; } print(json_list(json_records(json_array(),\"Row\")));").unwrap(), ["[]"]);
        assert_eq!(run("schema Row { id: Int; } let xs:List<Json>=json_records(json_array(json_object(\"id\",json_int(1))),\"Row\"); for x in xs { print(x); }").unwrap(), ["{\"id\":1}"]);
        let error = run("schema Row { id: Int; } print(1); let xs:List<Json>=json_records(json_array(json_object(\"id\",json_int(1)),json_object(\"id\",json_text(\"PRIVATE\"))),\"Row\");").unwrap_err();
        assert!(error.message.contains("item 1") && error.message.contains("id") && !error.message.contains("PRIVATE"));
    }
    #[test]
    fn missing_null_wrong_roots_and_empty_schemas_are_explicit() {
        for source in ["schema R { id: Int; } print(json_validate(json_object(),\"R\"));",
            "schema R { id: Int; } print(json_validate(json_object(\"id\",json_null()),\"R\"));",
            "schema R {} print(json_validate(json_array(),\"R\"));",
            "schema R {} print(json_list(json_records(json_object(),\"R\")));"] {
            assert!(compile(source).is_ok());
            assert!(run(source).is_err());
        }
        assert_eq!(run("schema R {} print(json_validate(json_object(\"private\",json_int(1)),\"R\"));").unwrap(), ["{}"]);
    }
    #[test]
    fn quoted_unicode_and_escaped_field_names_work() {
        assert_eq!(run(r#"schema R { "عنوان": Text; "a-b": Int; "": Bool; }
            print(json_validate(json_object("عنوان",json_text("نص"),"a-b",json_int(2),"",json_bool(true)),"R"));"#).unwrap(), [r#"{"":true,"a-b":2,"عنوان":"نص"}"#]);
        assert!(compile(r#"schema R { title: Text; "title": Text; }"#).is_err());
        let error = run(r#"schema R { "bad\nname": Int; } print(json_validate(json_object(),"R"));"#).unwrap_err();
        assert!(!error.message.contains('\n') && error.message.contains("\\n"));
    }
    #[test]
    fn malformed_declarations_are_rejected_before_execution() {
        for source in ["schema R { id: Int }", "schema R { id: Int; id: Text; }", "schema R {} schema R {}",
            "schema R { child: R; }", "schema R { nested: List<List<Int>>; }", "if false { schema R {} }",
            "for x in [1] { schema R {} }", "schema R { \"a\" + \"b\": Int; }", "schema R {"] {
            assert!(compile(source).is_err(), "{}", source);
        }
    }
    #[test]
    fn schema_names_are_prior_declared_literals_not_runtime_values() {
        for source in ["schema R {} print(json_validate(json_object(),\"Missing\"));",
            "print(json_validate(json_object(),\"R\")); schema R {}",
            "schema R {} let n:Text=\"R\"; print(json_validate(json_object(),n));",
            "schema R {} print(json_validate(json_object(),\"R\"+\"\"));",
            "schema R {} print(json_validate(json_object(),1));", "schema R {} print(json_validate());",
            "schema R {} print(json_validate(1,\"R\"));", "schema R {} print(json_records(json_object(),\"R\",1));",
            "let schema: Int=1;", "fn json_validate():Int=1;", "fn json_records():Int=1;"] {
            assert!(compile(source).is_err(), "{}", source);
        }
        assert!(compile("schema R { id:Int; } print(json_validate(json_object(),\"R\"));").is_ok());
    }
    #[test]
    fn declaration_counts_and_utf8_name_bounds_are_enforced() {
        let allowed: String = (0..32).map(|i| format!("schema R{} {{}}", i)).collect();
        assert!(compile(&allowed).is_ok());
        assert!(compile(&(allowed + "schema Extra {}" )).is_err());
        let fields: String = (0..16).map(|i| format!("f{}:Int;",i)).collect();
        assert!(compile(&format!("schema R {{{}}}",fields)).is_ok());
        assert!(compile(&format!("schema R {{{} extra:Int;}}",fields)).is_err());
        assert!(compile(&format!("schema {} {{}}", "R".repeat(64))).is_ok());
        assert!(compile(&format!("schema {} {{}}", "R".repeat(65))).is_err());
        assert!(compile(&format!("schema R {{ \"{}\":Text; }}", "ن".repeat(32))).is_ok());
        assert!(compile(&format!("schema R {{ \"{}\":Text; }}", "ن".repeat(33))).is_err());
    }
    #[test]
    fn definitions_work_in_functions_and_are_independent_between_programs() {
        let source = "schema R { id:Int; } fn clean(j:Json):Json=json_validate(j,\"R\"); print(clean(json_object(\"id\",json_int(1))));";
        assert_eq!(run(source).unwrap(), ["{\"id\":1}"]);
        assert!(compile("print(json_validate(json_object(),\"R\"));").is_err());
        assert!(compile("fn clean(j:Json):Json=json_validate(j,\"R\"); schema R { id:Int; }").is_err());
    }

    #[test]
    fn strict_result_accepts_distinct_flat_schemas() {
        let source = r#"schema Course { id:Int; title:Text; price_minor:Int; }
            schema Product { id:Int; name:Text; price_minor:Int; }
            print(json_validate_result(json_object("id",json_int(1),"title",json_text("تعلم"),"price_minor",json_int(2100)),"Course"));
            print(json_validate_result(json_object("id",json_int(2),"name",json_text("كتاب"),"price_minor",json_int(3000)),"Product"));"#;
        let lines = run(source).unwrap();
        assert_eq!(lines[0], r#"{"ok":true,"value":{"id":1,"price_minor":2100,"title":"تعلم"}}"#);
        assert_eq!(lines[1], r#"{"ok":true,"value":{"id":2,"name":"كتاب","price_minor":3000}}"#);
    }

    #[test]
    fn strict_result_classifies_errors_without_echoing_untrusted_input() {
        let prefix = r#"schema R { id:Int; title:Text; } "#;
        for (value, code) in [
            (r#"json_null()"#, "EXPECTED_OBJECT"),
            (r#"json_object("id",json_int(1))"#, "MISSING_FIELD"),
            (r#"json_object("id",json_text("PRIVATE_VALUE"),"title",json_text("ok"))"#, "TYPE_MISMATCH"),
            (r#"json_object("id",json_int(1),"title",json_text("ok"),"PRIVATE_FIELD",json_text("PRIVATE_VALUE"))"#, "EXTRA_FIELD"),
        ] {
            let source = format!(r#"{} print(json_validate_result({},"R"));"#, prefix, value);
            let output = run(&source).unwrap().remove(0);
            assert_eq!(output, format!(r#"{{"error":{{"code":"{}"}},"ok":false}}"#, code));
            assert!(!output.contains("PRIVATE"));
        }
    }

    #[test]
    fn strict_result_priority_follows_declaration_before_extras() {
        let prefix = r#"schema R { id:Int; title:Text; } "#;
        let source = format!(r#"{} print(json_validate_result(json_object("id",json_text("bad"),"other",json_null()),"R"));"#, prefix);
        assert!(run(&source).unwrap()[0].contains("TYPE_MISMATCH"));
        let source = format!(r#"{} print(json_validate_result(json_object("id",json_int(1),"other",json_null()),"R"));"#, prefix);
        assert!(run(&source).unwrap()[0].contains("MISSING_FIELD"));
    }

    #[test]
    fn strict_result_requires_static_schema_and_reserves_its_name() {
        for source in [r#"schema R {} print(json_validate_result());"#,
            r#"schema R {} print(json_validate_result(json_null()));"#,
            r#"schema R {} print(json_validate_result(1,"R"));"#,
            r#"schema R {} let name:Text="R"; print(json_validate_result(json_null(),name));"#,
            r#"print(json_validate_result(json_null(),"R")); schema R {}"#,
            r#"let json_validate_result: Int = 1;"#,
            r#"schema R {} if false { print(json_validate_result(1,"R")); }"#] {
            assert!(compile(source).is_err(), "{}", source);
        }
        assert!(compile(r#"schema R {} let result:Json=json_validate_result(json_object(),"R");"#).is_ok());
    }

    #[test]
    fn strict_result_does_not_change_original_projection() {
        let source = r#"schema R { id:Int; }
            let data:Json=json_object("id",json_int(1),"extra",json_text("secret"));
            print(json_validate(data,"R"));
            print(json_validate_result(data,"R"));
            print(json_validate(data,"R"));"#;
        let lines = run(source).unwrap();
        assert_eq!(lines[0], r#"{"id":1}"#);
        assert_eq!(lines[1], r#"{"error":{"code":"EXTRA_FIELD"},"ok":false}"#);
        assert_eq!(lines[2], lines[0]);
    }

    #[test]
    fn strict_result_allows_arbitrary_nested_json_when_declared_json() {
        let source = r#"schema R { payload:Json; }
            print(json_validate_result(json_object("payload",json_object("nested",json_int(1))),"R"));"#;
        assert_eq!(run(source).unwrap()[0], r#"{"ok":true,"value":{"payload":{"nested":1}}}"#);
    }

    #[test]
    fn strict_result_enforces_output_node_budget() {
        fn source(count: usize) -> String {
            let items = vec!["0"; count].join(",");
            format!(r#"schema R {{ payload:Json; }} print(json_validate_result(json_parse("{{\"payload\":[{}]}}"),"R"));"#, items)
        }
        assert!(run(&source(124)).is_ok());
        let error = run(&source(125)).unwrap_err();
        assert!(error.message.contains("JSON node limit"), "{}", error.message);
    }

    #[test]
    fn strict_result_releases_budget_on_success_and_failure() {
        use super::super::{memory, Span};
        let source = r#"schema R { id:Int; } print(json_validate_result(json_object("id",json_int(1)),"R"));"#;
        let program = compile(source).unwrap();
        let small = memory::Budget::new(1024);
        assert!(program.execute_with_memory(&Default::default(), small.clone()).is_err());
        assert!(small.reserve(1024, Span::default()).is_ok());
        let normal = memory::Budget::new(memory::LIMIT);
        assert!(program.execute_with_memory(&Default::default(), normal.clone()).is_ok());
        assert!(normal.reserve(memory::LIMIT, Span::default()).is_ok());
    }

    #[test]
    fn strict_records_accepts_multiple_records_and_empty_array() {
        let source = r#"schema Course { id:Int; title:Text; }
            print(json_records_result(json_array(json_object("id",json_int(1),"title",json_text("أ")),json_object("id",json_int(2),"title",json_text("ب"))),"Course"));
            print(json_records_result(json_array(),"Course"));"#;
        let lines = run(source).unwrap();
        assert_eq!(lines[0], r#"{"ok":true,"value":[{"id":1,"title":"أ"},{"id":2,"title":"ب"}]}"#);
        assert_eq!(lines[1], r#"{"ok":true,"value":[]}"#);
    }

    #[test]
    fn strict_records_reports_fixed_codes_and_zero_based_index() {
        let prefix = r#"schema R { id:Int; title:Text; } "#;
        for (value, expected) in [
            (r#"json_null()"#, r#"{"error":{"code":"EXPECTED_ARRAY"},"ok":false}"#),
            (r#"json_array(json_null())"#, r#"{"error":{"code":"EXPECTED_OBJECT","index":0},"ok":false}"#),
            (r#"json_array(json_object("id",json_int(1)))"#, r#"{"error":{"code":"MISSING_FIELD","index":0},"ok":false}"#),
            (r#"json_array(json_object("id",json_bool(true),"title",json_text("ok")))"#, r#"{"error":{"code":"TYPE_MISMATCH","index":0},"ok":false}"#),
            (r#"json_array(json_object("id",json_int(1),"title",json_text("ok"),"PRIVATE_FIELD",json_text("PRIVATE_VALUE")))"#, r#"{"error":{"code":"EXTRA_FIELD","index":0},"ok":false}"#),
        ] {
            let source = format!(r#"{} print(json_records_result({},"R"));"#, prefix, value);
            let output = run(&source).unwrap().remove(0);
            assert_eq!(output, expected);
            assert!(!output.contains("PRIVATE"));
        }
    }

    #[test]
    fn strict_records_fails_the_whole_batch_at_the_first_bad_record() {
        let source = r#"schema R { id:Int; title:Text; }
            let rows:Json=json_array(
                json_object("id",json_int(1),"title",json_text("PRIVATE_VALUE")),
                json_object("id",json_int(2),"title",json_text("ok"),"PRIVATE_FIELD",json_text("PRIVATE_VALUE")),
                json_object("id",json_text("wrong"),"title",json_text("ok"))
            );
            print(json_records_result(rows,"R"));"#;
        let output = run(source).unwrap().remove(0);
        assert_eq!(output, r#"{"error":{"code":"EXTRA_FIELD","index":1},"ok":false}"#);
        assert!(!output.contains("PRIVATE"));
    }

    #[test]
    fn strict_records_requires_a_declared_literal_schema_and_reserves_name() {
        for source in [r#"schema R {} print(json_records_result());"#,
            r#"schema R {} print(json_records_result(json_array()));"#,
            r#"schema R {} print(json_records_result(1,"R"));"#,
            r#"schema R {} let name:Text="R"; print(json_records_result(json_array(),name));"#,
            r#"print(json_records_result(json_array(),"R")); schema R {}"#,
            r#"let json_records_result: Int = 1;"#,
            r#"schema R {} if false { print(json_records_result(1,"R")); }"#] {
            assert!(compile(source).is_err(), "{}", source);
        }
        assert!(compile(r#"schema R {} let result:Json=json_records_result(json_array(),"R");"#).is_ok());
    }

    #[test]
    fn strict_records_does_not_change_previous_projection_or_single_record_behavior() {
        let source = r#"schema R { id:Int; }
            let row:Json=json_object("id",json_int(1),"extra",json_text("secret"));
            print(json_list(json_records(json_array(row),"R")));
            print(json_validate_result(row,"R"));
            print(json_records_result(json_array(row),"R"));"#;
        let lines = run(source).unwrap();
        assert_eq!(lines[0], r#"[{"id":1}]"#);
        assert_eq!(lines[1], r#"{"error":{"code":"EXTRA_FIELD"},"ok":false}"#);
        assert_eq!(lines[2], r#"{"error":{"code":"EXTRA_FIELD","index":0},"ok":false}"#);
    }

    #[test]
    fn strict_records_respects_json_output_limits_and_releases_budget() {
        use super::super::{memory, Span};
        fn source(count: usize) -> String {
            let items = vec!["0"; count].join(",");
            format!(r#"schema R {{ payload:Json; }} print(json_records_result(json_parse("[{{\"payload\":[{}]}}]"),"R"));"#, items)
        }
        assert!(run(&source(123)).is_ok());
        let error = run(&source(124)).unwrap_err();
        assert!(error.message.contains("JSON node limit"), "{}", error.message);
        let program = compile(r#"schema R { id:Int; } print(json_records_result(json_array(json_object("id",json_int(1))),"R"));"#).unwrap();
        let small = memory::Budget::new(1024);
        assert!(program.execute_with_memory(&Default::default(), small.clone()).is_err());
        assert!(small.reserve(1024, Span::default()).is_ok());
        let normal = memory::Budget::new(memory::LIMIT);
        assert!(program.execute_with_memory(&Default::default(), normal.clone()).is_ok());
        assert!(normal.reserve(memory::LIMIT, Span::default()).is_ok());
    }

    #[test]
    fn strict_records_does_not_inspect_nested_arbitrary_json_fields() {
        let source = r#"schema R { payload:Json; }
            print(json_records_result(json_array(json_object("payload",json_object("nested",json_int(1)))),"R"));"#;
        assert_eq!(run(source).unwrap()[0], r#"{"ok":true,"value":[{"payload":{"nested":1}}]}"#);
    }

    #[test]
    fn strict_records_checks_list_element_types_without_coercion() {
        let source = r#"schema R { labels:List<Text>; }
            print(json_records_result(json_array(json_object("labels",json_array(json_text("ok"),json_int(1)))),"R"));"#;
        assert_eq!(run(source).unwrap()[0], r#"{"error":{"code":"TYPE_MISMATCH","index":0},"ok":false}"#);
    }
}
