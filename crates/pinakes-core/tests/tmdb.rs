//! TMDB's posters and overviews on film and series cards (`also_on::tmdb`), by exact IDs alone,
//! against handmade answers shaped from TMDB's API reference (`tests/fixtures/media/tmdb`, see
//! its provenance.json): what a card takes, what it keeps through a failure, what goes when
//! TMDB's rows expire or TMDB is switched off, and that the credential appears nowhere.
use async_trait::async_trait;
use pinakes_core::{
    also_on::{self, tmdb::Tmdb, Context, Source},
    api::{self, AppState},
    config::{Config, QueueConfig},
    model::{Credit, CreditField, CreditProvider, ResourceMeta},
    queue,
    resolver::{
        egress::{Egress, FetchOptions, Fetched, Transport},
        Resolver,
    },
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
mod common;
use common::{get, PublicDns};

/// Test credentials of both forms; never real ones.
const TOKEN: &str =
    "eyJhbGciOiJIUzI1NiJ9.eyJhdWQiOiJwaW5ha2VzLXRlc3QifQ.cGluYWtlcy10ZXN0LXNpZ25hdHVyZQ";
const KEY: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
const POSTERS: &str = "https://image.tmdb.org/t/p/w500";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/media/tmdb/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// How TMDB answers.
#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Recorded,
    /// 503.
    Busy,
    /// 401: the credential refused.
    Refused,
}
/// TMDB's API from the fixtures (`find-{id}.json`; `tt0000001` names two movies, any other
/// unknown ID none); records each request's URL and whether it carried the credential.
struct Api {
    answer: Mutex<Answer>,
    seen: Mutex<Vec<String>>,
    credentials: Mutex<Vec<bool>>,
}
impl Api {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Answer::Recorded),
            seen: Mutex::default(),
            credentials: Mutex::default(),
        })
    }
    fn seen(&self) -> Vec<String> {
        let mut seen = self.seen.lock().unwrap().clone();
        seen.sort();
        seen
    }
}
#[async_trait]
impl Transport for Api {
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
        assert_eq!(url.host_str(), Some("api.themoviedb.org"), "{url}");
        self.seen.lock().unwrap().push(url.to_string());
        // The credential reaches the wire: a token as its header, a key in the wire's query.
        let wire = pinakes_core::resolver::egress::wire_url(url, options).to_string();
        let sent = options
            .headers
            .iter()
            .any(|(k, v)| *k == "authorization" && *v == format!("Bearer {TOKEN}"))
            || wire.ends_with(&format!("&api_key={KEY}"));
        self.credentials.lock().unwrap().push(sent);
        let (status, body) = match *self.answer.lock().unwrap() {
            Answer::Busy => (503, b"{}".to_vec()),
            Answer::Refused => (
                401,
                br#"{"success":false,"status_code":7,"status_message":"Invalid API key: You must be granted a valid key."}"#.to_vec(),
            ),
            Answer::Recorded => {
                let id = url.path().strip_prefix("/3/find/").unwrap();
                let name = format!("find-{id}.json");
                let body = if id == "tt0000001" {
                    fixture("find-two-movies.json")
                } else if std::path::Path::new(&format!(
                    "{}/tests/fixtures/media/tmdb/{name}",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .exists()
                {
                    fixture(&name)
                } else {
                    fixture("find-none.json")
                };
                (200, body)
            }
        };
        Ok(common::fetched(
            url.clone(),
            status,
            "application/json;charset=utf-8".into(),
            body,
        ))
    }
}

/// A configuration with this TMDB credential (`[tmdb] api_key`, as `TMDB_API_KEY` sets it), or
/// none.
fn config(credential: Option<&str>) -> Config {
    let mut config = Config::default();
    if let Some(credential) = credential {
        config.sections.set("tmdb", json!({"api_key": credential}));
    }
    config
}
fn cx(config: Config, api: &Arc<Api>) -> Context {
    Context {
        egress: Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            api.clone(),
        ),
        config: Arc::new(config),
    }
}
/// The TMDB source's run on every due resource.
async fn look_up(pool: &PgPool, cx: &Context) {
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(pool)
        .await
        .unwrap();
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(Tmdb)];
    also_on::drain(pool, cx, &sources, 100).await.unwrap();
}

