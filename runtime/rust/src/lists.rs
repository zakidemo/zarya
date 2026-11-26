//! One-dimensional immutable homogeneous lists. Not a generic nested container.
use super::{Error, Json, Result, Span, Type, Value};
pub const MAX_ITEMS: usize = 128;
pub const MAX_BYTES: usize = 65_536;

pub fn builtin(name: &str) -> bool { matches!(name, "len" | "at" | "append" | "sum" | "json_list") }

pub fn signature(name: &str, args: &[Type], span: Span) -> Result<Type> {
    let bad = || Error::new("Invalid list builtin arguments", span);
    match (name, args) {
        ("len", [list]) if list.is_list() => Ok(Type::Int),
        ("at", [list, Type::Int]) => list.element().ok_or_else(|| Error::new("at requires a typed list and Int index", span)),
        ("sum", [Type::IntList | Type::EmptyList]) => Ok(Type::Int),
        ("json_list", [list]) if list.is_list() => Ok(Type::Json),
        ("append", [Type::EmptyList, value]) => value.list().ok_or_else(bad),
        ("append", [list, value]) if list.element() == Some(*value) => Ok(*list),
        _ => Err(bad()),
    }
}

pub fn make(items: Vec<Value>, kind: Type, span: Span) -> Result<Value> {
    if items.len() > MAX_ITEMS { return Err(Error::new("List item limit exceeded", span)); }
    let mut bytes = 0;
    for item in &items {
        bytes += match item {
            Value::Int(_) => 8, Value::Bool(_) => 1,
            Value::Text(text) => text.len(), Value::Json(json) => json.byte_len(),
            Value::List {..} => return Err(Error::new("Nested lists are not supported", span)),
        };
        if bytes > MAX_BYTES { return Err(Error::new("List payload limit exceeded", span)); }
    }
    Ok(Value::List { items, kind })
}

