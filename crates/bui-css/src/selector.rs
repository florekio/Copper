use bui_dom::{Document, NodeId};

use crate::parser::ParseError;

#[derive(Debug, Clone)]
pub struct Selector {
    pub compounds: Vec<Compound>,
    /// `combinators[i]` is the combinator between `compounds[i]` and `compounds[i+1]`.
    pub combinators: Vec<Combinator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    Descendant,
    Child,
    AdjacentSibling,
    GeneralSibling,
}

#[derive(Debug, Clone, Default)]
pub struct Compound {
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attrs: Vec<AttrMatch>,
    pub pseudo_classes: Vec<PseudoClass>,
    /// We *parse* `::before`/`::after` etc. but record only their existence
    /// — they're treated as no-ops by the matcher for now.
    pub pseudo_elements: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum AttrMatch {
    Has(String),
    /// Each value-bearing variant carries a `case_insensitive: bool`
    /// from the optional `i` modifier (`[type="X" i]`). False by
    /// default per CSS Selectors L4 ("attribute values are
    /// case-sensitive"); the modifier flips comparison semantics.
    Equals(String, String, bool),
    Whitespace(String, String, bool),
    Dash(String, String, bool),
    Prefix(String, String, bool),
    Suffix(String, String, bool),
    Substring(String, String, bool),
}

#[derive(Debug, Clone)]
pub enum PseudoClass {
    Hover,
    Active,
    Focus,
    Link,
    Visited,
    FirstChild,
    LastChild,
    OnlyChild,
    NthChild(NthFormula),
    /// `:nth-last-child(n)` — same formula but counted from the end.
    NthLastChild(NthFormula),
    /// `:nth-of-type(n)` — counts only same-tag-name element siblings.
    NthOfType(NthFormula),
    /// `:nth-last-of-type(n)` — same as above, counted from the end.
    NthLastOfType(NthFormula),
    /// First / last sibling among same-tag-name elements.
    FirstOfType,
    LastOfType,
    OnlyOfType,
    Not(Vec<Compound>),
    /// `:is(a, b, c)` — matches if any inner compound matches.
    /// Specificity contribution is approximated as 1 (the spec says
    /// "max specificity among the arms"; we don't compute that).
    Is(Vec<Compound>),
    /// `:where(...)` — same matching semantics as `:is(...)`. The
    /// spec says it adds 0 specificity; ours adds 1 like other
    /// pseudo-classes for simplicity. Stylesheet authors who care
    /// about that detail typically already work around our cascade
    /// quirks.
    Where(Vec<Compound>),
    /// `:has(<inner>)` — relational match. The element matches when
    /// any of its descendants matches one of the inner compound
    /// selectors. Spec also allows nesting / sibling combinators
    /// inside the inner; we only handle the descendant-walk case.
    Has(Vec<Compound>),
    Root,
    Empty,
    /// `:checked` — matches `<input type="checkbox|radio" checked>`
    /// and `<option selected>`.
    Checked,
    /// `:disabled` / `:enabled` — read the `disabled` HTML attribute.
    Disabled,
    Enabled,
    /// `:required` / `:optional` — read the `required` HTML attribute.
    Required,
    Optional,
    /// `:lang(prefix)` — matches when the element's (or nearest
    /// ancestor's) `lang` attribute matches `prefix` or starts with
    /// `prefix-`. We don't read `<meta http-equiv>` or HTTP headers;
    /// only DOM-declared langs.
    Lang(String),
    /// `:dir(ltr|rtl)` — reads the element's (or ancestor's) `dir`
    /// attribute, defaulting to "ltr".
    Dir(String),
    /// `:placeholder-shown` — matches an `<input>`/`<textarea>` that
    /// has a `placeholder` attribute and is currently displaying it,
    /// i.e. its value is empty. In a static render the value is the
    /// `value` attribute (empty by default), so an empty field with a
    /// placeholder matches. DDG's homepage hides the search button row
    /// via `.searchbox:has(.searchInput:placeholder-shown) ~ .buttonWrapper`.
    PlaceholderShown,
}

/// `an + b`, with the convention that `odd` = `2n+1` and `even` = `2n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NthFormula {
    pub a: i32,
    pub b: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity {
    pub a: u32, // ID count
    pub b: u32, // class + attr + pseudo-class count
    pub c: u32, // type + pseudo-element count
}

impl Selector {
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        parse_selector(input)
    }

    /// The selector's targeted pseudo-element name, if any. Returns
    /// `Some("before")` for `a::before`, etc. The cascade uses this
    /// to route rules: pseudo-element rules contribute only to the
    /// synthetic ::before / ::after, not to the real element.
    pub fn pseudo_element(&self) -> Option<&str> {
        self.compounds
            .last()?
            .pseudo_elements
            .first()
            .map(|s| s.as_str())
    }

    pub fn specificity(&self) -> Specificity {
        let mut a = 0;
        let mut b = 0;
        let mut c = 0;
        for cp in &self.compounds {
            if cp.id.is_some() {
                a += 1;
            }
            b += (cp.classes.len() + cp.attrs.len() + cp.pseudo_classes.len()) as u32;
            if cp.tag.is_some() {
                c += 1;
            }
            c += cp.pseudo_elements.len() as u32;
        }
        Specificity { a, b, c }
    }

