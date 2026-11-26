//! Per-execution accounting for live values, copies, bindings and output.
//! Accounted bytes are a deterministic interpreter policy, not process RSS.
use std::cell::Cell;
use std::rc::Rc;
use super::{Error, Result, Span, Type, Value};

pub const LIMIT: usize = 8 * 1024 * 1024;
pub const SLOT: usize = 128;
pub const JSON_SCRATCH: usize = 2 * super::json::MAX_BYTES + SLOT;

#[derive(Clone)]
pub struct Budget { state: Rc<Cell<usize>>, limit: usize }

pub struct Lease { state: Rc<Cell<usize>>, bytes: usize }
impl Drop for Lease {
    fn drop(&mut self) { self.state.set(self.state.get() - self.bytes); }
}

// Deliberately no Clone: copying a runtime value must reserve first.
pub struct OwnedValue {
    pub value: Value,
    lease: Lease,
    name_lease: Option<Lease>,
}

pub struct Output { pub text: String, _lease: Lease }

impl Value {
    pub fn memory_bytes(&self) -> usize {
        SLOT.saturating_add(match self {
            Self::Int(_) | Self::Bool(_) => 0,
            Self::Text(text) => text.len(),
            Self::Json(json) => json.memory_bytes(),
            Self::List { items, .. } => items.iter().fold(0usize, |n, item| n.saturating_add(item.memory_bytes())),
        })
    }
}

impl Budget {
    pub fn new(limit: usize) -> Self { Self { state: Rc::new(Cell::new(0)), limit } }

    pub fn reserve(&self, bytes: usize, span: Span) -> Result<Lease> {
        let used = self.state.get().checked_add(bytes).filter(|n| *n <= self.limit)
            .ok_or_else(|| Error::new("Runtime memory budget exceeded", span))?;
        self.state.set(used);
        Ok(Lease { state: Rc::clone(&self.state), bytes })
    }

    pub fn make(&self, bytes: usize, span: Span, build: impl FnOnce() -> Result<Value>) -> Result<OwnedValue> {
        let mut lease = self.reserve(bytes, span)?;
        let value = build()?;
        let actual = value.memory_bytes();
        if actual > bytes { return Err(Error::new("Internal memory accounting error", span)); }
        // Builders reserve upper bounds before allocating, then return unused credit.
        lease.state.set(lease.state.get() - (bytes - actual));
        lease.bytes = actual;
        Ok(OwnedValue { value, lease, name_lease: None })
    }

    pub fn copy(&self, value: &Value, span: Span) -> Result<OwnedValue> {
        self.make(value.memory_bytes(), span, || Ok(value.clone()))
    }

    pub fn construct(&self, args: Vec<OwnedValue>, bytes: usize, scratch: usize, span: Span,
                     build: impl FnOnce(Vec<Value>) -> Result<Value>) -> Result<OwnedValue> {
        // Both argument leases and the result reservation remain live while the
        // constructor moves values and allocates container/encoder storage.
        let _scratch = self.reserve(scratch.saturating_add(args.len().saturating_mul(64)), span)?;
        self.make(bytes, span, || {
            let (values, leases): (Vec<_>, Vec<_>) = args.into_iter()
                .map(|v| (v.value, (v.lease, v.name_lease))).unzip();
            let result = build(values);
            drop(leases);
            result
        })
    }

    pub fn output(&self, value: &Value, span: Span) -> Result<Output> {
        let bytes = match value {
            Value::Int(_) => 20, Value::Bool(_) => 5, Value::Text(s) => s.len(),
            Value::Json(j) => j.byte_len(), Value::List { .. } => 32,
        };
        let lease = self.reserve(SLOT + bytes, span)?;
        let _scratch = self.reserve(if matches!(value, Value::Json(_)) { JSON_SCRATCH } else { 0 }, span)?;
        Ok(Output { text: value.output(), _lease: lease })
    }
}

