//! A Wikidata item's card shows a Commons file under a GNU licence (GPL, AGPL) or Commons'
//! `Attribution` licence, credited to its author, and refuses one that names no author, against
//! recorded answers (`tests/fixtures/commons_licences`, see its provenance.json).
use pinakes_core::{
    config::{EventConfig, ResolverConfig},
    event::{adapters::service::PlatformResolver, service::EventService},
    model::ResourceMeta,
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        osm::OsmResolver,
        web::WebResolver,
        Resolver,
    },
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

mod common;

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/commons_licences/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// One recorded item, its P18 file and what its card shows.
struct Item {
    qid: &'static str,
    /// The `titles` Commons is asked for.
    title: &'static str,
    commons: &'static str,
    image: &'static str,
    page: &'static str,
    author: &'static str,
    license: &'static str,
    license_url: &'static str,
}
const SIGNAL: Item = Item {
    qid: "Q19718090",
    title: "File:Signal Android 7.28.4 blue.png",
    commons: "commons-signal-android.json",
    image: "https://thumb.wikimedia.org/wikipedia/commons/thumb/5/52/Signal_Android_7.28.4_blue.png/960px-Signal_Android_7.28.4_blue.png",
    page: "https://commons.wikimedia.org/wiki/File:Signal_Android_7.28.4_blue.png",
    author: "Albe.26.1",
    license: "AGPL-3.0-only",
    license_url: "https://www.gnu.org/licenses/agpl-3.0.html",
};
const MINDUSTRY: Item = Item {
    qid: "Q57206016",
    title: "File:Snímek obrazovky (15).png",
    commons: "commons-mindustry-screenshot.json",
    image: "https://thumb.wikimedia.org/wikipedia/commons/thumb/d/d7/Sn%C3%ADmek_obrazovky_%2815%29.png/960px-Sn%C3%ADmek_obrazovky_%2815%29.png",
    page: "https://commons.wikimedia.org/wiki/File:Sn%C3%ADmek_obrazovky_(15).png",
    author: "Mvrt CZ",
    license: "GPL-3.0-only",
    license_url: "https://www.gnu.org/licenses/gpl-3.0.html",
};
const MATRIX: Item = Item {
    qid: "Q83495",
    title: "File:The.Matrix.glmatrix.1.png",
    commons: "commons-glmatrix.json",
    image: "https://thumb.wikimedia.org/wikipedia/commons/thumb/5/53/The.Matrix.glmatrix.1.png/960px-The.Matrix.glmatrix.1.png",
    page: "https://commons.wikimedia.org/wiki/File:The.Matrix.glmatrix.1.png",
    author: "Jamie Zawinski (program); Church of emacs (screenshot)",
    license: "Attribution",
    license_url: "https://commons.wikimedia.org/wiki/Template:Attribution",
};
const ITEMS: [&Item; 3] = [&SIGNAL, &MINDUSTRY, &MATRIX];

/// How Commons answers.
#[derive(Clone, Copy, Default, PartialEq)]
enum Commons {
    /// As recorded.
    #[default]
    Recorded,
    /// The recorded answer without its author (Artist removed).
    NoAuthor,
    /// The recorded answer with its LicenseUrl the GPL 2.0 text's.
    Gpl2Url,
}
/// Answers by host and path from the recorded fixtures; records each request.
struct Recorded {
    commons: Commons,
    seen: Mutex<Vec<String>>,
}
impl Recorded {
    /// The requests to `host`.
    fn to(&self, host: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with(&format!("{host}/")))
            .cloned()
            .collect()
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
        let host = url.host_str().unwrap_or_default().to_owned();
        let query = |k: &str| {
            url.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        self.seen.lock().unwrap().push(format!(
            "{host}{}?{}",
            url.path(),
            url.query().unwrap_or_default()
        ));
        let (status, body) = match (host.as_str(), url.path()) {
            ("www.wikidata.org", path) => {
                match ITEMS
                    .iter()
                    .find(|i| path == format!("/wiki/Special:EntityData/{}.json", i.qid))
                {
                    Some(item) => (200, fixture(&format!("wd-entity-{}.json", item.qid))),
                    None => (404, b"{}".to_vec()),
                }
            }
            ("commons.wikimedia.org", "/w/api.php") => {
                assert_eq!(query("iiurlwidth"), "640");
                let titles = query("titles");
                let item = ITEMS
                    .iter()
                    .find(|i| i.title == titles)
                    .unwrap_or_else(|| panic!("unexpected Commons file {titles}"));
                let mut v: Value = serde_json::from_slice(&fixture(item.commons)).unwrap();
                let meta = v["query"]["pages"][0]["imageinfo"][0]["extmetadata"]
                    .as_object_mut()
                    .unwrap();
                match self.commons {
                    Commons::Recorded => {}
                    Commons::NoAuthor => {
                        meta.remove("Artist");
                    }
                    Commons::Gpl2Url => {
                        meta.insert(
                            "LicenseUrl".into(),
                            json!({"value": "https://www.gnu.org/licenses/old-licenses/gpl-2.0.html", "source": "commons-desc-page"}),
                        );
                    }
                }
                (200, serde_json::to_vec(&v).unwrap())
            }
            _ => (404, b"{}".to_vec()),
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status,
            retry_after_secs: 60,
            location: None,
            content_type: "application/json".into(),
            body,
        })
    }
}

