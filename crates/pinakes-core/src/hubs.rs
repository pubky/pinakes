//! Exact hub lookups for "also on": MusicBrainz and Wikidata.
//!
//! A hub answers one exact question (this URL, this MBID, this Q-id, this external ID) and never
//! a fuzzy one: no title or name matching. Each lookup is a pure parser over a recorded or live
//! response plus a thin fetch through [`Egress`], so the per-host budgets (MusicBrainz 1/s,
//! Wikimedia 200/min) and the egress policy apply. A missing object is `Ok(None)`; a busy or
//! rate-limited hub is a [`RetryLater`] error, so the caller keeps its previous rows.
//!
//! The join keys a source records come from [`key`]: `mb:recording:{mbid}`,
//! `mb:release:{mbid}`, `mb:release-group:{mbid}`, `mb:artist:{mbid}` and `wikidata:{Q-id}`.
use crate::resolver::{
    egress::{Egress, FetchOptions, Fetched},
    RetryLater,
};
use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// The level of a MusicBrainz entity. A key never crosses levels: a recording joins recordings,
/// a release group joins release groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MbKind {
    Recording,
    Release,
    ReleaseGroup,
    Artist,
}
impl MbKind {
    /// The entity's name in the MusicBrainz API and in its key namespace.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Release => "release",
            Self::ReleaseGroup => "release-group",
            Self::Artist => "artist",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "recording" => Self::Recording,
            "release" => Self::Release,
            "release-group" | "release_group" => Self::ReleaseGroup,
            "artist" => Self::Artist,
            _ => return None,
        })
    }
}

/// Join keys, built only from validated identifiers.
pub mod key {
    use super::MbKind;
    /// `mb:{kind}:{mbid}` for a canonical (lower-case, hyphenated) MBID.
    pub fn mb(kind: MbKind, mbid: &str) -> Option<String> {
        super::mbid(mbid).map(|m| format!("mb:{}:{m}", kind.as_str()))
    }
    /// `wikidata:{Q-id}` for an item ID (`Q` and digits, no leading zero).
    pub fn wikidata(qid: &str) -> Option<String> {
        super::qid(qid).map(|q| format!("wikidata:{q}"))
    }
}

/// A canonical MBID: 36 bytes, lower-case hex in 8-4-4-4-12 groups. Upper case is folded; any
/// other spelling is refused.
pub fn mbid(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    let groups: Vec<&str> = s.split('-').collect();
    let shape = groups.iter().map(|g| g.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12];
    (shape
        && groups
            .iter()
            .all(|g| g.bytes().all(|b| b.is_ascii_hexdigit())))
    .then_some(s)
}
/// A Wikidata item ID: `Q` and 1–12 digits without a leading zero.
pub fn qid(s: &str) -> Option<String> {
    let digits = s.strip_prefix('Q')?;
    ((1..=12).contains(&digits.len())
        && !digits.starts_with('0')
        && digits.bytes().all(|b| b.is_ascii_digit()))
    .then(|| s.to_owned())
}

/// A hub's answer as JSON: `None` for 404, a retry for 429/503 (and other 5xx), an error for
/// anything else that is not 200 JSON.
fn json(fetched: &Fetched, hub: &'static str) -> Result<Option<Value>> {
    match fetched.status {
        200 => Ok(Some(serde_json::from_slice(&fetched.body)?)),
        404 => Ok(None),
        429 | 500..=599 => Err(RetryLater {
            class: if hub == "musicbrainz" {
                "musicbrainz_busy"
            } else {
                "wikidata_busy"
            },
            seconds: fetched.retry_after_secs.clamp(60, 3600),
        }
        .into()),
        status => bail!("{hub} answered {status}"),
    }
}
async fn get(egress: &Egress, url: &str, hub: &'static str) -> Result<Option<Value>> {
    let options = FetchOptions {
        accept: Some("application/json".into()),
        ..Default::default()
    };
    // Shared flights and a short cache: two sources asking the same thing ask once.
    let fetched = egress.discover(url, &options, 3600).await?;
    json(&fetched, hub)
}

pub mod musicbrainz {
    //! MusicBrainz `ws/2`: what a URL is linked to, and an entity's URL relations, ISRCs and
    //! barcode.
    use super::*;

    pub const BASE: &str = "https://musicbrainz.org/ws/2";