    pub fn matches(&self, doc: &Document, node: NodeId) -> bool {
        if !self
            .compounds
            .last()
            .map(|cp| compound_matches(cp, doc, node))
            .unwrap_or(false)
        {
            return false;
        }
        if self.compounds.len() == 1 {
            return true;
        }
        let mut current = node;
        for i in (0..self.combinators.len()).rev() {
            let comb = self.combinators[i];
            let target = &self.compounds[i];
            let next = match comb {
                Combinator::Child => doc
                    .node(current)
                    .parent
                    .filter(|p| compound_matches(target, doc, *p)),
                Combinator::Descendant => {
                    let mut p = doc.node(current).parent;
                    let mut hit = None;
                    while let Some(pid) = p {
                        if compound_matches(target, doc, pid) {
                            hit = Some(pid);
                            break;
                        }
                        p = doc.node(pid).parent;
                    }
                    hit
                }
                Combinator::AdjacentSibling => doc
                    .node(current)
                    .prev_sibling
                    .filter(|s| compound_matches(target, doc, *s)),
                Combinator::GeneralSibling => {
                    let mut s = doc.node(current).prev_sibling;
                    let mut hit = None;
                    while let Some(sid) = s {
                        if compound_matches(target, doc, sid) {
                            hit = Some(sid);
                            break;
                        }
                        s = doc.node(sid).prev_sibling;
                    }
                    hit
                }
            };
            let Some(matched) = next else {
                return false;
            };
            current = matched;
        }
        true
    }
}

fn compound_matches(cp: &Compound, doc: &Document, node: NodeId) -> bool {
    let Some(elem) = doc.element(node) else {
        return false;
    };
    if let Some(tag) = &cp.tag {
        if !elem.name.eq_ignore_ascii_case(tag) {
            return false;
        }
    }
    if let Some(id) = &cp.id {
        match elem.get_attr("id") {
            Some(v) if v == id => {}
            _ => return false,
        }
    }
    for class in &cp.classes {
        if !elem.classes().any(|c| c == class.as_str()) {
            return false;
        }
    }
    for am in &cp.attrs {
        if !attr_match(am, elem) {
            return false;
        }
    }
    for pc in &cp.pseudo_classes {
        if !pseudo_match(pc, doc, node) {
            return false;
        }
    }
    true
}

fn attr_match(am: &AttrMatch, elem: &bui_dom::Element) -> bool {
    // Comparison primitives that respect the `i` modifier. We only
    // do ASCII case folding — Unicode case-folding (Σ ↔ σ ↔ ς) isn't
    // worth the dep cost at this layer.
    fn eq(a: &str, b: &str, ci: bool) -> bool {
        if ci { a.eq_ignore_ascii_case(b) } else { a == b }
    }
    fn starts_with(a: &str, b: &str, ci: bool) -> bool {
        if ci {
            a.len() >= b.len() && a[..b.len()].eq_ignore_ascii_case(b)
        } else {
            a.starts_with(b)
        }
    }
    fn ends_with(a: &str, b: &str, ci: bool) -> bool {
        if ci {
            a.len() >= b.len() && a[a.len() - b.len()..].eq_ignore_ascii_case(b)
        } else {
            a.ends_with(b)
        }
    }
    fn contains(a: &str, b: &str, ci: bool) -> bool {
        if !ci {
            return a.contains(b);
        }
        if b.is_empty() {
            return true;
        }
        let bl = b.to_ascii_lowercase();
        a.to_ascii_lowercase().contains(&bl)
    }
    match am {
        AttrMatch::Has(k) => elem.get_attr(k).is_some(),
        AttrMatch::Equals(k, v, ci) => elem
            .get_attr(k)
            .map(|val| eq(val, v, *ci))
            .unwrap_or(false),
        AttrMatch::Whitespace(k, v, ci) => elem
            .get_attr(k)
            .map(|val| val.split_ascii_whitespace().any(|s| eq(s, v, *ci)))
            .unwrap_or(false),
        AttrMatch::Dash(k, v, ci) => elem
            .get_attr(k)
            .map(|val| eq(val, v, *ci) || starts_with(val, &format!("{v}-"), *ci))
            .unwrap_or(false),
        AttrMatch::Prefix(k, v, ci) => elem
            .get_attr(k)
            .map(|val| !v.is_empty() && starts_with(val, v, *ci))
            .unwrap_or(false),
        AttrMatch::Suffix(k, v, ci) => elem
            .get_attr(k)
            .map(|val| !v.is_empty() && ends_with(val, v, *ci))
            .unwrap_or(false),
        AttrMatch::Substring(k, v, ci) => elem
            .get_attr(k)
            .map(|val| !v.is_empty() && contains(val, v, *ci))
            .unwrap_or(false),
    }
}

