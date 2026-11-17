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
/// The most bytes of an attribute's value kept (`bounded` cuts a longer one).
pub const ATTRIBUTE_VALUE_CAP: usize = 256;
/// The longest description a document keeps (bytes).
pub const DESCRIPTION_CAP: usize = 2048;
/// The most credits a document keeps: one per enriched field.
pub const CREDITS_CAP: usize = 4;
/// The most entries a document's `related` list keeps.
pub const RELATED_CAP: usize = 32;

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
/// One object a card points at (docs/api.md, Related objects): on a Bitcoin or Lightning card,
/// the block a transaction is in, its inputs and outputs, a block's neighbours, an address's
/// latest transactions, a node's channels and peers, a channel's nodes and transactions; on a
/// podcast episode's card, its show.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct Related {
    /// What it is to this card: `block`, `input`, `output`, `previous`, `next`, `coinbase`,
    /// `tx`, `channel`, `peer`, `node`, `funding_tx`, `closing_tx` or `show`.
    pub rel: String,
    /// Its card's URI, a root `POST /v1/resolve` reads (`btc:tx/…`, `btc:address/…`,
    /// `ln:node/…`, a podcast show's feed or Podcast Index page); null for an input or output
    /// without an address, which `name` labels (`OP_RETURN`, `coinbase`).
    #[serde(default)]
    pub uri: Option<String>,
    /// Plain text: a block's "Block 975,251", a node's alias, a channel's peer's alias, a show's
    /// title from its feed, or the label of an entry without a `uri`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// In satoshis: an input's or output's value, a channel's capacity, a block's reward on its
    /// coinbase transaction, or what a transaction moved into (positive) or out of (negative)
    /// an address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_sat: Option<i64>,
    /// When it happened: the block time of a transaction or of a transaction's block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}
