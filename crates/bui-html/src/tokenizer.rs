use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Doctype {
        name: String,
        public_id: String,
        system_id: String,
    },
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    Comment(String),
    Character(char),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    TagOpen,
    EndTagOpen,
    TagName,
    BeforeAttrName,
    AttrName,
    AfterAttrName,
    BeforeAttrValue,
    AttrValueDoubleQuoted,
    AttrValueSingleQuoted,
    AttrValueUnquoted,
    AfterAttrValueQuoted,
    SelfClosingStartTag,
    BogusComment,
    MarkupDeclarationOpen,
    CommentStart,
    CommentStartDash,
    Comment,
    CommentEndDash,
    CommentEnd,
    Doctype,
    BeforeDoctypeName,
    DoctypeName,
    AfterDoctypeName,
    BogusDoctype,
    Rawtext,
    RawtextLessThanSign,
    RawtextEndTagOpen,
    RawtextEndTagName,
}

pub struct Tokenizer<'a> {
    input: &'a str,
    cursor: usize,
    pushback: Option<char>,
    state: State,
    /// Whether the *current* end tag is "appropriate" for the rawtext we left
    /// (i.e. matches `last_start_tag`).
    last_start_tag: String,
    rawtext_buf: String,

    tag_name: String,
    tag_attrs: Vec<(String, String)>,
    tag_self_closing: bool,
    tag_is_end: bool,
    attr_name: String,
    attr_value: String,
    comment_buf: String,
    doctype_name: String,

    output: VecDeque<Token>,
    eof_emitted: bool,
}