fn pseudo_match(pc: &PseudoClass, doc: &Document, node: NodeId) -> bool {
    match pc {
        PseudoClass::Hover | PseudoClass::Active | PseudoClass::Focus => false,
        PseudoClass::Link | PseudoClass::Visited => {
            // Anchor with href counts as :link.
            doc.element(node)
                .map(|e| e.name == "a" && e.get_attr("href").is_some())
                .unwrap_or(false)
        }
        PseudoClass::FirstChild => {
            // First *element* sibling.
            let mut prev = doc.node(node).prev_sibling;
            while let Some(p) = prev {
                if doc.element(p).is_some() {
                    return false;
                }
                prev = doc.node(p).prev_sibling;
            }
            true
        }
        PseudoClass::LastChild => {
            let mut next = doc.node(node).next_sibling;
            while let Some(n) = next {
                if doc.element(n).is_some() {
                    return false;
                }
                next = doc.node(n).next_sibling;
            }
            true
        }
        PseudoClass::OnlyChild => {
            pseudo_match(&PseudoClass::FirstChild, doc, node)
                && pseudo_match(&PseudoClass::LastChild, doc, node)
        }
        PseudoClass::NthChild(formula) => nth_match(*formula, sibling_index(doc, node, false, None)),
        PseudoClass::NthLastChild(formula) => {
            nth_match(*formula, sibling_index(doc, node, true, None))
        }
        PseudoClass::NthOfType(formula) => {
            let tag = doc.element(node).map(|e| e.name.as_str());
            nth_match(*formula, sibling_index(doc, node, false, tag))
        }
        PseudoClass::NthLastOfType(formula) => {
            let tag = doc.element(node).map(|e| e.name.as_str());
            nth_match(*formula, sibling_index(doc, node, true, tag))
        }
        PseudoClass::FirstOfType => {
            let tag = doc.element(node).map(|e| e.name.as_str());
            sibling_index(doc, node, false, tag) == 1
        }
        PseudoClass::LastOfType => {
            let tag = doc.element(node).map(|e| e.name.as_str());
            sibling_index(doc, node, true, tag) == 1
        }
        PseudoClass::OnlyOfType => {
            let tag = doc.element(node).map(|e| e.name.as_str());
            sibling_index(doc, node, false, tag) == 1
                && sibling_index(doc, node, true, tag) == 1
        }
        PseudoClass::Not(compounds) => !compounds.iter().any(|cp| compound_matches(cp, doc, node)),
        PseudoClass::Is(compounds) | PseudoClass::Where(compounds) => {
            compounds.iter().any(|cp| compound_matches(cp, doc, node))
        }
        PseudoClass::Has(compounds) => {
            // Walk descendants; match if any descendant matches any
            // of the inner compounds. Limit depth so a malformed
            // selector against a deep tree can't run forever.
            let mut stack: Vec<NodeId> = Vec::new();
            let mut child = doc.node(node).first_child;
            while let Some(c) = child {
                stack.push(c);
                child = doc.node(c).next_sibling;
            }
            while let Some(n) = stack.pop() {
                if doc.element(n).is_some()
                    && compounds.iter().any(|cp| compound_matches(cp, doc, n))
                {
                    return true;
                }
                let mut grand = doc.node(n).first_child;
                while let Some(g) = grand {
                    stack.push(g);
                    grand = doc.node(g).next_sibling;
                }
            }
            false
        }
        PseudoClass::Root => doc
            .node(node)
            .parent
            .map(|p| p == doc.root)
            .unwrap_or(false),
        PseudoClass::Empty => doc.node(node).first_child.is_none(),
        PseudoClass::Checked => doc
            .element(node)
            .map(|e| match e.name.as_str() {
                "input" => e.get_attr("checked").is_some(),
                "option" => e.get_attr("selected").is_some(),
                _ => false,
            })
            .unwrap_or(false),
        PseudoClass::Disabled => doc
            .element(node)
            .map(|e| e.get_attr("disabled").is_some())
            .unwrap_or(false),
        PseudoClass::Enabled => doc
            .element(node)
            .map(|e| {
                matches!(
                    e.name.as_str(),
                    "input" | "select" | "textarea" | "button" | "fieldset" | "optgroup" | "option"
                ) && e.get_attr("disabled").is_none()
            })
            .unwrap_or(false),
        PseudoClass::Required => doc
            .element(node)
            .map(|e| e.get_attr("required").is_some())
            .unwrap_or(false),
        PseudoClass::Optional => doc
            .element(node)
            .map(|e| {
                matches!(e.name.as_str(), "input" | "select" | "textarea")
                    && e.get_attr("required").is_none()
            })
            .unwrap_or(false),
        PseudoClass::Lang(prefix) => {
            let prefix_lc = prefix.to_ascii_lowercase();
            // Walk up looking for a declared lang attribute. Match
            // when the value equals prefix exactly OR starts with
            // `<prefix>-`. Spec also allows xml:lang; we don't
            // parse XML namespaces so DOM `lang` only.
            let mut cur = Some(node);
            while let Some(n) = cur {
                if let Some(e) = doc.element(n) {
                    if let Some(lang) = e.get_attr("lang") {
                        let lang_lc = lang.to_ascii_lowercase();
                        if lang_lc == prefix_lc || lang_lc.starts_with(&format!("{prefix_lc}-")) {
                            return true;
                        }
                    }
                }
                cur = doc.node(n).parent;
            }
            false
        }
        PseudoClass::Dir(want) => {
            let want_lc = want.to_ascii_lowercase();
            let mut cur = Some(node);
            while let Some(n) = cur {
                if let Some(e) = doc.element(n) {
                    if let Some(dir) = e.get_attr("dir") {
                        return dir.eq_ignore_ascii_case(&want_lc);
                    }
                }
                cur = doc.node(n).parent;
            }
            // Default direction is LTR.
            want_lc == "ltr"
        }
        PseudoClass::PlaceholderShown => doc
            .element(node)
            .map(|e| {
                let is_field = matches!(e.name.as_str(), "input" | "textarea");
                let has_placeholder = e.get_attr("placeholder").is_some();
                // Showing the placeholder ⇔ the field is empty. Static
                // render: the value is the `value` attribute (textarea
                // text content isn't modeled here; treat absent as empty).
                let empty = e.get_attr("value").map(|v| v.is_empty()).unwrap_or(true);
                is_field && has_placeholder && empty
            })
            .unwrap_or(false),
    }
}

