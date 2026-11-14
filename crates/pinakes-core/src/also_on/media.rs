//! What the book, podcast and film sources share: Wikidata property tables (how one external ID
//! becomes a derived link), the answer statuses of the services they read, and small checks.
use super::{Found, Link};
use crate::{hubs::wikidata::Item, resolver::RetryLater};
use anyhow::{bail, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;

/// One Wikidata external-ID property and the page its value names.
#[derive(Clone, Copy, Debug)]
pub struct Property {
    pub pid: &'static str,
    /// The service's name as a card shows it.
    pub site: &'static str,
    /// The page, with `$1` for the value (percent-encoded; `/` kept where the validator allows
    /// it).
    pub template: &'static str,
    valid: fn(&str) -> bool,
}
impl Property {
    /// The value a link to this property's page holds (the inverse of the page `links` makes),
    /// valid as the property's: such a link is a starting point too (`hubs::wikidata::item_with`).
    pub fn value_in(&self, uri: &str) -> Option<String> {
        crate::hubs::wikidata::template_value(self.template, uri).filter(|v| (self.valid)(v))
    }
}
pub(crate) const fn property(
    pid: &'static str,
    site: &'static str,
    template: &'static str,
    valid: fn(&str) -> bool,
) -> Property {
    Property {
        pid,
        site,
        template,
        valid,
    }
}

/// Characters a value keeps as they are in a link (unreserved, and `/` for path-like IDs).
const VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// The links an item has for these properties: one per property whose best-rank values are
/// exactly one value, and a valid one (several are ambiguous and add nothing, even when only one
/// of them is well-formed), and one per service (the first property in the table wins). A value
/// with a `.` or `..` path segment is refused: it would name another page.
pub fn links(item: &Item, table: &[Property]) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    for p in table {
        if out.iter().any(|l| l.site == p.site) {
            continue;
        }
        let mut values: Vec<&String> = item.id(p.pid).iter().collect();
        values.dedup();
        let values: Vec<&String> = match values.as_slice() {
            [one] if (p.valid)(one) && !one.split('/').any(|seg| seg == "." || seg == "..") => {
                vec![*one]
            }
            _ => Vec::new(),
        };
        if let [value] = values.as_slice() {
            let uri = p
                .template
                .replace("$1", &utf8_percent_encode(value, VALUE).to_string());
            out.push(Link::new(uri, p.site, "wikidata"));
        }
    }
    out
}

