//! Static capability label: what a program may read, which resource data can
//! be copied into its output, and which data its decisions depend on.
//!
//! No execution and no resource loading. The analysis over-approximates: it may
//! report more than a run uses, never less. It covers values and conditions; it
//! does not cover timing or the fact that a program stopped with an error.
use super::{compile, read_source, ExitCode, Expr, ExprKind, Function, Json, Program, Stmt, StmtKind, Value, schema};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write};

/// Which part of a resource a value was derived from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    /// Any part of the resource.
    Whole,
    /// Values stored under a key with this name.
    Field(String),
    /// Only how many records there are.
    Count,
    /// Only whether the resource parsed and matched its schema.
    Validity,
}
type Sources = BTreeSet<(String, Part)>;

/// What a value structurally is, when it is still a piece of one resource.
/// Only such values may be narrowed by `json_get` or by a schema.
#[derive(Clone, Debug, PartialEq)]
enum Shape {
    /// The resource itself, one of its records, or a list of its records.
    /// `fields` is `Some` once a schema projection limited the record's keys.
    Tree { resource: String, fields: Option<BTreeSet<String>> },
    /// Something stored under the key `field`.
    Inside { resource: String, field: String },
}
impl Shape {
    fn resource(&self) -> &str { match self { Self::Tree { resource, .. } | Self::Inside { resource, .. } => resource } }
    fn sources(&self) -> Sources {
        match self {
            Self::Tree { resource, fields: None } => [(resource.clone(), Part::Whole)].into(),
            Self::Tree { resource, fields: Some(fields) } =>
                fields.iter().map(|f| (resource.clone(), Part::Field(f.clone()))).collect(),
            Self::Inside { resource, field } => [(resource.clone(), Part::Field(field.clone()))].into(),
        }
    }
    fn only(&self, part: Part) -> Sources { [(self.resource().to_string(), part)].into() }
}

#[derive(Clone, Debug, Default)]
struct Flow {
    shape: Option<Shape>,
    /// Sources that also influenced this value and must never be narrowed.
    other: Sources,
    /// True for the `{ok, value | error}` object made by a `*_result` builtin.
    wrapper: bool,
}
impl Flow {
    fn all(&self) -> Sources {
        let mut out = self.other.clone();
        if let Some(shape) = &self.shape { out.extend(shape.sources()); }
        out
    }
    fn opaque(sources: Sources) -> Self { Self { shape: None, other: sources, wrapper: false } }
}

const NODE_BUDGET: usize = 200_000;

#[derive(Debug)]
pub struct Label {
    pub declared: BTreeSet<String>,
    pub reads: BTreeSet<String>,
    pub undeclared_reads: BTreeSet<String>,
    pub dynamic_read: bool,
    output: Sources,
    decisions: Sources,
    pub output_objects: BTreeSet<Vec<String>>,
    pub other_output: bool,
    pub prints: usize,
}
impl Label {
    pub fn unused(&self) -> BTreeSet<String> {
        if self.dynamic_read { return BTreeSet::new(); }
        self.declared.difference(&self.reads).cloned().collect()
    }
    /// Clean means every read is a literal, declared name, so the list of reads is exact.
    pub fn clean(&self) -> bool { self.undeclared_reads.is_empty() && !self.dynamic_read }
}

struct Analyzer<'a> {
    functions: &'a HashMap<String, Function>,
    schemas: &'a schema::Registry,
    label: Label,
    budget: usize,
}

fn literal_text(expr: &Expr) -> Option<&str> {
    match &expr.kind { ExprKind::Literal(Value::Text(text)) => Some(text), _ => None }
}

