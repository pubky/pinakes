//! "Also on": the same thing on other services, as a hint on each card (`Card::also_on`).
//!
//! A [`Source`] reads one resource and returns [`Found`]: exact join keys it vouches for (two
//! resources with the same key join a group) and derived links (the same thing on a service
//! nobody tagged). The worker ([`run`]) runs every enabled source that applies to a resource
//! after a save that changed it and again when the shortest source lifetime has passed; each
//! run replaces that source's rows for the resource ([`record`]). Derived links are never
//! fetched, never counted as references and never ranked; the card lists them after the tagged
//! members of its group (`also_on()` in the schema).
//!
//! A source is a struct implementing [`Source`], listed in [`registry`]. Its requests go
//! through [`Egress`] (the per-host budgets apply), and a page's own claims (`sameAs`, an ISRC
//! in its text) are never keys: a key comes from the service that owns the identifier, or from
//! an exact hub lookup (MusicBrainz, Wikidata).
use crate::{config::Config, model::ResourceMeta, resolver::egress::Egress};

pub mod books;
pub mod films;
pub mod media;
pub mod podcasts;
pub mod titles;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::watch;

/// `[also_on]`: the switch, whether derived links are kept, and per-source switches.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlsoOnConfig {
    pub enabled: bool,
    /// Keep the derived links sources find (off: keys only, so groups still form).
    pub derive_links: bool,
    /// `[also_on.sources.<id>] enabled = false` turns one source off; the IDs are
    /// [`registry`]'s.
    pub sources: BTreeMap<String, SourceOptions>,
    /// The Apple Music storefront (country) of the Apple links sources build, `us` by default.
    pub apple_storefront: String,
    #[serde(skip)]
    pub links_per_source: usize,
    #[serde(skip)]
    pub keys_per_source: usize,
    #[serde(skip)]
    pub facts_per_source: usize,
    #[serde(skip)]
    pub concurrency: usize,
    #[serde(skip)]
    pub retry_secs: u64,
}
impl Default for AlsoOnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            derive_links: true,
            sources: BTreeMap::new(),
            apple_storefront: "us".into(),
            links_per_source: 32,
            keys_per_source: 16,
            facts_per_source: 32,
            concurrency: 2,
            retry_secs: 300,
        }
    }
}
impl AlsoOnConfig {
    pub fn source_enabled(&self, source: &dyn Source) -> bool {
        self.enabled
            && self
                .sources
                .get(source.id())
                .and_then(|s| s.enabled)
                .unwrap_or(source.enabled_by_default())
    }
    /// The MusicBrainz web service: `[also_on.sources.musicbrainz] base_url`, or the public one.
    pub fn musicbrainz_base(&self) -> &str {
        self.sources
            .get("musicbrainz")
            .and_then(|s| s.base_url.as_deref())
            .unwrap_or(crate::hubs::musicbrainz::BASE)
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceOptions {
    pub enabled: Option<bool>,
    /// The web service a hub source asks (`musicbrainz` only: a mirror's `…/ws/2`).
    pub base_url: Option<String>,
}

/// One resource as a source sees it.
#[derive(Clone, Debug)]
pub struct Subject {
    pub id: String,
    pub uri: String,
    /// `ok`, `missing`, `unreachable`, … (a source for gone pages reads the others).
    pub status: String,
    /// The schema.org type, if resolved.
    pub type_name: Option<String>,
    /// The stored document, if resolved.
    pub doc: Option<ResourceMeta>,
    /// The keys other sources found for this resource (so a source can start from a hub's
    /// MBID or Q-id), with the source that found each.
    pub keys: Vec<(String, String)>,
    /// When the resource was first referenced (for an archived copy closest to that time).
    pub first_seen_at: Option<DateTime<Utc>>,
}
impl Subject {
    /// The keys in one namespace (`mb:recording`, `wikidata`, …), without the prefix.
    pub fn keys_in<'a>(&'a self, namespace: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.keys.iter().filter_map(move |(key, _)| {
            key.strip_prefix(namespace)
                .and_then(|rest| rest.strip_prefix(':'))
        })
    }
}

/// What a source found for one resource. Empty means the service has nothing for it; an error
/// from [`Source::lookup`] means "retry later" and keeps the previous rows until they expire.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub keys: Vec<Key>,
    pub links: Vec<Link>,
    /// Facts about the object (`isrc`, `upc`, `artist`, …): shown and searchable, never joined.
    pub facts: Vec<Fact>,
    /// What was found changes the card itself (a media page's title): the resource is read again
    /// now, and its finished card takes it.
    pub reread: bool,
}
/// A fact a source reads for a resource: a lower-case `name` (`[a-z][a-z0-9_]`, at most 32
/// bytes) and a printable `value` (at most 512 bytes). Several values of one name are kept.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fact {
    pub name: String,
    pub value: String,
}
impl Fact {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}
/// An exact join key, `{namespace}:{value}` (`mb:recording:{mbid}`, `wikidata:Q42`, `isrc:…`
/// stays an attribute and is not a key). `via` names the hub it came through.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub key: String,
    pub via: String,
}
/// A derived link: `site` is the service's name as a card shows it ("Apple Music"), `via` the
/// hub or service it came through (`musicbrainz`, `wikidata`, `itunes`), `kind` a qualifier
/// such as `archived`, `at` a time that belongs to the link (an archived copy's capture).
/// `key` is the exact key that named it: the hub entity or item it is a relation or claim of
/// (`mb:recording:…`, `wikidata:Q…`), or the barcode or ISRC a service's lookup answered with
/// exactly this link. A tagged resource that is this link takes the key and joins the group;
/// one that two entities of one single-object namespace name carries both and joins no group
/// (`groups::single_object`). Set it only for
/// an exact derivation, never for a search or a name match.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub uri: String,
    pub site: String,
    pub via: String,
    pub kind: Option<String>,
    pub at: Option<DateTime<Utc>>,
    pub key: Option<String>,
}
impl Link {
    pub fn new(uri: impl Into<String>, site: impl Into<String>, via: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            site: site.into(),
            via: via.into(),
            kind: None,
            at: None,
            key: None,
        }
    }
    /// This link, named by an exact lookup of `key`.
    pub fn keyed(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }
}