/// Each page as its own reading makes it.
struct Pages(Mutex<BTreeMap<String, ResourceMeta>>);
#[async_trait]
impl Resolver for Pages {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> anyhow::Result<ResourceMeta> {
        let mut doc = self.0.lock().unwrap()[uri].clone();
        doc.fetched_at = chrono::Utc::now().timestamp_millis();
        Ok(doc)
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(86_400)
    }
}
/// A page's card: its type, its own image and description, and the keys `films` found for it.
struct Page {
    uri: &'static str,
    kind: &'static str,
    image: Option<&'static str>,
    description: Option<&'static str>,
    keys: &'static [&'static str],
}
impl Page {
    fn doc(&self) -> ResourceMeta {
        let mut doc = ResourceMeta::new(self.uri, "web", 86_400);
        doc.set_type(self.kind);
        doc.name = Some(self.uri.rsplit('/').next().unwrap().into());
        doc.image = self.image.map(Into::into);
        doc.description = self.description.map(Into::into);
        doc
    }
}
/// Saves the pages' cards and their keys; the reader that reads them again.
async fn pages(pool: &PgPool, pages: &[Page]) -> (Vec<String>, Pages) {
    let mut ids = vec![];
    let mut docs = BTreeMap::new();
    for page in pages {
        let id = store::ensure_resource(pool, page.uri).await.unwrap();
        store::save_document(pool, &id, 0, &page.doc())
            .await
            .unwrap();
        sqlx::query("UPDATE resource SET resolver='web' WHERE id=$1")
            .bind(&id)
            .execute(pool)
            .await
            .unwrap();
        for key in page.keys {
            sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'films','wikidata',now()+interval '60 days')")
                .bind(&id)
                .bind(key)
                .execute(pool)
                .await
                .unwrap();
        }
        docs.insert(page.uri.to_owned(), page.doc());
        ids.push(id);
    }
    (ids, Pages(Mutex::new(docs)))
}
/// Every card read again now, as at its refresh.
async fn read_again(pool: &PgPool, pages: &Pages) {
    sqlx::query("UPDATE resource SET next_attempt_at=now(),priority_at=now(),final=false WHERE resolver='web'")
        .execute(pool)
        .await
        .unwrap();
    let cfg = QueueConfig::default();
    loop {
        let jobs = queue::claim_batch(pool, "web", &cfg, 16).await.unwrap();
        if jobs.is_empty() {
            break;
        }
        queue::run_batch(pool, pages, &cfg, &jobs).await.unwrap();
    }
}
/// The stored card: its image, description and credits' providers by field.
async fn stored(
    pool: &PgPool,
    id: &str,
) -> (
    Option<String>,
    Option<String>,
    Vec<(CreditField, CreditProvider)>,
) {
    let doc: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    let doc: ResourceMeta = serde_json::from_value(doc).unwrap();
    (
        doc.image,
        doc.description,
        doc.credits.iter().map(|c| (c.field, c.provider)).collect(),
    )
}

const MOVIE: Page = Page {
    uri: "https://www.netflix.com/title/70305903",
    kind: "Movie",
    image: None,
    description: Some("Own text."),
    keys: &["imdb:tt0816692"],
};
const SERIES: Page = Page {
    uri: "https://www.wikidata.org/wiki/Q1079",
    kind: "TVSeries",
    image: None,
    description: None,
    keys: &[],
};

