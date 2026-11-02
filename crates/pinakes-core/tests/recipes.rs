//! Every named example in the public OpenAPI document runs against a fixture
//! built from the same seed URIs and returns data, and docs/api.md lists every recipe.
use axum::{body::Body, extract::ConnectInfo, http::Request, routing::get, Json, Router};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, OsmConfig, QueueConfig},
    model::{identity, Point, ResourceMeta},
    queue, store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
mod common;
use common::conforms;
use tower::ServiceExt;

/// A shop's product page.
const ALTERNATE: &str =
    "https://www.alternate.ch/Corsair/SABRE-v3-PRO-PureVector-Ultralight-Ergo-Gaming-Maus/html/product/100222861";
/// A seed identity: it tagged every seed entry, and the reach example's viewer follows it.
const SEED: &str = "pubkyseedauthorpubkyseedauthorpubkyseedauthorpubkyse";
/// A second identity: it tagged the two Bitcoin events too, so related has a crowd of two.
const SECOND: &str = "pubkysecondtaggerpubkysecondtaggerpubkysecondtaggerp";

fn recorded(url: &str, content_type: &str, body: &[u8]) -> pinakes_core::resolver::egress::Fetched {
    pinakes_core::resolver::egress::Fetched {
        permanent_redirect: false,
        truncated: false,
        url: url.parse().unwrap(),
        body: body.to_vec(),
        content_type: content_type.into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    }
}

async fn save(pool: &PgPool, doc: &ResourceMeta) -> String {
    let id = store::ensure_resource(pool, &doc.url).await.unwrap();
    store::save_document(pool, &id, 0, doc).await.unwrap();
    id
}

fn card(uri: &str, source: &str, kind: &str, name: &str) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, source, 7 * 86400);
    doc.set_type(kind);
    doc.name = Some(name.into());
    doc
}

