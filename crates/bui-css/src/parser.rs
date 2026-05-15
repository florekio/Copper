use crate::selector::Selector;

#[derive(Debug, Clone)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub enum Rule {
    Style(StyleRule),
    /// At-rules are kept opaque for now (the contents may be a parseable
    /// nested stylesheet; we don't dive in).
    At {
        name: String,
        prelude: String,
        block: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct StyleRule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ParseError {
    #[error("selector: {0}")]
    Selector(String),
}

impl Stylesheet {
    pub fn parse(input: &str) -> Self {
        let mut p = Parser::new(input);
        let mut rules = Vec::new();
        loop {
            p.skip_ws_and_comments();
            if p.eof() {
                break;
            }
            if p.peek() == Some('@') {
                if let Some(at) = p.parse_at_rule() {
                    rules.push(at);
                }
            } else {
                if let Some(r) = p.parse_style_rule() {
                    rules.push(Rule::Style(r));
                }
            }
        }
        Self { rules }
    }
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            input: s.as_bytes(),
            pos: 0,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<char> {
        if self.pos >= self.input.len() {
            return None;
        }
        // Decode one UTF-8 char at `pos` so multi-byte content (e.g.
        // Wikipedia's `.hlist li::after { content: "\\a0 · " }` —
        // the · is bytes 0xC2 0xB7) round-trips through `out.push(c)`
        // intact instead of getting split into separate Latin-1
        // codepoints. We rely on the input being valid UTF-8 (it
        // came from `String::from_utf8_lossy` upstream).
        let s = std::str::from_utf8(&self.input[self.pos..]).ok()?;
        s.chars().next()
    }

    fn peek2(&self) -> Option<(char, char)> {
        let a = self.peek()?;
        let next_pos = self.pos + a.len_utf8();
        if next_pos >= self.input.len() {
            return None;
        }
        let s = std::str::from_utf8(&self.input[next_pos..]).ok()?;
        let b = s.chars().next()?;
        Some((a, b))
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// Step past one whole UTF-8 char at `pos`. Used by callers that
    /// peeked the char and decided to consume it without re-binding.
    fn step(&mut self) {
        if let Some(c) = self.peek() {
            self.pos += c.len_utf8();
        } else {
            self.pos += 1;
        }
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while let Some(c) = self.peek() {
                if c.is_ascii_whitespace() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.peek2() == Some(('/', '*')) {
                self.pos += 2;
                while !self.eof() && self.peek2() != Some(('*', '/')) {
                    self.pos += 1;
                }
                if !self.eof() {
                    self.pos += 2;
                }
                continue;
            }
            break;
        }
    }

    /// Read raw text up to (but not including) any of `stop` chars at top
    /// nesting level — i.e. respecting `()`, `[]`, `{}`, strings, and
    /// `/* */` comments.
    fn read_until(&mut self, stop: &[char]) -> String {
        let mut out = String::new();
        let mut paren = 0i32;
        let mut bracket = 0i32;
        let mut brace = 0i32;
        while let Some(c) = self.peek() {
            if paren == 0 && bracket == 0 && brace == 0 && stop.contains(&c) {
                break;
            }
            match c {
                '/' if self.peek2() == Some(('/', '*')) => {
                    self.pos += 2;
                    while !self.eof() && self.peek2() != Some(('*', '/')) {
                        self.pos += 1;
                    }
                    if !self.eof() {
                        self.pos += 2;
                    }
                    continue;
                }
                '"' | '\'' => {
                    let quote = c;
                    out.push(c);
                    self.pos += c.len_utf8();
                    while let Some(cc) = self.peek() {
                        out.push(cc);
                        self.pos += cc.len_utf8();
                        if cc == '\\' {
                            if let Some(esc) = self.peek() {
                                out.push(esc);
                                self.pos += esc.len_utf8();
                            }
                            continue;
                        }
                        if cc == quote {
                            break;
                        }
                    }
                    continue;
                }
                '(' => paren += 1,
                ')' => paren -= 1,
                '[' => bracket += 1,
                ']' => bracket -= 1,
                '{' => brace += 1,
                '}' => brace -= 1,
                _ => {}
            }
            out.push(c);
            self.pos += c.len_utf8();
        }
        out
    }

    fn parse_at_rule(&mut self) -> Option<Rule> {
        // Skip '@'
        self.pos += 1;
        // Read identifier
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let name = std::str::from_utf8(&self.input[start..self.pos])
            .ok()?
            .to_string();
        self.skip_ws_and_comments();
        let prelude = self.read_until(&['{', ';']).trim().to_string();
        let block = if self.peek() == Some('{') {
            self.pos += 1;
            let body = self.read_until(&['}']);
            if self.peek() == Some('}') {
                self.pos += 1;
            }
            Some(body)
        } else {
            if self.peek() == Some(';') {
                self.pos += 1;
            }
            None
        };
        Some(Rule::At {
            name,
            prelude,
            block,
        })
    }

    fn parse_style_rule(&mut self) -> Option<StyleRule> {
        let prelude = self.read_until(&['{']);
        if self.peek() != Some('{') {
            return None;
        }
        self.pos += 1;
        let body = self.read_until(&['}']);
        if self.peek() == Some('}') {
            self.pos += 1;
        }

        let selectors = parse_selector_list(&prelude);
        if selectors.is_empty() {
            return None;
        }
        let declarations = parse_declarations(&body);
        Some(StyleRule {
            selectors,
            declarations,
        })
    }
}

fn parse_selector_list(text: &str) -> Vec<Selector> {
    split_top_level(text, ',')
        .into_iter()
        .filter_map(|s| Selector::parse(s.trim()).ok())
        .collect()
}

fn parse_declarations(text: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for raw in split_top_level(text, ';') {
        let raw = strip_comments(raw.trim());
        if raw.is_empty() {
            continue;
        }
        if let Some((name, value)) = raw.split_once(':') {
            // CSS custom properties (`--foo`) are case-sensitive per
            // the Custom Properties spec — `--Mhs7de` and `--mhs7de`
            // are distinct variables. Standard properties are
            // ASCII-case-insensitive (`Color` == `color`), so only
            // those get lowercased. Without this split, Google's
            // `var(--Mhs7de)` lookup against the parser's stored
            // `--mhs7de` always missed and the body fell back to
            // 16px instead of the declared 14px.
            let trimmed = name.trim();
            let name = if trimmed.starts_with("--") {
                trimmed.to_string()
            } else {
                trimmed.to_ascii_lowercase()
            };
            let mut value = value.trim().to_string();
            let mut important = false;
            // Detect !important (case-insensitive, with optional whitespace).
            let lc = value.to_ascii_lowercase();
            if let Some(idx) = lc.rfind("!important") {
                let before = &value[..idx];
                let boundary = idx == 0
                    || before.ends_with(|c: char| c.is_ascii_whitespace());
                if boundary {
                    important = true;
                    value = before.trim_end().to_string();
                }
            }
            out.push(Declaration {
                name,
                value: value.trim().to_string(),
                important,
            });
        }
    }
    out
}

fn strip_comments(input: &str) -> String {
    // Walk the string by `char_indices` so multi-byte UTF-8 chars
    // (e.g. the `·` middle dot used by Wikipedia's `.hlist
    // li::after { content: "\\a0 · " }` rule) round-trip intact.
    // Previously we cast `bytes[i] as char`, which split each byte
    // into its own char and produced mojibake (`·` → `Â·`,
    // sometimes triple-encoded by downstream re-parses).
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            } else {
                break;
            }
            continue;
        }
        // Decode one UTF-8 char starting at byte i — checking the lead
        // byte's high bits tells us how many continuation bytes follow.
        let lead = bytes[i];
        let len = if lead < 0x80 {
            1
        } else if lead < 0xC0 {
            // Stray continuation byte — treat as one byte and move on.
            1
        } else if lead < 0xE0 {
            2
        } else if lead < 0xF0 {
            3
        } else {
            4
        };
        let end = (i + len).min(bytes.len());
        if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
            out.push_str(s);
        }
        i = end;
    }
    out
}

