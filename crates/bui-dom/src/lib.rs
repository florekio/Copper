//! bui-dom — arena-allocated DOM tree.
//!
//! Phase 2: tree structure + mutation API. JS bindings (Phase 5) will reach
//! into this via `NodeId` handles.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Doctype {
        name: String,
        public_id: String,
        system_id: String,
    },
    Element(Element),
    Text(String),
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
}

impl Element {
    pub fn get_attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn set_attr(&mut self, name: &str, value: &str) {
        if let Some((_, v)) = self
            .attrs
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            *v = value.to_string();
        } else {
            self.attrs.push((name.to_string(), value.to_string()));
        }
    }

    /// Remove every entry whose key matches `name` (ASCII case-insensitive).
    /// Returns true if at least one was removed — callers can use this to
    /// trip a dirty flag only on a real change. No-op when absent.
    pub fn remove_attr(&mut self, name: &str) -> bool {
        let before = self.attrs.len();
        self.attrs.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.attrs.len() != before
    }

    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.get_attr("class")
            .into_iter()
            .flat_map(|s| s.split_ascii_whitespace())
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
}

#[derive(Clone)]
pub struct Document {
    nodes: Vec<Node>,
    pub root: NodeId,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        let root = Node {
            kind: NodeKind::Document,
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
        };
        Self {
            nodes: vec![root],
            root: NodeId(0),
        }
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.index()]
    }

    pub fn element(&self, id: NodeId) -> Option<&Element> {
        match &self.node(id).kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn element_mut(&mut self, id: NodeId) -> Option<&mut Element> {
        match &mut self.node_mut(id).kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn create_element(&mut self, name: &str) -> NodeId {
        self.alloc(NodeKind::Element(Element {
            name: name.to_ascii_lowercase(),
            attrs: Vec::new(),
        }))
    }

    pub fn create_text(&mut self, data: &str) -> NodeId {
        self.alloc(NodeKind::Text(data.to_string()))
    }

    pub fn create_comment(&mut self, data: &str) -> NodeId {
        self.alloc(NodeKind::Comment(data.to_string()))
    }

    pub fn create_doctype(&mut self, name: &str, public: &str, system: &str) -> NodeId {
        self.alloc(NodeKind::Doctype {
            name: name.to_string(),
            public_id: public.to_string(),
            system_id: system.to_string(),
        })
    }

    fn alloc(&mut self, kind: NodeKind) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            kind,
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
        });
        id
    }

    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        self.detach(child);
        let last = self.nodes[parent.index()].last_child;
        self.nodes[child.index()].parent = Some(parent);
        self.nodes[child.index()].prev_sibling = last;
        self.nodes[child.index()].next_sibling = None;
        match last {
            Some(prev) => self.nodes[prev.index()].next_sibling = Some(child),
            None => self.nodes[parent.index()].first_child = Some(child),
        }
        self.nodes[parent.index()].last_child = Some(child);
    }

    /// Replace `parent`'s children with a single text node carrying
    /// `text`. Mirrors `Element.textContent =` setter semantics.
    /// Detaches each existing child (slots stay allocated but become
    /// unreachable from `parent`) and appends a fresh text node.
    pub fn set_text_content(&mut self, parent: NodeId, text: &str) {
        // Detach existing children first. We snapshot the ids because
        // `detach` walks sibling pointers and we'd race a live walk.
        let mut to_detach = Vec::new();
        let mut c = self.node(parent).first_child;
        while let Some(id) = c {
            to_detach.push(id);
            c = self.node(id).next_sibling;
        }
        for c in to_detach {
            self.detach(c);
        }
        let txt = self.create_text(text);
        self.append_child(parent, txt);
    }

    /// Detach `child` from its current parent, if any. The node remains
    /// allocated; callers can re-attach it.
    pub fn detach(&mut self, child: NodeId) {
        let Node {
            parent,
            prev_sibling,
            next_sibling,
            ..
        } = *self.node(child);
        if let Some(prev) = prev_sibling {
            self.nodes[prev.index()].next_sibling = next_sibling;
        }
        if let Some(next) = next_sibling {
            self.nodes[next.index()].prev_sibling = prev_sibling;
        }
        if let Some(p) = parent {
            if self.nodes[p.index()].first_child == Some(child) {
                self.nodes[p.index()].first_child = next_sibling;
            }
            if self.nodes[p.index()].last_child == Some(child) {
                self.nodes[p.index()].last_child = prev_sibling;
            }
        }
        self.nodes[child.index()].parent = None;
        self.nodes[child.index()].prev_sibling = None;
        self.nodes[child.index()].next_sibling = None;
    }

    pub fn children(&self, parent: NodeId) -> ChildrenIter<'_> {
        ChildrenIter {
            doc: self,
            next: self.node(parent).first_child,
        }
    }

    pub fn descendants(&self, root: NodeId) -> DescendantsIter<'_> {
        DescendantsIter {
            doc: self,
            stack: vec![root],
            include_root: true,
            yielded_root: false,
            root,
        }
    }

    pub fn pretty_print(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, self.root, 0);
        out
    }

    fn write_pretty(&self, out: &mut String, id: NodeId, depth: usize) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        match &self.node(id).kind {
            NodeKind::Document => out.push_str("#document"),
            NodeKind::Doctype { name, .. } => {
                out.push_str("<!DOCTYPE ");
                out.push_str(name);
                out.push('>');
            }
            NodeKind::Element(e) => {
                out.push('<');
                out.push_str(&e.name);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(v);
                    out.push('"');
                }
                out.push('>');
            }
            NodeKind::Text(t) => {
                out.push('"');
                out.push_str(&t.replace('\n', "\\n"));
                out.push('"');
            }
            NodeKind::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
        }
        out.push('\n');
        let mut child = self.node(id).first_child;
        while let Some(c) = child {
            self.write_pretty(out, c, depth + 1);
            child = self.node(c).next_sibling;
        }
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pretty_print())
    }
}

