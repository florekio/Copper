//! bui-html — HTML5 tokenizer + tree builder.
//!
//! Pragmatic Phase-2 subset: covers the markup real pages use (start/end
//! tags, attributes with all four quoting flavors, comments, doctype, named
//! and numeric character references, raw-text content of `<script>` and
//! `<style>`, void elements, basic implicit-close rules). Skipped: foster
//! parenting, the formatting-element adoption agency algorithm, template
//! parsing, foreign content (SVG/MathML), full named-entity table.

mod tokenizer;
mod tree_builder;

pub use tokenizer::{Token, Tokenizer};
pub use tree_builder::parse;