fn split_top_level(input: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                let quote = c;
                buf.push(c);
                while let Some(cc) = chars.next() {
                    buf.push(cc);
                    if cc == '\\' {
                        if let Some(esc) = chars.next() {
                            buf.push(esc);
                        }
                        continue;
                    }
                    if cc == quote {
                        break;
                    }
                }
                continue;
            }
            '(' => paren += 1,
            ')' => paren -= 1,
            '[' => bracket += 1,
            ']' => bracket -= 1,
            '{' => brace += 1,
            '}' => brace -= 1,
            _ => {}
        }
        if c == sep && paren == 0 && bracket == 0 && brace == 0 {
            out.push(std::mem::take(&mut buf));
        } else {
            buf.push(c);
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_rule() {
        let css = "p { color: red; font-size: 16px; }";
        let s = Stylesheet::parse(css);
        assert_eq!(s.rules.len(), 1);
        if let Rule::Style(sr) = &s.rules[0] {
            assert_eq!(sr.declarations.len(), 2);
            assert_eq!(sr.declarations[0].name, "color");
            assert_eq!(sr.declarations[0].value, "red");
            assert_eq!(sr.declarations[1].name, "font-size");
            assert_eq!(sr.declarations[1].value, "16px");
        } else {
            panic!()
        }
    }

    #[test]
    fn important_marker() {
        let s = Stylesheet::parse("p { color: red !important }");
        if let Rule::Style(sr) = &s.rules[0] {
            assert!(sr.declarations[0].important);
            assert_eq!(sr.declarations[0].value, "red");
        } else {
            panic!()
        }
    }

    #[test]
    fn comments_stripped() {
        let s = Stylesheet::parse("/* hi */ p /* x */ { color: /* z */ red; }");
        assert_eq!(s.rules.len(), 1);
    }

    #[test]
    fn at_media_kept_opaque() {
        let s = Stylesheet::parse("@media (min-width: 600px) { p { color: red; } }");
        match &s.rules[0] {
            Rule::At { name, .. } => assert_eq!(name, "media"),
            _ => panic!(),
        }
    }

    #[test]
    fn selector_list() {
        let s = Stylesheet::parse("h1, h2, h3 { color: black; }");
        if let Rule::Style(sr) = &s.rules[0] {
            assert_eq!(sr.selectors.len(), 3);
        } else {
            panic!()
        }
    }

    #[test]
    fn parse_first_google_style_block_no_phantom_display_none() {
        // The first inline `<style>` of google.com's homepage. The rule
        // count is what matters most: we should produce 9 style rules,
        // none of which leak a `display: none` declaration onto a
        // selector other than `.mwht9d`.
        let css = ".L3eUgb{display:flex;flex-direction:column;height:100%}\
                   .o3j99{flex-shrink:0;box-sizing:border-box}\
                   .n1xJcf{height:60px}\
                   .LLD4me{min-height:150px;height:calc(100% - 560px);max-height:290px}\
                   .yr19Zb{min-height:92px}\
                   .mwht9d{display:none}\
                   .ADHj4e{padding-top:0px;padding-bottom:85px}\
                   .oWyZre{width:100%;height:500px;border-width:0}\
                   .qarstb{flex-grow:1}";
        let s = Stylesheet::parse(css);
        let rules: Vec<&StyleRule> = s
            .rules
            .iter()
            .filter_map(|r| if let Rule::Style(sr) = r { Some(sr) } else { None })
            .collect();
        assert_eq!(rules.len(), 9, "expected 9 rules");
        for r in &rules {
            let sel_text = format!("{:?}", r.selectors);
            for d in &r.declarations {
                if d.name == "display" && d.value == "none" {
                    assert!(
                        sel_text.contains("mwht9d"),
                        "rule {} unexpectedly has display:none",
                        sel_text
                    );
                }
            }
        }
    }

    #[test]
    fn google_minified_with_repeated_selector() {
        // Real Google homepage `<style>` content has the same selector
        // appear twice without a separator newline. We must produce
        // both rules so the cascade picks up the second declaration
        // (the .LS8OJ container is `display:grid` per the second
        // rule, not `display:flex` per the first).
        let css = ".LS8OJ{display:flex;flex-direction:column}\
                   .k1zIA{height:100%}\
                   .LS8OJ{display:grid;justify-items:center}";
        let s = Stylesheet::parse(css);
        let style_rules: Vec<&StyleRule> = s
            .rules
            .iter()
            .filter_map(|r| if let Rule::Style(sr) = r { Some(sr) } else { None })
            .collect();
        assert_eq!(style_rules.len(), 3, "expected 3 style rules, got {}", style_rules.len());
        // Last .LS8OJ rule wins for `display`.
        let last = style_rules.last().unwrap();
        assert!(last.declarations.iter().any(|d| d.name == "display" && d.value == "grid"));
    }
}
