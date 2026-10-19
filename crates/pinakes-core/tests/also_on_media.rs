//! The book, podcast and film sources through Egress, against the answers recorded in
//! `tests/fixtures/media` (each family's `provenance.json` names the URL of every file).
use pinakes_core::{
    also_on::{books::Books, Context, Found, Source, Subject},
    config::{Config, EgressConfig},
    model::ResourceMeta,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
};
use std::{
    collections::BTreeMap,
    io::Read,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

/// One recorded answer.
#[derive(Clone)]
struct Answer {
    status: u16,
    location: Option<String>,
    content_type: String,
    body: Vec<u8>,
}

/// Answers every recorded URL as it was recorded, a Wikidata search nobody recorded with no
/// hits, and anything else with a test failure; records each request.
struct Recorded {
    answers: BTreeMap<String, Answer>,
    seen: Mutex<Vec<String>>,
}
fn read(family: &str, file: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/media/{family}/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    if file.ends_with(".gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(raw.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        out
    } else {
        raw
    }
}
impl Recorded {
    fn new() -> Arc<Self> {
        let mut answers = BTreeMap::new();
        for family in ["books", "films", "podcasts", "wikidata"] {
            let provenance: serde_json::Value =
                serde_json::from_slice(&read(family, "provenance.json")).unwrap();
            let entries = provenance.as_object().unwrap();
            for (name, p) in entries {
                let mut url = p["url"].as_str().unwrap().to_owned();
                // A followed redirect's answer is served at its target.
                if let Some(first) = name.strip_suffix("-followed") {
                    url = entries[first]["location"].as_str().unwrap().to_owned();
                }
                answers.insert(
                    url,
                    Answer {
                        status: p["status"].as_u64().unwrap() as u16,
                        location: p["location"].as_str().map(str::to_owned),
                        content_type: p["content_type"].as_str().unwrap_or("").to_owned(),
                        body: read(family, p["file"].as_str().unwrap()),
                    },
                );
            }
        }
        Arc::new(Self {
            answers,
            seen: Mutex::new(Vec::new()),
        })
    }
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}
#[async_trait::async_trait]
impl Transport for Recorded {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push(url.to_string());
        let param = |u: &url::Url, k: &str| {
            u.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
        };
        let query = |k: &str| param(url, k).is_some_and(|v| !v.is_empty());
        // A Wikidata search is the recorded one for the same statement, however it is spelled.
        let search = param(url, "srsearch").and_then(|asked| {
            self.answers.iter().find_map(|(u, a)| {
                let u = url::Url::parse(u).ok()?;
                (param(&u, "srsearch") == Some(asked.clone())).then(|| a.clone())
            })
        });
        let answer = match self.answers.get(url.as_str()).cloned().or(search) {
            Some(answer) => answer,
            // The recorded entities answer holds every item the tests read.
            None if url.host_str() == Some("www.wikidata.org") && query("ids") => self
                .answers
                .iter()
                .find(|(u, _)| u.contains("action=wbgetentities"))
                .unwrap()
                .1
                .clone(),
            None if url.host_str() == Some("www.wikidata.org") && query("srsearch") => Answer {
                status: 200,
                location: None,
                content_type: "application/json".into(),
                body: br#"{"batchcomplete":"","query":{"search":[]}}"#.to_vec(),
            },
            None => panic!("unrecorded request {url}"),
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status: answer.status,
            retry_after_secs: 0,
            location: answer.location,
            content_type: answer.content_type,
            body: answer.body,
        })
    }
}

fn cx(transport: Arc<Recorded>, config: Config) -> Context {
    Context {
        egress: Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), transport),
        config: Arc::new(config),
    }
}
fn subject(uri: &str, type_name: Option<&str>, doc: Option<ResourceMeta>) -> Subject {
    Subject {
        id: "r1".into(),
        uri: uri.into(),
        status: "ok".into(),
        type_name: type_name.map(str::to_owned),
        doc,
        keys: Vec::new(),
        first_seen_at: None,
    }
}
fn keys(found: &Found) -> Vec<&str> {
    found.keys.iter().map(|k| k.key.as_str()).collect()
}
fn links(found: &Found) -> Vec<(&str, &str, &str)> {
    found
        .links
        .iter()
        .map(|l| (l.site.as_str(), l.uri.as_str(), l.via.as_str()))
        .collect()
}

#[tokio::test]
async fn a_printed_book_and_its_work() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let s = subject("isbn:0743273567", None, None);
    assert!(Books.applies(&s));
    let found = Books.lookup(&cx, &s).await.unwrap();
    assert_eq!(
        keys(&found),
        [
            "openlibrary:edition:OL23109860M",
            "openlibrary:work:OL468431W",
            "wikidata:Q214371"
        ]
    );
    assert_eq!(
        links(&found),
        [
            (
                "Open Library",
                "https://openlibrary.org/books/OL23109860M",
                "openlibrary"
            ),
            (
                "Open Library",
                "https://openlibrary.org/works/OL468431W",
                "openlibrary"
            ),
            (
                "Internet Archive",
                "https://archive.org/details/greatgat00fitz",
                "openlibrary"
            ),
            (
                "Goodreads",
                "https://www.goodreads.com/book/show/4671",
                "openlibrary"
            ),
            (
                "LibraryThing",
                "https://www.librarything.com/work/2964",
                "openlibrary"
            ),
            (
                "Google Books",
                "https://books.google.com/books?id=iXn5U2IzVH0C",
                "openlibrary"
            ),
            ("Amazon", "https://www.amazon.com/dp/0743273567", "isbn"),
            (
                "Goodreads",
                "https://www.goodreads.com/work/editions/245494",
                "wikidata"
            ),
            (
                "Project Gutenberg",
                "https://www.gutenberg.org/ebooks/64317",
                "wikidata"
            ),
        ]
    );
    assert!(
        !recorded.seen().iter().any(|u| u.contains("itunes")),
        "a printed ISBN is never asked of iTunes"
    );
}

#[tokio::test]
async fn an_ebook_on_apple_books() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Books
        .lookup(&cx, &subject("isbn:9780804139038", None, None))
        .await
        .unwrap();
    assert!(links(&found).contains(&(
        "Apple Books",
        "https://books.apple.com/us/book/id721429965",
        "itunes"
    )));
    assert!(!found.links.iter().any(|l| l.site == "Amazon"));
}

#[tokio::test]
async fn an_unknown_isbn_finds_nothing() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Books
        .lookup(&cx, &subject("isbn:9798999999993", None, None))
        .await
        .unwrap();
    assert_eq!(found, Found::default());
    assert_eq!(recorded.seen().len(), 1);
}