impl OwnedValue {
    pub fn named(mut self, budget: &Budget, name: &str, span: Span) -> Result<Self> {
        self.name_lease = Some(budget.reserve(SLOT.saturating_add(name.len()), span)?);
        Ok(self)
    }
    pub fn coerce(mut self, expected: Type) -> Self { self.value = self.value.coerce(expected); self }
}

fn json_scalar_bytes(value: &Value) -> usize {
    128usize.saturating_add(match value {
        Value::Int(_) => 20, Value::Bool(_) => 5,
        Value::Text(text) => 2usize.saturating_add(text.len().saturating_mul(6)),
        Value::Json(json) => return json.memory_bytes(),
        _ => 0,
    })
}

pub fn json_result_bytes(name: &str, args: &[OwnedValue]) -> usize {
    let data = match name {
        "json_null" => 128 + 4,
        "json_text" | "json_int" | "json_bool" => args.first().map_or(128, |v| json_scalar_bytes(&v.value)),
        "json_array" => args.iter().fold(128usize, |n, v| n.saturating_add(json_scalar_bytes(&v.value))),
        "json_object" => args.chunks_exact(2).fold(128usize, |n, pair| {
            let key = match &pair[0].value { Value::Text(s) => s.len(), _ => 0 };
            n.saturating_add(128).saturating_add(key).saturating_add(json_scalar_bytes(&pair[1].value))
        }),
        _ => 0,
    };
    SLOT.saturating_add(data)
}

