//! Expression evaluator — evaluates an AST against an [`EvalContext`].

use crate::ExprParser;
use crate::ast::{Expr, Operand, Operator, Value};
use crate::context::EvalContext;

use pest::Parser;

/// Parse an expression string into an AST.
///
/// # Errors
///
/// Returns an error if the input is not a valid expression.
pub fn parse_expr(input: &str) -> anyhow::Result<Expr> {
    let pairs = ExprParser::parse(crate::Rule::expression, input)?;
    build_ast(pairs)
}

/// Evaluate an expression against a context.
#[must_use]
pub fn evaluate(expr: &Expr, ctx: &EvalContext) -> bool {
    match expr {
        Expr::Or(exprs) => exprs.iter().any(|e| evaluate(e, ctx)),
        Expr::And(exprs) => exprs.iter().all(|e| evaluate(e, ctx)),
        Expr::Not(inner) => !evaluate(inner, ctx),
        Expr::Comparison {
            left,
            operator,
            right,
        } => {
            let lv = resolve_operand(left, ctx);
            let rv = resolve_operand(right, ctx);
            compare(&lv, *operator, &rv)
        }
        Expr::Literal(Value::Boolean(b)) => *b,
        Expr::Literal(_) => false,
    }
}

/// Resolve an operand to a value using the context.
fn resolve_operand(operand: &Operand, ctx: &EvalContext) -> Value {
    match operand {
        Operand::Literal(v) => v.clone(),
        Operand::Identifier(id) => resolve_identifier(id, ctx),
    }
}

/// Resolve a dotted identifier to a value from the context.
fn resolve_identifier(id: &str, ctx: &EvalContext) -> Value {
    match id {
        // File context
        "FILE.size" => Value::Integer(
            i64::try_from(ctx.file.size).unwrap_or(i64::MAX),
        ),
        "FILE.mime" => Value::String(ctx.file.mime.clone()),
        "FILE.ext" => Value::String(ctx.file.ext.clone()),
        "FILE.name" => Value::String(ctx.file.name.clone()),
        "FILE.year" => Value::Integer(i64::from(ctx.file.year())),
        "FILE.shared" => Value::Boolean(ctx.file.shared()),
        "FILE.starred" => Value::Boolean(ctx.file.starred()),
        "FILE.dirty" => Value::Boolean(ctx.file.dirty()),
        "FILE.cached" => Value::Boolean(ctx.file.cached()),
        "FILE.pinned" => Value::Boolean(ctx.file.pinned()),

        // Device context
        "DEVICE.id" => Value::String(ctx.device.id.clone()),
        "DEVICE.name" => Value::String(ctx.device.name.clone()),
        "DEVICE.arch" => Value::String(ctx.device.arch.clone()),
        "DEVICE.os" => Value::String(ctx.device.os.clone()),

        // Disk context
        "DISK.free" => Value::Integer(
            i64::try_from(ctx.disk.free_bytes).unwrap_or(i64::MAX),
        ),
        "DISK.used" => Value::Integer(
            i64::try_from(ctx.disk.used_bytes()).unwrap_or(i64::MAX),
        ),

        // Network context
        "NETWORK.type" => Value::String(ctx.network.if_type.to_string()),
        "NETWORK.metered" => Value::Boolean(ctx.network.metered),
        "NETWORK.bandwidth" => Value::Integer(
            i64::try_from(ctx.network.bandwidth_bps.unwrap_or(0)).unwrap_or(i64::MAX),
        ),

        // Power context
        "POWER.source" => Value::String(ctx.power.source.to_string()),
        "POWER.battery" => Value::Integer(
            ctx.power.battery_pct.map_or(0i64, i64::from),
        ),

        // Time context
        "TIME.hour" => Value::Integer(i64::from(ctx.time.hour())),
        "TIME.day" => {
            use chrono::Datelike;
            Value::Integer(i64::from(ctx.time.now.weekday().number_from_monday()))
        }

        // Peer context
        "PEER.online" | "PEER.count" => Value::Integer(
            i64::try_from(ctx.peer.online_count).unwrap_or(i64::MAX),
        ),
        "PEER.has_file" => Value::Integer(
            i64::try_from(ctx.peer.peers_with_file).unwrap_or(i64::MAX),
        ),

        _ => Value::Integer(0),
    }
}

