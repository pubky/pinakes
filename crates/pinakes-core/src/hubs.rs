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

/// A hub's answer as JSON: `None` for 404, a retry for 429/503 (and other 5xx) and for an API
/// error Wikidata sends with 200 (`ratelimited`, `maxlag`; an unknown ID is an answer), an error
/// for anything else that is not 200 JSON.
fn json(fetched: &Fetched, hub: &'static str) -> Result<Option<Value>> {
    let busy = || -> anyhow::Error {
        RetryLater {
            class: if hub == "musicbrainz" {
                "musicbrainz_busy"
            } else {
                "wikidata_busy"
            },
            seconds: fetched.retry_after_secs.clamp(60, 3600),
        }
        .into()
    };
    match fetched.status {
        200 => {
            let v: Value = serde_json::from_slice(&fetched.body)?;
            match v["error"]["code"].as_str() {
                Some(code) if hub == "wikidata" && code != "no-such-entity" => Err(busy()),
                _ => Ok(Some(v)),
            }
        }
        404 => Ok(None),
        429 | 500..=599 => Err(busy()),
        status => bail!("{hub} answered {status}"),
    }
}
/// The body cap for Wikidata entities: a much-edited item carries megabytes of claims (Signal's
/// is 2.6 MB), above the egress default of 1 MB.
pub const WIKIDATA_CAP: usize = 8 * 1024 * 1024;
/// The body cap for MusicBrainz answers: an artist or release with very many URL relations
/// exceeds the egress default too.
pub const MUSICBRAINZ_CAP: usize = 8 * 1024 * 1024;
fn options(hub: &str) -> FetchOptions {
    FetchOptions {
        accept: Some("application/json".into()),
        body_cap: match hub {
            "wikidata" => Some(WIKIDATA_CAP),
            "musicbrainz" => Some(MUSICBRAINZ_CAP),
            _ => None,
        },
        ..Default::default()
    }
}
async fn get(egress: &Egress, url: &str, hub: &'static str) -> Result<Option<Value>> {
    // Shared flights and a short cache: two sources asking the same thing ask once.
    let fetched = egress.discover(url, &options(hub), 3600).await?;
    let answer = json(&fetched, hub);
    // A busy answer (Wikidata's `maxlag` comes with 200) is no answer to keep for an hour: the
    // next ask fetches again.
    if answer
        .as_ref()
        .err()
        .is_some_and(|e| e.downcast_ref::<RetryLater>().is_some())
    {
        egress.shorten_discovery(url, &options(hub), 1).await?;
    }
    answer
}
/// Cache `body` (status 200, JSON) or a 404 as the answer to `url`, a lookup a batch covered.
async fn seed(egress: &Egress, url: &str, hub: &str, answer: Option<&Value>) -> Result<()> {
    let fetched = Fetched {
        permanent_redirect: false,
        truncated: false,
        validators: Default::default(),
        url: url::Url::parse(url)?,
        status: if answer.is_some() { 200 } else { 404 },
        retry_after_secs: 60,
        location: None,
        content_type: "application/json".into(),
        body: serde_json::to_vec(answer.unwrap_or(&serde_json::json!({"error": "Not Found"})))?,
    };
    egress.seed(url, &options(hub), 3600, fetched).await
}

pub mod musicbrainz {
    //! MusicBrainz `ws/2`: what a URL is linked to, and an entity's URL relations, ISRCs and
    //! barcode.
    use super::*;

    /// The public web service; `[also_on.sources.musicbrainz] base_url` names a mirror instead.
    pub const BASE: &str = "https://musicbrainz.org/ws/2";