/// A positive decimal number without leading zeros.
pub(crate) fn digits(v: &str) -> bool {
    (1..=20).contains(&v.len()) && !v.starts_with('0') && v.bytes().all(|b| b.is_ascii_digit())
}
/// A slug: letters, digits and `-_.`.
pub(crate) fn slug(v: &str) -> bool {
    (1..=200).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
/// A lower-case UUID.
pub(crate) fn uuid(v: &str) -> bool {
    crate::hubs::mbid(v).is_some_and(|m| m == v)
}

/// A service's JSON answer: `None` for "no such thing" (one of `missing`), a retry for 429 and
/// 5xx (a minute later), an error for anything else.
pub(crate) fn json(status: u16, raw: &[u8], missing: &[u16]) -> Result<Option<Value>> {
    match status {
        200 => Ok(Some(serde_json::from_slice(raw)?)),
        s if missing.contains(&s) => Ok(None),
        429 | 500..=599 => Err(RetryLater {
            class: "upstream",
            seconds: 60,
        }
        .into()),
        s => bail!("unexpected status {s}"),
    }
}

/// A service's 429 or 5xx as a wait: a retry under `class` after the time the answer asked for,
/// held to a minute to an hour.
pub(crate) fn busy(
    class: &'static str,
    fetched: &crate::resolver::egress::Fetched,
) -> anyhow::Error {
    RetryLater {
        class,
        seconds: fetched.retry_after_secs.clamp(60, 3600),
    }
    .into()
}

/// When a run comes back after a request of its failed: the wait a `RetryLater` in the error's
/// chain names (a minute at least), else a quarter of an hour, in seconds.
pub(crate) fn retry_wait(error: &anyhow::Error) -> u64 {
    error
        .chain()
        .find_map(|c| c.downcast_ref::<RetryLater>())
        .map_or(900, |r| r.seconds.max(60))
}

/// What a failed secondary request leaves: a retry to pass on when nothing else was found.
#[derive(Default)]
pub(crate) struct Deferred(Option<anyhow::Error>);
impl Deferred {
    /// The answer, or `None` after counting the failure; a retry is kept for later.
    pub(crate) fn take<T>(
        &mut self,
        service: &'static str,
        result: Result<Option<T>>,
    ) -> Option<T> {
        match result {
            Ok(v) => v,
            Err(error) => {
                let class = crate::resolver::error_class(&error);
                crate::telemetry::also_on_service_error(service, class);
                tracing::debug!(service, class, "also-on service failed; the lookup goes on");
                if error.is::<crate::resolver::RetryLater>() && self.0.is_none() {
                    self.0 = Some(error);
                }
                None
            }
        }
    }
    /// The lookup's answer: what was found, or the retry when nothing was.
    pub(crate) fn finish(self, found: Found) -> Result<Found> {
        match self.0 {
            Some(retry) if found == Found::default() => Err(retry),
            _ => Ok(found),
        }
    }
}

/// The type a media link names, whatever its page calls itself: what the media source that reads
/// it says (a TMDB or Letterboxd movie is a `Movie`; a TMDB or TVmaze series a `TVSeries`; an
/// Open Library work or edition, or an ISBN, a `Book`; an Apple Podcasts, Podcast Index, Pocket
/// Casts, Castro or Fountain show a `PodcastSeries` and its episode a `PodcastEpisode`), else a
/// Trakt movie or show or a Goodreads book by its address.
pub fn link_type(uri: &str) -> Option<&'static str> {
    // A link one of the media sources reads (`also_on::Source::link_type`).
    if let Some(kind) = crate::also_on::registered()
        .iter()
        .find_map(|s| s.link_type(uri))
    {
        return kind;
    }
    let url = url::Url::parse(uri).ok()?;
    let host = url.host_str()?;
    let s: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (host.strip_prefix("www.").unwrap_or(host), s.as_slice()) {
        ("trakt.tv", ["movies", name]) if slug(name) => Some("Movie"),
        ("trakt.tv", ["shows", name]) if slug(name) => Some("TVSeries"),
        ("goodreads.com", ["work", "editions", id]) if work_id(id) => Some("Book"),
        ("goodreads.com", ["book", "show", id]) if work_id(id) => Some("Book"),
        ("fountain.fm", ["show", id]) if slug(id) => Some("PodcastSeries"),
        ("fountain.fm", ["episode", id]) if slug(id) => Some("PodcastEpisode"),
        ("castro.fm", ["episode", id]) if slug(id) => Some("PodcastEpisode"),
        _ => None,
    }
}

