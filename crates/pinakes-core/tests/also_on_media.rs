//! The book, podcast and film sources through Egress, against the answers recorded in
//! `tests/fixtures/media` (each family's `provenance.json` names the URL of every file).
use pinakes_core::{
    also_on::{books::Books, podcasts::Podcasts, Context, Found, Source, Subject},
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
    /// The Podcast Index key each Podcast Index request carried.
    signed_with: Mutex<Vec<Option<String>>>,
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
            signed_with: Mutex::new(Vec::new()),
        })
    }
    /// The same, with one more answer.
    fn with(url: &str, status: u16, location: &str) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        recorded.answers.insert(
            url.into(),
            Answer {
                status,
                location: Some(location.into()),
                content_type: "text/html".into(),
                body: Vec::new(),
            },
        );
        Arc::new(recorded)
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
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push(url.to_string());
        if url.host_str() == Some("api.podcastindex.org") {
            assert!(options.sensitive);
            let header = |name: &str| {
                options
                    .headers
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            };
            assert_eq!(header("Authorization").map(|h| h.len()), Some(40));
            self.signed_with.lock().unwrap().push(header("X-Auth-Key"));
        }
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

/// A config with a Podcast Index key and secret (fake: the answers are recorded).
fn indexed() -> Config {
    let mut config = Config::default();
    config.podcast_index.key = Some("KEY".into());
    config.podcast_index.secret = Some("SECRET".into());
    config
}
fn feed_card(uri: &str, feed: &str, item: Option<&str>) -> Subject {
    let mut doc = ResourceMeta::new(uri, "media", 3600);
    doc.attributes.insert("podcast_feed".into(), feed.into());
    if let Some(item) = item {
        doc.attributes.insert("podcast_guid".into(), item.into());
    }
    let kind = if item.is_some() {
        "PodcastEpisode"
    } else {
        "PodcastSeries"
    };
    subject(uri, Some(kind), Some(doc))
}
const DAILY: &str = "https://podcasts.apple.com/us/podcast/the-daily/id1200361736";

#[tokio::test]
async fn an_apple_show_without_podcast_index() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Podcasts
        .lookup(&cx, &subject(DAILY, None, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple:1200361736", "wikidata:Q48741148"]
    );
    assert_eq!(
        links(&found),
        [
            (
                "Apple Podcasts",
                "https://podcasts.apple.com/us/podcast/id1200361736",
                "itunes"
            ),
            ("Pocket Casts", "https://pca.st/itunes/1200361736", "itunes"),
            ("Castro", "https://castro.fm/itunes/1200361736", "itunes"),
            (
                "Spotify",
                "https://open.spotify.com/show/3IM0lmZxpFAY7CwMuv9H4g",
                "wikidata"
            ),
            (
                "Podcast Index",
                "https://podcastindex.org/podcast/743229",
                "wikidata"
            ),
            (
                "Podchaser",
                "https://www.podchaser.com/podcasts/243479",
                "wikidata"
            ),
            ("Castbox", "https://castbox.fm/channel/id463650", "wikidata"),
        ]
    );
    assert!(!recorded.seen().iter().any(|u| u.contains("podcastindex")));
}

#[tokio::test]
async fn an_apple_show_with_podcast_index() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &subject(DAILY, None, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple:1200361736",
            "podcast:guid:304b84f0-07b0-5265-b6b7-da5cf5aeb56e",
            "wikidata:Q48741148"
        ]
    );
    assert!(links(&found).contains(&(
        "Podcast Index",
        "https://podcastindex.org/podcast/743229",
        "podcastindex"
    )));
    assert_eq!(found.facts.len(), 1);
    assert_eq!(
        *recorded.signed_with.lock().unwrap(),
        [Some("KEY".to_string())]
    );
}

#[tokio::test]
async fn an_unknown_apple_id_asks_nobody_else() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(
            &cx,
            &subject("https://podcasts.apple.com/us/podcast/id1", None, None),
        )
        .await
        .unwrap();
    assert_eq!(found, Found::default());
    assert_eq!(recorded.seen().len(), 1, "Podcast Index would name a show");
}

#[tokio::test]
async fn an_apple_episode() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Podcasts
        .lookup(
            &cx,
            &subject(&format!("{DAILY}?i=1000797051580"), None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), ["podcast:apple_episode:1000797051580"]);
    assert_eq!(
        links(&found),
        [(
            "Apple Podcasts",
            "https://podcasts.apple.com/us/podcast/id1200361736?i=1000797051580",
            "itunes"
        )]
    );
    let gone = Podcasts
        .lookup(&cx, &subject(&format!("{DAILY}?i=1"), None, None))
        .await
        .unwrap();
    assert_eq!(gone, Found::default());
}

#[tokio::test]
async fn a_feed_names_its_guid() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let found = Podcasts
        .lookup(&cx, &feed_card(feed, feed, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:guid:917393e3-1b1e-5cef-ace4-edaa54e1f810"]
    );
    assert_eq!(found.keys[0].via, "feed");
    assert!(found.links.is_empty(), "no Apple ID without Podcast Index");
}

#[tokio::test]
async fn a_moved_feed_through_podcast_index() {
    // The show's first URL now redirects: the feed it ends on may be another publisher's, so its
    // GUID is Podcast Index's, which names the first URL as the feed's own.
    let old = "https://mp3s.nashownotes.com/pc20rss.xml";
    let recorded = Recorded::with(old, 301, "https://feeds.podcastindex.org/pc20.xml");
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &feed_card(old, old, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:guid:917393e3-1b1e-5cef-ace4-edaa54e1f810",
            "podcast:apple:1584274529"
        ]
    );
    assert_eq!(found.keys[0].via, "podcastindex");
    assert_eq!(
        links(&found),
        [
            (
                "Podcast Index",
                "https://podcastindex.org/podcast/920666",
                "podcastindex"
            ),
            (
                "Apple Podcasts",
                "https://podcasts.apple.com/us/podcast/id1584274529",
                "itunes"
            ),
            ("Pocket Casts", "https://pca.st/itunes/1584274529", "itunes"),
            ("Castro", "https://castro.fm/itunes/1584274529", "itunes"),
        ]
    );
}

#[tokio::test]
async fn a_feed_episode_on_apple_podcasts() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), indexed());
    let feed = "https://feeds.simplecast.com/54nAGcIl";
    let card = feed_card(
        "https://www.nytimes.com/2026/10/19/podcasts/the-daily/putin-ukraine.html",
        feed,
        Some("1897475d-9b60-447d-ba9b-79aa09f59551"),
    );
    let found = Podcasts.lookup(&cx, &card).await.unwrap();
    assert_eq!(keys(&found), ["podcast:apple_episode:1000797051580"]);
    assert_eq!(found.links.len(), 1);
    // Without Podcast Index the feed has no Apple ID.
    let found = Podcasts
        .lookup(&crate::cx(Recorded::new(), Config::default()), &card)
        .await
        .unwrap();
    assert_eq!(found, Found::default());
}