/// Compare two values with the given operator.
fn compare(left: &Value, op: Operator, right: &Value) -> bool {
    match op {
        Operator::Eq => value_eq(left, right),
        Operator::Ne => !value_eq(left, right),
        Operator::Lt => value_ord(left, right).is_some_and(std::cmp::Ordering::is_lt),
        Operator::Le => value_ord(left, right).is_some_and(std::cmp::Ordering::is_le),
        Operator::Gt => value_ord(left, right).is_some_and(std::cmp::Ordering::is_gt),
        Operator::Ge => value_ord(left, right).is_some_and(std::cmp::Ordering::is_ge),
        Operator::Matches => string_matches(left, right),
        Operator::Contains => string_contains(left, right),
        Operator::In => value_in(left, right),
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Integer(x), Value::Integer(y)) | (Value::Percentage(x), Value::Percentage(y)) => {
            x == y
        }
        (Value::Boolean(x), Value::Boolean(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        // Cross-type: compare size/duration by converting to common unit
        (Value::Size(_, _), _) | (_, Value::Size(_, _)) => {
            a.to_bytes() == b.to_bytes() && a.to_bytes().is_some()
        }
        (Value::Duration(_, _), _) | (_, Value::Duration(_, _)) => {
            a.to_seconds() == b.to_seconds() && a.to_seconds().is_some()
        }
        _ => false,
    }
}

use std::cmp::Ordering;

fn value_ord(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Integer(x), Value::Integer(y)) => Some(x.cmp(y)),
        (Value::Size(_, _), Value::Size(_, _)) => {
            let ab = a.to_bytes()?;
            let bb = b.to_bytes()?;
            Some(ab.cmp(&bb))
        }
        (Value::Duration(_, _), Value::Duration(_, _)) => {
            let as_ = a.to_seconds()?;
            let bs = b.to_seconds()?;
            Some(as_.cmp(&bs))
        }
        // Cross-type: try to compare integer with size/duration
        (Value::Integer(x), Value::Size(_, _)) => {
            let bb = b.to_bytes()?;
            Some((*x).cmp(&bb))
        }
        (Value::Size(_, _), Value::Integer(y)) => {
            let ab = a.to_bytes()?;
            Some(ab.cmp(y))
        }
        (Value::Integer(x), Value::Duration(_, _)) => {
            let bs = b.to_seconds()?;
            Some((*x).cmp(&bs))
        }
        (Value::Duration(_, _), Value::Integer(y)) => {
            let as_ = a.to_seconds()?;
            Some(as_.cmp(y))
        }
        _ => None,
    }
}

fn string_matches(haystack: &Value, pattern: &Value) -> bool {
    match (haystack, pattern) {
        (Value::String(s), Value::String(p)) => {
            // Simple glob matching
            if p.contains('*') {
                let parts: Vec<&str> = p.split('*').collect();
                let mut idx = 0usize;
                for (i, part) in parts.iter().enumerate() {
                    if part.is_empty() {
                        continue;
                    }
                    if i == 0 {
                        if !s.get(idx..).is_some_and(|rest| rest.starts_with(part)) {
                            return false;
                        }
                        idx += part.len();
                    } else if i == parts.len() - 1 {
                        if !s.ends_with(part) {
                            return false;
                        }
                    } else if let Some(pos) = s.get(idx..).and_then(|rest| rest.find(part)) {
                        idx += pos + part.len();
                    } else {
                        return false;
                    }
                }
                true
            } else {
                s == p
            }
        }
        _ => false,
    }
}

fn string_contains(haystack: &Value, needle: &Value) -> bool {
    match (haystack, needle) {
        (Value::String(s), Value::String(n)) => s.contains(n.as_str()),
        _ => false,
    }
}

const fn value_in(_item: &Value, _collection: &Value) -> bool {
    // Placeholder — would check list membership
    false
}

/// Build an AST from pest parse pairs.
fn build_ast(pairs: pest::iterators::Pairs<crate::Rule>) -> anyhow::Result<Expr> {
    let pair = pairs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty expression"))?;
    build_from_pair(pair)
}