pub fn call(name: &str, args: Vec<Value>, span: Span) -> Result<Value> {
    let mut args = args.into_iter();
    let (mut items, mut kind) = match args.next() {
        Some(Value::List {items, kind}) => (items, kind),
        _ => return Err(Error::new("Expected a list", span)),
    };
    match name {
        "len" => Ok(Value::Int(items.len() as i64)),
        "at" => {
            let index = match args.next() { Some(Value::Int(index)) => index, _ => return Err(Error::new("Expected Int index", span)) };
            let index = usize::try_from(index).map_err(|_| Error::new("List index out of bounds", span))?;
            items.get(index).cloned().ok_or_else(|| Error::new("List index out of bounds", span))
        }
        "append" => {
            let value = args.next().ok_or_else(|| Error::new("Expected appended value", span))?;
            if kind == Type::EmptyList { kind = value.typ().list().ok_or_else(|| Error::new("Nested lists are not supported", span))?; }
            items.push(value);
            make(items, kind, span)
        }
        "sum" => {
            let mut total = 0i64;
            for item in items {
                let number = match item { Value::Int(n) => n, _ => return Err(Error::new("sum requires List<Int>", span)) };
                total = total.checked_add(number).ok_or_else(|| Error::new("Int overflow in sum", span))?;
            }
            Ok(Value::Int(total))
        }
        "json_list" => {
            let mut values = Vec::new();
            for item in items {
                values.push(match item {
                    Value::Int(n) => Json::int(n), Value::Bool(b) => Json::boolean(b), Value::Json(j) => j,
                    Value::Text(text) => Json::text(&text).map_err(|message| Error::new(message, span))?,
                    _ => return Err(Error::new("Nested lists are not supported", span)),
                });
            }
            Json::array(values).map(Value::Json).map_err(|message| Error::new(message, span))
        }
        _ => Err(Error::new("Unknown list builtin", span)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::run;
    #[test]
    fn typed_lists_loops_and_sum() {
        let s = "let prices: List<Int> = [20, 25]; for price in prices { print(price * 2); } print(sum(prices));";
        assert_eq!(run(s).unwrap(), ["40", "50", "45"]);
        assert_eq!(run("let xs:List<Int>=[1,2]; print(sum(xs));").unwrap(), ["3"]);
    }
    #[test]
    fn empty_context_and_functions() {
        let s = "fn f(x: List<Int>): List<Int> = x; let xs: List<Int> = f([]); for x in xs { print(x); } print(len(xs)); print(sum([]));";
        assert_eq!(run(s).unwrap(), ["0", "0"]);
    }
    #[test]
    fn lists_as_function_results_and_append_are_immutable() {
        let s = "fn f(): List<Text> = []; let xs: List<Text> = f(); let ys: List<Text> = append(xs, \"hello\"); print(len(xs)); print(at(ys, 0));";
        assert_eq!(run(s).unwrap(), ["0", "hello"]);
    }
    #[test]
    fn json_list_and_scalar_kinds() {
        assert_eq!(run("print(json_list([true, false])); print(json_list([\"نص\"])); print(json_list([json_int(1)]));").unwrap(), ["[true,false]", "[\"نص\"]", "[1]"]);
    }
    #[test]
    fn empty_equality_ignores_inferred_kind() {
        assert_eq!(run("let xs: List<Int> = []; print(xs == []); print([] == xs); print([] != [1]);").unwrap(), ["true", "true", "true"]);
    }
    #[test]
    fn rejects_bad_list_types_and_scope() {
        for s in ["let x: List<Int> = [\"bad\"];", "print(len([1, true]));", "let x: List<List<Int>> = [];",
            "for x in 1 { print(x); }", "for x in [] { print(x); }", "print(at([], 0));", "print([1]);",
            "let xs: List<Int> = []; for x in xs { print(json_text(x)); }", "for x in [1] { let y: Int = x; } print(y);",
            "print(append([1], true));", "for x in [1] { permission text data; }", "for x in [1] { fn f(): Int = 1; }"] {
            assert!(run(s).is_err(), "{}", s);
        }
    }
    #[test]
    fn index_bounds_and_sum_overflow() {
        for s in ["print(at([1], -1));", "print(at([1], 1));", "let xs: List<Int> = []; print(at(xs, 0));",
            "print(sum([9223372036854775807, 1]));"] { assert!(run(s).is_err()); }
    }
    #[test]
    fn item_limit() {
        let literal = format!("let xs: List<Int> = [{}]; print(len(xs));", vec!["1";128].join(","));
        assert_eq!(run(&literal).unwrap(), ["128"]);
        assert!(run(&(literal + "let ys: List<Int> = append(xs, 1);")).is_err());
        assert!(run(&format!("print(len([{}]));", vec!["1";129].join(","))).is_err());
    }
    #[test]
    fn nested_loops_share_execution_budget() {
        let s = format!("let xs: List<Int> = [{}]; for x in xs {{ for y in xs {{ let z: Int = x + y; }} }}", vec!["1";100].join(","));
        assert!(run(&s).unwrap_err().message.contains("step limit"));
    }
    #[test]
    fn per_iteration_scope_is_fresh() {
        assert_eq!(run("for x in [1,2] { let y: Int = x; print(y); }").unwrap(), ["1", "2"]);
    }
    #[test]
    fn payload_bound_and_json_bounds_are_distinct() {
        let s = format!("let text: Text = \"{}\"; let xs: List<Text> = [{}];", "x".repeat(4096), vec!["text";17].join(","));
        assert!(run(&s).unwrap_err().message.contains("payload limit"));
        assert!(run(&format!("print(json_list([{}]));", vec!["1";128].join(","))).unwrap_err().message.contains("JSON node limit"));
    }
    #[test]
    fn malicious_type_nesting_is_rejected_before_recursion() {
        let source = format!("let xs: {}Int{} = [];", "List<".repeat(8000), ">".repeat(8000));
        assert!(run(&source).is_err());
    }
}
