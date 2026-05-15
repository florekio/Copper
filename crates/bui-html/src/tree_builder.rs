use bui_dom::{Document, NodeId, NodeKind};

use crate::tokenizer::{Token, Tokenizer};

/// Parse an HTML string into a `Document`.
///
/// Pragmatic Phase-2 strategy: stack-based insertion with implicit-close
/// rules for the common tags. We don't run the formatting-element adoption
/// agency, don't foster-parent out of tables, and don't run the full
/// insertion-mode automaton — but real pages mostly come through cleanly.
pub fn parse(input: &str) -> Document {
    let mut doc = Document::new();
    let mut t = Tokenizer::new(input);
    let mut stack: Vec<NodeId> = vec![doc.root];

    loop {
        let tok = t.next_token();
        match tok {
            Token::Eof => break,
            Token::Doctype {
                name,
                public_id,
                system_id,
            } => {
                if let Some(&top) = stack.last() {
                    if top == doc.root {
                        let dt = doc.create_doctype(&name, &public_id, &system_id);
                        doc.append_child(doc.root, dt);
                    }
                }
            }
            Token::Comment(c) => {
                let id = doc.create_comment(&c);
                let parent = *stack.last().unwrap_or(&doc.root);
                doc.append_child(parent, id);
            }
            Token::Character(ch) => insert_char(&mut doc, &stack, ch),
            Token::StartTag {
                name,
                attrs,
                self_closing,
            } => {
                handle_implicit_close(&doc, &mut stack, &name);

                let id = doc.create_element(&name);
                if let Some(elem) = doc.element_mut(id) {
                    elem.attrs = attrs;
                }
                let parent = *stack.last().unwrap_or(&doc.root);
                doc.append_child(parent, id);

                if !is_void(&name) && !self_closing {
                    stack.push(id);
                    if is_rawtext(&name) {
                        t.enter_rawtext(&name);
                    }
                }
            }
            Token::EndTag { name } => {
                if is_void(&name) {
                    continue;
                }
                if let Some(idx) = stack.iter().rposition(|nid| {
                    doc.element(*nid).map(|e| e.name == name).unwrap_or(false)
                }) {
                    stack.truncate(idx);
                }
            }
        }
    }

    doc
}

fn insert_char(doc: &mut Document, stack: &[NodeId], ch: char) {
    let parent = *stack.last().unwrap_or(&doc.root);
    if let Some(last) = doc.node(parent).last_child {
        if let NodeKind::Text(s) = &mut doc.node_mut(last).kind {
            s.push(ch);
            return;
        }
    }
    let id = doc.create_text(&ch.to_string());
    doc.append_child(parent, id);
}

fn handle_implicit_close(doc: &Document, stack: &mut Vec<NodeId>, new_tag: &str) {
    let top_name = |stack: &Vec<NodeId>| -> Option<String> {
        stack
            .last()
            .and_then(|id| doc.element(*id).map(|e| e.name.clone()))
    };

    // Block-level elements close any open <p>.
    if new_tag == "p" || is_block_level(new_tag) {
        while let Some(name) = top_name(stack) {
            if name == "p" {
                stack.pop();
                continue;
            }
            break;
        }
    }

    match new_tag {
        "li" => close_if_top(doc, stack, &["li"]),
        "dt" | "dd" => close_if_top(doc, stack, &["dt", "dd"]),
        "tr" => close_if_top(doc, stack, &["td", "th", "tr"]),
        "td" | "th" => close_if_top(doc, stack, &["td", "th"]),
        "thead" | "tbody" | "tfoot" => close_if_top(doc, stack, &["thead", "tbody", "tfoot"]),
        "option" => close_if_top(doc, stack, &["option"]),
        "optgroup" => close_if_top(doc, stack, &["option", "optgroup"]),
        _ => {}
    }
}

fn close_if_top(doc: &Document, stack: &mut Vec<NodeId>, names: &[&str]) {
    while let Some(&top) = stack.last() {
        let name = doc.element(top).map(|e| e.name.as_str()).unwrap_or("");
        if names.contains(&name) {
            stack.pop();
        } else {
            break;
        }
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_rawtext(name: &str) -> bool {
    matches!(name, "script" | "style" | "textarea" | "title")
}

fn is_block_level(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "details"
            | "dialog"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "main"
            | "nav"
            | "ol"
            | "pre"
            | "section"
            | "table"
            | "ul"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_page() {
        let html = r#"<!DOCTYPE html><html><head><title>T</title></head><body><h1>Hello</h1><p>World</p></body></html>"#;
        let doc = parse(html);
        let printed = doc.pretty_print();
        assert!(printed.contains("<html>"));
        assert!(printed.contains("<head>"));
        assert!(printed.contains("<title>"));
        assert!(printed.contains("<h1>"));
        assert!(printed.contains("\"Hello\""));
        assert!(printed.contains("\"World\""));
    }

    #[test]
    fn implicit_close_p() {
        let html = "<p>a<p>b<p>c";
        let doc = parse(html);
        // Three sibling <p> elements.
        let count = doc
            .descendants(doc.root)
            .filter(|id| {
                doc.element(*id).map(|e| e.name == "p").unwrap_or(false)
            })
            .count();
        assert_eq!(count, 3);
    }

    #[test]
    fn implicit_close_li() {
        let html = "<ul><li>a<li>b<li>c</ul>";
        let doc = parse(html);
        let count = doc
            .descendants(doc.root)
            .filter(|id| doc.element(*id).map(|e| e.name == "li").unwrap_or(false))
            .count();
        assert_eq!(count, 3);
    }

    #[test]
    fn void_elements() {
        let html = r#"<img src="x.png"><br><hr>"#;
        let doc = parse(html);
        let names: Vec<_> = doc
            .descendants(doc.root)
            .filter_map(|id| doc.element(id).map(|e| e.name.clone()))
            .collect();
        assert_eq!(names, vec!["img", "br", "hr"]);
    }

    #[test]
    fn rawtext_script() {
        let html = r#"<script>var s = "<b>not a tag</b>";</script><p>after</p>"#;
        let doc = parse(html);
        let names: Vec<_> = doc
            .descendants(doc.root)
            .filter_map(|id| doc.element(id).map(|e| e.name.clone()))
            .collect();
        assert!(names.contains(&"script".to_string()));
        assert!(names.contains(&"p".to_string()));
        // No <b> element should have been created from inside the string.
        assert!(!names.contains(&"b".to_string()));
    }

    #[test]
    fn entity_decoding() {
        let doc = parse("<p>foo &amp; bar &lt; baz</p>");
        let printed = doc.pretty_print();
        assert!(printed.contains("\"foo & bar < baz\""), "{printed}");
    }

    #[test]
    fn comment_stripped_from_text() {
        let doc = parse("<p>a<!--ignored-->b</p>");
        let printed = doc.pretty_print();
        assert!(printed.contains("\"a\""));
        assert!(printed.contains("\"b\""));
        assert!(printed.contains("<!--ignored-->"));
    }
}