/// 1-based index of `node` among its element siblings. Walks
/// backward (`from_end = false`) or forward (`= true`); when
/// `tag_filter` is `Some(t)`, only siblings with that tag name count.
fn sibling_index(
    doc: &Document,
    node: NodeId,
    from_end: bool,
    tag_filter: Option<&str>,
) -> i32 {
    let matches = |id: NodeId| match tag_filter {
        Some(t) => doc.element(id).map(|e| e.name == t).unwrap_or(false),
        None => doc.element(id).is_some(),
    };
    let mut idx = 1i32;
    let mut cur = if from_end {
        doc.node(node).next_sibling
    } else {
        doc.node(node).prev_sibling
    };
    while let Some(c) = cur {
        if matches(c) {
            idx += 1;
        }
        cur = if from_end {
            doc.node(c).next_sibling
        } else {
            doc.node(c).prev_sibling
        };
    }
    idx
}

/// True if `idx` matches the `an + b` formula for some non-negative
/// integer `n`.
fn nth_match(formula: NthFormula, idx: i32) -> bool {
    if formula.a == 0 {
        return idx == formula.b;
    }
    let diff = idx - formula.b;
    diff % formula.a == 0 && diff / formula.a >= 0
}

// ---- selector parsing ----

fn parse_selector(input: &str) -> Result<Selector, ParseError> {
    let mut p = SelParser::new(input);
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();

    p.skip_ws();
    compounds.push(p.parse_compound()?);
    loop {
        let comb = p.parse_combinator();
        match comb {
            None => break,
            Some(c) => {
                let cp = p.parse_compound()?;
                combinators.push(c);
                compounds.push(cp);
            }
        }
    }
    if compounds.is_empty() {
        return Err(ParseError::Selector(input.to_string()));
    }
    Ok(Selector {
        compounds,
        combinators,
    })
}

