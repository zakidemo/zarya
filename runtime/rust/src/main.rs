#![forbid(unsafe_code)]
//! Local reference runtime. This executable is not an OS sandbox.
mod json;
mod json_input;
mod schema;
mod check;
mod lists;
mod memory;
mod output_guard;
mod external_contract;
mod contract_check;
mod contract_explain;
mod resource_policy;
mod label;
use memory::{Budget, OwnedValue};
use json::Json;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;

const SOURCE_LIMIT: usize = 65_536;
const TEXT_LIMIT: usize = 4_096;
const STEP_LIMIT: usize = 10_000;
const DEPTH_LIMIT: usize = 64;
const RESOURCE_LIMIT: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
struct Span { start: usize, end: usize }

#[derive(Debug)]
struct Error { message: String, span: Span }
type Result<T> = std::result::Result<T, Error>;

impl Error {
    fn new(message: impl Into<String>, span: Span) -> Self {
        Self { message: message.into(), span }
    }
    fn render(&self, source: &str, filename: &str) -> String {
        let start = self.span.start.min(source.len());
        let before = &source[..start];
        let line = before.chars().filter(|c| *c == '\n').count() + 1;
        let beginning = before.rfind('\n').map_or(0, |i| i + 1);
        let raw_text = source[beginning..].split('\n').next().unwrap_or("");
        // A CR immediately before LF belongs to the line ending, not its text.
        let text = if source[beginning + raw_text.len()..].starts_with('\n') {
            raw_text.strip_suffix('\r').unwrap_or(raw_text)
        } else { raw_text };
        let column = source[beginning..start].chars().count() + 1;
        let end = self.span.end.min(beginning + text.len()).max(start);
        let expand = |s: &str, source_text: bool| {
            let mut out = String::new();
            let mut col = 0;
            for c in s.chars() {
                let bidi_control = matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' |
                    '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
                if c == '\t' && source_text {
                    let n = 4 - col % 4; out.push_str(&" ".repeat(n)); col += n;
                } else if c.is_control() || bidi_control {
                    let escaped = c.escape_unicode().to_string();
                    col += escaped.len();
                    out.push_str(&escaped);
                } else { out.push(c); col += 1; }
            }
            out
        };
        let prefix = expand(&source[beginning..start], true);
        let width = expand(&source[beginning..end], true).chars().count()
            .saturating_sub(prefix.chars().count()).max(1);
        format!("{}:{}:{}: Zarya error: {}\n{} | {}\n{} | {}{}",
            expand(filename, false), line, column, expand(&self.message, false), line, expand(text, true),
            " ".repeat(line.to_string().len()), " ".repeat(prefix.chars().count()), "^".repeat(width))
    }
}

#[derive(Clone, Debug)]
struct Token { text: String, span: Span }

fn lex(source: &str) -> Result<Vec<Token>> {
    if source.len() > SOURCE_LIMIT { return Err(Error::new("Source exceeds 64 KiB", Span::default())); }
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < source.len() {
        let start = pos;
        let c = source[pos..].chars().next().unwrap();
        if c.is_whitespace() { pos += c.len_utf8(); continue; }
        if source[pos..].starts_with("//") {
            pos += source[pos..].find('\n').unwrap_or(source.len() - pos);
            continue;
        }
        if c == '"' {
            pos += 1;
            let mut closed = false;
            while pos < source.len() {
                let ch = source[pos..].chars().next().unwrap();
                if ch == '"' { pos += 1; closed = true; break; }
                if ch < ' ' { return Err(Error::new("Invalid text literal", Span { start, end: pos + ch.len_utf8() })); }
                if ch == '\\' {
                    pos += 1;
                    let escaped = source[pos..].chars().next().ok_or_else(|| Error::new("Unterminated text literal", Span { start, end: pos }))?;
                    if !matches!(escaped, '"' | '\\' | 'n' | 'r' | 't') {
                        return Err(Error::new("Invalid text escape", Span { start: pos - 1, end: pos + escaped.len_utf8() }));
                    }
                    pos += escaped.len_utf8();
                } else { pos += ch.len_utf8(); }
            }
            if !closed { return Err(Error::new("Unterminated text literal", Span { start, end: pos })); }
        } else if c.is_ascii_digit() {
            while pos < source.len() && source.as_bytes()[pos].is_ascii_digit() { pos += 1; }
        } else if c.is_ascii_alphabetic() || c == '_' {
            while pos < source.len() && (source.as_bytes()[pos].is_ascii_alphanumeric() || source.as_bytes()[pos] == b'_') { pos += 1; }
        } else if ["==", "!=", "<=", ">=", "=>"].iter().any(|op| source[pos..].starts_with(op)) {
            pos += 2;
        } else if "{}[]():;,=+*<>-".contains(c) { pos += 1; }
        else { return Err(Error::new("Invalid token", Span { start, end: start + c.len_utf8() })); }
        tokens.push(Token { text: source[start..pos].into(), span: Span { start, end: pos } });
    }
    tokens.push(Token { text: "<EOF>".into(), span: Span { start: pos, end: pos } });
    Ok(tokens)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Type { Int, Text, Bool, Json, IntList, TextList, BoolList, JsonList, EmptyList }
impl Type {
    fn list(self) -> Option<Self> { match self { Self::Int => Some(Self::IntList), Self::Text => Some(Self::TextList), Self::Bool => Some(Self::BoolList), Self::Json => Some(Self::JsonList), _ => None } }
    fn element(self) -> Option<Self> { match self { Self::IntList => Some(Self::Int), Self::TextList => Some(Self::Text), Self::BoolList => Some(Self::Bool), Self::JsonList => Some(Self::Json), _ => None } }
    fn is_list(self) -> bool { self.element().is_some() || self == Self::EmptyList }

    fn name(self) -> &'static str { match self { Self::Int => "Int", Self::Text => "Text", Self::Bool => "Bool", Self::Json => "Json", Self::IntList => "List<Int>", Self::TextList => "List<Text>", Self::BoolList => "List<Bool>", Self::JsonList => "List<Json>", Self::EmptyList => "empty list" } }
}
#[derive(Clone, Debug, PartialEq)]
enum Value { Int(i64), Text(String), Bool(bool), Json(Json), List { items: Vec<Value>, kind: Type } }
impl Value {
    fn coerce(self, expected: Type) -> Self { match self { Self::List { items, kind: Type::EmptyList } if expected.is_list() => Self::List { items, kind: expected }, value => value } }

    fn typ(&self) -> Type { match self { Self::Int(_) => Type::Int, Self::Text(_) => Type::Text, Self::Bool(_) => Type::Bool, Self::Json(_) => Type::Json, Self::List {kind, ..} => *kind } }
    fn output(&self) -> String { match self { Self::Int(n) => n.to_string(), Self::Text(s) => s.clone(), Self::Bool(b) => b.to_string(), Self::Json(j) => j.serialize(), Self::List {..} => "<list: use json_list>".into() } }
}

#[derive(Clone, Debug)]
struct Expr { kind: ExprKind, span: Span, depth: usize }
#[derive(Clone, Debug)]
enum ExprKind {
    Literal(Value), Name(String), Neg(Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>), Call(String, Vec<Expr>), List(Vec<Expr>),
    Filter(Box<Expr>, String, Box<Expr>), Map(Box<Expr>, String, Box<Expr>),
}
impl Expr {
    fn new(kind: ExprKind, span: Span) -> Result<Self> {
        let depth = match &kind {
            ExprKind::Neg(x) => x.depth + 1,
            ExprKind::Binary(_, a, b) => a.depth.max(b.depth) + 1,
            ExprKind::Filter(items, _, condition) | ExprKind::Map(items, _, condition) => items.depth.max(condition.depth) + 1,
            ExprKind::Call(_, args) | ExprKind::List(args) => args.iter().map(|a| a.depth).max().unwrap_or(0) + 1,
            _ => 1,
        };
        if depth > DEPTH_LIMIT { return Err(Error::new("Expression nesting limit", span)); }
        Ok(Self { kind, span, depth })
    }
}
#[derive(Clone, Debug)]
struct Function { params: Vec<(String, Type)>, result: Type, body: Expr }
#[derive(Debug)]
struct Stmt { kind: StmtKind, span: Span }
#[derive(Debug)]
enum StmtKind {
    Let(String, Type, Expr), Print(Expr), Fn(String, Function), If(Expr, Vec<Stmt>, Vec<Stmt>),
    Permission(String), For(String, Expr, Vec<Stmt>),
    Schema(String, schema::Fields), Assert(Expr),
}

struct Parser { tokens: Vec<Token>, at: usize, depth: usize }
fn valid_identifier(text: &str) -> bool {
    let first = text.chars().next().unwrap_or(' ');
    (first.is_ascii_alphabetic() || first == '_') &&
        !json_input::builtin(text) && !schema::builtin(text) && text != "schema" &&
        !["let", "fn", "if", "else", "print", "assert", "true", "false", "permission", "for", "in", "filter", "len", "at", "append", "sum", "json_list", "text_contains", "read_text", "json_text", "json_int", "json_bool", "json_null", "json_array", "json_object"].contains(&text)
}
impl Parser {
    fn peek(&self) -> &Token { &self.tokens[self.at] }
    fn take(&mut self) -> Result<Token> {
        let t = self.peek().clone();
        if t.text == "<EOF>" { return Err(Error::new("Unexpected end of program", t.span)); }
        self.at += 1;
        Ok(t)
    }
    fn expect(&mut self, expected: &str) -> Result<Token> {
        if self.peek().text != expected {
            return Err(Error::new(format!("Expected {}, got {}", expected, self.peek().text), self.peek().span));
        }
        self.take()
    }
    fn name(&mut self) -> Result<Token> {
        let t = self.take()?;
        if !valid_identifier(&t.text) {
            return Err(Error::new("Expected identifier", t.span));
        }
        Ok(t)
    }
    fn typ(&mut self) -> Result<Type> {
        let t = self.take()?;
        if t.text == "List" {
            self.expect("<")?;
            if self.peek().text == "List" { return Err(Error::new("Nested lists are not supported", self.peek().span)); }
            let element = self.typ()?;
            if self.peek().text == ">=" {
                let span = self.peek().span;
                self.tokens[self.at] = Token {text: "=".into(), span: Span {start: span.start + 1, end: span.end}};
            } else { self.expect(">")?; }
            return element.list().ok_or_else(|| Error::new("Nested lists are not supported", t.span));
        }
        match t.text.as_str() {
            "Int" => Ok(Type::Int), "Text" => Ok(Type::Text), "Bool" => Ok(Type::Bool), "Json" => Ok(Type::Json),
            _ => Err(Error::new(format!("Unknown type: {}", t.text), t.span)),
        }
    }
    fn program(&mut self, end: &str) -> Result<Vec<Stmt>> {
        let mut stmts = Vec::new();
        while self.peek().text != end { stmts.push(self.statement()?); }
        Ok(stmts)
    }
    fn block(&mut self) -> Result<Vec<Stmt>> {
        self.expect("{")?;
        self.depth += 1;
        if self.depth > 32 { return Err(Error::new("Block nesting limit", self.peek().span)); }
        let body = self.program("}")?;
        self.expect("}")?;
        self.depth -= 1;
        Ok(body)
    }
    fn statement(&mut self) -> Result<Stmt> {
        let first = self.take()?;
        let kind = match first.text.as_str() {
            "schema" => {
                let name = self.name()?;
                if name.text.len() > schema::MAX_NAME_BYTES { return Err(Error::new("Schema name exceeds 64 bytes", name.span)); }
                self.expect("{")?;
                let mut fields: schema::Fields = Vec::new();
                while self.peek().text != "}" {
                    if fields.len() >= schema::MAX_FIELDS { return Err(Error::new("Schema field limit exceeded (16)", self.peek().span)); }
                    let field_span = self.peek().span;
                    let key = if self.peek().text.starts_with('"') {
                        match self.expr(0)?.kind {
                            ExprKind::Literal(Value::Text(text)) => text,
                            _ => return Err(Error::new("Schema field must be an identifier or literal Text", field_span)),
                        }
                    } else { self.name()?.text };
                    if key.len() > schema::MAX_NAME_BYTES { return Err(Error::new("Schema field name exceeds 64 UTF-8 bytes", field_span)); }
                    if fields.iter().any(|(name, _)| name == &key) { return Err(Error::new("Duplicate schema field", field_span)); }
                    self.expect(":")?;
                    fields.push((key, self.typ()?));
                    self.expect(";")?;
                }
                self.expect("}")?;
                StmtKind::Schema(name.text, fields)
            }
            "permission" => {
                self.expect("text")?;
                let resource = self.name()?.text;
                self.expect(";")?;
                StmtKind::Permission(resource)
            }
            "let" => {
                let name = self.name()?.text;
                self.expect(":")?;
                let typ = self.typ()?;
                self.expect("=")?;
                let expr = self.expr(0)?;
                self.expect(";")?;
                StmtKind::Let(name, typ, expr)
            }
            "print" => {
                self.expect("(")?;
                let expr = self.expr(0)?;
                self.expect(")")?;
                self.expect(";")?;
                StmtKind::Print(expr)
            }
            "assert" => {
                self.expect("(")?;
                let condition = self.expr(0)?;
                self.expect(")")?;
                self.expect(";")?;
                StmtKind::Assert(condition)
            }
            "fn" => {
                let name = self.name()?.text;
                self.expect("(")?;
                let mut params = Vec::new();
                if self.peek().text != ")" {
                    loop {
                        let arg = self.name()?.text;
                        self.expect(":")?;
                        params.push((arg, self.typ()?));
                        if self.peek().text != "," { break; }
                        self.take()?;
                    }
                }
                self.expect(")")?;
                self.expect(":")?;
                let result = self.typ()?;
                self.expect("=")?;
                let body = self.expr(0)?;
                self.expect(";")?;
                StmtKind::Fn(name, Function { params, result, body })
            }
            "for" => {
                let name = self.name()?.text;
                self.expect("in")?;
                let items = self.expr(0)?;
                StmtKind::For(name, items, self.block()?)
            }
            "if" => {
                let cond = self.expr(0)?;
                let yes = self.block()?;
                let no = if self.peek().text == "else" { self.take()?; self.block()? } else { Vec::new() };
                StmtKind::If(cond, yes, no)
            }
            _ => return Err(Error::new(format!("Unknown statement: {}", first.text), first.span)),
        };
        Ok(Stmt { kind, span: Span { start: first.span.start, end: self.tokens[self.at - 1].span.end } })
    }
    fn expr(&mut self, minimum: u8) -> Result<Expr> {
        self.depth += 1;
        if self.depth > DEPTH_LIMIT { return Err(Error::new("Expression nesting limit", self.peek().span)); }
        let token = self.take()?;
        let mut left = if token.text == "[" {
            let mut items = Vec::new();
            if self.peek().text != "]" {
                loop {
                    if items.len() >= lists::MAX_ITEMS { return Err(Error::new("List item limit exceeded", token.span)); }
                    items.push(self.expr(0)?);
                    if self.peek().text != "," { break; }
                    self.take()?;
                }
            }
            let end = self.expect("]")?.span.end;
            Expr::new(ExprKind::List(items), Span { start: token.span.start, end })?
        } else if token.text == "(" {
            let mut inner = self.expr(0)?;
            let close = self.expect(")")?;
            inner.span = Span { start: token.span.start, end: close.span.end };
            inner
        } else if token.text == "-" {
            // The minimum i64 has a magnitude one larger than i64::MAX.
            // Fold only that exact negative literal; all other negations keep
            // their existing checked runtime behavior.
            if self.peek().text == "9223372036854775808" {
                let magnitude = self.take()?;
                Expr::new(ExprKind::Literal(Value::Int(i64::MIN)),
                          Span { start: token.span.start, end: magnitude.span.end })?
            } else {
                let x = self.expr(40)?;
                let span = Span { start: token.span.start, end: x.span.end };
                Expr::new(ExprKind::Neg(Box::new(x)), span)?
            }
        } else if token.text.starts_with('"') {
            let mut text = String::new();
            let mut chars = token.text[1..token.text.len() - 1].chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    let escaped = chars.next().unwrap();
                    text.push(match escaped { 'n' => '\n', 'r' => '\r', 't' => '\t', x => x });
                } else { text.push(c); }
            }
            if text.chars().count() > TEXT_LIMIT { return Err(Error::new("Text limit exceeded", token.span)); }
            Expr::new(ExprKind::Literal(Value::Text(text)), token.span)?
        } else if token.text.as_bytes()[0].is_ascii_digit() {
            if token.text.len() > 19 { return Err(Error::new("Integer literal too large", token.span)); }
            let value = token.text.parse::<i64>().map_err(|_| Error::new("Int overflow", token.span))?;
            Expr::new(ExprKind::Literal(Value::Int(value)), token.span)?
        } else if token.text == "true" || token.text == "false" {
            Expr::new(ExprKind::Literal(Value::Bool(token.text == "true")), token.span)?
        } else if token.text == "filter" {
            // filter(collection, local_name, boolean_expression)
            self.expect("(")?;
            let items = self.expr(0)?;
            self.expect(",")?;
            let variable = self.name()?.text;
            self.expect(",")?;
            let predicate = self.expr(0)?;
            let close = self.expect(")")?;
            Expr::new(ExprKind::Filter(Box::new(items), variable, Box::new(predicate)),
                      Span { start: token.span.start, end: close.span.end })?
        } else if token.text.chars().next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_') {
            if self.peek().text == "(" {
                self.take()?;
                let mut args = Vec::new();
                let mut mapped = None;
                if self.peek().text != ")" {
                    loop {
                        let arg = self.expr(0)?;
                        if token.text == "map" && args.len() == 1 && self.peek().text == "=>" {
                            let name = match &arg.kind {
                                ExprKind::Name(name) if valid_identifier(name) => name.clone(),
                                _ => return Err(Error::new("map requires an identifier before =>", arg.span)),
                            };
                            self.take()?;
                            mapped = Some((name, self.expr(0)?));
                            break;
                        }
                        args.push(arg);
                        if self.peek().text != "," { break; }
                        self.take()?;
                    }
                }
                let close = self.expect(")")?;
                let span = Span { start: token.span.start, end: close.span.end };
                if let Some((name, transform)) = mapped {
                    let items = args.pop().ok_or_else(|| Error::new("map requires a list", span))?;
                    Expr::new(ExprKind::Map(Box::new(items), name, Box::new(transform)), span)?
                } else {
                    Expr::new(ExprKind::Call(token.text, args), span)?
                }
            } else { Expr::new(ExprKind::Name(token.text), token.span)? }
        } else { return Err(Error::new("Invalid expression", token.span)); };
        loop {
            let precedence = match self.peek().text.as_str() {
                "==" | "!=" | "<" | ">" | "<=" | ">=" => 10,
                "+" | "-" => 20, "*" => 30, _ => break,
            };
            if precedence < minimum { break; }
            let op = self.take()?.text;
            let right = self.expr(precedence + 1)?;
            let span = Span { start: left.span.start, end: right.span.end };
            left = Expr::new(ExprKind::Binary(op, Box::new(left), Box::new(right)), span)?;
        }
        self.depth -= 1;
        Ok(left)
    }
}

