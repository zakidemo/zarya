//! Experimental contract syntax checks; no guest program is executed.
use super::{read_output_policy, ExitCode, Json};
use std::io::{self, Write};

fn text(value: &str) -> Json { Json::text(value).expect("bounded contract check metadata") }

fn report(ok: bool) -> Json {
    let diagnostics = if ok {
        Json::array(vec![]).expect("bounded diagnostics")
    } else {
        let entry = Json::object(vec![
            ("code".into(), text("INVALID_CONTRACT")),
            ("message".into(), text("Invalid or unreadable contract")),
            ("severity".into(), text("error")),
        ]).expect("bounded diagnostic");
        Json::array(vec![entry]).expect("bounded diagnostics")
    };
    Json::object(vec![
        ("schema_version".into(), Json::int(1)),
        ("version".into(), text(env!("CARGO_PKG_VERSION"))),
        ("ok".into(), Json::boolean(ok)),
        ("phase".into(), text("contract_check")),
        ("diagnostics".into(), diagnostics),
    ]).expect("bounded contract report")
}

fn write_report<W: Write>(out: &mut W, ok: bool) -> io::Result<()> {
    super::write_terminal_line(out, &report(ok).serialize())
}

pub fn cli(filename: &str, structured: bool) -> ExitCode {
    let ok = read_output_policy(filename).is_ok();
    if structured {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        if write_report(&mut out, ok).and_then(|_| out.flush()).is_err() {
            eprintln!("Zarya error: cannot write contract check report");
            return ExitCode::from(1);
        }
    } else if ok {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        if writeln!(out, "Contract OK (no execution)").and_then(|_| out.flush()).is_err() {
            eprintln!("Zarya error: cannot write contract check result");
            return ExitCode::from(1);
        }
    } else {
        eprintln!("Zarya error: invalid or unreadable output contract");
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_report_is_structured() {
        let value = report(true);
        assert!(value.get("ok").unwrap().as_bool().unwrap());
        assert_eq!(value.get("phase").unwrap().as_text().unwrap(), "contract_check");
        assert_eq!(value.get("schema_version").unwrap().as_int().unwrap(), 1);
        assert_eq!(value.get("version").unwrap().as_text().unwrap(), env!("CARGO_PKG_VERSION"));
        assert!(value.get("diagnostics").unwrap().items().unwrap().is_empty());
    }

    #[test]
    fn invalid_report_has_only_generic_diagnostic() {
        let value = report(false);
        assert!(!value.get("ok").unwrap().as_bool().unwrap());
        let entries = value.get("diagnostics").unwrap().items().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].get("code").unwrap().as_text().unwrap(), "INVALID_CONTRACT");
        assert_eq!(entries[0].get("severity").unwrap().as_text().unwrap(), "error");
        assert_eq!(entries[0].get("message").unwrap().as_text().unwrap(), "Invalid or unreadable contract");
    }

    #[test]
    fn report_propagates_write_failure() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "synthetic"))
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        assert!(write_report(&mut Broken, true).is_err());
        assert!(write_report(&mut Broken, false).is_err());
    }
}