/// A movie card takes the poster its IMDb ID names, a series card the poster and overview its
/// own Wikidata item names, each credited to TMDB; a card's own image and text stay; a title of
/// another type, an ID that names two titles, or two IDs that name different ones give nothing;
/// a card with its own image and text, or of another kind, asks nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn film_and_series_cards_take_tmdb_posters_by_exact_id(pool: PgPool) {
    let (ids, reader) = pages(
        &pool,
        &[
            MOVIE,
            SERIES,
            // Its own image stays; it takes the overview.
            Page {
                uri: "https://letterboxd.com/film/interstellar/",
                kind: "Movie",
                image: Some("https://a.ltrbxd.com/poster.jpg"),
                description: None,
                keys: &["imdb:tt0816692"],
            },
            // A series whose IMDb ID names a movie.
            Page {
                uri: "https://trakt.tv/shows/not-a-series",
                kind: "TVSeries",
                image: None,
                description: None,
                keys: &["imdb:tt0816692"],
            },
            // An ID that names two movies.
            Page {
                uri: "https://www.imdb.com/title/tt0000001/",
                kind: "Movie",
                image: None,
                description: None,
                keys: &["imdb:tt0000001"],
            },
            // An IMDb ID and an item that name different titles.
            Page {
                uri: "https://www.wikidata.org/wiki/Q13417189",
                kind: "TVSeries",
                image: None,
                description: None,
                keys: &["imdb:tt0903747"],
            },
            // Its own image and text: nothing to fill, nothing asked.
            Page {
                uri: "https://letterboxd.com/film/the-matrix/",
                kind: "Movie",
                image: Some("https://a.ltrbxd.com/matrix.jpg"),
                description: Some("Own."),
                keys: &["imdb:tt0133093"],
            },
            // TMDB's own page: never asked, whatever it lacks.
            Page {
                uri: "https://www.themoviedb.org/movie/603-the-matrix",
                kind: "Movie",
                image: None,
                description: None,
                keys: &["imdb:tt0133093"],
            },
            // Not a film.
            Page {
                uri: "https://example.org/soundtrack",
                kind: "MusicRecording",
                image: None,
                description: None,
                keys: &["imdb:tt0133094"],
            },
        ],
    )
    .await;
    let api = Api::new();
    look_up(&pool, &cx(config(Some(TOKEN)), &api)).await;
    let find = |id: &str, source: &str| {
        format!("https://api.themoviedb.org/3/find/{id}?external_source={source}")
    };
    let mut asked = vec![
        find("tt0816692", "imdb_id"),
        find("Q1079", "wikidata_id"),
        find("tt0000001", "imdb_id"),
        find("tt0903747", "imdb_id"),
        find("Q13417189", "wikidata_id"),
    ];
    asked.sort();
    // Each answer once (the egress caches it), never for the Matrix or the soundtrack.
    assert_eq!(api.seen(), asked);
    assert!(api.credentials.lock().unwrap().iter().all(|sent| *sent));
    read_again(&pool, &reader).await;
    let poster = |path: &str| Some(format!("{POSTERS}{path}"));
    let tmdb = |fields: &[CreditField]| -> Vec<(CreditField, CreditProvider)> {
        fields.iter().map(|f| (*f, CreditProvider::Tmdb)).collect()
    };
    assert_eq!(
        stored(&pool, &ids[0]).await,
        (
            poster("/gEU2QniE6E77NI6lCU6MxlNBvIx.jpg"),
            Some("Own text.".into()),
            tmdb(&[CreditField::Image])
        )
    );
    let (image, description, credits) = stored(&pool, &ids[1]).await;
    assert_eq!(image, poster("/ztkUQFLlC19CCMYHW9o1zWhJRNq.jpg"));
    assert!(description
        .unwrap()
        .starts_with("Walter White, a New Mexico"));
    assert_eq!(
        credits,
        tmdb(&[CreditField::Image, CreditField::Description])
    );
    let (image, description, credits) = stored(&pool, &ids[2]).await;
    assert_eq!(image.as_deref(), Some("https://a.ltrbxd.com/poster.jpg"));
    assert!(description
        .unwrap()
        .starts_with("The adventures of a group"));
    assert_eq!(credits, tmdb(&[CreditField::Description]));
    for id in &ids[3..] {
        let (image, _, credits) = stored(&pool, id).await;
        assert!(credits.is_empty(), "{id}");
        assert!(image.is_none_or(|i| !i.starts_with(POSTERS)), "{id}");
    }
    // No TMDB row for the cards that took nothing.
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT resource_id FROM resource_fact WHERE source='tmdb' ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected = ids[..3].to_vec();
    expected.sort();
    assert_eq!(rows, expected);
    // The card as the API answers it: the credit names the title's page on TMDB.
    let app = api::router(AppState::new(pool.clone(), config(Some(TOKEN))).unwrap());
    let card = get(&app, &format!("/v1/resources/{}", ids[1])).await;
    assert_eq!(
        card["credits"],
        json!([
            {"field": "image", "provider": "tmdb", "source_url": "https://www.themoviedb.org/tv/1396", "author": null, "license": null, "license_url": null, "lang": null, "excerpted": false, "modified": false, "subject": null},
            {"field": "description", "provider": "tmdb", "source_url": "https://www.themoviedb.org/tv/1396", "author": null, "license": null, "license_url": null, "lang": "en", "excerpted": false, "modified": false, "subject": null},
        ])
    );
    assert_eq!(
        card["image"].as_str(),
        Some(format!("{POSTERS}/ztkUQFLlC19CCMYHW9o1zWhJRNq.jpg").as_str())
    );
    // A card read again unchanged (here, its whole reading the same) keeps them.
    read_again(&pool, &reader).await;
    assert_eq!(
        stored(&pool, &ids[1]).await.2,
        tmdb(&[CreditField::Image, CreditField::Description])
    );
}