fn require(expected: Type, actual: Type, span: Span) -> Result<()> {
    if expected.is_list() && actual.is_list() && (expected == Type::EmptyList || actual == Type::EmptyList) { return Ok(()); }
    if expected != actual { return Err(Error::new(format!("Expected {}, got {}", expected.name(), actual.name()), span)); }
    Ok(())
}

fn tick(budget: &mut usize, span: Span) -> Result<()> {
    if *budget == 0 { return Err(Error::new("Evaluation step limit", span)); }
    *budget -= 1;
    Ok(())
}

fn json_signature(name: &str, count: usize, span: Span) -> Option<Result<Vec<Type>>> {
    let types = match name {
        "json_text" if count == 1 => vec![Type::Text],
        "json_int" if count == 1 => vec![Type::Int],
        "json_bool" if count == 1 => vec![Type::Bool],
        "json_null" if count == 0 => vec![],
        "json_array" if count <= 32 => vec![Type::Json; count],
        "json_object" if count <= 32 && count % 2 == 0 => (0..count).map(|i| if i % 2 == 0 { Type::Text } else { Type::Json }).collect(),
        "json_text" | "json_int" | "json_bool" | "json_null" | "json_array" | "json_object" =>
            return Some(Err(Error::new("Invalid JSON builtin argument count (maximum 32)", span))),
        _ => return None,
    };
    Some(Ok(types))
}

fn json_call(name: &str, args: Vec<Value>, span: Span) -> Result<Json> {
    let invalid = || Error::new("Invalid JSON builtin arguments", span);
    let result = match name {
        "json_text" => match args.as_slice() { [Value::Text(s)] => Json::text(s), _ => return Err(invalid()) },
        "json_int" => match args.as_slice() { [Value::Int(n)] => Ok(Json::int(*n)), _ => return Err(invalid()) },
        "json_bool" => match args.as_slice() { [Value::Bool(b)] => Ok(Json::boolean(*b)), _ => return Err(invalid()) },
        "json_null" if args.is_empty() => Ok(Json::null()),
        "json_array" => {
            let mut items = Vec::new();
            for value in args {
                match value { Value::Json(j) => items.push(j), _ => return Err(invalid()) }
            }
            Json::array(items)
        }
        "json_object" => {
            let mut args = args.into_iter();
            let mut items = Vec::new();
            while let Some(key) = args.next() {
                match (key, args.next()) {
                    (Value::Text(key), Some(Value::Json(value))) => items.push((key, value)),
                    _ => return Err(invalid()),
                }
            }
            Json::object(items)
        }
        _ => return Err(invalid()),
    };
    result.map_err(|message| Error::new(message, span))
}