fn build_from_pair(pair: pest::iterators::Pair<crate::Rule>) -> anyhow::Result<Expr> {
    match pair.as_rule() {
        crate::Rule::expression | crate::Rule::or_expr => {
            let mut exprs = Vec::new();
            for inner in pair.into_inner() {
                if inner.as_rule() == crate::Rule::or_expr
                    || inner.as_rule() == crate::Rule::and_expr
                    || inner.as_rule() == crate::Rule::primary
                    || inner.as_rule() == crate::Rule::comparison
                {
                    exprs.push(build_from_pair(inner)?);
                }
            }
            if exprs.len() == 1 {
                exprs
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("internal: expected one expr"))
            } else {
                Ok(Expr::Or(exprs))
            }
        }
        crate::Rule::and_expr => {
            let mut exprs = Vec::new();
            for inner in pair.into_inner() {
                if inner.as_rule() == crate::Rule::primary
                    || inner.as_rule() == crate::Rule::comparison
                {
                    exprs.push(build_from_pair(inner)?);
                }
            }
            if exprs.len() == 1 {
                exprs
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("internal: expected one expr"))
            } else {
                Ok(Expr::And(exprs))
            }
        }
        crate::Rule::primary => {
            let inner = pair
                .into_inner()
                .next()
                .ok_or_else(|| anyhow::anyhow!("primary rule has no inner pair"))?;
            build_from_pair(inner)
        }
        crate::Rule::comparison => {
            let mut inner = pair.into_inner();
            let left = inner
                .next()
                .ok_or_else(|| anyhow::anyhow!("comparison missing left operand"))?;
            let op = inner
                .next()
                .ok_or_else(|| anyhow::anyhow!("comparison missing operator"))?;
            let right = inner
                .next()
                .ok_or_else(|| anyhow::anyhow!("comparison missing right operand"))?;

            Ok(Expr::Comparison {
                left: build_operand(left)?,
                operator: parse_operator(op.as_str()),
                right: build_operand(right)?,
            })
        }
        _ => Err(anyhow::anyhow!("unexpected rule: {:?}", pair.as_rule())),
    }
}

fn build_operand(pair: pest::iterators::Pair<crate::Rule>) -> anyhow::Result<Operand> {
    match pair.as_rule() {
        crate::Rule::operand => {
            let inner = pair
                .into_inner()
                .next()
                .ok_or_else(|| anyhow::anyhow!("operand has no inner pair"))?;
            build_operand(inner)
        }
        crate::Rule::identifier => Ok(Operand::Identifier(pair.as_str().to_string())),
        crate::Rule::literal => {
            let inner = pair
                .into_inner()
                .next()
                .ok_or_else(|| anyhow::anyhow!("literal has no inner pair"))?;
            Ok(Operand::Literal(parse_literal(inner)?))
        }
        _ => Err(anyhow::anyhow!(
            "expected operand, got {:?}",
            pair.as_rule()
        )),
    }
}

fn parse_operator(s: &str) -> Operator {
    match s {
        "!=" => Operator::Ne,
        "<" => Operator::Lt,
        "<=" => Operator::Le,
        ">" => Operator::Gt,
        ">=" => Operator::Ge,
        "matches" => Operator::Matches,
        "contains" => Operator::Contains,
        "in" => Operator::In,
        _ => Operator::Eq,
    }
}

fn parse_literal(pair: pest::iterators::Pair<crate::Rule>) -> anyhow::Result<Value> {
    match pair.as_rule() {
        crate::Rule::integer => Ok(Value::Integer(pair.as_str().parse()?)),
        crate::Rule::boolean => Ok(Value::Boolean(pair.as_str() == "true")),
        crate::Rule::string => {
            let s = pair.as_str();
            // Strip surrounding quotes — the grammar guarantees these are ASCII
            // quote characters at byte boundaries.
            let inner = s
                .get(1..s.len().saturating_sub(1))
                .ok_or_else(|| anyhow::anyhow!("malformed string literal: {s}"))?;
            Ok(Value::String(inner.to_string()))
        }
        crate::Rule::duration => {
            // The duration rule matches integer + suffix as one token
            parse_duration_literal(pair.as_str())
        }
        crate::Rule::size_bytes => parse_size_literal(pair.as_str()),
        crate::Rule::percentage => {
            let text = pair.as_str();
            let num: i64 = text.trim_end_matches('%').parse()?;
            Ok(Value::Percentage(num))
        }
        _ => Err(anyhow::anyhow!("unexpected literal: {:?}", pair.as_rule())),
    }
}

fn parse_duration_literal(s: &str) -> anyhow::Result<Value> {
    let (num_part, unit) = if let Some(stripped) = s.strip_suffix("ms") {
        (stripped, crate::ast::DurationUnit::Milliseconds)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, crate::ast::DurationUnit::Seconds)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, crate::ast::DurationUnit::Minutes)
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped, crate::ast::DurationUnit::Hours)
    } else if let Some(stripped) = s.strip_suffix('d') {
        (stripped, crate::ast::DurationUnit::Days)
    } else if let Some(stripped) = s.strip_suffix('w') {
        (stripped, crate::ast::DurationUnit::Weeks)
    } else if let Some(stripped) = s.strip_suffix('M') {
        (stripped, crate::ast::DurationUnit::Months)
    } else if let Some(stripped) = s.strip_suffix('y') {
        (stripped, crate::ast::DurationUnit::Years)
    } else {
        anyhow::bail!("invalid duration: {s}");
    };
    Ok(Value::Duration(num_part.parse()?, unit))
}

