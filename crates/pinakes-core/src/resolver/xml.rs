//! Parsing XML a reference steered (feeds, calendars as RSS or Atom): every such body the
//! resolvers, the event readers, the episode lists and the feed sources read goes through
//! [`parse`], which refuses the shapes `roxmltree` has no limit of its own for before it builds
//! a tree.
//!
//! `roxmltree` reads nested elements by recursion (one stack frame pair per level, no depth
//! limit), copies an element's inherited namespaces whenever it declares one, and compares each
//! attribute with the element's earlier ones. A streaming pass (`quick-xml`, no recursion, no
//! tree) measures those first, so a body that passes costs time and memory linear in its length.
use anyhow::{bail, ensure, Result};

/// The deepest elements may nest. An RSS item's fields sit at depth 4, XHTML content in an Atom
/// entry a few levels more.
pub const MAX_DEPTH: usize = 64;
/// The most nodes (elements, texts, comments, processing instructions) of one document.
pub const MAX_NODES: u32 = 1_000_000;
/// The most namespace declarations in scope at one element (a podcast feed declares about 20).
pub const MAX_NAMESPACES: usize = 64;
/// The most attributes of one element, namespace declarations aside.
pub const MAX_ATTRIBUTES: usize = 64;
/// The most work namespace declarations below the root may cost in all: an element that declares
/// `d` namespaces with `p` inherited ones counts `(p + d) * d + p` (what the tree builder
/// compares and copies for it). A feed whose 10,000 entries each declare four counts 660,000.
pub const MAX_NAMESPACE_WORK: u64 = 4_000_000;

/// `body` as a tree, with at most [`MAX_NODES`] nodes. An error for a body that is not
/// well-formed, has a DOCTYPE, or is past one of this module's limits.
pub fn parse(body: &str) -> Result<roxmltree::Document<'_>> {
    parse_capped(body, MAX_NODES)
}

/// [`parse`] with a node limit of the caller's.
pub fn parse_capped(body: &str, nodes: u32) -> Result<roxmltree::Document<'_>> {
    check(body, nodes)?;
    let options = roxmltree::ParsingOptions {
        // The tree's own root counts against its limit.
        nodes_limit: nodes.saturating_add(1),
        ..Default::default()
    };
    Ok(roxmltree::Document::parse_with_options(body, options)?)
}