struct Checker { functions: HashMap<String, Function>, schemas: schema::Registry, requested: HashMap<String, Span>, budget: usize }
impl Checker {
    fn expr(&mut self, expr: &Expr, env: &HashMap<String, Type>) -> Result<Type> {
        tick(&mut self.budget, expr.span)?;
        match &expr.kind {
            ExprKind::Literal(v) => Ok(v.typ()),
            ExprKind::List(items) => {
                let mut element = None;
                for item in items {
                    let typ = self.expr(item, env)?;
                    if typ.list().is_none() { return Err(Error::new("Nested lists are not supported", item.span)); }
                    if let Some(expected) = element { require(expected, typ, item.span)?; } else { element = Some(typ); }
                }
                Ok(element.and_then(Type::list).unwrap_or(Type::EmptyList))
            }
            ExprKind::Name(n) => env.get(n).copied().ok_or_else(|| Error::new(format!("Unknown variable: {}", n), expr.span)),
            ExprKind::Filter(items, variable, predicate) => {
                let list_type = self.expr(items, env)?;
                let element_type = list_type.element().ok_or_else(||
                    Error::new("filter requires a typed list; annotate empty lists first", items.span))?;
                if env.contains_key(variable) || self.functions.contains_key(variable) {
                    return Err(Error::new("Filter variable shadows an existing name", expr.span));
                }
                let mut local = env.clone();
                local.insert(variable.clone(), element_type);
                require(Type::Bool, self.expr(predicate, &local)?, predicate.span)?;
                Ok(list_type)
            }
            ExprKind::Map(items, variable, transform) => {
                let list_type = self.expr(items, env)?;
                let element_type = list_type.element().ok_or_else(||
                    Error::new("map requires a typed list; annotate empty lists first", items.span))?;
                if env.contains_key(variable) || self.functions.contains_key(variable) {
                    return Err(Error::new("Map variable shadows an existing name", expr.span));
                }
                let mut local = env.clone();
                local.insert(variable.clone(), element_type);
                self.expr(transform, &local)?.list().ok_or_else(||
                    Error::new("Nested lists are not supported", transform.span))
            }
            ExprKind::Neg(x) => { require(Type::Int, self.expr(x, env)?, x.span)?; Ok(Type::Int) }
            ExprKind::Call(name, args) => {
                if schema::builtin(name) {
                    if args.len() != 2 { return Err(Error::new("Schema builtin expects Json and a literal schema name", expr.span)); }
                    require(Type::Json, self.expr(&args[0], env)?, args[0].span)?;
                    schema::literal_name(&args[1], &self.schemas)?;
                    return Ok(if name == "json_records" { Type::JsonList } else { Type::Json });
                }
                if json_input::builtin(name) {
                    let (expected, result) = json_input::signature(name, expr.span)?;
                    if args.len() != expected.len() { return Err(Error::new("Incorrect JSON input builtin argument count", expr.span)); }
                    for (arg, typ) in args.iter().zip(expected) { require(typ, self.expr(arg, env)?, arg.span)?; }
                    return Ok(result);
                }
                if lists::builtin(name) {
                    let mut types = Vec::new();
                    for arg in args { types.push(self.expr(arg, env)?); }
                    return lists::signature(name, &types, expr.span);
                }
                if let Some(types) = json_signature(name, args.len(), expr.span) {
                    for (arg, expected) in args.iter().zip(types?) {
                        require(expected, self.expr(arg, env)?, arg.span)?;
                    }
                    return Ok(Type::Json);
                }
                if name == "text_contains" {
                    if args.len() != 2 { return Err(Error::new("text_contains expects two Text arguments", expr.span)); }
                    for arg in args { require(Type::Text, self.expr(arg, env)?, arg.span)?; }
                    return Ok(Type::Bool);
                }
                if name == "read_text" {
                    if args.len() != 1 { return Err(Error::new("read_text expects one Text resource name", expr.span)); }
                    require(Type::Text, self.expr(&args[0], env)?, args[0].span)?;
                    return Ok(Type::Text);
                }
                let f = self.functions.get(name).cloned().ok_or_else(|| Error::new(format!("Unknown function: {}", name), expr.span))?;
                if args.len() != f.params.len() { return Err(Error::new("Incorrect argument count", expr.span)); }
                for ((_, typ), arg) in f.params.iter().zip(args) { require(*typ, self.expr(arg, env)?, arg.span)?; }
                Ok(f.result)
            }
            ExprKind::Binary(op, a, b) => {
                let ta = self.expr(a, env)?;
                let tb = self.expr(b, env)?;
                require(ta, tb, b.span)?;
                match op.as_str() {
                    "+" if ta == Type::Text => Ok(Type::Text),
                    "+" | "-" | "*" => { require(Type::Int, ta, a.span)?; Ok(Type::Int) }
                    "==" | "!=" => Ok(Type::Bool),
                    _ => { require(Type::Int, ta, a.span)?; Ok(Type::Bool) }
                }
            }
        }
    }
    fn statements(&mut self, body: &[Stmt], env: &mut HashMap<String, Type>, top: bool) -> Result<()> {
        for stmt in body {
            tick(&mut self.budget, stmt.span)?;
            match &stmt.kind {
                StmtKind::Schema(name, fields) => {
                    if !top { return Err(Error::new("Schemas must be top-level", stmt.span)); }
                    if self.schemas.contains_key(name) { return Err(Error::new("Duplicate schema declaration", stmt.span)); }
                    if self.schemas.len() >= schema::MAX_SCHEMAS { return Err(Error::new("Schema count limit exceeded (32)", stmt.span)); }
                    self.schemas.insert(name.clone(), fields.clone());
                }
                StmtKind::Permission(name) => {
                    if !top { return Err(Error::new("Permissions must be top-level", stmt.span)); }
                    if name.len() > 64 { return Err(Error::new("Resource name exceeds 64 characters", stmt.span)); }
                    if self.requested.len() >= RESOURCE_LIMIT { return Err(Error::new("Resource count limit exceeded", stmt.span)); }
                    if self.requested.insert(name.clone(), stmt.span).is_some() {
                        return Err(Error::new("Duplicate permission request", stmt.span));
                    }
                }
                StmtKind::Fn(name, f) => {
                    if !top { return Err(Error::new("Functions must be top-level", stmt.span)); }
                    if env.contains_key(name) || self.functions.contains_key(name) { return Err(Error::new(format!("Duplicate name: {}", name), stmt.span)); }
                    let mut local = HashMap::new();
                    for (name, typ) in &f.params {
                        if local.insert(name.clone(), *typ).is_some() { return Err(Error::new("Duplicate parameter", stmt.span)); }
                    }
                    require(f.result, self.expr(&f.body, &local)?, f.body.span)?;
                    self.functions.insert(name.clone(), f.clone());
                }
                StmtKind::Let(name, typ, expr) => {
                    if env.contains_key(name) || self.functions.contains_key(name) { return Err(Error::new(format!("Duplicate name: {}", name), stmt.span)); }
                    require(*typ, self.expr(expr, env)?, expr.span)?;
                    env.insert(name.clone(), *typ);
                }
                StmtKind::Print(expr) => {
                    if self.expr(expr, env)?.is_list() { return Err(Error::new("Use json_list to print a list", expr.span)); }
                }
                StmtKind::Assert(cond) => {
                    require(Type::Bool, self.expr(cond, env)?, cond.span)?;
                }
                StmtKind::For(name, items, body) => {
                    let kind = self.expr(items, env)?;
                    let element = kind.element().ok_or_else(|| Error::new("for requires a typed list; annotate empty lists first", items.span))?;
                    if env.contains_key(name) || self.functions.contains_key(name) { return Err(Error::new("Loop variable shadows an existing name", stmt.span)); }
                    let mut local = env.clone();
                    local.insert(name.clone(), element);
                    self.statements(body, &mut local, false)?;
                }
                StmtKind::If(cond, yes, no) => {
                    require(Type::Bool, self.expr(cond, env)?, cond.span)?;
                    self.statements(yes, &mut env.clone(), false)?;
                    self.statements(no, &mut env.clone(), false)?;
                }
            }
        }
        Ok(())
    }
}

