//! Dev drill: time style_document on a synthetic Wikipedia-sized page.
//! Usage: cargo run -rq -p bui-style --example cascade_bench

use bui_css::Stylesheet;
use bui_dom::Document;

fn main() {
    // ~6000 elements: sections of divs/p/span/a/li with rotating classes.
    let mut doc = Document::new();
    let html = doc.create_element("html");
    let body = doc.create_element("body");
    doc.append_child(doc.root, html);
    doc.append_child(html, body);
    let tags = ["div", "p", "span", "a", "li", "h2"];
    for s in 0..200 {
        let section = doc.create_element("div");
        doc.element_mut(section)
            .unwrap()
            .set_attr("class", &format!("sect s{}", s % 40));
        doc.append_child(body, section);
        for i in 0..30 {
            let el = doc.create_element(tags[i % tags.len()]);
            if i % 2 == 0 {
                doc.element_mut(el)
                    .unwrap()
                    .set_attr("class", &format!("c{} item", (s + i) % 80));
            }
            if i % 17 == 0 {
                doc.element_mut(el)
                    .unwrap()
                    .set_attr("id", &format!("id-{s}-{i}"));
            }
            doc.append_child(section, el);
        }
    }

    // ~1600 rules with a real-world-ish key mix: mostly class- and
    // tag-keyed, some descendant chains, a few universal.
    let mut css = String::new();
    for r in 0..80 {
        for t in ["div", "p", "span", "a", "li"] {
            css.push_str(&format!(".c{r} {t} {{ color: rgb({r}, 0, 0); }}\n"));
            css.push_str(&format!("{t}.c{} {{ margin: {r}px; }}\n", (r + 1) % 80));
        }
        css.push_str(&format!(".s{} .item {{ padding: 1px; }}\n", r % 40));
        css.push_str(&format!("#id-1-{r} {{ outline: none; }}\n"));
    }
    css.push_str("* { box-sizing: border-box; }\n");
    let sheet = Stylesheet::parse(&css);

    let elements = doc.descendants(doc.root).count();
    println!("elements: {elements}, css rules: ~{}", css.lines().count());

    let mut best = f64::MAX;
    for _ in 0..5 {
        let started = std::time::Instant::now();
        let tree = bui_style::style_document(&doc, &[sheet.clone()]);
        let dt = started.elapsed().as_secs_f64();
        best = best.min(dt);
        std::hint::black_box(tree);
    }
    println!("style_document best of 5: {:.1} ms", best * 1000.0);
}