impl<'a> Analyzer<'a> {
    fn spend(&mut self) -> Result<(), String> {
        if self.budget == 0 { return Err("Label analysis limit exceeded".into()); }
        self.budget -= 1;
        Ok(())
    }
    fn union(&mut self, args: &[Expr], env: &HashMap<String, Flow>) -> Result<Sources, String> {
        let mut out = Sources::new();
        for arg in args { out.extend(self.expr(arg, env)?.all()); }
        Ok(out)
    }
    /// Evaluates `body` with `variable` bound to one element of `list`.
    fn element(&mut self, list: &Flow, variable: &str, body: &Expr, env: &HashMap<String, Flow>) -> Result<Flow, String> {
        let mut local = env.clone();
        local.insert(variable.to_string(), Flow { shape: list.shape.clone(), other: list.other.clone(), wrapper: false });
        self.expr(body, &local)
    }
    fn expr(&mut self, expr: &Expr, env: &HashMap<String, Flow>) -> Result<Flow, String> {
        self.spend()?;
        match &expr.kind {
            ExprKind::Literal(_) => Ok(Flow::default()),
            ExprKind::Name(name) => env.get(name).cloned().ok_or_else(|| "Unknown variable in label analysis".to_string()),
            ExprKind::Neg(inner) => Ok(Flow::opaque(self.expr(inner, env)?.all())),
            ExprKind::Binary(_, a, b) => {
                let mut sources = self.expr(a, env)?.all();
                sources.extend(self.expr(b, env)?.all());
                Ok(Flow::opaque(sources))
            }
            ExprKind::List(items) => Ok(Flow::opaque(self.union(items, env)?)),
            ExprKind::Filter(items, variable, predicate) => {
                let list = self.expr(items, env)?;
                let condition = self.element(&list, variable, predicate, env)?;
                self.label.decisions.extend(condition.all());
                Ok(Flow { wrapper: false, ..list })
            }
            ExprKind::Map(items, variable, transform) => {
                let list = self.expr(items, env)?;
                let item = self.element(&list, variable, transform, env)?;
                let mut other = item.other;
                other.extend(list.other);
                Ok(Flow { shape: item.shape, other, wrapper: false })
            }
            ExprKind::Call(name, args) => self.call(name, args, env),
        }
    }
    fn call(&mut self, name: &str, args: &[Expr], env: &HashMap<String, Flow>) -> Result<Flow, String> {
        match (name, args) {
            ("read_text", [key]) => {
                if let Some(resource) = literal_text(key) {
                    if !self.label.declared.contains(resource) {
                        // The run-time check denies this read, so no data flows from it.
                        self.label.undeclared_reads.insert(resource.to_string());
                        return Ok(Flow::default());
                    }
                    self.label.reads.insert(resource.to_string());
                    return Ok(Flow { shape: Some(Shape::Tree { resource: resource.to_string(), fields: None }), ..Flow::default() });
                }
                // A computed name can select any declared resource at run time.
                self.label.dynamic_read = true;
                let mut sources = self.expr(key, env)?.all();
                sources.extend(self.label.declared.iter().map(|r| (r.clone(), Part::Whole)));
                Ok(Flow::opaque(sources))
            }
            ("json_parse", [text]) => {
                let flow = self.expr(text, env)?;
                if let Some(shape) = &flow.shape { self.label.decisions.extend(shape.only(Part::Validity)); }
                Ok(Flow { wrapper: false, ..flow })
            }
            ("json_items", [value]) => Ok(Flow { wrapper: false, ..self.expr(value, env)? }),
            ("json_at" | "at", [value, index]) => {
                let mut flow = self.expr(value, env)?;
                flow.other.extend(self.expr(index, env)?.all());
                Ok(Flow { wrapper: false, ..flow })
            }
            ("json_len" | "len", [value]) => {
                let flow = self.expr(value, env)?;
                let mut sources = flow.other.clone();
                match &flow.shape {
                    Some(shape @ Shape::Tree { .. }) => sources.extend(shape.only(Part::Count)),
                    Some(shape) => sources.extend(shape.sources()),
                    None => {}
                }
                Ok(Flow::opaque(sources))
            }
            ("json_get", [value, key]) => {
                let flow = self.expr(value, env)?;
                let key_flow = self.expr(key, env)?;
                let literal = literal_text(key);
                if flow.wrapper {
                    return Ok(match literal {
                        Some("value") => {
                            // Reaching the value at all depends on validity: record it as a decision.
                            let mut other = flow.other;
                            if let Some(shape) = &flow.shape {
                                for source in shape.only(Part::Validity) {
                                    other.remove(&source);
                                    self.label.decisions.insert(source);
                                }
                            }
                            Flow { shape: flow.shape, other, wrapper: false }
                        }
                        Some("ok") | Some("error") => {
                            let mut sources = flow.other.clone();
                            if let Some(shape) = &flow.shape { sources.extend(shape.only(Part::Validity)); }
                            Flow::opaque(sources)
                        }
                        _ => {
                            let mut sources = flow.all();
                            sources.extend(key_flow.all());
                            Flow::opaque(sources)
                        }
                    });
                }
                let mut other = flow.other;
                other.extend(key_flow.all());
                let shape = match (flow.shape, literal) {
                    (Some(Shape::Tree { resource, .. }), Some(field)) => Some(Shape::Inside { resource, field: field.to_string() }),
                    (shape, _) => shape,
                };
                Ok(Flow { shape, other, wrapper: false })
            }
            (name, [value, schema_name]) if schema::builtin(name) => {
                let flow = self.expr(value, env)?;
                let declared: BTreeSet<String> = literal_text(schema_name).and_then(|n| self.schemas.get(n))
                    .map(|fields| fields.iter().map(|(field, _)| field.clone()).collect())
                    .ok_or_else(|| "Unknown schema in label analysis".to_string())?;
                let result = matches!(name, "json_validate_result" | "json_records_result");
                let mut other = flow.other;
                // The projection keeps only the schema's top-level fields.
                let shape = match flow.shape {
                    Some(Shape::Tree { resource, fields }) => {
                        let kept = match fields { Some(old) => old.intersection(&declared).cloned().collect(), None => declared };
                        Some(Shape::Tree { resource, fields: Some(kept) })
                    }
                    shape => shape,
                };
                if let Some(shape) = &shape {
                    if result { other.extend(shape.only(Part::Validity)); }
                    else { self.label.decisions.extend(shape.only(Part::Validity)); }
                }
                Ok(Flow { shape, other, wrapper: result })
            }
            _ => {
                let functions = self.functions;
                if let Some(function) = functions.get(name) {
                    if function.params.len() != args.len() { return Err("Argument count in label analysis".into()); }
                    let mut local = HashMap::new();
                    for ((param, _), arg) in function.params.iter().zip(args) {
                        local.insert(param.clone(), self.expr(arg, env)?);
                    }
                    return self.expr(&function.body, &local);
                }
                // Every remaining builtin only computes a new value from its arguments.
                Ok(Flow::opaque(self.union(args, env)?))
            }
        }
    }
    fn statements(&mut self, body: &[Stmt], env: &mut HashMap<String, Flow>) -> Result<(), String> {
        for stmt in body {
            self.spend()?;
            match &stmt.kind {
                StmtKind::Schema(..) | StmtKind::Permission(_) | StmtKind::Fn(..) => {}
                StmtKind::Let(name, _, expr) => {
                    let flow = self.expr(expr, env)?;
                    env.insert(name.clone(), flow);
                }
                StmtKind::Print(expr) => {
                    self.label.prints += 1;
                    let keys = match &expr.kind {
                        ExprKind::Call(name, args) if name == "json_object" =>
                            args.iter().step_by(2).map(|key| literal_text(key).map(str::to_string)).collect::<Option<Vec<_>>>(),
                        _ => None,
                    };
                    match keys {
                        Some(mut keys) => { keys.sort(); keys.dedup(); self.label.output_objects.insert(keys); }
                        None => self.label.other_output = true,
                    }
                    let sources = self.expr(expr, env)?.all();
                    self.label.output.extend(sources);
                }
                StmtKind::Assert(condition) => {
                    let sources = self.expr(condition, env)?.all();
                    self.label.decisions.extend(sources);
                }
                StmtKind::If(condition, yes, no) => {
                    let sources = self.expr(condition, env)?.all();
                    self.label.decisions.extend(sources);
                    self.statements(yes, &mut env.clone())?;
                    self.statements(no, &mut env.clone())?;
                }
                StmtKind::For(name, items, body) => {
                    let list = self.expr(items, env)?;
                    let mut local = env.clone();
                    local.insert(name.clone(), Flow { shape: list.shape, other: list.other, wrapper: false });
                    self.statements(body, &mut local)?;
                }
            }
        }
        Ok(())
    }
}