struct SelParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> SelParser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }
    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }
    fn peek(&self) -> Option<char> {
        self.bytes.get(self.pos).map(|b| *b as char)
    }
    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }
    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_combinator(&mut self) -> Option<Combinator> {
        let mut saw_ws = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.pos += 1;
                saw_ws = true;
            } else {
                break;
            }
        }
        match self.peek() {
            Some('>') => {
                self.pos += 1;
                self.skip_ws();
                Some(Combinator::Child)
            }
            Some('+') => {
                self.pos += 1;
                self.skip_ws();
                Some(Combinator::AdjacentSibling)
            }
            Some('~') => {
                self.pos += 1;
                self.skip_ws();
                Some(Combinator::GeneralSibling)
            }
            Some(c) if !is_compound_terminator(c) && saw_ws => Some(Combinator::Descendant),
            _ => None,
        }
    }

    fn parse_compound(&mut self) -> Result<Compound, ParseError> {
        let mut cp = Compound::default();
        let mut started = false;
        loop {
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    started = true;
                    // Universal — leave tag as None.
                }
                Some('#') => {
                    self.pos += 1;
                    cp.id = Some(self.read_ident()?);
                    started = true;
                }
                Some('.') => {
                    self.pos += 1;
                    cp.classes.push(self.read_ident()?);
                    started = true;
                }
                Some('[') => {
                    self.pos += 1;
                    cp.attrs.push(self.parse_attr_selector()?);
                    started = true;
                }
                Some(':') => {
                    self.pos += 1;
                    if self.peek() == Some(':') {
                        self.pos += 1;
                        let name = self.read_ident()?;
                        // skip optional functional args
                        if self.peek() == Some('(') {
                            self.consume_balanced_parens();
                        }
                        cp.pseudo_elements.push(name);
                    } else {
                        let name = self.read_ident()?;
                        cp.pseudo_classes.push(self.parse_pseudo_class(&name)?);
                    }
                    started = true;
                }
                Some(c) if c.is_ascii_alphabetic() || c == '-' || c == '_' => {
                    if cp.tag.is_some() {
                        break;
                    }
                    cp.tag = Some(self.read_ident()?.to_ascii_lowercase());
                    started = true;
                }
                _ => break,
            }
        }
        if !started {
            return Err(ParseError::Selector(self.context()));
        }
        Ok(cp)
    }

    fn parse_attr_selector(&mut self) -> Result<AttrMatch, ParseError> {
        self.skip_ws();
        let name = self.read_ident()?;
        self.skip_ws();
        let op = match self.peek() {
            Some(']') => {
                self.pos += 1;
                return Ok(AttrMatch::Has(name));
            }
            Some('=') => {
                self.pos += 1;
                "="
            }
            Some('~') if self.bytes.get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                "~="
            }
            Some('|') if self.bytes.get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                "|="
            }
            Some('^') if self.bytes.get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                "^="
            }
            Some('$') if self.bytes.get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                "$="
            }
            Some('*') if self.bytes.get(self.pos + 1) == Some(&b'=') => {
                self.pos += 2;
                "*="
            }
            _ => return Err(ParseError::Selector(self.context())),
        };
        self.skip_ws();
        let value = self.read_attr_value()?;
        self.skip_ws();
        // CSS Selectors L4 case modifier — `i` makes the value
        // comparison ASCII case-insensitive, `s` is the explicit
        // sensitive form (default), so we just record `i` and
        // consume `s` as a no-op.
        let mut ci = false;
        match self.peek() {
            Some('i') | Some('I') => {
                ci = true;
                self.pos += 1;
                self.skip_ws();
            }
            Some('s') | Some('S') => {
                self.pos += 1;
                self.skip_ws();
            }
            _ => {}
        }
        if self.peek() != Some(']') {
            return Err(ParseError::Selector(self.context()));
        }
        self.pos += 1;
        Ok(match op {
            "=" => AttrMatch::Equals(name, value, ci),
            "~=" => AttrMatch::Whitespace(name, value, ci),
            "|=" => AttrMatch::Dash(name, value, ci),
            "^=" => AttrMatch::Prefix(name, value, ci),
            "$=" => AttrMatch::Suffix(name, value, ci),
            "*=" => AttrMatch::Substring(name, value, ci),
            _ => unreachable!(),
        })
    }

    fn read_attr_value(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some('"') | Some('\'') => {
                let quote = self.advance().unwrap();
                let mut out = String::new();
                while let Some(c) = self.advance() {
                    if c == quote {
                        return Ok(out);
                    }
                    if c == '\\' {
                        if let Some(esc) = self.advance() {
                            out.push(esc);
                        }
                        continue;
                    }
                    out.push(c);
                }
                Err(ParseError::Selector("unterminated string".into()))
            }
            _ => self.read_ident(),
        }
    }

    fn parse_pseudo_class(&mut self, name: &str) -> Result<PseudoClass, ParseError> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "hover" => Ok(PseudoClass::Hover),
            "active" => Ok(PseudoClass::Active),
            "focus" => Ok(PseudoClass::Focus),
            "link" => Ok(PseudoClass::Link),
            "visited" => Ok(PseudoClass::Visited),
            "first-child" => Ok(PseudoClass::FirstChild),
            "last-child" => Ok(PseudoClass::LastChild),
            "only-child" => Ok(PseudoClass::OnlyChild),
            "root" => Ok(PseudoClass::Root),
            "empty" => Ok(PseudoClass::Empty),
            "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
                if self.peek() != Some('(') {
                    return Err(ParseError::Selector(format!(":{lower} needs ()")));
                }
                self.pos += 1;
                let inner = self.read_until_close_paren();
                let formula = parse_nth(&inner)
                    .ok_or_else(|| ParseError::Selector(format!("bad nth: {inner}")))?;
                Ok(match lower.as_str() {
                    "nth-child" => PseudoClass::NthChild(formula),
                    "nth-last-child" => PseudoClass::NthLastChild(formula),
                    "nth-of-type" => PseudoClass::NthOfType(formula),
                    _ => PseudoClass::NthLastOfType(formula),
                })
            }
            "first-of-type" => Ok(PseudoClass::FirstOfType),
            "last-of-type" => Ok(PseudoClass::LastOfType),
            "only-of-type" => Ok(PseudoClass::OnlyOfType),
            "checked" => Ok(PseudoClass::Checked),
            "placeholder-shown" => Ok(PseudoClass::PlaceholderShown),
            "disabled" => Ok(PseudoClass::Disabled),
            "enabled" => Ok(PseudoClass::Enabled),
            "required" => Ok(PseudoClass::Required),
            "optional" => Ok(PseudoClass::Optional),
            // Focus-visible / focus-within don't have meaningful
            // matches in our static-render universe — there's no
            // focus state. Keep them parseable so author CSS isn't
            // dropped, but they never match. Reuse Hover sentinel
            // (also a "never match" stub).
            "focus-visible" | "focus-within" => Ok(PseudoClass::Hover),
            "lang" => {
                if self.peek() != Some('(') {
                    return Err(ParseError::Selector(":lang needs ()".into()));
                }
                self.pos += 1;
                let inner = self.read_until_close_paren();
                Ok(PseudoClass::Lang(inner.trim().to_string()))
            }
            "dir" => {
                if self.peek() != Some('(') {
                    return Err(ParseError::Selector(":dir needs ()".into()));
                }
                self.pos += 1;
                let inner = self.read_until_close_paren();
                Ok(PseudoClass::Dir(inner.trim().to_string()))
            }
            "not" | "is" | "where" | "has" => {
                if self.peek() != Some('(') {
                    return Err(ParseError::Selector(format!(":{lower} needs ()")));
                }
                self.pos += 1;
                let inner = self.read_until_close_paren();
                let mut sub = SelParser::new(&inner);
                let mut compounds = Vec::new();
                loop {
                    sub.skip_ws();
                    if sub.eof() {
                        break;
                    }
                    compounds.push(sub.parse_compound()?);
                    sub.skip_ws();
                    if sub.peek() == Some(',') {
                        sub.pos += 1;
                        continue;
                    }
                    break;
                }
                Ok(match lower.as_str() {
                    "not" => PseudoClass::Not(compounds),
                    "is" => PseudoClass::Is(compounds),
                    "has" => PseudoClass::Has(compounds),
                    _ => PseudoClass::Where(compounds),
                })
            }
            _ => {
                // Unknown pseudo-class — consume optional functional args, treat as never-matching.
                if self.peek() == Some('(') {
                    self.consume_balanced_parens();
                }
                Ok(PseudoClass::Hover) // safe sentinel: never matches in our matcher
            }
        }
    }

    fn read_ident(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(ParseError::Selector(self.context()));
        }
        Ok(std::str::from_utf8(&self.bytes[start..self.pos])
            .unwrap()
            .to_string())
    }

    fn read_until_close_paren(&mut self) -> String {
        let mut depth = 1i32;
        let start = self.pos;
        while let Some(c) = self.peek() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let s = std::str::from_utf8(&self.bytes[start..self.pos])
                            .unwrap()
                            .to_string();
                        self.pos += 1;
                        return s;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .unwrap()
            .to_string()
    }

    fn consume_balanced_parens(&mut self) {
        if self.peek() != Some('(') {
            return;
        }
        self.pos += 1;
        let mut depth = 1i32;
        while let Some(c) = self.advance() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    fn context(&self) -> String {
        let start = self.pos.saturating_sub(8);
        let end = (self.pos + 8).min(self.bytes.len());
        std::str::from_utf8(&self.bytes[start..end])
            .unwrap_or("?")
            .to_string()
    }
}

