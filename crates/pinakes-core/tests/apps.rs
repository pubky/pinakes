//! The `apps` family in storage: an app's store links join one group through their store keys
//! and the Wikidata item that names them, against the recorded answers in
//! `tests/fixtures/lookups/apps`.
use pinakes_core::{
    also_on,
    api::{self, AppState},
    config::Config,
    groups,
    model::ResourceMeta,
    resolver::egress::{Egress, FetchOptions, Fetched, Transport},
    store,
};
use std::{collections::BTreeMap, io::Read, net::SocketAddr, sync::Arc};
use tower::ServiceExt;
mod common;
use common::media::PublicDns;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lookups/apps");

/// The recorded answers by URL; a Wikidata search nobody recorded finds nothing, anything
/// else is a 404.
struct Recorded(BTreeMap<String, (u16, String, Vec<u8>)>);
impl Recorded {
    fn new() -> Self {
        let read = |file: &str| {
            let raw = std::fs::read(format!("{FIXTURES}/{file}")).unwrap();
            if file.ends_with(".gz") {
                let mut out = vec![];
                flate2::read::GzDecoder::new(raw.as_slice())
                    .read_to_end(&mut out)
                    .unwrap();
                out
            } else {
                raw
            }
        };
        let provenance: serde_json::Value =
            serde_json::from_slice(&read("provenance.json")).unwrap();
        Self(
            provenance
                .as_object()
                .unwrap()
                .values()
                .map(|p| {
                    (
                        p["url"].as_str().unwrap().to_owned(),
                        (
                            p["status"].as_u64().unwrap() as u16,
                            p["content_type"].as_str().unwrap_or("").to_owned(),
                            read(p["file"].as_str().unwrap()),
                        ),
                    )
                })
                .collect(),
        )
    }
}
#[async_trait::async_trait]
impl Transport for Recorded {
    async fn get(&self, u: &url::Url, addrs: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(u, addrs, &Default::default()).await
    }
    async fn get_with(
        &self,
        u: &url::Url,
        _: &[SocketAddr],
        _: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let (status, content_type, body) = match self.0.get(u.as_str()) {
            Some(answer) => answer.clone(),
            None if u.as_str().contains("list=search") => (
                200,
                "application/json".into(),
                br#"{"query":{"search":[]}}"#.to_vec(),
            ),
            None => (404, "text/plain".into(), vec![]),
        };
        Ok(common::fetched(u.clone(), status, content_type, body))
    }
}