pub fn analyze(program: &Program) -> Result<Label, String> {
    let label = Label {
        declared: program.checker.requested.keys().cloned().collect(),
        reads: BTreeSet::new(), undeclared_reads: BTreeSet::new(), dynamic_read: false,
        output: Sources::new(), decisions: Sources::new(),
        output_objects: BTreeSet::new(), other_output: false, prints: 0,
    };
    let mut analyzer = Analyzer { functions: &program.checker.functions, schemas: &program.checker.schemas, label, budget: NODE_BUDGET };
    analyzer.statements(&program.body, &mut HashMap::new())?;
    Ok(analyzer.label)
}

/// One resource's contribution, grouped for display.
#[derive(Debug, Default, PartialEq)]
pub struct Use { pub all: bool, pub fields: Vec<String>, pub count: bool, pub validity: bool }

fn group(sources: &Sources) -> BTreeMap<String, Use> {
    let mut out: BTreeMap<String, Use> = BTreeMap::new();
    for (resource, part) in sources {
        let entry = out.entry(resource.clone()).or_default();
        match part {
            Part::Whole => entry.all = true,
            Part::Field(field) => entry.fields.push(field.clone()),
            Part::Count => entry.count = true,
            Part::Validity => entry.validity = true,
        }
    }
    out
}
impl Label {
    pub fn output_values(&self) -> BTreeMap<String, Use> { group(&self.output) }
    pub fn decision_values(&self) -> BTreeMap<String, Use> { group(&self.decisions) }
}

