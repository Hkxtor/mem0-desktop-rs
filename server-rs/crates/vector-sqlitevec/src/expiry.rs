/// The expiration rule the store needs, kept identical to `mem0::util::payload_is_expired`
/// (a date-only `expiration_date` in the past). Duplicated here rather than imported because the
/// store is a separate crate that owns the payload column; the two must not drift, and a test
/// pins the comparison.
pub(crate) fn payload_is_expired(payload: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    let Some(raw) = value
        .get("expiration_date")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    match chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        Ok(d) => d < chrono::Utc::now().date_naive(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem0_core::types::JsonMap;
    use serde_json::json;

    fn payload(date: Option<&str>) -> String {
        match date {
            Some(d) => json!({"data": "x", "expiration_date": d}).to_string(),
            None => json!({"data": "x"}).to_string(),
        }
    }

    /// The whole point of this module is that it agrees with `mem0::util::payload_is_expired`,
    /// which decides what `list_all` and `search` show. If the two ever disagree, the
    /// "unclassified" count starts offering to tag memories nobody can see — or stops offering
    /// ones they can. Compare them directly rather than re-asserting each rule on its own.
    #[test]
    fn the_expiry_rule_matches_the_core_one() {
        for date in [
            Some("2000-01-01"),
            Some("2999-12-31"),
            Some("not-a-date"),
            Some(""),
            None,
        ] {
            let raw = payload(date);
            let value: JsonMap = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                payload_is_expired(&raw),
                mem0_core::util::payload_is_expired(&value),
                "the two rules disagree for expiration_date={date:?}"
            );
        }
        // A payload that is not JSON at all is not something the store can expire.
        assert!(!payload_is_expired("{not json"));
    }
}
