//! The `apps` family in storage: an app's store links join one group through their store keys
//! and the Wikidata item that names them, against the recorded answers in
//! `tests/fixtures/lookups/apps`.
use pinakes_core::{
    also_on,
    config::Config,
    groups,
    model::ResourceMeta,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
    store,
};
use std::{collections::BTreeMap, io::Read, net::SocketAddr, sync::Arc};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lookups/apps");

struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

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
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: u.clone(),
            content_type,
            status,
            retry_after_secs: 60,
            location: None,
            body,
        })
    }
}

/// Signal's pages on Google Play, the App Store, Flathub and the Snap Store join one group (a
/// Play link in German and a storefront's App Store page are aliases of them): each finds
/// Signal's Wikidata item (Signal replaces TextSecure, which carries the same Android package),
/// whose store IDs are keys of each. A Google Play and an F-Droid page of a package Wikidata
/// lacks join through the package alone.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
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