fn quoted(text: &str) -> String {
    Json::text(text).map(|j| j.serialize()).unwrap_or_else(|_| "\"<too long>\"".into())
}
fn string_array<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
    format!("[{}]", items.into_iter().map(|s| quoted(s)).collect::<Vec<_>>().join(","))
}
fn uses_json(uses: &BTreeMap<String, Use>) -> String {
    let items: Vec<String> = uses.iter().map(|(resource, usage)| format!(
        "{{\"all\":{},\"count\":{},\"fields\":{},\"resource\":{},\"validity\":{}}}",
        usage.all, usage.count, string_array(&usage.fields), quoted(resource), usage.validity)).collect();
    format!("[{}]", items.join(","))
}

pub fn to_json(label: &Label) -> String {
    let objects: Vec<String> = label.output_objects.iter().map(|keys| string_array(keys)).collect();
    format!(concat!(
        "{{\"clean\":{},\"decisions\":{},\"declared\":{},\"dynamic_read\":{},",
        "\"effects\":\"read-declared-text-and-print-only\",\"ok\":true,\"other_output\":{},",
        "\"output\":{},\"output_objects\":[{}],\"phase\":\"label\",\"prints\":{},\"reads\":{},",
        "\"schema_version\":1,\"undeclared_reads\":{},\"unused\":{},\"version\":{}}}"),
        label.clean(), uses_json(&label.decision_values()), string_array(&label.declared), label.dynamic_read,
        label.other_output, uses_json(&label.output_values()), objects.join(","), label.prints,
        string_array(&label.reads), string_array(&label.undeclared_reads), string_array(&label.unused()),
        quoted(env!("CARGO_PKG_VERSION")))
}

struct Words {
    title: &'static str, reads: &'static str, nothing: &'static str, any_declared: &'static str,
    effects: &'static str, effects_text: &'static str, output: &'static str, decisions: &'static str,
    all: &'static str, fields: &'static str, count: &'static str, validity: &'static str,
    objects: &'static str, other: &'static str, findings: &'static str, none: &'static str,
    undeclared: &'static str, dynamic: &'static str, unused: &'static str, separator: &'static str, parts: &'static str,
}
const ENGLISH: Words = Words {
    title: "Zarya label (static analysis; the program was not run)",
    reads: "Reads", nothing: "nothing", any_declared: "any declared resource (name computed at run time)",
    effects: "Effects", effects_text: "reads the resources above and prints; the language has no write, file, network or SQL operation",
    output: "Data that can appear in the output", decisions: "Data used in conditions",
    all: "any part", fields: "fields", count: "number of records", validity: "whether it is valid",
    objects: "Output objects", other: "other printed values",
    findings: "Findings", none: "none",
    undeclared: "reads a resource it never declared (denied at run time)",
    dynamic: "computes a resource name at run time, so the list of reads is not exact",
    unused: "declares a resource it never reads", separator: ", ", parts: "; ",
};
const ARABIC: Words = Words {
    title: "ملصق Zarya (تحليل ثابت؛ لم يُشغَّل البرنامج)",
    reads: "يقرأ", nothing: "لا شيء", any_declared: "أي مورد مصرَّح به (الاسم يُحسب وقت التشغيل)",
    effects: "الآثار", effects_text: "يقرأ الموارد أعلاه ويطبع فقط؛ لا توجد في اللغة عمليات كتابة أو ملفات أو شبكة أو SQL",
    output: "بيانات قد تظهر في المخرجات", decisions: "بيانات تُستخدم في الشروط",
    all: "أي جزء", fields: "الحقول", count: "عدد السجلات", validity: "هل هو صالح",
    objects: "كائنات المخرجات", other: "قيم مطبوعة أخرى",
    findings: "ملاحظات", none: "لا شيء",
    undeclared: "يقرأ موردًا لم يصرّح به (يُرفض وقت التشغيل)",
    dynamic: "يحسب اسم المورد وقت التشغيل، فقائمة القراءة غير دقيقة",
    unused: "يصرّح بمورد لا يقرؤه أبدًا", separator: "، ", parts: "؛ ",
};

