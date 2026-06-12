//! Minimal JSON parser — just enough to read the GitHub releases API
//! response (`tag_name`, `html_url`, `assets[].name`,
//! `assets[].browser_download_url`). Hand-rolled because nothing else in
//! the workspace needs JSON; `serde_json` would replace this file with a
//! couple of derives if that ever changes.
//!
//! String escapes are handled in full (including `\uXXXX` surrogate
//! pairs) — release bodies contain arbitrary user text, so anything less
//! would mis-frame the document.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }
}

pub fn parse(input: &str) -> Result<Json, String> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(format!("trailing data at byte {}", p.pos));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", b as char, self.pos))
        }
    }

    fn eat_literal(&mut self, lit: &str) -> bool {
        if self.bytes[self.pos..].starts_with(lit.as_bytes()) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.eat_literal("true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat_literal("false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat_literal("null") => Ok(Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("unexpected input at byte {}", self.pos)),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(entries));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let esc = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&hi) {
                                // High surrogate — must pair with \uDC00..DFFF.
                                if !self.eat_literal("\\u") {
                                    return Err("lone high surrogate".into());
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err("invalid low surrogate".into());
                                }
                                let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                char::from_u32(cp).ok_or("invalid surrogate pair")?
                            } else {
                                char::from_u32(hi).ok_or("invalid \\u escape")?
                            };
                            out.push(c);
                        }
                        other => return Err(format!("bad escape '\\{}'", other as char)),
                    }
                }
                Some(_) => {
                    // Copy a run of plain bytes (UTF-8 passes through intact).
                    let start = self.pos;
                    while !matches!(self.peek(), None | Some(b'"' | b'\\')) {
                        self.pos += 1;
                    }
                    out.push_str(
                        std::str::from_utf8(&self.bytes[start..self.pos])
                            .map_err(|_| "invalid utf-8")?,
                    );
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let end = self.pos.checked_add(4).filter(|&e| e <= self.bytes.len());
        let end = end.ok_or("truncated \\u escape")?;
        let hex = std::str::from_utf8(&self.bytes[self.pos..end]).map_err(|_| "bad \\u escape")?;
        let v = u32::from_str_radix(hex, 16).map_err(|_| "bad \\u escape")?;
        self.pos = end;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| "bad number")?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|e| format!("bad number '{text}': {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse("true").unwrap(), Json::Bool(true));
        assert_eq!(parse(" -1.5e2 ").unwrap(), Json::Num(-150.0));
        assert_eq!(parse(r#""hi""#).unwrap(), Json::Str("hi".into()));
    }

    #[test]
    fn escapes() {
        let s = parse(r#""a\"b\\c\/d\n\té😀""#).unwrap();
        assert_eq!(s.as_str().unwrap(), "a\"b\\c/d\n\té😀");
    }

    #[test]
    fn rejects_lone_surrogate() {
        assert!(parse(r#""\uD83D""#).is_err());
        assert!(parse(r#""\uD83Dx""#).is_err());
    }

    #[test]
    fn nested() {
        let doc = parse(r#"{"a": [1, {"b": "c,]}\"x"}], "d": null}"#).unwrap();
        let arr = doc.get("a").unwrap().as_arr().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1].get("b").unwrap().as_str().unwrap(), "c,]}\"x");
        assert_eq!(doc.get("d"), Some(&Json::Null));
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse(r#"{"a" 1}"#).is_err());
        assert!(parse("1 2").is_err());
        assert!(parse(r#""unterminated"#).is_err());
    }

    #[test]
    fn release_shaped_document() {
        // Shape of api.github.com/repos/{owner}/{repo}/releases/latest,
        // including a body with quotes/newlines like real release notes.
        let doc = parse(
            r###"{
              "tag_name": "v0.2.0",
              "html_url": "https://github.com/florekio/Copper/releases/tag/v0.2.0",
              "body": "## What's new\n- fixed \"quoted\" things\n- misc",
              "prerelease": false,
              "assets": [
                {"name": "copper-0.2.0-aarch64-apple-darwin.tar.gz",
                 "browser_download_url": "https://github.com/florekio/Copper/releases/download/v0.2.0/copper-0.2.0-aarch64-apple-darwin.tar.gz"},
                {"name": "copper-0.2.0-aarch64-apple-darwin.tar.gz.sha256",
                 "browser_download_url": "https://github.com/florekio/Copper/releases/download/v0.2.0/copper-0.2.0-aarch64-apple-darwin.tar.gz.sha256"}
              ]
            }"###,
        )
        .unwrap();
        assert_eq!(doc.get("tag_name").unwrap().as_str().unwrap(), "v0.2.0");
        assert_eq!(doc.get("assets").unwrap().as_arr().unwrap().len(), 2);
    }
}
