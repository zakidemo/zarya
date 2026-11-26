//! Bounded integer-only JSON input and explicit typed accessors.
//! No path, evaluation, network or implicit scalar conversion is introduced.
use super::{json, lists, memory, Error, Json, OwnedValue, Result, Span, Type, Value};

// Valid trees have <=128 nodes, <=127 key entries and <=16 KiB encoded data.
// Reserve this entire upper bound before parsing, then refund unused credit.
pub const PARSE_RESULT_BYTES: usize = memory::SLOT + 256 * json::MAX_NODES + json::MAX_BYTES;

struct Reader<'a> { source: &'a str, pos: usize, nodes: usize, bytes: usize }
type InputResult<T> = std::result::Result<T, String>;

impl<'a> Reader<'a> {
    fn charge(&mut self, bytes: usize) -> InputResult<()> {
        self.bytes += bytes;
        if self.bytes > json::MAX_BYTES { return Err(self.error("JSON byte limit exceeded")); }
        Ok(())
    }
    fn error(&self, message: &str) -> String { format!("JSON input: {} at byte {}", message, self.pos) }
    fn peek(&self) -> Option<u8> { self.source.as_bytes().get(self.pos).copied() }
    fn ws(&mut self) { while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) { self.pos += 1; } }
    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) { self.pos += 1; true } else { false }
    }
    fn expect(&mut self, byte: u8) -> InputResult<()> {
        if self.take(byte) { Ok(()) } else { Err(self.error("unexpected token")) }
    }
    fn hex4(&mut self) -> InputResult<u32> {
        let mut n = 0;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(c @ b'0'..=b'9') => (c - b'0') as u32,
                Some(c @ b'a'..=b'f') => (c - b'a' + 10) as u32,
                Some(c @ b'A'..=b'F') => (c - b'A' + 10) as u32,
                _ => return Err(self.error("invalid Unicode escape")),
            };
            self.pos += 1;
            n = n * 16 + digit;
        }
        Ok(n)
    }
    fn string(&mut self) -> InputResult<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => { self.pos += 1; return Ok(out); }
                Some(b'\\') => {
                    self.pos += 1;
                    let byte = self.peek().ok_or_else(|| self.error("unterminated escape"))?;
                    if !byte.is_ascii() { return Err(self.error("invalid string escape")); }
                    self.pos += 1;
                    match byte {
                        b'"' | b'\\' | b'/' => out.push(byte as char),
                        b'b' => out.push('\u{8}'), b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'), b'r' => out.push('\r'), b't' => out.push('\t'),
                        b'u' => {
                            let high = self.hex4()?;
                            let scalar = if (0xD800..=0xDBFF).contains(&high) {
                                if !self.take(b'\\') || !self.take(b'u') { return Err(self.error("unpaired Unicode surrogate")); }
                                let low = self.hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&low) { return Err(self.error("unpaired Unicode surrogate")); }
                                0x10000 + ((high - 0xD800) << 10) + low - 0xDC00
                            } else { high };
                            out.push(char::from_u32(scalar).ok_or_else(|| self.error("unpaired Unicode surrogate"))?);
                        }
                        _ => return Err(self.error("invalid string escape")),
                    }
                }
                Some(c) if c < 0x20 => return Err(self.error("unescaped control character")),
                Some(_) => {
                    // pos advances only over ASCII syntax or a complete UTF-8 scalar.
                    let c = self.source[self.pos..].chars().next().unwrap();
                    self.pos += c.len_utf8();
                    out.push(c);
                }
            }
        }
    }
    fn number(&mut self) -> InputResult<Json> {
        let start = self.pos;
        self.take(b'-');
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) { return Err(self.error("leading zero")); }
            }
            Some(b'1'..=b'9') => { while matches!(self.peek(), Some(b'0'..=b'9')) { self.pos += 1; } }
            _ => return Err(self.error("invalid integer")),
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) { return Err(self.error("only signed 64-bit integers are supported")); }
        let n = self.source[start..self.pos].parse::<i64>().map_err(|_| self.error("integer out of range"))?;
        let value = Json::int(n);
        self.charge(value.byte_len())?;
        Ok(value)
    }
    fn value(&mut self, depth: usize) -> InputResult<Json> {
        if depth > json::MAX_DEPTH { return Err(self.error("JSON depth limit exceeded")); }
        if self.nodes >= json::MAX_NODES { return Err(self.error("JSON node limit exceeded")); }
        self.nodes += 1;
        self.ws();
        match self.peek() {
            Some(b'"') => {
                let text = self.string()?;
                self.charge(json::quoted_bytes(&text))?;
                Json::text(&text)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b'[') => {
                self.pos += 1;
                self.charge(2)?;
                self.ws();
                let mut items = Vec::new();
                if !self.take(b']') {
                    loop {
                        items.push(self.value(depth + 1)?);
                        self.ws();
                        if self.take(b']') { break; }
                        self.expect(b',')?;
                        self.charge(1)?;
                    }
                }
                Json::array(items)
            }
            Some(b'{') => {
                self.pos += 1;
                self.charge(2)?;
                self.ws();
                let mut items: Vec<(String, Json)> = Vec::new();
                if !self.take(b'}') {
                    loop {
                        self.ws();
                        let key = self.string()?;
                        if items.iter().any(|(name, _)| name == &key) { return Err(self.error("Duplicate JSON object key")); }
                        self.charge(json::quoted_bytes(&key) + 1)?;
                        self.ws(); self.expect(b':')?;
                        items.push((key, self.value(depth + 1)?));
                        self.ws();
                        if self.take(b'}') { break; }
                        self.expect(b',')?;
                        self.charge(1)?;
                    }
                }
                Json::object(items)
            }
            _ => {
                for (token, value) in [("true", Json::boolean(true)), ("false", Json::boolean(false)), ("null", Json::null())] {
                    if self.source[self.pos..].starts_with(token) { self.pos += token.len(); self.charge(value.byte_len())?; return Ok(value); }
                }
                Err(self.error("expected a JSON value"))
            }
        }
    }
}