/// The streaming pass: `Ok` when `body` is within the limits. Stops when its parse job is over
/// (`html::job_ended`: read again later, not the body's failure).
fn check(body: &str, nodes: u32) -> Result<()> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(body);
    // The namespace declarations in scope at each open element.
    let mut scopes: Vec<usize> = vec![];
    let mut seen = 0u32;
    let mut work = 0u64;
    for event in 0u64.. {
        if event % 1024 == 0 && super::html::job_over() {
            return Err(super::html::job_ended());
        }
        let (element, open) = match reader.read_event() {
            Ok(Event::Start(e)) => (e, true),
            Ok(Event::Empty(e)) => (e, false),
            Ok(Event::End(_)) => {
                scopes.pop();
                continue;
            }
            Ok(Event::Text(_) | Event::CData(_) | Event::Comment(_) | Event::PI(_)) => {
                seen += 1;
                ensure!(seen <= nodes, "XML: more than {nodes} nodes");
                continue;
            }
            Ok(Event::DocType(_)) => bail!("XML: a DOCTYPE"),
            Ok(Event::Decl(_)) => continue,
            Ok(Event::Eof) => break,
            Err(_) => bail!("XML: not well-formed"),
        };
        seen += 1;
        ensure!(seen <= nodes, "XML: more than {nodes} nodes");
        ensure!(
            scopes.len() < MAX_DEPTH,
            "XML: elements nest deeper than {MAX_DEPTH}"
        );
        let (mut declared, mut attributes) = (0usize, 0usize);
        for attribute in element.attributes().with_checks(false) {
            let Ok(attribute) = attribute else {
                bail!("XML: not well-formed");
            };
            if attribute.key.as_namespace_binding().is_some() {
                declared += 1;
            } else {
                attributes += 1;
            }
            ensure!(
                attributes <= MAX_ATTRIBUTES,
                "XML: more than {MAX_ATTRIBUTES} attributes on an element"
            );
            ensure!(
                declared <= MAX_NAMESPACES,
                "XML: more than {MAX_NAMESPACES} namespaces in scope"
            );
        }
        let inherited = scopes.last().copied().unwrap_or(0);
        let scope = inherited + declared;
        ensure!(
            scope <= MAX_NAMESPACES,
            "XML: more than {MAX_NAMESPACES} namespaces in scope"
        );
        if declared > 0 && !scopes.is_empty() {
            work += ((scope * declared) + inherited) as u64;
            ensure!(
                work <= MAX_NAMESPACE_WORK,
                "XML: namespace declarations past the work limit"
            );
        }
        if open {
            scopes.push(scope);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_parses_and_its_namespaces_resolve() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" version="2.0"><channel>
<title>Show</title><!-- note --><item><itunes:duration>61</itunes:duration>
<content><div xmlns="http://www.w3.org/1999/xhtml"><p>Text &amp; more</p></div></content>
</item></channel></rss>"#;
        let xml = parse(body).unwrap();
        let duration = xml
            .descendants()
            .find(|n| n.tag_name().name() == "duration")
            .unwrap();
        assert_eq!(
            duration.tag_name().namespace(),
            Some("http://www.itunes.com/dtds/podcast-1.0.dtd")
        );
        assert_eq!(duration.text(), Some("61"));
    }

    #[test]
    fn depth_is_refused_at_the_limit_and_not_before() {
        let nest = |levels: usize| format!("{}{}", "<a>".repeat(levels), "</a>".repeat(levels));
        assert!(parse(&nest(MAX_DEPTH)).is_ok());
        let error = parse(&nest(MAX_DEPTH + 1)).unwrap_err().to_string();
        assert!(error.contains("nest deeper"), "{error}");
    }

    #[test]
    fn nodes_attributes_and_a_doctype_are_refused() {
        let error = parse_capped("<a><b/><b/><b/></a>", 3)
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 3 nodes"), "{error}");
        assert!(parse_capped("<a><b/><b/></a>", 3).is_ok());

        let attributes = |n: usize| {
            let list: String = (0..n).map(|i| format!(" a{i}=''")).collect();
            format!("<a{list}/>")
        };
        assert!(parse(&attributes(MAX_ATTRIBUTES)).is_ok());
        let error = parse(&attributes(MAX_ATTRIBUTES + 1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("attributes"), "{error}");

        let error = parse("<!DOCTYPE a [<!ENTITY b 'c'>]><a>&b;</a>")
            .unwrap_err()
            .to_string();
        assert!(error.contains("DOCTYPE"), "{error}");
        assert!(parse("<a><b></a>").is_err());
    }

    /// 100,000 nested elements, closed and cut off, at each shared site that parses XML: every
    /// one answers "no feed" instead of overflowing the stack. The bodies are read in a child
    /// process (this test binary, running this test alone), so a site that recursed again would
    /// fail this test and not end the binary.
    #[test]
    fn nested_xml_is_refused_at_every_site() {
        const CHILD: &str = "PINAKES_TEST_NESTED_XML_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "resolver::xml::tests::nested_xml_is_refused_at_every_site",
                    "--exact",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "the child ended {:?}: {stdout}",
                out.status
            );
            return;
        }
        use crate::resolver::egress::Fetched;
        let open = "<a>".repeat(100_000);
        let closed = format!("{open}{}", "</a>".repeat(100_000));
        for nest in [open, closed] {
            let body = format!("<rss><channel><title>Deep</title>{nest}</channel></rss>");
            assert!(parse(&body).is_err());
            let f = Fetched::answer(
                "https://feeds.example.org/deep.xml".parse().unwrap(),
                200,
                "application/rss+xml".into(),
                body.clone().into_bytes(),
            );
            assert!(crate::episodes::from_feed_listed(&f).is_none());
            assert!(super::super::cards::podcast(f.url.as_str(), &f, 3600, 100).is_err());
            let read = crate::event::calendar::structured_feed(
                f.url.as_str(),
                body.as_bytes(),
                &crate::config::EventConfig::default(),
                chrono::Utc::now(),
            );
            assert!(matches!(read, Ok(None)), "{read:?}");
        }
    }

    /// 20,000 declarations on the root, and 40,000 children that each declare one more: the
    /// tree builder would copy the root's for every child.
    #[test]
    fn namespace_declarations_are_refused_past_their_limits() {
        let declarations =
            |n: usize| -> String { (0..n).map(|i| format!(" xmlns:n{i}='u:{i}'")).collect() };
        let wide = format!("<a{}/>", declarations(20_000));
        let error = parse(&wide).unwrap_err().to_string();
        assert!(error.contains("namespaces in scope"), "{error}");

        // Within the scope limit at every element, past the work limit in all.
        let children = "<b xmlns:x='u:x'/>".repeat(40_000);
        let body = format!("<a{}>{children}</a>", declarations(MAX_NAMESPACES - 1));
        let started = std::time::Instant::now();
        let error = parse(&body).unwrap_err().to_string();
        assert!(error.contains("work limit"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        // An Atom feed whose every entry declares XHTML stays far inside it.
        let entries =
            "<entry><content><div xmlns='http://www.w3.org/1999/xhtml'>x</div></content></entry>"
                .repeat(5_000);
        let feed = format!(
            "<feed xmlns='http://www.w3.org/2005/Atom'{}>{entries}</feed>",
            declarations(20)
        );
        assert!(parse(&feed).is_ok());
    }
}
