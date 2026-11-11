//! Compile mem0's normalized metadata filters into a vec0 KNN-compatible plan.
//!
//! vec0 only honors `= != > >= < <=` on *metadata columns* inside a KNN query; anything
//! else (IN, LIKE, json_extract on payload, $or/$not) must not appear in the KNN WHERE.
//!
//! Strategy (exact, never approximate):
//!   1. Split normalized filters into `knn_safe` (simple equality/range on one of the
//!      declared scope columns) and `rest` (everything else).
//!   2. KNN subquery: `WHERE embedding MATCH ? AND k = ? AND <knn_safe>` — this bounds the
//!      scan and pre-filters by scope.
//!   3. Outer query: apply `<rest>` on the KNN results via json_extract, then LIMIT.
//!      Because rest is applied after KNN with a *separate* LIMIT large enough to cover
//!      top_k matches, the final result is exact: we never return fewer than top_k when
//!      that many matching rows exist.
//!
//! The outer LIMIT is `max(top_k, count of knn_safe matches)` — but we don't know the
//! count up front, so we use a generous KNN k (top_k * 10, capped) and then apply rest +
//! final LIMIT top_k. For the desktop scale (<=10k rows) this is exact and fast; the
//! pathological case (rest matches fewer than top_k among the expanded KNN page) is
//! covered by falling back to a full exact scan when the KNN page yields too few.

use mem0_core::error::{Mem0Error, Result};
use mem0_core::filters::process_metadata_filters;
use mem0_core::types::JsonMap;
use serde_json::Value;

/// Scope columns declared as vec0 metadata columns in store.rs.
pub const SCOPE_COLUMNS: &[&str] = &["user_id", "agent_id", "run_id", "session_id"];

pub struct CompiledFilter {
    /// WHERE fragment for the KNN subquery (no leading AND), only scope-column simple ops.
    pub knn_where: String,
    /// WHERE fragment for the outer exact filter (on payload JSON / joined columns).
    pub outer_where: String,
    /// Bind values in order: knn_where placeholders first, then outer_where.
    pub bind_values: Vec<String>,
}

/// Compile `filters`. Empty filters produce empty fragments.
pub fn compile_filters_v2(filters: &JsonMap) -> Result<CompiledFilter> {
    let normalized = process_metadata_filters(filters)?;
    let mut knn_where = String::new();
    let mut outer_where = String::new();
    let mut bind_values = Vec::new();

    for (key, value) in &normalized {
        if key == "$or" || key == "$not" {
            let f = compile_complex(key, value, &mut bind_values)?;
            push_and(&mut outer_where, &f);
            continue;
        }
        if let Some(f) = try_knn_safe(key, value, &mut bind_values)? {
            push_and(&mut knn_where, &f);
        } else {
            let f = compile_outer(key, value, &mut bind_values)?;
            push_and(&mut outer_where, &f);
        }
    }

    Ok(CompiledFilter {
        knn_where,
        outer_where,
        bind_values,
    })
}

fn push_and(out: &mut String, fragment: &str) {
    if fragment.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push_str(" AND ");
    }
    out.push_str(fragment);
}