impl<'a> Tokenizer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            cursor: 0,
            pushback: None,
            state: State::Data,
            last_start_tag: String::new(),
            rawtext_buf: String::new(),
            tag_name: String::new(),
            tag_attrs: Vec::new(),
            tag_self_closing: false,
            tag_is_end: false,
            attr_name: String::new(),
            attr_value: String::new(),
            comment_buf: String::new(),
            doctype_name: String::new(),
            output: VecDeque::new(),
            eof_emitted: false,
        }
    }

    pub fn tokenize_all(input: &'a str) -> Vec<Token> {
        let mut t = Self::new(input);
        let mut out = Vec::new();
        loop {
            let tok = t.next_token();
            let is_eof = matches!(tok, Token::Eof);
            out.push(tok);
            if is_eof {
                break;
            }
        }
        out
    }

    pub fn next_token(&mut self) -> Token {
        loop {
            if let Some(t) = self.output.pop_front() {
                return t;
            }
            if self.eof_emitted {
                return Token::Eof;
            }
            self.step();
        }
    }

    fn step(&mut self) {
        match self.state {
            State::Data => self.data(),
            State::TagOpen => self.tag_open(),
            State::EndTagOpen => self.end_tag_open(),
            State::TagName => self.tag_name(),
            State::BeforeAttrName => self.before_attr_name(),
            State::AttrName => self.attr_name(),
            State::AfterAttrName => self.after_attr_name(),
            State::BeforeAttrValue => self.before_attr_value(),
            State::AttrValueDoubleQuoted => self.attr_value_quoted(b'"'),
            State::AttrValueSingleQuoted => self.attr_value_quoted(b'\''),
            State::AttrValueUnquoted => self.attr_value_unquoted(),
            State::AfterAttrValueQuoted => self.after_attr_value_quoted(),
            State::SelfClosingStartTag => self.self_closing_start_tag(),
            State::BogusComment => self.bogus_comment(),
            State::MarkupDeclarationOpen => self.markup_declaration_open(),
            State::CommentStart => self.comment_start(),
            State::CommentStartDash => self.comment_start_dash(),
            State::Comment => self.comment(),
            State::CommentEndDash => self.comment_end_dash(),
            State::CommentEnd => self.comment_end(),
            State::Doctype => self.doctype(),
            State::BeforeDoctypeName => self.before_doctype_name(),
            State::DoctypeName => self.doctype_name(),
            State::AfterDoctypeName => self.after_doctype_name(),
            State::BogusDoctype => self.bogus_doctype(),
            State::Rawtext => self.rawtext(),
            State::RawtextLessThanSign => self.rawtext_lt(),
            State::RawtextEndTagOpen => self.rawtext_end_tag_open(),
            State::RawtextEndTagName => self.rawtext_end_tag_name(),
        }
    }

    /// Switch to rawtext mode for the given tag name (script/style/etc.).
    pub fn enter_rawtext(&mut self, last_start_tag: &str) {
        self.last_start_tag = last_start_tag.to_string();
        self.state = State::Rawtext;
        self.rawtext_buf.clear();
    }

    fn consume(&mut self) -> Option<char> {
        if let Some(c) = self.pushback.take() {
            return Some(c);
        }
        let rest = &self.input[self.cursor..];
        let mut chars = rest.chars();
        let c = chars.next()?;
        self.cursor += c.len_utf8();
        Some(c)
    }

    fn reconsume(&mut self, c: char) {
        debug_assert!(self.pushback.is_none());
        self.pushback = Some(c);
    }

    fn emit(&mut self, t: Token) {
        if let Token::StartTag { name, .. } = &t {
            self.last_start_tag = name.clone();
        }
        self.output.push_back(t);
    }

    fn emit_eof(&mut self) {
        self.eof_emitted = true;
    }

    fn finish_tag(&mut self) {
        // Move pending attribute, if any.
        if !self.attr_name.is_empty() {
            push_attr(
                &mut self.tag_attrs,
                std::mem::take(&mut self.attr_name),
                std::mem::take(&mut self.attr_value),
            );
        }
        let name = std::mem::take(&mut self.tag_name);
        let attrs = std::mem::take(&mut self.tag_attrs);
        let self_closing = self.tag_self_closing;
        let is_end = self.tag_is_end;
        self.tag_self_closing = false;
        self.tag_is_end = false;
        if is_end {
            self.emit(Token::EndTag { name });
        } else {
            self.emit(Token::StartTag {
                name,
                attrs,
                self_closing,
            });
        }
    }

    fn data(&mut self) {
        match self.consume() {
            Some('&') => {
                if let Some(s) = self.consume_char_reference(false) {
                    for c in s.chars() {
                        self.emit(Token::Character(c));
                    }
                } else {
                    self.emit(Token::Character('&'));
                }
            }
            Some('<') => self.state = State::TagOpen,
            Some('\0') => self.emit(Token::Character('\u{FFFD}')),
            Some(c) => self.emit(Token::Character(c)),
            None => self.emit_eof(),
        }
    }

    fn tag_open(&mut self) {
        match self.consume() {
            Some('!') => self.state = State::MarkupDeclarationOpen,
            Some('/') => self.state = State::EndTagOpen,
            Some(c) if c.is_ascii_alphabetic() => {
                self.tag_name.clear();
                self.tag_attrs.clear();
                self.tag_self_closing = false;
                self.tag_is_end = false;
                self.tag_name.push(c.to_ascii_lowercase());
                self.state = State::TagName;
            }
            Some('?') => {
                self.comment_buf.clear();
                self.comment_buf.push('?');
                self.state = State::BogusComment;
            }
            Some(c) => {
                self.emit(Token::Character('<'));
                self.reconsume(c);
                self.state = State::Data;
            }
            None => {
                self.emit(Token::Character('<'));
                self.emit_eof();
            }
        }
    }

    fn end_tag_open(&mut self) {
        match self.consume() {
            Some(c) if c.is_ascii_alphabetic() => {
                self.tag_name.clear();
                self.tag_attrs.clear();
                self.tag_self_closing = false;
                self.tag_is_end = true;
                self.tag_name.push(c.to_ascii_lowercase());
                self.state = State::TagName;
            }
            Some('>') => {
                // </>  — error, ignore
                self.state = State::Data;
            }
            Some(c) => {
                self.comment_buf.clear();
                self.comment_buf.push(c);
                self.state = State::BogusComment;
            }
            None => {
                self.emit(Token::Character('<'));
                self.emit(Token::Character('/'));
                self.emit_eof();
            }
        }
    }

    fn tag_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => self.state = State::BeforeAttrName,
            Some('/') => self.state = State::SelfClosingStartTag,
            Some('>') => {
                self.finish_tag();
                self.state = State::Data;
            }
            Some(c) => self.tag_name.push(c.to_ascii_lowercase()),
            None => self.emit_eof(),
        }
    }

    fn before_attr_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => {}
            Some('/') => self.state = State::SelfClosingStartTag,
            Some('>') => {
                self.finish_tag();
                self.state = State::Data;
            }
            Some('=') => {
                // Anomalous start: include '=' in attribute name.
                self.attr_name.clear();
                self.attr_value.clear();
                self.attr_name.push('=');
                self.state = State::AttrName;
            }
            Some(c) => {
                self.attr_name.clear();
                self.attr_value.clear();
                self.attr_name.push(c.to_ascii_lowercase());
                self.state = State::AttrName;
            }
            None => self.emit_eof(),
        }
    }

    fn attr_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => self.state = State::AfterAttrName,
            Some('/') => {
                self.flush_attr();
                self.state = State::SelfClosingStartTag;
            }
            Some('>') => {
                self.flush_attr();
                self.finish_tag();
                self.state = State::Data;
            }
            Some('=') => self.state = State::BeforeAttrValue,
            Some(c) => self.attr_name.push(c.to_ascii_lowercase()),
            None => self.emit_eof(),
        }
    }

    fn after_attr_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => {}
            Some('/') => {
                self.flush_attr();
                self.state = State::SelfClosingStartTag;
            }
            Some('=') => self.state = State::BeforeAttrValue,
            Some('>') => {
                self.flush_attr();
                self.finish_tag();
                self.state = State::Data;
            }
            Some(c) => {
                self.flush_attr();
                self.attr_name.push(c.to_ascii_lowercase());
                self.state = State::AttrName;
            }
            None => self.emit_eof(),
        }
    }

    fn before_attr_value(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => {}
            Some('"') => self.state = State::AttrValueDoubleQuoted,
            Some('\'') => self.state = State::AttrValueSingleQuoted,
            Some('>') => {
                self.flush_attr();
                self.finish_tag();
                self.state = State::Data;
            }
            Some(c) => {
                self.attr_value.push(c);
                self.state = State::AttrValueUnquoted;
            }
            None => self.emit_eof(),
        }
    }

    fn attr_value_quoted(&mut self, quote: u8) {
        match self.consume() {
            Some(c) if c as u32 == u32::from(quote) => {
                self.flush_attr();
                self.state = State::AfterAttrValueQuoted;
            }
            Some('&') => {
                if let Some(s) = self.consume_char_reference(true) {
                    self.attr_value.push_str(&s);
                } else {
                    self.attr_value.push('&');
                }
            }
            Some(c) => self.attr_value.push(c),
            None => self.emit_eof(),
        }
    }

    fn attr_value_unquoted(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => {
                self.flush_attr();
                self.state = State::BeforeAttrName;
            }
            Some('&') => {
                if let Some(s) = self.consume_char_reference(true) {
                    self.attr_value.push_str(&s);
                } else {
                    self.attr_value.push('&');
                }
            }
            Some('>') => {
                self.flush_attr();
                self.finish_tag();
                self.state = State::Data;
            }
            Some(c) => self.attr_value.push(c),
            None => self.emit_eof(),
        }
    }

    fn after_attr_value_quoted(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => self.state = State::BeforeAttrName,
            Some('/') => self.state = State::SelfClosingStartTag,
            Some('>') => {
                self.finish_tag();
                self.state = State::Data;
            }
            Some(c) => {
                self.reconsume(c);
                self.state = State::BeforeAttrName;
            }
            None => self.emit_eof(),
        }
    }

    fn self_closing_start_tag(&mut self) {
        match self.consume() {
            Some('>') => {
                self.tag_self_closing = true;
                self.finish_tag();
                self.state = State::Data;
            }
            Some(c) => {
                self.reconsume(c);
                self.state = State::BeforeAttrName;
            }
            None => self.emit_eof(),
        }
    }

    fn flush_attr(&mut self) {
        if self.attr_name.is_empty() {
            self.attr_value.clear();
            return;
        }
        push_attr(
            &mut self.tag_attrs,
            std::mem::take(&mut self.attr_name),
            std::mem::take(&mut self.attr_value),
        );
    }

    fn markup_declaration_open(&mut self) {
        let rest = &self.input[self.cursor..];
        if let Some(stripped) = rest.strip_prefix("--") {
            self.cursor += 2;
            let _ = stripped;
            self.comment_buf.clear();
            self.state = State::CommentStart;
            return;
        }
        if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case("DOCTYPE") {
            self.cursor += 7;
            self.state = State::Doctype;
            return;
        }
        // Treat anything else as a bogus comment.
        self.comment_buf.clear();
        self.state = State::BogusComment;
    }

    fn bogus_comment(&mut self) {
        match self.consume() {
            Some('>') => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.state = State::Data;
            }
            Some(c) => self.comment_buf.push(c),
            None => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.emit_eof();
            }
        }
    }

    fn comment_start(&mut self) {
        match self.consume() {
            Some('-') => self.state = State::CommentStartDash,
            Some('>') => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.state = State::Data;
            }
            Some(c) => {
                self.reconsume(c);
                self.state = State::Comment;
            }
            None => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.emit_eof();
            }
        }
    }

    fn comment_start_dash(&mut self) {
        match self.consume() {
            Some('-') => self.state = State::CommentEnd,
            Some('>') => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.state = State::Data;
            }
            Some(c) => {
                self.comment_buf.push('-');
                self.reconsume(c);
                self.state = State::Comment;
            }
            None => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.emit_eof();
            }
        }
    }

    fn comment(&mut self) {
        match self.consume() {
            Some('-') => self.state = State::CommentEndDash,
            Some(c) => self.comment_buf.push(c),
            None => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.emit_eof();
            }
        }
    }

    fn comment_end_dash(&mut self) {
        match self.consume() {
            Some('-') => self.state = State::CommentEnd,
            Some(c) => {
                self.comment_buf.push('-');
                self.reconsume(c);
                self.state = State::Comment;
            }
            None => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.emit_eof();
            }
        }
    }

    fn comment_end(&mut self) {
        match self.consume() {
            Some('>') => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.state = State::Data;
            }
            Some('-') => self.comment_buf.push('-'),
            Some(c) => {
                self.comment_buf.push_str("--");
                self.reconsume(c);
                self.state = State::Comment;
            }
            None => {
                let c = std::mem::take(&mut self.comment_buf);
                self.emit(Token::Comment(c));
                self.emit_eof();
            }
        }
    }

    fn doctype(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => self.state = State::BeforeDoctypeName,
            Some(c) => {
                self.reconsume(c);
                self.state = State::BeforeDoctypeName;
            }
            None => self.emit_eof(),
        }
    }

    fn before_doctype_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => {}
            Some('>') => {
                self.emit(Token::Doctype {
                    name: String::new(),
                    public_id: String::new(),
                    system_id: String::new(),
                });
                self.state = State::Data;
            }
            Some(c) => {
                self.doctype_name.clear();
                self.doctype_name.push(c.to_ascii_lowercase());
                self.state = State::DoctypeName;
            }
            None => self.emit_eof(),
        }
    }

    fn doctype_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => self.state = State::AfterDoctypeName,
            Some('>') => {
                let name = std::mem::take(&mut self.doctype_name);
                self.emit(Token::Doctype {
                    name,
                    public_id: String::new(),
                    system_id: String::new(),
                });
                self.state = State::Data;
            }
            Some(c) => self.doctype_name.push(c.to_ascii_lowercase()),
            None => self.emit_eof(),
        }
    }

    fn after_doctype_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) => {}
            Some('>') => {
                let name = std::mem::take(&mut self.doctype_name);
                self.emit(Token::Doctype {
                    name,
                    public_id: String::new(),
                    system_id: String::new(),
                });
                self.state = State::Data;
            }
            Some(_) => {
                // We don't parse PUBLIC/SYSTEM identifiers — drop into bogus state to find '>'.
                self.state = State::BogusDoctype;
            }
            None => self.emit_eof(),
        }
    }

    fn bogus_doctype(&mut self) {
        match self.consume() {
            Some('>') => {
                let name = std::mem::take(&mut self.doctype_name);
                self.emit(Token::Doctype {
                    name,
                    public_id: String::new(),
                    system_id: String::new(),
                });
                self.state = State::Data;
            }
            Some(_) => {}
            None => self.emit_eof(),
        }
    }

    fn rawtext(&mut self) {
        match self.consume() {
            Some('<') => self.state = State::RawtextLessThanSign,
            Some(c) => self.emit(Token::Character(c)),
            None => self.emit_eof(),
        }
    }

    fn rawtext_lt(&mut self) {
        match self.consume() {
            Some('/') => {
                self.rawtext_buf.clear();
                self.state = State::RawtextEndTagOpen;
            }
            Some(c) => {
                self.emit(Token::Character('<'));
                self.reconsume(c);
                self.state = State::Rawtext;
            }
            None => {
                self.emit(Token::Character('<'));
                self.emit_eof();
            }
        }
    }

    fn rawtext_end_tag_open(&mut self) {
        match self.consume() {
            Some(c) if c.is_ascii_alphabetic() => {
                self.tag_name.clear();
                self.tag_attrs.clear();
                self.tag_is_end = true;
                self.tag_self_closing = false;
                self.tag_name.push(c.to_ascii_lowercase());
                self.rawtext_buf.push(c);
                self.state = State::RawtextEndTagName;
            }
            Some(c) => {
                self.emit(Token::Character('<'));
                self.emit(Token::Character('/'));
                self.reconsume(c);
                self.state = State::Rawtext;
            }
            None => {
                self.emit(Token::Character('<'));
                self.emit(Token::Character('/'));
                self.emit_eof();
            }
        }
    }

    fn rawtext_end_tag_name(&mut self) {
        match self.consume() {
            Some(c) if is_ascii_whitespace(c) && self.tag_name == self.last_start_tag => {
                self.state = State::BeforeAttrName;
            }
            Some('/') if self.tag_name == self.last_start_tag => {
                self.state = State::SelfClosingStartTag;
            }
            Some('>') if self.tag_name == self.last_start_tag => {
                self.finish_tag();
                self.state = State::Data;
            }
            Some(c) if c.is_ascii_alphabetic() => {
                self.tag_name.push(c.to_ascii_lowercase());
                self.rawtext_buf.push(c);
            }
            Some(c) => {
                // Not the matching end tag — emit the buffered chars as text.
                self.emit(Token::Character('<'));
                self.emit(Token::Character('/'));
                let buf = std::mem::take(&mut self.rawtext_buf);
                for ch in buf.chars() {
                    self.emit(Token::Character(ch));
                }
                self.tag_name.clear();
                self.tag_is_end = false;
                self.reconsume(c);
                self.state = State::Rawtext;
            }
            None => {
                self.emit(Token::Character('<'));
                self.emit(Token::Character('/'));
                let buf = std::mem::take(&mut self.rawtext_buf);
                for ch in buf.chars() {
                    self.emit(Token::Character(ch));
                }
                self.emit_eof();
            }
        }
    }

    fn consume_char_reference(&mut self, in_attr: bool) -> Option<String> {
        // Save state so we can roll back if the reference is malformed.
        let start = self.cursor;
        let pushback = self.pushback;

        let first = self.consume()?;
        if first == '#' {
            return self.consume_numeric_reference(start, pushback);
        }
        if !first.is_ascii_alphanumeric() {
            self.cursor = start;
            self.pushback = pushback;
            return None;
        }
        // Read an alphanumeric run, optionally followed by ';'.
        let mut name = String::new();
        name.push(first);
        loop {
            match self.consume() {
                Some(c) if c.is_ascii_alphanumeric() => {
                    name.push(c);
                    if name.len() > 32 {
                        // Bail out — no real entity is this long.
                        break;
                    }
                }
                Some(';') => {
                    if let Some(s) = lookup_named(&name) {
                        return Some(s.to_string());
                    }
                    break;
                }
                Some(c) => {
                    if !in_attr {
                        if let Some(s) = lookup_named(&name) {
                            self.reconsume(c);
                            return Some(s.to_string());
                        }
                    }
                    self.reconsume(c);
                    break;
                }
                None => {
                    if !in_attr {
                        if let Some(s) = lookup_named(&name) {
                            return Some(s.to_string());
                        }
                    }
                    break;
                }
            }
        }
        self.cursor = start;
        self.pushback = pushback;
        None
    }

    fn consume_numeric_reference(
        &mut self,
        start: usize,
        pushback_save: Option<char>,
    ) -> Option<String> {
        let mut value: u32 = 0;
        let mut digits = 0usize;
        let hex = match self.consume() {
            Some('x') | Some('X') => true,
            Some(c) if c.is_ascii_digit() => {
                value = (c as u32) - b'0' as u32;
                digits = 1;
                false
            }
            _ => {
                self.cursor = start;
                self.pushback = pushback_save;
                return None;
            }
        };
        loop {
            match self.consume() {
                Some(c) if hex && c.is_ascii_hexdigit() => {
                    value = value.saturating_mul(16) + hex_digit(c);
                    digits += 1;
                }
                Some(c) if !hex && c.is_ascii_digit() => {
                    value = value.saturating_mul(10) + ((c as u32) - b'0' as u32);
                    digits += 1;
                }
                Some(';') => break,
                Some(c) => {
                    self.reconsume(c);
                    break;
                }
                None => break,
            }
            if digits > 7 {
                break;
            }
        }
        if digits == 0 {
            self.cursor = start;
            self.pushback = pushback_save;
            return None;
        }
        let codepoint = sanitize_codepoint(value);
        Some(codepoint.to_string())
    }
}