struct Evaluator<'a> {
    functions: &'a HashMap<String, Function>, resources: &'a HashMap<String, OwnedValue>,
    schemas: &'a schema::Registry,
    budget: usize, memory: Budget, output: Vec<memory::Output>,
}
impl<'a> Evaluator<'a> {
    fn scoped_statements(&mut self, body: &[Stmt], env: &mut HashMap<String, OwnedValue>, binding: Option<(&str, OwnedValue)>) -> Result<()> {
        // Bindings are immutable and shadowing is rejected by the checker. Keep
        // existing values in place; track names so only child bindings are removed.
        let span = body.first().map_or(Span::default(), |stmt| stmt.span);
        let names = env.keys().fold(0usize, |n, key| n.saturating_add(memory::SLOT).saturating_add(key.len()));
        let _scope = self.memory.reserve(names, span)?;
        let outer: HashSet<String> = env.keys().cloned().collect();
        if let Some((name, value)) = binding {
            if env.contains_key(name) { return Err(Error::new("Loop variable shadows an existing name", span)); }
            let value = value.named(&self.memory, name, span)?;
            env.insert(name.into(), value);
        }
        let result = self.statements(body, env);
        env.retain(|name, _| outer.contains(name));
        result
    }
    fn expr(&mut self, expr: &Expr, env: &mut HashMap<String, OwnedValue>, depth: usize) -> Result<OwnedValue> {
        tick(&mut self.budget, expr.span)?;
        if depth > DEPTH_LIMIT { return Err(Error::new("Evaluation nesting limit", expr.span)); }
        let fail = || Error::new("Int overflow", expr.span);
        match &expr.kind {
            ExprKind::Literal(v) => self.memory.copy(v, expr.span),
            ExprKind::List(items) => {
                let mut values = Vec::new();
                for item in items { values.push(self.expr(item, env, depth + 1)?); }
                let kind = values.first().and_then(|v| v.value.typ().list()).unwrap_or(Type::EmptyList);
                let bytes = values.iter().fold(memory::SLOT, |n, v| n.saturating_add(v.value.memory_bytes()));
                self.memory.construct(values, bytes, 0, expr.span, |values| lists::make(values, kind, expr.span))
            }
            ExprKind::Name(n) => {
                let value = env.get(n).ok_or_else(|| Error::new(format!("Unknown variable: {}", n), expr.span))?;
                self.memory.copy(&value.value, expr.span)
            }
            ExprKind::Filter(items, variable, predicate) => {
                let collection = self.expr(items, env, depth + 1)?;
                let (items, kind) = match &collection.value {
                    Value::List { items, kind } => (items, *kind),
                    _ => return Err(Error::new("filter requires a list", items.span)),
                };
                // The temporary variable is removed even if predicate evaluation fails.
                if env.contains_key(variable) {
                    return Err(Error::new("Filter variable shadows an existing name", expr.span));
                }
                let mut selected = Vec::new();
                for item in items {
                    tick(&mut self.budget, expr.span)?;
                    let binding = self.memory.copy(item, expr.span)?
                        .named(&self.memory, variable, expr.span)?;
                    env.insert(variable.clone(), binding);
                    let evaluated = self.expr(predicate, env, depth + 1);
                    env.remove(variable);
                    let matched = evaluated?;
                    match matched.value {
                        Value::Bool(true) => selected.push(self.memory.copy(item, expr.span)?),
                        Value::Bool(false) => (),
                        _ => return Err(Error::new("Filter predicate must be Bool", predicate.span)),
                    }
                }
                let bytes = selected.iter().fold(memory::SLOT, |n, value| n.saturating_add(value.value.memory_bytes()));
                self.memory.construct(selected, bytes, 0, expr.span,
                    |values| lists::make(values, kind, expr.span))
            }
            ExprKind::Map(items, variable, transform) => {
                let collection = self.expr(items, env, depth + 1)?;
                let items = match &collection.value {
                    Value::List { items, .. } => items,
                    _ => return Err(Error::new("map requires a list", items.span)),
                };
                if env.contains_key(variable) {
                    return Err(Error::new("Map variable shadows an existing name", expr.span));
                }
                let mut mapped = Vec::new();
                for item in items {
                    tick(&mut self.budget, expr.span)?;
                    let binding = self.memory.copy(item, expr.span)?
                        .named(&self.memory, variable, expr.span)?;
                    env.insert(variable.clone(), binding);
                    let evaluated = self.expr(transform, env, depth + 1);
                    env.remove(variable);
                    mapped.push(evaluated?);
                }
                let kind = mapped.first().and_then(|value| value.value.typ().list()).unwrap_or(Type::EmptyList);
                let bytes = mapped.iter().fold(memory::SLOT, |n, value| n.saturating_add(value.value.memory_bytes()));
                self.memory.construct(mapped, bytes, 0, expr.span,
                    |values| lists::make(values, kind, expr.span))
            }
            ExprKind::Neg(x) => {
                let value = self.expr(x, env, depth + 1)?;
                self.memory.make(memory::SLOT, expr.span, || match value.value {
                    Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(fail),
                    _ => Err(Error::new("Unary minus requires Int", expr.span)),
                })
            }
            ExprKind::Call(name, args) => {
                if schema::builtin(name) {
                    let mut values = Vec::new();
                    for arg in args { values.push(self.expr(arg, env, depth + 1)?); }
                    let bytes = schema::result_bytes(name, &values);
                    return self.memory.construct(values, bytes, memory::JSON_SCRATCH, expr.span,
                        |values| schema::call(name, values, self.schemas, expr.span));
                }
                if json_input::builtin(name) {
                    let mut values = Vec::new();
                    for arg in args { values.push(self.expr(arg, env, depth + 1)?); }
                    let bytes = json_input::result_bytes(name, &values);
                    let scratch = if name == "json_parse" || name == "json_as_text" { memory::JSON_SCRATCH } else { 0 };
                    return self.memory.construct(values, bytes, scratch, expr.span, |values| json_input::call(name, values, expr.span));
                }
                if lists::builtin(name) {
                    let mut values = Vec::new();
                    for arg in args { values.push(self.expr(arg, env, depth + 1)?); }
                    let bytes = memory::list_result_bytes(name, &values);
                    let scratch = if name == "json_list" { memory::JSON_SCRATCH } else { 0 };
                    return self.memory.construct(values, bytes, scratch, expr.span, |values| lists::call(name, values, expr.span));
                }
                if json_signature(name, args.len(), expr.span).is_some() {
                    let mut values = Vec::new();
                    for arg in args { values.push(self.expr(arg, env, depth + 1)?); }
                    let bytes = memory::json_result_bytes(name, &values);
                    return self.memory.construct(values, bytes, memory::JSON_SCRATCH, expr.span, |values| json_call(name, values, expr.span).map(Value::Json));
                }
                if name == "text_contains" {
                    if args.len() != 2 { return Err(Error::new("text_contains expects two Text arguments", expr.span)); }
                    let haystack = self.expr(&args[0], env, depth + 1)?;
                    let needle = self.expr(&args[1], env, depth + 1)?;
                    return self.memory.make(memory::SLOT, expr.span, || match (&haystack.value, &needle.value) {
                        (Value::Text(haystack), Value::Text(needle)) => Ok(Value::Bool(haystack.contains(needle.as_str()))),
                        _ => Err(Error::new("text_contains requires Text arguments", expr.span)),
                    });
                }
                if name == "read_text" {
                    let key = self.expr(&args[0], env, depth + 1)?;
                    let key = match &key.value {
                        Value::Text(key) => key,
                        _ => return Err(Error::new("Resource name must be Text", expr.span)),
                    };
                    let value = self.resources.get(key).ok_or_else(|| Error::new("Permission denied: text resource is not granted to this program", expr.span))?;
                    return self.memory.copy(&value.value, expr.span);
                }
                let functions = self.functions;
                let f = functions.get(name).ok_or_else(|| Error::new("Unknown function", expr.span))?;
                let mut local = HashMap::new();
                for ((name, typ), arg) in f.params.iter().zip(args) {
                    let value = self.expr(arg, env, depth + 1)?.coerce(*typ).named(&self.memory, name, arg.span)?;
                    local.insert(name.clone(), value);
                }
                self.expr(&f.body, &mut local, depth + 1).map(|value| value.coerce(f.result))
            }
            ExprKind::Binary(op, a, b) => {
                let va = self.expr(a, env, depth + 1)?;
                let vb = self.expr(b, env, depth + 1)?;
                let bytes = match (op.as_str(), &va.value, &vb.value) {
                    ("+", Value::Text(a), Value::Text(b)) => memory::SLOT + a.len() + b.len(),
                    _ => memory::SLOT,
                };
                self.memory.make(bytes, expr.span, || match (op.as_str(), &va.value, &vb.value) {
                    ("+", Value::Int(a), Value::Int(b)) => a.checked_add(*b).map(Value::Int).ok_or_else(fail),
                    ("-", Value::Int(a), Value::Int(b)) => a.checked_sub(*b).map(Value::Int).ok_or_else(fail),
                    ("*", Value::Int(a), Value::Int(b)) => a.checked_mul(*b).map(Value::Int).ok_or_else(fail),
                    ("+", Value::Text(a), Value::Text(b)) => {
                        if a.chars().count() + b.chars().count() > TEXT_LIMIT { return Err(Error::new("Text limit exceeded", expr.span)); }
                        let mut text = String::with_capacity(a.len() + b.len());
                        text.push_str(a); text.push_str(b);
                        Ok(Value::Text(text))
                    }
                    ("==", Value::List {items: a, ..}, Value::List {items: b, ..}) => Ok(Value::Bool(a == b)),
                    ("!=", Value::List {items: a, ..}, Value::List {items: b, ..}) => Ok(Value::Bool(a != b)),
                    ("==", a, b) => Ok(Value::Bool(a == b)),
                    ("!=", a, b) => Ok(Value::Bool(a != b)),
                    ("<", Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a < b)),
                    (">", Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a > b)),
                    ("<=", Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a <= b)),
                    (">=", Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a >= b)),
                    _ => Err(Error::new("Invalid operand types", expr.span)),
                })
            }
        }
    }
    fn statements(&mut self, body: &[Stmt], env: &mut HashMap<String, OwnedValue>) -> Result<()> {
        for stmt in body {
            tick(&mut self.budget, stmt.span)?;
            match &stmt.kind {
                StmtKind::Fn(_, _) | StmtKind::Schema(_, _) | StmtKind::Permission(_) => (),
                StmtKind::Let(name, typ, expr) => {
                    let value = self.expr(expr, env, 0)?.coerce(*typ).named(&self.memory, name, stmt.span)?;
                    env.insert(name.clone(), value);
                }
                StmtKind::For(name, items, body) => {
                    let collection = self.expr(items, env, 0)?;
                    let values = match &collection.value {
                        Value::List {items, ..} => items,
                        _ => return Err(Error::new("for requires a list", stmt.span)),
                    };
                    for value in values {
                        tick(&mut self.budget, stmt.span)?;
                        let value = self.memory.copy(value, stmt.span)?;
                        self.scoped_statements(body, env, Some((name, value)))?;
                    }
                }
                StmtKind::Print(expr) => {
                    let value = self.expr(expr, env, 0)?;
                    if self.output.len() >= 256 { return Err(Error::new("Output line limit", stmt.span)); }
                    self.output.push(self.memory.output(&value.value, stmt.span)?);
                }
                StmtKind::Assert(cond) => {
                    let value = self.expr(cond, env, 0)?;
                    match value.value {
                        Value::Bool(true) => (),
                        Value::Bool(false) => return Err(Error::new("Assertion failed", stmt.span)),
                        _ => return Err(Error::new("Expected Bool", cond.span)),
                    }
                }
                StmtKind::If(cond, yes, no) => {
                    let val = self.expr(cond, env, 0)?;
                    match val.value {
                        Value::Bool(b) => self.scoped_statements(if b { yes } else { no }, env, None)?,
                        _ => return Err(Error::new("Expected Bool", cond.span)),
                    }
                }
            }
        }
        Ok(())
    }
}

struct Program { body: Vec<Stmt>, checker: Checker }

fn compile(source: &str) -> Result<Program> {
    let mut parser = Parser { tokens: lex(source)?, at: 0, depth: 0 };
    let body = parser.program("<EOF>")?;
    let mut checker = Checker { functions: HashMap::new(), schemas: HashMap::new(), requested: HashMap::new(), budget: STEP_LIMIT };
    checker.statements(&body, &mut HashMap::new(), true)?;
    Ok(Program { body, checker })
}

impl Program {
    fn execute(&self, host_resources: &HashMap<String, String>) -> Result<Vec<String>> {
        self.execute_with_memory(host_resources, Budget::new(memory::LIMIT))
    }
    fn execute_with_memory(&self, host_resources: &HashMap<String, String>, memory: Budget) -> Result<Vec<String>> {
        let mut resources = HashMap::new();
        for (name, span) in &self.checker.requested {
            let text = host_resources.get(name).ok_or_else(|| Error::new(
                format!("Permission denied: host did not grant text resource {}", name), *span))?;
            if text.chars().count() > TEXT_LIMIT { return Err(Error::new("Text resource limit exceeded", *span)); }
            let value = memory.make(memory::SLOT + text.len(), *span, || Ok(Value::Text(text.clone())))?
                .named(&memory, name, *span)?;
            resources.insert(name.clone(), value);
        }
        let mut evaluator = Evaluator { functions: &self.checker.functions, schemas: &self.checker.schemas, resources: &resources, budget: STEP_LIMIT, memory, output: Vec::new() };
        evaluator.statements(&self.body, &mut HashMap::new())?;
        Ok(evaluator.output.into_iter().map(|line| line.text).collect())
    }
}

#[cfg(test)]
fn run(source: &str) -> Result<Vec<String>> { compile(source)?.execute(&HashMap::new()) }

