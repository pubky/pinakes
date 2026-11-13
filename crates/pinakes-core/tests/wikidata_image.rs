//! A Wikidata item's card shows its image (`P18`) only when Commons says the file is free to
//! show, with its credit, through the Wikidata adapter and a Wikipedia place's page, against
//! recorded answers (`tests/fixtures/wikidata_image`, see its provenance.json).
use pinakes_core::{
    config::{EventConfig, ResolverConfig},
    event::{adapters::service::PlatformResolver, service::EventService},
    model::{CreditField, ResourceMeta},
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

const ITEM: &str = "https://www.wikidata.org/wiki/Q359924";
const ARTICLE: &str = "https://en.wikipedia.org/wiki/Golden_Gate_Hotel_and_Casino";
const IMAGE: &str = "https://thumb.wikimedia.org/wikipedia/commons/thumb/c/c5/M-408116720.jpg/960px-M-408116720.jpg";
const FILE_PATH: &str = "https://commons.wikimedia.org/wiki/Special:FilePath/M%2D408116720%2Ejpg";

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/wikidata_image/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
/// A recorded JSON answer with one edit (provenance.json names each).
fn edited(name: &str, edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&fixture(name)).unwrap();
    edit(&mut v);
    serde_json::to_vec(&v).unwrap()
}
/// The recorded Commons answer with its licence fields replaced.
fn licensed(fields: Value) -> Vec<u8> {
    edited("commons-M-408116720.json", |v| {
        let meta = &mut v["query"]["pages"][0]["imageinfo"][0]["extmetadata"];
        for (key, value) in fields.as_object().unwrap() {
            if value.is_null() {
                meta.as_object_mut().unwrap().remove(key);
            } else {
                meta[key] = json!({"value": value, "source": "commons-desc-page"});
            }
        }
    })
}

/// How Commons answers.
#[derive(Clone, Copy, Default, PartialEq)]
enum Commons {
    /// As recorded: CC BY 2.0, by bjora857.
    #[default]
    Recorded,
    /// Marked non-free.
    NonFree,
    /// Under a licence off the list (GFDL).
    Gfdl,
    /// Under CC0, without an author.
    Cc0,
    /// 503 with Retry-After: 120.
    Busy,
    /// Never answers.
    Silent,
}
#[derive(Clone, Copy, Default)]
struct Mode {
    commons: Commons,
    /// The item has no image (its P18 statement removed).
    no_image: bool,
}
/// Answers by host and path from the recorded fixtures; records each request.
struct Recorded {
    mode: Mode,
    seen: Mutex<Vec<String>>,
}
impl Recorded {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            seen: Mutex::default(),
        })
    }
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
        let (status, retry_after, content_type, body) = match (host.as_str(), url.path()) {
            ("www.wikidata.org", "/wiki/Special:EntityData/Q359924.json") => {
                let body = if self.mode.no_image {
                    edited("wd-entity-Q359924.json", |v| {
                        v["entities"]["Q359924"]["claims"]
                            .as_object_mut()
                            .unwrap()
                            .remove("P18");
                    })
                } else {
                    fixture("wd-entity-Q359924.json")
                };
                (200, 60, "application/json", body)
            }
            ("commons.wikimedia.org", "/w/api.php") => {
                assert_eq!(query("titles"), "File:M-408116720.jpg");
                assert_eq!(query("iiurlwidth"), "640");
                let body = match self.mode.commons {
                    Commons::Recorded => fixture("commons-M-408116720.json"),
                    Commons::NonFree => licensed(json!({"NonFree": "true"})),
                    Commons::Gfdl => licensed(
                        json!({"License": "gfdl", "LicenseShortName": "GFDL", "LicenseUrl": null}),
                    ),
                    Commons::Cc0 => licensed(json!({
                        "License": "cc0", "LicenseShortName": "CC0",
                        "LicenseUrl": "https://creativecommons.org/publicdomain/zero/1.0/deed.en",
                        "Artist": null,
                    })),
                    Commons::Busy => b"{}".to_vec(),
                    Commons::Silent => std::future::pending::<Vec<u8>>().await,
                };
                let (status, retry) = if self.mode.commons == Commons::Busy {
                    (503, 120)
                } else {
                    (200, 60)
                };
                (status, retry, "application/json", body)
            }
            // Wikipedia serves no summary: the card is read from the article's page.
            ("en.wikipedia.org", path) if path.starts_with("/api/rest_v1/page/summary/") => {
                (404, 60, "application/json", b"{}".to_vec())
            }
            ("en.wikipedia.org", "/wiki/Golden_Gate_Hotel_and_Casino") => (
                200,
                60,
                "text/html; charset=UTF-8",
                br#"<!DOCTYPE html><html><head><title>Golden Gate Hotel and Casino - Wikipedia</title>
<script>RLCONF={"wgPageName":"Golden_Gate_Hotel_and_Casino","wgWikibaseItemId":"Q359924"};</script>
</head><body><table class="infobox vcard"><tr><td><span class="geo">36.1714; -115.146</span></td></tr></table>
<p>The Golden Gate Hotel and Casino is a hotel and casino in Las Vegas, Nevada.</p></body></html>"#
                    .to_vec(),
            ),
            _ => (404, 60, "text/plain", b"not found".to_vec()),
        };
        Ok(Fetched {
            retry_after_secs: retry_after,
            ..common::fetched(url.clone(), status, content_type.into(), body)
        })
    }
}

