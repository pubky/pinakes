use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};
use utoipa::ToSchema;

pub const DOCUMENT_CAP: usize = 8192;
pub const ATTRIBUTE_CAP: usize = 32;

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    #[default]
    Pending,
    Missing,
    Unreachable,
    Error,
}
impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Pending => "pending",
            Self::Missing => "missing",
            Self::Unreachable => "unreachable",
            Self::Error => "error",
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Point {
    pub lat: f64,
    pub lon: f64,
}
/// A client that opens an object, and the object's address there.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct OpenIn {
    /// The client's name ("Loopky", "Pubky Rooms").
    pub client: String,
    /// An `https:` URL.
    pub url: String,
}
/// Audio a client can play: an episode's enclosure, or a recording's public preview (docs/api.md,
/// Stream).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct Stream {
    pub url: String,
    #[serde(rename = "type")]
    pub media_type: String,
    pub duration_s: Option<u32>,
    pub preview: bool,
}
impl Stream {
    /// The stream, when `url` is public, direct audio a client may play: `https` or `http` on a
    /// named host (no IP address, `localhost` or single label), no credentials, no parameter that
    /// signs it or makes it expire ([`signed`]), at most 2,048 bytes; `media_type` an `audio/` or
    /// `video/` type of at most 127 bytes (RFC 6838's limit for a subtype; the parameters are
    /// dropped); a length of at most a week (0 is unknown). Every source's stream is built here.
    pub fn public(
        url: &str,
        media_type: &str,
        duration_s: Option<u64>,
        preview: bool,
    ) -> Option<Stream> {
        let parsed = url::Url::parse(url.trim()).ok()?;
        if !matches!(parsed.scheme(), "https" | "http")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query_pairs().any(|(name, _)| signed(&name))
        {
            return None;
        }
        let url::Host::Domain(host) = parsed.host()? else {
            return None;
        };
        let host = host.trim_end_matches('.');
        if !host.contains('.') || host == "localhost" || host.ends_with(".localhost") {
            return None;
        }
        let essence = media_type.split(';').next()?.trim().to_ascii_lowercase();
        let (top, sub) = essence.split_once('/')?;
        if !matches!(top, "audio" | "video")
            || sub.is_empty()
            || essence.len() > 127
            || !sub
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
        {
            return None;
        }
        let duration_s = match duration_s {
            Some(s) if s > 7 * 86400 => return None,
            s => s.filter(|s| *s > 0).map(|s| s as u32),
        };
        let url = String::from(parsed);
        (url.len() <= 2048).then_some(Stream {
            url,
            media_type: essence,
            duration_s,
            preview,
        })
    }
}
/// A query parameter that signs a URL or makes it expire (`X-Amz-Signature`, Deezer's
/// `hdnea`, a `token`, an `expires`): such a link stops working, or works only for whom it was
/// made, so it is no public stream.
fn signed(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "sig" | "hdnea" | "hmac" | "auth" | "expires" | "exp" | "policy" | "key-pair-id"
    ) || name.starts_with("x-amz-")
        || name.starts_with("x-goog-")
        || name.contains("token")
        || name.contains("signature")
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocationSource {
    Uri,
    Osm,
    Page,
    Geocoded,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize, ToSchema)]