fn parse_cli(args: &[String]) -> std::result::Result<(String, HashMap<String, String>, bool), String> {
    if args.is_empty() { return Err("Usage: zarya <file.zr> [--safe-output] [--grant-text NAME=PATH]".into()); }
    let filename = args[0].clone();
    let mut grants = HashMap::new();
    let mut force_safe_output = false;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--safe-output" {
            if force_safe_output { return Err("Duplicate --safe-output".into()); }
            force_safe_output = true;
            i += 1;
            continue;
        }
        if args[i] != "--grant-text" || i + 1 >= args.len() { return Err("Expected --grant-text NAME=PATH".into()); }
        let (name, path) = args[i + 1].split_once('=').ok_or("Expected NAME=PATH")?;
        if name.len() > 64 { return Err("Resource name exceeds 64 characters".into()); }
        let mut chars = name.chars();
        if !chars.next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_') ||
            !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') || path.is_empty() {
            return Err("Invalid grant name or empty path".into());
        }
        if grants.insert(name.to_string(), path.to_string()).is_some() { return Err("Duplicate host grant".into()); }
        if grants.len() > RESOURCE_LIMIT { return Err("Host resource count limit exceeded".into()); }
        i += 2;
    }
    Ok((filename, grants, force_safe_output))
}

// Plain text output is data. When a terminal displays it, escape control and
// bidirectional-formatting characters; leave ordinary Unicode untouched.
// JSON-compatible escapes keep serialized JSON valid. This is display safety,
// not an OS sandbox or a guarantee about what data a program may disclose.
fn terminal_unsafe(c: char) -> bool {
    matches!(c, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' |
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' |
        '\u{2066}'..='\u{2069}')
}

fn needs_safe_output(is_terminal: bool, forced: bool) -> bool { is_terminal || forced }

fn write_terminal_line<W: Write>(out: &mut W, line: &str) -> io::Result<()> {
    for c in line.chars() {
        if terminal_unsafe(c) { write!(out, "\\u{:04x}", c as u32)?; }
        else { write!(out, "{}", c)?; }
    }
    writeln!(out)
}

// The operator-selected allowlist is checked for all declarations before any
// resource loader is invoked. This does not classify the contents of grants.
fn load_after_resource_policy<T, F>(program: &Program, policy: Option<&resource_policy::Policy>, load: F) -> Result<T>
where F: FnOnce() -> Result<T> {
    if let Some(policy) = policy { policy.authorize(&program.checker.requested)?; }
    load()
}

fn load_resources(program: &Program, grants: &HashMap<String, String>) -> Result<HashMap<String, String>> {
    let mut resources = HashMap::new();
    for (name, span) in &program.checker.requested {
        if !grants.contains_key(name) { return Err(Error::new(
            format!("Permission denied: host did not grant text resource {}", name), *span)); }
    }
    // Only the host selects paths. Guest names never become filesystem paths.
    for (name, span) in &program.checker.requested {
        let path = grants.get(name).ok_or_else(|| Error::new(
            format!("Permission denied: host did not grant text resource {}", name), *span))?;
        let mut bytes = Vec::new();
        File::open(path).and_then(|file| file.take((TEXT_LIMIT * 4 + 1) as u64).read_to_end(&mut bytes))
            .map_err(|_| Error::new(format!("Cannot load host-granted resource {}", name), *span))?;
        if bytes.len() > TEXT_LIMIT * 4 { return Err(Error::new("Text resource byte limit exceeded", *span)); }
        let text = String::from_utf8(bytes).map_err(|_| Error::new("Text resource must be UTF-8", *span))?;
        if text.chars().count() > TEXT_LIMIT { return Err(Error::new("Text resource limit exceeded", *span)); }
        resources.insert(name.clone(), text);
    }
    Ok(resources)
}