/// What the media sources found about a resource that types its card from its next refresh:
/// an episode key (a page that is one of a show's episodes), and a title's type (a Netflix title
/// is a movie or a series as its Wikidata item says).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeHints {
    pub episode: bool,
    pub title_type: Option<String>,
    /// The object's title and description the `titles` source found (a page the hook did not
    /// name).
    pub title: Option<String>,
    pub title_description: Option<String>,
    /// The audio an also-on source heard of for it ([`STREAMS`]), with that source.
    pub stream: Option<(String, crate::model::Stream)>,
    /// The live facts of each source that fills cards (`also_on::Source::fills`), by source.
    pub filled: std::collections::BTreeMap<String, Vec<super::Fact>>,
}
/// The also-on sources that hear of a stream, as the facts `stream_url`, `stream_type` and
/// `stream_duration`, first the one a card takes: `podcasts`, an episode's enclosure, and
/// `itunes`, an Apple Music track's preview (`preview: true`).
pub const STREAMS: &[(&str, bool)] = &[("podcasts", false), ("itunes", true)];
/// An episode's enclosure as a key, `podcast:enclosure:{SHA-256 of its URL}` (an http(s) URL,
/// without its fragment): the very file a feed, Podcast Index, iTunes or a player's page plays.
/// One episode may play several (a show's feed and the copy iTunes reads).
pub fn enclosure_key(url: &str) -> Option<String> {
    use sha2::Digest as _;
    let mut url = url::Url::parse(url.trim()).ok()?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    url.set_fragment(None);
    let hash = sha2::Sha256::digest(url.as_str().as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("podcast:enclosure:{hex}"))
}
/// Whether the card plays a stream of its own: one no [`STREAMS`] source wrote into it
/// ([`type_card`], which fingerprints what it wrote in `hinted_by`). A source that heard of the
/// stream a card plays keeps hearing of it, or the card loses it at its next refresh.
pub fn own_stream(doc: &crate::model::ResourceMeta) -> bool {
    let Some(stream) = &doc.stream else {
        return false;
    };
    !STREAMS.iter().any(|(source, _)| {
        doc.hinted_by.get(*source).is_some_and(|written| {
            [None, Some("episode")]
                .into_iter()
                .any(|also| *written == stream_fingerprint(also, stream))
        })
    })
}
/// Record a stream a [`STREAMS`] source heard of as its facts: a card without a stream of its
/// own takes it ([`type_card`]), and one that has none yet is read again now. A link longer than
/// a fact holds (512 bytes) is none.
pub fn hear(found: &mut super::Found, subject: &super::Subject, stream: crate::model::Stream) {
    if stream.url.len() > 512 {
        return;
    }
    found.reread |= subject.doc.as_ref().is_some_and(|d| d.stream.is_none());
    found.facts.push(super::Fact::new("stream_url", stream.url));
    found
        .facts
        .push(super::Fact::new("stream_type", stream.media_type));
    if let Some(seconds) = stream.duration_s {
        found
            .facts
            .push(super::Fact::new("stream_duration", seconds.to_string()));
    }
}
/// The hints of these resources (live rows only), from the sources that are on.
pub async fn type_hints(
    pool: &sqlx::PgPool,
    ids: &[String],
) -> Result<std::collections::HashMap<String, TypeHints>> {
    let sources = "source NOT IN (SELECT source FROM also_on_source_off)";
    let mut out: std::collections::HashMap<String, TypeHints> = Default::default();
    let episodes: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT resource_id FROM resource_key WHERE resource_id=ANY($1) AND source='podcasts' AND key LIKE 'podcast:item:%' AND expires_at>now() AND {sources}"
    ))
    .bind(ids)
    .fetch_all(pool)
    .await?;
    for id in episodes {
        out.entry(id).or_default().episode = true;
    }
    let fillers: Vec<&str> = crate::also_on::registered()
        .iter()
        .filter(|s| !s.fills().is_empty())
        .map(|s| s.id())
        .collect();
    // Every fact a hint reads, in one statement: a title's type (`films`), a title and its
    // description (`titles`), the facts of the sources that fill cards and the stream facts.
    // The rows come by resource, source, name and value, so the first row of a name is its
    // least value: the one a hint with one value takes.
    let facts: Vec<(String, String, String, String)> = sqlx::query_as(&format!(
        "SELECT resource_id,source,name,value FROM resource_fact WHERE resource_id=ANY($1) AND expires_at>now() AND {sources} AND ((source='films' AND name='title_type') OR (source='titles' AND name IN ('title','title_description')) OR source=ANY($2) OR (source=ANY($3) AND name IN ('stream_url','stream_type','stream_duration'))) ORDER BY resource_id,source,name,value"
    ))
    .bind(ids)
    .bind(&fillers)
    .bind(STREAMS.iter().map(|(s, _)| *s).collect::<Vec<_>>())
    .fetch_all(pool)
    .await?;
    let mut heard: std::collections::HashMap<(String, String), [Option<String>; 3]> =
        Default::default();
    for (id, source, name, value) in facts {
        // A row may be read in more than one way: each is as its own statement read it.
        let single = match (source.as_str(), name.as_str()) {
            ("films", "title_type") => Some(0),
            ("titles", "title") => Some(1),
            ("titles", "title_description") => Some(2),
            _ => None,
        };
        if let Some(field) = single {
            let hints = out.entry(id.clone()).or_default();
            let slot = match field {
                0 => &mut hints.title_type,
                1 => &mut hints.title,
                _ => &mut hints.title_description,
            };
            if slot.is_none() {
                *slot = Some(value.clone());
            }
        }
        let takes = crate::also_on::registered()
            .iter()
            .any(|s| s.id() == source && s.fills().contains(&name.as_str()));
        if takes {
            out.entry(id.clone())
                .or_default()
                .filled
                .entry(source.clone())
                .or_default()
                .push(super::Fact::new(name.clone(), value.clone()));
        }
        let at = match name.as_str() {
            "stream_url" => 0,
            "stream_type" => 1,
            "stream_duration" => 2,
            _ => continue,
        };
        if STREAMS.iter().any(|(s, _)| *s == source) {
            let slot = &mut heard.entry((id, source)).or_default()[at];
            if slot.is_none() {
                *slot = Some(value);
            }
        }
    }
    for (source, preview) in STREAMS.iter().rev() {
        for ((id, from), [url, kind, seconds]) in &heard {
            if from != source {
                continue;
            }
            let stream = crate::model::Stream::public(
                url.as_deref().unwrap_or_default(),
                kind.as_deref().unwrap_or_default(),
                seconds.as_deref().and_then(|s| s.parse().ok()),
                *preview,
            );
            if let Some(stream) = stream {
                out.entry(id.clone()).or_default().stream = Some((from.clone(), stream));
            }
        }
    }
    Ok(out)
}

