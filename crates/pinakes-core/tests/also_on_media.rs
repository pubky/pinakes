//! The book, podcast and film sources through Egress, against the answers recorded in
//! `tests/fixtures/media` (each family's `provenance.json` names the URL of every file).
use pinakes_core::{
    also_on::{
        books::Books,
        films::Films,
        podcasts::{derived_guid, item_key, Podcasts},
        Context, Fact, Found, Source, Subject,
    },
    config::{Config, EgressConfig},
    model::ResourceMeta,
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        RetryLater,
    },
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
                // A refusal is served only where a test asks for it (`with_recorded`).
                if name.ends_with("-401") {
                    continue;
                }
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
    /// The same, with recorded answers not served by default, each in place of its URL's.
    fn with_recorded(more: &[(&str, &str)]) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        for (family, name) in more {
            let provenance: serde_json::Value =
                serde_json::from_slice(&read(family, "provenance.json")).unwrap();
            let p = &provenance[*name];
            recorded.answers.insert(
                p["url"].as_str().unwrap().into(),
                Answer {
                    status: p["status"].as_u64().unwrap() as u16,
                    location: None,
                    content_type: p["content_type"].as_str().unwrap_or("").to_owned(),
                    body: read(family, p["file"].as_str().unwrap()),
                },
            );
        }
        Arc::new(recorded)
    }
    /// The same, with more answers: a URL and its status, and its `Location` or HTML body.
    fn with(more: &[(&str, u16, &str)]) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        for (url, status, then) in more {
            let redirect = (300..400).contains(status);
            recorded.answers.insert(
                (*url).into(),
                Answer {
                    status: *status,
                    location: redirect.then(|| (*then).into()),
                    content_type: "text/html".into(),
                    body: if redirect {
                        Vec::new()
                    } else {
                        then.as_bytes().to_vec()
                    },
                },
            );
        }
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
        if url.host_str() == Some("unreadable.example") {
            anyhow::bail!("body over the cap");
        }
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
            "openlibrary:work:OL468431W"
        ]
    );
    // The work's Q-id is a fact: an edition reaches the item one way only, and the Open
    // Library work key already joins the editions.
    assert!(found.facts.contains(&Fact::new("wikidata", "Q214371")));
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
        found
            .links
            .iter()
            .filter(|l| l.via == "wikidata")
            .all(|l| l.kind.as_deref() == Some("work")),
        "the item's pages are the work's"
    );
    assert!(
        !recorded.seen().iter().any(|u| u.contains("itunes")),
        "a printed ISBN is never asked of iTunes"
    );
}

