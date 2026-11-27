//! The place rules a read applies with the OSM catalogue on, from what is stored (nothing is
//! fetched or written):
//!
//! - A map link (Google Maps, Apple Maps, Organic Maps, `geo:`) whose name and point come
//!   from the link itself is "maybe the same" as the one imported place with the same name within
//!   50 m; with none or several, it stays a point. The place lists such links too.
//! - A deleted OSM object is maybe the same as the one imported place of its name and category
//!   within 50 m of where it was (an exact successor makes its link an alias instead:
//!   `osm::follow`).
//! - A place whose name changed to an unrelated one says which places it was (`osm_rename`).
//!
//! Names are compared folded ([`place::folded`]): case, accents, spaces and punctuation aside.
use super::{place, NEAR_METRES};
use anyhow::Result;
use sqlx::PgPool;

/// The most imported places or links one rule reads around a point.
const AROUND: i64 = 64;
/// The most map links near a place a read checks (each with one more query).
pub const LINKS_CHECKED: usize = 8;

/// Whether the OSM catalogue holds any card (its state row's count).
pub async fn imported(pool: &PgPool) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM catalogue_state WHERE catalogue='osm' AND cards>0)",
    )
    .fetch_one(pool)
    .await?)
}

/// An imported place near a point.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Near {
    pub source_id: String,
    pub uri: String,
    pub name: String,
    pub other_names: Vec<String>,
    pub category: Option<String>,
}
impl Near {
    /// Whether one of its names is `name`, folded.
    pub fn named(&self, name: &str) -> bool {
        let name = place::folded(name);
        !name.is_empty()
            && std::iter::once(&self.name)
                .chain(&self.other_names)
                .any(|n| place::folded(n) == name)
    }
}

/// The imported places within 50 m of a point, nearest first.
pub async fn places_near(pool: &PgPool, lat: f64, lon: f64) -> Result<Vec<Near>> {
    Ok(sqlx::query_as(
        "SELECT source_id,uri,name,other_names,details->>'category' AS category FROM catalogue_card \
         WHERE catalogue='osm' AND ST_DWithin(point::geography,ST_SetSRID(ST_MakePoint($2,$1),4326)::geography,$3) \
         ORDER BY point::geography <-> ST_SetSRID(ST_MakePoint($2,$1),4326)::geography LIMIT $4",
    )
    .bind(lat)
    .bind(lon)
    .bind(NEAR_METRES)
    .bind(AROUND)
    .fetch_all(pool)
    .await?)
}

/// The one imported place near a point with `name`, if exactly one has it.
pub fn the_one<'a>(places: &'a [Near], name: &str) -> Option<&'a Near> {
    let mut named = places.iter().filter(|p| p.named(name));
    let one = named.next()?;
    named.next().is_none().then_some(one)
}

/// A map link as the rules read it: a stored card whose name and point come from its link.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct MapLink {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub site: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// Whether people referenced it.
    pub referenced: bool,
}
/// The sources whose cards are read from a map link itself.
pub const MAP_LINK_SOURCES: &[&str] = &["geo", "google_maps", "apple_maps"];
/// Whether a name is a place's name and not a point's coordinates.
pub fn place_name(name: &str) -> bool {
    name.chars().any(char::is_alphabetic)
}

/// The map links within 50 m of a point, the referenced first (a link nobody referenced can't
/// crowd them out), then the nearest.
pub async fn links_near(pool: &PgPool, lat: f64, lon: f64) -> Result<Vec<MapLink>> {
    let links: Vec<MapLink> = sqlx::query_as(
        "SELECT r.id,r.uri,r.name,r.doc->>'site' AS site,ST_Y(r.point) AS lat,ST_X(r.point) AS lon,\
         EXISTS(SELECT 1 FROM reference f WHERE f.resource_id=r.id) AS referenced FROM resource r \
         WHERE r.point IS NOT NULL AND r.name IS NOT NULL AND r.status='ok' AND r.doc->>'location_source'='uri' AND r.doc->>'source'=ANY($4) \
         AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) \
         AND ST_DWithin(r.point::geography,ST_SetSRID(ST_MakePoint($2,$1),4326)::geography,$3) \
         ORDER BY EXISTS(SELECT 1 FROM reference f WHERE f.resource_id=r.id) DESC,r.point::geography <-> ST_SetSRID(ST_MakePoint($2,$1),4326)::geography,r.id LIMIT $5",
    )
    .bind(lat)
    .bind(lon)
    .bind(NEAR_METRES)
    .bind(MAP_LINK_SOURCES)
    .bind(AROUND)
    .fetch_all(pool)
    .await?;
    Ok(links.into_iter().filter(|l| place_name(&l.name)).collect())
}

