//! Fluent query and aggregation builders for AeorDB plugins.
//!
//! These builders serialize to the JSON format expected by the AeorDB query
//! engine and call the corresponding host functions via FFI.
//!
//! # Query JSON format
//!
//! ```json
//! {
//!     "path": "/users",
//!     "where": { "field": "name", "op": "contains", "value": "Wyatt" },
//!     "limit": 10,
//!     "order_by": [{"field": "name", "direction": "asc"}]
//! }
//! ```
//!
//! Boolean combinations use `AND` / `OR` / `NOT` keys:
//!
//! ```json
//! {
//!     "path": "/users",
//!     "where": {
//!         "AND": [
//!             { "field": "name", "op": "contains", "value": "Wyatt" },
//!             { "field": "age", "op": "gt", "value": 21 }
//!         ]
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::PluginError;

// ---------------------------------------------------------------------------
// Internal query node representation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum QueryNode {
    Field {
        field: String,
        op: String,
        value: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        value2: Option<serde_json::Value>,
    },
    #[allow(clippy::upper_case_acronyms)]
    AND(Vec<QueryNode>),
    #[allow(clippy::upper_case_acronyms)]
    OR(Vec<QueryNode>),
    #[allow(clippy::upper_case_acronyms)]
    NOT(Box<QueryNode>),
}