/// What a source's lookup may use.
#[derive(Clone)]
pub struct Context {
    pub egress: Egress,
    pub config: Arc<Config>,
}

/// One source of keys and derived links. `id` is its config name (`[also_on.sources.<id>]`)
/// and the `source` of its rows: lower-case letters, digits and `_`, at most 32 bytes.
#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &'static str;
    /// Whether this source reads `subject` (by its URI's service, type or keys). No I/O.
    fn applies(&self, subject: &Subject) -> bool;
    /// How long its rows live before it runs again (they expire after twice that, so one
    /// failed refresh keeps them).
    fn ttl(&self) -> Duration {
        Duration::from_secs(30 * 86_400)
    }
    /// Before a batch of resources (their URIs): ask the service, in as few requests as it
    /// allows, what this source will ask for each, so their lookups find it cached. Best effort.
    async fn prefetch(&self, _cx: &Context, _uris: &[String]) {}
    /// Whether it runs without `[also_on.sources.<id>] enabled = true` (a service whose terms
    /// the operator should accept first is off by default).
    fn enabled_by_default(&self) -> bool {
        true
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found>;
}

/// Every source this build has. A source added by a later change is listed here.
pub fn registry() -> Vec<Arc<dyn Source>> {
    // In this order: each source sees the keys the ones before it found.
    vec![
        Arc::new(crate::music_sources::ServiceSource),
        Arc::new(crate::music_sources::MusicBrainzSource),
        Arc::new(crate::music_sources::WikidataSource),
        Arc::new(crate::music_sources::DeezerSource),
        Arc::new(crate::music_sources::ItunesSource),
        Arc::new(crate::wikipedia::WikipediaSource),
        Arc::new(crate::wikipedia::WikidataItemSource),
        Arc::new(books::Books),
        Arc::new(films::Films),
        Arc::new(podcasts::Podcasts),
        Arc::new(titles::Titles),
    ]
}
/// The source IDs `[also_on.sources]` may name.
pub fn source_ids() -> Vec<&'static str> {
    registry().iter().map(|s| s.id()).collect()
}

fn token(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
/// A key as stored: a lower-case namespace (letters, digits, `-`, `_`, and `:` between its
/// parts, e.g. `mb:recording`), then `:` and a value of printable characters without spaces;
/// 3–512 bytes.
pub fn valid_key(key: &str) -> bool {
    let Some((namespace, value)) = key.rsplit_once(':') else {
        return false;
    };
    (3..=512).contains(&key.len())
        && !namespace.is_empty()
        && namespace.split(':').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        })
        && !value.is_empty()
        && value.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