/// The seed entries the recipes touch, resolved offline: recorded registry responses, an OSM
/// cache entry, a parsed holiday calendar and synthetic cards, each tagged with its seed labels.
async fn fixture(pool: &PgPool) {
    let now = chrono::Utc::now();
    let lugano = Point {
        lat: 46.0037,
        lon: 8.9511,
    };
    let mut docs = vec![
        card(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "media",
            "VideoObject",
            "Rick Astley - Never Gonna Give You Up",
        ),
        card("isbn:9781119473862", "isbn", "Book", "Mastering Bitcoin"),
        card(
            "https://en.wikipedia.org/wiki/Bitcoin",
            "web",
            "Article",
            "Bitcoin - Wikipedia",
        ),
    ];
    let mut event = card(
        "https://luma.com/376whqyw",
        "event_platform",
        "Event",
        "Bitcoin conference Lugano",
    );
    event.start = Some(now + chrono::Duration::days(30));
    event.end = Some(now + chrono::Duration::days(30) + chrono::Duration::hours(8));
    event.location = Some(lugano.clone());
    docs.push(event);
    // A Zurich meetup later today, a Mapky post and a plain web page.
    let mut meetup = card(
        "https://www.meetup.com/bitcoin-meetup-switzerland/events/316894744/",
        "event_platform",
        "Event",
        "Bitcoin meetup Zurich",
    );
    meetup.start = Some(now + chrono::Duration::hours(6));
    meetup.location = Some(Point {
        lat: 47.3769,
        lon: 8.5417,
    });
    // The same meetup on its organiser's site: possibly the same event (`maybe_same`).
    let mut organiser = card(
        "https://bitcoinmeetup.ch/events/zurich/",
        "web",
        "Event",
        "Bitcoin Meetup Zürich 🍻 21.10.",
    );
    organiser.start = meetup.start.map(|s| s + chrono::Duration::minutes(2));
    organiser.location = Some(Point {
        lat: 47.3772,
        lon: 8.5420,
    });
    docs.push(meetup);
    docs.push(organiser);
    let mut post = card(
        "pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/00351MT54YS9G",
        "pubky",
        "SocialMediaPosting",
        "Great place to enjoy a beer!",
    );
    post.site = "mapky.app".into();
    docs.push(post);
    docs.push(card(
        "https://example.org/",
        "web",
        "WebPage",
        "Example Domain",
    ));
    let mut party = card(
        "https://partiful.com/e/AK0FMV5lu2PmfSyxuXwY",
        "event_platform",
        "Event",
        "A party later this week",
    );
    party.start = Some(now + chrono::Duration::days(3));
    docs.push(party);
    let mut wiki = card(
        "https://en.wikipedia.org/wiki/Lugano",
        "web",
        "Article",
        "Lugano - Wikipedia",
    );
    wiki.location = Some(lugano.clone());
    docs.push(wiki);
    // A recipe with its ingredients.
    let mut lasagne = card(
        "https://www.bbcgoodfood.com/recipes/classic-lasagne-0",
        "jsonld",
        "Recipe",
        "Classic lasagne",
    );
    lasagne.attributes.insert(
        "ingredients".into(),
        "olive oil; lean beef mince; passata; ball mozzarella".into(),
    );
    docs.push(lasagne);
    // Two job postings: a remote one, and one on site in Lugano.
    for (uri, name, remote) in [
        (
            "https://bitcoinerjobs.com/job/1682142-staff-software-engineer-river",
            "Staff Software Engineer",
            true,
        ),
        (
            "https://bitcoinerjobs.com/job/2563734-investment-associateanalyst-battery",
            "Investment Associate/Analyst",
            false,
        ),
    ] {
        let mut job = card(uri, "web", "JobPosting", name);
        job.start = Some(now - chrono::Duration::days(20));
        job.end = Some(now + chrono::Duration::days(20));
        if remote {
            job.attributes.insert("remote".into(), "true".into());
        } else {
            job.location = Some(lugano.clone());
        }
        docs.push(job);
    }
    for doc in &docs {
        save(pool, doc).await;
    }
    // A geo pin, resolved by the offline geo resolver.
    store::ensure_resource(pool, "geo:46.0037,8.9511")
        .await
        .unwrap();
    let job = queue::claim(pool, "geo", &QueueConfig::default())
        .await
        .unwrap()
        .unwrap();
    queue::run_job(
        pool,
        &pinakes_core::resolver::geo::GeoResolver,
        &QueueConfig::default(),
        &job,
    )
    .await
    .unwrap();
    // A node (Berlin), a way and a relation from fresh osm_place cache entries: the OSM worker
    // maps them without Nominatim.
    let berlin = "https://www.openstreetmap.org/node/240109189";
    let nominatim: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/nominatim.json")).unwrap();
    for record in &nominatim {
        let place = pinakes_core::resolver::osm::map_place(record, 30 * 86400).unwrap();
        let at = place.location.as_ref().unwrap();
        let (kind, osm_id) = (
            record["osm_type"].as_str().unwrap(),
            record["osm_id"].as_i64().unwrap(),
        );
        sqlx::query("INSERT INTO osm_place(osm_type,osm_id,point,name,category,type,type_path,doc,fetch_doc,fetched_at,refresh_at,status) VALUES($8,$9,ST_SetSRID(ST_MakePoint($1,$2),4326),$3,$4,$5,$6,$7,$7,now(),now()+interval '30 days','ok')")
            .bind(at.lon)
            .bind(at.lat)
            .bind(&place.name)
            .bind(&place.category)
            .bind(&place.type_name)
            .bind(&place.type_path)
            .bind(serde_json::to_value(&place).unwrap())
            .bind(kind)
            .bind(osm_id)
            .execute(pool)
            .await
            .unwrap();
        store::ensure_resource(
            pool,
            &format!("https://www.openstreetmap.org/{kind}/{osm_id}"),
        )
        .await
        .unwrap();
    }
    let osm = pinakes_core::resolver::osm::OsmResolver::new(
        pool.clone(),
        OsmConfig {
            nominatim_url: "http://127.0.0.1:9".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let jobs = queue::claim_batch(pool, "osm", &QueueConfig::default(), 50)
        .await
        .unwrap();
    queue::run_batch(pool, &osm, &QueueConfig::default(), &jobs)
        .await
        .unwrap();
    let state: (String, Option<String>) =
        sqlx::query_as("SELECT status,last_error FROM resource WHERE uri=$1")
            .bind(berlin)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(state.0, "ok", "{state:?}");
    // The Swiss holiday feed: one date past (upcoming events must not list it) and
    // two ahead.
    let feed = "https://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics";
    let day = |d: i64| {
        (now + chrono::Duration::days(d))
            .format("%Y%m%d")
            .to_string()
    };
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:holiday-a\r\nDTSTART;VALUE=DATE:{}\r\nSUMMARY:Holiday A\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:holiday-b\r\nDTSTART;VALUE=DATE:{}\r\nSUMMARY:Holiday B\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:holiday-past\r\nDTSTART;VALUE=DATE:{}\r\nSUMMARY:Past holiday\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        day(20),
        day(60),
        day(-20)
    );
    let calendar =
        pinakes_core::event::calendar::parse(feed, ics.as_bytes(), &Default::default(), now)
            .unwrap();
    let feed_id = store::ensure_resource(pool, feed).await.unwrap();
    store::save_document(pool, &feed_id, 0, &calendar)
        .await
        .unwrap();
    // Two more calendars, each with dates of its own.
    for (uri, prefix) in [
        (
            "https://www.calendarlabs.com/ical-calendar/ics/76/US_Holidays.ics",
            "us-holiday-",
        ),
        (
            "https://calendar.google.com/calendar/ical/en.german%23holiday%40group.v.calendar.google.com/public/basic.ics",
            "german-holiday-",
        ),
    ] {
        let calendar = pinakes_core::event::calendar::parse(
            uri,
            ics.replace("holiday-", prefix).as_bytes(),
            &Default::default(),
            now,
        )
        .unwrap();
        let id = store::ensure_resource(pool, uri).await.unwrap();
        store::save_document(pool, &id, 0, &calendar).await.unwrap();
    }
    // The Nature article and its DOI card form one group (recorded Crossref and page head).
    let csl = include_bytes!("fixtures/registry/crossref-nature12373.csl.json");
    let doi = pinakes_core::resolver::cards::parse_doi(
        "doi:10.1038/nature12373",
        &recorded(
            "https://api.crossref.org/v1/works/x/transform",
            "application/json",
            csl,
        ),
        86400,
    )
    .unwrap();
    save(pool, &doi).await;
    let landing = "https://www.nature.com/articles/nature12373";
    let fetched = recorded(
        landing,
        "text/html",
        include_bytes!("fixtures/registry/nature12373-head.html"),
    );
    let page = pinakes_core::resolver::html::off_runtime(|| {
        pinakes_core::resolver::web::extract(landing, &fetched, 86400).unwrap()
    })
    .doc;
    save(pool, &page).await;
    // The book from its recorded Open Library record.
    let book = pinakes_core::resolver::cards::parse_isbn(
        "isbn:9780140328721",
        "9780140328721",
        &recorded(
            "https://openlibrary.org/api/books?bibkeys=ISBN:9780140328721&format=json&jscmd=data",
            "application/json",
            include_bytes!("fixtures/registry/openlibrary-9780140328721.json"),
        ),
        86400,
    )
    .unwrap();
    save(pool, &book).await;
    // Every other URI the catalogue resolves or looks up, as a card of its `card_type` (or in the
    // state its example shows), tagged so search finds it; an alias form resolves through its
    // root, a map link or `geo:` URI through the offline geo resolver, and the pending example
    // stays unseen.
    let mut kinds = BTreeMap::new();
    let mut states = BTreeMap::new();
    for recipe in pinakes_core::recipes::recipes() {
        if let Some(kind) = &recipe.card_type {
            kinds.insert(
                recipe.body.as_ref().unwrap()["uri"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                kind.clone(),
            );
        }
        if matches!(
            recipe.expect.as_str(),
            "missing" | "unreachable" | "pending"
        ) {
            states.insert(
                recipe.body.as_ref().unwrap()["uri"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                recipe.expect.clone(),
            );
        }
    }
    for (uri, kind) in &kinds {
        let target = pinakes_core::alias::root(uri).map_or_else(|| uri.clone(), |a| a.root);
        let id = identity(&target).unwrap().0;
        let resolved: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM resource WHERE id=$1 AND status<>'pending')",
        )
        .bind(&id)
        .fetch_one(pool)
        .await
        .unwrap();
        if !resolved {
            let route = pinakes_core::resolver::classify(&target).unwrap_or("web");
            if route == "geo" {
                store::ensure_resource(pool, &target).await.unwrap();
                let job = queue::claim(pool, "geo", &QueueConfig::default())
                    .await
                    .unwrap()
                    .unwrap();
                queue::run_job(
                    pool,
                    &pinakes_core::resolver::geo::GeoResolver,
                    &QueueConfig::default(),
                    &job,
                )
                .await
                .unwrap();
            } else {
                let name = target.rsplit(['/', ':']).find(|s| !s.is_empty()).unwrap();
                let mut doc = card(&target, route, kind, name);
                // Search lists events by their dates.
                if kind.ends_with("Event") {
                    doc.start = Some(now + chrono::Duration::days(10));
                }
                save(pool, &doc).await;
            }
        }
        tag(pool, &id, "catalogue").await;
    }
    for (uri, state) in &states {
        if state == "pending" {
            continue;
        }
        let mut doc = ResourceMeta::new(uri, "web", 86400);
        doc.status = if state == "missing" {
            pinakes_core::model::Status::Missing
        } else {
            pinakes_core::model::Status::Unreachable
        };
        save(pool, &doc).await;
        // Missing is confirmed by a second observation.
        doc.fetched_at += 1000;
        save(pool, &doc).await;
    }
    // What the also-on sources would record: one recording on Spotify and Deezer (its MBID and
    // ISRC), the album's barcode, and one Wikidata item for two Wikipedia editions. The shared
    // keys join each pair, as the hubs' answers do.
    {
        use pinakes_core::also_on::{self, AlsoOnConfig, Fact, Found, Key};
        let key = |k: &str, via: &str| Key {
            key: k.into(),
            via: via.into(),
        };
        for (uri, found) in [
            (
                "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
                Found {
                    keys: vec![key(
                        "mb:recording:9f9cf187-d6f9-437f-9d98-d59cdbd52757",
                        "musicbrainz",
                    )],
                    facts: vec![Fact::new("isrc", "GBAYE9701376")],
                    ..Default::default()
                },
            ),
            (
                "https://www.deezer.com/track/138539973",
                Found {
                    keys: vec![key(
                        "mb:recording:9f9cf187-d6f9-437f-9d98-d59cdbd52757",
                        "musicbrainz",
                    )],
                    facts: vec![Fact::new("isrc", "GBAYE9701376")],
                    ..Default::default()
                },
            ),
            (
                "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE",
                Found {
                    facts: vec![Fact::new("upc", "634904078164")],
                    ..Default::default()
                },
            ),
            (
                "https://en.wikipedia.org/wiki/Immanuel_Kant",
                Found {
                    keys: vec![key("wikidata:Q9312", "wikidata")],
                    ..Default::default()
                },
            ),
            (
                "https://de.wikipedia.org/wiki/Immanuel_Kant",
                Found {
                    keys: vec![key("wikidata:Q9312", "wikidata")],
                    ..Default::default()
                },
            ),
        ] {
            let id = identity(uri).unwrap().0;
            also_on::record(
                pool,
                &id,
                "musicbrainz",
                found,
                std::time::Duration::from_secs(30 * 86400),
                &AlsoOnConfig::default(),
            )
            .await
            .unwrap();
            pinakes_core::groups::reconcile(pool, &id, &Default::default()).await;
        }
        // A place's keys as `osm_wikidata` records them.
        let place = "https://www.openstreetmap.org/node/240109189";
        let id = identity(place).unwrap().0;
        also_on::record(
            pool,
            &id,
            "osm_wikidata",
            Found {
                keys: vec![
                    key("osm:node/240109189", "wikidata"),
                    key("wikidata:Q64", "wikidata"),
                ],
                ..Default::default()
            },
            std::time::Duration::from_secs(30 * 86400),
            &AlsoOnConfig::default(),
        )
        .await
        .unwrap();
        pinakes_core::groups::reconcile(pool, &id, &Default::default()).await;
        pinakes_core::groups::drain(pool, &Default::default(), 100)
            .await
            .unwrap();
    }
    // What the books, podcasts and films sources would record: an edition's work, ISBN and
    // pages, a show's Apple ID and GUID, a film's IMDb ID and Wikidata item (the show and the
    // film are synthetic cards).
    {
        use pinakes_core::also_on::{self, AlsoOnConfig, Fact, Found, Key, Link};
        let apple = "https://podcasts.apple.com/us/podcast/the-daily/id1200361736";
        let imdb = "https://www.imdb.com/title/tt0111161/";
        let mut mouse = card(
            ALTERNATE,
            "jsonld",
            "Product",
            "SABRE v3 PRO PureVector Ultralight Ergo",
        );
        mouse
            .attributes
            .insert("gtin".into(), "0840440422266".into());
        save(pool, &mouse).await;
        save(pool, &card(apple, "web", "PodcastSeries", "The Daily")).await;
        save(
            pool,
            &card(imdb, "web", "Movie", "The Shawshank Redemption (1994)"),
        )
        .await;
        let key = |k: &str, via: &str| Key {
            key: k.into(),
            via: via.into(),
        };
        for (uri, source, found) in [
            (
                "isbn:9780140328721",
                "books",
                Found {
                    reread: false,
                    retry_after: None,
                    keys: vec![
                        key("openlibrary:edition:OL7353617M", "openlibrary"),
                        key("openlibrary:work:OL45804W", "openlibrary"),
                    ],
                    links: vec![
                        Link::new(
                            "https://openlibrary.org/books/OL7353617M",
                            "Open Library",
                            "openlibrary",
                        ),
                        Link::new("https://www.amazon.com/dp/0140328726", "Amazon", "isbn"),
                    ],
                    facts: vec![Fact::new("isbn", "9780140328721")],
                },
            ),
            (
                apple,
                "podcasts",
                Found {
                    reread: false,
                    retry_after: None,
                    keys: vec![
                        key("podcast:apple:1200361736", "itunes"),
                        key(
                            "podcast:guid:304b84f0-07b0-5265-b6b7-da5cf5aeb56e",
                            "podcastindex",
                        ),
                    ],
                    links: vec![
                        Link::new("https://pca.st/itunes/1200361736", "Pocket Casts", "itunes"),
                        Link::new(
                            "https://open.spotify.com/show/3IM0lmZxpFAY7CwMuv9H4g",
                            "Spotify",
                            "wikidata",
                        ),
                    ],
                    facts: vec![Fact::new(
                        "podcast_guid",
                        "304b84f0-07b0-5265-b6b7-da5cf5aeb56e",
                    )],
                },
            ),
            (
                ALTERNATE,
                "product_keys",
                Found {
                    reread: false,
                    retry_after: None,
                    keys: vec![key("gtin:0840440422266", "page")],
                    links: vec![],
                    facts: vec![],
                },
            ),
            (
                imdb,
                "films",
                Found {
                    reread: false,
                    retry_after: None,
                    keys: vec![key("imdb:tt0111161", "imdb")],
                    facts: vec![
                        Fact::new("wikidata", "Q172241"),
                        Fact::new("title_type", "Movie"),
                    ],
                    links: vec![
                        Link::new("https://www.themoviedb.org/movie/278", "TMDB", "wikidata"),
                        Link::new(
                            "https://letterboxd.com/film/the-shawshank-redemption/",
                            "Letterboxd",
                            "wikidata",
                        ),
                    ],
                },
            ),
        ] {
            let id = identity(uri).unwrap().0;
            also_on::record(
                pool,
                &id,
                source,
                found,
                std::time::Duration::from_secs(30 * 86400),
                &AlsoOnConfig::default(),
            )
            .await
            .unwrap();
            pinakes_core::groups::reconcile(pool, &id, &Default::default()).await;
        }
        pinakes_core::groups::drain(pool, &Default::default(), 100)
            .await
            .unwrap();
    }
    // Seed labels, by the seed identity.
    for (uri, labels) in [
        (
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            &["music", "video"][..],
        ),
        ("https://luma.com/376whqyw", &["conference", "bitcoin"]),
        ("isbn:9781119473862", &["books", "bitcoin"]),
        (
            "https://en.wikipedia.org/wiki/Bitcoin",
            &["bitcoin", "wikipedia"],
        ),
        (
            "https://en.wikipedia.org/wiki/Lugano",
            &["lugano", "wikipedia"],
        ),
        ("geo:46.0037,8.9511", &["lugano", "pubky"]),
        (berlin, &["berlin", "travel"]),
        (feed, &["calendar", "switzerland"]),
        (landing, &["science", "physics"]),
        ("doi:10.1038/nature12373", &["science", "physics"]),
        ("isbn:9780140328721", &["books", "children"]),
        (
            "https://www.meetup.com/bitcoin-meetup-switzerland/events/316894744/",
            &["bitcoin", "meetup"],
        ),
        (
            "pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/00351MT54YS9G",
            &["pubky", "mapky"],
        ),
        ("https://example.org/", &["web"]),
        (
            "https://podcasts.apple.com/us/podcast/the-daily/id1200361736",
            &["podcast", "news"],
        ),
        ("https://www.imdb.com/title/tt0111161/", &["film"]),
        (ALTERNATE, &["shop"]),
        (
            "https://www.bbcgoodfood.com/recipes/classic-lasagne-0",
            &["food"],
        ),
        (
            "https://bitcoinerjobs.com/job/1682142-staff-software-engineer-river",
            &["jobs"],
        ),
        (
            "https://bitcoinerjobs.com/job/2563734-investment-associateanalyst-battery",
            &["jobs"],
        ),
    ] {
        let id = identity(uri).unwrap().0;
        for label in labels {
            tag(pool, &id, label).await;
        }
    }
    for uri in [
        "https://luma.com/376whqyw",
        "https://www.meetup.com/bitcoin-meetup-switzerland/events/316894744/",
    ] {
        tag_as(pool, &identity(uri).unwrap().0, "bitcoin", SECOND).await;
    }
    while pinakes_core::same_event::drain(pool, &Config::default(), 200)
        .await
        .unwrap()
        > 0
    {}
}

/// One tag by the seed identity, with the resource's score.
async fn tag(pool: &PgPool, id: &str, label: &str) {
    tag_as(pool, id, label, SEED).await
}
async fn tag_as(pool: &PgPool, id: &str, label: &str, author: &str) {
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag',$3,$4,now()) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(author)
        .bind(label)
        .bind(match author {
            SEED => format!("/pub/pubky.app/tags/{id}-{label}"),
            _ => format!("/pub/pubky.app/tags/{id}-{label}-{author}"),
        })
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::recompute_scores(&mut tx, &[id.to_owned()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn following() -> Json<Vec<String>> {
    Json(vec![SEED.to_owned()])
}

async fn mock_nexus() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v0/user/{id}/following", get(following))
        .route("/v0/user/{id}/friends", get(following))
        .with_state(());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), task)
}

async fn send(app: &Router, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
    let mut request = Request::builder().method(method).uri(target);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let mut request = request
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string())),
    )
}