/// Custom serialization: AND/OR/NOT need to serialize as `{"AND": [...]}`
/// while Field nodes serialize flat.  The `#[serde(untagged)]` above handles
/// Field, but we need explicit keys for the logical operators.
impl QueryNode {
    fn to_json(&self) -> serde_json::Value {
        match self {
            QueryNode::Field {
                field,
                op,
                value,
                value2,
            } => {
                let mut map = serde_json::json!({
                    "field": field,
                    "op": op,
                    "value": value,
                });
                if let Some(v2) = value2 {
                    map["value2"] = v2.clone();
                }
                map
            }
            QueryNode::AND(children) => {
                serde_json::json!({
                    "AND": children.iter().map(|c| c.to_json()).collect::<Vec<_>>()
                })
            }
            QueryNode::OR(children) => {
                serde_json::json!({
                    "OR": children.iter().map(|c| c.to_json()).collect::<Vec<_>>()
                })
            }
            QueryNode::NOT(child) => {
                serde_json::json!({ "NOT": child.to_json() })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sort direction
// ---------------------------------------------------------------------------

/// Sort direction for query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    fn as_str(&self) -> &'static str {
        match self {
            SortDirection::Asc => "asc",
            SortDirection::Desc => "desc",
        }
    }
}

// ---------------------------------------------------------------------------
// Sort field
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SortField {
    field: String,
    direction: SortDirection,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// A single query result returned by the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    /// Path of the matching file.
    pub path: String,
    /// Relevance score (higher is better).
    #[serde(default)]
    pub score: f64,
    /// Names of the indexes / operations that matched.
    #[serde(default)]
    pub matched_by: Vec<String>,
}

/// Aggregation result returned by the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateResult {
    /// Per-group aggregation results.
    #[serde(default)]
    pub groups: Vec<serde_json::Value>,
    /// Total count (if `count` was requested without `group_by`).
    #[serde(default)]
    pub total_count: Option<u64>,
}

// ---------------------------------------------------------------------------
// QueryBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for constructing AeorDB queries.
///
/// # Example
///
/// ```rust,no_run
/// # use aeordb_plugin_sdk::query_builder::{QueryBuilder, SortDirection};
/// let results = QueryBuilder::new("/users")
///     .field("name").contains("Wyatt")
///     .field("age").gt_u64(21)
///     .sort("name", SortDirection::Asc)
///     .limit(10)
///     .execute();
/// ```
pub struct QueryBuilder {
    path: String,
    nodes: Vec<QueryNode>,
    limit_value: Option<usize>,
    offset_value: Option<usize>,
    sort_fields: Vec<SortField>,
}

impl QueryBuilder {
    /// Create a new query builder targeting the given path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            nodes: Vec::new(),
            limit_value: None,
            offset_value: None,
            sort_fields: Vec::new(),
        }
    }

    /// Start a field-level condition.  Call an operator method on the returned
    /// [`FieldQueryBuilder`] to complete the condition and return the
    /// `QueryBuilder` for further chaining.
    pub fn field(self, name: impl Into<String>) -> FieldQueryBuilder {
        FieldQueryBuilder {
            parent: self,
            field_name: name.into(),
        }
    }

    /// Add an AND group built via a closure.
    ///
    /// ```rust,no_run
    /// # use aeordb_plugin_sdk::query_builder::QueryBuilder;
    /// QueryBuilder::new("/users")
    ///     .and(|q| q.field("name").contains("Wyatt").field("active").eq_bool(true))
    ///     .limit(10);
    /// ```
    pub fn and<F>(mut self, build_fn: F) -> Self
    where
        F: FnOnce(QueryBuilder) -> QueryBuilder,
    {
        let inner = build_fn(QueryBuilder::new(""));
        if !inner.nodes.is_empty() {
            self.nodes.push(QueryNode::AND(inner.nodes));
        }
        self
    }

    /// Add an OR group built via a closure.
    pub fn or<F>(mut self, build_fn: F) -> Self
    where
        F: FnOnce(QueryBuilder) -> QueryBuilder,
    {
        let inner = build_fn(QueryBuilder::new(""));
        if !inner.nodes.is_empty() {
            self.nodes.push(QueryNode::OR(inner.nodes));
        }
        self
    }

    /// Negate a condition built via a closure.
    pub fn not<F>(mut self, build_fn: F) -> Self
    where
        F: FnOnce(QueryBuilder) -> QueryBuilder,
    {
        let inner = build_fn(QueryBuilder::new(""));
        if let Some(first_node) = inner.nodes.into_iter().next() {
            self.nodes.push(QueryNode::NOT(Box::new(first_node)));
        }
        self
    }

    /// Limit the number of results.
    pub fn limit(mut self, count: usize) -> Self {
        self.limit_value = Some(count);
        self
    }

    /// Skip the first `count` results.
    pub fn offset(mut self, count: usize) -> Self {
        self.offset_value = Some(count);
        self
    }

    /// Add a sort field with direction.
    pub fn sort(mut self, field: impl Into<String>, direction: SortDirection) -> Self {
        self.sort_fields.push(SortField {
            field: field.into(),
            direction,
        });
        self
    }

    /// Serialize the builder state to a JSON value matching the AeorDB query
    /// format.  This is public so tests and the host-side can inspect the
    /// output without calling `execute`.
    pub fn to_json(&self) -> serde_json::Value {
        let where_clause = self.build_where_clause();

        let mut query = serde_json::json!({
            "path": self.path,
            "where": where_clause,
        });

        if let Some(limit) = self.limit_value {
            query["limit"] = serde_json::json!(limit);
        }
        if let Some(offset) = self.offset_value {
            query["offset"] = serde_json::json!(offset);
        }
        if !self.sort_fields.is_empty() {
            let sort: Vec<serde_json::Value> = self
                .sort_fields
                .iter()
                .map(|sf| {
                    serde_json::json!({
                        "field": sf.field,
                        "direction": sf.direction.as_str(),
                    })
                })
                .collect();
            query["order_by"] = serde_json::json!(sort);
        }

        query
    }

    /// Execute the query by calling the host `aeordb_query` function.
    pub fn execute(self) -> Result<Vec<QueryResult>, PluginError> {
        let json = self.to_json();
        let response = crate::context::call_query(&json)?;

        // The host may return { "results": [...] } or a bare array.
        let results_value = response
            .get("results")
            .cloned()
            .unwrap_or(response);

        serde_json::from_value(results_value).map_err(|e| {
            PluginError::SerializationFailed(format!("failed to parse query results: {}", e))
        })
    }

    // -- Internal -----------------------------------------------------------

    fn build_where_clause(&self) -> serde_json::Value {
        match self.nodes.len() {
            0 => serde_json::json!({}),
            1 => self.nodes[0].to_json(),
            _ => {
                // Multiple top-level conditions are implicitly ANDed
                QueryNode::AND(self.nodes.clone()).to_json()
            }
        }
    }

    /// Internal: push a completed field node.
    fn push_node(mut self, node: QueryNode) -> Self {
        self.nodes.push(node);
        self
    }
}