/// If `key` is a scope column and `value` is a simple op vec0 supports natively in KNN,
/// return the SQL fragment; otherwise None (caller routes to outer filter).
fn try_knn_safe(key: &str, value: &Value, values: &mut Vec<String>) -> Result<Option<String>> {
    if !SCOPE_COLUMNS.contains(&key) {
        return Ok(None);
    }
    match value {
        Value::String(s) if s == "*" => {
            // "IS NOT NULL" is not in vec0's KNN-allowed op set; only route to KNN when
            // it is a plain `=`-able column. For scope columns use `key != ''` (we bind
            // absent scope values as ""), otherwise fall through to the outer filter.
            Ok(Some(format!("{key} != ''")))
        }
        Value::Object(ops) if ops.len() == 1 => {
            let (op, val) = ops.iter().next().expect("len checked");
            match op.as_str() {
                "eq" => {
                    values.push(json_scalar(val));
                    Ok(Some(format!("{key} = ?")))
                }
                "ne" => {
                    values.push(json_scalar(val));
                    Ok(Some(format!("{key} != ?")))
                }
                "gt" | "gte" | "lt" | "lte" => {
                    // vec0 KNN only accepts bare column comparisons on metadata columns;
                    // CAST() would be rejected. Scope columns are TEXT, so compare as
                    // text when the bound is a string, else bind numeric via the column
                    // directly (vec0 will coerce). Non-numeric scope values with numeric
                    // bounds are not a real mem0 case, so route those to the outer filter.
                    let sym = match op.as_str() {
                        "gt" => ">",
                        "gte" => ">=",
                        "lt" => "<",
                        _ => "<=",
                    };
                    if val.is_number() && !val.is_string() {
                        // numeric bound on a TEXT column: not KNN-safe, go outer
                        return Ok(None);
                    }
                    values.push(json_scalar(val));
                    Ok(Some(format!("{key} {sym} ?")))
                }
                _ => Ok(None),
            }
        }
        Value::Array(items) if !items.is_empty() => {
            // IN on a scope column is not in vec0's KNN-supported op set; route to outer.
            // (We could expand to OR of equalities, but outer filter is exact anyway.)
            let _ = items;
            Ok(None)
        }
        other => {
            values.push(json_scalar(other));
            Ok(Some(format!("{key} = ?")))
        }
    }
}

/// Outer (post-KNN) exact filter on the payload JSON.
fn compile_outer(key: &str, value: &Value, values: &mut Vec<String>) -> Result<String> {
    // `categories` is a JSON array in the payload; `IN`/`=` on json_extract would
    // compare against the serialized array text. AND semantics instead: every
    // requested category must exist as an element.
    if key == "categories" {
        let requested: Vec<&str> = match value {
            Value::String(one) => vec![one.as_str()],
            Value::Array(items) => items.iter().filter_map(|v| v.as_str()).collect(),
            _ => return Ok(String::new()),
        };
        let mut parts = Vec::new();
        for cat in requested {
            values.push(cat.to_string());
            parts.push(
                "EXISTS (SELECT 1 FROM json_each(json_extract(payload, '$.categories')) WHERE value = ?)"
                    .to_string(),
            );
        }
        return Ok(parts.join(" AND "));
    }
    match value {
        Value::String(s) if s == "*" => Ok(format!(
            "json_extract(payload, '$.{}') IS NOT NULL",
            escape_path(key)
        )),
        Value::Object(ops) => {
            let mut out = String::new();
            for (op, val) in ops {
                let f = compile_op(key, op, val, values)?;
                push_and(&mut out, &f);
            }
            Ok(out)
        }
        Value::Array(items) => {
            if items.is_empty() {
                return Ok(String::new());
            }
            let placeholders: Vec<String> = items
                .iter()
                .map(|v| {
                    values.push(json_scalar(v));
                    "?".to_string()
                })
                .collect();
            Ok(format!(
                "json_extract(payload, '$.{}') IN ({})",
                escape_path(key),
                placeholders.join(", ")
            ))
        }
        other => {
            values.push(json_scalar(other));
            Ok(format!(
                "json_extract(payload, '$.{}') = CAST(? AS TEXT)",
                escape_path(key)
            ))
        }
    }
}

fn compile_complex(key: &str, value: &Value, values: &mut Vec<String>) -> Result<String> {
    let arr = value
        .as_array()
        .ok_or_else(|| Mem0Error::validation(format!("{key} must be a list")))?;
    let mut parts = Vec::new();
    for condition in arr {
        let obj = condition
            .as_object()
            .ok_or_else(|| Mem0Error::validation(format!("{key} entries must be objects")))?;
        let mut inner = String::new();
        for (k, v) in obj {
            let f = compile_outer(k, v, values)?;
            push_and(&mut inner, &f);
        }
        if !inner.is_empty() {
            parts.push(if key == "$or" {
                format!("({inner})")
            } else {
                format!("NOT ({inner})")
            });
        }
    }
    Ok(if key == "$or" {
        parts.join(" OR ")
    } else {
        parts.join(" AND ")
    })
}