pub struct OsmId {
    #[serde(rename = "type")]
    pub osm_type: String,
    pub id: i64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct ResourceMeta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_keys: Vec<crate::group_keys::GroupKey>,
    #[serde(default, flatten)]
    pub event: crate::event::EventFields,
    /// A resolution result, never embedded in the bounded resource document.
    #[serde(skip)]
    #[schema(ignore)]
    pub container: Option<Box<crate::event::Container>>,
    #[serde(rename = "type")]
    pub type_name: String,
    pub type_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The audio a client can play ([`Stream`]): an episode's enclosure, a recording's preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<Stream>,
    pub url: String,
    pub site: String,
    /// The service the link is on, by its URL alone ("Spotify", "F-Droid", "Wikidata"), when
    /// this build knows it (`api::service_site`): only such a member takes a service's one place
    /// on a card (`also_on()`), never a page that names itself after one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Point>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_source: Option<LocationSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub osm_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[f64; 4]>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub address: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub attributes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// The clients that open a Pubky app's object (`resolver::pubky_apps`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_in: Vec<OpenIn>,
    /// URLs that may name this same page: its head canonical and the end of an all-permanent
    /// redirect chain. The only alias candidates (`alias::canonical`); never stored.
    #[serde(skip)]
    pub alias_candidates: Vec<String>,
    /// The HTTP status of an answer that failed (`status` is `Error`), so the queue records which
    /// kind of failure it was; never stored.
    #[serde(skip)]
    pub http_status: Option<u16>,
    /// Missing by its own content (a Pubky app's object without a card), not because its
    /// upstream failed to answer: confirmed at once, as it cannot flap. Never stored.
    #[serde(skip)]
    #[schema(ignore)]
    pub withheld: bool,
    /// When the card changes by itself (an incident expires, an auction ends): its next fetch
    /// comes no later (`queue::refresh_by`). Never stored.
    #[serde(skip)]
    #[schema(ignore)]
    pub refresh_by: Option<DateTime<Utc>>,
    /// The root an official API names for this object (a filing under its primary filer): the
    /// URI becomes an alias of it after the fetch (`alias::source`). Never stored.
    #[serde(skip)]
    pub source_root: Option<String>,
    /// A podcast show's latest episodes, from its feed (`episodes`): the save writes them to
    /// `podcast_episode`, never into the document. `None` leaves the stored list as it is.
    #[serde(skip)]
    #[schema(ignore)]
    pub episodes: Option<Vec<crate::episodes::Episode>>,
    pub source: String,
    /// The also-on sources whose findings the queue wrote into this card itself, each with a
    /// fingerprint of every value it wrote (`titles`: the name and description; `films`,
    /// `podcasts` or an artist's source: the type; `podcasts` and `itunes`: the stream;
    /// `media::type_card`, `music::type_card`). A
    /// 304 reuses the card only while each of them is on and finds exactly that now
    /// (`media::outdated`); else the page is read again in full. Read from the list of sources
    /// an earlier build stored, as unknown fingerprints.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "hinted_by"
    )]
    #[schema(ignore)]
    pub hinted_by: BTreeMap<String, String>,
    #[serde(default)]
    pub fetched_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ttl_secs: u64,
    pub status: Status,
}
/// `ResourceMeta::hinted_by`, as stored now (source to fingerprint) or as a list of sources (an
/// earlier build's), whose fingerprints are unknown: such a card is read again at its next 304.
fn hinted_by<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Fingerprints(BTreeMap<String, String>),
        Sources(Vec<String>),
    }
    Ok(match Stored::deserialize(d)? {
        Stored::Fingerprints(map) => map,
        Stored::Sources(list) => list.into_iter().map(|s| (s, String::new())).collect(),
    })
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
impl ResourceMeta {
    pub fn new(uri: &str, source: &str, ttl_secs: u64) -> Self {
        Self {
            type_name: "WebPage".into(),
            type_path: type_path("WebPage"),
            url: uri.into(),
            site: url::Url::parse(uri)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default(),
            // The service the link is on, by its URL alone: what lets it take a service's place on a
            // card, never what the page says about itself.
            service: crate::api::service_site(uri),
            source: source.into(),
            fetched_at: Utc::now().timestamp_millis(),
            ttl_secs,
            status: Status::Ok,
            ..Default::default()
        }
    }
    pub fn set_type(&mut self, name: &str) {
        let path = type_path(name);
        if let Some(leaf) = path.last() {
            self.type_name = leaf.clone();
            self.type_path = path;
        }
    }
    pub fn validate(&self, allowed_attributes: &[&str]) -> Result<()> {
        if self.attributes.len() > ATTRIBUTE_CAP {
            bail!("document exceeds 32 attributes");
        }
        if self
            .attributes
            .keys()
            .any(|k| !allowed_attributes.contains(&k.as_str()))
        {
            bail!("attribute is not allowlisted");
        }
        if self.type_path != type_path(&self.type_name) || self.type_path.is_empty() {
            bail!("invalid schema.org type path");
        }
        if let Some(p) = &self.location {
            if !p.lat.is_finite()
                || !p.lon.is_finite()
                || !(-90.0..=90.0).contains(&p.lat)
                || !(-180.0..=180.0).contains(&p.lon)
            {
                bail!("invalid point");
            }
        }
        // PostgreSQL jsonb adds spaces; leave enough room for its canonical representation.
        if serde_json::to_vec(self)?.len() > DOCUMENT_CAP
            || serde_json::to_string_pretty(self)?.len() > DOCUMENT_CAP
        {
            bail!("document exceeds 8 KB");
        }
        Ok(())
    }
    pub fn bounded(mut self, allowed_attributes: &[&str]) -> Result<Self> {
        self.event.bound();
        self.group_keys.retain(crate::group_keys::valid_stored);
        self.group_keys.sort_by(|a, b| a.key.cmp(&b.key));
        self.group_keys.dedup_by(|a, b| a.key == b.key);
        self.group_keys.truncate(16);
        self.attributes
            .retain(|k, _| allowed_attributes.contains(&k.as_str()));
        while self.attributes.len() > ATTRIBUTE_CAP {
            self.attributes.pop_last();
        }
        for v in self.attributes.values_mut() {
            truncate(v, 256);
        }
        for (value, max) in [
            (&mut self.name, 512),
            (&mut self.description, 2048),
            (&mut self.category, 128),
        ] {
            if let Some(s) = value {
                truncate(s, max);
            }
        }
        for value in self.address.values_mut() {
            truncate(value, 256);
        }
        if self.image.as_ref().is_some_and(|s| s.len() > 2048) {
            self.image = None;
        }
        self.stream = self.stream.take().and_then(|s| {
            Stream::public(
                &s.url,
                &s.media_type,
                s.duration_s.map(u64::from),
                s.preview,
            )
        });
        self.aliases.truncate(4);
        self.open_in.retain(|o| {
            o.url.len() <= 2048
                && o.client.len() <= 64
                && url::Url::parse(&o.url).is_ok_and(|u| u.scheme() == "https")
        });
        self.open_in.truncate(4);
        while self.validate(allowed_attributes).is_err() {
            if self.attributes.pop_last().is_some()
                || self.aliases.pop().is_some()
                || self.open_in.pop().is_some()
                || self.group_keys.pop().is_some()
                || self.event.shrink_performers()
            {
                continue;
            }
            if self.description.take().is_some()
                || self.image.take().is_some()
                || self.stream.take().is_some()
            {
                continue;
            }
            self.validate(allowed_attributes)?;
        }
        Ok(self)
    }
}
pub fn truncate(s: &mut String, max: usize) {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
}
pub fn type_path(name: &str) -> Vec<String> {
    static TYPES: OnceLock<BTreeMap<String, Vec<String>>> = OnceLock::new();
    let types = TYPES.get_or_init(|| {
        let mut types: BTreeMap<String, Vec<String>> =
            serde_json::from_str(include_str!("../../../data/schemaorg-types.json"))
                .expect("pinned vocabulary");
        // Pinakes's own types for objects schema.org has no class for (not schema.org).
        let local: BTreeMap<String, Vec<String>> =
            serde_json::from_str(include_str!("../../../data/local-types.json"))
                .expect("local types");
        for (name, parents) in local {
            assert!(
                types.insert(name, parents).is_none(),
                "a local type shadows schema.org"
            );
        }
        types
    });
    let name = name
        .strip_prefix("https://schema.org/")
        .or_else(|| name.strip_prefix("http://schema.org/"))
        .or_else(|| name.strip_prefix("schema:"))
        .unwrap_or(name);
    fn visit(
        name: &str,
        types: &BTreeMap<String, Vec<String>>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(name.into()) {
            return;
        }
        if let Some(parents) = types.get(name) {
            for p in parents {
                visit(p, types, seen, out);
            }
            out.push(name.into());
        }
    }
    let mut out = vec![];
    visit(name, types, &mut BTreeSet::new(), &mut out);
    out
}
pub fn identity(uri: &str) -> Result<(String, String, String)> {
    if uri.is_empty() || uri.len() > 2048 {
        bail!("URI must be 1–2048 bytes");
    }
    let (uri, scheme) = crate::identity::normalize_uri(uri).map_err(anyhow::Error::msg)?;
    let id = crate::identity::resource_id(&uri);
    Ok((id, uri, scheme))
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A stream is public, direct audio or video: an episode's enclosure (a tracking redirect is
    /// the publisher's own link) or iTunes' preview, never a signed, expiring or private link.
    #[test]
    fn a_stream_is_only_a_public_direct_link() {
        let enclosure = "https://www.podtrac.com/pts/redirect.mp3/dovetail.prxu.org/7057/141e0c43-f935-41de-be94-708448052c4d/darknet-diaries-ep180-conti.mp3";
        assert_eq!(
            Stream::public(enclosure, "Audio/MPEG; charset=binary", Some(3557), false),
            Some(Stream {
                url: enclosure.into(),
                media_type: "audio/mpeg".into(),
                duration_s: Some(3557),
                preview: false,
            })
        );
        // iTunes' preview of a track; the feed's own query (`aid`, `feed`) is no signature.
        for url in [
            "https://audio-ssl.itunes.apple.com/itunes-assets/AudioPreview221/v4/8a/1f/43/8a1f43b0-d1e3-2c71-a5d8-1c1f5c7e1c4f/mzaf_1.plus.aac.p.m4a",
            "https://dts.podtrac.com/redirect.mp3/nyt.simplecastaudio.com/a/default.mp3?aid=rss_feed&feed=Sl5CSM3S",
            "http://feeds.example.org/ep 1.mp3",
        ] {
            assert!(Stream::public(url, "audio/mp4", None, true).is_some(), "{url}");
        }
        // A length of 0 is unknown.
        assert_eq!(
            Stream::public(enclosure, "video/mp4", Some(0), false)
                .unwrap()
                .duration_s,
            None
        );
        for (url, ty) in [
            // Deezer's recorded preview: signed, and gone within the day.
            ("https://cdnt-preview.dzcdn.net/api/1/1/f/2/c/0/f2c895eba263c4bd0e1fa4bce235854c.mp3?hdnea=exp=1792455917~acl=/api/1/1/f/2/c/0/f2c895eba263c4bd0e1fa4bce235854c.mp3*~data=user_id=0,application_id=42~hmac=00", "audio/mpeg"),
            ("https://bucket.s3.amazonaws.com/ep.mp3?X-Amz-Signature=abc&X-Amz-Expires=600", "audio/mpeg"),
            ("https://cdn.example.org/ep.mp3?Expires=1792455917&Key-Pair-Id=K1&Policy=p", "audio/mpeg"),
            ("https://cdn.example.org/ep.mp3?access_token=abc", "audio/mpeg"),
            ("https://cdn.example.org/ep.mp3?__token__=st=1~hmac=2", "audio/mpeg"),
            ("https://user:pass@cdn.example.org/ep.mp3", "audio/mpeg"),
            ("ftp://cdn.example.org/ep.mp3", "audio/mpeg"),
            ("https://127.0.0.1/ep.mp3", "audio/mpeg"),
            ("https://[::1]/ep.mp3", "audio/mpeg"),
            ("https://localhost/ep.mp3", "audio/mpeg"),
            ("https://media/ep.mp3", "audio/mpeg"),
            ("https://cdn.example.org/ep.mp3", "text/html"),
            ("https://cdn.example.org/ep.mp3", "audio"),
            ("https://cdn.example.org/ep.mp3", "audio/<x>"),
        ] {
            assert_eq!(Stream::public(url, ty, None, false), None, "{url} {ty}");
        }
        let long = format!("https://cdn.example.org/{}.mp3", "a".repeat(2048));
        assert_eq!(Stream::public(&long, "audio/mpeg", None, false), None);
        // A type of at most 127 bytes, so a stored stream stays small (review of 49666ef1).
        let typed = |sub: usize| {
            Stream::public(
                enclosure,
                &format!("audio/{}", "x".repeat(sub)),
                None,
                false,
            )
        };
        assert_eq!(typed(121).unwrap().media_type.len(), 127);
        assert_eq!(typed(122), None);
        assert_eq!(typed(5000), None);
        // Long parameters are dropped, not counted.
        assert_eq!(
            Stream::public(
                enclosure,
                &format!("audio/mpeg; x={}", "y".repeat(5000)),
                None,
                false
            )
            .unwrap()
            .media_type,
            "audio/mpeg"
        );
        assert_eq!(
            Stream::public(enclosure, "audio/mpeg", Some(8 * 86400), false),
            None
        );
        // A stored document's stream is held to the same rules.
        let mut doc = ResourceMeta::new("https://podcast.example/feed?guid=1", "media", 60);
        doc.stream = Some(Stream {
            url: "https://cdn.example.org/ep.mp3?token=1".into(),
            media_type: "audio/mpeg".into(),
            duration_s: None,
            preview: false,
        });
        assert_eq!(doc.clone().bounded(&[]).unwrap().stream, None);
        doc.stream = Some(Stream {
            url: enclosure.into(),
            media_type: format!("audio/{}", "x".repeat(5000)),
            duration_s: None,
            preview: false,
        });
        assert_eq!(doc.bounded(&[]).unwrap().stream, None);
    }
    #[test]
    fn identity_normalizes_and_hashes() {
        let (id, uri, scheme) =
            identity("HTTPS://user:pass@Example.COM:443/path?q=1#frag").unwrap();
        assert_eq!(id, "2397d03755c83364010ee03600730121");
        assert_eq!(uri, "https://example.com/path?q=1");
        assert_eq!(scheme, "https");
        assert!(identity("no-scheme").is_err());
        assert!(identity(&"x".repeat(2049)).is_err());
    }
    #[test]
    fn hierarchy_keeps_multiple_parents_and_uncommon_types() {
        for (child, parent) in [
            ("CafeOrCoffeeShop", "Place"),
            ("CafeOrCoffeeShop", "Organization"),
            ("MusicEvent", "Event"),
            ("ScholarlyArticle", "CreativeWork"),
        ] {
            let path = type_path(child);
            assert!(path.contains(&parent.into()));
            assert_eq!(path.last().unwrap(), child);
            assert_eq!(path.iter().collect::<BTreeSet<_>>().len(), path.len());
        }
        assert_eq!(
            type_path("https://schema.org/MusicEvent"),
            type_path("MusicEvent")
        );
        assert!(type_path("InventedType").is_empty());
        // Local types (data/local-types.json) sit under Thing.
        assert_eq!(type_path("BitcoinBlock"), ["Thing", "BitcoinBlock"]);
        assert_eq!(type_path("LightningChannel"), ["Thing", "LightningChannel"]);
    }
    #[test]
    fn documents_are_bounded() {
        let mut doc = ResourceMeta::new("https://example.org", "web", 100);
        doc.description = Some("é".repeat(9000));
        assert!(doc.validate(&[]).is_err());
        let doc = doc.bounded(&[]).unwrap();
        assert!(serde_json::to_vec(&doc).unwrap().len() < DOCUMENT_CAP);
        // The worst line-up (sixteen performers with eight long links each) fits the document:
        // performers are trimmed, links first, within their own share.
        let mut doc = ResourceMeta::new("https://example.org/festival", "web", 3600);
        doc.set_type("MusicEvent");
        doc.event.performers = (0..16)
            .map(|n| crate::event::Performer {
                name: format!("Performer {n}"),
                links: (0..8)
                    .map(|l| format!("https://www.songkick.com/artists/{n}{l}-performer-with-a-long-slug-{l}"))
                    .collect(),
            })
            .collect();
        let doc = doc.bounded(&[]).unwrap();
        assert!(serde_json::to_vec(&doc).unwrap().len() <= DOCUMENT_CAP);
        assert!(!doc.event.performers.is_empty());
        assert!(
            serde_json::to_vec(&doc.event.performers).unwrap().len()
                <= crate::event::PERFORMERS_CAP
        );
        let mut doc = doc.clone();
        doc.attributes = (0..33).map(|n| (format!("x{n}"), "v".into())).collect();
        let keys: Vec<_> = doc.attributes.keys().cloned().collect();
        let allowed: Vec<_> = keys.iter().map(String::as_str).collect();
        assert!(doc.validate(&allowed).is_err());
        assert_eq!(doc.bounded(&allowed).unwrap().attributes.len(), 32);
        let mut doc = ResourceMeta::new("https://example.org", "web", 100);
        doc.attributes.insert("secret".into(), "x".into());
        assert!(doc.validate(&[]).is_err());
    }
}