/// Without `TMDB_API_KEY` TMDB is never asked, cards stay as their pages make them, and
/// `/v1/info` names no attribution; with it, `/v1/info` gives TMDB's notice verbatim and its logo.
#[sqlx::test(migrations = "../../migrations")]
async fn without_a_key_tmdb_is_never_asked(pool: PgPool) {
    let (ids, reader) = pages(&pool, &[MOVIE, SERIES]).await;
    let api = Api::new();
    let off = config(None);
    also_on::configure(&pool, &off).await.unwrap();
    // The worker as the daemon runs it, every registered source.
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let only: Vec<Arc<dyn Source>> = also_on::registry()
        .into_iter()
        .filter(|s| s.id() == "tmdb")
        .collect();
    assert_eq!(only.len(), 1);
    also_on::drain(&pool, &cx(off.clone(), &api), &only, 100)
        .await
        .unwrap();
    read_again(&pool, &reader).await;
    assert_eq!(api.seen(), Vec::<String>::new());
    assert_eq!(
        stored(&pool, &ids[0]).await,
        (None, Some("Own text.".into()), vec![])
    );
    assert_eq!(stored(&pool, &ids[1]).await, (None, None, vec![]));
    let app = api::router(AppState::new(pool.clone(), off).unwrap());
    let info = get(&app, "/v1/info").await;
    assert!(info.get("attributions").is_none(), "{info}");
    // An empty key, or `[tmdb] enabled = false`, is off too.
    for off in [config(Some("")), {
        let mut c = config(Some(TOKEN));
        c.sections
            .set("tmdb", json!({"api_key": TOKEN, "enabled": false}));
        c
    }] {
        assert!(!pinakes_core::also_on::runs(&off, &Tmdb));
        assert!(pinakes_core::also_on::switched_off(&off, &Tmdb));
    }
    // With the key, whether or not the also-on worker runs: its rows and the cards' values show.
    let mut idle = config(Some(KEY));
    idle.also_on.enabled = false;
    let app = api::router(AppState::new(pool.clone(), idle).unwrap());
    assert_eq!(
        get(&app, "/v1/info").await["attributions"][0]["provider"],
        "tmdb"
    );
    let app = api::router(AppState::new(pool.clone(), config(Some(KEY))).unwrap());
    let info = get(&app, "/v1/info").await;
    assert_eq!(
        info["attributions"],
        json!([{
            "provider": "tmdb",
            "notice": "This product uses the TMDB API but is not endorsed or certified by TMDB.",
            "logo": "https://www.themoviedb.org/assets/2/v4/logos/v2/blue_short-8e7b30f73a4020692ccca9c88bafe5dcb6f8a62a4c6bc55cd9ba82bb2cd95f6c.svg",
            "url": "https://www.themoviedb.org",
        }])
    );
}