fn uses_text(uses: &BTreeMap<String, Use>, words: &Words, out: &mut Vec<String>) {
    if uses.is_empty() { out.push(format!("  {}", words.nothing)); }
    for (resource, usage) in uses {
        let mut parts = Vec::new();
        if usage.all { parts.push(words.all.to_string()); }
        else if !usage.fields.is_empty() { parts.push(format!("{} {}", words.fields, usage.fields.join(words.separator))); }
        if usage.count && !usage.all { parts.push(words.count.to_string()); }
        if usage.validity && !usage.all { parts.push(words.validity.to_string()); }
        out.push(format!("  {}: {}", resource, parts.join(words.parts)));
    }
}

pub fn to_text(label: &Label, arabic: bool) -> Vec<String> {
    let words = if arabic { &ARABIC } else { &ENGLISH };
    let join = |items: &BTreeSet<String>| items.iter().cloned().collect::<Vec<_>>().join(words.separator);
    let mut out = vec![words.title.to_string()];
    let reads = if label.dynamic_read { words.any_declared.to_string() }
        else if label.reads.is_empty() { words.nothing.to_string() } else { join(&label.reads) };
    out.push(format!("{}: {}", words.reads, reads));
    out.push(format!("{}: {}", words.effects, words.effects_text));
    out.push(format!("{}:", words.output));
    uses_text(&label.output_values(), words, &mut out);
    out.push(format!("{}:", words.decisions));
    uses_text(&label.decision_values(), words, &mut out);
    let mut objects: Vec<String> = label.output_objects.iter().map(|keys| format!("{{{}}}", keys.join(words.separator))).collect();
    if label.other_output { objects.push(words.other.to_string()); }
    out.push(format!("{}: {}", words.objects, if objects.is_empty() { words.nothing.to_string() } else { objects.join(" | ") }));
    let mut findings = Vec::new();
    if !label.undeclared_reads.is_empty() { findings.push(format!("{}: {}", words.undeclared, join(&label.undeclared_reads))); }
    if label.dynamic_read { findings.push(words.dynamic.to_string()); }
    if !label.unused().is_empty() { findings.push(format!("{}: {}", words.unused, join(&label.unused()))); }
    if findings.is_empty() { out.push(format!("{}: {}", words.findings, words.none)); }
    else { out.push(format!("{}:", words.findings)); out.extend(findings.into_iter().map(|f| format!("  {}", f))); }
    out
}

fn emit(lines: &[String]) -> bool {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    lines.iter().all(|line| super::write_terminal_line(&mut out, line).is_ok()) && out.flush().is_ok()
}