fn web(pool: &PgPool, transport: Arc<Recorded>) -> WebResolver {
    WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            transport,
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
    }
}
/// Q359924's card through the Wikidata adapter, and the transport that served it.
async fn item_card(pool: &PgPool, mode: Mode) -> (ResourceMeta, Arc<Recorded>) {
    let t = Recorded::new(mode);
    let platform = PlatformResolver {
        web: web(pool, t.clone()),
    };
    (platform.resolve(ITEM).await.unwrap(), t)
}
fn credits(doc: &ResourceMeta) -> Value {
    serde_json::to_value(&doc.credits).unwrap()
}
fn image_credit(author: Option<&str>, license: &str, license_url: &str) -> Value {
    json!({
        "field": "image", "provider": "wikimedia_commons",
        "source_url": "https://commons.wikimedia.org/wiki/File:M-408116720.jpg",
        "author": author, "license": license, "license_url": license_url,
        "lang": null, "excerpted": false, "modified": false, "subject": null,
    })
}

/// A place's item with a CC BY 2.0 file: the card shows Commons' thumbnail with the file's
/// author, licence and page, never the file's Special:FilePath link. Its description is
/// Wikidata's own (CC0) and carries no credit. One Commons request, of the width the enriched
/// cards ask for.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_item_shows_its_free_image_with_its_credit(pool: PgPool) {
    let (doc, t) = item_card(&pool, Mode::default()).await;
    assert_eq!(doc.type_name, "Place");
    assert_eq!(doc.name.as_deref(), Some("Golden Gate Hotel and Casino"));
    assert_eq!(
        doc.description.as_deref(),
        Some("hotel and casino in Las Vegas, Nevada, United States")
    );
    assert_eq!(doc.image.as_deref(), Some(IMAGE));
    assert_eq!(
        credits(&doc),
        json!([image_credit(
            Some("bjora857"),
            "CC-BY-2.0",
            "https://creativecommons.org/licenses/by/2.0/"
        )])
    );
    assert!(doc.unread_enriched.is_empty() && doc.refresh_by.is_none());
    assert_eq!(t.to("commons.wikimedia.org").len(), 1);
    assert!(t.to("upload.wikimedia.org").is_empty() && t.to("thumb.wikimedia.org").is_empty());
}

