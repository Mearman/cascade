//! Expression AST types.

use serde::{Deserialize, Serialize};

/// A parsed expression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    Comparison {
        left: Operand,
        operator: Operator,
        right: Operand,
    },
    Literal(Value),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Matches,
    Contains,
    In,
}

/// An operand in a comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Operand {
    Identifier(String),
    Literal(Value),
}

/// A literal value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Integer(i64),
    Boolean(bool),
    String(String),
    Duration(i64, DurationUnit),
    Size(i64, SizeUnit),
    Percentage(i64),
}

/// Duration units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DurationUnit {
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

/// Size units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeUnit {
    Bytes,
    Kilobytes,
    Megabytes,
    Gigabytes,
    Terabytes,
}

impl Value {
    /// Convert to seconds (for duration values).
    pub fn to_seconds(&self) -> Option<i64> {
        match self {
            Value::Duration(v, unit) => Some(match unit {
                DurationUnit::Milliseconds => v / 1000,
                DurationUnit::Seconds => *v,
                DurationUnit::Minutes => v * 60,
                DurationUnit::Hours => v * 3600,
                DurationUnit::Days => v * 86400,
                DurationUnit::Weeks => v * 604_800,
                DurationUnit::Months => v * 2_592_000, // 30 days
                DurationUnit::Years => v * 31_536_000,
            }),
            _ => None,
        }
    }

    /// Convert to bytes (for size values).
    pub fn to_bytes(&self) -> Option<i64> {
        match self {
            Value::Size(v, unit) => Some(match unit {
                SizeUnit::Bytes => *v,
                SizeUnit::Kilobytes => v * 1024,
                SizeUnit::Megabytes => v * 1024 * 1024,
                SizeUnit::Gigabytes => v * 1024 * 1024 * 1024,
                SizeUnit::Terabytes => v * 1024 * 1024 * 1024 * 1024,
            }),
            _ => None,
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Integer(v) => write!(f, "{v}"),
            Value::Boolean(v) => write!(f, "{v}"),
            Value::String(v) => write!(f, "\"{v}\""),
            Value::Duration(v, unit) => {
                let s = match unit {
                    DurationUnit::Milliseconds => "ms",
                    DurationUnit::Seconds => "s",
                    DurationUnit::Minutes => "m",
                    DurationUnit::Hours => "h",
                    DurationUnit::Days => "d",
                    DurationUnit::Weeks => "w",
                    DurationUnit::Months => "M",
                    DurationUnit::Years => "y",
                };
                write!(f, "{v}{s}")
            }
            Value::Size(v, unit) => {
                let s = match unit {
                    SizeUnit::Bytes => "B",
                    SizeUnit::Kilobytes => "KB",
                    SizeUnit::Megabytes => "MB",
                    SizeUnit::Gigabytes => "GB",
                    SizeUnit::Terabytes => "TB",
                };
                write!(f, "{v}{s}")
            }
            Value::Percentage(v) => write!(f, "{v}%"),
        }
    }
}