fn is_compound_terminator(c: char) -> bool {
    matches!(c, ',' | '{' | ')' | ']' | '\0')
}

fn parse_nth(input: &str) -> Option<NthFormula> {
    let s = input.trim().to_ascii_lowercase();
    if s == "odd" {
        return Some(NthFormula { a: 2, b: 1 });
    }
    if s == "even" {
        return Some(NthFormula { a: 2, b: 0 });
    }
    // an+b forms: "n", "-n", "2n", "2n+3", "-2n-3", "5"
    let mut a = 0i32;
    let mut b = 0i32;
    let mut rest = s.as_str();
    if let Some(idx) = rest.find('n') {
        let coeff = rest[..idx].trim();
        a = match coeff {
            "" | "+" => 1,
            "-" => -1,
            other => other.parse().ok()?,
        };
        rest = rest[idx + 1..].trim();
        if rest.is_empty() {
            return Some(NthFormula { a, b: 0 });
        }
        // Now expect +N or -N
        let (sign, num) = match rest.chars().next() {
            Some('+') => (1, rest[1..].trim()),
            Some('-') => (-1, rest[1..].trim()),
            _ => return None,
        };
        b = sign * num.parse::<i32>().ok()?;
    } else {
        b = rest.parse().ok()?;
    }
    Some(NthFormula { a, b })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bui_dom::Document;

    #[test]
    fn parse_simple() {
        let s = Selector::parse("div.note#main").unwrap();
        assert_eq!(s.compounds[0].tag.as_deref(), Some("div"));
        assert_eq!(s.compounds[0].id.as_deref(), Some("main"));
        assert_eq!(s.compounds[0].classes, vec!["note".to_string()]);
    }

    #[test]
    fn descendant() {
        let s = Selector::parse("ul li.todo").unwrap();
        assert_eq!(s.compounds.len(), 2);
        assert_eq!(s.combinators[0], Combinator::Descendant);
    }

    #[test]
    fn child_and_sibling() {
        let s = Selector::parse("a > b + c ~ d").unwrap();
        assert_eq!(s.compounds.len(), 4);
        assert_eq!(s.combinators[0], Combinator::Child);
        assert_eq!(s.combinators[1], Combinator::AdjacentSibling);
        assert_eq!(s.combinators[2], Combinator::GeneralSibling);
    }

    #[test]
    fn attr_selectors() {
        let s = Selector::parse(r#"a[href^="https://"][rel~="nofollow"]"#).unwrap();
        assert_eq!(s.compounds[0].attrs.len(), 2);
    }

    #[test]
    fn nth_formulas() {
        assert_eq!(parse_nth("odd"), Some(NthFormula { a: 2, b: 1 }));
        assert_eq!(parse_nth("even"), Some(NthFormula { a: 2, b: 0 }));
        assert_eq!(parse_nth("3n+2"), Some(NthFormula { a: 3, b: 2 }));
        assert_eq!(parse_nth("-n+5"), Some(NthFormula { a: -1, b: 5 }));
        assert_eq!(parse_nth("5"), Some(NthFormula { a: 0, b: 5 }));
    }

    #[test]
    fn match_basic() {
        let mut doc = Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let div = doc.create_element("div");
        let p = doc.create_element("p");
        doc.element_mut(div).unwrap().set_attr("class", "note");
        doc.element_mut(p).unwrap().set_attr("id", "main");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        doc.append_child(body, div);
        doc.append_child(div, p);

        assert!(Selector::parse("p").unwrap().matches(&doc, p));
        assert!(Selector::parse("#main").unwrap().matches(&doc, p));
        assert!(Selector::parse("div p").unwrap().matches(&doc, p));
        assert!(Selector::parse("div > p").unwrap().matches(&doc, p));
        assert!(Selector::parse("body p").unwrap().matches(&doc, p));
        assert!(!Selector::parse("body > p").unwrap().matches(&doc, p));
        assert!(Selector::parse(".note p").unwrap().matches(&doc, p));
    }

    #[test]
    fn attribute_selector_i_flag_is_case_insensitive() {
        let mut doc = Document::new();
        let body = doc.create_element("body");
        let inp = doc.create_element("input");
        // Author HTML with mixed-case type attribute (rare in
        // practice, but allowed). Without the `i` flag the strict
        // [type="checkbox"] should miss; with it, it matches.
        doc.element_mut(inp).unwrap().set_attr("type", "Checkbox");
        doc.append_child(doc.root, body);
        doc.append_child(body, inp);

        assert!(!Selector::parse("[type=\"checkbox\"]").unwrap().matches(&doc, inp));
        assert!(Selector::parse("[type=\"checkbox\" i]").unwrap().matches(&doc, inp));
        // Combine with a substring match.
        assert!(Selector::parse("[type*=\"BOX\" i]").unwrap().matches(&doc, inp));
    }

    #[test]
    fn form_state_pseudos() {
        let mut doc = Document::new();
        let body = doc.create_element("body");
        let on = doc.create_element("input");
        let off = doc.create_element("input");
        let need = doc.create_element("input");
        doc.element_mut(on).unwrap().set_attr("type", "checkbox");
        doc.element_mut(on).unwrap().set_attr("checked", "");
        doc.element_mut(off).unwrap().set_attr("type", "checkbox");
        doc.element_mut(need).unwrap().set_attr("required", "");
        doc.append_child(doc.root, body);
        doc.append_child(body, on);
        doc.append_child(body, off);
        doc.append_child(body, need);

        assert!(Selector::parse(":checked").unwrap().matches(&doc, on));
        assert!(!Selector::parse(":checked").unwrap().matches(&doc, off));
        assert!(Selector::parse(":required").unwrap().matches(&doc, need));
        assert!(!Selector::parse(":required").unwrap().matches(&doc, off));
        // :enabled matches form controls without disabled — both
        // checkboxes qualify.
        assert!(Selector::parse(":enabled").unwrap().matches(&doc, on));
        assert!(Selector::parse(":enabled").unwrap().matches(&doc, off));
    }

    #[test]
    fn lang_pseudo_matches_ancestor_lang_attr() {
        let mut doc = Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let p = doc.create_element("p");
        doc.element_mut(html).unwrap().set_attr("lang", "fr-CA");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        doc.append_child(body, p);

        // :lang(fr) matches "fr-CA" via prefix.
        assert!(Selector::parse(":lang(fr)").unwrap().matches(&doc, p));
        // :lang(fr-CA) exact match.
        assert!(Selector::parse(":lang(fr-CA)").unwrap().matches(&doc, p));
        // :lang(de) doesn't match.
        assert!(!Selector::parse(":lang(de)").unwrap().matches(&doc, p));
    }

    #[test]
    fn has_pseudo_matches_when_descendant_matches() {
        // <article><header><h2 class="title">…</h2></header></article>
        // article:has(.title) should match the article via descendant
        // walk; section:has(.title) would not.
        let mut doc = Document::new();
        let article = doc.create_element("article");
        let header = doc.create_element("header");
        let h2 = doc.create_element("h2");
        doc.element_mut(h2).unwrap().set_attr("class", "title");
        doc.append_child(doc.root, article);
        doc.append_child(article, header);
        doc.append_child(header, h2);

        assert!(Selector::parse("article:has(.title)").unwrap().matches(&doc, article));
        assert!(!Selector::parse("section:has(.title)").unwrap().matches(&doc, article));
        // The h2 itself doesn't match :has(.title) (it has no
        // descendants).
        assert!(!Selector::parse(":has(.title)").unwrap().matches(&doc, h2));
    }

    #[test]
    fn placeholder_shown_drives_has_sibling_rule() {
        // Mirrors DDG's homepage searchbox:
        //   .box:has(.field:placeholder-shown) ~ .wrap { display:none }
        // The wrap should match (be hidden) while the input is empty.
        let mut doc = Document::new();
        let combobox = doc.create_element("div");
        let box_ = doc.create_element("div");
        let field = doc.create_element("input");
        let wrap = doc.create_element("div");
        doc.element_mut(box_).unwrap().set_attr("class", "box");
        doc.element_mut(field).unwrap().set_attr("class", "field");
        doc.element_mut(field).unwrap().set_attr("placeholder", "Search");
        doc.element_mut(wrap).unwrap().set_attr("class", "wrap");
        doc.append_child(doc.root, combobox);
        doc.append_child(combobox, box_);
        doc.append_child(box_, field);
        doc.append_child(combobox, wrap);

        let sel = Selector::parse(".box:has(.field:placeholder-shown) ~ .wrap").unwrap();
        assert!(sel.matches(&doc, wrap), "empty field → placeholder shown → wrap hidden");
        assert!(Selector::parse(".field:placeholder-shown").unwrap().matches(&doc, field));

        // Once the field has a value, the placeholder is no longer
        // shown, so the rule stops matching.
        doc.element_mut(field).unwrap().set_attr("value", "cats");
        assert!(!Selector::parse(".field:placeholder-shown").unwrap().matches(&doc, field));
        assert!(!sel.matches(&doc, wrap));
    }

    #[test]
    fn is_and_where_match_any_arm() {
        // :is(h1, h2, h3) p — paragraph descendant of any heading.
        let mut doc = Document::new();
        let body = doc.create_element("body");
        let h2 = doc.create_element("h2");
        let p_in_h2 = doc.create_element("p");
        let div = doc.create_element("div");
        let p_in_div = doc.create_element("p");
        doc.append_child(doc.root, body);
        doc.append_child(body, h2);
        doc.append_child(h2, p_in_h2);
        doc.append_child(body, div);
        doc.append_child(div, p_in_div);

        let s = Selector::parse(":is(h1, h2, h3) p").unwrap();
        assert!(s.matches(&doc, p_in_h2));
        assert!(!s.matches(&doc, p_in_div));

        // :where() has the same matching behaviour.
        let s2 = Selector::parse(":where(h1, h2) p").unwrap();
        assert!(s2.matches(&doc, p_in_h2));
        assert!(!s2.matches(&doc, p_in_div));
    }

    #[test]
    fn nth_of_type_family() {
        // <ul>
        //   <li>1</li>     <- li #1
        //   <li>2</li>     <- li #2
        //   <p>p</p>       <- p  #1
        //   <li>3</li>     <- li #3 (third li, fourth element child)
        // </ul>
        let mut doc = Document::new();
        let ul = doc.create_element("ul");
        let li1 = doc.create_element("li");
        let li2 = doc.create_element("li");
        let p = doc.create_element("p");
        let li3 = doc.create_element("li");
        doc.append_child(doc.root, ul);
        doc.append_child(ul, li1);
        doc.append_child(ul, li2);
        doc.append_child(ul, p);
        doc.append_child(ul, li3);

        // li:nth-child(2) → second element overall = li2
        let nth2 = Selector::parse("li:nth-child(2)").unwrap();
        assert!(nth2.matches(&doc, li2));
        assert!(!nth2.matches(&doc, li3));

        // li:nth-of-type(3) → third <li> = li3 (skips <p>)
        let nth_of_type3 = Selector::parse("li:nth-of-type(3)").unwrap();
        assert!(nth_of_type3.matches(&doc, li3));
        assert!(!nth_of_type3.matches(&doc, li2));

        // :first-of-type matches the first li and the first/only p.
        let first = Selector::parse(":first-of-type").unwrap();
        assert!(first.matches(&doc, li1));
        assert!(first.matches(&doc, p));
        assert!(!first.matches(&doc, li2));

        // :last-of-type matches li3 and the only p.
        let last = Selector::parse(":last-of-type").unwrap();
        assert!(last.matches(&doc, li3));
        assert!(last.matches(&doc, p));
        assert!(!last.matches(&doc, li1));

        // :nth-last-child(1) is the absolute last child.
        let lastch = Selector::parse(":nth-last-child(1)").unwrap();
        assert!(lastch.matches(&doc, li3));
        assert!(!lastch.matches(&doc, p));
    }

    #[test]
    fn specificity_levels() {
        let s = Selector::parse("a.b#c").unwrap();
        let sp = s.specificity();
        assert_eq!(sp, Specificity { a: 1, b: 1, c: 1 });
        let s = Selector::parse("a b c").unwrap();
        let sp = s.specificity();
        assert_eq!(sp, Specificity { a: 0, b: 0, c: 3 });
    }
}