impl TypeHints {
    /// What `source` finds for a card now, as one fingerprint of every value it writes into it
    /// (`titles`: the title and description; `films`: the title type; `podcasts`: the episode;
    /// a source that fills cards: its facts); `None` when it finds nothing.
    pub fn finding(&self, source: &str) -> Option<String> {
        match source {
            "titles" => self.title.as_deref().map(|title| {
                fingerprint(&[title, self.title_description.as_deref().unwrap_or("")])
            }),
            "films" => self.title_type.as_deref().map(|t| fingerprint(&[t])),
            // An episode without a stream keeps the fingerprint it had before streams.
            "podcasts" => match self.heard("podcasts") {
                None => self.episode.then(|| fingerprint(&["episode"])),
                Some(s) => Some(stream_fingerprint(self.episode.then_some("episode"), s)),
            },
            source if self.filled.contains_key(source) => {
                let facts = &self.filled[source];
                let values: Vec<&str> = facts
                    .iter()
                    .flat_map(|f| [f.name.as_str(), f.value.as_str()])
                    .collect();
                Some(fingerprint(&values))
            }
            source => self.heard(source).map(|s| stream_fingerprint(None, s)),
        }
    }
}
impl TypeHints {
    /// The stream `source` heard of, when the card takes that source's.
    fn heard(&self, source: &str) -> Option<&crate::model::Stream> {
        self.stream
            .as_ref()
            .filter(|(from, _)| from == source)
            .map(|(_, s)| s)
    }
}
/// The fingerprint of a stream a source wrote into a card, with what else it wrote.
fn stream_fingerprint(also: Option<&str>, s: &crate::model::Stream) -> String {
    let seconds = s.duration_s.map(|d| d.to_string()).unwrap_or_default();
    fingerprint(&[also.unwrap_or(""), &s.url, &s.media_type, &seconds])
}
/// A short fingerprint of the values a source wrote into a card (`ResourceMeta::hinted_by`).
pub fn fingerprint(values: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in values {
        hasher.update(value.as_bytes());
        hasher.update(&[0x1f]);
    }
    hasher.finalize().to_hex()[..16].to_owned()
}
/// The also-on source of a live `artist_type` fact, from the sources that are on, and its value.
pub async fn artist_type(pool: &sqlx::PgPool, id: &str) -> Result<Option<(String, String)>> {
    Ok(sqlx::query_as("SELECT value,source FROM resource_fact WHERE resource_id=$1 AND name='artist_type' AND expires_at>now() AND source NOT IN (SELECT source FROM also_on_source_off) ORDER BY value LIMIT 1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
/// Whether a stored card carries a finding (`ResourceMeta::hinted_by`) its source no longer
/// gives as it is: switched off, its row expired or gone, or any value it wrote changed or
/// dropped (a description gone under an unchanged title). A 304 does not reuse such a card
/// (`queue`): the page is read again in full, and takes what the sources find now.
pub async fn outdated(
    pool: &sqlx::PgPool,
    id: &str,
    doc: &crate::model::ResourceMeta,
) -> Result<bool> {
    if doc.hinted_by.is_empty() {
        return Ok(false);
    }
    let hints = type_hints(pool, &[id.to_owned()])
        .await?
        .remove(id)
        .unwrap_or_default();
    let artist = artist_type(pool, id).await?;
    Ok(doc.hinted_by.iter().any(|(source, written)| {
        let now = hints.finding(source).or_else(|| {
            artist
                .as_ref()
                .filter(|(_, s)| s == source)
                .map(|(t, _)| fingerprint(&[t]))
        });
        now.as_ref() != Some(written)
    }))
}
/// Record that `source`'s finding, `fingerprint`, is written into the card.
pub fn hinted(doc: &mut crate::model::ResourceMeta, source: &str, fingerprint: String) {
    doc.hinted_by.insert(source.to_owned(), fingerprint);
}

/// A Goodreads ID: digits, then an optional title slug (`1540236.The_Hobbit`, `4671-the-great-gatsby`).
fn work_id(v: &str) -> bool {
    let n = v.split(['.', '-']).next().unwrap_or("");
    digits(n)
}

/// Type a resolved card by what its media link is ([`link_type`]), or by what the media sources
/// found ([`TypeHints`]); a card that did not resolve, or a link nothing types, is left alone.
/// Then the sources that fill cards write what their facts give into it ([`fill`]).
pub fn type_card(uri: &str, hints: &TypeHints, doc: &mut crate::model::ResourceMeta) {
    if doc.status != crate::model::Status::Ok {
        return;
    }
    let hinted = hints
        .title_type
        .as_deref()
        .and_then(|t| ["Movie", "TVSeries"].into_iter().find(|k| *k == t));
    // A Netflix card its Wikidata item already typed (`media_records`) keeps that type.
    let own = ["Movie", "TVSeries"]
        .into_iter()
        .find(|k| *k == doc.type_name);
    if let Some(title) = &hints.title {
        if crate::sources::apply_title(uri, title, hints.title_description.as_deref(), doc) {
            self::hinted(doc, "titles", hints.finding("titles").unwrap_or_default());
        }
    }
    match link_type(uri) {
        Some("CreativeWork") => {
            if hinted.is_some() {
                self::hinted(doc, "films", hints.finding("films").unwrap_or_default());
            }
            doc.set_type(hinted.or(own).unwrap_or("CreativeWork"))
        }
        Some(kind) => doc.set_type(kind),
        None if hints.episode => {
            self::hinted(
                doc,
                "podcasts",
                hints.finding("podcasts").unwrap_or_default(),
            );
            doc.set_type("PodcastEpisode")
        }
        None => {}
    }
    // A card without a stream of its own takes the one a source heard of.
    if let (None, Some((source, stream))) = (&doc.stream, &hints.stream) {
        doc.stream = Some(stream.clone());
        self::hinted(doc, source, hints.finding(source).unwrap_or_default());
    }
    fill(hints, doc);
}
/// Let each source that fills cards (`also_on::Source::fill_card`) write what its live facts give
/// into the typed card, after taking out what it wrote before; one with nothing to write (off,
/// its facts gone, the card has its own values) leaves no fingerprint.
pub fn fill(hints: &TypeHints, doc: &mut crate::model::ResourceMeta) {
    for source in crate::also_on::registered()
        .iter()
        .filter(|s| !s.fills().is_empty())
    {
        let facts = hints
            .filled
            .get(source.id())
            .map(Vec::as_slice)
            .unwrap_or_default();
        if source.fill_card(facts, doc) {
            self::hinted(
                doc,
                source.id(),
                hints.finding(source.id()).unwrap_or_default(),
            );
        } else {
            doc.hinted_by.remove(source.id());
        }
    }
    strip_unowned(doc);
}
/// Take out of a card the values a source that fills cards wrote that its live facts would no
/// longer write (`hints`: the live facts; expired, switched off, another poster), field by field:
/// a value the live facts still give stays, whatever else of them changed (a new overview on a
/// card with its own text). Also the values of a provider no source of this build writes. Whether
/// anything went. It never writes a value.
pub fn unfill(hints: &TypeHints, doc: &mut crate::model::ResourceMeta) -> bool {
    use crate::model::CreditField;
    let before = (
        doc.image.clone(),
        doc.description.clone(),
        doc.credits.clone(),
    );
    for source in crate::also_on::registered()
        .iter()
        .filter(|s| !s.fills().is_empty())
    {
        let Some(provider) = source.fills_as() else {
            continue;
        };
        // What the live facts would make of this card now.
        let facts = hints
            .filled
            .get(source.id())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut now = doc.clone();
        source.fill_card(facts, &mut now);
        for field in [CreditField::Image, CreditField::Description] {
            let credit = |d: &crate::model::ResourceMeta| {
                d.credits
                    .iter()
                    .find(|c| c.field == field && c.provider == provider)
                    .cloned()
            };
            let value = |d: &crate::model::ResourceMeta| match field {
                CreditField::Image => d.image.clone(),
                CreditField::Description => d.description.clone(),
            };
            let Some(written) = credit(doc) else {
                continue;
            };
            if (Some(written), value(doc)) != (credit(&now), value(&now)) {
                match field {
                    CreditField::Image => doc.image = None,
                    CreditField::Description => doc.description = None,
                }
                doc.credits
                    .retain(|c| !(c.field == field && c.provider == provider));
            }
        }
        if !doc.credits.iter().any(|c| c.provider == provider) {
            doc.hinted_by.remove(source.id());
        }
    }
    strip_unowned(doc);
    (
        doc.image.clone(),
        doc.description.clone(),
        doc.credits.clone(),
    ) != before
}
/// Take out the values (and credits) of a provider written from facts that no source of this
/// build writes (`also_on::Source::fills_as`): a removed source's values never outlive it.
fn strip_unowned(doc: &mut crate::model::ResourceMeta) {
    let owned: Vec<crate::model::CreditProvider> = crate::also_on::registered()
        .iter()
        .filter_map(|s| s.fills_as())
        .collect();
    let gone = |p: crate::model::CreditProvider| p.from_facts() && !owned.contains(&p);
    for credit in doc.credits.iter().filter(|c| gone(c.provider)) {
        match credit.field {
            crate::model::CreditField::Image => doc.image = None,
            crate::model::CreditField::Description => doc.description = None,
        }
    }
    doc.credits.retain(|c| !gone(c.provider));
}

/// An IMDb title ID.
pub fn imdb(v: &str) -> bool {
    v.strip_prefix("tt")
        .is_some_and(|d| matches!(d.len(), 7 | 8) && d.bytes().all(|b| b.is_ascii_digit()))
}
/// Whether a film link is the title's own page (not a season, cast or review page under it).
pub fn exact_page(uri: &str) -> bool {
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    let s: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    match (
        url.host_str().map(|h| h.strip_prefix("www.").unwrap_or(h)),
        s.as_slice(),
    ) {
        // TVmaze's show page carries its slug; its subpages have fixed names.
        (Some("tvmaze.com"), [_, _, sub]) => !matches!(
            *sub,
            "episodes" | "seasons" | "cast" | "crew" | "characters" | "gallery" | "news" | "akas"
        ),
        // A Netflix title, localized or not.
        (Some("netflix.com"), [_, "title", _]) => true,
        (_, s) => s.len() == 2,
    }
}
/// What a title's Wikidata item says it is, by the kind of IDs it holds: a movie (TMDB movie,
/// TheTVDB movie, Apple TV movie, Trakt film) or a series (TMDB TV, TheTVDB series, Apple TV
/// show, TVmaze show). Both or neither: nothing.
pub fn title_type(item: &Item) -> Option<&'static str> {
    let any = |ps: &[&str]| ps.iter().any(|p| !item.id(p).is_empty());
    match (
        any(&["P4947", "P12196", "P9586", "P12492"]),
        any(&["P4983", "P4835", "P9751", "P8600"]),
    ) {
        (true, false) => Some("Movie"),
        (false, true) => Some("TVSeries"),
        _ => None,
    }
}

/// A GUID in lower case, if it is one.
pub fn guid(v: &str) -> Option<String> {
    let v = v.trim().to_ascii_lowercase();
    uuid(&v).then_some(v)
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Recorded answers in `tests/fixtures/media/<family>/` (see each `provenance.json`).
    use std::io::Read;

    pub fn read(family: &str, name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/media/{family}/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        if name.ends_with(".gz") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut out)
                .expect("gzip fixture");
            out
        } else {
            raw
        }
    }
    /// The recorded Wikidata items (`shared/entities-media.json.gz`) by Q-id.
    pub fn items() -> Vec<crate::hubs::wikidata::Item> {
        let raw = read("shared", "entities-media.json.gz");
        crate::hubs::wikidata::parse_entities(&serde_json::from_slice(&raw).unwrap())
    }
    pub fn item(qid: &str) -> crate::hubs::wikidata::Item {
        items()
            .into_iter()
            .find(|i| i.qid == qid)
            .unwrap_or_else(|| panic!("{qid}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hubs::wikidata::Item;
    use std::collections::BTreeMap;

    /// A card without a stream of its own takes the one a source heard of, fingerprinted so a
    /// change reads it again; a card's own stays. An episode without a stream keeps the
    /// fingerprint it had before streams, so no stored card is read again for nothing.
    #[test]
    fn a_card_takes_the_stream_a_source_heard_of() {
        use crate::model::{ResourceMeta, Stream};
        let heard = Stream::public(
            "https://cdn.example.org/ep.mp3",
            "audio/mpeg",
            Some(60),
            false,
        )
        .unwrap();
        let page = "https://example.org/episode/1";
        let hints = TypeHints {
            episode: true,
            stream: Some(("podcasts".into(), heard.clone())),
            ..Default::default()
        };
        let mut doc = ResourceMeta::new(page, "web", 60);
        type_card(page, &hints, &mut doc);
        assert_eq!(doc.type_name, "PodcastEpisode");
        assert_eq!(doc.stream.as_ref(), Some(&heard));
        assert_eq!(
            doc.hinted_by["podcasts"],
            hints.finding("podcasts").unwrap()
        );
        // Another stream is another finding: the stored card is outdated.
        let moved = TypeHints {
            stream: Some((
                "podcasts".into(),
                Stream::public(
                    "https://cdn.example.org/ep2.mp3",
                    "audio/mpeg",
                    Some(60),
                    false,
                )
                .unwrap(),
            )),
            ..hints.clone()
        };
        assert_ne!(moved.finding("podcasts"), hints.finding("podcasts"));
        let episode_only = TypeHints {
            episode: true,
            ..Default::default()
        };
        assert_eq!(
            episode_only.finding("podcasts"),
            Some(fingerprint(&["episode"]))
        );
        // A card that plays already keeps its own.
        let own =
            Stream::public("https://cdn.example.org/own.mp3", "audio/mpeg", None, false).unwrap();
        let mut doc = ResourceMeta::new(page, "web", 60);
        doc.stream = Some(own.clone());
        type_card(page, &hints, &mut doc);
        assert_eq!(doc.stream, Some(own));
        // iTunes' preview on an Apple Music track (a card `music::type_card` typed).
        let preview = Stream::public(
            "https://audio-ssl.itunes.apple.com/a/mzaf_1.plus.aac.p.m4a",
            "audio/mp4",
            None,
            true,
        )
        .unwrap();
        let track = "https://music.apple.com/us/song/1";
        let mut doc = ResourceMeta::new(track, "media", 60);
        let hints = TypeHints {
            stream: Some(("itunes".into(), preview.clone())),
            ..Default::default()
        };
        type_card(track, &hints, &mut doc);
        assert_eq!(doc.stream, Some(preview));
        assert_eq!(doc.hinted_by["itunes"], hints.finding("itunes").unwrap());
    }

    fn item(ids: &[(&str, &[&str])]) -> Item {
        Item {
            qid: "Q1".into(),
            ids: ids
                .iter()
                .map(|(p, v)| (p.to_string(), v.iter().map(|s| s.to_string()).collect()))
                .collect::<BTreeMap<_, _>>(),
            sitelinks: BTreeMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn what_an_items_ids_say_a_title_is() {
        assert_eq!(title_type(&fixture::item("Q172241")), Some("Movie"));
        assert_eq!(title_type(&fixture::item("Q1079")), Some("TVSeries"));
    }
    #[test]
    fn one_value_one_link_one_per_site() {
        const TABLE: &[Property] = &[
            property("P1", "A", "https://a.example/$1", digits),
            property("P2", "A", "https://a.example/other/$1", digits),
            property("P3", "B", "https://b.example/$1", slug),
            property("P4", "C", "https://c.example/$1", digits),
            property("P5", "D", "https://d.example/$1/", |v| v.contains('/')),
        ];
        let links = links(
            &item(&[
                ("P1", &["12"]),
                ("P2", &["34"]),
                ("P3", &["x y"]),
                ("P4", &["1", "x"]),
                ("P5", &["movies/the film"]),
            ]),
            TABLE,
        );
        let got: Vec<(&str, &str)> = links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("A", "https://a.example/12"),
                ("D", "https://d.example/movies/the%20film/")
            ]
        );
        assert!(links.iter().all(|l| l.via == "wikidata"));
        // A value that walks out of its path names another page.
        const SLUGS: &[Property] = &[property("P1", "A", "https://a.example/film/$1/", |v| {
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'/')
        })];
        for v in ["..", ".", "m/../x"] {
            assert!(
                super::links(&item(&[("P1", &[v])]), SLUGS).is_empty(),
                "{v}"
            );
        }
        assert_eq!(super::links(&item(&[("P1", &["a.b"])]), SLUGS).len(), 1);
    }

    #[test]
    /// Each media source types its own links (its tests); a Trakt or Goodreads link is typed here,
    /// by its address.
    fn media_links_are_typed_by_what_they_are() {
        for (uri, kind) in [
            ("https://trakt.tv/shows/breaking-bad", Some("TVSeries")),
            ("https://trakt.tv/movies/the-matrix-1999", Some("Movie")),
            ("https://trakt.tv/shows/breaking-bad/seasons/1", None),
            ("https://example.org/", None),
            (
                "https://www.goodreads.com/work/editions/1540236",
                Some("Book"),
            ),
            (
                "https://www.goodreads.com/book/show/4671.The_Great_Gatsby",
                Some("Book"),
            ),
        ] {
            assert_eq!(link_type(uri), kind, "{uri}");
        }
        // What the sources found types the card from its next refresh.
        let typed = |uri: &str, hints: TypeHints| {
            let mut doc = crate::model::ResourceMeta::new(uri, "web", 60);
            doc.set_type("Article");
            type_card(uri, &hints, &mut doc);
            doc.type_name
        };
        let episode = TypeHints {
            episode: true,
            ..Default::default()
        };
        assert_eq!(
            typed("https://darknetdiaries.com/episode/180/", episode.clone()),
            "PodcastEpisode"
        );
        assert_eq!(
            typed(
                "https://darknetdiaries.com/episode/180/",
                TypeHints::default()
            ),
            "Article"
        );
    }

    #[test]
    fn checks() {
        assert!(digits("278") && !digits("0278") && !digits("") && !digits("2a"));
        assert!(slug("the-shawshank-redemption") && !slug("a b"));
        assert!(uuid("304b84f0-07b0-5265-b6b7-da5cf5aeb56e"));
        assert!(!uuid("304B84F0-07B0-5265-B6B7-DA5CF5AEB56E"));
    }
}