impl std::fmt::Debug for QueryBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryBuilder")
            .field("path", &self.path)
            .field("node_count", &self.nodes.len())
            .field("limit", &self.limit_value)
            .field("sort_count", &self.sort_fields.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// FieldQueryBuilder
// ---------------------------------------------------------------------------

/// Intermediate builder for specifying the operation on a single field.
/// All operator methods consume `self` and return the parent [`QueryBuilder`].
pub struct FieldQueryBuilder {
    parent: QueryBuilder,
    field_name: String,
}

impl FieldQueryBuilder {
    // -- Equality -----------------------------------------------------------

    /// Exact match on raw bytes.
    pub fn eq(self, value: &[u8]) -> QueryBuilder {
        self.finish("eq", serde_json::json!(base64_encode(value)), None)
    }

    /// Exact match on a u64 value.
    pub fn eq_u64(self, value: u64) -> QueryBuilder {
        self.finish("eq", serde_json::json!(value), None)
    }

    /// Exact match on an i64 value.
    pub fn eq_i64(self, value: i64) -> QueryBuilder {
        self.finish("eq", serde_json::json!(value), None)
    }

    /// Exact match on an f64 value.
    pub fn eq_f64(self, value: f64) -> QueryBuilder {
        self.finish("eq", serde_json::json!(value), None)
    }

    /// Exact match on a string value.
    pub fn eq_str(self, value: &str) -> QueryBuilder {
        self.finish("eq", serde_json::json!(value), None)
    }

    /// Exact match on a boolean value.
    pub fn eq_bool(self, value: bool) -> QueryBuilder {
        self.finish("eq", serde_json::json!(value), None)
    }

    // -- Greater than -------------------------------------------------------

    /// Greater than comparison on raw bytes.
    pub fn gt(self, value: &[u8]) -> QueryBuilder {
        self.finish("gt", serde_json::json!(base64_encode(value)), None)
    }

    /// Greater than comparison on a u64.
    pub fn gt_u64(self, value: u64) -> QueryBuilder {
        self.finish("gt", serde_json::json!(value), None)
    }

    /// Greater than comparison on a string.
    pub fn gt_str(self, value: &str) -> QueryBuilder {
        self.finish("gt", serde_json::json!(value), None)
    }

    /// Greater than comparison on an f64.
    pub fn gt_f64(self, value: f64) -> QueryBuilder {
        self.finish("gt", serde_json::json!(value), None)
    }

    // -- Less than ----------------------------------------------------------

    /// Less than comparison on raw bytes.
    pub fn lt(self, value: &[u8]) -> QueryBuilder {
        self.finish("lt", serde_json::json!(base64_encode(value)), None)
    }

    /// Less than comparison on a u64.
    pub fn lt_u64(self, value: u64) -> QueryBuilder {
        self.finish("lt", serde_json::json!(value), None)
    }

    /// Less than comparison on a string.
    pub fn lt_str(self, value: &str) -> QueryBuilder {
        self.finish("lt", serde_json::json!(value), None)
    }

    /// Less than comparison on an f64.
    pub fn lt_f64(self, value: f64) -> QueryBuilder {
        self.finish("lt", serde_json::json!(value), None)
    }

    // -- Between ------------------------------------------------------------

    /// Between (inclusive) on raw bytes.
    pub fn between(self, min: &[u8], max: &[u8]) -> QueryBuilder {
        self.finish(
            "between",
            serde_json::json!(base64_encode(min)),
            Some(serde_json::json!(base64_encode(max))),
        )
    }

    /// Between (inclusive) on u64 values.
    pub fn between_u64(self, min: u64, max: u64) -> QueryBuilder {
        self.finish(
            "between",
            serde_json::json!(min),
            Some(serde_json::json!(max)),
        )
    }

    /// Between (inclusive) on string values.
    pub fn between_str(self, min: &str, max: &str) -> QueryBuilder {
        self.finish(
            "between",
            serde_json::json!(min),
            Some(serde_json::json!(max)),
        )
    }

    // -- In -----------------------------------------------------------------

    /// Match any of the given raw byte values.
    pub fn in_values(self, values: &[&[u8]]) -> QueryBuilder {
        let encoded: Vec<String> = values.iter().map(|v| base64_encode(v)).collect();
        self.finish("in", serde_json::json!(encoded), None)
    }

    /// Match any of the given u64 values.
    pub fn in_u64(self, values: &[u64]) -> QueryBuilder {
        self.finish("in", serde_json::json!(values), None)
    }

    /// Match any of the given string values.
    pub fn in_str(self, values: &[&str]) -> QueryBuilder {
        self.finish("in", serde_json::json!(values), None)
    }

    // -- Text search --------------------------------------------------------

    /// Substring / trigram contains search.
    pub fn contains(self, text: &str) -> QueryBuilder {
        self.finish("contains", serde_json::json!(text), None)
    }

    /// Similarity search (trigram) with a threshold (0.0–1.0).
    pub fn similar(self, text: &str, threshold: f64) -> QueryBuilder {
        self.finish(
            "similar",
            serde_json::json!({ "text": text, "threshold": threshold }),
            None,
        )
    }

    /// Phonetic (Soundex/Metaphone) search.
    pub fn phonetic(self, text: &str) -> QueryBuilder {
        self.finish("phonetic", serde_json::json!(text), None)
    }

    /// Fuzzy / Levenshtein distance search.
    pub fn fuzzy(self, text: &str) -> QueryBuilder {
        self.finish("fuzzy", serde_json::json!(text), None)
    }

    /// Full-text match query.
    pub fn match_query(self, text: &str) -> QueryBuilder {
        self.finish("match", serde_json::json!(text), None)
    }

    // -- Internal -----------------------------------------------------------

    fn finish(
        self,
        op: &str,
        value: serde_json::Value,
        value2: Option<serde_json::Value>,
    ) -> QueryBuilder {
        self.parent.push_node(QueryNode::Field {
            field: self.field_name,
            op: op.to_string(),
            value,
            value2,
        })
    }
}

// ---------------------------------------------------------------------------
// AggregateBuilder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum AggregateOperation {
    Count,
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

/// Fluent builder for constructing AeorDB aggregation queries.
///
/// # Example
///
/// ```rust,no_run
/// # use aeordb_plugin_sdk::query_builder::AggregateBuilder;
/// let result = AggregateBuilder::new("/orders")
///     .count()
///     .sum("total")
///     .avg("total")
///     .group_by("status")
///     .execute();
/// ```
pub struct AggregateBuilder {
    path: String,
    operations: Vec<AggregateOperation>,
    group_by_fields: Vec<String>,
    where_nodes: Vec<QueryNode>,
    limit_value: Option<usize>,
}

impl AggregateBuilder {
    /// Create a new aggregation builder targeting the given path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            operations: Vec::new(),
            group_by_fields: Vec::new(),
            where_nodes: Vec::new(),
            limit_value: None,
        }
    }

    /// Request a count aggregation.
    pub fn count(mut self) -> Self {
        self.operations.push(AggregateOperation::Count);
        self
    }

    /// Request a sum aggregation on the given field.
    pub fn sum(mut self, field: impl Into<String>) -> Self {
        self.operations.push(AggregateOperation::Sum(field.into()));
        self
    }

    /// Request an average aggregation on the given field.
    pub fn avg(mut self, field: impl Into<String>) -> Self {
        self.operations.push(AggregateOperation::Avg(field.into()));
        self
    }

    /// Request a minimum value aggregation on the given field.
    pub fn min_val(mut self, field: impl Into<String>) -> Self {
        self.operations.push(AggregateOperation::Min(field.into()));
        self
    }

    /// Request a maximum value aggregation on the given field.
    pub fn max_val(mut self, field: impl Into<String>) -> Self {
        self.operations.push(AggregateOperation::Max(field.into()));
        self
    }

    /// Group results by the given field.
    pub fn group_by(mut self, field: impl Into<String>) -> Self {
        self.group_by_fields.push(field.into());
        self
    }

    /// Limit the number of groups returned.
    pub fn limit(mut self, count: usize) -> Self {
        self.limit_value = Some(count);
        self
    }

    /// Add a where condition via a closure that builds a QueryBuilder.
    pub fn filter<F>(mut self, build_fn: F) -> Self
    where
        F: FnOnce(QueryBuilder) -> QueryBuilder,
    {
        let inner = build_fn(QueryBuilder::new(""));
        self.where_nodes = inner.nodes;
        self
    }

    /// Serialize the builder state to JSON.
    pub fn to_json(&self) -> serde_json::Value {
        let mut has_count = false;
        let mut sum_fields: Vec<&str> = Vec::new();
        let mut avg_fields: Vec<&str> = Vec::new();
        let mut min_fields: Vec<&str> = Vec::new();
        let mut max_fields: Vec<&str> = Vec::new();

        for operation in &self.operations {
            match operation {
                AggregateOperation::Count => has_count = true,
                AggregateOperation::Sum(field) => sum_fields.push(field),
                AggregateOperation::Avg(field) => avg_fields.push(field),
                AggregateOperation::Min(field) => min_fields.push(field),
                AggregateOperation::Max(field) => max_fields.push(field),
            }
        }

        let aggregate = serde_json::json!({
            "count": has_count,
            "sum": sum_fields,
            "avg": avg_fields,
            "min": min_fields,
            "max": max_fields,
            "group_by": self.group_by_fields,
        });

        let where_clause = match self.where_nodes.len() {
            0 => serde_json::json!({}),
            1 => self.where_nodes[0].to_json(),
            _ => QueryNode::AND(self.where_nodes.clone()).to_json(),
        };

        let mut query = serde_json::json!({
            "path": self.path,
            "where": where_clause,
            "aggregate": aggregate,
        });

        if let Some(limit) = self.limit_value {
            query["limit"] = serde_json::json!(limit);
        }

        query
    }

    /// Execute the aggregation by calling the host `aeordb_aggregate` function.
    pub fn execute(self) -> Result<AggregateResult, PluginError> {
        let json = self.to_json();
        let response = crate::context::call_aggregate(&json)?;
        serde_json::from_value(response).map_err(|e| {
            PluginError::SerializationFailed(format!("failed to parse aggregate result: {}", e))
        })
    }
}