/// A derived link as stored: its resource ID, its normalised URI and the link.
type CheckedLink = (String, String, Link);
/// A fact's name and value as stored.
fn valid_fact(f: &Fact) -> bool {
    let name = f.name.as_bytes();
    (1..=32).contains(&name.len())
        && name[0].is_ascii_lowercase()
        && name
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        && (1..=512).contains(&f.value.len())
        && f.value.trim() == f.value
        && !f.value.chars().any(char::is_control)
}
/// A source's result, checked and bounded: keys valid, links HTTP(S) URLs that are not the
/// resource itself (normalised, deduplicated by their resource ID), sites and vias well formed,
/// facts well formed and each once.
fn checked(
    resource_id: &str,
    found: Found,
    cfg: &AlsoOnConfig,
) -> (Vec<Key>, Vec<CheckedLink>, Vec<Fact>) {
    let mut facts: Vec<Fact> = found
        .facts
        .into_iter()
        .filter_map(|mut f| {
            if let Some(normal) = identifier(&f.name) {
                f.value = normal(&f.value)?;
            }
            Some(f)
        })
        .filter(valid_fact)
        .collect();
    facts.sort();
    facts.dedup();
    facts.truncate(cfg.facts_per_source);
    let mut keys: Vec<Key> = found
        .keys
        .into_iter()
        .filter_map(|mut k| {
            k.key = normal_key(&k.key)?;
            Some(k)
        })
        .filter(|k| valid_key(&k.key) && token(&k.via))
        .collect();
    keys.sort();
    keys.dedup_by(|a, b| a.key == b.key);
    keys.truncate(cfg.keys_per_source);
    // In the source's order (its best first), each link once.
    let mut links = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if cfg.derive_links {
        for mut link in found.links {
            if links.len() == cfg.links_per_source {
                break;
            }
            let site = link.site.trim().to_owned();
            if site.is_empty()
                || site.len() > 128
                || site.chars().any(char::is_control)
                || !token(&link.via)
                || link.kind.as_deref().is_some_and(|k| {
                    !(1..=32).contains(&k.len())
                        || !k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
                // Credentials in a link are refused, not stripped.
                || url::Url::parse(&link.uri)
                    .map_or(true, |u| !u.username().is_empty() || u.password().is_some())
            {
                continue;
            }
            let Ok((id, uri, scheme)) = crate::model::identity(&link.uri) else {
                continue;
            };
            if !matches!(scheme.as_str(), "http" | "https") || id == resource_id || uri.len() > 2048
            {
                continue;
            }
            // A key that is not a valid key is dropped; the link stays.
            link.key = link
                .key
                .as_deref()
                .and_then(normal_key)
                .filter(|k| valid_key(k));
            if seen.insert(id.clone()) {
                link.site = site;
                links.push((id, uri, link));
            }
        }
    }
    (keys, links, facts)
}

/// ISRCs and barcodes have one stored form (`music::isrc`, `music::gtin13`), as keys and facts.
fn identifier(namespace: &str) -> Option<fn(&str) -> Option<String>> {
    match namespace {
        "isrc" => Some(crate::music::isrc),
        "upc" => Some(crate::music::gtin13),
        _ => None,
    }
}
/// A key in its stored form; a malformed ISRC or barcode is no key.
fn normal_key(key: &str) -> Option<String> {
    match key.split_once(':') {
        Some((namespace, value)) => match identifier(namespace) {
            Some(normal) => Some(format!("{namespace}:{}", normal(value)?)),
            None => Some(key.to_owned()),
        },
        None => Some(key.to_owned()),
    }
}
/// Replace `source`'s keys and links for `resource_id` with `found` (checked and bounded); they
/// live for twice `ttl`.
pub async fn record(
    pool: &PgPool,
    resource_id: &str,
    source: &str,
    found: Found,
    ttl: Duration,
    cfg: &AlsoOnConfig,
) -> Result<()> {
    anyhow::ensure!(token(source), "invalid source id {source}");
    let (keys, links, facts) = checked(resource_id, found, cfg);
    let expires = Utc::now()
        + chrono::Duration::from_std(ttl.saturating_mul(2)).unwrap_or(chrono::Duration::days(60));
    let mut tx = pool.begin().await?;
    // The write order first, then the resource rows this run's triggers update (its own, and
    // those its keyed links name, now and before) in ID order, so neither a save nor two runs
    // naming each other deadlock.
    lock_writers(&mut tx).await?;
    let named: Vec<&str> = std::iter::once(resource_id)
        .chain(
            links
                .iter()
                .filter(|(_, _, l)| l.key.is_some())
                .map(|(id, ..)| id.as_str()),
        )
        .collect();
    sqlx::query("SELECT 1 FROM resource WHERE id=ANY($1::text[] || ARRAY(SELECT link_id FROM derived_link WHERE resource_id=$2 AND source=$3 AND key IS NOT NULL)) ORDER BY id FOR NO KEY UPDATE")
        .bind(&named)
        .bind(resource_id)
        .bind(source)
        .execute(&mut *tx)
        .await?;
    let key_rows: Vec<Value> = keys
        .iter()
        .map(|k| serde_json::json!({"key": k.key, "via": k.via}))
        .collect();
    sqlx::query("DELETE FROM resource_key WHERE resource_id=$1 AND source=$2 AND NOT key=ANY(SELECT v->>'key' FROM jsonb_array_elements($3) v)")
        .bind(resource_id).bind(source).bind(Value::from(key_rows.clone())).execute(&mut *tx).await?;
    // A key another source already vouches for keeps that source; this one adds nothing to it.
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) SELECT $1,v->>'key',$2,v->>'via',$4 FROM jsonb_array_elements($3) v ON CONFLICT(resource_id,key) DO UPDATE SET seen_at=now(),expires_at=greatest(resource_key.expires_at,excluded.expires_at) WHERE resource_key.source=excluded.source")
        .bind(resource_id).bind(source).bind(Value::from(key_rows)).bind(expires).execute(&mut *tx).await?;
    let link_rows: Vec<Value> = links
        .iter()
        .map(|(id, uri, l)| serde_json::json!({"id": id, "uri": uri, "site": l.site, "via": l.via, "kind": l.kind, "at": l.at, "key": l.key}))
        .collect();
    sqlx::query("DELETE FROM derived_link WHERE resource_id=$1 AND source=$2 AND NOT link_id=ANY(SELECT v->>'id' FROM jsonb_array_elements($3) v)")
        .bind(resource_id).bind(source).bind(Value::from(link_rows.clone())).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO derived_link(resource_id,link_id,uri,site,source,via,kind,at,key,expires_at) SELECT $1,v->>'id',v->>'uri',v->>'site',$2,v->>'via',v->>'kind',(v->>'at')::timestamptz,v->>'key',$4 FROM jsonb_array_elements($3) v ON CONFLICT(resource_id,link_id) DO UPDATE SET uri=excluded.uri,site=excluded.site,via=excluded.via,kind=excluded.kind,at=excluded.at,key=excluded.key,seen_at=now(),expires_at=excluded.expires_at WHERE derived_link.source=excluded.source")
        .bind(resource_id).bind(source).bind(Value::from(link_rows)).bind(expires).execute(&mut *tx).await?;
    let fact_rows: Vec<Value> = facts
        .iter()
        .map(|f| serde_json::json!({"name": f.name, "value": f.value}))
        .collect();
    sqlx::query("DELETE FROM resource_fact WHERE resource_id=$1 AND source=$2 AND NOT (name,value) IN (SELECT v->>'name',v->>'value' FROM jsonb_array_elements($3) v)")
        .bind(resource_id).bind(source).bind(Value::from(fact_rows.clone())).execute(&mut *tx).await?;
    // A fact another source already reports keeps that source.
    sqlx::query("INSERT INTO resource_fact(resource_id,name,value,source,expires_at) SELECT $1,v->>'name',v->>'value',$2,$4 FROM jsonb_array_elements($3) v ON CONFLICT(resource_id,name,value) DO UPDATE SET seen_at=now(),expires_at=greatest(resource_fact.expires_at,excluded.expires_at) WHERE resource_fact.source=excluded.source")
        .bind(resource_id).bind(source).bind(Value::from(fact_rows)).bind(expires).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// The write order (`store::lock_writers`) shared, as saves take it, and the alias lock: a run
/// locks rows of several resources in an order of its own (ID order), which a shared writer may
/// meet the other way round.
async fn lock_writers(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    crate::store::lock_writers(tx, crate::store::WriteOrder::Shared).await?;
    crate::alias::lock(tx).await
}

/// Delete keys and links nobody refreshed in time (their deletion regroups as a change would).
pub async fn prune(pool: &PgPool) -> Result<u64> {
    // Each deletion takes the write order, then locks what its triggers update, as `record` does.
    let mut tx = pool.begin().await?;
    lock_writers(&mut tx).await?;
    sqlx::query("SELECT 1 FROM resource WHERE id IN (SELECT resource_id FROM resource_key WHERE expires_at<=now()) ORDER BY id FOR NO KEY UPDATE")
        .execute(&mut *tx)
        .await?;
    let keys = sqlx::query("DELETE FROM resource_key WHERE expires_at<=now()")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    let mut tx = pool.begin().await?;
    lock_writers(&mut tx).await?;
    sqlx::query("SELECT 1 FROM resource WHERE id IN (SELECT link_id FROM derived_link WHERE expires_at<=now() AND key IS NOT NULL) ORDER BY id FOR NO KEY UPDATE")
        .execute(&mut *tx)
        .await?;
    let links = sqlx::query("DELETE FROM derived_link WHERE expires_at<=now()")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    let facts = sqlx::query("DELETE FROM resource_fact WHERE expires_at<=now()")
        .execute(pool)
        .await?
        .rows_affected();
    Ok(keys + links + facts)
}

async fn subject(pool: &PgPool, id: &str) -> Result<Option<Subject>> {
    let row: Option<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',r.id,'uri',r.uri,'status',r.status,'type',r.type,'doc',r.doc,'first_seen_at',(SELECT min(created_at) FROM reference f WHERE f.resource_id=r.id),'keys',coalesce((SELECT jsonb_agg(jsonb_build_array(k.key,k.source) ORDER BY k.key) FROM resource_key k WHERE k.resource_id=r.id AND k.expires_at>now()),'[]')) FROM resource_document r WHERE r.id=$1 AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id)")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(Subject {
        id: row["id"].as_str().unwrap_or_default().into(),
        uri: row["uri"].as_str().unwrap_or_default().into(),
        status: row["status"].as_str().unwrap_or_default().into(),
        type_name: row["type"].as_str().map(str::to_owned),
        doc: row["doc"]
            .get("type")
            .and_then(|_| serde_json::from_value(row["doc"].clone()).ok()),
        keys: serde_json::from_value(row["keys"].clone()).unwrap_or_default(),
        first_seen_at: row["first_seen_at"].as_str().and_then(|s| s.parse().ok()),
    }))
}

