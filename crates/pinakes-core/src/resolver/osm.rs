use super::Resolver;
use crate::{
    config::OsmConfig,
    model::{LocationSource, OsmId, Point, ResourceMeta, Status},
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, HashMap},
    sync::OnceLock,
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

pub const ATTRIBUTION: &str = "© OpenStreetMap contributors (ODbL)";
pub const ATTRIBUTES: &[&str] = &[
    "opening_hours",
    "website",
    "contact:website",
    "phone",
    "contact:phone",
    "email",
    "cuisine",
    "wheelchair",
    "brand",
    "operator",
    "currency:XBT",
    "payment:lightning",
    "payment:onchain",
    "payment:lightning_contactless",
    "wikidata",
    "wikipedia",
    "level",
    "capacity",
];

impl OsmId {
    pub fn key(&self) -> String {
        format!("{}/{}", self.osm_type, self.id)
    }
    pub fn lookup_id(&self) -> String {
        format!(
            "{}{}",
            match self.osm_type.as_str() {
                "node" => "N",
                "way" => "W",
                _ => "R",
            },
            self.id
        )
    }
    pub fn uri(&self) -> String {
        format!("https://www.openstreetmap.org/{}", self.key())
    }
}
pub fn osm_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "openstreetmap.org"
        || host.ends_with(".openstreetmap.org")
        || host == "osm.org"
        || host.ends_with(".osm.org")
}
pub fn public_upstream(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => super::egress::public_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => super::egress::public_ip(ip.into()),
        Some(url::Host::Domain(host)) => osm_host(host),
        None => false,
    }
}
/// An OSM object named by its canonical URL or an alias of it: `/{type}/{id}` with an optional
/// trailing slash or query, `/{type}/{id}/history[/{version}]`, `/edit?{type}={id}`, object
/// shortlinks `/go/<code>?{type}={id}`, the editing API
/// `api.openstreetmap.org/api/0.6/{type}/{id}[/full][.json|.xml]` (`full` for ways and
/// relations) and Nominatim `details`/`ui/details.html?osmtype=N|W|R&osmid=` (each exactly once,
/// without `place_id` or `postcode`). `/edit` and shortlinks name exactly one object. Every alias
/// resolves through the shared `osm_place` row and carries the same `osm:` group key.
pub fn parse_id(uri: &str) -> Option<OsmId> {
    let url = url::Url::parse(uri).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let (osm_type, digits) = match host.as_str() {
        "www.openstreetmap.org" | "openstreetmap.org" | "osm.org" | "www.osm.org" => {
            // osm.org's Share → Short link on an object page is `/go/<code>?way=N`, whose
            // permalink redirects to `/way/N`; the code is only the map centre.
            let shortlink = path
                .strip_prefix("go/")
                .is_some_and(|code| crate::resolver::geo::shortlink(code).is_some());
            if path == "edit" || shortlink {
                let mut found = url
                    .query_pairs()
                    .filter(|(k, _)| matches!(k.as_ref(), "node" | "way" | "relation"));
                let (k, v) = found.next()?;
                if found.next().is_some() {
                    return None;
                }
                (k.into_owned(), v.into_owned())
            } else {
                let path = path.strip_suffix('/').unwrap_or(path);
                let mut parts = path.split('/');
                let (osm_type, digits) = (parts.next()?, parts.next()?);
                match (parts.next(), parts.next(), parts.next()) {
                    (None, _, _) | (Some("history"), None, _) => {}
                    (Some("history"), Some(version), None) if positive(version).is_some() => {}
                    _ => return None,
                }
                (osm_type.to_owned(), digits.to_owned())
            }
        }
        "api.openstreetmap.org" => {
            let rest = path.strip_prefix("api/0.6/")?;
            let rest = [".json", ".xml"]
                .iter()
                .find_map(|ext| rest.strip_suffix(ext))
                .unwrap_or(rest);
            let (osm_type, rest) = rest.split_once('/')?;
            let digits = match rest.split_once('/') {
                Some((digits, "full")) if osm_type != "node" => digits,
                Some(_) => return None,
                None => rest,
            };
            (osm_type.to_owned(), digits.to_owned())
        }
        "nominatim.openstreetmap.org" if matches!(path, "details" | "ui/details.html") => {
            // Nominatim's details endpoint reads `place_id`, then `postcode`, before `osmtype`,
            // and accepts only N, W and R; anything ambiguous is not an alias.
            let only = |name: &str| {
                let mut values = url.query_pairs().filter(|(k, _)| k == name);
                let (_, value) = values.next()?;
                values.next().is_none().then(|| value.into_owned())
            };
            if url
                .query_pairs()
                .any(|(k, _)| matches!(k.as_ref(), "place_id" | "postcode"))
            {
                return None;
            }
            let osm_type = match only("osmtype")?.as_str() {
                "N" => "node",
                "W" => "way",
                "R" => "relation",
                _ => return None,
            };
            (osm_type.to_owned(), only("osmid")?)
        }
        _ => return None,
    };
    if !matches!(osm_type.as_str(), "node" | "way" | "relation") {
        return None;
    }
    Some(OsmId {
        osm_type,
        id: positive(&digits)?,
    })
}
/// A positive decimal id without sign or leading zero that fits in i64.
fn positive(digits: &str) -> Option<i64> {
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().filter(|id| *id > 0)
}
/// When a link is read again after the upstream refused Pinakes (401, 403) without saying when.
const DENIED_RETRY_SECS: u64 = 900;
fn copy_error(error: &anyhow::Error) -> anyhow::Error {
    if let Some(retry) = error.downcast_ref::<super::RetryLater>() {
        return retry.clone().into();
    }
    // An answer's class and status stay on each job's copy, for its stored error and log line.
    if let (Some(class), Some(status)) = (
        error.downcast_ref::<super::ErrorClass>(),
        error.downcast_ref::<super::Upstream>(),
    ) {
        return anyhow::Error::new(*class).context(super::Upstream(status.0));
    }
    if error.is::<super::NotModified>() {
        return super::NotModified.into();
    }
    anyhow::anyhow!("{error:#}")
}
pub(super) fn retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    retry_after_named(headers).unwrap_or(60)
}
/// The wait a `Retry-After` header names (seconds or an HTTP date), 1 s to an hour; `None`
/// without one.
pub(super) fn retry_after_named(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.parse::<u64>().ok().or_else(|| {
                chrono::DateTime::parse_from_rfc2822(s).ok().map(|at| {
                    (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
                        .num_seconds()
                        .max(1) as u64
                })
            })
        })
        .map(|seconds| seconds.clamp(1, 3600))
}
pub fn schema_type(category: &str) -> &'static str {
    static MAP: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        serde_json::from_str::<Value>(include_str!("../../../../data/type-mappings.json")).unwrap()
            ["osm"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
            .collect()
    });
    map.get(category)
        .or_else(|| {
            category
                .split_once('/')
                .and_then(|(c, _)| map.get(&format!("{c}/*")))
        })
        .map(String::as_str)
        .unwrap_or("Place")
}
fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}
fn decimal(value: &Value) -> Result<f64> {
    let n = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .context("missing coordinate")?;
    ensure!(n.is_finite(), "non-finite coordinate");
    Ok(n)
}
pub fn map_place(value: &Value, ttl: u64) -> Result<ResourceMeta> {
    let osm_type = value["osm_type"].as_str().context("missing osm_type")?;
    let id = value["osm_id"].as_i64().context("missing osm_id")?;
    let uri = format!("https://www.openstreetmap.org/{osm_type}/{id}");
    let key = parse_id(&uri).context("invalid OSM object")?;
    let mut doc = ResourceMeta::new(&uri, "osm", ttl);
    doc.osm_id = Some(key.key());
    doc.attribution = Some(ATTRIBUTION.into());
    doc.location = Some(Point {
        lat: decimal(&value["lat"])?,
        lon: decimal(&value["lon"])?,
    });
    doc.location_source = Some(LocationSource::Osm);
    doc.category = text(&value["category"])
        .or_else(|| text(&value["class"]))
        .zip(text(&value["type"]))
        .map(|(c, t)| format!("{c}/{t}"));
    doc.set_type(schema_type(doc.category.as_deref().unwrap_or_default()));
    doc.name = text(&value["namedetails"]["name"])
        .or_else(|| text(&value["namedetails"]["name:en"]))
        .or_else(|| text(&value["display_name"]).and_then(|s| s.split(',').next()))
        .map(|s| s.trim().to_owned());
    let address = &value["address"];
    for (key, candidates) in [
        ("street", &["road", "pedestrian", "footway", "street"][..]),
        ("house_number", &["house_number"]),
        ("postcode", &["postcode"]),
        ("city", &["city", "town", "village", "municipality"]),
        ("country_code", &["country_code"]),
    ] {
        if let Some(v) = candidates.iter().find_map(|k| text(&address[*k])) {
            doc.address.insert(key.into(), v.into());
        }
    }
    if let Some(tags) = value["extratags"].as_object() {
        for key in ATTRIBUTES {
            if let Some(s) = tags.get(*key).and_then(Value::as_str) {
                doc.attributes.insert((*key).into(), s.into());
            }
        }
    }
    if let Some(bounds) = value["boundingbox"].as_array() {
        ensure!(bounds.len() == 4, "invalid bounding box");
        let b = [
            decimal(&bounds[2])?,
            decimal(&bounds[0])?,
            decimal(&bounds[3])?,
            decimal(&bounds[1])?,
        ];
        ensure!(
            b[0] >= -180.0
                && b[2] <= 180.0
                && b[1] >= -90.0
                && b[3] <= 90.0
                && b[0] <= b[2]
                && b[1] <= b[3],
            "invalid bounding box"
        );
        doc.bbox = Some(b);
    }
    doc.bounded(ATTRIBUTES)
}
fn missing(id: &OsmId, ttl: u64) -> ResourceMeta {
    let mut doc = ResourceMeta::new(&id.uri(), "osm", ttl);
    doc.set_type("Place");
    doc.osm_id = Some(id.key());
    doc.attribution = Some(ATTRIBUTION.into());
    doc.status = Status::Missing;
    doc
}