#[tokio::test]
async fn a_work_link() {
    let cx = cx(Recorded::new(), Config::default());
    let hobbit = Books
        .lookup(
            &cx,
            &subject(
                "https://openlibrary.org/works/OL27482W/The_Hobbit",
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(keys(&hobbit), ["openlibrary:work:OL27482W"]);
    assert_eq!(hobbit.facts, [Fact::new("wikidata", "Q74287")]);
    assert!(links(&hobbit).contains(&(
        "Goodreads",
        "https://www.goodreads.com/work/editions/1540236",
        "wikidata"
    )));
    assert!(
        hobbit.links.iter().all(|l| l.kind.is_none()),
        "the work itself"
    );
    // Dune's work has no Wikidata item naming it (the item names merged and wrong IDs): the key
    // only. A merged ID is followed to the work it is now.
    let dune = Books
        .lookup(
            &cx,
            &subject("https://openlibrary.org/works/OL893414W", None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&dune), ["openlibrary:work:OL893414W"]);
    assert!(dune.links.is_empty() && dune.facts.is_empty());
    let merged = Books
        .lookup(
            &cx,
            &subject("https://openlibrary.org/works/OL893415W", None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&merged), ["openlibrary:work:OL893414W"]);
    assert_eq!(
        links(&merged),
        [(
            "Open Library",
            "https://openlibrary.org/works/OL893414W",
            "openlibrary"
        )]
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
const GUID_DAILY: &str = "304b84f0-07b0-5265-b6b7-da5cf5aeb56e";

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
        ],
        "never the card's own service"
    );
    assert!(found.facts.is_empty(), "no GUID without Podcast Index");
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
    assert_eq!(found.keys[1].via, "podcastindex");
    assert!(links(&found).contains(&(
        "Podcast Index",
        "https://podcastindex.org/podcast/743229",
        "podcastindex"
    )));
    assert_eq!(found.facts, [Fact::new("podcast_guid", GUID_DAILY)]);
    assert_eq!(
        *recorded.signed_with.lock().unwrap(),
        [Some("KEY".to_string())]
    );
}

#[tokio::test]
async fn a_refused_podcast_index_key_fails_nothing() {
    // Podcast Index refuses the key (401): everything else still counts.
    let recorded = Recorded::with_recorded(&[("podcasts", "pi-byitunesid-401")]);
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &subject(DAILY, None, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple:1200361736", "wikidata:Q48741148"]
    );
    assert!(sites(&found).contains(&"Pocket Casts"));
    assert!(found.facts.is_empty());
    // When nothing at all could be read (iTunes answering 503), the retry is passed on.
    let show = "https://itunes.apple.com/lookup?id=1200361736&entity=podcast&country=us";
    let busy = Recorded::with(&[(show, 503, "")]);
    let error = Podcasts
        .lookup(&crate::cx(busy, indexed()), &subject(DAILY, None, None))
        .await
        .unwrap_err();
    assert!(error.is::<RetryLater>(), "{error:#}");
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
async fn players_name_a_show_by_its_apple_id() {
    let cx = cx(Recorded::new(), Config::default());
    let found = Podcasts
        .lookup(
            &cx,
            &subject("https://pca.st/itunes/1200361736", None, None),
        )
        .await
        .unwrap();
    assert_eq!(found.keys[0].key, "podcast:apple:1200361736");
    assert!(sites(&found).contains(&"Castro"));
    assert!(!sites(&found).contains(&"Pocket Casts"), "the card itself");
    // An ID iTunes doesn't know still names itself: the key, from the URL alone.
    let found = Podcasts
        .lookup(&cx, &subject("https://castro.fm/itunes/1", None, None))
        .await
        .unwrap();
    assert_eq!(keys(&found), ["podcast:apple:1"]);
    assert_eq!(found.keys[0].via, "url");
    assert!(found.links.is_empty());
}

#[tokio::test]
async fn a_podcast_index_page() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(
            &cx,
            &subject("https://podcastindex.org/podcast/743229", None, None),
        )
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
        "Podcast feed",
        "https://feeds.simplecast.com/54nAGcIl",
        "podcastindex"
    )));
    assert!(sites(&found).contains(&"Apple Podcasts"));
    assert!(!sites(&found).contains(&"Podcast Index"), "the card itself");
    assert_eq!(found.facts, [Fact::new("podcast_guid", GUID_DAILY)]);
    // Without a key a Podcast Index page has nothing to read.
    let found = Podcasts
        .lookup(
            &crate::cx(Recorded::new(), Config::default()),
            &subject("https://podcastindex.org/podcast/743229", None, None),
        )
        .await
        .unwrap();
    assert_eq!(found, Found::default());
}