/// A file marked non-free, or under a licence off the list, gives no image and no credit; a CC0
/// file is shown, credited without an author. An item without an image asks Commons nothing.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn only_a_free_file_is_shown(pool: PgPool) {
    for commons in [Commons::NonFree, Commons::Gfdl] {
        let (doc, t) = item_card(
            &pool,
            Mode {
                commons,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(doc.image, None);
        assert!(doc.credits.is_empty(), "{:?}", doc.credits);
        assert!(doc.unread_enriched.is_empty());
        assert_eq!(t.to("commons.wikimedia.org").len(), 1);
    }
    let (doc, _) = item_card(
        &pool,
        Mode {
            commons: Commons::Cc0,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(doc.image.as_deref(), Some(IMAGE));
    assert_eq!(
        credits(&doc),
        json!([image_credit(
            None,
            "CC0-1.0",
            "https://creativecommons.org/publicdomain/zero/1.0/"
        )])
    );
    let (doc, t) = item_card(
        &pool,
        Mode {
            no_image: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(doc.name.as_deref(), Some("Golden Gate Hotel and Casino"));
    assert!(doc.description.is_some());
    assert_eq!(doc.image, None);
    assert!(doc.credits.is_empty());
    assert!(t.to("commons.wikimedia.org").is_empty());
}

/// A busy Commons, or one that does not answer in time, leaves the image unread: no image from
/// this reading, an early read again, and the stored card's credited image kept
/// (`keep_enriched`). A stored image without a credit (a Special:FilePath link) is not kept.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_busy_commons_keeps_the_stored_image(pool: PgPool) {
    let (stored, _) = item_card(&pool, Mode::default()).await;
    assert_eq!(stored.image.as_deref(), Some(IMAGE));
    for commons in [Commons::Busy, Commons::Silent] {
        let started = std::time::Instant::now();
        let (doc, t) = item_card(
            &pool,
            Mode {
                commons,
                ..Default::default()
            },
        )
        .await;
        // Within the card's wait for Commons, far inside its own read.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(t.to("commons.wikimedia.org").len(), 1);
        assert_eq!(doc.image, None);
        assert!(doc.credits.is_empty());
        assert_eq!(doc.unread_enriched, vec![CreditField::Image]);
        assert!(doc.refresh_by.is_some());
        assert_eq!(doc.name, stored.name);

        let mut kept = doc.clone();
        kept.keep_enriched(&stored);
        assert_eq!(kept.image.as_deref(), Some(IMAGE));
        assert_eq!(kept.credits, stored.credits);
        assert!(kept.unread_enriched.is_empty());

        let mut uncredited = stored.clone();
        uncredited.image = Some(FILE_PATH.into());
        uncredited.credits.clear();
        let mut fresh = doc.clone();
        fresh.keep_enriched(&uncredited);
        assert_eq!(fresh.image, None);
        assert!(fresh.credits.is_empty());
    }
}

/// A Wikipedia page about a place, read from its Wikidata item when Wikipedia serves no summary
/// of the article, shows the item's image the same way: verified on Commons, with its credit.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_wikipedia_place_shows_its_item_image_with_its_credit(pool: PgPool) {
    for (commons, image) in [(Commons::Recorded, Some(IMAGE)), (Commons::NonFree, None)] {
        let t = Recorded::new(Mode {
            commons,
            ..Default::default()
        });
        let doc = web(&pool, t.clone())
            .resolve_with_policy(ARTICLE, Default::default())
            .await
            .unwrap();
        assert_eq!(doc.type_name, "Place");
        assert_eq!(doc.name.as_deref(), Some("Golden Gate Hotel and Casino"));
        assert_eq!(doc.image.as_deref(), image);
        let expected = match image {
            Some(_) => json!([image_credit(
                Some("bjora857"),
                "CC-BY-2.0",
                "https://creativecommons.org/licenses/by/2.0/"
            )]),
            None => json!([]),
        };
        assert_eq!(credits(&doc), expected);
        assert_eq!(t.to("www.wikidata.org").len(), 1);
        assert_eq!(t.to("commons.wikimedia.org").len(), 1);
    }
}
