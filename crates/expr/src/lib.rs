//! Expression language — PEG grammar parser and evaluator.
//!
//! Conditional rules in `.cascade` files use a small expression language
//! evaluated against an [`EvalContext`]. Built with `pest` PEG parser.

pub mod ast;
pub mod context;
pub mod eval;

// Pest generates the parser from grammar.pest at compile time.
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "grammar.pest"]
pub struct ExprParser;
