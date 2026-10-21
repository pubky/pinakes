//! The one way to read HTML: a linear scan with hard caps, then the parser, in a parse slot.
//!
//! A page parse is CPU work no timeout can stop, and some inputs make the parser superlinear
//! (one tag with tens of thousands of attributes, tens of thousands of unclosed elements). Every
//! HTML parse in the crate goes through `document` or `fragment` (a test checks no other code
//! names the parser), which first scan the text in one pass, the way the HTML tokenizer reads
//! it, and cut it at the first point that passes a cap:
//!
//! | Cap | Real pages (149 pages of the hosts stable holds, 2026-10-21) |
//! | --- | --- |
//! | `MAX_TAGS` 50,000 start tags | median 508, 95th percentile 2,805, most 16,440 (a Wikidata item) |
//! | `MAX_DEPTH` 256 open elements | median 14, 95th percentile 31, most 62 |
//! | `MAX_TAG_ATTRIBUTES` 64 per tag | median 9, 95th percentile 14, most 29 |
//! | `MAX_ATTRIBUTE_BYTES` 1 MiB per value | median 1 KB, 95th percentile 13 KB, most 121 KB |
//!
//! Text needs no cap: the parser reads it in linear time, and the page caps bound it. Depth
//! counts open elements as the parser does, unclosed ones too: an end tag closes up to the
//! element it names and is ignored when none is open. Void elements and the ones a sibling
//! closes (`p`, `li`, `td`, `option`, …) are not counted, so old markup with unclosed list items
//! is not cut. Text in `script`, `style` and the other raw-text elements, and comments, are not
//! markup.
//!
//! A parse runs in `blocking`: on the blocking pool, holding one of a fixed number of permits
//! until the parse itself ends, even after its caller stopped waiting. In debug builds a parse on
//! a runtime thread outside `blocking` panics, so the tests find any path that skips it.
use std::cell::Cell;
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;

pub const MAX_TAGS: usize = 50_000;
pub const MAX_DEPTH: usize = 256;
pub const MAX_TAG_ATTRIBUTES: usize = 64;
pub const MAX_ATTRIBUTE_BYTES: usize = 1024 * 1024;
/// The most ancestors a walk up a parsed tree looks at: the depth cap and the elements it does
/// not count.
pub const MAX_ANCESTORS: usize = 2 * MAX_DEPTH;

const VOID: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "image", "img",
    "input", "keygen", "link", "meta", "param", "source", "track", "wbr",
];
/// Elements a sibling or the parser closes implicitly: never counted toward depth.
const IMPLIED: &[&str] = &[
    "body", "caption", "colgroup", "dd", "dt", "head", "html", "li", "optgroup", "option", "p",
    "rb", "rp", "rt", "rtc", "tbody", "td", "tfoot", "th", "thead", "tr",
];
/// Elements whose content is text up to their own end tag.
const RAW: &[&str] = &[
    "iframe", "noembed", "noframes", "noscript", "script", "style", "textarea", "title", "xmp",
];

/// Page parses that may run at once: the machine's parallelism, at least two.
static PERMITS: LazyLock<usize> = LazyLock::new(|| {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2)
});
static PARSES: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(*PERMITS)));
thread_local! {
    static IN_SLOT: Cell<bool> = const { Cell::new(false) };
}

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
        IN_SLOT.with(|slot| slot.set(true));
        let out = parse();
        IN_SLOT.with(|slot| slot.set(false));
        out
    })
    .await?)
}

/// Run a parse on a thread of its own, outside the runtime and its permits, and wait for it: for
/// tests and tools that parse from async code. Daemon code uses `blocking`; a test checks no code
/// in `src` calls this.
#[doc(hidden)]
pub fn off_runtime<T: Send>(parse: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                IN_SLOT.with(|slot| slot.set(true));
                parse()
            })
            .join()
            .expect("parse thread")
    })
}

fn check_slot() {
    debug_assert!(
        IN_SLOT.with(Cell::get) || tokio::runtime::Handle::try_current().is_err(),
        "an HTML parse on a runtime thread outside resolver::html::blocking"
    );
}

/// `text` parsed as an HTML document, cut at the first cap it passes.
pub fn document(text: &str) -> scraper::Html {
    check_slot();
    scraper::Html::parse_document(bounded(text))
}

