//! Parsing fetched HTML with bounded work.
//!
//! A page parse is CPU work on the blocking pool that no timeout can stop: an abandoned parse
//! keeps running while its worker takes the next job. Two bounds keep a hostile page from
//! costing more than a page:
//! - `document` cuts the text before the first tag with more than `MAX_TAG_ATTRIBUTES`
//!   attributes. The tokenizer checks each new attribute against the tag's others, so one tag
//!   with tens of thousands of attributes takes minutes; no real page has one.
//! - `blocking` runs a parse holding one of a fixed number of permits until the parse itself
//!   ends, so abandoned parses never outnumber the permits.
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;

/// The most attributes one tag may have before the page is cut there.
pub const MAX_TAG_ATTRIBUTES: usize = 256;

/// Page parses that may run at once: the machine's parallelism, at least two.
static PERMITS: LazyLock<usize> = LazyLock::new(|| {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2)
});
static PARSES: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(*PERMITS)));

/// The page parses running now.
pub fn running() -> usize {
    *PERMITS - PARSES.available_permits()
}

/// Run `parse` on the blocking pool once a parse permit is free, holding the permit until
/// `parse` returns, even when the caller has stopped waiting.
pub async fn blocking<T, F>(parse: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = PARSES.clone().acquire_owned().await?;
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        parse()
    })
    .await?)
}

/// `text` parsed as an HTML document, cut before a tag with more than `MAX_TAG_ATTRIBUTES`
/// attributes.
pub fn document(text: &str) -> scraper::Html {
    scraper::Html::parse_document(bounded(text))
}

/// `text` up to the first tag with more than `MAX_TAG_ATTRIBUTES` attributes. A tag is `<`
/// followed by a letter, up to `>` outside quotes; an attribute starts at a name character after
/// white space. Comments and other markup are not tags. Linear in the text.
pub fn bounded(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' || !bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic) {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i + 1;
        let mut attributes = 0usize;
        let mut quote: Option<u8> = None;
        let mut after_space = false;
        let mut closed = false;
        while j < bytes.len() {
            let b = bytes[j];
            match quote {
                Some(q) if b == q => quote = None,
                Some(_) => {}
                None => match b {
                    b'"' | b'\'' => {
                        quote = Some(b);
                        after_space = false;
                    }
                    b'>' => {
                        closed = true;
                        break;
                    }
                    // Not a tag after all: markup starts again before this one closed.
                    b'<' => break,
                    b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => after_space = true,
                    b'=' | b'/' => after_space = false,
                    _ => {
                        if after_space {
                            attributes += 1;
                            if attributes > MAX_TAG_ATTRIBUTES {
                                return &text[..start];
                            }
                        }
                        after_space = false;
                    }
                },
            }
            j += 1;
        }
        i = if closed { j + 1 } else { j.max(start + 1) };
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page whose head holds a megabyte of styles and then a tag with 60,000 distinct
    /// attributes, the shape that kept a parse running past its timeout.
    pub fn hostile_page() -> String {
        let mut page = format!(
            "<html><head><title>Kept</title><style>{}</style></head><body><div",
            "x".repeat(1024 * 1024)
        );
        for n in 0..60_000 {
            page.push_str(&format!(" a{n}=\"v\""));
        }
        page.push_str("></div></body></html>");
        page
    }

    #[test]
    fn a_tag_with_too_many_attributes_ends_the_page() {
        let page = hostile_page();
        let cut = bounded(&page);
        assert!(cut.ends_with("<body>"), "{}", &cut[cut.len() - 20..]);
        let started = std::time::Instant::now();
        let html = document(&page);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let title = scraper::Selector::parse("title").unwrap();
        assert_eq!(
            html.select(&title)
                .next()
                .map(|t| t.text().collect::<String>()),
            Some("Kept".to_owned())
        );
    }

    #[test]
    fn ordinary_markup_is_left_whole() {
        let mut many = String::from("<div");
        for n in 0..MAX_TAG_ATTRIBUTES {
            many.push_str(&format!(" data-{n}=\"a b c\""));
        }
        many.push('>');
        for text in [
            "<html><head><title>T</title></head><body><p class='a b c'>x</p></body></html>",
            "<script>if (a < b && c<d) { x = '<div a b c d>'; }</script><p>y</p>",
            "<!-- <div a b c --><p>z</p>",
            "a < b, and <unclosed",
            many.as_str(),
        ] {
            assert_eq!(bounded(text), text);
        }
    }

    #[tokio::test]
    async fn a_parse_holds_its_permit_until_it_ends() {
        let before = running();
        let (started, wait) = std::sync::mpsc::channel();
        let (finish, done) = std::sync::mpsc::channel::<()>();
        let parse = tokio::spawn(blocking(move || {
            started.send(()).unwrap();
            done.recv().unwrap();
        }));
        tokio::task::spawn_blocking(move || wait.recv().unwrap())
            .await
            .unwrap();
        // The caller stops waiting; the parse still runs and still holds its permit.
        parse.abort();
        assert!(running() > before);
        finish.send(()).unwrap();
        for _ in 0..200 {
            if running() == before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(running(), before);
    }
}