/// Run the due sources of up to `limit` resources; returns how many were handled.
pub async fn drain(
    pool: &PgPool,
    cx: &Context,
    sources: &[Arc<dyn Source>],
    limit: i64,
) -> Result<usize> {
    let cfg = &cx.config.also_on;
    // Claimed rows move one lease ahead, so a crashed worker's rows come back.
    let due: Vec<String> = sqlx::query_scalar("UPDATE also_on_dirty SET next_attempt_at=now()+interval '10 minutes' WHERE resource_id IN (SELECT resource_id FROM also_on_dirty WHERE next_attempt_at<=now() ORDER BY next_attempt_at,resource_id LIMIT $1 FOR UPDATE SKIP LOCKED) RETURNING resource_id")
        .bind(limit)
        .fetch_all(pool)
        .await?;
    let handled = due.len();
    // Batched hub lookups for the whole batch first (MusicBrainz URLs, Wikidata items).
    let uris: Vec<String> = sqlx::query_scalar("SELECT uri FROM resource WHERE id=ANY($1)")
        .bind(&due)
        .fetch_all(pool)
        .await?;
    for source in sources.iter().filter(|s| cfg.source_enabled(s.as_ref())) {
        source.prefetch(cx, &uris).await;
    }
    let work = due.into_iter().map(|id| async move {
        let result = one(pool, cx, sources, &id).await;
        (id, result)
    });
    let results = futures_util::stream::iter(work)
        .buffer_unordered(cfg.concurrency.max(1))
        .collect::<Vec<_>>()
        .await;
    // A change saved while a resource was read marked it due again (its lease, ten minutes ahead,
    // became now): that mark stands, so the change is read too.
    for (id, result) in results {
        match result {
            Ok(Some(next)) => {
                sqlx::query("UPDATE also_on_dirty SET next_attempt_at=now()+$2,attempts=0,last_error=NULL WHERE resource_id=$1 AND next_attempt_at>now()")
                    .bind(&id).bind(next).execute(pool).await?;
            }
            Ok(None) => {
                sqlx::query(
                    "DELETE FROM also_on_dirty WHERE resource_id=$1 AND next_attempt_at>now()",
                )
                .bind(&id)
                .execute(pool)
                .await?;
            }
            Err(error) => {
                tracing::warn!(resource=%id, error=%format!("{error:#}"), "also-on lookup will retry");
                sqlx::query("UPDATE also_on_dirty SET attempts=attempts+1,last_error=$2,next_attempt_at=now()+make_interval(secs => least($3::float8*power(2,least(attempts,6)),3600)) WHERE resource_id=$1")
                    .bind(&id).bind(crate::resolver::error_class(&error)).bind(cfg.retry_secs as f64).execute(pool).await?;
            }
        }
    }
    Ok(handled)
}
/// One resource: every enabled source that applies runs; each success replaces its rows. Ok
/// with the time until the next run (the shortest lifetime among them), Ok(None) when no
/// source applies, Err when any source failed (the others' results are kept).
async fn one(
    pool: &PgPool,
    cx: &Context,
    sources: &[Arc<dyn Source>],
    id: &str,
) -> Result<Option<chrono::Duration>> {
    let Some(mut subject) = subject(pool, id).await? else {
        return Ok(None);
    };
    let cfg = &cx.config.also_on;
    let mut next: Option<Duration> = None;
    let mut failed = None;
    let mut reread = false;
    // In registry order: each source sees the keys the ones before it found in this run (a
    // barcode MusicBrainz names, then the services looked up by it).
    for source in sources.iter().filter(|s| cfg.source_enabled(s.as_ref())) {
        if !source.applies(&subject) {
            // It read the resource before (a key it starts from is gone): its rows go too.
            let had: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource_key WHERE resource_id=$1 AND source=$2) OR EXISTS(SELECT 1 FROM derived_link WHERE resource_id=$1 AND source=$2) OR EXISTS(SELECT 1 FROM resource_fact WHERE resource_id=$1 AND source=$2)")
                .bind(id)
                .bind(source.id())
                .fetch_one(pool)
                .await?;
            if had {
                record(pool, id, source.id(), Found::default(), source.ttl(), cfg)
                    .await
                    .with_context(|| format!("clear {}", source.id()))?;
            }
            continue;
        }
        match source.lookup(cx, &subject).await {
            Ok(found) => {
                reread |= found.reread;
                record(pool, id, source.id(), found, source.ttl(), cfg)
                    .await
                    .with_context(|| format!("record {}", source.id()))?;
                next = Some(next.map_or(source.ttl(), |n| n.min(source.ttl())));
                subject.keys = sqlx::query_as("SELECT key,source FROM resource_key WHERE resource_id=$1 AND expires_at>now() ORDER BY key")
                    .bind(id)
                    .fetch_all(pool)
                    .await?;
            }
            Err(error) => failed = Some(error.context(format!("source {}", source.id()))),
        }
    }
    if reread {
        sqlx::query("UPDATE resource SET next_attempt_at=now(),refresh_at=now(),priority_at=coalesce(priority_at,now()) WHERE id=$1 AND NOT final AND NOT dead_letter")
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(next.map(|n| chrono::Duration::from_std(n).unwrap_or(chrono::Duration::days(30))))
}

/// The worker: drains due resources until shutdown, pausing when nothing is due.
pub async fn run(
    pool: PgPool,
    cx: Context,
    sources: Vec<Arc<dyn Source>>,
    mut shutdown: watch::Receiver<bool>,
) {
    if !cx.config.also_on.enabled || sources.is_empty() {
        return;
    }
    loop {
        // Up to 100 resources a tick, so their hub lookups batch.
        let handled = tokio::select! { _=shutdown.changed()=>break, result=drain(&pool,&cx,&sources,100)=>result };
        match handled {
            Ok(100) => continue,
            Ok(_) => {}
            Err(error) => tracing::warn!(error=%format!("{error:#}"), "also-on worker will retry"),
        }
        tokio::select! { _=shutdown.changed()=>break, _=tokio::time::sleep(Duration::from_secs(5))=>{} }
    }
}