pub struct OsmResolver {
    pub pool: PgPool,
    pub config: OsmConfig,
    client: reqwest::Client,
    next: Mutex<Instant>,
}
impl OsmResolver {
    pub fn new(pool: PgPool, config: OsmConfig) -> Result<Self> {
        // A client of its own, outside `Egress`: it asks only the Nominatim URL the operator
        // sets (`osm.nominatim_url`), which no reference steers; it follows no redirect and
        // reads at most 2 MiB (`request`).
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(super::egress::user_agent(&config.contact))
            .gzip(true)
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            pool,
            config,
            client,
            next: Mutex::new(Instant::now()),
        })
    }
    pub(crate) async fn request(&self, url: reqwest::Url) -> Result<Option<Value>> {
        // One lock covers the request too: all batches and fallback calls share the host budget.
        let mut next = self.next.lock().await;
        // A cooldown the upstream asked for (its 429 or 5xx) is not slept through with the lock
        // held: the job is put off until then, without a request and without a charge, as
        // Egress does for a host slot further away than `MAX_SLOT_WAIT`.
        let wait = next.saturating_duration_since(Instant::now());
        if wait > super::egress::MAX_SLOT_WAIT {
            return Err(super::RetryLater {
                class: "rate_limited",
                seconds: wait.as_secs() + 1,
            }
            .into());
        }
        tokio::time::sleep_until(*next).await;
        let public = public_upstream(&url);
        let rate = if public {
            self.config.requests_per_second.min(1.0)
        } else {
            self.config.requests_per_second
        };
        *next = Instant::now() + Duration::from_secs_f64(1.0 / rate);
        let mut observation = crate::telemetry::FetchGuard::start();
        let response =
            self.client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| super::RetryLater {
                    class: if e.is_timeout() { "timeout" } else { "connect" },
                    seconds: 60,
                })?;
        observation.status(response.status().as_u16());
        let status = response.status();
        // The request's path below the configured base (`lookup`, `search`): never the query, a
        // user's link or the base's own path, which may hold a token.
        let base = url::Url::parse(&self.config.nominatim_url).ok();
        let below = base
            .as_ref()
            .and_then(|base| url.path().strip_prefix(base.path().trim_end_matches('/')))
            .map_or("", |path| path.trim_start_matches('/'));
        tracing::info!(
            source = "osm",
            path = below,
            status = status.as_u16(),
            "OSM upstream request"
        );
        if status.as_u16() == 304 {
            return Err(super::NotModified.into());
        }
        // A 429 is the operator's own quota at the upstream (it answers every job of the batch
        // alike): the daemon's own wait (`quota`), which charges no link. A 5xx is charged.
        if status.as_u16() == 429 || status.is_server_error() {
            let seconds = retry_after(response.headers());
            *next = Instant::now() + Duration::from_secs(seconds);
            return Err(super::RetryLater {
                class: if status.as_u16() == 429 {
                    "quota"
                } else {
                    "http_5xx"
                },
                seconds,
            }
            .into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Ok(None);
        }
        // The upstream refuses Pinakes (a key expired or out of quota), which says nothing of
        // the link: it is read again later and never dead-letters for it. The refusal is
        // counted in `pinakes_source_token_refusals_total{source="osm"}`.
        if matches!(status.as_u16(), 401 | 403) {
            crate::sources::token_refused("osm");
            return Err(super::RetryLater {
                class: "upstream_denied",
                seconds: retry_after_named(response.headers()).unwrap_or(DENIED_RETRY_SECS),
            }
            .into());
        }
        if !status.is_success() {
            // A redirect is no client error: this client follows none, so the operator's
            // Nominatim URL names the wrong place.
            let class = if status.is_redirection() {
                "http_3xx"
            } else {
                "http_4xx"
            };
            return Err(anyhow::Error::new(super::ErrorClass(class))
                .context(super::Upstream(status.as_u16())));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| super::RetryLater {
                class: if e.is_timeout() { "timeout" } else { "connect" },
                seconds: 60,
            })?;
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(super::RetryLater {
                    class: "policy",
                    seconds: 60,
                }
                .into());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
    async fn cached(&self, id: &OsmId) -> Result<Option<ResourceMeta>> {
        let value: Option<Value> = sqlx::query_scalar(
            "SELECT fetch_doc FROM osm_place WHERE osm_type=$1 AND osm_id=$2 AND refresh_at>now()",
        )
        .bind(&id.osm_type)
        .bind(id.id)
        .fetch_optional(&self.pool)
        .await?;
        value
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }
    pub(crate) async fn save(
        &self,
        id: &OsmId,
        doc: &ResourceMeta,
        display_name: Option<&str>,
    ) -> Result<String> {
        // A display name is never a link that carries a credential.
        let mut display_name = display_name
            .filter(|s| !crate::capability::names_credential(s))
            .map(str::to_owned);
        if let Some(s) = &mut display_name {
            crate::model::truncate(s, 2048);
        }
        // Nothing cached names a credential but the place's own link (`capability::strip_document`).
        let mut doc = doc.clone();
        crate::capability::strip_document(&mut doc, &id.uri())?;
        let doc = &doc;
        let mut tx = self.pool.begin().await?;
        sqlx::query(r#"INSERT INTO osm_place(osm_type,osm_id,point,bbox,name,category,address,tags,display_name,type,type_path,doc,fetched_at,refresh_at,status,fetch_doc,fetch_display_name)
          VALUES($1,$2,CASE WHEN $3::float8 IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($4,$3),4326) END,$5,$6,$7,$8,$9,$10,$11,$12,$13,to_timestamp($14::float8/1000),to_timestamp($14::float8/1000)+$15*interval '1 second',$16,$13,$10)
          ON CONFLICT(osm_type,osm_id) DO UPDATE SET fetch_doc=excluded.fetch_doc,fetch_display_name=excluded.fetch_display_name,fetched_at=excluded.fetched_at,refresh_at=excluded.refresh_at
          WHERE osm_place.fetched_at<=excluded.fetched_at"#)
          .bind(&id.osm_type).bind(id.id).bind(doc.location.as_ref().map(|p|p.lat)).bind(doc.location.as_ref().map(|p|p.lon)).bind(doc.bbox.map(|b|b.to_vec()))
          .bind(&doc.name).bind(&doc.category).bind(json!(doc.address)).bind(json!(doc.attributes)).bind(display_name).bind(&doc.type_name).bind(&doc.type_path).bind(json!(doc)).bind(doc.fetched_at as f64).bind(doc.ttl_secs as f64).bind(doc.status.as_str()).execute(&mut *tx).await?;
        // Normalize the actually retained fetch, including on a late response. Hold the row lock until its version is registered, so every
        // returned key names facts stored in versions and uses the change writer's hash.
        let retained: Value = sqlx::query_scalar(
            "SELECT fetch_doc FROM osm_place WHERE osm_type=$1 AND osm_id=$2 FOR UPDATE",
        )
        .bind(&id.osm_type)
        .bind(id.id)
        .fetch_one(&mut *tx)
        .await?;
        let normalized = crate::changes::normalize(&serde_json::from_value(retained)?);
        let version: String = sqlx::query_scalar("UPDATE osm_place SET versions=versions || jsonb_build_object(md5($3::jsonb::text),$3::jsonb) WHERE osm_type=$1 AND osm_id=$2 RETURNING md5($3::jsonb::text)")
            .bind(&id.osm_type).bind(id.id).bind(normalized).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn batch(&self, uris: &[String], probe: bool) -> Vec<Result<ResourceMeta>> {
        if uris.len() > 50 {
            return uris
                .iter()
                .map(|_| Err(anyhow::anyhow!("OSM batch exceeds 50 objects")))
                .collect();
        }
        let ids = uris.iter().map(|u| parse_id(u)).collect::<Vec<_>>();
        let mut found: HashMap<OsmId, Result<ResourceMeta>> = HashMap::new();
        let mut pending = Vec::new();
        for id in ids.iter().flatten() {
            if found.contains_key(id) || pending.contains(id) {
                continue;
            }
            match if probe {
                Ok(None)
            } else {
                self.cached(id).await
            } {
                Ok(Some(doc)) => {
                    found.insert(id.clone(), Ok(doc));
                }
                Ok(None) => pending.push(id.clone()),
                Err(e) => {
                    found.insert(id.clone(), Err(e));
                }
            }
        }
        if !pending.is_empty() {
            let response = async {
                let mut url: reqwest::Url =
                    format!("{}/lookup", self.config.nominatim_url.trim_end_matches('/'))
                        .parse()?;
                url.query_pairs_mut()
                    .append_pair(
                        "osm_ids",
                        &pending
                            .iter()
                            .map(OsmId::lookup_id)
                            .collect::<Vec<_>>()
                            .join(","),
                    )
                    .append_pair("format", "jsonv2")
                    .append_pair("addressdetails", "1")
                    .append_pair("extratags", "1")
                    .append_pair("namedetails", "1");
                let body = self
                    .request(url)
                    .await?
                    .context("Nominatim lookup endpoint missing")?;
                body.as_array()
                    .cloned()
                    .context("Nominatim response is not a list")
            }
            .await;
            match response {
                Err(error) => {
                    for id in pending {
                        found.insert(id, Err(copy_error(&error)));
                    }
                }
                Ok(rows) => {
                    if rows.is_empty() {
                        crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "missing");
                    }
                    let mut places = HashMap::new();
                    for row in rows {
                        let id = row["osm_type"]
                            .as_str()
                            .zip(row["osm_id"].as_i64())
                            .and_then(|(kind, id)| {
                                parse_id(&format!("https://www.openstreetmap.org/{kind}/{id}"))
                            });
                        // Unattributable/unrequested rows cannot poison otherwise valid members.
                        let Some(id) = id.filter(|id| pending.contains(id)) else {
                            continue;
                        };
                        places.entry(id).or_insert_with(|| {
                            map_place(&row, self.config.ttl_secs)
                                .map(|doc| (doc, text(&row["display_name"]).map(str::to_owned)))
                        });
                    }
                    for id in pending {
                        let result = async {
                            let (doc, display) = if let Some(place) = places.remove(&id) {
                                place?
                            } else {
                                (missing(&id, self.config.ttl_secs), None)
                            };
                            crate::telemetry::document(
                                crate::telemetry::last_fetch(),
                                &self.manifest(),
                                &doc,
                            );
                            if !probe {
                                self.save(&id, &doc, display.as_deref()).await?;
                            }
                            Ok(doc)
                        }
                        .await;
                        found.insert(id, result);
                    }
                }
            }
        }
        ids.iter()
            .map(|id| {
                id.as_ref()
                    .and_then(|id| found.get(id))
                    .map(|result| result.as_ref().cloned().map_err(copy_error))
                    .unwrap_or_else(|| Err(anyhow::anyhow!("invalid OSM object URI")))
            })
            .collect()
    }
}
#[async_trait]
impl Resolver for OsmResolver {
    fn manifest(&self) -> crate::adapter::Manifest {
        let mut manifest = crate::adapter::manifest("osm");
        manifest.cadence_floor = self.config.ttl_secs.max(3600);
        manifest
    }
    fn name(&self) -> &'static str {
        "osm"
    }
    fn matches(&self, uri: &str) -> bool {
        parse_id(uri).is_some()
    }
    fn batch_size(&self) -> usize {
        50
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta> {
        self.batch(&[uri.into()], false)
            .await
            .pop()
            .context("empty OSM batch")?
    }
    async fn resolve_batch(&self, uris: &[String]) -> Vec<Result<ResourceMeta>> {
        self.batch(uris, false).await
    }
    async fn probe(&self, uri: &str) -> Result<ResourceMeta> {
        self.batch(&[uri.into()], true)
            .await
            .pop()
            .context("empty OSM probe")?
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(self.config.ttl_secs)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn object_aliases_preserve_resource_identity() {
        for uri in [
            "https://www.openstreetmap.org/node/1",
            "http://www.openstreetmap.org/node/1",
            "https://openstreetmap.org/node/1",
            "https://osm.org/node/1",
            "https://www.openstreetmap.org/node/1/",
            "https://www.openstreetmap.org/node/1?x=1",
            "https://www.openstreetmap.org./node/1#map=1",
        ] {
            assert_eq!(parse_id(uri).unwrap().key(), "node/1");
            assert_eq!(super::super::classify(uri), Some("osm"));
        }
        assert_ne!(
            crate::identity::resource_id("https://osm.org/node/1"),
            crate::identity::resource_id("https://www.openstreetmap.org/node/1")
        );
        for uri in [
            "https://www.openstreetmap.org/go/abc",
            "https://www.openstreetmap.org/?mlat=1",
            "https://www.openstreetmap.org/node/0",
            "https://www.openstreetmap.org/node/-1",
            "https://www.openstreetmap.org/node/01",
            "https://www.openstreetmap.org/node/9223372036854775808",
            "https://wiki.openstreetmap.org/node/1",
            "https://evilopenstreetmap.org/node/1",
            "https://www.openstreetmap.org/node/1//",
        ] {
            assert!(parse_id(uri).is_none(), "{uri}");
        }
        // Object aliases name the same object and route to the OSM resolver.
        for (uri, key) in [
            ("https://www.openstreetmap.org/node/240109189/history", "node/240109189"),
            ("https://osm.org/way/50637691/history/12", "way/50637691"),
            ("https://www.openstreetmap.org/relation/7/history/", "relation/7"),
            ("https://www.openstreetmap.org/edit?node=240109189", "node/240109189"),
            ("https://openstreetmap.org/edit?editor=id&way=50637691", "way/50637691"),
            ("https://api.openstreetmap.org/api/0.6/node/240109189", "node/240109189"),
            ("https://api.openstreetmap.org/api/0.6/node/240109189.json", "node/240109189"),
            ("https://api.openstreetmap.org/api/0.6/way/50637691/full", "way/50637691"),
            ("https://api.openstreetmap.org/api/0.6/relation/7/full.xml", "relation/7"),
            ("https://nominatim.openstreetmap.org/ui/details.html?osmtype=N&osmid=240109189", "node/240109189"),
            ("https://nominatim.openstreetmap.org/details?osmtype=W&osmid=50637691&class=tourism", "way/50637691"),
            // osm.org's Share → Short link on an object page.
            ("https://osm.org/go/0EEQjE--?way=50637691", "way/50637691"),
            ("https://www.openstreetmap.org/go/0GAjIv8h?node=240109189&m", "node/240109189"),
            ("https://openstreetmap.org/go/D~hV--?relation=7", "relation/7"),
        ] {
            assert_eq!(parse_id(uri).map(|id| id.key()).as_deref(), Some(key), "{uri}");
            assert_eq!(super::super::classify(uri), Some("osm"), "{uri}");
        }
        for uri in [
            "https://www.openstreetmap.org/node/1/history/0",
            "https://www.openstreetmap.org/node/1/history/2/x",
            "https://www.openstreetmap.org/node/1/changesets",
            "https://www.openstreetmap.org/edit",
            "https://www.openstreetmap.org/edit?node=1&way=2",
            "https://www.openstreetmap.org/edit?node=01",
            "https://www.openstreetmap.org/history",
            "https://api.openstreetmap.org/api/0.6/node/1/full",
            "https://api.openstreetmap.org/api/0.6/node/1/ways",
            "https://api.openstreetmap.org/api/0.6/changeset/1",
            "https://api.openstreetmap.org/api/0.5/node/1",
            "https://nominatim.openstreetmap.org/ui/details.html?osmtype=X&osmid=1",
            "https://nominatim.openstreetmap.org/ui/details.html?osmtype=N&osmid=0",
            "https://nominatim.openstreetmap.org/search?osmtype=N&osmid=1",
            // Upstream accepts N, W and R only, and place_id/postcode take precedence.
            "https://nominatim.openstreetmap.org/details?osmtype=w&osmid=50637691",
            "https://nominatim.openstreetmap.org/details?osmtype=N&osmid=1&osmid=2",
            "https://nominatim.openstreetmap.org/details?osmtype=N&osmtype=W&osmid=1",
            "https://nominatim.openstreetmap.org/details?place_id=9&osmtype=N&osmid=1",
            "https://nominatim.openstreetmap.org/details?postcode=6900&osmtype=N&osmid=1",
            "https://osm.org/go/0EEQjE--",
            "https://osm.org/go/0EEQjE--?way=1&node=2",
            "https://osm.org/go/0EEQjE--?changeset=1",
            "https://osm.org/go/!!?way=1",
            "https://osm.org/go/0EEQjE--/x?way=1",
            "https://user@www.openstreetmap.org/node/1/history",
            "https://www.openstreetmap.org:8443/edit?node=1",
            "https://api.openstreetmap.org:443x/api/0.6/node/1",
        ] {
            assert!(parse_id(uri).is_none(), "{uri}");
        }
        for url in [
            "https://NOMINATIM.OPENSTREETMAP.ORG./",
            "https://www.openstreetmap.org",
            "https://api.openstreetmap.org",
            "https://93.184.216.34",
            "https://[2606:4700:4700::1111]",
        ] {
            assert!(public_upstream(&url.parse().unwrap()), "{url}");
        }
        assert!(!public_upstream(&"http://127.0.0.1:1234".parse().unwrap()));
    }
    #[test]
    fn mappings_keep_multiple_inheritance() {
        for (category, kind) in [
            ("amenity/cafe", "CafeOrCoffeeShop"),
            ("shop/unknown", "Store"),
            ("natural/peak", "Mountain"),
            ("unknown/foo", "Place"),
            ("public_transport/station", "BusStation"),
        ] {
            assert_eq!(schema_type(category), kind);
            assert!(crate::model::type_path(kind).contains(&"Place".to_owned()));
        }
        let all: Value =
            serde_json::from_str(include_str!("../../../../data/type-mappings.json")).unwrap();
        for kind in all["osm"].as_object().unwrap().values() {
            assert!(!crate::model::type_path(kind.as_str().unwrap()).is_empty());
        }
    }
    #[sqlx::test(migrations = "../../migrations")]
    async fn saved_version_is_the_key_even_with_diagnostic_fields(pool: PgPool) {
        let resolver = OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap();
        let id = parse_id("https://www.openstreetmap.org/node/123").unwrap();
        let mut doc = ResourceMeta::new(&id.uri(), "osm", 3600);
        doc.set_type("Place");
        doc.osm_id = Some(id.key());
        doc.aliases = vec![
            "https://example.org/z".into(),
            "https://example.org/a".into(),
            "https://example.org/z".into(),
        ];
        doc.attributes.insert("content_length".into(), "42".into());
        doc.event.warnings.push("fixture_warning".into());
        let version = resolver.save(&id, &doc, None).await.unwrap();
        let saved: Value =
            sqlx::query_scalar("SELECT versions->$1 FROM osm_place WHERE osm_id=123")
                .bind(&version)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(saved, crate::changes::normalize(&doc));
        let expected: String = sqlx::query_scalar("SELECT md5($1::jsonb::text)")
            .bind(crate::changes::normalize(&doc))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, expected);
        let resource = crate::store::ensure_resource(&pool, &id.uri())
            .await
            .unwrap();
        crate::store::save_document(&pool, &resource, 0, &doc)
            .await
            .unwrap();
        let resource_version: String =
            sqlx::query_scalar("SELECT osm_version FROM resource WHERE id=$1")
                .bind(resource)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(resource_version, version);
        assert!(saved.get("fetched_at").is_none());
        doc.fetched_at -= 1000;
        doc.name = Some("Stale".into());
        assert_eq!(resolver.save(&id, &doc, None).await.unwrap(), version);
    }
    /// The place cache stores no tag or document field that carries a credential.
    #[sqlx::test(migrations = "../../migrations")]
    async fn the_place_cache_stores_no_credential(pool: PgPool) {
        let resolver = OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap();
        let id = parse_id("https://www.openstreetmap.org/node/124").unwrap();
        let mut doc = ResourceMeta::new(&id.uri(), "osm", 3600);
        doc.set_type("Place");
        doc.osm_id = Some(id.key());
        doc.name = Some("Shop".into());
        doc.attributes
            .insert("website".into(), "https://shop.example/?key=SECRET".into());
        doc.attributes
            .insert("phone".into(), "+41 00 000 00 00".into());
        resolver.save(&id, &doc, None).await.unwrap();
        let stored: String = sqlx::query_scalar(
            "SELECT concat_ws(' ',tags::text,doc::text,fetch_doc::text,versions::text) FROM osm_place WHERE osm_id=124",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!stored.contains("SECRET"), "{stored}");
        assert!(stored.contains("+41 00 000 00 00"), "{stored}");
    }
    /// A place's display name that is a link carrying a credential is not cached; a name is.
    #[sqlx::test(migrations = "../../migrations")]
    async fn the_place_cache_stores_no_credential_display_name(pool: PgPool) {
        let resolver = OsmResolver::new(pool.clone(), OsmConfig::default()).unwrap();
        for (osm_id, display_name) in [
            (125, "https://shop.example/?token=SECRET"),
            (126, "Shop, Main Street"),
        ] {
            let id = parse_id(&format!("https://www.openstreetmap.org/node/{osm_id}")).unwrap();
            let mut doc = ResourceMeta::new(&id.uri(), "osm", 3600);
            doc.set_type("Place");
            doc.osm_id = Some(id.key());
            doc.name = Some("Shop".into());
            resolver.save(&id, &doc, Some(display_name)).await.unwrap();
        }
        let stored: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT osm_id,display_name,fetch_display_name FROM osm_place WHERE osm_id IN(125,126) ORDER BY osm_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            stored,
            vec![
                (125, None, None),
                (
                    126,
                    Some("Shop, Main Street".into()),
                    Some("Shop, Main Street".into())
                )
            ]
        );
    }
}