#[tokio::test]
async fn an_episode_has_one_key_on_apple_and_podcast_index() {
    let cx = cx(Recorded::new(), indexed());
    let apple = Podcasts
        .lookup(
            &cx,
            &subject(&format!("{DAILY}?i=1000797051580"), None, None),
        )
        .await
        .unwrap();
    let item = item_key(GUID_DAILY, "1897475d-9b60-447d-ba9b-79aa09f59551").unwrap();
    assert_eq!(
        keys(&apple),
        ["podcast:apple_episode:1000797051580", item.as_str()]
    );
    assert!(apple.links.is_empty(), "its own Apple page is the card");
    let index = Podcasts
        .lookup(
            &cx,
            &subject(
                "https://podcastindex.org/podcast/743229?episode=61953304662",
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        keys(&index),
        ["podcast:apple_episode:1000797051580", item.as_str()]
    );
    assert_eq!(
        links(&index),
        [(
            "Apple Podcasts",
            "https://podcasts.apple.com/us/podcast/id1200361736?i=1000797051580",
            "itunes"
        )]
    );
    // Without Podcast Index the Apple episode has its track only; a gone episode has nothing.
    let cx = crate::cx(Recorded::new(), Config::default());
    let alone = Podcasts
        .lookup(
            &cx,
            &subject(&format!("{DAILY}?i=1000797051580"), None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&alone), ["podcast:apple_episode:1000797051580"]);
    let gone = Podcasts
        .lookup(&cx, &subject(&format!("{DAILY}?i=1"), None, None))
        .await
        .unwrap();
    assert_eq!(gone, Found::default());
}

#[tokio::test]
async fn a_feed_names_its_guid_as_a_fact() {
    // Without Podcast Index's word, the feed's own GUID is a fact, never a key.
    let cx = cx(Recorded::new(), Config::default());
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let found = Podcasts
        .lookup(&cx, &feed_card(feed, feed, None))
        .await
        .unwrap();
    assert!(found.keys.is_empty());
    assert_eq!(
        found.facts,
        [Fact::new(
            "podcast_guid",
            "917393e3-1b1e-5cef-ace4-edaa54e1f810"
        )]
    );
    assert!(found.links.is_empty(), "no Apple ID without Podcast Index");
}

#[tokio::test]
async fn a_moved_feed_through_podcast_index() {
    // The show's first URL now redirects: the feed it ends on may be another publisher's, so its
    // GUID is Podcast Index's, which names the first URL as the feed's own.
    let old = "https://mp3s.nashownotes.com/pc20rss.xml";
    let recorded = Recorded::with(&[(old, 301, "https://feeds.podcastindex.org/pc20.xml")]);
    let cx = cx(recorded.clone(), indexed());
    let found = Podcasts
        .lookup(&cx, &feed_card(old, old, None))
        .await
        .unwrap();
    assert_eq!(
        keys(&found),
        [
            "podcast:apple:1584274529",
            "podcast:guid:917393e3-1b1e-5cef-ace4-edaa54e1f810"
        ]
    );
    assert_eq!(found.keys[1].via, "podcastindex");
    assert_eq!(
        found.facts,
        [Fact::new(
            "podcast_guid",
            "917393e3-1b1e-5cef-ace4-edaa54e1f810"
        )]
    );
    assert_eq!(
        sites(&found),
        ["Apple Podcasts", "Pocket Casts", "Castro", "Podcast Index"]
    );
}

#[tokio::test]
async fn a_feed_item_page_joins_its_episode() {
    // Podcasting 2.0's item PC20-275 names its page as `<link>`; Podcast Index vouches for the
    // feed's GUID, and iTunes lists the item as track 1000796718231.
    let feed = "https://feeds.podcastindex.org/pc20.xml";
    let page = "https://www.podpage.com/podcasting-20/off-the-rails/";
    let cx = cx(Recorded::new(), indexed());
    let found = Podcasts
        .lookup(&cx, &feed_card(page, feed, Some("PC20-275")))
        .await
        .unwrap();
    let item = item_key("917393e3-1b1e-5cef-ace4-edaa54e1f810", "PC20-275").unwrap();
    assert_eq!(
        keys(&found),
        ["podcast:apple_episode:1000796718231", item.as_str()]
    );
    // Another page claiming the item gets its Apple track (by the item GUID) but not the key.
    let other = Podcasts
        .lookup(
            &cx,
            &feed_card("https://example.org/copy/", feed, Some("PC20-275")),
        )
        .await
        .unwrap();
    assert_eq!(keys(&other), ["podcast:apple_episode:1000796718231"]);
}

#[tokio::test]
async fn a_feed_episode_on_apple_podcasts() {
    let feed = "https://feeds.simplecast.com/54nAGcIl";
    // The Daily's feed (not recorded: too large) is unreadable here.
    let recorded = Recorded::with(&[(feed, 404, "")]);
    let cx = cx(recorded.clone(), indexed());
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

/// Letterboxd's film page, where its `imdb/` redirect for The Shawshank Redemption ends.
const SHAWSHANK_PAGE: (&str, u16, &str) = (
    "https://letterboxd.com/film/the-shawshank-redemption/",
    200,
    "<html><head><title>The Shawshank Redemption (1994)</title></head><body></body></html>",
);
fn sites(found: &Found) -> Vec<&str> {
    found.links.iter().map(|l| l.site.as_str()).collect()
}

#[tokio::test]
async fn a_film_by_its_imdb_link() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let s = subject("https://www.imdb.com/title/tt0111161/", None, None);
    assert!(Films.applies(&s));
    let found = Films.lookup(&cx, &s).await.unwrap();
    assert_eq!(keys(&found), ["imdb:tt0111161", "wikidata:Q172241"]);
    assert_eq!(found.keys[0].via, "imdb");
    assert_eq!(
        sites(&found),
        [
            "TMDB",
            "Letterboxd",
            "Trakt",
            "Rotten Tomatoes",
            "TheTVDB",
            "Apple TV",
            "Netflix",
            "MUBI"
        ],
        "never the card's own service"
    );
    let seen = recorded.seen();
    assert!(
        !seen.iter().any(|u| u.contains("letterboxd.com")),
        "the item has its Letterboxd page"
    );
    // A film is no show on TVmaze.
    assert!(seen.contains(&"https://api.tvmaze.com/lookup/shows?imdb=tt0111161".to_string()));
    assert!(!sites(&found).contains(&"TVmaze"));
}

#[tokio::test]
async fn a_film_by_its_tmdb_link_reaches_its_imdb_id_through_wikidata() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Films
        .lookup(
            &cx,
            &subject(
                "https://www.themoviedb.org/movie/278-the-shawshank-redemption",
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), ["imdb:tt0111161", "wikidata:Q172241"]);
    assert_eq!(found.keys[0].via, "wikidata");
    assert!(sites(&found).contains(&"IMDb"));
    assert!(!sites(&found).contains(&"TMDB"));
}

