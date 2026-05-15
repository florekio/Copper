//! bui-css — CSS Syntax L3 + Selectors L4 (subset).
//!
//! Phase 3 subset:
//!   * Tokenizer that handles real-world CSS (comments, strings, urls,
//!     numbers, dimensions, identifiers, hashes, at-keywords, blocks).
//!   * Parser that produces a flat list of `Rule`s; at-rules are kept as
//!     opaque blocks (their content is not applied yet).
//!   * Selectors L4 subset: type, universal, id, class, attribute,
//!     descendant, child, adjacent-sibling, general-sibling combinators;
//!     `:hover`, `:active`, `:focus`, `:link`, `:visited`, `:first-child`,
//!     `:last-child`, `:nth-child(an+b)`, `:not(<compound>)`.
//!   * Specificity computed per Selectors L4.

mod parser;
mod selector;

pub use parser::{Declaration, ParseError, Rule, Stylesheet, StyleRule};
pub use selector::{
    AttrMatch, Combinator, Compound, NthFormula, PseudoClass, Selector, Specificity,
};