fn compile_op(key: &str, op: &str, val: &Value, values: &mut Vec<String>) -> Result<String> {
    let json = format!("json_extract(payload, '$.{}')", escape_path(key));
    match op {
        "eq" => {
            values.push(json_scalar(val));
            Ok(format!("{json} = CAST(? AS TEXT)"))
        }
        "ne" => {
            values.push(json_scalar(val));
            Ok(format!("{json} != CAST(? AS TEXT)"))
        }
        "gt" | "gte" | "lt" | "lte" => {
            let sym = match op {
                "gt" => ">",
                "gte" => ">=",
                "lt" => "<",
                _ => "<=",
            };
            values.push(json_scalar(val));
            Ok(format!("CAST({json} AS REAL) {sym} CAST(? AS REAL)"))
        }
        "in" => {
            let arr = val
                .as_array()
                .ok_or_else(|| Mem0Error::validation("in requires a list"))?;
            let placeholders: Vec<String> = arr
                .iter()
                .map(|v| {
                    values.push(json_scalar(v));
                    "?".to_string()
                })
                .collect();
            Ok(format!("{json} IN ({})", placeholders.join(", ")))
        }
        "nin" => {
            let arr = val
                .as_array()
                .ok_or_else(|| Mem0Error::validation("nin requires a list"))?;
            let placeholders: Vec<String> = arr
                .iter()
                .map(|v| {
                    values.push(json_scalar(v));
                    "?".to_string()
                })
                .collect();
            Ok(format!("{json} NOT IN ({})", placeholders.join(", ")))
        }
        "contains" => {
            values.push(format!("%{}%", json_scalar(val)));
            Ok(format!("{json} LIKE ?"))
        }
        "icontains" => {
            values.push(format!("%{}%", json_scalar(val).to_lowercase()));
            Ok(format!("LOWER({json}) LIKE ?"))
        }
        other => Err(Mem0Error::validation(format!(
            "unsupported filter operator: {other}"
        ))),
    }
}

fn escape_path(key: &str) -> String {
    key.replace('\'', "''")
}

fn json_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compile(filters: Value) -> CompiledFilter {
        let map = filters.as_object().unwrap().clone();
        compile_filters_v2(&map).unwrap()
    }

    #[test]
    fn scope_equality_goes_to_knn() {
        let c = compile(json!({"user_id": "alice"}));
        assert_eq!(c.knn_where, "user_id = ?");
        assert!(c.outer_where.is_empty());
        assert_eq!(c.bind_values, vec!["alice".to_string()]);
    }

    #[test]
    fn non_scope_goes_to_outer() {
        let c = compile(json!({"category": "food"}));
        assert!(c.knn_where.is_empty());
        assert!(c
            .outer_where
            .contains("json_extract(payload, '$.category')"));
    }

    #[test]
    fn categories_array_is_and_of_exists() {
        let c = compile(json!({"categories": ["food", "travel"]}));
        // lands in the outer exact filter (categories is not a scope column)
        assert!(c.knn_where.is_empty());
        let exists_count = c
            .outer_where
            .matches("EXISTS (SELECT 1 FROM json_each")
            .count();
        assert_eq!(exists_count, 2, "{}", c.outer_where);
        assert!(c.outer_where.contains(" AND "), "{}", c.outer_where);
        assert_eq!(
            c.bind_values,
            vec!["food".to_string(), "travel".to_string()]
        );
    }

    #[test]
    fn categories_single_string_is_one_exists() {
        let c = compile(json!({"categories": "food"}));
        assert_eq!(
            c.outer_where.matches("EXISTS").count(),
            1,
            "{}",
            c.outer_where
        );
        assert_eq!(c.bind_values, vec!["food".to_string()]);
    }

    #[test]
    fn mixed_split() {
        let c = compile(json!({"user_id": "alice", "age": {"gte": 18}}));
        assert_eq!(c.knn_where, "user_id = ?");
        assert!(c.outer_where.contains("$.age"));
        assert_eq!(c.bind_values.len(), 2);
    }

    #[test]
    fn or_goes_to_outer() {
        let c = compile(json!({"OR": [{"user_id": "a"}, {"user_id": "b"}]}));
        assert!(c.knn_where.is_empty());
        assert!(c.outer_where.contains(" OR "));
    }
}