impl Related {
    pub fn new(rel: &str, uri: Option<String>) -> Self {
        Self {
            rel: rel.into(),
            uri,
            name: None,
            amount_sat: None,
            at: None,
        }
    }
    /// The entry within its bounds: a `rel` of 1–32 lower-case letters and underscores, a `uri`
    /// of at most 256 bytes without spaces, a `name` of at most 128 bytes. `None` when one of
    /// them is out of bounds, or when it has neither a `uri` nor a `name`.
    fn bounded(mut self) -> Option<Self> {
        let rel_ok = (1..=32).contains(&self.rel.len())
            && self
                .rel
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_');
        let uri_ok = self.uri.as_deref().is_none_or(|u| {
            !u.is_empty() && u.len() <= 256 && !u.chars().any(|c| c.is_whitespace())
        });
        self.name = self
            .name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
        if let Some(name) = &mut self.name {
            truncate(name, 128);
        }
        (rel_ok && uri_ok && (self.uri.is_some() || self.name.is_some())).then_some(self)
    }
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
        if !public_host(&parsed) || parsed.query_pairs().any(|(name, _)| signed(&name)) {
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
/// Whether `url` is an `http` or `https` link on a named public host, with no user info: a
/// domain name with a dot, not `localhost` or a name under it. An IP address, `localhost` or a
/// name with no dot is private to whoever reads the link (their own machine or network), so a
/// stream or a card's image is never one. A public name that resolves to such an address
/// (wildcard DNS) is not caught here: that needs the name resolved.
pub fn public_host(url: &url::Url) -> bool {
    matches!(url.scheme(), "https" | "http")
        && url.username().is_empty()
        && url.password().is_none()
        && match url.host() {
            Some(url::Host::Domain(host)) => {
                let host = host.trim_end_matches('.');
                host.contains('.') && host != "localhost" && !host.ends_with(".localhost")
            }
            _ => false,
        }
}
/// Whether `s` is a web link (`http` or `https`) that is not on a named public host
/// ([`public_host`]): an image clients would load from someone's own machine or network. A link
/// of another scheme (`pubky://`) is no web link here.
pub fn private_web_link(s: &str) -> bool {
    url::Url::parse(s)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && !public_host(&url))
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
/// The card field a credit is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CreditField {
    Image,
    Description,
}
/// The service an enriched value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CreditProvider {
    /// A Wikipedia article's summary.
    Wikipedia,
    /// A file on Wikimedia Commons.
    WikimediaCommons,
    /// A Wikidata item's English description.
    Wikidata,
    /// A release's front cover in the Cover Art Archive (MusicBrainz's).
    CoverArtArchive,
    /// A film's or series' poster or overview from TMDB (The Movie Database), by the title's
    /// exact IDs. A client that shows it shows TMDB's logo and notice (`/v1/info`
    /// `attributions`).
    Tmdb,
}
impl CreditProvider {
    /// Whether its values carry no licence: a cover from the Cover Art Archive (cover art is its
    /// label's) and TMDB's poster or overview (credited to TMDB).
    pub fn unlicensed(self) -> bool {
        matches!(self, Self::CoverArtArchive | Self::Tmdb)
    }
    /// Whether its values are written from an also-on source's facts when a card is typed
    /// (`also_on::Source::fill_card`), not by the card's own reading.
    pub fn from_facts(self) -> bool {
        Self::FROM_FACTS.contains(&self)
    }
    /// The providers whose values are written from facts ([`Self::from_facts`]).
    pub const FROM_FACTS: &'static [Self] = &[Self::Tmdb];
    /// The provider's name as stored and served (`tmdb`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Wikipedia => "wikipedia",
            Self::WikimediaCommons => "wikimedia_commons",
            Self::Wikidata => "wikidata",
            Self::CoverArtArchive => "cover_art_archive",
            Self::Tmdb => "tmdb",
        }
    }
    /// What a client showing the provider's values shows (`/v1/info` `attributions`): TMDB's notice, verbatim, its logo and its site.
    pub fn attribution(self) -> Option<Attribution> {
        match self {
            Self::Tmdb => Some(Attribution {
                notice: "This product uses the TMDB API but is not endorsed or certified by TMDB.",
                logo: "https://www.themoviedb.org/assets/2/v4/logos/v2/blue_short-8e7b30f73a4020692ccca9c88bafe5dcb6f8a62a4c6bc55cd9ba82bb2cd95f6c.svg",
                url: "https://www.themoviedb.org",
            }),
            _ => None,
        }
    }
    /// The longest a change line that holds one of the provider's values is kept (days),
    /// whatever `limits.change_retention_days` says and whether or not its source is on now
    /// (`changes::retain`): TMDB's six months, from a value up to 60 days old.
    pub fn change_days_cap(self) -> Option<u32> {
        match self {
            Self::Tmdb => Some(120),
            _ => None,
        }
    }
    /// The card types a provider written from facts fills: the only cards that can hold its
    /// values (`also_on::refill` reads those alone, by the `type_path` index).
    pub fn filled_types(self) -> &'static [&'static str] {
        match self {
            Self::Tmdb => &["Movie", "TVSeries"],
            _ => &[],
        }
    }
}
/// A provider's attribution ([`CreditProvider::attribution`]): the notice and logo a client
/// showing its values shows, and its site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribution {
    pub notice: &'static str,
    pub logo: &'static str,
    pub url: &'static str,
}
/// Where one of the card's `image` or `description` came from when Pinakes added it from another
/// source than the link itself, and the licence it is used under (docs/api.md, Credits). Show it
/// with the value: the author, the licence (linked to `license_url`) and the source page.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct Credit {
    /// The card field the value is in.
    pub field: CreditField,
    pub provider: CreditProvider,
    /// The page the value came from: the Wikipedia article, the file's page on Commons, the
    /// item's page on Wikidata, the cover art page of the release on MusicBrainz, or the title's
    /// page on TMDB. An `https:` URL of at most 1,024 bytes.
    pub source_url: String,
    /// Who made it, plain text of at most 256 bytes: "Wikipedia contributors" for an article, the
    /// file's author for an image. Null for a public-domain or CC0 file that names none, for a
    /// Wikidata description (CC0), for a cover from the Cover Art Archive and for TMDB's values.
    pub author: Option<String>,
    /// An SPDX licence ID (`CC-BY-SA-4.0`, `CC-BY-2.0`, `CC0-1.0`, `GPL-3.0-only`,
    /// `LGPL-2.1-or-later`), `public-domain`, or `Attribution` (Commons' licence of that name,
    /// which has no SPDX ID). Null for a cover from the Cover Art Archive and for TMDB's poster
    /// or overview, which carry no licence: cover art is its label's, shown as a service shows an
    /// album's cover, and TMDB's values carry none either (`license_url` is null too).
    // No licence is the empty string here, and only such a credit holds it (`bounded`).
    #[serde(
        serialize_with = "license_or_null",
        deserialize_with = "license_from_null"
    )]
    #[schema(value_type = Option<String>)]
    pub license: String,
    /// The licence's text: an `https:` URL; null when it has none (public domain, a cover from
    /// the Cover Art Archive, TMDB's values).
    pub license_url: Option<String>,
    /// The language of a text (`en`, or another Wikipedia's language code, `de`); null for an
    /// image.
    pub lang: Option<String>,
    /// The text is a part of a longer source: a summary is an article's first paragraphs, and a
    /// long one is cut at a sentence or a word. A Wikidata description is whole.
    pub excerpted: bool,
    /// The value was changed beyond cutting it.
    pub modified: bool,
    /// The link of the entity the value is about, when that is not the card's own: a recording's
    /// description taken from its work's item names the work (`https://musicbrainz.org/work/…`).
    pub subject: Option<String>,
}
/// A credit's licence as JSON: null for none (the empty string).
fn license_or_null<S: serde::Serializer>(license: &str, s: S) -> Result<S::Ok, S::Error> {
    if license.is_empty() {
        s.serialize_none()
    } else {
        s.serialize_str(license)
    }
}
fn license_from_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}
impl Credit {
    /// The credit within its bounds: every URL `https:` and at most 1,024 bytes, the author at
    /// most 256 bytes (cut at a character), the licence ID and language at most 64. `None` when
    /// a URL or ID is out of bounds, when a value of a provider without licences
    /// ([`CreditProvider::unlicensed`]) names a licence or any other value names none, when a
    /// URL is a link that carries a credential
    /// (`capability::names_credential`: the store would drop that part of the credit,
    /// `capability::strip_document`, and leave its value uncredited), or when the author, free
    /// text, holds such a link anywhere (`capability::contains_credential`: `Jane
    /// https://…?api_key=…`, which no store rule removes). The value is then not taken at all.
    pub fn bounded(mut self) -> Option<Self> {
        let https = |s: &str| {
            s.len() <= 1024
                && url::Url::parse(s).is_ok_and(|u| u.scheme() == "https")
                && !crate::capability::names_credential(s)
        };
        let id = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        };
        if let Some(author) = &mut self.author {
            truncate(author, 256);
        }
        self.author = self.author.filter(|a| !a.trim().is_empty());
        // A cover from the Cover Art Archive and TMDB's values have no licence; every other
        // value has one.
        let licensed = if self.provider.unlicensed() {
            self.license.is_empty() && self.license_url.is_none()
        } else {
            id(&self.license)
        };
        (https(&self.source_url)
            && !self
                .author
                .as_deref()
                .is_some_and(crate::capability::contains_credential)
            && self.license_url.as_deref().is_none_or(https)
            && self.subject.as_deref().is_none_or(https)
            && licensed
            && self.lang.as_deref().is_none_or(id))
        .then_some(self)
    }
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
    /// The frame a client may load to show the link ([`crate::embed::Embed`]) that the page
    /// itself names: its oEmbed answer's `<iframe>` or its own player `<meta>`. An embed that
    /// follows from the link alone (`embed::link`) is added when the card is served, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed: Option<crate::embed::Embed>,
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
    /// Where `image` and `description` came from when an enrichment added them (`Credit`, at
    /// most one per field): stored with the values, and gone with them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credits: Vec<Credit>,
    /// The enriched fields this reading could not read, for a transient failure: the queue keeps
    /// their stored values and credits (`keep_enriched`). Never stored.
    #[serde(skip)]
    #[schema(ignore)]
    pub unread_enriched: Vec<CreditField>,
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
    /// The objects the card points at ([`Related`]), at most [`RELATED_CAP`]: a chain card's
    /// block, inputs and outputs, neighbours, transactions, channels and peers; an episode's
    /// show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<Related>,
    /// How many entries of a `rel` there are when `related` holds fewer of them (a transaction's
    /// 400 inputs, 10 of them listed): only for a `rel` the list holds and cuts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub related_total: BTreeMap<String, u64>,
    /// URLs that may name this same page: its head canonical and the end of an all-permanent
    /// redirect chain. The only alias candidates (`alias::canonical`); never stored.
    #[serde(skip)]
    pub alias_candidates: Vec<String>,
    /// The other spelling of an event page on a platform that gives each event two, named by the
    /// page's own head canonical in a fetch that ended at the page itself
    /// (`alias::event_spelling`); never stored.
    #[serde(skip)]
    #[schema(ignore)]
    pub event_spellings: Vec<String>,
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
    /// A podcast episode's page as the feed item this card was read from names it (its link,
    /// without tracking parameters): the page becomes an alias of this card after the fetch when
    /// the feed's own publisher publishes it (`episodes::join_page`). Never stored.
    #[serde(skip)]
    #[schema(ignore)]
    pub episode_page: Option<String>,
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
            truncate(v, ATTRIBUTE_VALUE_CAP);
        }
        for (value, max) in [
            (&mut self.name, 512),
            (&mut self.description, DESCRIPTION_CAP),
            (&mut self.category, 128),
        ] {
            if let Some(s) = value {
                truncate(s, max);
            }
        }
        for value in self.address.values_mut() {
            truncate(value, 256);
        }
        // Clients load an image from where it is named: a web link's host is a named public one.
        if self
            .image
            .as_ref()
            .is_some_and(|s| s.len() > 2048 || private_web_link(s))
        {
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
        self.embed = self.embed.take().and_then(crate::embed::Embed::bounded);
        self.credits = std::mem::take(&mut self.credits)
            .into_iter()
            .filter_map(Credit::bounded)
            .collect();
        self.credited();
        self.aliases.truncate(4);
        self.open_in.retain(|o| {
            o.url.len() <= 2048
                && o.client.len() <= 64
                && url::Url::parse(&o.url).is_ok_and(|u| u.scheme() == "https")
        });
        self.open_in.truncate(4);
        self.related = std::mem::take(&mut self.related)
            .into_iter()
            .filter_map(Related::bounded)
            .collect();
        while self.related.len() > RELATED_CAP {
            self.cut_related();
        }
        // A total stays only for a `rel` the list holds and cuts.
        let related = &self.related;
        self.related_total.retain(|rel, total| {
            let listed = related.iter().filter(|r| &r.rel == rel).count() as u64;
            listed > 0 && *total > listed
        });
        while self.validate(allowed_attributes).is_err() {
            if self.shed() {
                continue;
            }
            if self.embed.take().is_some()
                || self.description.take().is_some()
                || self.image.take().is_some()
                || self.stream.take().is_some()
            {
                self.credited();
                continue;
            }
            self.validate(allowed_attributes)?;
        }
        Ok(self)
    }
    /// Drops the last entry of `related`, counting it in `related_total`: the list of its `rel`
    /// is cut there. A `rel` it leaves without entries keeps no total (a total is only for a
    /// `rel` the list holds). False when the list is empty.
    fn cut_related(&mut self) -> bool {
        let Some(last) = self.related.pop() else {
            return false;
        };
        let left = self.related.iter().filter(|r| r.rel == last.rel).count() as u64;
        if left == 0 {
            self.related_total.remove(&last.rel);
        } else {
            let total = self.related_total.entry(last.rel).or_insert(left + 1);
            *total = (*total).max(left + 1);
        }
        true
    }
    /// Drops one of the parts a document too large gives up first, before its description and
    /// image: an entry of `related` (the last), an attribute (the last), an alias, a client, a
    /// group key, a performer. False when none is left.
    fn shed(&mut self) -> bool {
        self.cut_related()
            || self.attributes.pop_last().is_some()
            || self.aliases.pop().is_some()
            || self.open_in.pop().is_some()
            || self.group_keys.pop().is_some()
            || self.event.shrink_performers()
    }
    /// Whether the document fits [`DOCUMENT_CAP`], in both of its stored forms.
    fn fits(&self) -> bool {
        serde_json::to_vec(self).is_ok_and(|v| v.len() <= DOCUMENT_CAP)
            && serde_json::to_string_pretty(self).is_ok_and(|v| v.len() <= DOCUMENT_CAP)
    }
    /// Keeps the credits of the fields that hold a value, one per field, at most
    /// [`CREDITS_CAP`].
    fn credited(&mut self) {
        let mut seen = vec![];
        let (image, description) = (self.image.is_some(), self.description.is_some());
        self.credits.retain(|c| {
            let present = match c.field {
                CreditField::Image => image,
                CreditField::Description => description,
            };
            present && !seen.contains(&c.field) && {
                seen.push(c.field);
                true
            }
        });
        self.credits.truncate(CREDITS_CAP);
    }
    /// Takes from `stored` (the accepted document) the enriched fields this reading left unread
    /// (`unread_enriched`): each one's stored value and credit, when the stored document has
    /// both. A field the stored document holds without a credit was not enriched: this reading's
    /// own value stays. When this reading with the stored values would no longer fit
    /// ([`DOCUMENT_CAP`]), it gives up what a reading too large gives up first ([`Self::shed`]:
    /// the last attributes, then aliases, …), as `bounded` does for a value read now: a transient
    /// failure never loses a stored value and its credit for fresh data. Should even that not
    /// fit, the stored document stays whole, as for an unchanged page.
    ///
    /// The early read again (`refresh_by`) waits at least twice as long as since the stored
    /// reading, so a failure that lasts is asked about less and less often, up to the card's own
    /// refresh time; a card read in full a refresh time ago is not read again sooner.
    pub fn keep_enriched(&mut self, stored: &ResourceMeta) {
        if let Some(by) = self.refresh_by {
            let now = Utc::now();
            let since = (now.timestamp_millis() - stored.fetched_at).max(0) / 1000;
            let later = now + chrono::Duration::seconds(since.saturating_mul(2));
            self.refresh_by = Some(by.max(later));
        }
        let mut kept = self.clone();
        let mut took = false;
        for field in std::mem::take(&mut kept.unread_enriched) {
            // A value an also-on source wrote from its facts (`also_on::Source::fill_card`) is
            // that source's: its facts keep it or not, never a reading that left the field unread.
            let Some(credit) = stored
                .credits
                .iter()
                .find(|c| c.field == field && !c.provider.from_facts())
            else {
                continue;
            };
            let value = match field {
                CreditField::Image => (&mut kept.image, &stored.image),
                CreditField::Description => (&mut kept.description, &stored.description),
            };
            let Some(stored_value) = value.1 else {
                continue;
            };
            *value.0 = Some(stored_value.clone());
            kept.credits.retain(|c| c.field != field);
            kept.credits.push(credit.clone());
            took = true;
        }
        kept.credits.sort_by_key(|c| c.field);
        while took && !kept.fits() && kept.shed() {}
        if kept.fits() || !took {
            *self = kept;
        } else {
            // The stored document, accepted as it is (as `queue` takes an unchanged page's), with
            // this reading's time and early read again.
            let (fetched_at, refresh_by) = (self.fetched_at, self.refresh_by);
            *self = stored.clone();
            self.fetched_at = fetched_at;
            self.refresh_by = refresh_by;
            self.unread_enriched.clear();
        }
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
    /// A provider's name is its stored and served form: the SQL that finds its credits by name
    /// (`changes::retain`, `api::info`) matches what is stored.
    #[test]
    fn a_provider_s_name_is_its_serialized_form() {
        for p in [
            CreditProvider::Wikipedia,
            CreditProvider::WikimediaCommons,
            CreditProvider::Wikidata,
            CreditProvider::CoverArtArchive,
            CreditProvider::Tmdb,
        ] {
            assert_eq!(serde_json::to_value(p).unwrap(), p.name());
        }
        assert_eq!(CreditProvider::Tmdb.change_days_cap(), Some(120));
        assert!(CreditProvider::FROM_FACTS
            .iter()
            .all(|p| p.attribution().is_some() && !p.filled_types().is_empty()));
    }
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
        // A type of at most 127 bytes, so a stored stream stays small.
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
    fn credit(field: CreditField, source: &str) -> Credit {
        Credit {
            field,
            provider: CreditProvider::Wikipedia,
            source_url: source.into(),
            author: Some("Wikipedia contributors".into()),
            license: "CC-BY-SA-4.0".into(),
            license_url: Some("https://creativecommons.org/licenses/by-sa/4.0/".into()),
            lang: Some("en".into()),
            excerpted: true,
            modified: false,
            subject: None,
        }
    }
    /// A cover from the Cover Art Archive has no licence (JSON null) and no licence URL; every
    /// other credit has a licence, and a cover's credit has none.
    #[test]
    fn a_cover_credit_has_no_licence() {
        let cover = Credit {
            field: CreditField::Image,
            provider: CreditProvider::CoverArtArchive,
            source_url:
                "https://musicbrainz.org/release/e7ba3cb7-a074-45ee-870f-3baeb6d3e8bf/cover-art"
                    .into(),
            author: None,
            license: String::new(),
            license_url: None,
            lang: None,
            excerpted: false,
            modified: false,
            subject: None,
        };
        let json = serde_json::to_value(cover.clone().bounded().unwrap()).unwrap();
        assert_eq!(json["provider"], "cover_art_archive");
        assert_eq!(
            (&json["license"], &json["license_url"]),
            (&serde_json::Value::Null, &serde_json::Value::Null)
        );
        let back: Credit = serde_json::from_value(json).unwrap();
        assert_eq!(back, cover);
        // A stored licence reads back as it was.
        let article = credit(CreditField::Description, "https://en.wikipedia.org/wiki/A");
        let back: Credit = serde_json::from_value(serde_json::to_value(&article).unwrap()).unwrap();
        assert_eq!(back.license, "CC-BY-SA-4.0");
        for bad in [
            Credit {
                license: "CC-BY-4.0".into(),
                ..cover.clone()
            },
            Credit {
                license_url: Some("https://creativecommons.org/licenses/by/4.0/".into()),
                ..cover.clone()
            },
            Credit {
                license: String::new(),
                ..article
            },
        ] {
            assert_eq!(bad.clone().bounded(), None, "{bad:?}");
        }
    }
    /// A credit is stored only with its field's value, once per field, within its bounds.
    #[test]
    fn credits_go_with_their_values_and_stay_bounded() {
        let mut doc = ResourceMeta::new("https://musicbrainz.org/artist/x", "musicbrainz", 60);
        doc.description = Some("Text".into());
        doc.credits = vec![
            credit(
                CreditField::Image,
                "https://commons.wikimedia.org/wiki/File:A.jpg",
            ),
            credit(CreditField::Description, "https://en.wikipedia.org/wiki/A"),
            credit(CreditField::Description, "https://en.wikipedia.org/wiki/B"),
        ];
        let bounded = doc.clone().bounded(&[]).unwrap();
        assert_eq!(
            bounded.credits,
            [credit(
                CreditField::Description,
                "https://en.wikipedia.org/wiki/A"
            )]
        );
        let mut author = credit(CreditField::Description, "https://en.wikipedia.org/wiki/A");
        author.author = Some("é".repeat(200));
        assert_eq!(author.clone().bounded().unwrap().author.unwrap().len(), 256);
        for bad in [
            Credit {
                source_url: "http://en.wikipedia.org/wiki/A".into(),
                ..author.clone()
            },
            Credit {
                source_url: format!("https://en.wikipedia.org/wiki/{}", "a".repeat(1024)),
                ..author.clone()
            },
            Credit {
                license: "CC BY SA".into(),
                ..author.clone()
            },
            Credit {
                license_url: Some("javascript:alert(1)".into()),
                ..author.clone()
            },
            Credit {
                subject: Some("musicbrainz:work".into()),
                ..author.clone()
            },
            // The credential rule: no part of a credit is a link that carries one.
            Credit {
                source_url: "https://commons.wikimedia.org/wiki/File:A.jpg?token=x".into(),
                ..author.clone()
            },
            Credit {
                license_url: Some(
                    "https://creativecommons.org/licenses/by/4.0/#access_token=x".into(),
                ),
                ..author.clone()
            },
            Credit {
                author: Some("https://www.flickr.com/people/a?api_key=x".into()),
                ..author.clone()
            },
            // An author is free text: a link inside it counts too.
            Credit {
                author: Some("Jane https://example.org/p?api_key=SECRET".into()),
                ..author.clone()
            },
            Credit {
                author: Some("Photo: Jane (https://example.org/p?access_token=x)".into()),
                ..author.clone()
            },
        ] {
            assert_eq!(bad.clone().bounded(), None, "{bad:?}");
        }
        // Trimmed to fit, a document drops a credit with its value.
        doc.description = Some("é".repeat(1024));
        doc.attributes = (0..32)
            .map(|n| (format!("x{n}"), "v".repeat(256)))
            .collect();
        let keys: Vec<_> = doc.attributes.keys().cloned().collect();
        let allowed: Vec<_> = keys.iter().map(String::as_str).collect();
        let bounded = doc.bounded(&allowed).unwrap();
        assert!(bounded.description.is_some() || bounded.credits.is_empty());
    }
    /// An enriched field a reading left unread keeps the stored value and credit; one the stored
    /// card holds without a credit is no enrichment, and the reading's own value stays.
    #[test]
    fn an_unread_field_keeps_its_stored_value_and_credit() {
        let mut stored = ResourceMeta::new("https://musicbrainz.org/artist/x", "musicbrainz", 60);
        stored.image = Some("https://upload.wikimedia.org/a.jpg".into());
        stored.description = Some("Stored summary".into());
        stored.credits = vec![
            credit(
                CreditField::Image,
                "https://commons.wikimedia.org/wiki/File:A.jpg",
            ),
            credit(CreditField::Description, "https://en.wikipedia.org/wiki/A"),
        ];
        let mut fresh = ResourceMeta::new("https://musicbrainz.org/artist/x", "musicbrainz", 60);
        fresh.description = Some("Artist · 3:00".into());
        fresh.unread_enriched = vec![CreditField::Image, CreditField::Description];
        let mut kept = fresh.clone();
        kept.keep_enriched(&stored);
        assert_eq!(kept.image, stored.image);
        assert_eq!(kept.description, stored.description);
        assert_eq!(kept.credits, stored.credits);
        assert!(kept.unread_enriched.is_empty());
        // Stored without credits: nothing was enriched, nothing is kept.
        let mut plain = stored.clone();
        plain.credits.clear();
        let mut kept = fresh.clone();
        kept.keep_enriched(&plain);
        assert_eq!(
            (kept.image, kept.description.as_deref()),
            (None, Some("Artist · 3:00"))
        );
        // Only the unread field is taken: a description read now stays this reading's.
        let mut one = fresh.clone();
        one.unread_enriched = vec![CreditField::Image];
        one.keep_enriched(&stored);
        assert_eq!(one.image, stored.image);
        assert_eq!(one.description.as_deref(), Some("Artist · 3:00"));
        assert_eq!(one.credits, stored.credits[..1]);
    }
    /// A transient failure never loses a stored enrichment: when a large fresh reading with the
    /// stored values would pass the document cap, the reading gives up its last attributes, as
    /// it would for a value read now, and keeps the stored values and credits, with its own time
    /// and early read again.
    #[test]
    fn an_unread_field_past_the_cap_keeps_its_stored_value() {
        let uri = "https://musicbrainz.org/artist/x";
        let mut stored = ResourceMeta::new(uri, "musicbrainz", 60);
        stored.image = Some("https://upload.wikimedia.org/a.jpg".into());
        stored.description = Some("d".repeat(2048));
        stored.attributes = [("genre".to_owned(), "jazz".to_owned())].into();
        stored.credits = vec![
            credit(
                CreditField::Image,
                "https://commons.wikimedia.org/wiki/File:A.jpg",
            ),
            credit(CreditField::Description, "https://en.wikipedia.org/wiki/A"),
        ];
        stored.fetched_at -= 3_600_000;
        assert!(serde_json::to_string_pretty(&stored).unwrap().len() <= DOCUMENT_CAP);
        let mut full = ResourceMeta::new(uri, "musicbrainz", 60);
        full.attributes = (0..30)
            .map(|n| (format!("x{n}"), "v".repeat(200)))
            .collect();
        full.unread_enriched = vec![CreditField::Image, CreditField::Description];
        full.refresh_by = Some(Utc::now() + chrono::Duration::minutes(5));
        let fetched_at = full.fetched_at;
        full.keep_enriched(&stored);
        assert_eq!(full.image, stored.image);
        assert_eq!(full.description, stored.description);
        assert_eq!(full.credits, stored.credits);
        // This reading's facts, the first that fit; none of the stored ones.
        assert!(full.attributes.len() < 30 && full.attributes.contains_key("x0"));
        assert!(!full.attributes.contains_key("genre"));
        assert!(serde_json::to_string_pretty(&full).unwrap().len() <= DOCUMENT_CAP);
        assert_eq!(full.fetched_at, fetched_at);
        assert!(full.refresh_by.unwrap() >= Utc::now() + chrono::Duration::minutes(119));
        assert!(full.unread_enriched.is_empty());
        // Nothing stored to keep: the reading stays as it is.
        let mut plain = stored.clone();
        plain.credits.clear();
        let mut alone = ResourceMeta::new(uri, "musicbrainz", 60);
        alone.attributes = [("x0".to_owned(), "fresh".to_owned())].into();
        alone.unread_enriched = vec![CreditField::Image];
        alone.keep_enriched(&plain);
        assert_eq!(
            (alone.image, alone.attributes["x0"].as_str()),
            (None, "fresh")
        );
        assert!(alone.unread_enriched.is_empty());
    }
    /// A field unread again is asked about less and less often: at least twice as long as since
    /// the stored reading, never sooner than the reading asked.
    #[test]
    fn an_unread_field_backs_off() {
        let now = Utc::now();
        let mut stored = ResourceMeta::new("https://musicbrainz.org/artist/x", "musicbrainz", 60);
        let mut fresh = stored.clone();
        fresh.unread_enriched = vec![CreditField::Image];
        fresh.refresh_by = Some(now + chrono::Duration::hours(1));
        // Read just now: the reading's own hour.
        let mut soon = fresh.clone();
        soon.keep_enriched(&stored);
        let by = soon.refresh_by.unwrap();
        assert!(
            by >= now + chrono::Duration::minutes(59) && by <= now + chrono::Duration::minutes(61)
        );
        // Read three hours ago: six hours from now.
        stored.fetched_at = (now - chrono::Duration::hours(3)).timestamp_millis();
        let mut later = fresh.clone();
        later.keep_enriched(&stored);
        let by = later.refresh_by.unwrap();
        assert!(
            by >= now + chrono::Duration::minutes(359)
                && by <= now + chrono::Duration::minutes(361)
        );
        // No early read asked: none is added.
        let mut none = fresh.clone();
        none.refresh_by = None;
        none.keep_enriched(&stored);
        assert_eq!(none.refresh_by, None);
    }
    /// A recording's card with a 2,048-byte description, an image and both credits at generous
    /// lengths keeps them all within the document cap; one past it (every URL of both credits
    /// near its 1,024-byte bound) gives up an enriched field, its credit with it, and still saves.
    #[test]
    fn an_enriched_card_fits_the_document() {
        let card = |url: usize| {
            let mut doc = ResourceMeta::new(
                &format!("https://musicbrainz.org/recording/{}", "0".repeat(36)),
                "musicbrainz",
                86400,
            );
            doc.set_type("MusicRecording");
            doc.name = Some("n".repeat(512));
            doc.description = Some("d".repeat(DESCRIPTION_CAP));
            doc.image = Some(format!("https://upload.wikimedia.org/{}", "i".repeat(url)));
            doc.attributes.insert("artist".into(), "a".repeat(256));
            let long = |field| Credit {
                field,
                provider: CreditProvider::WikimediaCommons,
                source_url: format!("https://commons.wikimedia.org/wiki/{}", "s".repeat(url)),
                author: Some("a".repeat(256)),
                license: "CC-BY-SA-3.0-DE".into(),
                // Past the realistic: every URL of the credit near its bound.
                license_url: Some(format!(
                    "https://creativecommons.org/licenses/by-sa/3.0/de/{}",
                    "l".repeat(if url > 300 { url - 60 } else { 0 })
                )),
                lang: Some("en".into()),
                excerpted: true,
                modified: false,
                subject: Some(format!(
                    "https://musicbrainz.org/work/{}",
                    "w".repeat(if url > 300 { url } else { 36 })
                )),
            };
            doc.credits = vec![long(CreditField::Image), long(CreditField::Description)];
            doc
        };
        let doc = card(300);
        let bounded = doc.clone().bounded(&["artist"]).unwrap();
        assert_eq!(bounded.description, doc.description);
        assert_eq!(bounded.image, doc.image);
        assert_eq!(bounded.credits, doc.credits);
        assert!(serde_json::to_string_pretty(&bounded).unwrap().len() <= DOCUMENT_CAP);
        let past = card(980).bounded(&["artist"]).unwrap();
        assert!(serde_json::to_string_pretty(&past).unwrap().len() <= DOCUMENT_CAP);
        assert!(past.description.is_none() || past.image.is_none());
        for c in &past.credits {
            match c.field {
                CreditField::Image => assert!(past.image.is_some()),
                CreditField::Description => assert!(past.description.is_some()),
            }
        }
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
    /// A card's image that is a web link is on a named public host, as a stream's link is: an
    /// IP address, `localhost`, a name with no dot or a link with user info is private to
    /// whoever reads the card, and goes.
    #[test]
    fn a_card_image_is_on_a_named_public_host() {
        for private in [
            "http://127.0.0.1/a.jpg",
            "https://10.0.0.5:8443/a.jpg",
            "http://[::1]/a.jpg",
            "http://localhost/a.jpg",
            "https://printer.localhost/a.jpg",
            "http://intranet/a.jpg",
            "https://user@cdn.example/a.jpg",
        ] {
            let mut doc = ResourceMeta::new("https://example.org/p", "web", 100);
            doc.image = Some(private.into());
            assert_eq!(doc.bounded(&[]).unwrap().image, None, "{private}");
        }
        // A named public host, and a link that is no web link (a Pubky file), stay.
        for public in [
            "https://cdn.example.org/a.jpg",
            "pubky://ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi/pub/app/a.png",
        ] {
            let mut doc = ResourceMeta::new("https://example.org/p", "web", 100);
            doc.image = Some(public.into());
            assert_eq!(doc.bounded(&[]).unwrap().image.as_deref(), Some(public));
        }
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

    fn input(n: usize) -> Related {
        let mut r = Related::new("input", Some(format!("btc:address/1Addr{n}")));
        r.amount_sat = Some(n as i64);
        r
    }

    /// `related` keeps only well-formed entries, at most `RELATED_CAP`, and counts the entries
    /// of a `rel` it cuts; a total stays only for a `rel` it lists and cuts.
    #[test]
    fn related_is_bounded_and_counts_what_it_cuts() {
        let mut doc = ResourceMeta::new("btc:tx/x", "mempool", 60);
        let mut label = Related::new("output", None);
        label.name = Some(format!("  {}  ", "n".repeat(300)));
        doc.related = vec![label];
        doc.related.extend((0..40).map(input));
        doc.related
            .push(Related::new("Not A Rel", Some("btc:tx/y".into())));
        doc.related.push(Related::new("output", None));
        doc.related
            .push(Related::new("output", Some("btc:tx/a b".into())));
        doc.related
            .push(Related::new("output", Some("x".repeat(257))));
        doc.related_total = BTreeMap::from([
            ("input".into(), 400),
            ("output".into(), 1),
            ("peer".into(), 9),
        ]);
        let bounded = doc.bounded(&[]).unwrap();
        assert_eq!(bounded.related.len(), RELATED_CAP);
        assert_eq!(
            bounded.related[0].name.as_deref(),
            Some("n".repeat(128).as_str())
        );
        assert_eq!(
            bounded.related[1..],
            (0..31).map(input).collect::<Vec<_>>()[..]
        );
        assert_eq!(
            bounded.related_total,
            BTreeMap::from([("input".into(), 400)])
        );

        // Cut with no total of its own: the count is what it held.
        let mut doc = ResourceMeta::new("btc:tx/x", "mempool", 60);
        doc.related = (0..40).map(input).collect();
        let bounded = doc.bounded(&[]).unwrap();
        assert_eq!(bounded.related.len(), RELATED_CAP);
        assert_eq!(
            bounded.related_total,
            BTreeMap::from([("input".into(), 40)])
        );

        // A list too large for the document gives up its last entries, counted, before any
        // fact; stored and read back, it is the same.
        let mut doc = ResourceMeta::new("btc:tx/x", "mempool", 60);
        doc.attributes
            .insert("page_url".into(), "https://mempool.space/tx/x".into());
        doc.related = (0..RELATED_CAP)
            .map(|n| Related::new("output", Some(format!("btc:address/{n:0>240}"))))
            .collect();
        let bounded = doc.bounded(&["page_url"]).unwrap();
        assert!(bounded.related.len() < RELATED_CAP);
        assert!(!bounded.related.is_empty());
        assert_eq!(bounded.related_total["output"], RELATED_CAP as u64);
        assert_eq!(bounded.attributes.len(), 1);
        // A `rel` whose every entry is given up keeps no total.
        let mut two = bounded.clone();
        two.related = vec![input(1), Related::new("block", Some("btc:block/b".into()))];
        two.related_total = BTreeMap::from([("block".into(), 3), ("input".into(), 5)]);
        assert!(two.shed());
        assert_eq!(two.related, [input(1)]);
        assert_eq!(two.related_total, BTreeMap::from([("input".into(), 5)]));
        let stored: ResourceMeta =
            serde_json::from_value(serde_json::to_value(&bounded).unwrap()).unwrap();
        assert_eq!(
            (stored.related, stored.related_total),
            (bounded.related, bounded.related_total)
        );
        // An entry without a `uri` is stored with a null one; a document without a list has
        // neither field.
        let mut label = Related::new("output", None);
        label.name = Some("OP_RETURN".into());
        assert_eq!(
            serde_json::to_value(&label).unwrap(),
            serde_json::json!({"rel": "output", "uri": null, "name": "OP_RETURN"})
        );
        let plain =
            serde_json::to_value(ResourceMeta::new("https://example.org", "web", 60)).unwrap();
        assert!(plain.get("related").is_none() && plain.get("related_total").is_none());
    }
}