pub fn parse(source: &str) -> InputResult<Json> {
    if source.len() > json::MAX_BYTES { return Err("JSON input byte limit exceeded".into()); }
    let mut reader = Reader { source, pos: 0, nodes: 0, bytes: 0 };
    let value = reader.value(1)?;
    reader.ws();
    if reader.pos != source.len() { return Err(reader.error("trailing data")); }
    Ok(value)
}

pub fn decode_text(encoded: &str) -> InputResult<String> {
    // Only used for validated canonical scalar strings; retain a bound here too.
    if encoded.len() > json::MAX_BYTES { return Err("JSON text limit exceeded".into()); }
    let mut reader = Reader { source: encoded, pos: 0, nodes: 0, bytes: 0 };
    let text = reader.string()?;
    if reader.pos != encoded.len() { return Err(reader.error("trailing data")); }
    if text.chars().count() > super::TEXT_LIMIT { return Err("Text limit exceeded".into()); }
    Ok(text)
}

pub fn builtin(name: &str) -> bool {
    matches!(name, "json_parse" | "json_get" | "json_at" | "json_len" | "json_items" |
        "json_as_text" | "json_as_int" | "json_as_bool" | "json_is_null")
}

pub fn signature(name: &str, span: Span) -> Result<(Vec<Type>, Type)> {
    Ok(match name {
        "json_parse" => (vec![Type::Text], Type::Json),
        "json_get" => (vec![Type::Json, Type::Text], Type::Json),
        "json_at" => (vec![Type::Json, Type::Int], Type::Json),
        "json_len" | "json_as_int" => (vec![Type::Json], Type::Int),
        "json_as_text" => (vec![Type::Json], Type::Text),
        "json_as_bool" | "json_is_null" => (vec![Type::Json], Type::Bool),
        "json_items" => (vec![Type::Json], Type::JsonList),
        _ => return Err(Error::new("Unknown JSON input builtin", span)),
    })
}

pub fn result_bytes(name: &str, args: &[OwnedValue]) -> usize {
    if name == "json_parse" { return PARSE_RESULT_BYTES; }
    match args.first().map(|v| &v.value) {
        Some(Value::Json(j)) => match name {
            "json_get" | "json_at" => memory::SLOT + j.memory_bytes(),
            "json_as_text" => memory::SLOT + j.byte_len(),
            "json_items" => j.items().map_or(memory::SLOT, |items| items.iter().fold(memory::SLOT,
                |n, item| n.saturating_add(memory::SLOT + item.memory_bytes()))),
            _ => memory::SLOT,
        },
        _ => memory::SLOT,
    }
}