/// One request per example name, rebuilt from the served document exactly as Swagger UI sends
/// it: body examples as the JSON body, parameter examples by name with empty values left out.
fn examples(doc: &Value) -> BTreeMap<String, (String, String, Option<Value>)> {
    let mut requests = BTreeMap::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            let mut names = BTreeSet::new();
            let body = &operation["requestBody"]["content"]["application/json"]["examples"];
            names.extend(
                body.as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.clone()),
            );
            let parameters = operation["parameters"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for p in &parameters {
                names.extend(
                    p["examples"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(k, _)| k.clone())
                        .filter(|k| k != pinakes_core::recipes::NONE),
                );
            }
            for name in names {
                let mut target = path.clone();
                let mut query = vec![];
                for p in &parameters {
                    let Some(example) = p["examples"].get(&name) else {
                        continue;
                    };
                    let value = match &example["value"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if value.is_empty() {
                        continue;
                    }
                    let key = p["name"].as_str().unwrap();
                    if p["in"] == "path" {
                        target = target.replace(&format!("{{{key}}}"), &value);
                    } else {
                        // As Swagger UI sends it: every value encoded.
                        let value: String =
                            url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
                        query.push(format!("{key}={value}"));
                    }
                }
                if !query.is_empty() {
                    target = format!("{target}?{}", query.join("&"));
                }
                assert!(
                    !target.contains('{'),
                    "{name}: unfilled path parameter in {target}"
                );
                let value = body.get(&name).map(|e| e["value"].clone());
                let previous =
                    requests.insert(name.clone(), (method.to_ascii_uppercase(), target, value));
                assert!(previous.is_none(), "example name {name} is used twice");
            }
        }
    }
    requests
}

