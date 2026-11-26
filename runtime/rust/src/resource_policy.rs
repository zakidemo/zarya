//! Experimental, opt-in resource-name allowlist selected by the host.
//! This policy is checked before loading any declared text resource. It does
//! not classify values, authenticate the operator, or provide an OS sandbox.
use std::collections::{HashMap, HashSet};
use super::{json_input, valid_identifier, Error, Result, Span, RESOURCE_LIMIT};

pub struct Policy { allowed: HashSet<String> }

fn valid_resource(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || !valid_identifier(name) { return false; }
    let mut chars = name.chars();
    chars.next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_') &&
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn parse(source: &str) -> std::result::Result<Policy, ()> {
    let doc = json_input::parse(source).map_err(|_| ())?;
    if !doc.is_object() || doc.len().map_err(|_| ())? != 2 { return Err(()); }
    if doc.get("version").map_err(|_| ())?.as_int().map_err(|_| ())? != 1 { return Err(()); }
    let items = doc.get("allow").map_err(|_| ())?.items().map_err(|_| ())?;
    if items.len() > RESOURCE_LIMIT { return Err(()); }
    let mut allowed = HashSet::new();
    for item in items {
        let name = item.as_text().map_err(|_| ())?;
        if !valid_resource(&name) || !allowed.insert(name) { return Err(()); }
    }
    Ok(Policy { allowed })
}

impl Policy {
    pub fn authorize(&self, requested: &HashMap<String, Span>) -> Result<()> {
        for (name, span) in requested {
            if !self.allowed.contains(name) {
                return Err(Error::new("Resource policy denied a requested resource", *span));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(names: &str) -> std::result::Result<Policy, ()> {
        parse(&format!(r#"{{"version":1,"allow":{}}}"#, names))
    }

    #[test]
    fn empty_and_valid_allowlists_parse() {
        assert!(policy("[]").is_ok());
        assert!(policy(r#"["public_data","_request1"]"#).is_ok());
    }

    #[test]
    fn requires_exact_root_and_version() {
        for source in [r#"{}"#, r#"[]"#, r#"{"version":true,"allow":[]}"#,
            r#"{"version":2,"allow":[]}"#, r#"{"version":1,"allow":[],"extra":true}"#,
            r#"{"version":1,"version":1,"allow":[]}"#] { assert!(parse(source).is_err()); }
    }

    #[test]
    fn requires_an_array_of_text_names() {
        for names in ["null", "true", r#"[1]"#, r#"[{}]"#, r#"[null]"#, r#"[[]]"#] {
            assert!(policy(names).is_err());
        }
    }

    #[test]
    fn duplicate_names_are_rejected_after_json_decoding() {
        assert!(policy(r#"["catalog","catalog"]"#).is_err());
        assert!(policy(r#"["catalog","\u0063atalog"]"#).is_err());
    }

    #[test]
    fn resource_count_is_bounded() {
        let sixteen = (0..RESOURCE_LIMIT).map(|i| format!("\"r{}\"", i)).collect::<Vec<_>>().join(",");
        let seventeen = format!("{},\"extra\"", sixteen);
        assert!(policy(&format!("[{}]", sixteen)).is_ok());
        assert!(policy(&format!("[{}]", seventeen)).is_err());
    }

    #[test]
    fn only_supported_identifier_names_are_allowed() {
        for name in ["", "1start", "path/name", "a-b", "نص", "let", "read_text", "json_parse"] {
            assert!(!valid_resource(name));
        }
        assert!(valid_resource(&"a".repeat(64)));
        assert!(!valid_resource(&"a".repeat(65)));
        assert!(valid_resource("_public2"));
    }

    #[test]
    fn malformed_json_is_rejected() {
        for input in ["", "{", r#"{"version":1,"allow":[],}"#, "\u{feff}{}"] {
            assert!(parse(input).is_err());
        }
    }

    #[test]
    fn all_requested_names_must_be_allowed() {
        let allow = policy(r#"["public"]"#).unwrap();
        let mut requested = HashMap::new();
        requested.insert("public".to_string(), Span::default());
        assert!(allow.authorize(&requested).is_ok());
        requested.insert("secret".to_string(), Span::default());
        assert!(allow.authorize(&requested).is_err());
    }

    #[test]
    fn allowed_names_do_not_require_a_request() {
        let allow = policy(r#"["public","unused"]"#).unwrap();
        assert!(allow.authorize(&HashMap::new()).is_ok());
    }
}