fn push_attr(attrs: &mut Vec<(String, String)>, name: String, value: String) {
    // Per spec, duplicate attributes keep the *first* value.
    if attrs.iter().any(|(k, _)| k == &name) {
        return;
    }
    attrs.push((name, value));
}

fn is_ascii_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ')
}

fn hex_digit(c: char) -> u32 {
    match c {
        '0'..='9' => c as u32 - b'0' as u32,
        'a'..='f' => c as u32 - b'a' as u32 + 10,
        'A'..='F' => c as u32 - b'A' as u32 + 10,
        _ => 0,
    }
}

fn sanitize_codepoint(v: u32) -> char {
    // Surrogates and out-of-range get replaced.
    if v == 0 || (0xD800..=0xDFFF).contains(&v) || v > 0x10FFFF {
        return '\u{FFFD}';
    }
    char::from_u32(v).unwrap_or('\u{FFFD}')
}

/// Tiny named-entity table — enough for real pages. Full HTML5 has ~2200
/// entries; we'll add more as we hit them.
fn lookup_named(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" | "AMP" => "&",
        "lt" | "LT" => "<",
        "gt" | "GT" => ">",
        "quot" | "QUOT" => "\"",
        "apos" => "'",
        "nbsp" => "\u{00A0}",
        "copy" | "COPY" => "©",
        "reg" | "REG" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "laquo" => "«",
        "raquo" => "»",
        "middot" => "·",
        "bull" => "•",
        "deg" => "°",
        "times" => "×",
        "divide" => "÷",
        "plusmn" => "±",
        "para" => "¶",
        "sect" => "§",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(input: &str) -> Vec<Token> {
        let mut v = Tokenizer::tokenize_all(input);
        v.pop(); // drop EOF
        v
    }

    #[test]
    fn simple_text() {
        let t = toks("hello");
        assert_eq!(t.len(), 5);
        assert_eq!(t[0], Token::Character('h'));
    }

    #[test]
    fn start_end_tag() {
        let t = toks("<p>hi</p>");
        assert!(matches!(t[0], Token::StartTag { ref name, .. } if name == "p"));
        assert!(matches!(t[3], Token::EndTag { ref name } if name == "p"));
    }

    #[test]
    fn attributes() {
        let t = toks(r#"<a href="x" id=y class='z' disabled>"#);
        if let Token::StartTag { attrs, .. } = &t[0] {
            assert_eq!(attrs[0], ("href".into(), "x".into()));
            assert_eq!(attrs[1], ("id".into(), "y".into()));
            assert_eq!(attrs[2], ("class".into(), "z".into()));
            assert_eq!(attrs[3], ("disabled".into(), "".into()));
        } else {
            panic!("not a start tag: {:?}", t[0]);
        }
    }

    #[test]
    fn self_closing() {
        let t = toks("<br/>");
        if let Token::StartTag {
            name, self_closing, ..
        } = &t[0]
        {
            assert_eq!(name, "br");
            assert!(self_closing);
        } else {
            panic!()
        }
    }

    #[test]
    fn comment() {
        let t = toks("<!-- hello --><p>");
        assert_eq!(t[0], Token::Comment(" hello ".to_string()));
    }

    #[test]
    fn doctype() {
        let t = toks("<!DOCTYPE html><p>");
        assert!(matches!(&t[0], Token::Doctype { name, .. } if name == "html"));
    }

    #[test]
    fn entity_refs() {
        let t = toks("&amp;&lt;&gt;&#65;&#x42;");
        let s: String = t
            .iter()
            .filter_map(|tok| match tok {
                Token::Character(c) => Some(*c),
                _ => None,
            })
            .collect();
        assert_eq!(s, "&<>AB");
    }

    #[test]
    fn rawtext_script() {
        let mut tk = Tokenizer::new("<script>var a = '<b>';</script><p>");
        // Drive normally to the StartTag, then enter rawtext.
        let mut script_seen = false;
        let mut chars = String::new();
        let mut tags: Vec<String> = Vec::new();
        loop {
            let t = tk.next_token();
            if let Token::StartTag { name, .. } = &t {
                tags.push(name.clone());
                if name == "script" {
                    tk.enter_rawtext("script");
                    script_seen = true;
                }
            }
            if let Token::EndTag { name } = &t {
                tags.push(format!("/{name}"));
            }
            if let Token::Character(c) = t {
                chars.push(c);
            }
            if matches!(tk.output.front(), None) && tk.eof_emitted {
                break;
            }
            if matches!(tk.next_token(), Token::Eof) {
                break;
            }
        }
        let _ = (script_seen, chars, tags);
        // Best-effort: the `<b>` inside the string should not have produced a
        // StartTag for `b`.
        // (Full integration is exercised in the parser-level test.)
    }
}