/// The schema behind a `$ref` into the document's components.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn every_named_example_returns_data(pool: PgPool) {
    fixture(&pool).await;
    let (nexus_url, nexus) = mock_nexus().await;
    let app = api::router(
        AppState::new(pool, {
            let mut config = Config {
                nexus_url,
                ..Default::default()
            };
            // The catalogue sends many previews from one address.
            config.preview.requests_per_minute = 10_000;
            config.preview.uri_requests_per_minute = 10_000;
            config.api.requests_per_minute = 100_000;
            config
        })
        .await
        .unwrap(),
    );
    let (status, doc) = send(&app, "GET", "/v1/openapi.json", None).await;
    assert_eq!(status, 200);
    assert!(!doc.to_string().contains("\"parameters\":null"));
    let requests = examples(&doc);
    let recipes = pinakes_core::recipes::recipes();
    assert_eq!(
        requests.keys().cloned().collect::<BTreeSet<_>>(),
        recipes
            .iter()
            .map(|r| r.name.clone())
            .collect::<BTreeSet<_>>(),
        "every recipe is a named example and every named example a recipe"
    );
    for recipe in &recipes {
        let (method, target, body) = &requests[&recipe.name];
        assert_eq!(method, &recipe.method, "{}", recipe.name);
        let (status, response) = send(&app, method, target, body.as_ref()).await;
        let wanted = if recipe.expect == "pending" { 202 } else { 200 };
        assert_eq!(
            status, wanted,
            "{} {method} {target}: {response}",
            recipe.name
        );
        // The response matches the schema the document promises for it, exactly.
        let schema = &doc["paths"][&recipe.path][recipe.method.to_ascii_lowercase()]["responses"]
            [wanted.to_string()]["content"]["application/json"]["schema"];
        assert!(schema.is_object(), "{}: no {wanted} schema", recipe.name);
        let mut errors = vec![];
        conforms(&doc, schema, &response, "$", &mut errors);
        assert!(
            errors.is_empty(),
            "{}: {errors:#?}\n{response}",
            recipe.name
        );
        let count = |v: &Value| v.as_array().map_or(0, Vec::len);
        let found = match recipe.expect.as_str() {
            "card" => usize::from(response["status"] == "ok" && response["type"].is_string()),
            "items" => count(&response).max(count(&response["items"])),
            "map" => count(&response["clusters"]) + count(&response["points"]),
            "group" => usize::from(count(&response["members"]) >= 2),
            "same_event" => count(&response["maybe_same"]),
            "lines" => count(&response["lines"]),
            state @ ("missing" | "unreachable" | "pending") => {
                usize::from(response["status"] == state)
            }
            other => panic!("{}: unknown expectation {other}", recipe.name),
        };
        assert!(
            found > 0,
            "{} {method} {target} returned no data: {response}",
            recipe.name
        );
        // A time window from now never lists a past date.
        if recipe.query.get("from").is_some_and(|f| f == "now") {
            let floor = chrono::Utc::now() - chrono::Duration::minutes(2);
            for item in response["items"].as_array().unwrap() {
                let start = item["start"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .unwrap_or_else(|| panic!("an event without a start: {item}"));
                assert!(start >= floor, "upcoming events lists a past date: {item}");
            }
        }
    }
    nexus.abort();
}