fn read_source(filename: &str) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(filename)?.take((SOURCE_LIMIT + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > SOURCE_LIMIT { return Err(io::Error::new(io::ErrorKind::InvalidData, "Source exceeds 64 KiB")); }
    String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Source must be UTF-8"))
}

enum OutputPolicy {
    Names(output_guard::NamesPolicy),
    External(external_contract::Policy),
}

impl OutputPolicy {
    fn validate(&self, lines: &[String]) -> std::result::Result<String, ()> {
        match self {
            Self::Names(policy) => output_guard::validate(policy, lines),
            Self::External(policy) => policy.validate(lines),
        }
    }
}

fn read_output_policy(filename: &str) -> std::result::Result<external_contract::Policy, ()> {
    let mut bytes = Vec::new();
    File::open(filename).map_err(|_| ())?.take((json::MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() > json::MAX_BYTES { return Err(()); }
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    external_contract::parse(&text)
}

fn read_resource_policy(filename: &str) -> std::result::Result<resource_policy::Policy, ()> {
    let mut bytes = Vec::new();
    File::open(filename).map_err(|_| ())?.take((json::MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() > json::MAX_BYTES { return Err(()); }
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    resource_policy::parse(&text)
}

fn main() -> ExitCode {
    let args: Vec<String> = match env::args_os().skip(1).map(|s| s.into_string()).collect::<std::result::Result<_, _>>() {
        Ok(args) => args,
        Err(_) => { eprintln!("Zarya error: arguments must be Unicode"); return ExitCode::from(2); }
    };
    if args.len() == 1 && args[0] == "--version" {
        println!("Zarya {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.len() == 1 && args[0] == "--help" {
        println!("Usage: zarya <file.zr> [--safe-output] [--grant-text NAME=PATH]\n       zarya --resource-policy-v1 POLICY.json [--contract-policy-v1 OUTPUT.json] <file.zr> [options]\n       zarya --contract-names-v1 PRICE TEXT <file.zr> [options]\n       zarya --contract-policy-v1 POLICY.json <file.zr> [options]\n       zarya --check <file.zr>\n       zarya --check-json <file.zr>\n       zarya --label <file.zr>\n       zarya --label-ar <file.zr>\n       zarya --label-json <file.zr>\n       zarya --check-contract POLICY.json\n       zarya --check-contract-json POLICY.json\n       zarya --explain-contract-json POLICY.json\n       zarya --version");
        return ExitCode::SUCCESS;
    }
    if args.first().map_or(false, |arg| arg == "--check" || arg == "--check-json") {
        if args.len() != 2 {
            eprintln!("Usage: zarya --check[-json] <file.zr>; no resource grants accepted in check mode");
            return ExitCode::from(2);
        }
        return check::cli(&args[1], args[0] == "--check-json");
    }
    if args.first().map_or(false, |arg| arg == "--label" || arg == "--label-ar" || arg == "--label-json") {
        if args.len() != 2 {
            eprintln!("Usage: zarya --label[-ar|-json] <file.zr>; no resource grants accepted in label mode");
            return ExitCode::from(2);
        }
        return label::cli(&args[1], &args[0]);
    }
    if args.first().map_or(false, |arg| arg == "--check-contract" || arg == "--check-contract-json") {
        if args.len() != 2 {
            eprintln!("Usage: zarya --check-contract[-json] POLICY.json; no program or grants accepted");
            return ExitCode::from(2);
        }
        return contract_check::cli(&args[1], args[0] == "--check-contract-json");
    }
    if args.first().map_or(false, |arg| arg == "--explain-contract-json") {
        if args.len() != 2 {
            eprintln!("Usage: zarya --explain-contract-json POLICY.json; no program or grants accepted");
            return ExitCode::from(2);
        }
        return contract_explain::cli(&args[1]);
    }
    // The policy flag must appear first, before the optional output policy.
    if args.iter().skip(1).any(|arg| arg == "--resource-policy-v1") {
        eprintln!("Zarya error: --resource-policy-v1 must appear first");
        return ExitCode::from(2);
    }
    let (resource_args, resource_policy) = if args.first().map_or(false, |a| a == "--resource-policy-v1") {
        if args.len() < 3 {
            eprintln!("Usage: zarya --resource-policy-v1 POLICY.json <file.zr> [options]");
            return ExitCode::from(2);
        }
        let policy = match read_resource_policy(&args[1]) {
            Ok(policy) => policy,
            Err(_) => { eprintln!("Zarya error: invalid or unreadable resource policy"); return ExitCode::from(2); }
        };
        (&args[2..], Some(policy))
    } else { (&args[..], None) };
    let (program_args, output_policy) = if resource_args.first().map_or(false, |a| a == "--contract-names-v1") {
        if resource_args.len() < 4 {
            eprintln!("Usage: zarya --contract-names-v1 PRICE TEXT <file.zr> [options]");
            return ExitCode::from(2);
        }
        let policy = match output_guard::NamesPolicy::new(&resource_args[1], &resource_args[2]) {
            Ok(policy) => policy,
            Err(_) => { eprintln!("Zarya error: invalid output contract arguments"); return ExitCode::from(2); }
        };
        (&resource_args[3..], Some(OutputPolicy::Names(policy)))
    } else if resource_args.first().map_or(false, |a| a == "--contract-policy-v1") {
        if resource_args.len() < 3 {
            eprintln!("Usage: zarya --contract-policy-v1 POLICY.json <file.zr> [options]");
            return ExitCode::from(2);
        }
        let policy = match read_output_policy(&resource_args[1]) {
            Ok(policy) => policy,
            Err(_) => { eprintln!("Zarya error: invalid or unreadable output contract"); return ExitCode::from(2); }
        };
        (&resource_args[2..], Some(OutputPolicy::External(policy)))
    } else { (&resource_args[..], None) };
    if resource_policy.is_some() && program_args.first().map_or(true, |arg| arg.starts_with("--")) {
        eprintln!("Usage: zarya --resource-policy-v1 POLICY.json <file.zr> [options]");
        return ExitCode::from(2);
    }
    let (filename, grants, force_safe_output) = match parse_cli(program_args) {
        Ok(args) => args,
        Err(e) => { eprintln!("{}", e); return ExitCode::from(2); }
    };
    let source = match read_source(&filename) {
        Ok(s) => s,
        Err(e) => { eprintln!("Zarya error: {}", e); return ExitCode::from(1); }
    };
    let result = compile(&source).and_then(|program| {
        let resources = load_after_resource_policy(&program, resource_policy.as_ref(), || load_resources(&program, &grants))?;
        program.execute(&resources)
    });
    match result {
        // Avoid making an additional aggregate copy with lines.join().
        Ok(lines) => {
            if let Some(ref policy) = output_policy {
                let accepted = match policy.validate(&lines) {
                    Ok(value) => value,
                    Err(_) => { eprintln!("Zarya error: output contract rejected"); return ExitCode::from(1); }
                };
                let stdout = io::stdout();
                let mut output = stdout.lock();
                if write_terminal_line(&mut output, &accepted).and_then(|_| output.flush()).is_err() {
                    eprintln!("Zarya error: cannot write accepted output");
                    return ExitCode::from(1);
                }
                return ExitCode::SUCCESS;
            }
            if lines.is_empty() { println!(); }
            if needs_safe_output(io::stdout().is_terminal(), force_safe_output) {
                let stdout = io::stdout();
                let mut output = stdout.lock();
                for line in lines {
                    if write_terminal_line(&mut output, &line).is_err() {
                        eprintln!("Zarya error: cannot write terminal-safe output");
                        return ExitCode::from(1);
                    }
                }
                if output.flush().is_err() {
                    eprintln!("Zarya error: cannot write terminal-safe output");
                    return ExitCode::from(1);
                }
            } else {
                for line in lines { println!("{}", line); }
            }
            ExitCode::SUCCESS
        }
        Err(e) => { eprintln!("{}", e.render(&source, &filename)); ExitCode::from(1) }
    }
}

#[cfg(test)]
mod resource_policy_gate_tests {
    use super::*;

    #[test]
    fn denied_declaration_never_calls_loader() {
        let program = compile("permission text public; permission text secret; print(1);").unwrap();
        let policy = resource_policy::parse(r#"{"version":1,"allow":["public"]}"#).unwrap();
        let mut called = false;
        let result = load_after_resource_policy(&program, Some(&policy), || {
            called = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!called);
    }

    #[test]
    fn unused_declaration_is_still_checked() {
        let program = compile("permission text secret; print(1);").unwrap();
        let policy = resource_policy::parse(r#"{"version":1,"allow":[]}"#).unwrap();
        let mut called = false;
        let result = load_after_resource_policy(&program, Some(&policy), || {
            called = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!called);
    }

    #[test]
    fn permitted_declaration_calls_loader() {
        let program = compile("permission text public; print(1);").unwrap();
        let policy = resource_policy::parse(r#"{"version":1,"allow":["public"]}"#).unwrap();
        let mut called = false;
        let result = load_after_resource_policy(&program, Some(&policy), || {
            called = true;
            Ok(())
        });
        assert!(result.is_ok());
        assert!(called);
    }

    #[test]
    fn absent_policy_preserves_existing_loader_behavior() {
        let program = compile("permission text secret; print(1);").unwrap();
        let mut called = false;
        let result = load_after_resource_policy(&program, None, || {
            called = true;
            Ok(())
        });
        assert!(result.is_ok());
        assert!(called);
    }
}

#[cfg(test)]
mod terminal_output_tests {
    use super::*;

    #[test]
    fn control_and_direction_characters_are_escaped() {
        let mut output = Vec::new();
        write_terminal_line(&mut output, "A\u{1b}[31m\t\r\nكتاب\u{202e}\u{85}").unwrap();
        assert_eq!(String::from_utf8(output).unwrap(),
                   "A\\u001b[31m\\u0009\\u000d\\u000aكتاب\\u202e\\u0085\n");
    }

    #[test]
    fn safe_json_remains_valid_and_roundtrips() {
        let value = "كتاب \u{85}\u{202e}\u{1b}";
        let json = Json::text(value).unwrap().serialize();
        let mut output = Vec::new();
        write_terminal_line(&mut output, &json).unwrap();
        let safe = String::from_utf8(output).unwrap();
        assert!(!safe.contains('\u{85}') && !safe.contains('\u{202e}') && !safe.contains('\u{1b}'));
        let parsed = json_input::parse(safe.trim_end_matches('\n')).unwrap();
        assert_eq!(parsed.as_text().unwrap(), value);
    }

    #[test]
    fn ordinary_unicode_and_existing_escapes_are_unchanged() {
        let mut output = Vec::new();
        write_terminal_line(&mut output, "مرحبا 🦀 \\u001b").unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "مرحبا 🦀 \\u001b\n");
    }

    #[test]
    fn output_mode_is_selected_for_terminal_or_explicit_flag() {
        assert!(needs_safe_output(true, false));
        assert!(needs_safe_output(false, true));
        assert!(needs_safe_output(true, true));
        assert!(!needs_safe_output(false, false));
    }

    #[test]
    fn explicit_flag_and_grants_are_parsed_strictly() {
        let parse = |args: &[&str]| parse_cli(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let (_, grants, safe) = parse(&["x.zr", "--grant-text", "data=file", "--safe-output"]).unwrap();
        assert!(safe);
        assert_eq!(grants.get("data").map(String::as_str), Some("file"));
        assert!(!parse(&["x.zr"]).unwrap().2);
        assert!(parse(&["x.zr", "--safe-output", "--safe-output"]).is_err());
        assert!(parse(&["x.zr", "--safe-output", "--grant-text"]).is_err());
        assert!(parse(&["x.zr", "--other"]).is_err());
    }

    #[test]
    fn output_errors_are_returned_to_caller() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        assert!(write_terminal_line(&mut Broken, "hello").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_escapes_controls_and_preserves_tab_alignment() {
        let source = "\t\u{1b}[31m";
        let error = compile(source).err().unwrap();
        let shown = error.render(source, "name\u{1b}.zr");
        assert!(!shown.contains('\u{1b}'));
        assert!(shown.contains("name\\u{1b}.zr:1:2: Zarya error: Invalid token"));
        assert!(shown.contains("1 |     \\u{1b}[31m\n  |     ^^^^^^"));
    }
    #[test]
    fn diagnostic_escapes_message_and_direction_controls() {
        let source = "مرحبا \u{202e}X";
        let error = Error::new("bad\u{7}message", Span { start: "مرحبا ".len(), end: "مرحبا \u{202e}".len() });
        let shown = error.render(source, "safe.zr");
        assert!(!shown.contains('\u{7}') && !shown.contains('\u{202e}'));
        assert!(shown.contains("bad\\u{7}message"));
        assert!(shown.contains("مرحبا \\u{202e}X"));
    }
    #[test]
    fn diagnostic_preserves_crlf_line_endings_and_unicode_positions() {
        let source = "// مرحبا\r\nlet x: Int = \"bad\";\r\n";
        let error = compile(source).err().unwrap();
        let shown = error.render(source, "demo.zr");
        assert!(shown.starts_with("demo.zr:2:14: Zarya error: Expected Int, got Text"));
        assert!(shown.contains("2 | let x: Int = \"bad\";\n"));
        assert!(!shown.contains('\r') && !shown.contains("\\u{d}"));
    }
    #[test]
    fn assertion_passes_true_and_rejects_false_at_runtime() {
        assert_eq!(run("assert(true); print(7);").unwrap(), ["7"]);
        let error = run("assert(false);").unwrap_err();
        assert_eq!(error.message, "Assertion failed");
    }
    #[test]
    fn assertions_are_type_checked_including_unreachable_branches() {
        for source in ["assert(1);", "assert(\"text\");", "assert();",
            "assert(true, false);", "assert true;", "if false { assert(1); }",
            "let assert: Int = 1;", "fn assert(): Int = 1;"] {
            assert!(compile(source).is_err(), "{}", source);
        }
        assert!(compile("assert(false);").is_ok()); // Static check does not execute.
    }
    #[test]
    fn assertion_reports_the_statement_without_echoing_runtime_values() {
        let source = "let secret: Text = \"PRIVATE_VALUE\";\nassert(secret == \"allowed\");";
        let error = run(source).unwrap_err();
        assert_eq!(error.message, "Assertion failed");
        assert_eq!(&source[error.span.start..error.span.end], "assert(secret == \"allowed\");");
        assert!(!error.render(source, "demo.zr").contains("PRIVATE_VALUE"));
    }
    #[test]
    fn assertions_work_in_loop_scopes_and_only_executed_branches() {
        let source = "let prices: List<Int> = [0, 2100, 2500]; for p in prices { assert(p >= 0); } if false { assert(false); } else { print(\"ok\"); }";
        assert_eq!(run(source).unwrap(), ["ok"]);
        let error = run("let prices: List<Int> = [0, -1]; for p in prices { assert(p >= 0); }").unwrap_err();
        assert_eq!(error.message, "Assertion failed");
    }
    #[test]
    fn assertion_failure_releases_runtime_budget_for_reuse() {
        let program = compile("let price: Int = -1; assert(price >= 0);").unwrap();
        let memory = Budget::new(4096);
        for _ in 0..100 {
            let error = program.execute_with_memory(&HashMap::new(), memory.clone()).unwrap_err();
            assert_eq!(error.message, "Assertion failed");
        }
    }
    #[test]
    fn schema_prices_require_nonnegative_values_before_output() {
        let source = r#"permission text catalog;
            schema Course { price_minor: Int; }
            let courses: List<Json> = json_records(json_parse(read_text("catalog")), "Course");
            print("buffered");
            for course in courses {
                let price: Int = json_as_int(json_get(course, "price_minor"));
                assert(price >= 0);
            }
            print("valid");"#;
        let program = compile(source).unwrap();
        let mut host = HashMap::new();
        host.insert("catalog".into(), r#"[{"price_minor":0},{"price_minor":2100}]"#.into());
        assert_eq!(program.execute(&host).unwrap(), ["buffered", "valid"]);
        host.insert("catalog".into(), r#"[{"price_minor":2100},{"price_minor":-1}]"#.into());
        assert_eq!(program.execute(&host).unwrap_err().message, "Assertion failed");
        host.insert("catalog".into(), r#"[{"price_minor":"2100"}]"#.into());
        assert!(program.execute(&host).unwrap_err().message.contains("expected Int"));
    }
    #[test]
    fn child_bindings_are_removed_on_runtime_failure() {
        let program = compile("let inner: Int = 2; print(9223372036854775807 + 1);").unwrap();
        let memory = Budget::new(memory::LIMIT);
        let outer = memory.copy(&Value::Int(1), Span::default()).unwrap();
        let item = memory.copy(&Value::Int(3), Span::default()).unwrap();
        let mut evaluator = Evaluator {functions: &program.checker.functions, schemas: &program.checker.schemas, resources: &HashMap::new(), budget: STEP_LIMIT, memory, output: Vec::new()};
        let mut env = HashMap::from([("outer".into(), outer)]);
        assert!(evaluator.scoped_statements(&program.body, &mut env, Some(("item", item))).is_err());
        assert_eq!(env.len(), 1);
        assert_eq!(env["outer"].value, Value::Int(1));
    }
    #[test]
    fn academy() {
        let source = include_str!("../../examples/academy.zr");
        assert_eq!(run(source).unwrap(), ["Arab AI Academy", "60", "Group order"]);
    }
    #[test]
    fn precedence() { assert_eq!(run("print(2 + 3 * 4);").unwrap(), ["14"]); }
    #[test]
    fn functions() { assert_eq!(run("fn f(x: Int): Int = x * 2; print(f(3));").unwrap(), ["6"]); }
    #[test]
    fn both_branches_checked() { assert!(run("if false { let x: Int = true; }").is_err()); }
    #[test]
    fn local_scope() { assert!(run("if true { let x: Int = 1; } print(x);").is_err()); }
    #[test]
    fn checked_overflow() { assert!(run("print(9223372036854775807 + 1);").is_err()); }
    #[test]
    fn type_span() {
        let source = "// example\nlet age: Int = \"hello\";";
        let error = run(source).unwrap_err();
        assert_eq!(&source[error.span.start..error.span.end], "\"hello\"");
        assert!(error.render(source, "demo.zr").starts_with("demo.zr:2:16: Zarya error: Expected Int, got Text"));
    }
    #[test]
    fn unicode() { assert_eq!(run("print(\"مرحبا\");").unwrap(), ["مرحبا"]); }
    #[test]
    fn forbidden_operations() { assert!(run("print(read_file(\"secret\"));").is_err()); }
    #[test]
    fn no_recursion() { assert!(run("fn f(x: Int): Int = f(x);").is_err()); }
    #[test]
    fn output_limit() { assert!(run(&"print(1);".repeat(257)).is_err()); }
    #[test]
    fn source_limit() { assert!(run(&" ".repeat(SOURCE_LIMIT + 1)).is_err()); }
    #[test]
    fn deep_input() { assert!(run(&format!("print({}1{});", "(".repeat(100), ")".repeat(100))).is_err()); }
    #[test]
    fn long_left_chain() { assert!(run(&format!("print(1{});", "+1".repeat(1000))).is_err()); }
    #[test]
    fn invalid_syntax() {
        for s in ["@", "print(1)", "if true {", "print(\"bad\ttext\");", "print(\"\\q\");"] { assert!(run(s).is_err(), "{}", s); }
    }
    #[test]
    fn runs_independent() { assert!(run("let x: Int = 1;").is_ok()); assert!(run("print(x);").is_err()); }
    #[test]
    fn duplicate_parameter() { assert!(run("fn f(x: Int, x: Int): Int = x;").is_err()); }
    #[test]
    fn empty_program() { assert_eq!(run("").unwrap(), Vec::<String>::new()); }

    fn resources() -> HashMap<String, String> {
        HashMap::from([("courses".into(), "Python, Rust".into()), ("secret".into(), "private".into())])
    }
    #[test]
    fn permission_alone_does_not_authorize() {
        assert!(run("permission text courses; print(read_text(\"courses\"));").unwrap_err().message.contains("host did not grant"));
    }
    #[test]
    fn host_grant_alone_does_not_authorize() {
        assert!(compile("print(read_text(\"courses\"));").unwrap().execute(&resources()).unwrap_err().message.contains("Permission denied"));
    }
    #[test]
    fn declared_and_granted_resource_can_be_read() {
        let program = compile("permission text courses; print(read_text(\"courses\"));").unwrap();
        assert_eq!(program.execute(&resources()).unwrap(), ["Python, Rust"]);
    }
    #[test]
    fn undeclared_host_resource_stays_hidden() {
        let program = compile("permission text courses; print(read_text(\"secret\"));").unwrap();
        assert!(program.execute(&resources()).is_err());
    }
    #[test]
    fn dynamic_name_does_not_bypass_policy() {
        let program = compile("permission text courses; print(read_text(\"sec\" + \"ret\"));").unwrap();
        assert!(program.execute(&resources()).is_err());
    }
    #[test]
    fn resource_name_is_not_a_path() {
        let program = compile("permission text courses; print(read_text(\"/etc/passwd\"));").unwrap();
        assert!(program.execute(&resources()).is_err());
    }
    #[test]
    fn permission_applies_inside_functions() {
        let source = "permission text courses; fn data(): Text = read_text(\"courses\"); print(data());";
        assert_eq!(compile(source).unwrap().execute(&resources()).unwrap(), ["Python, Rust"]);
    }
    #[test]
    fn permissions_do_not_survive_runs() {
        let program = compile("permission text courses; print(read_text(\"courses\"));").unwrap();
        assert!(program.execute(&resources()).is_ok());
        assert!(program.execute(&HashMap::new()).is_err());
    }
    #[test]
    fn rejects_bad_permissions_and_builtin_calls() {
        for source in [
            "permission network courses;", "permission text courses; permission text courses;",
            "if true { permission text courses; }", "print(read_text(1));", "print(read_text());",
            "fn read_text(): Text = \"forged\";", "let read_text: Int = 1;",
        ] { assert!(compile(source).is_err(), "{}", source); }
    }
    #[test]
    fn resource_size_and_count_bounded() {
        let program = compile("permission text courses;").unwrap();
        let host = HashMap::from([("courses".into(), "x".repeat(TEXT_LIMIT + 1))]);
        assert!(program.execute(&host).is_err());
        let source: String = (0..17).map(|i| format!("permission text r{};", i)).collect();
        assert!(compile(&source).is_err());
    }
    #[test]
    fn unused_permission_still_requires_host_grant() {
        assert!(run("permission text courses; print(1);").is_err());
    }
    #[test]
    fn cli_arguments_are_strict() {
        let parse = |args: &[&str]| parse_cli(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(parse(&["demo.zr", "--grant-text", "courses=data.txt"]).is_ok());
        for args in [vec![], vec!["demo.zr", "--grant-text"], vec!["demo.zr", "--grant-text", "../x=data"],
            vec!["demo.zr", "--grant-text", "x="], vec!["demo.zr", "--all"],
            vec!["demo.zr", "--grant-text", "x=a", "--grant-text", "x=b"]] { assert!(parse(&args).is_err()); }
    }
    #[test]
    fn unrequested_host_paths_are_never_opened() {
        let program = compile("print(1);").unwrap();
        let grants = HashMap::from([("unused".into(), "/does/not/exist".into())]);
        assert!(load_resources(&program, &grants).unwrap().is_empty());
    }
    #[test]
    fn json_type_and_nested_builders() {
        let source = "let j: Json = json_object(\"title\", json_text(\"Python\"), \"price\", json_int(20)); print(json_array(j, json_null(), json_bool(true)));";
        assert_eq!(run(source).unwrap(), ["[{\"price\":20,\"title\":\"Python\"},null,true]"]);
    }
    #[test]
    fn json_functions_are_typed() {
        assert_eq!(run("fn wrap(x: Json): Json = json_array(x); print(wrap(json_int(2)));").unwrap(), ["[2]"]);
    }
    #[test]
    fn json_invalid_types_and_arities_rejected() {
        for source in ["print(json_text(1));", "print(json_int(true));", "print(json_bool(1));",
            "print(json_null(1));", "print(json_array(1));", "print(json_object(\"x\"));",
            "print(json_object(1, json_null()));", "print(json_object(\"x\", true));",
            "fn json_int(): Int = 1;", "let json_array: Int = 1;", "print(json_int(1) + json_int(2));"] {
            assert!(run(source).is_err(), "{}", source);
        }
    }
    #[test]
    fn duplicate_json_key_is_runtime_error() {
        assert!(run("print(json_object(\"x\", json_int(1), \"x\", json_int(2)));").unwrap_err().message.contains("Duplicate"));
    }
    #[test]
    fn json_argument_count_bounded() {
        let source = format!("print(json_array({}));", vec!["json_null()"; 33].join(","));
        assert!(run(&source).is_err());
    }
    #[test]
    fn json_expansion_is_bounded() {
        let mut source = "let j0: Json = json_null();".to_string();
        for i in 1..8 { source += &format!("let j{}: Json = json_array(j{}, j{});", i, i-1, i-1); }
        assert!(run(&source).unwrap_err().message.contains("JSON node limit"));
    }
    #[test]
    fn json_depth_is_bounded_at_runtime() {
        let mut source = "let j0: Json = json_null();".to_string();
        for i in 1..17 { source += &format!("let j{}: Json = json_array(j{});", i, i-1); }
        assert!(run(&source).unwrap_err().message.contains("JSON depth limit"));
    }
    #[test]
    fn json_ordering_is_not_numeric_ordering() {
        assert!(run("print(json_int(1) < json_int(2));").is_err());
        assert_eq!(run("print(json_int(1) == json_int(1));").unwrap(), ["true"]);
    }
}


#[cfg(test)]
mod text_contains_tests {
    use super::*;

    #[test]
    fn basic_case_and_empty_needle() {
        let source = r#"print(text_contains("Python course", "thon"));
            print(text_contains("Python course", "python"));
            print(text_contains("", ""));
            print(text_contains("abc", ""));
            print(text_contains("", "a"));"#;
        assert_eq!(run(source).unwrap(), ["true", "false", "true", "true", "false"]);
    }

    #[test]
    fn unicode_is_exact_without_normalization() {
        let source = "print(text_contains(\"الذكاء الاصطناعي\", \"الذكاء\"));\n\
            print(text_contains(\"é\", \"e\u{301}\"));\n\
            print(text_contains(\"e\u{301}\", \"é\"));\n\
            print(text_contains(\"🙂مرحبا\", \"🙂\"));";
        assert_eq!(run(source).unwrap(), ["true", "false", "false", "true"]);
    }

    #[test]
    fn signatures_and_reserved_names_are_checked() {
        for source in [
            "print(text_contains());", "print(text_contains(\"a\"));",
            "print(text_contains(\"a\", \"b\", \"c\"));",
            "print(text_contains(1, \"b\"));", "print(text_contains(\"a\", true));",
            "let wrong: Text = text_contains(\"a\", \"b\");",
            "if false { print(text_contains(1, \"b\")); }",
            "let text_contains: Int = 1;", "fn text_contains(): Int = 1;",
        ] { assert!(compile(source).is_err(), "{}", source); }
    }

    #[test]
    fn works_in_functions_and_loops() {
        let source = r#"fn match_name(name: Text): Bool = text_contains(name, "thon");
            let names: List<Text> = ["Python", "Zarya"];
            for name in names { if match_name(name) { print(name); } }"#;
        assert_eq!(run(source).unwrap(), ["Python"]);
    }

    #[test]
    fn text_grants_are_still_required() {
        let source = r#"permission text secret; print(text_contains(read_text("secret"), "needle"));"#;
        let program = compile(source).unwrap();
        assert!(program.execute(&HashMap::new()).is_err());
        let mut resources = HashMap::new();
        resources.insert("secret".into(), "a needle in text".into());
        assert_eq!(program.execute(&resources).unwrap(), ["true"]);
    }

    #[test]
    fn memory_budget_is_released_between_runs() {
        let program = compile("print(text_contains(\"مرحبا\", \"رح\"));").unwrap();
        let memory = Budget::new(2048);
        for _ in 0..100 {
            assert_eq!(program.execute_with_memory(&HashMap::new(), memory.clone()).unwrap(), ["true"]);
        }
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn filter_ints_preserves_order_and_original() {
        let source = "let xs:List<Int>=[3,1,2]; let ys:List<Int>=filter(xs,x,x>=2); print(json_list(ys)); print(json_list(xs));";
        assert_eq!(run(source).unwrap(), ["[3,2]", "[3,1,2]"]);
    }

    #[test]
    fn filter_supports_text_bool_and_json_lists() {
        let source = r#"let texts:List<Text>=["one","two"];
            let flags:List<Bool>=[true,false,true];
            let rows:List<Json>=[json_int(1),json_int(2)];
            print(json_list(filter(texts,t,t=="two")));
            print(json_list(filter(flags,f,f)));
            print(json_list(filter(rows,r,json_as_int(r)>1)));"#;
        assert_eq!(run(source).unwrap(), ["[\"two\"]", "[true,true]", "[2]"]);
    }

    #[test]
    fn filter_reads_outer_values_and_keeps_empty_type() {
        let source = "let limit:Int=5; let xs:List<Int>=[4,5,6]; let empty:List<Int>=[]; print(json_list(filter(xs,x,x<=limit))); print(len(filter(empty,z,z>0)));";
        assert_eq!(run(source).unwrap(), ["[4,5]", "0"]);
    }

    #[test]
    fn filter_rejects_wrong_collection_and_predicate_types() {
        for source in ["print(len(filter(1,x,true)));", "print(len(filter([1],x,1)));",
            "print(len(filter([],x,true)));", "let v:List<Int>=filter([1],x,\"text\");"] {
            assert!(compile(source).is_err(), "{}", source);
        }
    }

    #[test]
    fn filter_rejects_shadowing_and_variable_escape() {
        for source in ["let x:Int=1; print(len(filter([1],x,true)));",
            "fn x():Int=1; print(len(filter([1],x,true)));",
            "let xs:List<Int>=filter([1],item,true); print(item);",
            "let filter:Int=1;", "print(len(filter([1],filter,true)));", "print(filter);"] {
            assert!(compile(source).is_err(), "{}", source);
        }
    }

    #[test]
    fn nested_filter_scopes_are_independent() {
        let source = "let xs:List<Int>=[1,2,3]; print(json_list(filter(filter(xs,a,a>=2),b,b<=2))); print(json_list(filter(xs,x,len(filter(xs,y,y==x))>0)));";
        assert_eq!(run(source).unwrap(), ["[2]", "[1,2,3]"]);
    }

    #[test]
    fn filter_works_in_functions_with_parameters() {
        let source = "fn select(xs:List<Int>,limit:Int):List<Int>=filter(xs,x,x<=limit); print(json_list(select([1,2,3],2)));";
        assert_eq!(run(source).unwrap(), ["[1,2]"]);
    }

    #[test]
    fn filter_skips_predicate_for_empty_input_but_checks_its_type() {
        let source = "let xs:List<Int>=[]; print(len(filter(xs,x,9223372036854775807+1>0)));";
        assert_eq!(run(source).unwrap(), ["0"]);
        assert!(compile("let xs:List<Int>=[]; print(len(filter(xs,x,1))); ").is_err());
    }

    #[test]
    fn filter_failure_discards_buffered_output() {
        let source = "let xs:List<Int>=[1,2]; print(5); print(len(filter(xs,x,at(xs,x)>0)));";
        assert!(compile(source).is_ok());
        assert!(run(source).is_err());
    }

    #[test]
    fn filter_nested_loops_share_execution_budget() {
        let source = format!("let xs:List<Int>=[{}]; let ys:List<Int>=filter(xs,x,len(filter(xs,y,y>=x))>0); print(len(ys));", vec!["1";100].join(","));
        assert!(run(&source).unwrap_err().message.contains("step limit"));
    }

    #[test]
    fn filter_accepts_the_existing_list_item_limit() {
        let source = format!("let xs:List<Int>=[{}]; let ys:List<Int>=filter(xs,x,x==1); print(len(ys));", vec!["1";128].join(","));
        assert_eq!(run(&source).unwrap(), ["128"]);
    }

    #[test]
    fn filter_supports_a_single_json_response() {
        let source = r#"let products:List<Json>=[json_object("id",json_int(1),"price",json_int(54)),json_object("id",json_int(2),"price",json_int(25))];
            let ceiling:Int=30;
            let selected:List<Json>=filter(products,p,json_as_int(json_get(p,"price"))<=ceiling);
            print(json_object("max_price",json_int(ceiling),"products",json_list(selected)));"#;
        assert_eq!(run(source).unwrap(), [r#"{"max_price":30,"products":[{"id":2,"price":25}]}"#]);
    }

    #[test]
    fn filter_respects_resource_permissions() {
        let source = r#"permission text term; let xs:List<Text>=["a","b"];
            print(json_list(filter(xs,x,x==read_text("term"))));"#;
        let program = compile(source).unwrap();
        assert!(program.execute(&HashMap::new()).is_err());
        let grants = HashMap::from([("term".into(), "b".into())]);
        assert_eq!(program.execute(&grants).unwrap(), ["[\"b\"]"]);
    }
}

#[cfg(test)]
mod map_tests {
    use super::*;

    #[test]
    fn maps_integers_without_mutating_original() {
        let source = "let xs:List<Int>=[3,1,2]; let ys:List<Int>=map(xs,x=>x*2); print(json_list(xs)); print(json_list(ys));";
        assert_eq!(run(source).unwrap(), ["[3,1,2]", "[6,2,4]"]);
    }

    #[test]
    fn maps_between_scalar_and_json_kinds() {
        let source = r#"let words:List<Text>=["book","cat"];
            let hits:List<Bool>=map(words,w=>text_contains(w,"o"));
            let values:List<Json>=map([1,2],n=>json_int(n));
            print(json_list(hits)); print(json_list(values));"#;
        assert_eq!(run(source).unwrap(), ["[true,false]", "[1,2]"]);
    }

    #[test]
    fn maps_json_records_to_selected_fields() {
        let source = r#"let rows:List<Json>=[json_object("name",json_text("Book"),"secret",json_text("hide")),
                json_object("name",json_text("Laptop"),"secret",json_text("hide"))];
            let names:List<Text>=map(rows,row=>json_as_text(json_get(row,"name")));
            print(json_list(names));"#;
        assert_eq!(run(source).unwrap(), [r#"["Book","Laptop"]"#]);
    }

    #[test]
    fn typed_empty_map_can_be_appended() {
        let source = "let xs:List<Int>=[]; let ys:List<Int>=map(xs,n=>n+1); print(json_list(append(ys,5)));";
        assert_eq!(run(source).unwrap(), ["[5]"]);
    }

    #[test]
    fn empty_map_skips_execution_but_still_checks_types() {
        let source = "let xs:List<Int>=[]; print(len(map(xs,n=>9223372036854775807+1)));";
        assert_eq!(run(source).unwrap(), ["0"]);
        assert!(compile("let xs:List<Int>=[]; print(len(map(xs,n=>text_contains(1,\"x\"))));").is_err());
    }

    #[test]
    fn rejects_invalid_inputs_and_nested_lists() {
        for source in ["print(len(map(1,n=>n)));", "print(len(map([],n=>1)));",
            "print(len(map([1],n=>[n])));", "let ys:List<Text>=map([1],n=>n+1);",
            "print(len(map([1],1=>1)));", "print(len(map([1],json_int=>1)));"] {
            assert!(compile(source).is_err(), "{}", source);
        }
    }

    #[test]
    fn rejects_shadowing_and_escaping_the_local_name() {
        for source in ["let n:Int=1; print(len(map([1],n=>n)));",
            "fn n():Int=1; print(len(map([1],n=>n)));",
            "let ys:List<Int>=map([1],item=>item); print(item);"] {
            assert!(compile(source).is_err(), "{}", source);
        }
    }

    #[test]
    fn composes_with_filter_and_functions() {
        let source = "fn twice(xs:List<Int>):List<Int>=map(xs,x=>x*2); print(json_list(twice(filter([1,2,3],p,p>=2))));";
        assert_eq!(run(source).unwrap(), ["[4,6]"]);
    }

    #[test]
    fn keeps_existing_map_names_and_function_calls() {
        assert_eq!(run("fn map(n:Int):Int=n+1; print(map(4)); print(json_list(map([1,2],x=>x+1)));").unwrap(), ["5", "[2,3]"]);
        assert_eq!(run("let map:Int=7; print(map); print(json_list(map([1],v=>v+map)));").unwrap(), ["7", "[8]"]);
    }

    #[test]
    fn errors_do_not_expose_buffered_output() {
        let source = "print(123); let xs:List<Int>=map([1,2],n=>9223372036854775807+n);";
        assert!(compile(source).is_ok());
        assert!(run(source).unwrap_err().message.contains("overflow"));
    }

    #[test]
    fn memory_is_reusable_after_success_and_failure() {
        let ok = compile("print(len(map([1,2,3],n=>n+1)));").unwrap();
        let fail = compile("print(len(map([1,2],n=>9223372036854775807+n)));").unwrap();
        let memory = Budget::new(16_000);
        for _ in 0..40 {
            assert!(fail.execute_with_memory(&HashMap::new(), memory.clone()).is_err());
            assert_eq!(ok.execute_with_memory(&HashMap::new(), memory.clone()).unwrap(), ["3"]);
        }
    }

    #[test]
    fn nested_map_uses_shared_step_budget() {
        let source = format!("let xs:List<Int>=[{}]; print(len(map(xs,a=>len(map(xs,b=>a+b)))));", vec!["1";100].join(","));
        assert!(run(&source).unwrap_err().message.contains("step limit"));
    }

    #[test]
    fn mapped_lists_obey_payload_limit() {
        let source = format!("let t:Text=\"{}\"; let ys:List<Text>=map([{}],n=>t);", "x".repeat(4096), vec!["1";17].join(","));
        assert!(run(&source).unwrap_err().message.contains("payload limit"));
    }

    #[test]
    fn map_respects_resource_permissions() {
        let source = r#"permission text term; let xs:List<Text>=["a","b"]; print(json_list(map(xs,x=>x+read_text("term"))));"#;
        let program = compile(source).unwrap();
        assert!(program.execute(&HashMap::new()).is_err());
        let grants = HashMap::from([("term".into(), "!".into())]);
        assert_eq!(program.execute(&grants).unwrap(), [r#"["a!","b!"]"#]);
    }

    #[test]
    fn accepts_existing_maximum_number_of_list_items() {
        let source = format!("print(len(map([{}],n=>n)));", vec!["1";128].join(","));
        assert_eq!(run(&source).unwrap(), ["128"]);
    }
}

#[cfg(test)]
mod int64_boundary_tests {
    use super::*;

    #[test]
    fn minimum_literal_with_or_without_space() {
        assert_eq!(run("print(-9223372036854775808); print(- 9223372036854775808);").unwrap(),
                   ["-9223372036854775808", "-9223372036854775808"]);
    }

    #[test]
    fn maximum_still_works_and_out_of_range_literals_fail() {
        assert_eq!(run("print(9223372036854775807);").unwrap(), ["9223372036854775807"]);
        for source in ["print(9223372036854775808);", "print(-9223372036854775809);",
                       "print(-92233720368547758080);", "print(-(9223372036854775808));"] {
            assert!(compile(source).is_err(), "{}", source);
        }
    }

    #[test]
    fn arithmetic_at_minimum_is_checked() {
        assert_eq!(run("print(-9223372036854775808 + 1);").unwrap(), ["-9223372036854775807"]);
        let program = compile("print(-9223372036854775808 - 1);").unwrap();
        assert!(program.execute(&HashMap::new()).unwrap_err().message.contains("Int overflow"));
    }

    #[test]
    fn negating_minimum_overflows_without_emitting_buffered_output() {
        let program = compile("print(7); print(--9223372036854775808);").unwrap();
        assert!(program.execute(&HashMap::new()).unwrap_err().message.contains("Int overflow"));
    }

    #[test]
    fn minimum_round_trips_through_json() {
        let source = r#"let n: Int = json_as_int(json_parse("-9223372036854775808"));
                        print(n == -9223372036854775808); print(json_int(n));"#;
        assert_eq!(run(source).unwrap(), ["true", "-9223372036854775808"]);
    }

    #[test]
    fn minimum_can_be_passed_to_function() {
        let source = "fn identity(x: Int): Int = x; print(identity(-9223372036854775808));";
        assert_eq!(run(source).unwrap(), ["-9223372036854775808"]);
    }

    #[test]
    fn minimum_works_in_branches_and_comparisons() {
        let source = "if false { print(-9223372036854775808); } else { print(-9223372036854775808 < -9223372036854775807); }";
        assert_eq!(run(source).unwrap(), ["true"]);
    }

    #[test]
    fn multiplication_precedence_remains_correct() {
        assert_eq!(run("print(-9223372036854775808 * 0);").unwrap(), ["0"]);
    }
}