    /// A MusicBrainz page's path segments after `/ws/2/` when it has one (`musicbrainz.org`,
    /// `www.` and `beta.`, http or https, a trailing slash and parameters dropped).
    fn page_segments(uri: &str) -> Option<Vec<String>> {
        let url = url::Url::parse(uri).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !matches!(
                url.host_str()?,
                "musicbrainz.org" | "www.musicbrainz.org" | "beta.musicbrainz.org"
            )
        {
            return None;
        }
        let mut segments: Vec<String> = url
            .path_segments()?
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if segments.get(..2) == Some(&["ws".to_owned(), "2".to_owned()]) {
            segments.drain(..2);
        }
        Some(segments)
    }
    /// A link to a recording, release, release group or artist (its page or its web service
    /// record): the entity, its MBID and its page `https://musicbrainz.org/{kind}/{mbid}`.
    pub fn entity_link(uri: &str) -> Option<(MbKind, String, String)> {
        let segments = page_segments(uri)?;
        let [kind, id] = segments.as_slice() else {
            return None;
        };
        let kind = match kind.as_str() {
            "recording" => MbKind::Recording,
            "release" => MbKind::Release,
            "release-group" => MbKind::ReleaseGroup,
            "artist" => MbKind::Artist,
            _ => return None,
        };
        let mbid = mbid(id)?;
        let page = format!("https://musicbrainz.org/{}/{mbid}", kind.as_str());
        Some((kind, mbid, page))
    }
    /// A link to an ISRC's page (`/isrc/{ISRC}`): the ISRC as keys store it and its page.
    pub fn isrc_link(uri: &str) -> Option<(String, String)> {
        let segments = page_segments(uri)?;
        let ["isrc", code] = segments.iter().map(String::as_str).collect::<Vec<_>>()[..] else {
            return None;
        };
        let isrc = crate::music::isrc(code)?;
        let page = format!("https://musicbrainz.org/isrc/{isrc}");
        Some((isrc, page))
    }

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
        /// Artists: `Person`, `Group`, `Orchestra`, `Choir`, `Character` or `Other`.
        pub artist_type: Option<String>,
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
            artist_type: (kind == MbKind::Artist)
                .then(|| v["type"].as_str())
                .flatten()
                .filter(|t| t.len() <= 32 && t.bytes().all(|b| b.is_ascii_alphabetic()))
                .map(str::to_owned),
        })
    }
    fn inc(kind: MbKind) -> &'static str {
        match kind {
            MbKind::Recording => "url-rels+isrcs+artist-credits",
            MbKind::Release => "url-rels+release-groups+artist-credits",
            MbKind::ReleaseGroup => "url-rels+artist-credits",
            MbKind::Artist => "url-rels",
        }
    }
    /// The URL lookup for `link` at the web service `base` (exact; MusicBrainz stores URLs as
    /// given, so the link is sent as the card has it).
    pub fn url_lookup(base: &str, link: &str) -> String {
        let mut url = url::Url::parse(&format!("{base}/url")).expect("a checked base URL");
        url.query_pairs_mut()
            .append_pair("resource", link)
            .append_pair(
                "inc",
                "recording-rels+release-rels+release-group-rels+artist-rels",
            )
            .append_pair("fmt", "json");
        url.into()
    }
    pub fn entity_lookup(base: &str, kind: MbKind, mbid: &str) -> String {
        format!("{base}/{}/{mbid}?inc={}&fmt=json", kind.as_str(), inc(kind))
    }
    /// The batched URL lookup: what each of `links` is linked to, in one request.
    pub fn urls_lookup(base: &str, links: &[String]) -> String {
        let mut url = url::Url::parse(&format!("{base}/url")).expect("a checked base URL");
        {
            let mut query = url.query_pairs_mut();
            for link in links {
                query.append_pair("resource", link);
            }
            query
                .append_pair(
                    "inc",
                    "recording-rels+release-rels+release-group-rels+artist-rels",
                )
                .append_pair("fmt", "json");
        }
        url.into()
    }
    /// At most this many links per batched URL lookup (MusicBrainz takes 100; the request line
    /// stays short).
    pub const URL_BATCH: usize = 50;
    /// Ask MusicBrainz about many links at once and cache each link's answer as its own lookup's
    /// (`url` then asks nothing). An answer applies only to the URL MusicBrainz echoes, byte for
    /// byte; a link left out is unknown (its 404 cached) only when the answer is whole
    /// (`url-count` answers) and every echo matched a link asked. Best effort: a failed batch
    /// caches nothing, and its links ask alone; a busy MusicBrainz stops the batches.
    pub async fn prefetch_urls(egress: &Egress, base: &str, links: &[String]) -> Result<usize> {
        let mut seeded = 0;
        for chunk in links.chunks(URL_BATCH) {
            let v = match get(egress, &urls_lookup(base, chunk), "musicbrainz").await {
                Ok(Some(v)) => v,
                Ok(None) => continue,
                Err(e) if e.downcast_ref::<RetryLater>().is_some() => return Err(e),
                Err(_) => continue,
            };
            let answers: Vec<&Value> = v["urls"].as_array().into_iter().flatten().collect();
            let whole = v["url-count"].as_u64() == Some(answers.len() as u64);
            let exact = whole
                && answers.iter().all(|a| {
                    a["resource"]
                        .as_str()
                        .is_some_and(|r| chunk.iter().any(|l| l == r))
                });
            for link in chunk {
                let answer = answers
                    .iter()
                    .find(|a| a["resource"].as_str() == Some(link.as_str()));
                if answer.is_none() && !exact {
                    continue;
                }
                seed(
                    egress,
                    &url_lookup(base, link),
                    "musicbrainz",
                    answer.copied(),
                )
                .await?;
                seeded += 1;
            }
        }
        Ok(seeded)
    }
    /// What `link` is linked to; `None` when MusicBrainz does not know the URL. `base` is the
    /// web service ([`BASE`] or a mirror).
    pub async fn url(egress: &Egress, base: &str, link: &str) -> Result<Option<UrlTargets>> {
        Ok(get(egress, &url_lookup(base, link), "musicbrainz")
            .await?
            .map(|v| parse_url(&v)))
    }
    /// A recording's original album: of its official albums, the earliest release of a release
    /// group that is an Album with no secondary type (not a compilation, live or remix album);
    /// the release group's title.
    pub async fn original_album(
        egress: &Egress,
        base: &str,
        recording: &str,
    ) -> Result<Option<String>> {
        let Some(recording) = mbid(recording) else {
            return Ok(None);
        };
        let url = format!("{base}/release?recording={recording}&type=album&status=official&inc=release-groups&limit=100&fmt=json");
        Ok(get(egress, &url, "musicbrainz")
            .await?
            .and_then(|v| parse_original_album(&v)))
    }
    pub fn parse_original_album(v: &Value) -> Option<String> {
        v["releases"]
            .as_array()?
            .iter()
            .filter(|r| {
                let group = &r["release-group"];
                group["primary-type"] == "Album"
                    && group["secondary-types"]
                        .as_array()
                        .is_none_or(Vec::is_empty)
            })
            .filter_map(|r| {
                let date = r["date"].as_str().filter(|d| d.len() >= 4)?;
                let title = r["release-group"]["title"].as_str()?.trim();
                (!title.is_empty()).then_some((date, title))
            })
            .min()
            .map(|(_, title)| title.to_owned())
    }
    /// The recordings an ISRC is on; `None` when MusicBrainz does not know the ISRC.
    pub async fn isrc(egress: &Egress, base: &str, isrc: &str) -> Result<Option<Vec<String>>> {
        let Some(isrc) = crate::music::isrc(isrc) else {
            return Ok(None);
        };
        Ok(get(
            egress,
            &format!("{base}/isrc/{isrc}?fmt=json"),
            "musicbrainz",
        )
        .await?
        .map(|v| parse_isrc(&v)))
    }
    /// `GET /release?query=barcode:…`: the releases carrying exactly `upc` (a GTIN-13; a
    /// 12-digit UPC-A is asked for in both spellings, as MusicBrainz stores barcodes as given),
    /// each once; empty when MusicBrainz has none. The search matches the barcode field alone,
    /// and the answer is checked against it.
    pub async fn barcode(egress: &Egress, base: &str, upc: &str) -> Result<Vec<String>> {
        let Some(upc) = crate::music::gtin13(upc) else {
            return Ok(vec![]);
        };
        Ok(get(egress, &barcode_lookup(base, &upc), "musicbrainz")
            .await?
            .map(|v| parse_barcode(&v, &upc))
            .unwrap_or_default())
    }
    pub fn barcode_lookup(base: &str, upc: &str) -> String {
        let query = match upc.strip_prefix('0').filter(|_| upc.len() == 13) {
            Some(short) => format!("barcode:({upc} OR {short})"),
            None => format!("barcode:{upc}"),
        };
        let mut url = url::Url::parse(&format!("{base}/release")).expect("a checked base URL");
        url.query_pairs_mut()
            .append_pair("query", &query)
            .append_pair("fmt", "json")
            .append_pair("limit", "10");
        url.into()
    }
    /// The releases of a barcode search whose barcode is `upc` once normalised, each once.
    pub fn parse_barcode(v: &Value, upc: &str) -> Vec<String> {
        let mut ids: Vec<String> = v["releases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                r["barcode"]
                    .as_str()
                    .and_then(crate::music::gtin13)
                    .is_some_and(|b| b == upc)
            })
            .filter_map(|r| mbid(r["id"].as_str()?))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
    /// `GET /isrc/{ISRC}`: the recordings' MBIDs, each once.
    pub fn parse_isrc(v: &Value) -> Vec<String> {
        let mut ids: Vec<String> = v["recordings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| mbid(r["id"].as_str()?))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
    /// One entity with its URL relations; `None` when the MBID is unknown.
    pub async fn entity(
        egress: &Egress,
        base: &str,
        kind: MbKind,
        id: &str,
    ) -> Result<Option<Entity>> {
        let Some(id) = mbid(id) else {
            return Ok(None);
        };
        Ok(get(egress, &entity_lookup(base, kind, &id), "musicbrainz")
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
    /// The item a Wikidata item link names, with the link's canonical form
    /// (`https://www.wikidata.org/wiki/{Q-id}`): `/wiki/Q…`, `/entity/Q…` and
    /// `/wiki/Special:EntityPage/Q…`, on `www.`, `m.` or the bare host.
    pub fn item_link(uri: &str) -> Option<(String, String)> {
        let url = url::Url::parse(uri).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !matches!(
                url.host_str()?,
                "www.wikidata.org" | "m.wikidata.org" | "wikidata.org"
            )
        {
            return None;
        }
        let id = url
            .path()
            .strip_prefix("/wiki/Special:EntityPage/")
            .or_else(|| url.path().strip_prefix("/wiki/"))
            .or_else(|| url.path().strip_prefix("/entity/"))?;
        let qid = qid(id)?;
        let canonical = format!("https://www.wikidata.org/wiki/{qid}");
        Some((qid, canonical))
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
    /// At most this many items per batched read (Wikidata takes 50; an item can be megabytes).
    pub const ITEM_BATCH: usize = 10;
    /// Read many items at once and cache each one as its own read's answer (`items` with one
    /// Q-id then asks nothing). Best effort: a batch past the body cap caches nothing.
    pub async fn prefetch_items(egress: &Egress, qids: &[String]) -> Result<usize> {
        let mut seeded = 0;
        for chunk in qids.chunks(ITEM_BATCH) {
            let Ok(Some(v)) = get(egress, &entities_lookup(chunk), "wikidata").await else {
                continue;
            };
            for qid in chunk {
                if let Some(entity) = v["entities"].get(qid) {
                    let one = serde_json::json!({"entities": {qid.as_str(): entity}});
                    seed(
                        egress,
                        &entities_lookup(std::slice::from_ref(qid)),
                        "wikidata",
                        Some(&one),
                    )
                    .await?;
                    seeded += 1;
                }
            }
        }
        Ok(seeded)
    }
    pub fn entities_lookup(qids: &[String]) -> String {
        format!(
            "{API}?action=wbgetentities&ids={}&props=claims%7Csitelinks%2Furls&format=json",
            qids.join("%7C")
        )
    }
    /// A property ID (`P` and digits) and an external-ID value the search syntax can carry
    /// exactly (letters, digits and `._:-/`, at most 200 bytes; a Trakt ID is `shows/breaking-bad`).
    fn statement(property: &str, value: &str) -> Option<String> {
        let digits = property.strip_prefix('P')?;
        let property_ok = (1..=8).contains(&digits.len())
            && !digits.starts_with('0')
            && digits.bytes().all(|b| b.is_ascii_digit());
        let value_ok = (1..=200).contains(&value.len())
            && value.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-' | b'/')
            });
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
        let read = match items(egress, &hits).await {
            // Items over the cap together: each alone. One over it alone can't be verified, so
            // the search has no answer (its refusal).
            Err(e) if hits.len() > 1 && crate::resolver::egress::over_cap(&e) => {
                let mut read = vec![];
                for qid in &hits {
                    read.extend(items(egress, std::slice::from_ref(qid)).await?);
                }
                read
            }
            read => read?,
        };
        Ok(verified(read, property, value))
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

    /// Items over the cap together are read one by one, so one never sinks the other's answer;
    /// one over it alone is no answer (its refusal).
    #[tokio::test(start_paused = true)]
    async fn items_over_the_cap_together_are_read_alone() {
        let search = wikidata::search_lookup("P4983", "1396").unwrap();
        let both = wikidata::entities_lookup(&["Q1".into(), "Q2".into()]);
        let (one, two) = (
            wikidata::entities_lookup(&["Q1".into()]),
            wikidata::entities_lookup(&["Q2".into()]),
        );
        let item = |q: &str, v: &str| {
            serde_json::to_vec(
                &serde_json::json!({"entities": {q: {"type": "item", "id": q,
                "claims": {"P4983": [{"rank": "normal", "mainsnak": {"snaktype": "value",
                    "datatype": "external-id", "datavalue": {"value": v}}}]}}}}),
            )
            .unwrap()
        };
        let hits = br#"{"query":{"search":[{"title":"Q1"},{"title":"Q2"}]}}"#.to_vec();
        let (egress, recorded) = crate::sources::fixture::egress(vec![
            (&search, 200, "application/json", hits.clone()),
            (&both, 200, "!too-large", vec![]),
            (&one, 200, "application/json", item("Q1", "1396")),
            (&two, 200, "application/json", item("Q2", "9999")),
        ]);
        let found = wikidata::item_with(&egress, "P4983", "1396").await.unwrap();
        assert!(
            matches!(&found, wikidata::Found::One(i) if i.qid == "Q1"),
            "{found:?}"
        );
        assert!(recorded.asked().contains(&both), "{:?}", recorded.asked());
        // Q2 over the cap alone: the search can't be verified.
        let (egress, _) = crate::sources::fixture::egress(vec![
            (&search, 200, "application/json", hits),
            (&both, 200, "!too-large", vec![]),
            (&one, 200, "application/json", item("Q1", "1396")),
            (&two, 200, "!too-large", vec![]),
        ]);
        let error = wikidata::item_with(&egress, "P4983", "1396")
            .await
            .unwrap_err();
        assert!(crate::resolver::egress::over_cap(&error), "{error:#}");
    }

    /// Wikidata's `maxlag` comes with 200: the answer is busy, and the next ask fetches again
    /// instead of reading it from the cache for an hour.
    #[tokio::test(start_paused = true)]
    async fn a_busy_answer_is_asked_again() {
        let url = wikidata::search_lookup("P4983", "1396").unwrap();
        let (egress, recorded) = crate::sources::fixture::egress(vec![(
            &url,
            200,
            "application/json",
            br#"{"error":{"code":"maxlag","info":"Waiting for a database server"}}"#.to_vec(),
        )]);
        for _ in 0..2 {
            let error = wikidata::item_with(&egress, "P4983", "1396")
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<RetryLater>().is_some(), "{error:#}");
            tokio::time::advance(std::time::Duration::from_secs(2)).await;
        }
        assert_eq!(recorded.asked().len(), 2);
    }
    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Every spelling of a MusicBrainz entity or ISRC page is one link; other pages are not.
    #[test]
    fn musicbrainz_links_name_their_entity() {
        let recording = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
        for uri in [
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            "http://musicbrainz.org/recording/B1A9C0E9-D987-4042-AE91-78D6A3267D69/",
            "https://beta.musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            "https://www.musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69?x=1",
            "https://musicbrainz.org/ws/2/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69?inc=isrcs&fmt=json",
        ] {
            assert_eq!(
                musicbrainz::entity_link(uri),
                Some((
                    MbKind::Recording,
                    recording.to_owned(),
                    format!("https://musicbrainz.org/recording/{recording}")
                )),
                "{uri}"
            );
        }
        let group = "https://musicbrainz.org/release-group/1f2e3d4c-5b6a-4978-8695-a4b3c2d1e0f9";
        assert_eq!(
            musicbrainz::entity_link(group).map(|(kind, ..)| kind),
            Some(MbKind::ReleaseGroup)
        );
        assert_eq!(
            musicbrainz::isrc_link("https://musicbrainz.org/isrc/gbum71029604"),
            Some((
                "GBUM71029604".to_owned(),
                "https://musicbrainz.org/isrc/GBUM71029604".to_owned()
            ))
        );
        for other in [
            "https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711/releases",
            "https://musicbrainz.org/work/a74b1b7f-71a5-4011-9441-d0b5e4122711",
            "https://musicbrainz.org/recording/not-an-mbid",
            "https://musicbrainz.org.evil.example/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            "https://musicbrainz.org/isrc/NOTANISRC",
            "ftp://musicbrainz.org/isrc/GBUM71029604",
        ] {
            assert!(
                musicbrainz::entity_link(other).is_none()
                    && musicbrainz::isrc_link(other).is_none(),
                "{other}"
            );
        }
    }

    /// Wikidata sends a rate limit as a 200 with an `error`: busy, never "no item".
    #[test]
    fn a_wikidata_error_sent_with_200_is_busy() {
        let answer = |body: &str| Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url::Url::parse("https://www.wikidata.org/w/api.php").unwrap(),
            status: 200,
            retry_after_secs: 5,
            location: None,
            content_type: "application/json".into(),
            body: body.as_bytes().to_vec(),
        };
        let limited = answer(r#"{"error":{"code":"ratelimited","info":"slow down"}}"#);
        let err = json(&limited, "wikidata").unwrap_err();
        assert!(err.downcast_ref::<RetryLater>().is_some(), "{err:#}");
        let unknown = answer(r#"{"error":{"code":"no-such-entity","id":"Q0"}}"#);
        assert!(json(&unknown, "wikidata").unwrap().is_some());
        assert!(json(&limited, "musicbrainz").unwrap().is_some());
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
        let search = serde_json::json!({"releases": [
            {"id": "70477949-9da4-4fb3-ba60-268ff6f3a62c", "barcode": "4018939306052"},
            {"id": "11111111-2222-4333-8444-555555555555", "barcode": "4018939306052"},
            {"id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee", "barcode": "4018939306069"},
            {"id": "99999999-8888-4777-8666-555555555555", "barcode": "018939306052"}
        ]});
        assert_eq!(
            musicbrainz::parse_barcode(&search, "4018939306052"),
            [
                "11111111-2222-4333-8444-555555555555",
                "70477949-9da4-4fb3-ba60-268ff6f3a62c"
            ]
        );
        assert!(
            musicbrainz::barcode_lookup("https://musicbrainz.org/ws/2", "0196874054246")
                .contains("barcode%3A%280196874054246+OR+196874054246%29")
        );
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
        assert!(musicbrainz::url_lookup(
            musicbrainz::BASE,
            "https://open.spotify.com/track/x?si=1&a=b"
        )
        .contains("resource=https%3A%2F%2Fopen.spotify.com%2Ftrack%2Fx%3Fsi%3D1%26a%3Db"));
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
        assert!(wikidata::search_lookup("P8013", "shows/breaking-bad")
            .unwrap()
            .contains("haswbstatement%3AP8013%3Dshows%2Fbreaking-bad"));
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
