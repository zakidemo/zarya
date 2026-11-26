//! Static checking only: no execution and no resource loading.
use super::{compile, read_source, Error, ExitCode, Json};
use std::io::{self, Write};

fn text(value: &str) -> Json { Json::text(value).expect("bounded check metadata") }
fn object(fields: Vec<(&str, Json)>) -> Json {
    Json::object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
        .expect("bounded check report")
}

pub fn report(source: Option<&str>, result: std::result::Result<Vec<String>, &Error>) -> Json {
    let (ok, phase, permissions, diagnostics) = match result {
        Ok(mut names) => {
            names.sort();
            let values = names.iter().map(|s| text(s)).collect();
            (true, "check", Json::array(values).expect("bounded permissions"), Json::array(vec![]).unwrap())
        }
        Err(error) => {
            let message: String = error.message.chars().take(512).collect();
            let truncated = error.message.chars().count() > 512;
            let location = source.map(|s| {
                let start = error.span.start.min(s.len());
                let end = error.span.end.min(s.len()).max(start);
                let line = s[..start].chars().filter(|c| *c == '\n').count() + 1;
                let beginning = s[..start].rfind('\n').map_or(0, |i| i+1);
                let column = s[beginning..start].chars().count() + 1;
                (line, column, start, end)
            });
            let number = |index| match location {
                Some((line, column, start, end)) => Json::int([line, column, start, end][index] as i64),
                None => Json::null(),
            };
            let diagnostic = object(vec![
                ("severity", text("error")), ("message", text(&message)),
                ("message_truncated", Json::boolean(truncated)),
                ("line", number(0)), ("column", number(1)),
                ("start_byte", number(2)), ("end_byte", number(3)),
            ]);
            (false, if source.is_some() { "check" } else { "read" },
                Json::array(vec![]).unwrap(), Json::array(vec![diagnostic]).unwrap())
        }
    };
    object(vec![
        ("schema_version", Json::int(1)), ("version", text(env!("CARGO_PKG_VERSION"))),
        ("ok", Json::boolean(ok)), ("phase", text(phase)),
        ("permissions", permissions), ("diagnostics", diagnostics),
    ])
}

// Structured reports contain only JSON and the final line delimiter. Escaping
// raw terminal-control and directional characters changes JSON bytes, but not
// the decoded values. Apply this even to pipes, which might later be displayed.
fn write_report<W: Write>(out: &mut W, value: &Json) -> io::Result<()> {
    super::write_terminal_line(out, &value.serialize())
}

fn emit_report(value: Json, status: ExitCode) -> ExitCode {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if write_report(&mut out, &value).and_then(|_| out.flush()).is_err() {
        eprintln!("Zarya error: cannot write structured check report");
        ExitCode::from(1)
    } else { status }
}

pub fn cli(filename: &str, structured: bool) -> ExitCode {
    let source = match read_source(filename) {
        Ok(source) => source,
        Err(error) => {
            if structured {
                let error = Error::new("Cannot read bounded UTF-8 source", Default::default());
                return emit_report(report(None, Err(&error)), ExitCode::from(1));
            } else { eprintln!("Zarya error: {}", error); }
            return ExitCode::from(1);
        }
    };
    match compile(&source) {
        Ok(program) => {
            if structured {
                let names = program.checker.requested.keys().cloned().collect();
                return emit_report(report(Some(&source), Ok(names)), ExitCode::SUCCESS);
            } else { println!("Check OK (no execution)"); }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if structured { return emit_report(report(Some(&source), Err(&error)), ExitCode::from(1)); }
            else { eprintln!("{}", error.render(&source, filename)); }
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_output_escapes_controls_and_preserves_json_escapes() {
        let error = Error::new("A\u{202e}\\B\u{0085} العربية", Default::default());
        let value = report(None, Err(&error));
        let mut bytes = Vec::new();
        write_report(&mut bytes, &value).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(r"A\u202e\\B\u0085 العربية"));
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains('\u{0085}'));
        assert!(text.ends_with('\n'));
    }
    #[test]
    fn structured_output_propagates_write_failure() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "synthetic"))
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let value = report(None, Ok(Vec::new()));
        assert!(write_report(&mut Broken, &value).is_err());
    }
    #[test]
    fn successful_report_contains_sorted_permission_requests() {
        let output = report(Some(""), Ok(vec!["z".into(), "a".into()])).serialize();
        assert!(output.contains("\"permissions\":[\"a\",\"z\"]"));
        assert!(output.contains("\"ok\":true"));
    }
    #[test]
    fn structured_positions_use_unicode_columns_and_byte_offsets() {
        let source = "// العربية\nlet x: Int = \"bad\";";
        let error = compile(source).err().unwrap();
        let out = report(Some(source), Err(&error)).serialize();
        assert!(out.contains("\"column\":14"));
        assert!(out.contains("\"line\":2"));
        assert!(out.contains(&format!("\"start_byte\":{}", source.find("\"bad\"").unwrap())));
    }
    #[test]
    fn read_error_has_null_locations() {
        let error = Error::new("Cannot read source", Default::default());
        let out = report(None, Err(&error)).serialize();
        assert!(out.contains("\"phase\":\"read\""));
        assert!(out.contains("\"line\":null"));
    }
    #[test]
    fn long_messages_are_bounded_without_invalid_unicode() {
        let error = Error::new("🦀".repeat(2000), Default::default());
        let out = report(Some(""), Err(&error)).serialize();
        assert!(out.len() < 4096);
        assert!(out.contains("\"message_truncated\":true"));
    }
    #[test]
    fn check_does_not_evaluate_overflow_or_require_grants() {
        assert!(compile("permission text data; print(read_text(\"data\")); print(9223372036854775807 + 1);").is_ok());
    }
}