/// TMDB's rows live 60 days after the last read that answered: a failure (503) keeps a poster
/// read 50 days ago; rows past that are dropped and the card loses the poster at its next
/// reading; a removed key hides them at once (`also_on::configure`) and the card loses it too.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failure_keeps_a_recent_poster_and_old_or_switched_off_ones_go(pool: PgPool) {
    let (ids, reader) = pages(&pool, &[MOVIE]).await;
    let api = Api::new();
    let on = config(Some(TOKEN));
    also_on::configure(&pool, &on).await.unwrap();
    look_up(&pool, &cx(on.clone(), &api)).await;
    read_again(&pool, &reader).await;
    let poster = Some(format!("{POSTERS}/gEU2QniE6E77NI6lCU6MxlNBvIx.jpg"));
    assert_eq!(stored(&pool, &ids[0]).await.0, poster);
    let rows: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "SELECT min(seen_at),min(expires_at) FROM resource_fact WHERE source='tmdb'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let lifetime = (rows.1 - rows.0).num_days();
    assert!((59..=60).contains(&lifetime), "{lifetime}");
    // Read 50 days ago; TMDB busy now.
    sqlx::query("UPDATE resource_fact SET seen_at=now()-interval '50 days',expires_at=now()+interval '10 days' WHERE source='tmdb'")
        .execute(&pool)
        .await
        .unwrap();
    *api.answer.lock().unwrap() = Answer::Busy;
    let busy = cx(on.clone(), &api);
    look_up(&pool, &busy).await;
    assert!(api.seen().len() >= 2, "asked again");
    read_again(&pool, &reader).await;
    assert_eq!(stored(&pool, &ids[0]).await.0, poster, "kept");
    // Read 181 days ago: long expired, pruned.
    sqlx::query("UPDATE resource_fact SET seen_at=now()-interval '181 days',expires_at=now()-interval '121 days' WHERE source='tmdb'")
        .execute(&pool)
        .await
        .unwrap();
    also_on::prune(&pool).await.unwrap();
    look_up(&pool, &busy).await;
    read_again(&pool, &reader).await;
    assert_eq!(
        stored(&pool, &ids[0]).await,
        (None, Some("Own text.".into()), vec![])
    );
    // Back, then the key removed.
    *api.answer.lock().unwrap() = Answer::Recorded;
    let fresh = cx(on.clone(), &Api::new());
    look_up(&pool, &fresh).await;
    read_again(&pool, &reader).await;
    assert_eq!(stored(&pool, &ids[0]).await.0, poster);
    let off = config(None);
    also_on::configure(&pool, &off).await.unwrap();
    read_again(&pool, &reader).await;
    assert_eq!(
        stored(&pool, &ids[0]).await,
        (None, Some("Own text.".into()), vec![])
    );
    // And its facts are no longer shown.
    let app = api::router(AppState::new(pool.clone(), off).unwrap());
    let card = get(&app, &format!("/v1/resources/{}", ids[0])).await;
    assert!(!card.to_string().contains("tmdb"), "{card}");
}