/// Signal's pages on Google Play, the App Store, Flathub and the Snap Store join one group (a
/// Play link in German and a storefront's App Store page are aliases of them): each finds
/// Signal's Wikidata item (Signal replaces TextSecure, which carries the same Android package),
/// whose store IDs are keys of each. A Google Play and an F-Droid page of a package Wikidata
/// lacks join through the package alone.
#[sqlx::test(migrations = "../../migrations")]
async fn an_apps_store_links_join_one_group(pool: sqlx::PgPool) {
    if !also_on::registry().iter().any(|s| s.id() == "app_stores") {
        return;
    }
    // Each store's own address: every other spelling is an alias of it.
    let signal = [
        "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms",
        "https://apps.apple.com/app/id874139669",
        "https://flathub.org/apps/org.signal.Signal",
        "https://snapcraft.io/signal-desktop",
    ];
    let unlisted = [
        "https://play.google.com/store/apps/details?id=org.example.unlisted",
        "https://f-droid.org/packages/org.example.unlisted",
    ];
    let mut ids = vec![];
    for uri in signal.iter().chain(&unlisted) {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.set_type("MobileApplication");
        d.name = Some("An app".into());
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    // A Play link in another language, and the App Store's page in a storefront, are the same
    // app: aliases of its one address.
    for (spelling, root) in [
        (
            "https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms&hl=de",
            &ids[0],
        ),
        (
            "https://apps.apple.com/us/app/signal-private-messenger/id874139669",
            &ids[1],
        ),
        (
            "https://f-droid.org/en/packages/org.example.unlisted/",
            &ids[5],
        ),
    ] {
        let id = store::ensure_resource(&pool, spelling).await.unwrap();
        assert_eq!(
            &pinakes_core::alias::resolve_id(&pool, &id).await.unwrap(),
            root,
            "{spelling}"
        );
    }
    let config = Config::default();
    let cx = also_on::Context {
        egress: Egress::with_backends(
            Default::default(),
            Arc::new(PublicDns),
            Arc::new(Recorded::new()),
        ),
        config: Arc::new(config),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let group_of = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT (group_hint($1))->>'id'")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let mut signal_groups = vec![];
    for id in &ids[..4] {
        signal_groups.push(group_of(id.clone()).await);
    }
    assert!(signal_groups[0].is_some(), "{signal_groups:?}");
    assert!(
        signal_groups.iter().all(|g| g == &signal_groups[0]),
        "{signal_groups:?}"
    );
    let unlisted_groups = (
        group_of(ids[4].clone()).await,
        group_of(ids[5].clone()).await,
    );
    assert!(unlisted_groups.0.is_some(), "{unlisted_groups:?}");
    assert_eq!(unlisted_groups.0, unlisted_groups.1);
    assert_ne!(unlisted_groups.0, signal_groups[0]);
    // The keys the store pages carry: their own, and Signal's through its item.
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT key FROM resource_key WHERE resource_id = ANY($1) ORDER BY key",
    )
    .bind(&ids[..4])
    .fetch_all(&pool)
    .await
    .unwrap();
    for key in [
        "android:org.thoughtcrime.securesms",
        "appstore:874139669",
        "flathub:org.signal.Signal",
        "snap:signal-desktop",
        "wikidata:Q19718090",
    ] {
        assert!(keys.iter().any(|k| k == key), "{key} in {keys:?}");
    }
}

/// An app Wikidata lacks, on the App Store and Flathub under one website and one name (as each
/// store spells it): no group, but each card names the other as possibly the same app. A Snap
/// with the same name under another website is not named.
#[sqlx::test(migrations = "../../migrations")]
async fn an_app_wikidata_lacks_is_only_possibly_the_same(pool: sqlx::PgPool) {
    if !also_on::registry().iter().any(|s| s.id() == "app_stores") {
        return;
    }
    let cards = [
        (
            "https://apps.apple.com/app/id1234567890",
            "Example Notes - Private Notes",
            "https://www.example.org/",
        ),
        (
            "https://flathub.org/apps/org.example.Notes",
            "Example Notes: Private Notes",
            "https://example.org",
        ),
        (
            "https://snapcraft.io/example-notes",
            "Example Notes - Private Notes",
            "https://notes.example/",
        ),
    ];
    let mut ids = vec![];
    for (uri, name, website) in cards {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "web", 86400);
        d.set_type("SoftwareApplication");
        d.name = Some(name.into());
        d.attributes.insert("website".into(), website.into());
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    let cx = also_on::Context {
        egress: Egress::with_backends(
            Default::default(),
            Arc::new(PublicDns),
            Arc::new(Recorded::new()),
        ),
        config: Arc::new(Config::default()),
    };
    sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    also_on::drain(&pool, &cx, &also_on::registry(), 100)
        .await
        .unwrap();
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let facts: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM resource_fact WHERE name='app_match' AND resource_id=ANY($1) ORDER BY value",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        facts,
        [
            "example.org example notes private notes",
            "example.org example notes private notes",
            "notes.example example notes private notes",
        ]
    );
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = |id: &String| {
        let app = app.clone();
        let mut request = axum::http::Request::get(format!("/v1/resources/{id}"))
            .body(axum::body::Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
        ));
        async move {
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), 200);
            let body = http_body_util::BodyExt::collect(response.into_body())
                .await
                .unwrap()
                .to_bytes();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        }
    };
    let named = |card: &serde_json::Value| -> Vec<String> {
        card["maybe_same"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let store_card = read(&ids[0]).await;
    assert_eq!(named(&store_card), [ids[1].clone()], "{store_card}");
    assert_eq!(store_card["maybe_same"][0]["score"], 1.0);
    assert_eq!(named(&read(&ids[1]).await), [ids[0].clone()]);
    assert!(named(&read(&ids[2]).await).is_empty());
    // The app hint goes with the same-event switch, whatever the same-recording one says.
    for (settings, hinted) in [
        ("[groups]\nsame_recording_hint = \"off\"\n", true),
        ("[event]\nsame_event_hint = \"off\"\n", false),
    ] {
        let config: Config = toml::from_str(settings).unwrap();
        let app = api::router(AppState::new(pool.clone(), config).unwrap());
        let mut request = axum::http::Request::get(format!("/v1/resources/{}", ids[0]))
            .body(axum::body::Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let card: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            named(&card).len(),
            usize::from(hinted),
            "{settings}: {card}"
        );
    }
    let group = |id: &String| {
        let (pool, id) = (pool.clone(), id.clone());
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT (group_hint($1))->>'id'")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let (a, b) = (group(&ids[0]).await, group(&ids[1]).await);
    assert!(a.is_none() || a != b, "never one group: {a:?} {b:?}");
}