    /// One URL relation of an entity: the relation type (`free streaming`, `streaming`,
    /// `purchase for download`, `discogs`, …) and the URL.
    #[derive(Clone, Debug, PartialEq)]
    pub struct UrlRelation {
        pub kind: String,
        pub url: String,
    }
    /// A MusicBrainz entity as a source needs it.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Entity {
        pub kind: MbKind,
        pub mbid: String,
        pub title: Option<String>,
        /// Current URL relations (ended ones are left out), in the hub's order.
        pub urls: Vec<UrlRelation>,
        /// Recordings: their ISRCs (an attribute, never a key).
        pub isrcs: Vec<String>,
        /// Releases: the barcode (UPC/EAN), an attribute.
        pub barcode: Option<String>,
        /// Releases: their release group.
        pub release_group: Option<String>,
        /// The artist credit as MusicBrainz writes it ("A feat. B").
        pub artist: Option<String>,
        /// Recordings: the length in milliseconds.
        pub length_ms: Option<u64>,
    }
    /// What a URL is linked to: each current target with its level.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct UrlTargets {
        pub targets: Vec<(MbKind, String)>,
    }

    fn relations(v: &Value) -> Vec<UrlRelation> {
        v["relations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["target-type"] == "url" && r["ended"] != true)
            .filter_map(|r| {
                Some(UrlRelation {
                    kind: r["type"].as_str()?.to_owned(),
                    url: r["url"]["resource"].as_str()?.to_owned(),
                })
            })
            .collect()
    }
    /// `GET /url?resource=…&inc=…-rels`: the entities a URL is related to.
    pub fn parse_url(v: &Value) -> UrlTargets {
        let mut targets: Vec<(MbKind, String)> = v["relations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["ended"] != true)
            .filter_map(|r| {
                // The target sits under its type's name (`recording`, `release_group`, …).
                let name = r["target-type"].as_str()?;
                Some((MbKind::parse(name)?, mbid(r[name]["id"].as_str()?)?))
            })
            .collect();
        targets.sort();
        targets.dedup();
        UrlTargets { targets }
    }
    /// An artist credit: each name and its join phrase, in order.
    fn credit(v: &Value) -> Option<String> {
        let credit: String = v["artist-credit"]
            .as_array()?
            .iter()
            .map(|c| {
                format!(
                    "{}{}",
                    c["name"].as_str().unwrap_or_default(),
                    c["joinphrase"].as_str().unwrap_or_default()
                )
            })
            .collect();
        let credit = credit.trim();
        (!credit.is_empty()).then(|| credit.to_owned())
    }
    /// `GET /{kind}/{mbid}?inc=url-rels…`.
    pub fn parse_entity(kind: MbKind, v: &Value) -> Option<Entity> {
        let mut isrcs: Vec<String> = v["isrcs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|s| s.len() == 12 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
            .map(str::to_ascii_uppercase)
            .collect();
        isrcs.sort();
        isrcs.dedup();
        Some(Entity {
            kind,
            mbid: mbid(v["id"].as_str()?)?,
            title: v["title"]
                .as_str()
                .or_else(|| v["name"].as_str())
                .map(str::to_owned),
            urls: relations(v),
            isrcs,
            barcode: v["barcode"]
                .as_str()
                .filter(|b| (8..=14).contains(&b.len()) && b.bytes().all(|c| c.is_ascii_digit()))
                .map(str::to_owned),
            release_group: v["release-group"]["id"].as_str().and_then(mbid),
            artist: credit(v),
            length_ms: v["length"]
                .as_u64()
                .filter(|ms| (1..=86_400_000).contains(ms)),
        })
    }
    fn inc(kind: MbKind) -> &'static str {
        match kind {
            MbKind::Recording => "url-rels+isrcs+artist-credits",
            MbKind::Release => "url-rels+release-groups+artist-credits",
            MbKind::ReleaseGroup | MbKind::Artist => "url-rels",
        }
    }
    /// The URL lookup for `link` (exact; MusicBrainz stores URLs as given, so the link is sent
    /// as the card has it).
    pub fn url_lookup(link: &str) -> String {
        let mut url = url::Url::parse(&format!("{BASE}/url")).expect("constant URL");
        url.query_pairs_mut()
            .append_pair("resource", link)
            .append_pair(
                "inc",
                "recording-rels+release-rels+release-group-rels+artist-rels",
            )
            .append_pair("fmt", "json");
        url.into()
    }
    pub fn entity_lookup(kind: MbKind, mbid: &str) -> String {
        format!("{BASE}/{}/{mbid}?inc={}&fmt=json", kind.as_str(), inc(kind))
    }
    /// What `link` is linked to; `None` when MusicBrainz does not know the URL.
    pub async fn url(egress: &Egress, link: &str) -> Result<Option<UrlTargets>> {
        Ok(get(egress, &url_lookup(link), "musicbrainz")
            .await?
            .map(|v| parse_url(&v)))
    }
    /// One entity with its URL relations; `None` when the MBID is unknown.
    pub async fn entity(egress: &Egress, kind: MbKind, id: &str) -> Result<Option<Entity>> {
        let Some(id) = mbid(id) else {
            return Ok(None);
        };
        Ok(get(egress, &entity_lookup(kind, &id), "musicbrainz")
            .await?
            .and_then(|v| parse_entity(kind, &v)))
    }
}

pub mod wikidata {
    //! Wikidata: an item's best-rank external IDs and its Wikipedia (and other) sitelinks, and
    //! the item that carries one external ID.
    use super::*;

    pub const API: &str = "https://www.wikidata.org/w/api.php";

    /// An item as a source needs it.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Item {
        pub qid: String,
        /// External-ID and string claims at their best rank (preferred if any, else normal;
        /// deprecated never), by property.
        pub ids: BTreeMap<String, Vec<String>>,
        /// Sitelinks: site (`enwiki`, `dewiki`, `commonswiki`, …) → page URL.
        pub sitelinks: BTreeMap<String, String>,
    }
    impl Item {
        pub fn id(&self, property: &str) -> &[String] {
            self.ids.get(property).map_or(&[], Vec::as_slice)
        }
    }
    /// The item found by an exact external ID.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Found {
        None,
        One(Box<Item>),
        /// Several items carry the ID at their best rank: no item is chosen.
        Ambiguous(usize),
    }

    /// `wbgetentities` with `props=claims|sitelinks/urls`.
    pub fn parse_entities(v: &Value) -> Vec<Item> {
        let mut items: Vec<Item> = v["entities"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(id, e)| {
                if e.get("missing").is_some() || e["type"] != "item" {
                    return None;
                }
                let qid = qid(e["id"].as_str().unwrap_or(id))?;
                let mut ids = BTreeMap::new();
                for (property, claims) in e["claims"].as_object().into_iter().flatten() {
                    let claims = claims.as_array().map_or(&[][..], Vec::as_slice);
                    let rank = if claims.iter().any(|c| c["rank"] == "preferred") {
                        "preferred"
                    } else {
                        "normal"
                    };
                    let values: Vec<String> = claims
                        .iter()
                        .filter(|c| c["rank"] == rank)
                        .map(|c| &c["mainsnak"])
                        .filter(|s| {
                            s["snaktype"] == "value"
                                && matches!(s["datatype"].as_str(), Some("external-id" | "string"))
                        })
                        .filter_map(|s| s["datavalue"]["value"].as_str())
                        .map(str::to_owned)
                        .collect();
                    if !values.is_empty() {
                        ids.insert(property.clone(), values);
                    }
                }
                let sitelinks = e["sitelinks"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(site, l)| Some((site.clone(), l["url"].as_str()?.to_owned())))
                    .collect();
                Some(Item {
                    qid,
                    ids,
                    sitelinks,
                })
            })
            .collect();
        items.sort_by(|a, b| a.qid.cmp(&b.qid));
        items
    }
    /// `list=search` hits as item IDs.
    pub fn parse_search(v: &Value) -> Vec<String> {
        v["query"]["search"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| qid(h["title"].as_str()?))
            .collect()
    }
    pub fn entities_lookup(qids: &[String]) -> String {
        format!(
            "{API}?action=wbgetentities&ids={}&props=claims%7Csitelinks%2Furls&format=json",
            qids.join("%7C")
        )
    }
    /// A property ID (`P` and digits) and an external-ID value the search syntax can carry
    /// exactly (letters, digits and `._:-`, at most 200 bytes).
    fn statement(property: &str, value: &str) -> Option<String> {
        let digits = property.strip_prefix('P')?;
        let property_ok = (1..=8).contains(&digits.len())
            && !digits.starts_with('0')
            && digits.bytes().all(|b| b.is_ascii_digit());
        let value_ok = (1..=200).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
        (property_ok && value_ok).then(|| format!("haswbstatement:{property}={value}"))
    }
    pub fn search_lookup(property: &str, value: &str) -> Option<String> {
        let query = statement(property, value)?;
        let mut url = url::Url::parse(API).expect("constant URL");
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("list", "search")
            .append_pair("srsearch", &query)
            .append_pair("srlimit", "5")
            .append_pair("format", "json");
        Some(url.into())
    }
    /// Up to 50 items by ID; unknown or redirected-away IDs are left out.
    pub async fn items(egress: &Egress, qids: &[String]) -> Result<Vec<Item>> {
        let mut qids: Vec<String> = qids.iter().filter_map(|q| qid(q)).collect();
        qids.sort();
        qids.dedup();
        qids.truncate(50);
        if qids.is_empty() {
            return Ok(vec![]);
        }
        Ok(get(egress, &entities_lookup(&qids), "wikidata")
            .await?
            .map(|v| parse_entities(&v))
            .unwrap_or_default())
    }
    /// The one item carrying `value` for `property` at its best rank. The search index only
    /// proposes; each hit is read back and its claim compared byte for byte, so a stale index
    /// or a deprecated claim never joins. Two or more verified items are ambiguous.
    pub async fn item_with(egress: &Egress, property: &str, value: &str) -> Result<Found> {
        let Some(lookup) = search_lookup(property, value) else {
            return Ok(Found::None);
        };
        let hits = get(egress, &lookup, "wikidata")
            .await?
            .map(|v| parse_search(&v))
            .unwrap_or_default();
        Ok(verified(items(egress, &hits).await?, property, value))
    }
    /// The verification step of [`item_with`].
    pub fn verified(items: Vec<Item>, property: &str, value: &str) -> Found {
        let mut matching: Vec<Item> = items
            .into_iter()
            .filter(|i| i.id(property).iter().any(|v| v == value))
            .collect();
        match matching.len() {
            0 => Found::None,
            1 => Found::One(Box::new(matching.remove(0))),
            n => Found::Ambiguous(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn keys_take_only_canonical_identifiers() {
        let id = "9f9cf187-d6f9-437f-9d98-d59cdbd52757";
        assert_eq!(
            key::mb(MbKind::Recording, id).as_deref(),
            Some("mb:recording:9f9cf187-d6f9-437f-9d98-d59cdbd52757")
        );
        assert_eq!(
            key::mb(MbKind::ReleaseGroup, &id.to_ascii_uppercase()).as_deref(),
            Some("mb:release-group:9f9cf187-d6f9-437f-9d98-d59cdbd52757")
        );
        for bad in [
            "",
            "9f9cf187d6f9437f9d98d59cdbd52757",
            "9f9cf187-d6f9-437f-9d98-d59cdbd5275g",
            "{9f9cf187-d6f9-437f-9d98-d59cdbd52757}",
        ] {
            assert!(key::mb(MbKind::Artist, bad).is_none(), "{bad}");
        }
        assert_eq!(
            key::wikidata("Q202996").as_deref(),
            Some("wikidata:Q202996")
        );
        for bad in [
            "Q",
            "Q0",
            "Q012",
            "q202996",
            "P436",
            "Q202996 ",
            "Q1234567890123",
        ] {
            assert!(key::wikidata(bad).is_none(), "{bad}");
        }
        // Every key a constructor builds is a key the store accepts.
        assert!(crate::also_on::valid_key(
            &key::mb(MbKind::ReleaseGroup, id).unwrap()
        ));
        assert!(crate::also_on::valid_key(&key::wikidata("Q42").unwrap()));
    }

    #[test]
    fn musicbrainz_url_entity_and_release_parsers() {
        let track = musicbrainz::parse_url(&fixture("mb-url-spotify-track.json"));
        assert_eq!(
            track.targets,
            [(
                MbKind::Recording,
                "9f9cf187-d6f9-437f-9d98-d59cdbd52757".to_owned()
            )]
        );
        let album = musicbrainz::parse_url(&fixture("mb-url-spotify-album.json"));
        assert_eq!(album.targets[0].0, MbKind::Release);
        let recording =
            musicbrainz::parse_entity(MbKind::Recording, &fixture("mb-recording.json")).unwrap();
        assert_eq!(recording.isrcs, ["GBAYE9701376", "GBBKS1700108"]);
        assert!(recording
            .urls
            .iter()
            .any(|u| u.url == "https://www.deezer.com/track/138539973"));
        let release =
            musicbrainz::parse_entity(MbKind::Release, &fixture("mb-release.json")).unwrap();
        assert_eq!(release.barcode.as_deref(), Some("634904078164"));
        assert_eq!(
            release.release_group.as_deref(),
            Some("b1392450-e666-3926-a536-22c65f834433")
        );
        assert!(release
            .urls
            .iter()
            .any(|u| u.url == "https://radiohead.bandcamp.com/album/ok-computer"));
        let group =
            musicbrainz::parse_entity(MbKind::ReleaseGroup, &fixture("mb-release-group.json"))
                .unwrap();
        assert!(group.urls.iter().all(|u| !u.url.is_empty()));
        assert!(
            musicbrainz::url_lookup("https://open.spotify.com/track/x?si=1&a=b")
                .contains("resource=https%3A%2F%2Fopen.spotify.com%2Ftrack%2Fx%3Fsi%3D1%26a%3Db")
        );
    }

    #[test]
    fn wikidata_best_rank_ids_sitelinks_and_ambiguity() {
        let items = wikidata::parse_entities(&fixture("wd-entities.json"));
        assert_eq!(
            items.iter().map(|i| i.qid.as_str()).collect::<Vec<_>>(),
            ["Q202996", "Q33328630"]
        );
        let ok = &items[0];
        assert_eq!(ok.id("P436"), ["b1392450-e666-3926-a536-22c65f834433"]);
        assert_eq!(
            ok.sitelinks.get("enwiki").map(String::as_str),
            Some("https://en.wikipedia.org/wiki/OK_Computer")
        );
        assert!(ok.id("P31").is_empty(), "items are not external IDs");
        // Both items carry the release group at normal rank: no item is chosen.
        assert_eq!(
            wikidata::verified(
                items.clone(),
                "P436",
                "b1392450-e666-3926-a536-22c65f834433"
            ),
            wikidata::Found::Ambiguous(2)
        );
        assert_eq!(
            wikidata::verified(items.clone(), "P2205", "2fGCAYUMssLKiUAoNdxGLx"),
            wikidata::Found::One(Box::new(items[0].clone()))
        );
        // Byte for byte: a different case is another ID.
        assert_eq!(
            wikidata::verified(items, "P2205", "2FGCAYUMSSLKIUAONDXGLX"),
            wikidata::Found::None
        );
        assert_eq!(
            wikidata::parse_search(&fixture("wd-search-p436.json")),
            ["Q202996", "Q33328630"]
        );
        // The search syntax only carries plain IDs.
        assert!(wikidata::search_lookup("P436", "a b").is_none());
        assert!(wikidata::search_lookup("P0436", "x").is_none());
        assert!(wikidata::search_lookup("P436", "b1392450-e666").is_some());
    }

    #[test]
    fn a_deprecated_claim_never_counts_and_preferred_wins() {
        let v = serde_json::json!({"entities":{"Q1":{"type":"item","id":"Q1","claims":{
            "P1":[{"rank":"deprecated","mainsnak":{"snaktype":"value","datatype":"external-id","datavalue":{"value":"old"}}},
                  {"rank":"normal","mainsnak":{"snaktype":"value","datatype":"external-id","datavalue":{"value":"n"}}}],
            "P2":[{"rank":"preferred","mainsnak":{"snaktype":"value","datatype":"external-id","datavalue":{"value":"best"}}},
                  {"rank":"normal","mainsnak":{"snaktype":"value","datatype":"external-id","datavalue":{"value":"other"}}}],
            "P3":[{"rank":"normal","mainsnak":{"snaktype":"novalue","datatype":"external-id"}}]}},
            "Q2":{"id":"Q2","missing":""}}});
        let items = wikidata::parse_entities(&v);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id("P1"), ["n"]);
        assert_eq!(items[0].id("P2"), ["best"]);
        assert!(items[0].id("P3").is_empty());
    }
}