/// The maintenance pass brings a card no reading brings back (final here) to TMDB's live rows:
/// a TMDB poster whose rows expired goes, with its credit and a change line, and the search
/// revision moves; the card's other values and credits, and cards TMDB gave nothing, stay.
#[sqlx::test(migrations = "../../migrations")]
async fn maintenance_drops_expired_tmdb_values_and_nothing_else(pool: PgPool) {
    let (ids, reader) = pages(&pool, &[SERIES, MOVIE]).await;
    let api = Api::new();
    look_up(&pool, &cx(config(Some(TOKEN)), &api)).await;
    read_again(&pool, &reader).await;
    // The series card's description is Wikipedia's (a credit of another source), and both
    // cards are final: never read again.
    let wikipedia = Credit {
        field: CreditField::Description,
        provider: CreditProvider::Wikipedia,
        source_url: "https://en.wikipedia.org/wiki/Breaking_Bad".into(),
        author: Some("Wikipedia contributors".into()),
        license: "CC-BY-SA-4.0".into(),
        license_url: Some("https://creativecommons.org/licenses/by-sa/4.0/".into()),
        lang: Some("en".into()),
        excerpted: true,
        modified: false,
        subject: None,
    };
    let doc: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut doc: ResourceMeta = serde_json::from_value(doc).unwrap();
    doc.description = Some("An American crime drama.".into());
    doc.credits.retain(|c| c.field != CreditField::Description);
    doc.credits.push(wikipedia.clone());
    sqlx::query("UPDATE resource SET doc=$2,final=true WHERE id=$1")
        .bind(&ids[0])
        .bind(serde_json::to_value(&doc).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    // A card TMDB gave nothing, with its own image.
    let other = store::ensure_resource(&pool, "https://example.org/film-page")
        .await
        .unwrap();
    let mut own = ResourceMeta::new("https://example.org/film-page", "web", 86_400);
    own.set_type("Movie");
    own.image = Some("https://example.org/own.jpg".into());
    store::save_document(&pool, &other, 0, &own).await.unwrap();
    let untouched: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&other)
        .fetch_one(&pool)
        .await
        .unwrap();
    // Current rows: nothing to do.
    assert_eq!(also_on::refill(&pool, || false).await.unwrap(), 0);
    // TMDB edits the movie's overview, which its card (its own text) does not show: the poster
    // it shows is still TMDB's, and stays, with no change line.
    let movie_lines = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&ids[1])
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let lines = movie_lines().await;
    sqlx::query("UPDATE resource_fact SET value='Another overview.' WHERE source='tmdb' AND name='tmdb_overview' AND resource_id=$1")
        .bind(&ids[1])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET final=true WHERE id=$1")
        .bind(&ids[1])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(also_on::refill(&pool, || false).await.unwrap(), 0);
    assert_eq!(movie_lines().await, lines);
    assert_eq!(
        stored(&pool, &ids[1]).await.2,
        [(CreditField::Image, CreditProvider::Tmdb)]
    );
    // A card of a type TMDB never fills is not read by the pass (it reads Movie and TVSeries
    // cards alone, by their type's index), even with such a credit in it.
    let song = store::ensure_resource(&pool, "https://example.org/song")
        .await
        .unwrap();
    let mut odd = ResourceMeta::new("https://example.org/song", "web", 86_400);
    odd.set_type("MusicRecording");
    odd.image = Some(format!("{POSTERS}/p.jpg"));
    odd.credits.push(Credit {
        field: CreditField::Image,
        provider: CreditProvider::Tmdb,
        source_url: "https://www.themoviedb.org/movie/1".into(),
        author: None,
        license: String::new(),
        license_url: None,
        lang: None,
        excerpted: false,
        modified: false,
        subject: None,
    });
    store::save_document(&pool, &song, 0, &odd).await.unwrap();
    let song_doc = || async {
        sqlx::query_scalar::<_, Value>("SELECT doc FROM resource WHERE id=$1")
            .bind(&song)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let song_before = song_doc().await;
    assert_eq!(also_on::refill(&pool, || false).await.unwrap(), 0);
    assert_eq!(song_doc().await, song_before);
    // The series' TMDB rows expire; the movie's stay.
    sqlx::query("UPDATE resource_fact SET expires_at=now()-interval '1 second' WHERE source='tmdb' AND resource_id=$1")
        .bind(&ids[0])
        .execute(&pool)
        .await
        .unwrap();
    also_on::prune(&pool).await.unwrap();
    let revision = || async {
        sqlx::query_scalar::<_, i64>("SELECT coalesce(sum(writes),0)::bigint FROM search_write")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let changes = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change WHERE resource_id=$1")
            .bind(&ids[0])
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let (before, lines) = (revision().await, changes().await);
    assert_eq!(also_on::refill(&pool, || false).await.unwrap(), 1);
    assert!(revision().await > before, "the search revision moves");
    assert_eq!(changes().await, lines + 1, "one change line");
    let (image, description, credits) = stored(&pool, &ids[0]).await;
    assert_eq!(image, None);
    assert_eq!(description.as_deref(), Some("An American crime drama."));
    assert_eq!(
        credits,
        [(CreditField::Description, CreditProvider::Wikipedia)]
    );
    let still_final: bool = sqlx::query_scalar("SELECT final FROM resource WHERE id=$1")
        .bind(&ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(still_final, "its schedule stays");
    // The movie keeps its live poster; the other card is as it was.
    assert_eq!(
        stored(&pool, &ids[1]).await.2,
        [(CreditField::Image, CreditProvider::Tmdb)]
    );
    let after: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&other)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, untouched);
    assert_eq!(also_on::refill(&pool, || false).await.unwrap(), 0);
}