/// `text` parsed as an HTML fragment, cut at the first cap it passes.
pub fn fragment(text: &str) -> scraper::Html {
    check_slot();
    scraper::Html::parse_fragment(bounded(text))
}

/// `text` up to the first cap it passes (see the module).
pub fn bounded(text: &str) -> &str {
    let mut scan = Scan::default();
    scan.feed(text.as_bytes());
    // A cut is at a `<`, a character boundary.
    scan.cut.map_or(text, |at| &text[..at])
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum State {
    #[default]
    Data,
    TagOpen,
    /// After `<!`, and after `<!-`.
    Markup(u8),
    CommentStart,
    CommentStartDash,
    /// In a comment, with the count of `-` just read (at most 2).
    Comment(u8),
    CommentEndBang,
    /// `<!x…>`, `<?…>`, `</ …>`: up to `>`.
    Bogus,
    EndTagOpen,
    EndTagName,
    EndTagRest,
    TagName,
    BeforeAttribute,
    AttributeName,
    AfterAttributeName,
    BeforeValue,
    Quoted(u8),
    Unquoted,
    AfterQuoted,
    SelfClosing,
    /// In a raw-text element, matching its end tag: 0 text, 1 after `<`, 2 after `</` and some
    /// of the name, 3 after the whole name.
    Raw(u8),
    /// After `<plaintext>`: everything is text.
    Plaintext,
}

/// The scan of an HTML text, fed in one piece or chunk by chunk: where it must be cut (the
/// first cap passed) and where its head ends (after `</head>`, or where `<body` opens it).
#[derive(Debug, Default)]
pub struct Scan {
    state: State,
    /// Bytes read so far.
    at: usize,
    tags: usize,
    open: Vec<Box<str>>,
    attributes: usize,
    value: usize,
    tag_start: usize,
    name: String,
    raw: Option<&'static str>,
    raw_matched: usize,
    cut: Option<usize>,
    head_end: Option<usize>,
}

impl Scan {
    /// Where the text must be cut, when a cap was passed.
    pub fn cut(&self) -> Option<usize> {
        self.cut
    }
    /// Where the head ends, when it has.
    pub fn head_end(&self) -> Option<usize> {
        self.head_end
    }
    /// Read the next bytes of the text. Once a cap is passed, nothing more is read.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.cut.is_some() {
                return;
            }
            self.step(b);
            self.at += 1;
        }
    }

    fn stop(&mut self) {
        self.cut = Some(self.tag_start);
    }

    fn name_byte(&mut self, b: u8) {
        if self.name.len() < 32 {
            self.name.push(b.to_ascii_lowercase() as char);
        }
    }

    fn attribute(&mut self) {
        self.attributes += 1;
        if self.attributes > MAX_TAG_ATTRIBUTES {
            self.stop();
        }
    }

    fn value_byte(&mut self) {
        self.value += 1;
        if self.value > MAX_ATTRIBUTE_BYTES {
            self.stop();
        }
    }

    /// A start tag ended at the current byte (`>`).
    fn start_tag(&mut self) {
        self.tags += 1;
        if self.tags > MAX_TAGS {
            return self.stop();
        }
        let name = self.name.as_str();
        if name == "body" && self.head_end.is_none() {
            self.head_end = Some(self.tag_start);
        }
        if name == "plaintext" {
            self.state = State::Plaintext;
            return;
        }
        if let Some(raw) = RAW.iter().find(|r| **r == name) {
            self.raw = Some(raw);
            self.raw_matched = 0;
            self.state = State::Raw(0);
            return;
        }
        if !VOID.contains(&name) && !IMPLIED.contains(&name) {
            self.open.push(name.into());
            if self.open.len() > MAX_DEPTH {
                return self.stop();
            }
        }
        self.state = State::Data;
    }

    /// An end tag ended at the current byte (`>`).
    fn end_tag(&mut self) {
        let name = self.name.as_str();
        if name == "head" && self.head_end.is_none() {
            self.head_end = Some(self.at + 1);
        }
        if let Some(i) = self.open.iter().rposition(|open| &**open == name) {
            self.open.truncate(i);
        }
        self.state = State::Data;
    }

    fn step(&mut self, b: u8) {
        use State::*;
        let space = matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c');
        self.state = match self.state {
            Data => {
                if b == b'<' {
                    self.tag_start = self.at;
                    TagOpen
                } else {
                    Data
                }
            }
            TagOpen => match b {
                b'!' => Markup(0),
                b'/' => EndTagOpen,
                b'?' => Bogus,
                b'<' => {
                    self.tag_start = self.at;
                    TagOpen
                }
                b if b.is_ascii_alphabetic() => {
                    self.name.clear();
                    self.attributes = 0;
                    self.name_byte(b);
                    TagName
                }
                _ => Data,
            },
            Markup(0) => match b {
                b'-' => Markup(1),
                b'>' => Data,
                _ => Bogus,
            },
            Markup(_) => match b {
                b'-' => CommentStart,
                b'>' => Data,
                _ => Bogus,
            },
            CommentStart => match b {
                b'>' => Data,
                b'-' => CommentStartDash,
                _ => Comment(0),
            },
            CommentStartDash => match b {
                b'>' => Data,
                b'-' => Comment(2),
                _ => Comment(0),
            },
            Comment(dashes) => match b {
                b'-' => Comment((dashes + 1).min(2)),
                b'>' if dashes >= 2 => Data,
                b'!' if dashes >= 2 => CommentEndBang,
                _ => Comment(0),
            },
            CommentEndBang => match b {
                b'>' => Data,
                b'-' => Comment(1),
                _ => Comment(0),
            },
            Bogus => {
                if b == b'>' {
                    Data
                } else {
                    Bogus
                }
            }
            EndTagOpen => match b {
                b if b.is_ascii_alphabetic() => {
                    self.name.clear();
                    self.name_byte(b);
                    EndTagName
                }
                b'>' => Data,
                _ => Bogus,
            },
            EndTagName => match b {
                b'>' => {
                    self.end_tag();
                    return;
                }
                b'/' => EndTagRest,
                b if space => {
                    let _ = b;
                    EndTagRest
                }
                b => {
                    self.name_byte(b);
                    EndTagName
                }
            },
            EndTagRest => {
                if b == b'>' {
                    self.end_tag();
                    return;
                }
                EndTagRest
            }
            TagName => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                b'/' => SelfClosing,
                _ if space => BeforeAttribute,
                b => {
                    self.name_byte(b);
                    TagName
                }
            },
            BeforeAttribute => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                b'/' => SelfClosing,
                _ if space => BeforeAttribute,
                _ => {
                    self.attribute();
                    AttributeName
                }
            },
            AttributeName => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                b'/' => SelfClosing,
                b'=' => BeforeValue,
                _ if space => AfterAttributeName,
                _ => AttributeName,
            },
            AfterAttributeName => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                b'/' => SelfClosing,
                b'=' => BeforeValue,
                _ if space => AfterAttributeName,
                _ => {
                    self.attribute();
                    AttributeName
                }
            },
            BeforeValue => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                b'"' | b'\'' => {
                    self.value = 0;
                    Quoted(b)
                }
                _ if space => BeforeValue,
                _ => {
                    self.value = 0;
                    self.value_byte();
                    Unquoted
                }
            },
            Quoted(quote) => {
                if b == quote {
                    AfterQuoted
                } else {
                    self.value_byte();
                    Quoted(quote)
                }
            }
            Unquoted => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                _ if space => BeforeAttribute,
                _ => {
                    self.value_byte();
                    Unquoted
                }
            },
            AfterQuoted => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                b'/' => SelfClosing,
                _ if space => BeforeAttribute,
                _ => {
                    self.attribute();
                    AttributeName
                }
            },
            SelfClosing => match b {
                b'>' => {
                    self.start_tag();
                    return;
                }
                _ => {
                    self.attribute();
                    AttributeName
                }
            },
            Raw(stage) => self.raw_step(stage, b, space),
            Plaintext => Plaintext,
        };
    }

    fn raw_step(&mut self, stage: u8, b: u8, space: bool) -> State {
        let name = self.raw.unwrap_or("").as_bytes();
        match stage {
            0 => {
                if b == b'<' {
                    self.tag_start = self.at;
                    State::Raw(1)
                } else {
                    State::Raw(0)
                }
            }
            1 => {
                if b == b'/' {
                    self.raw_matched = 0;
                    State::Raw(2)
                } else if b == b'<' {
                    self.tag_start = self.at;
                    State::Raw(1)
                } else {
                    State::Raw(0)
                }
            }
            2 => {
                if name.get(self.raw_matched) == Some(&b.to_ascii_lowercase()) {
                    self.raw_matched += 1;
                    if self.raw_matched == name.len() {
                        State::Raw(3)
                    } else {
                        State::Raw(2)
                    }
                } else if b == b'<' {
                    self.tag_start = self.at;
                    State::Raw(1)
                } else {
                    State::Raw(0)
                }
            }
            _ => {
                if b == b'>' || b == b'/' || space {
                    self.name.clear();
                    self.name.push_str(self.raw.unwrap_or(""));
                    self.raw = None;
                    if b == b'>' {
                        self.end_tag();
                        return self.state;
                    }
                    State::EndTagRest
                } else if b == b'<' {
                    self.tag_start = self.at;
                    State::Raw(1)
                } else {
                    State::Raw(0)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str) -> &str {
        bounded(text)
    }
    fn timed<T>(f: impl FnOnce() -> T) -> T {
        let started = std::time::Instant::now();
        let out = f();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        out
    }
    fn title(html: &scraper::Html) -> Option<String> {
        let title = scraper::Selector::parse("title").unwrap();
        html.select(&title).next().map(|t| t.text().collect())
    }

    #[test]
    fn real_markup_is_left_whole() {
        let mut attributes = String::from("<div");
        for n in 0..MAX_TAG_ATTRIBUTES {
            attributes.push_str(&format!(" data-{n}=\"a b c\""));
        }
        attributes.push('>');
        let list = format!("<ul>{}</ul>", "<li>item".repeat(2_000));
        let nested = format!(
            "{}x{}",
            "<div>".repeat(MAX_DEPTH),
            "</div>".repeat(MAX_DEPTH)
        );
        for text in [
            "<html><head><title>T</title></head><body><p class='a b c'>x</p></body></html>",
            "<script>if (a < b && c<d) { x = '<div a b c d>'; }</script><p>y</p>",
            "<!-- <div a b c --><p>z</p>",
            "a < b, and <unclosed",
            attributes.as_str(),
            list.as_str(),
            nested.as_str(),
            // Stray end tags close nothing.
            "</div></span><div>ok</div>",
        ] {
            assert_eq!(cut(text), text);
        }
    }

    /// Each of Codex's probes is cut where it passes a cap, and parses in well under a second.
    #[test]
    fn hostile_markup_is_cut_at_its_cap() {
        // One tag with 60,000 attributes, separated by spaces or by `/`.
        let mut spaced = String::from("<title>Kept</title><div");
        let mut slashed = String::from("<title>Kept</title><div");
        for n in 0..60_000 {
            spaced.push_str(&format!(" a{n}='x'"));
            slashed.push_str(&format!("/a{n}='x'"));
        }
        // Attributes run together after a quoted value.
        let mut joined = String::from("<title>Kept</title><div a0='x'");
        for n in 1..60_000 {
            joined.push_str(&format!("a{n}='x'"));
        }
        // 20,000 unclosed divs; 30,000 `<span>x`; spans closed by end tags of other names.
        let unclosed = format!("<title>Kept</title>{}", "<div>".repeat(20_000));
        let spans = format!("<title>Kept</title>{}", "<span>x".repeat(30_000));
        let stray = format!("<title>Kept</title>{}", "<span></x>".repeat(30_000));
        // 13,000 divs with no closing tags behind a comment that closes at once.
        let comment = format!("<title>Kept</title><!-->{}", "<div>".repeat(13_000));
        let many = format!("<title>Kept</title>{}", "<br>".repeat(60_000));
        for page in [
            &spaced, &slashed, &joined, &unclosed, &spans, &stray, &comment, &many,
        ] {
            assert!(cut(page).len() < page.len());
            let html = timed(|| document(page));
            assert_eq!(title(&html).as_deref(), Some("Kept"));
        }
    }

    /// Codex's probes, each through the code that met it, finish in well under a second (in a
    /// debug build) and overflow no stack.
    #[test]
    fn codex_s_probes_finish_at_once() {
        use crate::resolver::{egress::Fetched, interstitial, web};
        let fetched = |status: u16, content_type: &str, body: String| Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: "https://hostile.example.org/p".parse().unwrap(),
            content_type: content_type.into(),
            status,
            retry_after_secs: 0,
            location: None,
            body: body.into_bytes(),
        };
        let url: url::Url = "https://hostile.example.org/p".parse().unwrap();
        // 10,000 nested spans in an Internet Archive description: plain text, no overflow.
        let nested = format!(
            "<p>Kept</p>{}deep{}",
            "<span>".repeat(10_000),
            "</span>".repeat(10_000)
        );
        let text = timed(|| crate::sources::kit::html_text(&nested));
        assert_eq!(text.as_deref(), Some("Kept"));
        // 20,000 unclosed divs; a tag with 60,000 attributes separated by `/`.
        let unclosed = format!("<title>Kept</title>{}", "<div>".repeat(20_000));
        let mut slashed = String::from("<title>Kept</title><div");
        for n in 0..60_000 {
            slashed.push_str(&format!("/a{n}='x'"));
        }
        for page in [unclosed, slashed] {
            let out = timed(|| web::extract(url.as_str(), &fetched(200, "text/html", page), 3600));
            assert_eq!(out.doc.name.or(out.title).as_deref(), Some("Kept"));
        }
        // Repeated `<span>x` nesting against the wall check.
        let spans = format!("<title>Kept</title>{}", "<span>x".repeat(30_000));
        timed(|| interstitial::inspect(&url, &fetched(200, "text/html", spans)));
        // Nested JSON-LD scripts.
        let ld = r#"<script type="application/ld+json">{"@type":"Event","name":"x"}</script>"#;
        let json_ld = format!("<title>Kept</title>{}", format!("<div>{ld}").repeat(20_000));
        timed(|| web::extract(url.as_str(), &fetched(200, "text/html", json_ld), 3600));
        // A 503 text/plain answer of 13,000 divs, read by the wall check.
        let plain = "<div>".repeat(13_000);
        timed(|| interstitial::inspect(&url, &fetched(503, "text/plain", plain)));
    }

    /// Text inside script, style, comments and other raw text is not markup: `<body` there does
    /// not end the head, and a giant tag there is not counted.
    #[test]
    fn the_head_ends_at_markup_only() {
        let head_end = |text: &str| {
            let mut scan = Scan::default();
            scan.feed(text.as_bytes());
            scan.head_end()
        };
        let tail = "<title>T</title></head><body>";
        for decoy in [
            "<script>var s = '<body class=x>';</script>",
            "<style>/* <body> */ a { b: c }</style>",
            "<!-- <body> -->",
            "<noscript><body></noscript>",
        ] {
            let page = format!("<html><head>{decoy}{tail}");
            assert_eq!(
                head_end(&page),
                Some(page.len() - "<body>".len()),
                "{decoy}"
            );
        }
        assert_eq!(head_end("<head><title>T</title><BODY class=x>"), Some(22));
        assert_eq!(head_end("<head><title>T</title>"), None);
        let mut giant = String::from("<script>x = '<div");
        for n in 0..1_000 {
            giant.push_str(&format!(" a{n}"));
        }
        giant.push_str(">';</script><p>after</p>");
        assert_eq!(cut(&giant), giant);
    }

    /// Fed in pieces, the scan finds what it finds in one piece: a tag split across chunks.
    #[test]
    fn a_scan_fed_in_pieces_matches_one_piece() {
        let page = format!(
            "<html><head><title>T</title><script>a = '</head>'</script></he{}ad><body>",
            ""
        );
        let mut whole = Scan::default();
        whole.feed(page.as_bytes());
        for size in [1, 2, 3, 7] {
            let mut pieces = Scan::default();
            for chunk in page.as_bytes().chunks(size) {
                pieces.feed(chunk);
            }
            assert_eq!(pieces.head_end(), whole.head_end(), "{size}");
        }
        assert_eq!(whole.head_end(), Some(page.len() - "<body>".len()));
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

    /// No code but this module names the HTML parser's entry points.
    #[test]
    fn only_this_module_parses_html() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut found = vec![];
        let entry = [
            ["Html::", "parse_document"].concat(),
            ["Html::", "parse_fragment"].concat(),
            ["Html::", "new_document"].concat(),
            ["Html::", "new_fragment"].concat(),
            ["html5ever", "::"].concat(),
            ["parse_document", "(scraper"].concat(),
            ["html::", "off_runtime("].concat(),
        ];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "rs")
                    && !path.ends_with("resolver/html.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for name in &entry {
                        if text.contains(name.as_str()) {
                            found.push(format!("{}: {name}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }
}