/// The links among `ids` that carry an exact place key (a Wikidata item's, an OSM object's):
/// joined exactly, such a link is never "maybe the same" as a place.
pub async fn exactly_placed(
    pool: &PgPool,
    ids: &[String],
) -> Result<std::collections::HashSet<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT resource_id FROM resource_key WHERE resource_id=ANY($1) AND (key LIKE 'wikidata:%' OR key LIKE 'osm:%')",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

/// What a stored resource is to the rules.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Stored {
    pub osm_type: Option<String>,
    pub osm_id: Option<i64>,
    pub source: Option<String>,
    pub location_source: Option<String>,
    pub status: String,
}
pub async fn stored(pool: &PgPool, id: &str) -> Result<Option<Stored>> {
    Ok(sqlx::query_as(
        "SELECT osm_type,osm_id,doc->>'source' AS source,doc->>'location_source' AS location_source,status FROM resource WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Where a deleted object was: its name, category and point as its card was when an update
/// removed it (`osm_removed`); for an object removed otherwise, its place row's, with the last
/// unrelated name an update saw.
pub async fn last_place(
    pool: &PgPool,
    osm_type: &str,
    osm_id: i64,
) -> Result<Option<(String, String, f64, f64)>> {
    // Its last card as the update that removed it saw it, else what a read of it last stored.
    Ok(sqlx::query_as(
        "SELECT name,category,lat,lon FROM (\
           SELECT 0 AS rank,name,category,ST_Y(point) AS lat,ST_X(point) AS lon FROM osm_removed WHERE osm_type=$1 AND osm_id=$2 AND category IS NOT NULL \
           UNION ALL SELECT 1,coalesce((SELECT r.new_name FROM osm_rename r WHERE r.osm_type=o.osm_type AND r.osm_id=o.osm_id ORDER BY r.changed_at DESC LIMIT 1),o.name),o.category,ST_Y(o.point),ST_X(o.point) \
           FROM osm_place o WHERE o.osm_type=$1 AND o.osm_id=$2 AND o.name IS NOT NULL AND o.category IS NOT NULL AND o.point IS NOT NULL) x ORDER BY rank LIMIT 1",
    )
    .bind(osm_type)
    .bind(osm_id)
    .fetch_optional(pool)
    .await?)
}

/// An earlier, unrelated name of a place, and when it changed.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Renamed {
    pub old_name: String,
    pub old_category: Option<String>,
    pub changed_at: chrono::DateTime<chrono::Utc>,
}
pub async fn renames(pool: &PgPool, osm_type: &str, osm_id: i64) -> Result<Vec<Renamed>> {
    Ok(sqlx::query_as(
        "SELECT old_name,old_category,changed_at FROM osm_rename WHERE osm_type=$1 AND osm_id=$2 ORDER BY changed_at LIMIT 16",
    )
    .bind(osm_type)
    .bind(osm_id)
    .fetch_all(pool)
    .await?)
}

/// How many references a resource got before an instant.
pub async fn references_before(
    pool: &PgPool,
    id: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM reference f WHERE (f.resource_id=$1 OR f.resource_id IN (SELECT alias_id FROM resource_alias WHERE root_id=$1)) AND f.created_at<$2",
    )
    .bind(id)
    .bind(at)
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(name: &str, others: &[&str]) -> Near {
        Near {
            source_id: format!("node/{}", name.len()),
            uri: String::new(),
            name: name.into(),
            other_names: others.iter().map(|s| (*s).to_owned()).collect(),
            category: None,
        }
    }

    #[test]
    fn exactly_one_place_with_the_name() {
        let places = vec![
            near("Café Engel", &[]),
            near("Engel", &["Gasthaus Engel"]),
            near("Löwen", &[]),
        ];
        assert_eq!(
            the_one(&places, "cafe engel").map(|p| p.name.as_str()),
            Some("Café Engel")
        );
        assert_eq!(
            the_one(&places, "Gasthaus Engel").map(|p| p.name.as_str()),
            Some("Engel")
        );
        assert!(the_one(&places, "Adler").is_none());
        let twice = vec![near("Engel", &[]), near("ENGEL", &[])];
        assert!(the_one(&twice, "Engel").is_none());
        assert!(the_one(&places, "").is_none());
        assert!(!place_name("47.14, 9.52"));
        assert!(place_name("Engel"));
    }
}