/// Captured log lines.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

/// Neither credential form appears anywhere but on the wire: not in the logs (every level), a
/// stored row of any table (cards, facts, `last_error`s, change lines), the egress' cache (keyed
/// by the URL alone) or an answer's URL, whether TMDB answers, fails or refuses it.
#[sqlx::test(migrations = "../../migrations")]
async fn the_credential_never_leaves_the_request(pool: PgPool) {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (ids, reader) = pages(&pool, &[MOVIE, SERIES]).await;
    for credential in [TOKEN, KEY] {
        for answer in [Answer::Recorded, Answer::Busy, Answer::Refused] {
            let api = Api::new();
            *api.answer.lock().unwrap() = answer;
            let cx = cx(config(Some(credential)), &api);
            look_up(&pool, &cx).await;
            read_again(&pool, &reader).await;
            assert!(!api.seen().is_empty());
            assert!(api.credentials.lock().unwrap().iter().all(|sent| *sent));
            for url in api.seen() {
                assert!(!url.contains(credential), "{url}");
            }
            // The cache holds the answer under the URL alone.
            if answer == Answer::Recorded {
                let options = FetchOptions {
                    accept: Some("application/json".into()),
                    sensitive: true,
                    cache_tag: Some("tmdb".into()),
                    ..Default::default()
                };
                let url = "https://api.themoviedb.org/3/find/tt0816692?external_source=imdb_id";
                assert!(cx.egress.discovered(url, &options).await.unwrap());
                let fetched = cx.egress.discover(url, &options, 3600).await.unwrap();
                assert!(!format!("{fetched:?}").contains(credential));
            }
            // Every row of every table, as text.
            let tables: Vec<String> = sqlx::query_scalar(
                "SELECT quote_ident(table_name) FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            for table in tables {
                let found: bool = sqlx::query_scalar(&format!(
                    "SELECT EXISTS(SELECT 1 FROM {table} t WHERE strpos(t::text,$1)>0)"
                ))
                .bind(credential)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert!(!found, "{table}");
            }
        }
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(!logged.is_empty());
        assert!(!logged.contains(credential), "{logged}");
    }
    // It was read: the token's run filled the cards.
    assert!(
        !stored(&pool, &ids[1]).await.2.is_empty() || !stored(&pool, &ids[0]).await.2.is_empty()
    );
}

/// Change lines keep a card's values for `limits.change_retention_days`: while a TMDB credential
/// is set, more than 120 days is refused (TMDB's six months, from a value up to 60 days old), with
/// a message naming both settings and never the key; without one, or with TMDB switched off, any
/// retention stands.
#[test]
fn a_long_change_retention_is_refused_while_tmdb_is_on() {
    for credential in [TOKEN, KEY] {
        let mut on = config(Some(credential));
        on.refresh.retention_days = 120;
        on.validate().unwrap();
        on.refresh.retention_days = 121;
        let refused = on.validate().unwrap_err().to_string();
        assert!(
            refused.contains("limits.change_retention_days") && refused.contains("TMDB_API_KEY"),
            "{refused}"
        );
        assert!(!refused.contains(credential), "{refused}");
    }
    let mut off = config(None);
    off.refresh.retention_days = 3650;
    off.validate().unwrap();
    let mut disabled = config(None);
    disabled
        .sections
        .set("tmdb", json!({"api_key": TOKEN, "enabled": false}));
    disabled.refresh.retention_days = 3650;
    disabled.validate().unwrap();
}