/// `mode` is "--label", "--label-ar" or "--label-json". Exit 0 only for a clean label.
pub fn cli(filename: &str, mode: &str) -> ExitCode {
    let structured = mode == "--label-json";
    let source = match read_source(filename) {
        Ok(source) => source,
        Err(error) => {
            if structured {
                let error = super::Error::new("Cannot read bounded UTF-8 source", Default::default());
                emit(&[super::check::report(None, Err(&error)).serialize()]);
            } else { eprintln!("Zarya error: {}", error); }
            return ExitCode::from(1);
        }
    };
    let program = match compile(&source) {
        Ok(program) => program,
        Err(error) => {
            if structured { emit(&[super::check::report(Some(&source), Err(&error)).serialize()]); }
            else { eprintln!("{}", error.render(&source, filename)); }
            return ExitCode::from(1);
        }
    };
    let label = match analyze(&program) {
        Ok(label) => label,
        Err(message) => { eprintln!("Zarya error: {}", message); return ExitCode::from(1); }
    };
    let lines = if structured { vec![to_json(&label)] } else { to_text(&label, mode == "--label-ar") };
    if !emit(&lines) { eprintln!("Zarya error: cannot write label"); return ExitCode::from(1); }
    if label.clean() { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn label(source: &str) -> Label { analyze(&compile(source).unwrap()).unwrap() }
    fn fields(names: &[&str]) -> Use { Use { fields: names.iter().map(|s| s.to_string()).collect(), ..Use::default() } }
    fn set(names: &[&str]) -> BTreeSet<String> { names.iter().map(|s| s.to_string()).collect() }

    const SEARCH: &str = r#"
        permission text courses_public; permission text request;
        schema SearchRequest { max_price_minor: Int; search_text: Text; }
        schema PublicCourse { id: Int; title: Text; price_minor: Int; }
        let request_result: Json = json_validate_result(json_parse(read_text("request")), "SearchRequest");
        if json_as_bool(json_get(request_result, "ok")) {
            let query: Json = json_get(request_result, "value");
            let maximum: Int = json_as_int(json_get(query, "max_price_minor"));
            let term: Text = json_as_text(json_get(query, "search_text"));
            let records: Json = json_records_result(json_parse(read_text("courses_public")), "PublicCourse");
            if json_as_bool(json_get(records, "ok")) {
                let courses: List<Json> = json_items(json_get(records, "value"));
                let cheap: List<Json> = filter(courses, course, json_as_int(json_get(course, "price_minor")) <= maximum);
                let picked: List<Json> = filter(cheap, course, text_contains(json_as_text(json_get(course, "title")), term));
                let items: List<Json> = map(picked, course => json_object("id", json_get(course, "id"), "title", json_get(course, "title")));
                print(json_object("count", json_int(len(items)), "courses", json_list(items), "search_text", json_text(term)));
            } else { print(json_object("error", json_text("invalid_courses"))); }
        } else { print(json_object("error", json_text("invalid_request"))); }"#;

    #[test]
    fn search_program_label_is_field_precise() {
        let l = label(SEARCH);
        assert!(l.clean());
        assert_eq!(l.reads, set(&["courses_public", "request"]));
        assert!(l.unused().is_empty());
        let output = l.output_values();
        // price_minor is filtered on but never copied into the output.
        assert_eq!(output["courses_public"], fields(&["id", "title"]));
        assert_eq!(output["request"], fields(&["search_text"]));
        let decisions = l.decision_values();
        assert_eq!(decisions["courses_public"], Use { validity: true, ..fields(&["price_minor", "title"]) });
        assert_eq!(decisions["request"], Use { validity: true, ..fields(&["max_price_minor", "search_text"]) });
        assert_eq!(l.output_objects, [vec!["count".to_string(), "courses".into(), "search_text".into()], vec!["error".to_string()]].into());
        assert!(!l.other_output);
        assert_eq!(l.prints, 3);
    }

    #[test]
    fn undeclared_literal_read_is_reported_before_any_run() {
        let l = label("permission text request; print(read_text(\"secrets\"));");
        assert!(!l.clean());
        assert_eq!(l.undeclared_reads, set(&["secrets"]));
        assert!(l.reads.is_empty());
        assert!(l.output_values().is_empty());
        assert_eq!(l.unused(), set(&["request"]));
    }

    #[test]
    fn computed_resource_name_taints_every_declared_resource() {
        let l = label("permission text a; permission text b; print(read_text(\"a\" + \"\"));");
        assert!(l.dynamic_read && !l.clean());
        let output = l.output_values();
        assert!(output["a"].all && output["b"].all);
        assert!(l.unused().is_empty());
    }

    #[test]
    fn printing_a_resource_reports_any_part() {
        let l = label("permission text secret; print(read_text(\"secret\"));");
        assert!(l.clean());
        assert!(l.output_values()["secret"].all);
        assert!(l.other_output);
    }

    #[test]
    fn schema_projection_limits_whole_records_to_declared_fields() {
        let l = label(r#"permission text r; schema S { id: Int; title: Text; }
            print(json_list(json_records(json_parse(read_text("r")), "S")));"#);
        assert_eq!(l.output_values()["r"], fields(&["id", "title"]));
        assert_eq!(l.decision_values()["r"], Use { validity: true, ..Use::default() });
    }

    #[test]
    fn a_locally_built_object_key_never_narrows_a_resource() {
        let l = label(r#"permission text r;
            print(json_get(json_object("id", json_parse(read_text("r"))), "id"));"#);
        assert!(l.output_values()["r"].all);
    }

    #[test]
    fn an_index_taken_from_another_resource_is_not_narrowed() {
        let l = label(r#"permission text a; permission text b;
            let rows: List<Json> = json_items(json_parse(read_text("a")));
            print(json_get(at(rows, json_as_int(json_parse(read_text("b")))), "id"));"#);
        let output = l.output_values();
        assert_eq!(output["a"], fields(&["id"]));
        assert!(output["b"].all);
    }

    #[test]
    fn functions_are_followed() {
        let l = label("permission text courses; fn data(): Text = read_text(\"courses\"); print(data());");
        assert!(l.output_values()["courses"].all);
        let l = label(r#"permission text r; fn pick(row: Json): Json = json_get(row, "name");
            print(pick(json_parse(read_text("r"))));"#);
        assert_eq!(l.output_values()["r"], fields(&["name"]));
    }

    #[test]
    fn a_condition_on_secret_data_is_a_decision_not_an_output_value() {
        let l = label(r#"permission text secret;
            if text_contains(read_text("secret"), "x") { print("yes"); } else { print("no"); }"#);
        assert!(l.output_values().is_empty());
        assert!(l.decision_values()["secret"].all);
    }

    #[test]
    fn counting_records_reports_only_the_count() {
        let l = label("permission text r; print(len(json_items(json_parse(read_text(\"r\")))));");
        assert_eq!(l.output_values()["r"], Use { count: true, ..Use::default() });
    }

    #[test]
    fn printing_a_validation_result_reports_fields_and_validity() {
        let l = label(r#"permission text r; schema S { id: Int; }
            let result: Json = json_validate_result(json_parse(read_text("r")), "S");
            print(json_get(result, "ok")); print(result);"#);
        assert_eq!(l.output_values()["r"], Use { validity: true, ..fields(&["id"]) });
    }

    #[test]
    fn loops_and_asserts_are_covered() {
        let l = label(r#"permission text r;
            let rows: List<Json> = json_items(json_parse(read_text("r")));
            for row in rows { assert(json_as_int(json_get(row, "price")) >= 0); print(json_get(row, "name")); }"#);
        assert_eq!(l.output_values()["r"], fields(&["name"]));
        assert_eq!(l.decision_values()["r"], Use { validity: true, ..fields(&["price"]) });
    }

    /// The label must never under-report: values outside the labelled fields
    /// do not appear in real output.
    #[test]
    fn labelled_fields_match_what_a_real_run_can_print() {
        let program = compile(SEARCH).unwrap();
        let l = analyze(&program).unwrap();
        let mut resources = Map::new();
        resources.insert("request".to_string(), r#"{"max_price_minor":9000,"search_text":"MARK_TERM"}"#.to_string());
        resources.insert("courses_public".to_string(),
            r#"[{"id":7,"title":"MARK_TERM MARK_TITLE","price_minor":8123}]"#.to_string());
        let output = program.execute(&resources).unwrap().join("\n");
        assert!(output.contains("MARK_TITLE") && output.contains("MARK_TERM") && output.contains("\"id\":7"));
        assert!(!output.contains("8123") && !output.contains("9000"));
        let values = l.output_values();
        assert!(!values["courses_public"].fields.contains(&"price_minor".to_string()));
        assert!(!values["request"].fields.contains(&"max_price_minor".to_string()));
    }

    #[test]
    fn json_and_text_reports_are_stable() {
        let l = label("permission text r; permission text spare; print(json_object(\"n\", json_get(json_parse(read_text(\"r\")), \"name\")));");
        let json = to_json(&l);
        assert!(json.contains("\"reads\":[\"r\"]") && json.contains("\"unused\":[\"spare\"]"));
        assert!(json.contains("\"output\":[{\"all\":false,\"count\":false,\"fields\":[\"name\"],\"resource\":\"r\",\"validity\":false}]"));
        assert!(json.contains("\"output_objects\":[[\"n\"]]") && json.contains("\"clean\":true"));
        assert!(super::super::json_input::parse(&json).is_ok());
        let english = to_text(&l, false).join("\n");
        assert!(english.contains("Reads: r") && english.contains("r: fields name") && english.contains("never reads: spare"));
        let arabic = to_text(&l, true).join("\n");
        assert!(arabic.contains("يقرأ: r") && arabic.contains("الحقول name"));
    }
}