/// The item's card through the Wikidata adapter, and the transport that served it.
async fn item_card(pool: &PgPool, item: &Item, commons: Commons) -> (ResourceMeta, Arc<Recorded>) {
    let t = Arc::new(Recorded {
        commons,
        seen: Mutex::default(),
    });
    let platform = PlatformResolver {
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            egress: Egress::with_backends(
                common::unpaced_egress_config(),
                Arc::new(PublicDns),
                t.clone(),
            ),
            config: ResolverConfig::default(),
            events: Some(Arc::new(EventService {
                pool: pool.clone(),
                config: EventConfig {
                    geocoding: false,
                    ..Default::default()
                },
                osm: Arc::new(OsmResolver::new(pool.clone(), Default::default()).unwrap()),
            })),
        },
    };
    let uri = format!("https://www.wikidata.org/wiki/{}", item.qid);
    (platform.resolve(&uri).await.unwrap(), t)
}
fn credit(item: &Item) -> Value {
    json!({
        "field": "image", "provider": "wikimedia_commons", "source_url": item.page,
        "author": item.author, "license": item.license, "license_url": item.license_url,
        "lang": null, "excerpted": false, "modified": false, "subject": null,
    })
}

/// Signal's screenshot (AGPL), Mindustry's (GPLv3) and The Matrix's (Commons' `Attribution`):
/// each card shows Commons' thumbnail with one credit, the file's author, the licence's SPDX ID
/// (or `Attribution`) and its text, and the file's page. One Commons request each.
#[sqlx::test(migrations = "../../migrations")]
async fn gnu_and_attribution_files_show_with_their_credit(pool: PgPool) {
    for item in ITEMS {
        let (doc, t) = item_card(&pool, item, Commons::Recorded).await;
        assert_eq!(doc.image.as_deref(), Some(item.image), "{}", item.qid);
        assert_eq!(
            serde_json::to_value(&doc.credits).unwrap(),
            json!([credit(item)]),
            "{}",
            item.qid
        );
        assert!(doc.unread_enriched.is_empty() && doc.refresh_by.is_none());
        assert_eq!(t.to("commons.wikimedia.org").len(), 1, "{}", item.qid);
    }
    let (doc, _) = item_card(&pool, &MATRIX, Commons::Recorded).await;
    assert_eq!(
        (doc.type_name.as_str(), doc.name.as_deref()),
        ("Movie", Some("The Matrix"))
    );
}

/// The licence alone does not show a file: without its author, or with a URL that names another
/// licence than its name, the card has no image and no credit.
#[sqlx::test(migrations = "../../migrations")]
async fn a_free_licence_without_its_author_shows_nothing(pool: PgPool) {
    let (shown, _) = item_card(&pool, &SIGNAL, Commons::Recorded).await;
    assert_eq!(shown.image.as_deref(), Some(SIGNAL.image));
    for item in ITEMS {
        let (doc, t) = item_card(&pool, item, Commons::NoAuthor).await;
        assert_eq!(doc.image, None, "{}", item.qid);
        assert!(doc.credits.is_empty(), "{}: {:?}", item.qid, doc.credits);
        assert!(doc.unread_enriched.is_empty(), "a refusal is an answer");
        assert_eq!(t.to("commons.wikimedia.org").len(), 1);
    }
    for item in [&SIGNAL, &MINDUSTRY] {
        let (doc, _) = item_card(&pool, item, Commons::Gpl2Url).await;
        assert_eq!(doc.image, None, "{}", item.qid);
        assert!(doc.credits.is_empty());
    }
}
