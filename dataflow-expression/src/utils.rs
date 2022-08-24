use nom_sql::SqlType;
use readyset_data::{DfType, DfValue};

use crate::{BuiltinFunction, Expr};

/** These helpers initialize `Expr` variants with a type field. These are
 * not inteded for use outside of tests. A planned implementation of the type
 * inference system will make the type paramter of `Expr` generic, which
 * will allow variants to be constructed without any type information - leaving
 * that to the type inference system. These functions will then be removed */

/// Helper to create `Expr::Column`. Type is unknown by default. The correct type may need to
/// be populated when type is checked at runtime
///
/// Not intended for use outside of tests
pub fn make_column(index: usize) -> Expr {
    column_with_type(index, DfType::Unknown)
}

/// Create `Expr::Column` with type set to Int
///
/// Not intended for use outside of tests
pub fn make_int_column(index: usize) -> Expr {
    column_with_type(index, DfType::Sql(SqlType::Int(None)))
}

/// Create `Expr::Column` with `DfType` ty.
pub fn column_with_type(index: usize, ty: DfType) -> Expr {
    Expr::Column { index, ty }
}

/// Create `Expr::Literal` from `DfValue`. Type is inferred from `DfValue`.
///
/// Not intended for use outside of tests
pub fn make_literal(val: DfValue) -> Expr {
    Expr::Literal {
        val: val.clone(),
        ty: val.sql_type().into(),
    }
}

/// Create `Expr::Call` from `BuiltinFunction`. Type is `Unknown`.
///
/// Not intended for use outside of tests
pub fn make_call(func: BuiltinFunction) -> Expr {
    Expr::Call {
        func: Box::new(func),
        ty: DfType::Unknown,
    }
}

/// Returns the type of data stored in a JSON value as a string.
pub fn get_json_value_type(json: &serde_json::Value) -> &'static str {
    match json {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}