/// A recipe whose resolver is switched off is refused by `resolver::admit`, as `/v1/resolve`
/// refuses it: the endpoint answers 400 with the same reason, and the live recipe check
/// (`e2e_stack --live-recipes`), which asks `admit`, reports the refusal instead of a card.
/// Calendars switched off are read as web pages, by both.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_recipe_whose_resolver_is_off_is_refused_as_the_endpoint_refuses_it(pool: PgPool) {
    use pinakes_core::resolver::{admit, ROUTES};
    let uris: Vec<String> = pinakes_core::recipes::recipes()
        .into_iter()
        .filter(|r| r.path == "/v1/resolve")
        .filter_map(|r| r.body.as_ref()?["uri"].as_str().map(str::to_owned))
        .collect();
    let on = Config::default();
    for route in ROUTES {
        // An event platform's web link falls back to the web route; a Nostr link has none.
        let uri = uris
            .iter()
            .find(|u| {
                admit(u, &on) == Ok(route) && (route != "event_platform" || u.starts_with("nostr:"))
            })
            .unwrap_or_else(|| panic!("no recipe takes the {route} route"));
        let mut off = Config::default();
        match route {
            "web" => off.web.enabled = false,
            "pubky" => off.pubky.enabled = false,
            "geo" => off.geo.enabled = false,
            "osm" => off.osm.enabled = false,
            "event" => off.event.calendars = false,
            "event_platform" => off.event.adapters.enabled = false,
            _ => unreachable!("{route}"),
        }
        if route == "event" {
            assert_eq!(admit(uri, &off), Ok("web"), "{uri}");
            continue;
        }
        assert_eq!(admit(uri, &off), Err("resolver is disabled"), "{uri}");
        let app = api::router(AppState::new(pool.clone(), off).await.unwrap());
        let (code, answer) = send(&app, "POST", "/v1/resolve", Some(&json!({ "uri": uri }))).await;
        assert_eq!(
            (code, answer),
            (400, json!({"error":"resolver is disabled"})),
            "{uri}"
        );
    }
}

