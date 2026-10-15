use super::Resolver;
use crate::{
    config::OsmConfig,
    model::{LocationSource, OsmId, Point, ResourceMeta, Status},
};
use anyhow::{bail, ensure, Context, Result};
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
pub fn parse_id(uri: &str) -> Option<OsmId> {
    let path = uri.strip_prefix("https://www.openstreetmap.org/")?;
    let (osm_type, digits) = path.split_once('/')?;
    if !matches!(osm_type, "node" | "way" | "relation")
        || digits.is_empty()
        || digits.starts_with('0')
        || !digits.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let id = digits.parse::<i64>().ok().filter(|id| *id > 0)?;
    Some(OsmId {
        osm_type: osm_type.into(),
        id,
    })
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
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!(
                "pinakes/{} (+https://github.com/pubky/pinakes)",
                env!("CARGO_PKG_VERSION")
            ))
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
    async fn request(&self, url: reqwest::Url) -> Result<Option<Value>> {
        // One lock covers the request too: all batches and fallback calls share the host budget.
        let mut next = self.next.lock().await;
        tokio::time::sleep_until(*next).await;
        let public = url
            .host_str()
            .is_some_and(|s| s == "nominatim.openstreetmap.org" || s == "api.openstreetmap.org");
        let rate = if public {
            self.config.requests_per_second.min(1.0)
        } else {
            self.config.requests_per_second
        };
        *next = Instant::now() + Duration::from_secs_f64(1.0 / rate);
        let response = self.client.get(url.clone()).send().await?;
        let status = response.status();
        tracing::info!(
            source = "osm",
            path = url.path(),
            status = status.as_u16(),
            "OSM upstream request"
        );
        if status.as_u16() == 429 {
            let seconds = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|s| s.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60)
                .clamp(1, 86400);
            *next = Instant::now() + Duration::from_secs(seconds);
            bail!("OSM upstream rate_limited (429)");
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Ok(None);
        }
        ensure!(status.is_success(), "OSM upstream HTTP {status}");
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            ensure!(
                bytes.len() + chunk.len() <= 2 * 1024 * 1024,
                "OSM response exceeds 2 MB"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
    async fn cached(&self, id: &OsmId) -> Result<Option<ResourceMeta>> {
        let value: Option<Value> = sqlx::query_scalar(
            "SELECT doc FROM osm_place WHERE osm_type=$1 AND osm_id=$2 AND refresh_at>now()",
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
    async fn save(&self, id: &OsmId, doc: &ResourceMeta, display_name: Option<&str>) -> Result<()> {
        let mut display_name = display_name.map(str::to_owned);
        if let Some(s) = &mut display_name {
            crate::model::truncate(s, 2048);
        }
        sqlx::query(r#"INSERT INTO osm_place(osm_type,osm_id,point,bbox,name,category,address,tags,display_name,type,type_path,doc,fetched_at,refresh_at,status)
          VALUES($1,$2,CASE WHEN $3::float8 IS NULL THEN NULL ELSE ST_SetSRID(ST_MakePoint($4,$3),4326) END,$5,$6,$7,$8,$9,$10,$11,$12,$13,to_timestamp($14::float8/1000),to_timestamp($14::float8/1000)+$15*interval '1 second',$16)
          ON CONFLICT(osm_type,osm_id) DO UPDATE SET point=excluded.point,bbox=excluded.bbox,name=excluded.name,category=excluded.category,address=excluded.address,tags=excluded.tags,display_name=excluded.display_name,type=excluded.type,type_path=excluded.type_path,doc=excluded.doc,fetched_at=excluded.fetched_at,refresh_at=excluded.refresh_at,status=excluded.status
          WHERE osm_place.fetched_at<=excluded.fetched_at"#)
          .bind(&id.osm_type).bind(id.id).bind(doc.location.as_ref().map(|p|p.lat)).bind(doc.location.as_ref().map(|p|p.lon)).bind(doc.bbox.map(|b|b.to_vec()))
          .bind(&doc.name).bind(&doc.category).bind(json!(doc.address)).bind(json!(doc.attributes)).bind(display_name).bind(&doc.type_name).bind(&doc.type_path).bind(json!(doc)).bind(doc.fetched_at as f64).bind(doc.ttl_secs as f64).bind(doc.status.as_str()).execute(&self.pool).await?;
        Ok(())
    }
    async fn fallback(&self, id: &OsmId) -> Result<Option<Value>> {
        let suffix = if id.osm_type == "node" {
            ".json"
        } else {
            "/full.json"
        };
        let url = format!(
            "{}/api/0.6/{}{suffix}",
            self.config.api_url.trim_end_matches('/'),
            id.key()
        )
        .parse()?;
        let Some(body) = self.request(url).await? else {
            return Ok(None);
        };
        let elements = body["elements"]
            .as_array()
            .context("missing OSM elements")?;
        let object = elements
            .iter()
            .find(|e| e["type"] == id.osm_type && e["id"] == id.id)
            .context("missing OSM element")?;
        if object["visible"] == false {
            return Ok(None);
        }
        let nodes: Vec<_> = if id.osm_type == "node" {
            vec![object]
        } else {
            elements.iter().filter(|e| e["type"] == "node").collect()
        };
        ensure!(
            !nodes.is_empty(),
            "OSM full response has no member coordinates"
        );
        let points = nodes
            .iter()
            .map(|n| {
                Ok(Point {
                    lat: decimal(&n["lat"])?,
                    lon: decimal(&n["lon"])?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let lat = points.iter().map(|p| p.lat).sum::<f64>() / points.len() as f64;
        let lon = points
            .iter()
            .map(|p| p.lon.to_radians().sin())
            .sum::<f64>()
            .atan2(points.iter().map(|p| p.lon.to_radians().cos()).sum())
            .to_degrees();
        let tags = &object["tags"];
        let category = [
            "amenity",
            "tourism",
            "shop",
            "leisure",
            "natural",
            "place",
            "boundary",
            "railway",
            "public_transport",
        ]
        .into_iter()
        .find(|c| text(&tags[*c]).is_some())
        .unwrap_or("place");
        let mut address = serde_json::Map::new();
        for (from, to) in [
            ("addr:street", "road"),
            ("addr:housenumber", "house_number"),
            ("addr:postcode", "postcode"),
            ("addr:city", "city"),
            ("addr:country", "country_code"),
        ] {
            if let Some(s) = text(&tags[from]) {
                address.insert(to.into(), json!(s));
            }
        }
        Ok(Some(
            json!({"osm_type":id.osm_type,"osm_id":id.id,"lat":lat,"lon":lon,"category":category,"type":text(&tags[category]).unwrap_or("unknown"),"namedetails":tags,"display_name":text(&tags["name"]).unwrap_or_default(),"address":address,"extratags":tags,"boundingbox":[points.iter().map(|p|p.lat).fold(f64::INFINITY,f64::min),points.iter().map(|p|p.lat).fold(f64::NEG_INFINITY,f64::max),points.iter().map(|p|p.lon).fold(f64::INFINITY,f64::min),points.iter().map(|p|p.lon).fold(f64::NEG_INFINITY,f64::max)]}),
        ))
    }
    async fn batch(&self, uris: &[String]) -> Result<Vec<ResourceMeta>> {
        ensure!(uris.len() <= 50, "OSM batch exceeds 50 objects");
        let ids = uris
            .iter()
            .map(|u| parse_id(u).context("noncanonical OSM URI"))
            .collect::<Result<Vec<_>>>()?;
        let mut found = HashMap::new();
        let mut pending = Vec::new();
        for id in &ids {
            if found.contains_key(id) {
                continue;
            }
            if let Some(doc) = self.cached(id).await? {
                found.insert(id.clone(), doc);
            } else if !pending.contains(id) {
                pending.push(id.clone());
            }
        }
        if !pending.is_empty() {
            let mut url: reqwest::Url =
                format!("{}/lookup", self.config.nominatim_url.trim_end_matches('/')).parse()?;
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
            let rows = body
                .as_array()
                .context("Nominatim response is not a list")?;
            let mut places = HashMap::new();
            for row in rows {
                let doc = map_place(row, self.config.ttl_secs)?;
                let id = parse_id(&doc.url).unwrap();
                ensure!(
                    pending.contains(&id),
                    "Nominatim returned an unrequested object"
                );
                places
                    .entry(id)
                    .or_insert((doc, text(&row["display_name"]).map(str::to_owned)));
            }
            for id in pending {
                let (doc, display) = if let Some(place) = places.remove(&id) {
                    place
                } else if self.config.fallback_enabled {
                    match self.fallback(&id).await? {
                        Some(row) => (
                            map_place(&row, self.config.ttl_secs)?,
                            text(&row["display_name"]).map(str::to_owned),
                        ),
                        None => (missing(&id, self.config.ttl_secs), None),
                    }
                } else {
                    (missing(&id, self.config.ttl_secs), None)
                };
                self.save(&id, &doc, display.as_deref()).await?;
                found.insert(id, doc);
            }
        }
        ids.iter()
            .map(|id| found.get(id).cloned().context("missing OSM URI in batch"))
            .collect()
    }
}
#[async_trait]
impl Resolver for OsmResolver {
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
        self.batch(&[uri.into()])
            .await?
            .pop()
            .context("empty OSM batch")
    }
    async fn resolve_batch(&self, uris: &[String]) -> Vec<Result<ResourceMeta>> {
        match self.batch(uris).await {
            Ok(docs) => docs.into_iter().map(Ok).collect(),
            Err(e) => uris.iter().map(|_| Err(anyhow::anyhow!("{e:#}"))).collect(),
        }
    }
    fn refresh_after(&self, _: &ResourceMeta) -> Duration {
        Duration::from_secs(self.config.ttl_secs)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_objects_only() {
        for kind in ["node", "way", "relation"] {
            assert!(parse_id(&format!("https://www.openstreetmap.org/{kind}/123")).is_some());
        }
        for url in [
            "http://www.openstreetmap.org/node/1",
            "https://openstreetmap.org/node/1",
            "https://osm.org/node/1",
            "https://www.openstreetmap.org/node/1/",
            "https://www.openstreetmap.org/node/1?x=1",
            "https://www.openstreetmap.org/node/1#map=1",
            "https://www.openstreetmap.org/go/abc",
            "https://www.openstreetmap.org/?mlat=1",
            "https://www.openstreetmap.org/node/0",
            "https://www.openstreetmap.org/node/-1",
            "https://www.openstreetmap.org/node/01",
            "https://www.openstreetmap.org/node/9223372036854775808",
        ] {
            assert!(parse_id(url).is_none(), "{url}");
            assert_eq!(super::super::classify(url), Some("web"));
        }
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
}