/// With the key removed, `/v1/info` still lists TMDB's attribution while a stored card holds a
/// TMDB value (a final card no read reaches); once the maintenance pass has taken the values
/// out, it lists none.
#[sqlx::test(migrations = "../../migrations")]
async fn the_attribution_stays_while_a_card_holds_a_tmdb_value(pool: PgPool) {
    let (ids, reader) = pages(&pool, &[MOVIE]).await;
    let on = config(Some(TOKEN));
    also_on::configure(&pool, &on).await.unwrap();
    look_up(&pool, &cx(on, &Api::new())).await;
    read_again(&pool, &reader).await;
    assert_eq!(
        stored(&pool, &ids[0]).await.2,
        [(CreditField::Image, CreditProvider::Tmdb)]
    );
    sqlx::query("UPDATE resource SET final=true WHERE id=$1")
        .bind(&ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let off = config(None);
    also_on::configure(&pool, &off).await.unwrap();
    let app = api::router(AppState::new(pool.clone(), off.clone()).unwrap());
    let info = get(&app, "/v1/info").await;
    assert_eq!(info["attributions"][0]["provider"], "tmdb", "{info}");
    assert_eq!(
        info["attributions"][0]["notice"],
        "This product uses the TMDB API but is not endorsed or certified by TMDB."
    );
    assert_eq!(also_on::refill(&pool, || false).await.unwrap(), 1);
    assert_eq!(stored(&pool, &ids[0]).await.2, vec![]);
    let app = api::router(AppState::new(pool.clone(), off).unwrap());
    let info = get(&app, "/v1/info").await;
    assert!(info.get("attributions").is_none(), "{info}");
}

/// A change line that holds a TMDB value goes after 120 days, whatever
/// `limits.change_retention_days` says and with TMDB off; a line of the same age without one
/// stays, as does a younger one.
#[sqlx::test(migrations = "../../migrations")]
async fn change_lines_with_tmdb_values_go_after_120_days(pool: PgPool) {
    let plain = Page {
        uri: "https://example.org/plain-page",
        kind: "WebPage",
        image: None,
        description: Some("A page."),
        keys: &[],
    };
    let (ids, reader) = pages(&pool, &[MOVIE, plain]).await;
    let on = config(Some(TOKEN));
    look_up(&pool, &cx(on, &Api::new())).await;
    read_again(&pool, &reader).await;
    let tmdb_lines = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource_change WHERE (before::text LIKE '%\"provider\": \"tmdb\"%' OR after::text LIKE '%\"provider\": \"tmdb\"%')")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let lines_of = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM resource_change WHERE resource_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert!(tmdb_lines().await > 0);
    let plain_lines = lines_of(ids[1].clone()).await;
    assert!(plain_lines > 0);
    let age = |days: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query("UPDATE resource_change SET at=now()-$1*interval '1 day'")
                .bind(days as f64)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    // TMDB off now, every line kept ten years.
    also_on::configure(&pool, &config(None)).await.unwrap();
    age(100).await;
    pinakes_core::changes::retain(&pool, 3650).await.unwrap();
    assert!(tmdb_lines().await > 0, "younger than 120 days: kept");
    age(121).await;
    pinakes_core::changes::retain(&pool, 3650).await.unwrap();
    assert_eq!(tmdb_lines().await, 0);
    assert_eq!(
        lines_of(ids[1].clone()).await,
        plain_lines,
        "no TMDB value: kept"
    );
    assert!(
        lines_of(ids[0].clone()).await > 0,
        "the movie's lines without one stay"
    );
}