fn parse_size_literal(s: &str) -> anyhow::Result<Value> {
    let (num_part, unit) = if let Some(stripped) = s.strip_suffix("TB") {
        (stripped, crate::ast::SizeUnit::Terabytes)
    } else if let Some(stripped) = s.strip_suffix("GB") {
        (stripped, crate::ast::SizeUnit::Gigabytes)
    } else if let Some(stripped) = s.strip_suffix("MB") {
        (stripped, crate::ast::SizeUnit::Megabytes)
    } else if let Some(stripped) = s.strip_suffix("KB") {
        (stripped, crate::ast::SizeUnit::Kilobytes)
    } else if let Some(stripped) = s.strip_suffix('B') {
        (stripped, crate::ast::SizeUnit::Bytes)
    } else {
        anyhow::bail!("invalid size: {s}");
    };
    Ok(Value::Size(num_part.parse()?, unit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{EvalContext, FileFlags, NetworkType};

    #[test]
    fn parse_simple_comparison() {
        let expr = parse_expr("FILE.size > 100").unwrap();
        match &expr {
            Expr::Comparison { operator, .. } => {
                assert_eq!(*operator, Operator::Gt);
            }
            _ => panic!("expected comparison"),
        }
    }

    #[test]
    fn parse_and_expression() {
        let expr = parse_expr("FILE.size > 100 && NETWORK.metered == false").unwrap();
        match &expr {
            Expr::And(exprs) => {
                assert_eq!(exprs.len(), 2);
            }
            _ => panic!("expected and"),
        }
    }

    #[test]
    fn parse_or_expression() {
        let expr = parse_expr("FILE.cached == true || FILE.pinned == true").unwrap();
        match &expr {
            Expr::Or(exprs) => {
                assert_eq!(exprs.len(), 2);
            }
            _ => panic!("expected or"),
        }
    }

    #[test]
    fn parse_string_literal() {
        let expr = parse_expr("NETWORK.type == \"wifi\"").unwrap();
        match &expr {
            Expr::Comparison { right, .. } => match right {
                Operand::Literal(Value::String(s)) => assert_eq!(s, "wifi"),
                _ => panic!("expected string literal"),
            },
            _ => panic!("expected comparison"),
        }
    }

    #[test]
    fn parse_duration_literal() {
        let expr = parse_expr("FILE.size > 10MB").unwrap();
        match &expr {
            Expr::Comparison { right, .. } => match right {
                Operand::Literal(Value::Size(v, unit)) => {
                    assert_eq!(*v, 10);
                    assert_eq!(*unit, crate::ast::SizeUnit::Megabytes);
                }
                _ => panic!("expected size literal"),
            },
            _ => panic!("expected comparison"),
        }
    }

    #[test]
    fn evaluate_file_size_gt() {
        let mut ctx = EvalContext::default();
        ctx.file.size = 200;
        let expr = parse_expr("FILE.size > 100").unwrap();
        assert!(evaluate(&expr, &ctx));
    }

    #[test]
    fn evaluate_network_type_eq() {
        let mut ctx = EvalContext::default();
        ctx.network.if_type = NetworkType::Wifi;
        let expr = parse_expr("NETWORK.type == \"wifi\"").unwrap();
        assert!(evaluate(&expr, &ctx));
    }

    #[test]
    fn evaluate_and_expression() {
        let mut ctx = EvalContext::default();
        ctx.file.size = 200;
        ctx.network.metered = false;
        let expr = parse_expr("FILE.size > 100 && NETWORK.metered == false").unwrap();
        assert!(evaluate(&expr, &ctx));
    }

    #[test]
    fn evaluate_or_expression() {
        let mut ctx = EvalContext::default();
        ctx.file.flags = FileFlags::default().with_pinned(true);
        let expr = parse_expr("FILE.cached == true || FILE.pinned == true").unwrap();
        assert!(evaluate(&expr, &ctx));
    }

    #[test]
    fn evaluate_size_comparison() {
        let mut ctx = EvalContext::default();
        ctx.file.size = 5 * 1024 * 1024; // 5MB
        let expr = parse_expr("FILE.size > 1MB").unwrap();
        assert!(evaluate(&expr, &ctx));
    }
}