pub fn list_result_bytes(name: &str, args: &[OwnedValue]) -> usize {
    match (name, args.first().map(|v| &v.value)) {
        ("append", Some(list)) => list.memory_bytes().saturating_add(args.get(1).map_or(0, |v| v.value.memory_bytes())),
        ("at", Some(Value::List { items, .. })) => {
            let index = args.get(1).and_then(|v| match v.value { Value::Int(n) => usize::try_from(n).ok(), _ => None });
            index.and_then(|i| items.get(i)).map_or(SLOT, Value::memory_bytes)
        }
        ("json_list", Some(Value::List { items, .. })) => items.iter()
            .fold(SLOT + 128, |n, v| n.saturating_add(json_scalar_bytes(v))),
        _ => SLOT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::compile;

    fn limited(source: &str, limit: usize) -> Result<Vec<String>> {
        let budget = Budget::new(limit);
        let result = compile(source).unwrap().execute_with_memory(&Default::default(), budget.clone());
        assert_eq!(budget.state.get(), 0, "execution leaked memory credit");
        result
    }

    fn fixture(copies: usize) -> String {
        let mut source = format!("let t: Text = \"{}\"; let seed: List<Text> = [{}];", "x".repeat(4096), vec!["t"; 16].join(","));
        for i in 0..copies { source += &format!("let l{}: List<Text> = seed;", i); }
        source
    }
    #[test]
    fn exact_boundary_overflow_and_release() {
        let budget = Budget::new(32);
        let lease = budget.reserve(32, Span::default()).unwrap();
        assert!(budget.reserve(1, Span::default()).is_err());
        assert!(budget.reserve(usize::MAX, Span::default()).is_err());
        assert_eq!(budget.state.get(), 32);
        drop(lease);
        assert_eq!(budget.state.get(), 0);
        assert!(budget.reserve(32, Span::default()).is_ok());
    }
    #[test]
    fn refused_builder_does_not_allocate_and_failed_builder_releases() {
        let budget = Budget::new(SLOT);
        let called = Cell::new(false);
        assert!(budget.make(SLOT + 1, Span::default(), || { called.set(true); Ok(Value::Int(1)) }).is_err());
        assert!(!called.get());
        assert!(budget.make(SLOT, Span::default(), || Err(Error::new("test", Span::default()))).is_err());
        assert_eq!(budget.state.get(), 0);
    }
    #[test]
    fn independent_budgets_and_copy_peak() {
        let budget = Budget::new(SLOT);
        let value = budget.copy(&Value::Int(1), Span::default()).unwrap();
        assert!(budget.copy(&value.value, Span::default()).is_err());
        assert!(Budget::new(SLOT).copy(&value.value, Span::default()).is_ok());
        drop(value);
        assert_eq!(budget.state.get(), 0);
    }
    #[test]
    fn aggregate_lists_stop_with_a_positioned_error() {
        let source = fixture(200) + "print(1);";
        let error = limited(&source, LIMIT).unwrap_err();
        assert_eq!(error.message, "Runtime memory budget exceeded");
        assert!(error.span.start > 4096 && error.span.end <= source.len());
        assert_eq!(limited(&(fixture(48) + "if true { if true { print(1); } }"), LIMIT).unwrap(), ["1"]);
    }
    #[test]
    fn scope_values_are_released_each_iteration() {
        let source = fixture(0) + &format!("for i in [{}] {{ let a: List<Text> = seed; let b: List<Text> = seed; }} print(1);", vec!["1"; 100].join(","));
        // Total allocations exceed 8 MiB; the live working set fits 512 KiB.
        assert_eq!(limited(&source, 512 * 1024).unwrap(), ["1"]);
    }
    #[test]
    fn functions_share_the_budget_with_the_caller() {
        let source = format!("fn id(x: Text): Text = x; let t: Text = \"{}\"; print(id(t));", "x".repeat(4000));
        assert_eq!(limited(&source, 11_000).unwrap_err().message, "Runtime memory budget exceeded");
        assert_eq!(limited(&source, 14_000).unwrap(), ["x".repeat(4000)]);
    }
    #[test]
    fn builtin_copies_and_concatenation_reserve_their_peak() {
        let source = fixture(0) + "let item: Text = at(seed, 0);";
        assert_eq!(limited(&source, 143_000).unwrap_err().message, "Runtime memory budget exceeded");
        assert!(limited(&source, 256_000).is_ok());
        let text = format!("let t: Text = \"{}\"; print(t + t);", "x".repeat(2000));
        assert_eq!(limited(&text, 10_000).unwrap_err().message, "Runtime memory budget exceeded");
        assert_eq!(limited(&text, 16_000).unwrap(), ["x".repeat(4000)]);
        assert_eq!(limited("print(len([1]));", 2048).unwrap(), ["1"]);
    }
    #[test]
    fn buffered_output_is_charged_until_execution_finishes() {
        let source = format!("let t: Text = \"{}\"; print(t); print(t); print(t);", "x".repeat(4000));
        assert_eq!(limited(&source, 15_000).unwrap_err().message, "Runtime memory budget exceeded");
        assert_eq!(limited(&source, 32_000).unwrap().len(), 3);
    }
    #[test]
    fn granted_snapshots_and_reads_are_charged() {
        let program = compile("permission text data; print(read_text(\"data\"));").unwrap();
        let host = std::collections::HashMap::from([("data".into(), "x".repeat(4000))]);
        for limit in [4096, 8000] {
            let budget = Budget::new(limit);
            let error = program.execute_with_memory(&host, budget.clone()).unwrap_err();
            assert_eq!(error.message, "Runtime memory budget exceeded");
            assert_eq!(budget.state.get(), 0);
        }
        assert_eq!(program.execute(&host).unwrap(), ["x".repeat(4000)]);
    }
    #[test]
    fn json_structures_charge_nodes_not_just_encoded_bytes() {
        let json = super::super::Json::array(vec![super::super::Json::null(); 32]).unwrap();
        assert!(json.memory_bytes() > json.byte_len() * 20);
        let source = format!("let j: Json = json_array({}); let xs: List<Json> = [{}]; let ys: List<Json> = xs;", vec!["json_null()";32].join(","), vec!["j";128].join(","));
        assert_eq!(limited(&source, 800_000).unwrap_err().message, "Runtime memory budget exceeded");
        assert!(limited(&source, LIMIT).is_ok());
    }
    #[test]
    fn failures_release_arguments_scopes_resources_and_output() {
        for source in [
            "print(1); if true { let x: Text = \"hello\"; print(9223372036854775807 + 1); }",
            "print(json_object(\"x\", json_null(), \"x\", json_null()));",
            "print(at([1], 5));",
            "print(json_parse(\"{bad\"));",
            "print(json_as_int(json_text(\"20\")));",
        ] {
            assert!(limited(source, LIMIT).is_err());
        }
        let source = "fn id(x: List<Text>): List<Text> = x; let a: List<Text> = [\"hi\", \"نص\"]; let b: List<Text> = append(id(a), \"🦀\"); print(json_list(b)); print(at(b, 0) + \"!\"); print(sum([1,2])); print(len(b)); print(json_object(\"k\", json_array(json_bool(true), json_int(-1), json_text(\"نص\"))));";
        for limit in [0, 128, 512, 1024, 8192, 16_384, 32_768, LIMIT] {
            if let Err(error) = limited(source, limit) { assert_eq!(error.message, "Runtime memory budget exceeded"); }
        }
        assert_eq!(limited(source, LIMIT).unwrap().len(), 5);
    }
    #[test]
    fn repeat_execution_gets_a_fresh_budget() {
        let bad = compile(&(fixture(200) + "print(1);")).unwrap();
        let good = compile("print(42);").unwrap();
        for _ in 0..2 {
            assert_eq!(bad.execute(&Default::default()).unwrap_err().message, "Runtime memory budget exceeded");
            assert_eq!(good.execute(&Default::default()).unwrap(), ["42"]);
        }
    }
    #[test]
    fn json_input_reserves_peak_and_releases_on_success_or_error() {
        let source = r#"let j: Json = json_parse("{\"items\":[1,2],\"name\":\"نص\"}");
            let xs: List<Json> = json_items(json_get(j,"items"));
            print(json_as_text(json_get(j,"name"))); print(json_as_int(json_at(json_get(j,"items"),0)));"#;
        assert_eq!(limited(source, 65_536).unwrap_err().message, "Runtime memory budget exceeded");
        for limit in [0, 128, 8192, 65_536, 90_000, LIMIT] {
            if let Err(error) = limited(source, limit) { assert_eq!(error.message, "Runtime memory budget exceeded"); }
        }
        assert_eq!(limited(source, LIMIT).unwrap(), ["نص", "1"]);
        assert!(limited(r#"print(1); print(json_parse("{\"a\":1,\"a\":2}"));"#, LIMIT).is_err());
    }
    #[test]
    fn schema_projection_reserves_before_copy_and_releases_on_failure() {
        let source = r#"schema Row { title:Text; } let j:Json=json_object("title",json_text("نص"),"private",json_text("hidden"));
            let xs:List<Json>=json_records(json_array(j,j),"Row"); print(json_list(xs));"#;
        for limit in [0, 128, 1024, 16_384, 32_768, 65_536, LIMIT] {
            if let Err(error) = limited(source, limit) { assert_eq!(error.message, "Runtime memory budget exceeded"); }
        }
        assert_eq!(limited(source, LIMIT).unwrap(), [r#"[{"title":"نص"},{"title":"نص"}]"#]);
        assert!(limited("schema R { id:Int; } print(1); let xs:List<Json>=json_records(json_array(json_object(\"id\",json_int(1)),json_object()),\"R\");", LIMIT).is_err());
    }
    #[test]
    fn filter_releases_bindings_results_and_output_on_predicate_error() {
        let source = "let xs:List<Int>=[1,2]; print(1); let ys:List<Int>=filter(xs,x,at(xs,x)>0);";
        let program = compile(source).unwrap();
        let budget = Budget::new(LIMIT);
        for _ in 0..3 {
            assert!(program.execute_with_memory(&Default::default(), budget.clone()).is_err());
            assert_eq!(budget.state.get(), 0, "filter failure leaked memory credit");
        }
    }

}