impl std::fmt::Debug for AggregateBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateBuilder")
            .field("path", &self.path)
            .field("operation_count", &self.operations.len())
            .field("group_by", &self.group_by_fields)
            .field("limit", &self.limit_value)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- QueryBuilder serialization -----------------------------------------

    #[test]
    fn test_empty_query() {
        let builder = QueryBuilder::new("/users");
        let json = builder.to_json();
        assert_eq!(json["path"], "/users");
        assert_eq!(json["where"], serde_json::json!({}));
        assert!(json.get("limit").is_none());
        assert!(json.get("order_by").is_none());
    }

    #[test]
    fn test_single_eq_str() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .eq_str("Wyatt")
            .to_json();
        assert_eq!(json["path"], "/users");
        assert_eq!(json["where"]["field"], "name");
        assert_eq!(json["where"]["op"], "eq");
        assert_eq!(json["where"]["value"], "Wyatt");
    }

    #[test]
    fn test_single_eq_u64() {
        let json = QueryBuilder::new("/users")
            .field("age")
            .eq_u64(30)
            .to_json();
        assert_eq!(json["where"]["field"], "age");
        assert_eq!(json["where"]["op"], "eq");
        assert_eq!(json["where"]["value"], 30);
    }

    #[test]
    fn test_single_eq_i64() {
        let json = QueryBuilder::new("/data")
            .field("offset")
            .eq_i64(-42)
            .to_json();
        assert_eq!(json["where"]["value"], -42);
    }

    #[test]
    fn test_single_eq_f64() {
        // 3.14 picked as a representative non-integer; not used as PI.
        let value = 3.14_f64;
        let json = QueryBuilder::new("/data")
            .field("score")
            .eq_f64(value)
            .to_json();
        assert_eq!(json["where"]["value"], value);
    }

    #[test]
    fn test_single_eq_bool() {
        let json = QueryBuilder::new("/users")
            .field("active")
            .eq_bool(true)
            .to_json();
        assert_eq!(json["where"]["value"], true);
    }

    #[test]
    fn test_gt_u64() {
        let json = QueryBuilder::new("/users")
            .field("age")
            .gt_u64(21)
            .to_json();
        assert_eq!(json["where"]["op"], "gt");
        assert_eq!(json["where"]["value"], 21);
    }

    #[test]
    fn test_gt_str() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .gt_str("M")
            .to_json();
        assert_eq!(json["where"]["op"], "gt");
        assert_eq!(json["where"]["value"], "M");
    }

    #[test]
    fn test_gt_f64() {
        let json = QueryBuilder::new("/data")
            .field("temperature")
            .gt_f64(98.6)
            .to_json();
        assert_eq!(json["where"]["op"], "gt");
        assert_eq!(json["where"]["value"], 98.6);
    }

    #[test]
    fn test_lt_u64() {
        let json = QueryBuilder::new("/users")
            .field("age")
            .lt_u64(65)
            .to_json();
        assert_eq!(json["where"]["op"], "lt");
        assert_eq!(json["where"]["value"], 65);
    }

    #[test]
    fn test_lt_str() {
        let json = QueryBuilder::new("/data")
            .field("label")
            .lt_str("Z")
            .to_json();
        assert_eq!(json["where"]["op"], "lt");
        assert_eq!(json["where"]["value"], "Z");
    }

    #[test]
    fn test_lt_f64() {
        let json = QueryBuilder::new("/data")
            .field("weight")
            .lt_f64(100.5)
            .to_json();
        assert_eq!(json["where"]["op"], "lt");
        assert_eq!(json["where"]["value"], 100.5);
    }

    #[test]
    fn test_between_u64() {
        let json = QueryBuilder::new("/users")
            .field("age")
            .between_u64(18, 65)
            .to_json();
        assert_eq!(json["where"]["op"], "between");
        assert_eq!(json["where"]["value"], 18);
        assert_eq!(json["where"]["value2"], 65);
    }

    #[test]
    fn test_between_str() {
        let json = QueryBuilder::new("/data")
            .field("name")
            .between_str("A", "M")
            .to_json();
        assert_eq!(json["where"]["op"], "between");
        assert_eq!(json["where"]["value"], "A");
        assert_eq!(json["where"]["value2"], "M");
    }

    #[test]
    fn test_in_u64() {
        let json = QueryBuilder::new("/users")
            .field("role_id")
            .in_u64(&[1, 2, 3])
            .to_json();
        assert_eq!(json["where"]["op"], "in");
        assert_eq!(json["where"]["value"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn test_in_str() {
        let json = QueryBuilder::new("/users")
            .field("status")
            .in_str(&["active", "pending"])
            .to_json();
        assert_eq!(json["where"]["op"], "in");
        assert_eq!(
            json["where"]["value"],
            serde_json::json!(["active", "pending"])
        );
    }

    #[test]
    fn test_contains() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .contains("Wyatt")
            .to_json();
        assert_eq!(json["where"]["op"], "contains");
        assert_eq!(json["where"]["value"], "Wyatt");
    }

    #[test]
    fn test_similar() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .similar("Wyat", 0.8)
            .to_json();
        assert_eq!(json["where"]["op"], "similar");
        assert_eq!(json["where"]["value"]["text"], "Wyat");
        assert_eq!(json["where"]["value"]["threshold"], 0.8);
    }

    #[test]
    fn test_phonetic() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .phonetic("Smith")
            .to_json();
        assert_eq!(json["where"]["op"], "phonetic");
        assert_eq!(json["where"]["value"], "Smith");
    }

    #[test]
    fn test_fuzzy() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .fuzzy("Wyat")
            .to_json();
        assert_eq!(json["where"]["op"], "fuzzy");
        assert_eq!(json["where"]["value"], "Wyat");
    }

    #[test]
    fn test_match_query() {
        let json = QueryBuilder::new("/docs")
            .field("body")
            .match_query("rust programming")
            .to_json();
        assert_eq!(json["where"]["op"], "match");
        assert_eq!(json["where"]["value"], "rust programming");
    }

    #[test]
    fn test_eq_raw_bytes() {
        let json = QueryBuilder::new("/data")
            .field("hash")
            .eq(&[0xDE, 0xAD, 0xBE, 0xEF])
            .to_json();
        assert_eq!(json["where"]["op"], "eq");
        // Should be base64-encoded
        let value = json["where"]["value"].as_str().unwrap();
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value)
            .unwrap();
        assert_eq!(decoded, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    // -- Multiple conditions (implicit AND) ---------------------------------

    #[test]
    fn test_multiple_conditions_implicit_and() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .contains("Wyatt")
            .field("age")
            .gt_u64(21)
            .to_json();

        // Two top-level conditions => wrapped in AND
        let where_clause = &json["where"];
        assert!(where_clause.get("AND").is_some());
        let and_children = where_clause["AND"].as_array().unwrap();
        assert_eq!(and_children.len(), 2);
        assert_eq!(and_children[0]["field"], "name");
        assert_eq!(and_children[1]["field"], "age");
    }

    // -- Explicit AND / OR / NOT --------------------------------------------

    #[test]
    fn test_explicit_and() {
        let json = QueryBuilder::new("/users")
            .and(|q| {
                q.field("name")
                    .contains("Wyatt")
                    .field("age")
                    .gt_u64(21)
            })
            .to_json();

        let where_clause = &json["where"];
        assert!(where_clause.get("AND").is_some());
        let children = where_clause["AND"].as_array().unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn test_explicit_or() {
        let json = QueryBuilder::new("/users")
            .or(|q| {
                q.field("role")
                    .eq_str("admin")
                    .field("role")
                    .eq_str("superadmin")
            })
            .to_json();

        let where_clause = &json["where"];
        assert!(where_clause.get("OR").is_some());
        let children = where_clause["OR"].as_array().unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn test_not() {
        let json = QueryBuilder::new("/users")
            .not(|q| q.field("banned").eq_bool(true))
            .to_json();

        let where_clause = &json["where"];
        assert!(where_clause.get("NOT").is_some());
        assert_eq!(where_clause["NOT"]["field"], "banned");
        assert_eq!(where_clause["NOT"]["value"], true);
    }

    #[test]
    fn test_nested_boolean_logic() {
        let json = QueryBuilder::new("/users")
            .and(|q| {
                q.field("active")
                    .eq_bool(true)
                    .or(|q2| {
                        q2.field("role")
                            .eq_str("admin")
                            .field("role")
                            .eq_str("moderator")
                    })
            })
            .to_json();

        let and_children = json["where"]["AND"].as_array().unwrap();
        assert_eq!(and_children.len(), 2);
        assert_eq!(and_children[0]["field"], "active");
        assert!(and_children[1].get("OR").is_some());
    }

    // -- Limit, offset, sort ------------------------------------------------

    #[test]
    fn test_limit() {
        let json = QueryBuilder::new("/users")
            .field("active")
            .eq_bool(true)
            .limit(25)
            .to_json();
        assert_eq!(json["limit"], 25);
    }

    #[test]
    fn test_offset() {
        let json = QueryBuilder::new("/users")
            .field("active")
            .eq_bool(true)
            .offset(50)
            .to_json();
        assert_eq!(json["offset"], 50);
    }

    #[test]
    fn test_sort_single() {
        let json = QueryBuilder::new("/users")
            .sort("name", SortDirection::Asc)
            .to_json();
        let sort = json["order_by"].as_array().unwrap();
        assert_eq!(sort.len(), 1);
        assert_eq!(sort[0]["field"], "name");
        assert_eq!(sort[0]["direction"], "asc");
    }

    #[test]
    fn test_sort_multiple() {
        let json = QueryBuilder::new("/users")
            .sort("last_name", SortDirection::Asc)
            .sort("first_name", SortDirection::Desc)
            .to_json();
        let sort = json["order_by"].as_array().unwrap();
        assert_eq!(sort.len(), 2);
        assert_eq!(sort[0]["field"], "last_name");
        assert_eq!(sort[0]["direction"], "asc");
        assert_eq!(sort[1]["field"], "first_name");
        assert_eq!(sort[1]["direction"], "desc");
    }

    // -- Full fluent chain --------------------------------------------------

    #[test]
    fn test_full_fluent_chain() {
        let json = QueryBuilder::new("/users")
            .field("name")
            .contains("Wyatt")
            .field("age")
            .between_u64(18, 65)
            .sort("name", SortDirection::Asc)
            .limit(10)
            .offset(0)
            .to_json();

        assert_eq!(json["path"], "/users");
        assert_eq!(json["limit"], 10);
        assert_eq!(json["offset"], 0);
        assert!(json["order_by"].as_array().is_some());

        let where_clause = &json["where"];
        let and_children = where_clause["AND"].as_array().unwrap();
        assert_eq!(and_children.len(), 2);
        assert_eq!(and_children[0]["op"], "contains");
        assert_eq!(and_children[1]["op"], "between");
        assert_eq!(and_children[1]["value2"], 65);
    }

    // -- Execute on native target -------------------------------------------

    #[test]
    fn test_execute_native_error() {
        let result = QueryBuilder::new("/users")
            .field("name")
            .eq_str("test")
            .execute();
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(
                    message.contains("WASM context"),
                    "unexpected error: {}",
                    message
                );
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    // -- AggregateBuilder ---------------------------------------------------

    #[test]
    fn test_aggregate_count() {
        let json = AggregateBuilder::new("/users").count().to_json();
        assert_eq!(json["path"], "/users");
        assert_eq!(json["aggregate"]["count"], true);
    }

    #[test]
    fn test_aggregate_sum() {
        let json = AggregateBuilder::new("/orders")
            .sum("total")
            .to_json();
        let sum = json["aggregate"]["sum"].as_array().unwrap();
        assert_eq!(sum, &[serde_json::json!("total")]);
    }

    #[test]
    fn test_aggregate_avg() {
        let json = AggregateBuilder::new("/orders")
            .avg("price")
            .to_json();
        let avg = json["aggregate"]["avg"].as_array().unwrap();
        assert_eq!(avg, &[serde_json::json!("price")]);
    }

    #[test]
    fn test_aggregate_min_max() {
        let json = AggregateBuilder::new("/data")
            .min_val("temperature")
            .max_val("temperature")
            .to_json();
        let min = json["aggregate"]["min"].as_array().unwrap();
        let max = json["aggregate"]["max"].as_array().unwrap();
        assert_eq!(min, &[serde_json::json!("temperature")]);
        assert_eq!(max, &[serde_json::json!("temperature")]);
    }

    #[test]
    fn test_aggregate_group_by() {
        let json = AggregateBuilder::new("/orders")
            .count()
            .sum("total")
            .group_by("status")
            .to_json();
        assert_eq!(json["aggregate"]["count"], true);
        let group_by = json["aggregate"]["group_by"].as_array().unwrap();
        assert_eq!(group_by, &[serde_json::json!("status")]);
    }

    #[test]
    fn test_aggregate_limit() {
        let json = AggregateBuilder::new("/orders")
            .count()
            .group_by("category")
            .limit(5)
            .to_json();
        assert_eq!(json["limit"], 5);
    }

    #[test]
    fn test_aggregate_with_filter() {
        let json = AggregateBuilder::new("/orders")
            .count()
            .filter(|q| q.field("status").eq_str("completed"))
            .to_json();
        assert_eq!(json["where"]["field"], "status");
        assert_eq!(json["where"]["value"], "completed");
    }

    #[test]
    fn test_aggregate_full_chain() {
        let json = AggregateBuilder::new("/orders")
            .count()
            .sum("total")
            .avg("total")
            .min_val("total")
            .max_val("total")
            .group_by("status")
            .group_by("region")
            .filter(|q| q.field("year").eq_u64(2026))
            .limit(10)
            .to_json();

        assert_eq!(json["path"], "/orders");
        assert_eq!(json["limit"], 10);
        assert_eq!(json["aggregate"]["count"], true);

        let sum = json["aggregate"]["sum"].as_array().unwrap();
        assert_eq!(sum.len(), 1);

        let group_by = json["aggregate"]["group_by"].as_array().unwrap();
        assert_eq!(group_by.len(), 2);

        assert_eq!(json["where"]["field"], "year");
    }

    #[test]
    fn test_aggregate_execute_native_error() {
        let result = AggregateBuilder::new("/users").count().execute();
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(message.contains("WASM context"));
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_aggregate_no_operations() {
        let json = AggregateBuilder::new("/data").to_json();
        assert_eq!(json["aggregate"]["count"], false);
        assert!(json["aggregate"]["sum"].as_array().unwrap().is_empty());
        assert!(json["aggregate"]["avg"].as_array().unwrap().is_empty());
    }

    // -- QueryResult deserialization ----------------------------------------

    #[test]
    fn test_query_result_deserialization() {
        let json = serde_json::json!({
            "path": "/users/1.json",
            "score": 0.95,
            "matched_by": ["name_idx", "trigram"]
        });
        let result: QueryResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.path, "/users/1.json");
        assert!((result.score - 0.95).abs() < f64::EPSILON);
        assert_eq!(result.matched_by, vec!["name_idx", "trigram"]);
    }

    #[test]
    fn test_query_result_defaults() {
        let json = serde_json::json!({
            "path": "/users/1.json"
        });
        let result: QueryResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.score, 0.0);
        assert!(result.matched_by.is_empty());
    }

    #[test]
    fn test_aggregate_result_deserialization() {
        let json = serde_json::json!({
            "groups": [
                {"status": "active", "count": 42},
                {"status": "inactive", "count": 8}
            ],
            "total_count": 50
        });
        let result: AggregateResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.total_count, Some(50));
    }

    #[test]
    fn test_aggregate_result_defaults() {
        let json = serde_json::json!({});
        let result: AggregateResult = serde_json::from_value(json).unwrap();
        assert!(result.groups.is_empty());
        assert!(result.total_count.is_none());
    }

    // -- Debug impls --------------------------------------------------------

    #[test]
    fn test_query_builder_debug() {
        let builder = QueryBuilder::new("/users")
            .field("name")
            .eq_str("test")
            .limit(10);
        let debug = format!("{:?}", builder);
        assert!(debug.contains("QueryBuilder"));
        assert!(debug.contains("/users"));
    }

    #[test]
    fn test_aggregate_builder_debug() {
        let builder = AggregateBuilder::new("/orders").count().group_by("status");
        let debug = format!("{:?}", builder);
        assert!(debug.contains("AggregateBuilder"));
        assert!(debug.contains("status"));
    }

    // -- SortDirection ------------------------------------------------------

    #[test]
    fn test_sort_direction_as_str() {
        assert_eq!(SortDirection::Asc.as_str(), "asc");
        assert_eq!(SortDirection::Desc.as_str(), "desc");
    }

    // -- Edge cases ---------------------------------------------------------

    #[test]
    fn test_empty_and_closure() {
        // An AND with no conditions should not add a node
        let json = QueryBuilder::new("/users")
            .and(|q| q)
            .field("name")
            .eq_str("test")
            .to_json();
        // Should just have the single field condition, no AND wrapper
        assert_eq!(json["where"]["field"], "name");
    }

    #[test]
    fn test_empty_or_closure() {
        let json = QueryBuilder::new("/users")
            .or(|q| q)
            .field("name")
            .eq_str("test")
            .to_json();
        assert_eq!(json["where"]["field"], "name");
    }

    #[test]
    fn test_not_with_empty_closure() {
        // NOT with no conditions should not add a node
        let json = QueryBuilder::new("/users")
            .not(|q| q)
            .to_json();
        assert_eq!(json["where"], serde_json::json!({}));
    }

    #[test]
    fn test_in_empty_values() {
        let json = QueryBuilder::new("/data")
            .field("tags")
            .in_str(&[])
            .to_json();
        assert_eq!(json["where"]["op"], "in");
        assert_eq!(json["where"]["value"], serde_json::json!([]));
    }

    #[test]
    fn test_between_raw_bytes() {
        let json = QueryBuilder::new("/data")
            .field("key")
            .between(&[0x00], &[0xFF])
            .to_json();
        assert_eq!(json["where"]["op"], "between");
        assert!(json["where"]["value"].is_string());
        assert!(json["where"]["value2"].is_string());
    }

    #[test]
    fn test_gt_raw_bytes() {
        let json = QueryBuilder::new("/data")
            .field("key")
            .gt(&[0xAB])
            .to_json();
        assert_eq!(json["where"]["op"], "gt");
    }

    #[test]
    fn test_lt_raw_bytes() {
        let json = QueryBuilder::new("/data")
            .field("key")
            .lt(&[0xCD])
            .to_json();
        assert_eq!(json["where"]["op"], "lt");
    }

    #[test]
    fn test_in_raw_bytes() {
        let json = QueryBuilder::new("/data")
            .field("hash")
            .in_values(&[&[0x01], &[0x02]])
            .to_json();
        assert_eq!(json["where"]["op"], "in");
        let values = json["where"]["value"].as_array().unwrap();
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn test_path_with_special_characters() {
        let json = QueryBuilder::new("/data/my files/2026")
            .field("name")
            .eq_str("test")
            .to_json();
        assert_eq!(json["path"], "/data/my files/2026");
    }
}