/// Docs/api.md lists every recipe's request line, so the recipes there cannot drift.
#[test]
fn the_api_docs_list_every_recipe() {
    let docs = include_str!("../../../docs/api.md");
    let section = docs
        .split("## Examples")
        .nth(1)
        .expect("an 'Examples' section");
    for recipe in pinakes_core::recipes::recipes() {
        // A source's examples are listed on its own page.
        if let Some(source) = recipe.source {
            let page = std::fs::read_to_string(format!(
                "{}/../../docs/sources/{source}.md",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            assert!(
                page.contains(&recipe.summary),
                "docs/sources/{source}.md lacks {}",
                recipe.summary
            );
            continue;
        }
        let line = recipe.request_line();
        assert!(section.contains(&line), "docs/api.md lacks {line}");
    }
}

#[test]
fn the_schema_check_refuses_dangling_refs_extra_properties_and_ambiguous_one_of() {
    let doc = json!({"components":{"schemas":{
        "Closed":{"type":"object","properties":{"a":{"type":"integer"}},"additionalProperties":false},
        "Either":{"oneOf":[{"type":"integer"},{"type":"number"}]},
    }}});
    let errors = |schema: Value, value: Value| {
        let mut e = vec![];
        conforms(&doc, &schema, &value, "$", &mut e);
        e
    };
    assert!(errors(
        json!({"$ref":"#/components/schemas/Closed"}),
        json!({"a":1})
    )
    .is_empty());
    // A closed object that declares no properties allows none.
    assert_eq!(
        errors(
            json!({"type":"object","additionalProperties":false}),
            json!({"x":1})
        ),
        ["$: undeclared property x"]
    );
    assert_eq!(
        errors(
            json!({"$ref":"#/components/schemas/Closed"}),
            json!({"a":1,"b":2})
        ),
        ["$: undeclared property b"]
    );
    assert_eq!(
        errors(json!({"$ref":"#/components/schemas/Missing"}), json!(1)),
        ["$: dangling $ref #/components/schemas/Missing"]
    );
    // An integer is also a number: two options match, so `oneOf` fails.
    assert_eq!(
        errors(json!({"$ref":"#/components/schemas/Either"}), json!(1)).len(),
        1
    );
    assert!(errors(json!({"$ref":"#/components/schemas/Either"}), json!(1.5)).is_empty());
}
