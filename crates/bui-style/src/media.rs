//! `@media` query parsing and evaluation.
//!
//! Subset implemented: media types (`all`, `screen`, `print`),
//! feature expressions `(min-width: Npx)` / `(max-width: Npx)` /
//! `(min-height: Npx)` / `(max-height: Npx)` / `(orientation: landscape|portrait)`,
//! combined with `and`, alternated with `,`, optionally negated by a
//! leading `not`. Anything we can't parse evaluates to *match* — we
//! prefer to over-apply rules rather than silently drop a stylesheet.

#[derive(Debug, Clone, Copy)]
pub struct ViewportSize {
    pub width: f32,
    pub height: f32,
}

impl ViewportSize {
    pub const DEFAULT_DESKTOP: Self = Self {
        width: 1280.0,
        height: 800.0,
    };
}

/// Evaluate a `@media` prelude (the text between `@media` and `{`)
/// against the viewport. Returns `true` if at least one of the
/// comma-separated alternatives matches.
pub fn matches(prelude: &str, vp: ViewportSize) -> bool {
    if prelude.trim().is_empty() {
        return true;
    }
    for alt in prelude.split(',') {
        if matches_single(alt.trim(), vp) {
            return true;
        }
    }
    false
}

fn matches_single(query: &str, vp: ViewportSize) -> bool {
    if query.is_empty() {
        return true;
    }
    let (negate, rest) = if let Some(r) = strip_kw(query, "not") {
        (true, r)
    } else {
        (false, query)
    };
    // `only` is just a hint to legacy parsers — we accept it without a
    // semantic effect.
    let rest = strip_kw(rest, "only").unwrap_or(rest);
    // Optional media type leads, then `and (feature) and (feature)...`.
    let (ty, rest) = read_media_type(rest);
    let mut ok = match ty {
        "screen" | "all" | "" => true,
        "print" => false,
        _ => true,
    };
    let mut tail = rest.trim();
    while ok && !tail.is_empty() {
        // Expect `and (feature)`.
        if let Some(after_and) = strip_kw(tail, "and") {
            tail = after_and.trim();
        } else if !tail.starts_with('(') {
            // Unrecognized tail; bail open (match).
            break;
        }
        let Some(close) = tail.find(')') else { break };
        let feature = &tail[1..close];
        ok = ok && eval_feature(feature, vp);
        tail = tail[close + 1..].trim();
    }
    if negate { !ok } else { ok }
}

fn eval_feature(feature: &str, vp: ViewportSize) -> bool {
    let (name, value) = match feature.split_once(':') {
        Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim()),
        None => return true, // bare features like `(color)` — assume match.
    };
    match name.as_str() {
        "min-width" => parse_px(value).is_some_and(|px| vp.width >= px),
        "max-width" => parse_px(value).is_some_and(|px| vp.width <= px),
        "min-height" => parse_px(value).is_some_and(|px| vp.height >= px),
        "max-height" => parse_px(value).is_some_and(|px| vp.height <= px),
        "orientation" => match value.to_ascii_lowercase().as_str() {
            "landscape" => vp.width >= vp.height,
            "portrait" => vp.height >= vp.width,
            _ => true,
        },
        // Unknown feature — match (we'd rather over-apply than drop a sheet).
        _ => true,
    }
}

/// Strip a leading keyword + a single whitespace separator. Returns the
/// remainder if the keyword matched, otherwise `None`. The match is
/// case-insensitive.
fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if s.len() < kw.len() {
        return None;
    }
    if !s[..kw.len()].eq_ignore_ascii_case(kw) {
        return None;
    }
    let after = &s[kw.len()..];
    // Either end-of-string or a whitespace boundary.
    if after.is_empty() {
        return Some(after);
    }
    let next = after.as_bytes()[0];
    if next.is_ascii_whitespace() || next == b'(' {
        Some(after)
    } else {
        None
    }
}

fn read_media_type(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    if s.starts_with('(') {
        return ("", s);
    }
    let end = s
        .find(|c: char| c.is_ascii_whitespace() || c == ',')
        .unwrap_or(s.len());
    (&s[..end], &s[end..])
}

fn parse_px(v: &str) -> Option<f32> {
    let v = v.trim();
    if let Some(num) = v.strip_suffix("px") {
        return num.trim().parse().ok();
    }
    // Treat unit-less or em / rem as px-equivalent for evaluation only.
    if let Some(num) = v.strip_suffix("em").or_else(|| v.strip_suffix("rem")) {
        return num.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    v.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(w: f32, h: f32) -> ViewportSize {
        ViewportSize { width: w, height: h }
    }

    #[test]
    fn min_width_threshold() {
        assert!(matches("(min-width: 600px)", vp(800.0, 600.0)));
        assert!(!matches("(min-width: 1000px)", vp(800.0, 600.0)));
    }

    #[test]
    fn max_width_threshold() {
        assert!(matches("(max-width: 1000px)", vp(800.0, 600.0)));
        assert!(!matches("(max-width: 500px)", vp(800.0, 600.0)));
    }

    #[test]
    fn screen_matches() {
        assert!(matches("screen", vp(800.0, 600.0)));
        assert!(matches("screen and (min-width: 100px)", vp(800.0, 600.0)));
        assert!(!matches("print", vp(800.0, 600.0)));
    }

    #[test]
    fn comma_alternatives() {
        // First alternative fails (too narrow), second passes.
        assert!(matches(
            "(min-width: 9999px), (min-width: 100px)",
            vp(800.0, 600.0),
        ));
    }

    #[test]
    fn not_negates() {
        assert!(!matches("not screen", vp(800.0, 600.0)));
        assert!(matches("not print", vp(800.0, 600.0)));
    }

    #[test]
    fn unknown_feature_matches() {
        // Forward-compat: unrecognized features fall through as match.
        assert!(matches("(prefers-color-scheme: dark)", vp(800.0, 600.0)));
    }

    #[test]
    fn orientation() {
        assert!(matches("(orientation: landscape)", vp(1280.0, 800.0)));
        assert!(!matches("(orientation: portrait)", vp(1280.0, 800.0)));
        assert!(matches("(orientation: portrait)", vp(400.0, 800.0)));
    }
}