#[tokio::test]
async fn a_series_by_its_tvmaze_link() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Films
        .lookup(
            &cx,
            &subject("https://www.tvmaze.com/shows/169/breaking-bad", None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), ["imdb:tt0903747", "wikidata:Q1079"]);
    assert_eq!(found.keys[0].via, "tvmaze");
    assert!(!sites(&found).contains(&"TVmaze"));
    assert!(sites(&found).contains(&"IMDb"));
    // Letterboxd has no film for a series: its imdb/ page answers "not found".
    assert!(recorded
        .seen()
        .contains(&"https://letterboxd.com/imdb/tt0903747/".to_string()));
    assert!(!sites(&found).contains(&"Letterboxd"));
}

#[tokio::test]
async fn letterboxd_and_tvmaze_by_imdb_id() {
    // An IMDb ID Wikidata has no item for: Letterboxd's redirect names the film exactly.
    let tt = "https://www.imdb.com/title/tt9999999/";
    let recorded = Recorded::with(&[
        (
            "https://letterboxd.com/imdb/tt9999999/",
            302,
            "/film/the-shawshank-redemption/",
        ),
        SHAWSHANK_PAGE,
    ]);
    let cx = cx(recorded.clone(), Config::default());
    let found = Films.lookup(&cx, &subject(tt, None, None)).await.unwrap();
    assert_eq!(keys(&found), ["imdb:tt9999999"]);
    assert_eq!(
        links(&found),
        [(
            "Letterboxd",
            "https://letterboxd.com/film/the-shawshank-redemption/",
            "letterboxd"
        )]
    );
    // Without the redirect, and with TVmaze's 404, nothing.
    let found = Films
        .lookup(
            &crate::cx(Recorded::new(), Config::default()),
            &subject(tt, None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), ["imdb:tt9999999"]);
    assert!(found.links.is_empty());
}

#[tokio::test]
async fn an_unreadable_feed_has_no_guid_and_fails_nothing() {
    let feed = "https://unreadable.example/feed.xml";
    let found = Podcasts
        .lookup(
            &cx(Recorded::new(), Config::default()),
            &feed_card(feed, feed, None),
        )
        .await
        .unwrap();
    assert!(found.keys.is_empty() && found.links.is_empty());
    assert_eq!(
        found.facts,
        [Fact::new("podcast_guid", derived_guid(feed).unwrap(),)],
        "the derived GUID, a fact only"
    );
}