pub struct ChildrenIter<'a> {
    doc: &'a Document,
    next: Option<NodeId>,
}

impl Iterator for ChildrenIter<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = self.doc.node(id).next_sibling;
        Some(id)
    }
}

pub struct DescendantsIter<'a> {
    doc: &'a Document,
    stack: Vec<NodeId>,
    include_root: bool,
    yielded_root: bool,
    root: NodeId,
}

impl Iterator for DescendantsIter<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        if self.include_root && !self.yielded_root {
            self.yielded_root = true;
            // push children onto stack in reverse to preserve order
            let mut child = self.doc.node(self.root).last_child;
            while let Some(c) = child {
                self.stack.push(c);
                child = self.doc.node(c).prev_sibling;
            }
            // remove root from initial stack since we yielded it explicitly
            self.stack.retain(|n| *n != self.root);
            return Some(self.root);
        }
        let id = self.stack.pop()?;
        if id == self.root {
            return self.next();
        }
        let mut child = self.doc.node(id).last_child;
        while let Some(c) = child {
            self.stack.push(c);
            child = self.doc.node(c).prev_sibling;
        }
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tree() {
        let mut doc = Document::new();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let p = doc.create_element("p");
        let text = doc.create_text("hi");
        doc.append_child(doc.root, html);
        doc.append_child(html, body);
        doc.append_child(body, p);
        doc.append_child(p, text);
        let printed = doc.pretty_print();
        assert!(printed.contains("<html>"));
        assert!(printed.contains("<body>"));
        assert!(printed.contains("<p>"));
        assert!(printed.contains("\"hi\""));
    }

    #[test]
    fn detach_reattach() {
        let mut doc = Document::new();
        let a = doc.create_element("a");
        let b = doc.create_element("b");
        let c = doc.create_element("c");
        doc.append_child(doc.root, a);
        doc.append_child(a, b);
        doc.append_child(a, c);
        // move b under c
        doc.append_child(c, b);
        assert_eq!(doc.node(b).parent, Some(c));
        assert_eq!(doc.node(a).first_child, Some(c));
        assert_eq!(doc.node(a).last_child, Some(c));
    }

    #[test]
    fn classes_iter() {
        let mut doc = Document::new();
        let id = doc.create_element("div");
        doc.element_mut(id).unwrap().set_attr("class", "foo bar baz");
        let classes: Vec<&str> = doc.element(id).unwrap().classes().collect();
        assert_eq!(classes, vec!["foo", "bar", "baz"]);
    }
}
