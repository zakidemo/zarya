//! Experimental opt-in names-only output policy.
//! Runs after the program finishes and before any program output is written.
//! This is not a sandbox, a semantic data classification rule or authorization.
use super::json_input;

pub struct NamesPolicy {
    price: i64,
    text: String,
}

impl NamesPolicy {
    pub fn new(price: &str, text: &str) -> std::result::Result<Self, ()> {
        if price.is_empty() || price.len() > 19 ||
            !price.bytes().all(|b| b.is_ascii_digit()) ||
            text.is_empty() || text.len() > 128 || text.chars().any(char::is_control) {
            return Err(());
        }
        let price = price.parse::<i64>().map_err(|_| ())?;
        Ok(Self { price, text: text.to_string() })
    }
}

pub fn validate(policy: &NamesPolicy, lines: &[String]) -> std::result::Result<String, ()> {
    if lines.len() != 1 { return Err(()); }
    let data = json_input::parse(&lines[0]).map_err(|_| ())?;
    if !data.is_object() || data.len().map_err(|_| ())? != 4 { return Err(()); }
    let count = data.get("count").map_err(|_| ())?.as_int().map_err(|_| ())?;
    let price = data.get("max_price_minor").map_err(|_| ())?.as_int().map_err(|_| ())?;
    let search = data.get("search_text").map_err(|_| ())?.as_text().map_err(|_| ())?;
    let products = data.get("products").map_err(|_| ())?.items().map_err(|_| ())?;
    if products.len() > 20 || count != products.len() as i64 ||
        price != policy.price || search.as_str() != policy.text.as_str() { return Err(()); }
    for row in products {
        if !row.is_object() || row.len().map_err(|_| ())? != 1 { return Err(()); }
        let name = row.get("name").map_err(|_| ())?.as_text().map_err(|_| ())?;
        if name.len() > 4096 || !name.contains(policy.text.as_str()) { return Err(()); }
    }
    Ok(data.serialize())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn check(input: &str) -> bool {
        validate(&NamesPolicy::new("10000", "Book").unwrap(), &[input.to_string()]).is_ok()
    }
    #[test]
    fn valid_and_empty_are_allowed() {
        assert!(check(r#"{"count":1,"max_price_minor":10000,"products":[{"name":"Book Demo"}],"search_text":"Book"}"#));
        assert!(check(r#"{"count":0,"max_price_minor":10000,"products":[],"search_text":"Book"}"#));
    }
    #[test]
    fn extra_fields_are_rejected() {
        assert!(!check(r#"{"count":1,"max_price_minor":10000,"products":[{"name":"Book","id":2}],"search_text":"Book"}"#));
        assert!(!check(r#"{"count":0,"max_price_minor":10000,"products":[],"search_text":"Book","secret":"x"}"#));
    }
    #[test]
    fn inconsistent_query_and_count_are_rejected() {
        assert!(!check(r#"{"count":2,"max_price_minor":10000,"products":[{"name":"Book"}],"search_text":"Book"}"#));
        assert!(!check(r#"{"count":0,"max_price_minor":9,"products":[],"search_text":"Book"}"#));
        assert!(!check(r#"{"count":0,"max_price_minor":10000,"products":[],"search_text":"Other"}"#));
    }
    #[test]
    fn malformed_and_duplicate_json_are_rejected() {
        assert!(!check(r#"{"count":0,"count":0,"max_price_minor":10000,"products":[],"search_text":"Book"}"#));
        assert!(!check(r#"{"count":true,"max_price_minor":10000,"products":[],"search_text":"Book"}"#));
        assert!(!check(r#"{"error":"invalid_products"}"#));
        assert!(!check("{} {}"));
    }
    #[test]
    fn matching_name_and_record_type_are_required() {
        assert!(!check(r#"{"count":1,"max_price_minor":10000,"products":[{"name":"Laptop"}],"search_text":"Book"}"#));
        assert!(!check(r#"{"count":1,"max_price_minor":10000,"products":[{"name":1}],"search_text":"Book"}"#));
        assert!(!check(r#"{"count":1,"max_price_minor":10000,"products":["Book"],"search_text":"Book"}"#));
    }
    #[test]
    fn one_output_only() {
        let p = NamesPolicy::new("10000", "Book").unwrap();
        assert!(validate(&p, &[]).is_err());
        assert!(validate(&p, &["{}".into(), "{}".into()]).is_err());
    }
    #[test]
    fn arguments_are_bounded_and_typed() {
        for price in ["", "-1", "1.0", "9223372036854775808", "10000000000000000000"] {
            assert!(NamesPolicy::new(price, "Book").is_err());
        }
        for text in ["", "hello\n", "\u{7f}"] {
            assert!(NamesPolicy::new("1", text).is_err());
        }
        assert!(NamesPolicy::new("0", "كتاب").is_ok());
    }
    #[test]
    fn output_is_bounded_and_all_records_are_checked() {
        let rows = (0..21).map(|_| r#"{"name":"Book"}"#).collect::<Vec<_>>().join(",");
        assert!(!check(&format!(r#"{{"count":21,"max_price_minor":10000,"products":[{}],"search_text":"Book"}}"#, rows)));
        let long = "Book".to_string() + &"a".repeat(4093);
        assert!(!check(&format!(r#"{{"count":1,"max_price_minor":10000,"products":[{{"name":"{}"}}],"search_text":"Book"}}"#, long)));
        assert!(!check(r#"{"count":2,"max_price_minor":10000,"products":[{"name":"Book"},{"name":"Book","id":4}],"search_text":"Book"}"#));
    }
}
