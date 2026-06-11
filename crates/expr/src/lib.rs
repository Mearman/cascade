#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! Expression language — PEG grammar parser and evaluator.
//!
//! Conditional rules in `.cascade` files use a small expression language
//! evaluated against an [`context::EvalContext`]. Built with `pest` PEG parser.

pub mod ast;
pub mod context;
pub mod eval;
pub mod providers;

#[cfg(test)]
mod eval_tests;
#[cfg(test)]
mod providers_tests;

// Pest generates the parser from grammar.pest at compile time.
use pest_derive::Parser;

#[derive(Debug, Clone, Copy, Parser)]
#[grammar = "grammar.pest"]
pub struct ExprParser;
