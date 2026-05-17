//! Phase 19 first-cut: defuse Closure-compiler IIFE errors.
//!
//! Google's modern bundle ships JS shaped like:
//!
//! ```js
//! this.gbar_=this.gbar_||{};
//! (function(_){var window=this;
//!     var fe=function(a){this.J=_.x(a)};
//!     _.B(fe,_.S);
//!     // …thousands of `_.X` calls…
//! }).call(this);
//! ```
//!
//! The IIFE expects `_` to be the Closure-library runtime
//! (`goog.global` namespace), populated by a separate bootstrap
//! script we don't fetch. `.call(this)` passes no argument, so
//! `_` is undefined inside, and every `_.X` access throws on
//! the first reach.
//!
//! Until we vendor real closure-library (Phase 19 proper, weeks
//! of work), this module does the lightest-possible workaround:
//!
//! 1. Scan the script for every `_.X` access — the minified
//!    symbol set this specific bundle uses.
//! 2. Generate a stub namespace where each symbol is an empty
//!    function (so `_.X(…)`, `_.X.prototype`, `new _.X()`,
//!    `_.B(fe, _.S)`-style inheritance helpers all behave
//!    without throwing).
//! 3. Rewrite the IIFE to default `_` to that namespace when
//!    called with no argument.
//!
//! What this does NOT do: the stubs are no-ops, so inheritance
//! chains aren't real, event helpers don't fire, and the
//! bundle's actual rendering doesn't happen. The user still
//! sees a near-empty page on Google /search. The point of this
//! pass is to (a) stop the error cascade so the dev-dock
//! Console is readable and (b) leave behind a single namespace
//! we can iterate on — replacing one no-op at a time with a
//! real implementation gets us toward Phase 19 proper without
//! a giant up-front vendoring spike.

use std::collections::BTreeSet;

/// IIFE detection pattern. Specifically targets the form
/// Google ships; rewriting other `function(_){…}` callers
/// would be a false-positive on user code that legitimately
/// names a parameter `_`.
const IIFE_PATTERN: &str = "(function(_){var window=this;";

/// If `source` looks like a Closure IIFE bundle, return a
/// rewritten version with a stub namespace prepended and the
/// IIFE patched to use it. Otherwise return the source
/// unchanged.
pub fn maybe_inject(source: &str) -> String {
    if !source.contains(IIFE_PATTERN) {
        return source.to_string();
    }
    let symbols = collect_closure_symbols(source);
    if symbols.is_empty() {
        return source.to_string();
    }
    let mut stub = String::new();
    // `__closure_ns` is a single shared namespace across every
    // script on the page — symbols added by one IIFE are
    // visible to the next, matching what real closure-library
    // does. Re-evaluating the same `if (typeof …)` guard each
    // script keeps re-runs idempotent.
    stub.push_str(
        "if (typeof __closure_ns === 'undefined') { var __closure_ns = {}; }\n",
    );
    for sym in &symbols {
        // Use `||` so an earlier real implementation isn't
        // clobbered by the no-op stub.
        stub.push_str(&format!(
            "__closure_ns.{sym} = __closure_ns.{sym} || function(){{}};\n"
        ));
    }
    // Replace every IIFE in the script. Multiple Closure IIFEs
    // typically appear in the same `<script>` block (gbar boot
    // + result-render + telemetry); each one starts blank
    // because Google's code never reaches back to use `_` from
    // outside.
    let rewritten = source.replace(
        IIFE_PATTERN,
        "(function(_){_ = _ || __closure_ns; var window=this;",
    );
    let mut out = String::with_capacity(stub.len() + rewritten.len() + 1);
    out.push_str(&stub);
    out.push('\n');
    out.push_str(&rewritten);
    out
}

/// Walk the source and collect every `_.X` access — the
/// minified Closure symbol set THIS specific bundle uses.
/// Boundary-checked so legit identifiers ending in `_` (like
/// `gbar_` or a user's `foo_` variable) don't get scooped up.
fn collect_closure_symbols(source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'_' && bytes[i + 1] == b'.' {
            // Preceding char must not be an identifier-
            // continuation char — otherwise the `_` is the tail
            // of a longer name (e.g. `gbar_.X`, `___closure.X`).
            let prev_ok = i == 0 || !is_ident_cont(bytes[i - 1]);
            if prev_ok {
                let mut j = i + 2;
                while j < bytes.len() && is_ident_cont(bytes[j]) {
                    j += 1;
                }
                if j > i + 2 {
                    let name = &source[i + 2..j];
                    if name.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                        out.insert(name.to_string());
                    }
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_closure_source_passes_through() {
        let src = "var x = 1; console.log(x);";
        assert_eq!(maybe_inject(src), src);
    }

    #[test]
    fn collects_closure_symbols() {
        let src =
            "(function(_){var window=this; var fe=function(a){this.J=_.x(a)};_.B(fe,_.S);}).call(this);";
        let syms = collect_closure_symbols(src);
        assert!(syms.contains("x"));
        assert!(syms.contains("B"));
        assert!(syms.contains("S"));
    }

    #[test]
    fn does_not_scoop_up_trailing_underscore_idents() {
        // `gbar_.X` ends in `_` but `gbar_` is a complete
        // identifier — the `.X` access is on it, not on the
        // bare `_`.
        let src = "var gbar_={};gbar_.X=1;";
        let syms = collect_closure_symbols(src);
        assert!(!syms.contains("X"));
    }

    #[test]
    fn rewrites_iife_to_default_namespace() {
        let src = "(function(_){var window=this; var v=_.B;}).call(this);";
        let out = maybe_inject(src);
        assert!(out.contains("__closure_ns"));
        assert!(out.contains("_.B"));
        assert!(out.contains("_ = _ || __closure_ns"));
    }

    /// End-to-end via Zinc: an IIFE that previously threw on
    /// `_.B is not a function` now runs to completion (the
    /// stub is a no-op but doesn't throw).
    #[test]
    fn rewritten_iife_evaluates_without_throwing() {
        use zinc::engine::Engine;

        let src = "var ran = 0;\n\
                   (function(_){var window=this; _.B(); _.S(); _.G('x'); ran = 1;}).call(this);";
        let out = maybe_inject(src);
        let mut engine = Engine::new();
        engine
            .eval(&out)
            .expect("rewritten IIFE should eval without throwing");
        let (ran, _) = engine.eval_with_output("ran");
        assert_eq!(ran, "1");
    }
}