pub fn call(name: &str, args: Vec<Value>, span: Span) -> Result<Value> {
    let invalid = || Error::new("Invalid JSON input builtin arguments", span);
    let error = |message| Error::new(message, span);
    if name == "json_parse" {
        return match args.as_slice() { [Value::Text(s)] => parse(s).map(Value::Json).map_err(error), _ => Err(invalid()) };
    }
    let j = match args.first() { Some(Value::Json(j)) => j, _ => return Err(invalid()) };
    let value = match (name, args.as_slice()) {
        ("json_get", [_, Value::Text(key)]) => Value::Json(j.get(key).map_err(error)?.clone()),
        ("json_at", [_, Value::Int(index)]) => Value::Json(j.at(*index).map_err(error)?.clone()),
        ("json_len", [_]) => Value::Int(j.len().map_err(error)? as i64),
        ("json_as_text", [_]) => Value::Text(j.as_text().map_err(error)?),
        ("json_as_int", [_]) => Value::Int(j.as_int().map_err(error)?),
        ("json_as_bool", [_]) => Value::Bool(j.as_bool().map_err(error)?),
        ("json_is_null", [_]) => Value::Bool(j.is_null()),
        ("json_items", [_]) => return lists::make(j.items().map_err(error)?.iter().cloned().map(Value::Json).collect(), Type::JsonList, span),
        _ => return Err(invalid()),
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{compile, run};
    #[test]
    fn parses_scalars_nested_values_and_canonicalizes() {
        let j = parse(r#" { "z": [true,false,null,-0], "a": "نص 🦀" } "#).unwrap();
        assert_eq!(j.serialize(), r#"{"a":"نص 🦀","z":[true,false,null,0]}"#);
        assert_eq!(j.get("a").unwrap().as_text().unwrap(), "نص 🦀");
        assert_eq!(j.get("z").unwrap().len().unwrap(), 4);
        assert!(j.get("z").unwrap().at(2).unwrap().is_null());
        assert_eq!(parse(&j.serialize()).unwrap(), j);
    }
    #[test]
    fn integer_boundaries_and_non_integer_numbers() {
        for n in [i64::MIN, -1, 0, 1, i64::MAX] { assert_eq!(parse(&n.to_string()).unwrap().as_int().unwrap(), n); }
        for input in ["9223372036854775808", "-9223372036854775809", "00", "-01", "+1", "-", "1.0", "1e2", "1E+2", "NaN", "Infinity"] {
            assert!(parse(input).is_err(), "{}", input);
        }
    }
    #[test]
    fn decodes_all_escapes_and_surrogate_pairs() {
        let j = parse(r#"{"a":"\u0061","crab":"\uD83E\uDD80","controls":"\/\b\f\n\r\t\\\"\u0000"}"#).unwrap();
        assert_eq!(j.get("a").unwrap().as_text().unwrap(), "a");
        assert_eq!(j.get("crab").unwrap().as_text().unwrap(), "🦀");
        assert_eq!(j.get("controls").unwrap().as_text().unwrap(), "/\u{8}\u{c}\n\r\t\\\"\0");
        assert_eq!(parse(&j.serialize()).unwrap(), j);
    }
    #[test]
    fn rejects_unpaired_surrogates_controls_and_bad_escapes() {
        for input in [r#""\uD800""#, r#""\uDC00""#, r#""\uD800\u0061""#, r#""\uD800x""#,
            r#""\uZZZZ""#, r#""\u123""#, r#""\x20""#, r#""\é""#, "\"raw\nline\"", "\"raw\0nul\"", "\"unterminated", "\"\\"] {
            assert!(parse(input).is_err(), "{:?}", input);
        }
    }
    #[test]
    fn duplicate_keys_are_compared_after_decoding() {
        for input in [r#"{"a":1,"a":2}"#, r#"{"a":1,"\u0061":2}"#, r#"{"\\":1,"\u005c":2}"#] {
            assert!(parse(input).unwrap_err().contains("Duplicate"));
        }
    }
    #[test]
    fn strict_syntax_and_complete_consumption() {
        for input in ["", " ", "\u{feff}{}", "\u{a0}{}", "/*x*/{}", "[1,]", "{\"a\":1,}", "[1 2]",
            "{a:1}", "{'a':1}", "true false", "{}x", "[", "{", "TRUE", "null\0", "{\"a\" 1}"] {
            assert!(parse(input).is_err(), "{:?}", input);
        }
        assert!(parse(" \t\r\n{}\t ").is_ok());
    }
    #[test]
    fn depth_and_nodes_are_bounded_before_unbounded_recursion() {
        assert!(parse(&format!("{}0{}", "[".repeat(15), "]".repeat(15))).is_ok());
        assert!(parse(&format!("{}0{}", "[".repeat(16), "]".repeat(16))).unwrap_err().contains("depth"));
        assert!(parse(&"[".repeat(4000)).unwrap_err().contains("depth"));
        assert!(parse(&format!("[{}]", vec!["null";127].join(","))).is_ok());
        assert!(parse(&format!("[{}]", vec!["null";128].join(","))).unwrap_err().contains("node"));
    }
    #[test]
    fn input_and_canonical_expansion_have_separate_byte_bounds() {
        assert!(parse(&" ".repeat(json::MAX_BYTES + 1)).unwrap_err().contains("byte"));
        assert!(parse(&format!("\"{}\"", "x".repeat(json::MAX_BYTES - 2))).is_ok());
        // Small raw escape sequences can be larger in canonical output.
        assert!(parse(&format!(r#"["{}","{}"]"#, "\\b".repeat(1500), "\\b".repeat(1500))).unwrap_err().contains("byte"));
        let j = parse(&format!("\"{}\"", "x".repeat(super::super::TEXT_LIMIT + 1))).unwrap();
        assert!(j.as_text().unwrap_err().contains("Text limit"));
    }
    #[test]
    fn typed_accessors_and_iteration_run_in_the_language() {
        let source = r#"let root: Json = json_parse("{\"items\":[1,2],\"ok\":true,\"title\":\"نص\",\"empty\":null}");
            print(json_as_text(json_get(root,"title"))); print(json_as_bool(json_get(root,"ok")));
            print(json_is_null(json_get(root,"empty"))); print(json_len(root));
            let xs: List<Json> = json_items(json_get(root,"items"));
            for x in xs { print(json_as_int(x)); }
            print(json_as_int(json_at(json_get(root,"items"),1)));"#;
        assert_eq!(run(source).unwrap(), ["نص", "true", "true", "4", "1", "2", "2"]);
    }
    #[test]
    fn wrong_json_kinds_missing_fields_and_indexes_fail_without_coercion() {
        for source in [r#"print(json_as_int(json_text("20")));"#, "print(json_as_text(json_int(20)));",
            "print(json_as_bool(json_int(1)));", "print(json_len(json_null()));", "print(json_items(json_object()));",
            "print(json_get(json_array(),\"x\"));", "print(json_get(json_object(),\"x\"));",
            "print(json_at(json_array(),0));", "print(json_at(json_array(json_int(1)),-1));"] {
            assert!(run(source).is_err(), "{}", source);
        }
        assert_eq!(run("print(json_is_null(json_int(0)));").unwrap(), ["false"]);
    }
    #[test]
    fn signatures_and_reserved_names_are_checked_before_execution() {
        for source in ["print(json_parse(1));", "print(json_parse());", "print(json_get(json_null(),1));",
            "print(json_at(json_null(),true));", "print(json_as_text(1));", "print(json_len());",
            "let xs:List<Int>=json_items(json_array());", "if false { print(json_parse(1)); }"] {
            assert!(compile(source).is_err(), "{}", source);
        }
        for name in ["json_parse", "json_get", "json_at", "json_len", "json_items", "json_as_text", "json_as_int", "json_as_bool", "json_is_null"] {
            assert!(compile(&format!("fn {}(): Int = 1;", name)).is_err());
        }
        assert!(compile(r#"print(json_parse("malformed"));"#).is_ok());
        assert!(run(r#"print(json_parse("malformed"));"#).is_err());
    }
    #[test]
    fn parsing_a_name_does_not_grant_access() {
        let program = compile(r#"permission text public;
            let j: Json = json_parse(read_text("public"));
            print(read_text(json_as_text(json_get(j,"resource"))));"#).unwrap();
        let host = std::collections::HashMap::from([("public".into(), r#"{"resource":"secret"}"#.into()), ("secret".into(), "PRIVATE_SENTINEL".into())]);
        assert!(program.execute(&host).unwrap_err().message.contains("Permission denied"));
    }
}
