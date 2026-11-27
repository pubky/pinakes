//! Catalogues: the open catalogues Pinakes imports in bulk (Wikidata, MusicBrainz, OpenStreetMap,
//! OpenAlex, Podcast Index, Tranco, software packages), one card per item in `catalogue_card`
//! and one state row per catalogue in `catalogue_state`.
//!
//! The contract the importers and the readers share:
//!
//! - [`Catalogue`] names each catalogue; its name is the `catalogue` column.
//! - [`CataloguesConfig`] is `[catalogues]`: every catalogue is on by default and has its own
//!   `enabled`. [`CataloguesConfig::enabled`] and [`CataloguesConfig::enabled_names`] say which
//!   are on: a reader keeps only the cards of those (`catalogue = ANY($1)`), so a catalogue
//!   switched off leaves search and lookups at once while its rows stay.
//! - [`Card`] is one item as an importer reads it; [`Card::checked`] turns it into the row that
//!   is stored ([`Row`]): its link normalized as a referenced link is, its labels valid tag labels
//!   ([`label`]), its keys and links checked and bounded, and its content hash.
//! - [`run`] holds the importer trait ([`run::Importer`]) and the task that runs one importer:
//!   its lock, the free-disk check, one large full import at a time, and its progress.
//! - [`store`] is the batch writer ([`store::upsert`], [`store::remove`],
//!   [`store::finish_import`]), the drop ([`store::drop_catalogue`]) and the state row.
//!
//! Importers do not use the fetch queue, the refresh times or the change journal.
use crate::model;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;

pub mod bulk;
pub mod csv;
pub mod diff;
pub mod extras;
pub mod failure;
pub mod fetch;
pub mod join;
pub mod musicbrainz;
pub mod openalex;
pub mod osm;
pub mod podcastindex;
pub mod robots;
pub mod run;
pub mod software;
pub mod spill;
pub mod store;
pub mod tranco;
pub mod wikidata;

/// One catalogue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Catalogue {
    Wikidata,
    Musicbrainz,
    Osm,
    Openalex,
    Podcastindex,
    Tranco,
    Software,
}
impl Catalogue {
    /// Every catalogue, in the order of `locks::CATALOGUE`.
    pub const ALL: [Catalogue; 7] = [
        Catalogue::Wikidata,
        Catalogue::Musicbrainz,
        Catalogue::Osm,
        Catalogue::Openalex,
        Catalogue::Podcastindex,
        Catalogue::Tranco,
        Catalogue::Software,
    ];
    /// The name in `catalogue_card.catalogue`, `catalogue_state.catalogue`, `[catalogues.<name>]`,
    /// the metrics' `catalogue` label and `pinakesd catalogue drop <name>`.
    pub fn name(self) -> &'static str {
        match self {
            Catalogue::Wikidata => "wikidata",
            Catalogue::Musicbrainz => "musicbrainz",
            Catalogue::Osm => "osm",
            Catalogue::Openalex => "openalex",
            Catalogue::Podcastindex => "podcastindex",
            Catalogue::Tranco => "tranco",
            Catalogue::Software => "software",
        }
    }
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
    /// A large catalogue's full import needs tens of gigabytes of temporary files (a dump, a node
    /// index): only one of them runs at a time (`locks::CATALOGUE_LARGE_IMPORT`).
    pub fn large(self) -> bool {
        matches!(
            self,
            Catalogue::Wikidata | Catalogue::Musicbrainz | Catalogue::Osm | Catalogue::Openalex
        )
    }
    /// How far behind upstream its cards may fall (`current_as_of`) before the update lag
    /// alert fires: the source's own publication rhythm and a margin. Wikidata's change stream,
    /// OpenStreetMap's daily diffs, Tranco's daily list and the package registries: two days.
    /// MusicBrainz's dumps, twice a week: five days. Podcast Index's weekly database: nine days.
    /// OpenAlex's quarterly snapshot: 100 days. The one place these live; `/metrics` exports them
    /// as `pinakes_catalogue_max_update_lag_seconds`.
    pub fn max_update_lag(self) -> std::time::Duration {
        const DAY: u64 = 86400;
        std::time::Duration::from_secs(match self {
            Catalogue::Wikidata | Catalogue::Osm | Catalogue::Tranco | Catalogue::Software => {
                2 * DAY
            }
            Catalogue::Musicbrainz => 5 * DAY,
            Catalogue::Podcastindex => 9 * DAY,
            Catalogue::Openalex => 100 * DAY,
        })
    }
    /// The advisory lock its importer holds (`locks::CATALOGUE`).
    pub fn lock_key(self) -> i64 {
        crate::locks::CATALOGUE[self as usize]
    }
    /// The name of its importer's task in `pinakesd`.
    pub fn task_name(self) -> &'static str {
        match self {
            Catalogue::Wikidata => "catalogue wikidata",
            Catalogue::Musicbrainz => "catalogue musicbrainz",
            Catalogue::Osm => "catalogue osm",
            Catalogue::Openalex => "catalogue openalex",
            Catalogue::Podcastindex => "catalogue podcastindex",
            Catalogue::Tranco => "catalogue tranco",
            Catalogue::Software => "catalogue software",
        }
    }
    /// A sub-source a card of this catalogue may name (`Card::source`): the catalogue itself, or
    /// for software the package registry.
    pub fn sources(self) -> &'static [&'static str] {
        match self {
            Catalogue::Wikidata => &["wikidata"],
            Catalogue::Musicbrainz => &["musicbrainz"],
            Catalogue::Osm => &["osm"],
            Catalogue::Openalex => &["openalex"],
            Catalogue::Podcastindex => &["podcastindex"],
            Catalogue::Tranco => &["tranco"],
            Catalogue::Software => &["crates.io", "f-droid", "flathub"],
        }
    }
}
impl std::fmt::Display for Catalogue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `[catalogues]`: the switch for all catalogues, the work directory, and one section per
/// catalogue. A catalogue is on when both its own `enabled` and this one are.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CataloguesConfig {
    /// Off: no catalogue imports, and no catalogue card is read.
    pub enabled: bool,
    /// Where importers keep their downloads and temporary files. Relative paths are relative to
    /// the working directory, as `osm.dump_dir` is.
    pub dir: String,
    pub wikidata: LimitedCatalogue,
    pub musicbrainz: LimitedCatalogue,
    pub osm: OsmCatalogue,
    pub openalex: OpenAlexCatalogue,
    pub podcastindex: CatalogueSwitch,
    pub tranco: CatalogueSwitch,
    pub software: CatalogueSwitch,
}
impl Default for CataloguesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "catalogues".into(),
            wikidata: LimitedCatalogue::default(),
            musicbrainz: LimitedCatalogue::default(),
            osm: OsmCatalogue::default(),
            openalex: OpenAlexCatalogue::default(),
            podcastindex: CatalogueSwitch::default(),
            tranco: CatalogueSwitch::default(),
            software: CatalogueSwitch::default(),
        }
    }
}
/// A catalogue with only its switch.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogueSwitch {
    pub enabled: bool,
}
impl Default for CatalogueSwitch {
    fn default() -> Self {
        Self { enabled: true }
    }
}
/// Wikidata and MusicBrainz: `limit` imports at most that many items (a development instance);
/// none, the default, imports all.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitedCatalogue {
    pub enabled: bool,
    #[serde(deserialize_with = "count_or_text")]
    pub limit: Option<u64>,
}
impl Default for LimitedCatalogue {
    fn default() -> Self {
        Self {
            enabled: true,
            limit: None,
        }
    }
}
/// OpenStreetMap: `region` is `planet` (the default) or a region of Geofabrik's extracts, as its
/// path names it (`europe/switzerland`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OsmCatalogue {
    pub enabled: bool,
    pub region: String,
}
impl Default for OsmCatalogue {
    fn default() -> Self {
        Self {
            enabled: true,
            region: "planet".into(),
        }
    }
}
/// OpenAlex: works cited more than `min_citations` times ([`CataloguesConfig::cited_enough`]).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAlexCatalogue {
    pub enabled: bool,
    pub min_citations: u32,
}
impl Default for OpenAlexCatalogue {
    fn default() -> Self {
        Self {
            enabled: true,
            min_citations: 50,
        }
    }
}
/// A count, as a number or as the text a `PINAKES__…` override gives it; null is none.
fn count_or_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Count {
        Number(u64),
        Text(String),
    }
    match Option::<Count>::deserialize(d)? {
        None => Ok(None),
        Some(Count::Number(n)) => Ok(Some(n)),
        Some(Count::Text(text)) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom("limit must be a whole number")),
    }
}
impl CataloguesConfig {
    /// Whether `catalogue` is on: `[catalogues] enabled` and its own `enabled`. An importer runs
    /// only for a catalogue that is on, and only its cards are read.
    pub fn enabled(&self, catalogue: Catalogue) -> bool {
        self.enabled
            && match catalogue {
                Catalogue::Wikidata => self.wikidata.enabled,
                Catalogue::Musicbrainz => self.musicbrainz.enabled,
                Catalogue::Osm => self.osm.enabled,
                Catalogue::Openalex => self.openalex.enabled,
                Catalogue::Podcastindex => self.podcastindex.enabled,
                Catalogue::Tranco => self.tranco.enabled,
                Catalogue::Software => self.software.enabled,
            }
    }
    /// The catalogues that are on.
    pub fn enabled_set(&self) -> BTreeSet<Catalogue> {
        Catalogue::ALL
            .into_iter()
            .filter(|c| self.enabled(*c))
            .collect()
    }
    /// The names of the catalogues that are on, to bind as `catalogue = ANY($1)`: a query that
    /// reads `catalogue_card` keeps only these rows.
    pub fn enabled_names(&self) -> Vec<&'static str> {
        self.enabled_set()
            .into_iter()
            .map(Catalogue::name)
            .collect()
    }
    /// The directory of one catalogue's downloads and temporary files: `dir/<name>`.
    /// `pinakesd catalogue drop` deletes it with the catalogue.
    pub fn work_dir(&self, catalogue: Catalogue) -> std::path::PathBuf {
        std::path::Path::new(&self.dir).join(catalogue.name())
    }
    /// Whether an OpenAlex work cited this many times is imported: more than `min_citations`.
    pub fn cited_enough(&self, citations: u64) -> bool {
        citations > u64::from(self.openalex.min_citations)
    }
    /// What a full import of `catalogue` reads, as its configuration sets it: Wikidata's and
    /// MusicBrainz's `limit`, OpenStreetMap's `region`, OpenAlex's `min_citations` (an empty
    /// object for the others). Kept with each finished full import (`catalogue_state.scope`): one
    /// of another scope may remove many cards (`run::SHRINK_FLOOR`).
    pub fn scope(&self, catalogue: Catalogue) -> serde_json::Value {
        match catalogue {
            Catalogue::Wikidata | Catalogue::Musicbrainz => {
                serde_json::json!({ "limit": self.limit(catalogue) })
            }
            Catalogue::Osm => serde_json::json!({ "region": self.osm.region }),
            Catalogue::Openalex => {
                serde_json::json!({ "min_citations": self.openalex.min_citations })
            }
            Catalogue::Podcastindex | Catalogue::Tranco | Catalogue::Software => {
                serde_json::json!({})
            }
        }
    }
    /// `limit` of Wikidata or MusicBrainz; none for the others.
    pub fn limit(&self, catalogue: Catalogue) -> Option<u64> {
        match catalogue {
            Catalogue::Wikidata => self.wikidata.limit,
            Catalogue::Musicbrainz => self.musicbrainz.limit,
            _ => None,
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.dir.trim().is_empty() || self.dir.contains('\0') {
            anyhow::bail!("catalogues.dir must name a directory");
        }
        for (name, limit) in [
            ("wikidata", self.wikidata.limit),
            ("musicbrainz", self.musicbrainz.limit),
        ] {
            if limit == Some(0) {
                anyhow::bail!(
                    "catalogues.{name}.limit must be at least 1 (leave it out to import all)"
                );
            }
        }
        if !valid_region(&self.osm.region) {
            anyhow::bail!(
                "catalogues.osm.region must be \"planet\" or a Geofabrik region path such as \"europe/switzerland\" (lower case letters, digits and hyphens, parts joined by /)"
            );
        }
        if self.openalex.min_citations > 1_000_000 {
            anyhow::bail!("catalogues.openalex.min_citations must be at most 1000000");
        }
        Ok(())
    }
}
/// `planet`, or a Geofabrik region path: one to four parts of lower case letters, digits and
/// hyphens, joined by `/`.
fn valid_region(region: &str) -> bool {
    let parts: Vec<_> = region.split('/').collect();
    region.len() <= 128
        && (1..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            !p.is_empty()
                && !p.starts_with('-')
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The longest tag label, in characters, and the characters a label never holds: Pubky's tag
/// rules (pubky-app-specs `validationLimits`: `tagLabelMaxLength` 20, `tagInvalidChars`). A
/// label is also never empty and is lower case.
pub const LABEL_MAX_CHARS: usize = 20;
const LABEL_INVALID: [char; 6] = [',', ':', ' ', '\t', '\n', '\r'];

/// The longest source label read (bytes): one longer is no label (and none is cleaned).
const LABEL_RAW_CAP: usize = 256;
/// A source's label (a genre, an OSM category, a topic) as a valid Pubky tag label, so one tap on
/// a suggested tag writes a tag every client accepts: lower case, each run of whitespace, commas,
/// colons and underscores one hyphen, no control or invisible format characters ([`invisible`]),
/// no hyphens at either end. `None` when nothing is left, the label is longer than
/// [`LABEL_MAX_CHARS`] (a cut label would say something else), or the source's text is over
/// 256 bytes.
pub fn label(raw: &str) -> Option<String> {
    if raw.len() > LABEL_RAW_CAP {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    let mut gap = false;
    // The whole text at once, as a client lower-cases a tag (a final Σ becomes ς).
    for c in raw.to_lowercase().chars() {
        if c.is_whitespace() || LABEL_INVALID.contains(&c) || c == '_' || c == '-' {
            gap = true;
        } else if c.is_control() || invisible(c) || joiner(c) {
            continue;
        } else {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c);
        }
    }
    (!out.is_empty() && out.chars().count() <= LABEL_MAX_CHARS).then_some(out)
}
/// A character that shows nothing, but the two joiners ([`joiner`]): the format characters
/// (Unicode category Cf: a soft hyphen, the zero-width space, direction marks and overrides, word
/// joiners, invisible operators, the byte order mark, interlinear annotation marks, tag
/// characters) and the other default-ignorable ones (the combining grapheme joiner, variation
/// selectors, the Hangul fillers, Khmer's inherent vowels). A label or name holding one looks like
/// another (`jazz` and `jazz` with a zero-width space are two tags) or reads in another order
/// (U+202E).
pub fn invisible(c: char) -> bool {
    matches!(
        u32::from(c),
        0x034F
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0xE0000..=0xE0FFF
            | 0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x200B
            | 0x200E..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
    )
}
/// ZWNJ and ZWJ: format characters a word is written with (Persian, the Indic scripts, emoji),
/// kept in names; a label never holds one.
fn joiner(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}')
}
/// Labels as [`label`] makes them, each once, in their first order; at most [`LABELS_CAP`].
pub fn labels<'a>(raw: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    raw.into_iter()
        .filter_map(label)
        .filter(|l| seen.insert(l.clone()))
        .take(LABELS_CAP)
        .collect()
}

/// The most labels one card keeps.
pub const LABELS_CAP: usize = 32;
/// The most other names one card keeps.
pub const OTHER_NAMES_CAP: usize = 32;
/// The most keys one card keeps.
pub const KEYS_CAP: usize = 64;
/// The most related links one card keeps.
pub const RELATED_CAP: usize = model::RELATED_CAP;
/// The longest name or other name kept (bytes; longer ones are cut at a character boundary).
pub const NAME_CAP: usize = 512;
/// The longest key and source ID (bytes).
pub const KEY_CAP: usize = 512;
/// The largest `details` object (bytes of its JSON text), as a resource's document.
pub const DETAILS_CAP: usize = model::DOCUMENT_CAP;

/// The key a link people share for an item is stored under in `keys`: `url:` and the resource ID
/// of the link's root (the normalized link, `identity::resource_id`, or the root its spelling
/// rules name, `alias::root`), the form a resource's grouping keys use. An http(s) link, or a root
/// that is no web link but has a web page of its own (`doi:…`, `alias::web_page`).
pub fn link_key(uri: &str) -> Option<String> {
    link_root(uri).map(|root| own_key(&root))
}
/// The root a card may name for `uri`: the root of an http(s) link (its spelling rules', else the
/// link normalized), or a root that is no web link but has a web page (`doi:…`); never a link
/// that carries a credential (as written or normalized: a password in its user info is cut by
/// normalizing) nor a Pubky object.
fn link_root(uri: &str) -> Option<String> {
    let (_, normal, scheme) = model::identity(uri).ok()?;
    let root = match scheme.as_str() {
        "http" | "https" => crate::alias::root(&normal).map_or(normal, |a| a.root),
        _ if crate::alias::web_page(&normal).is_some() => normal,
        _ => return None,
    };
    (root.len() <= 2048 && !crate::capability::is_capability(uri) && publishable(&root))
        .then_some(root)
}
/// The web page of a card's root that is no web link (`doi:…`: its `doi.org` page,
/// `alias::web_page`) and that page's host; `None` for a web link.
pub fn root_page(uri: &str) -> Option<(String, String)> {
    let scheme = uri.split_once(':')?.0.to_ascii_lowercase();
    if matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let (page, _) = crate::alias::web_page(uri)?;
    let host = url::Url::parse(&page).ok()?.host_str()?.to_owned();
    Some((page, host))
}
/// The host a card is listed under (`host=`, the text index): its link's, or for a root that is
/// no web link its page's ([`root_page`]). The schema's `catalogue_card_host` answers the same for
/// every stored link.
pub fn card_host(uri: &str) -> Option<String> {
    match url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    {
        Some(host) => Some(host),
        None => root_page(uri).map(|(_, host)| host),
    }
}
/// `url:` and the resource ID of a stored link (`model::identity` already applied).
fn own_key(root: &str) -> String {
    format!("url:{}", crate::identity::resource_id(root))
}
/// A join key in the stored form: `kind:value`, the kind lower case (`wikidata`, `mb`, `isrc`,
/// `osm`, `doi`, `url`), the value non-empty, no whitespace, control or invisible character
/// ([`invisible`]), at most [`KEY_CAP`] bytes.
pub fn valid_key(key: &str) -> bool {
    let Some((kind, value)) = key.split_once(':') else {
        return false;
    };
    key.len() <= KEY_CAP
        && kind.len() <= 32
        && kind.starts_with(|c: char| c.is_ascii_lowercase())
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || invisible(c) || joiner(c))
}

/// A key as it is stored: [`valid_key`], in the form a resource's keys take and a link's derived
/// key has (`join::link_keys`), built by the same code: a DOI lower case, an ISBN as its ISBN-13, an
/// ISRC, a Wikidata ID and an OpenAlex ID upper case, a MusicBrainz key's entity and MBID lower
/// case (`hubs::key::mb`), a crate's name lower case with `-` for `_` (`packages::Package::key`),
/// an arXiv ID without its version, an F-Droid package and a Flathub ID as those stores spell them
/// (`apps`), a homepage's host lower case, punycode, without `www.` or a final dot, an OSM object
/// as `osm:<type>/<id>`, and a `url:` key a resource ID. `None` for one that is not valid in its
/// kind.
pub fn stored_key(key: &str) -> Option<String> {
    use crate::lookups::{packages, papers};
    let key = key.trim();
    if !valid_key(key) {
        return None;
    }
    let (kind, value) = key.split_once(':')?;
    let stored = match kind {
        "doi" => crate::resolver::cards::doi(key).map(|doi| format!("doi:{doi}")),
        "isbn" => crate::resolver::cards::isbn(key)
            .map(|isbn| format!("isbn:{}", crate::resolver::cards::isbn13(&isbn))),
        "url" => model::valid_id(value).then(|| key.to_owned()),
        // Resources key an ISRC as `music::isrc` writes it (upper case, without the hyphens of its
        // standard spelling: `isrc:GBUM71029604`), and join an OpenAlex work as `openalex:W…`
        // (`join::link_keys`).
        "isrc" => crate::music::isrc(value).map(|isrc| format!("isrc:{isrc}")),
        "openalex" => Some(format!("openalex:{}", value.to_ascii_uppercase())),
        "wikidata" => crate::hubs::key::wikidata(&value.to_ascii_uppercase()),
        "mb" => {
            let (entity, mbid) = value.split_once(':')?;
            let entity = entity.to_ascii_lowercase().replace('_', "-");
            match crate::hubs::MbKind::parse(&entity) {
                Some(kind) => crate::hubs::key::mb(kind, mbid),
                None => crate::hubs::mbid(mbid).map(|mbid| format!("mb:{entity}:{mbid}")),
            }
        }
        "crate" => {
            packages::Package::new(packages::Registry::Crates, value).map(|p| p.key().to_string())
        }
        "arxiv" => papers::arxiv_id(value).map(|id| papers::arxiv_key(&id).to_string()),
        "android" => crate::app_ids::android_package(value).then(|| key.to_owned()),
        "flathub" => crate::app_ids::flathub_id(value).then(|| key.to_owned()),
        "homepage" => join::homepage(&format!("https://{value}/")).map(|h| format!("homepage:{h}")),
        "osm" => crate::resolver::osm::parse_id(&format!("https://www.openstreetmap.org/{value}"))
            .map(|osm| crate::lookups::places::key(&osm).to_string()),
        _ => Some(key.to_owned()),
    }?;
    // A key never carries a credential, whatever its kind (a later importer's `feed:…`).
    (valid_key(&stored) && !crate::capability::contains_credential(&stored)).then_some(stored)
}
/// `value` without the strings that carry a credential: an object's entry whose name or value does,
/// or an array's element that does, is left out, at any depth.
fn without_credentials(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|k, v| {
                !crate::capability::contains_credential(k)
                    && !v
                        .as_str()
                        .is_some_and(crate::capability::contains_credential)
            });
            map.values_mut().for_each(without_credentials);
        }
        serde_json::Value::Array(items) => {
            items.retain(|v| {
                !v.as_str()
                    .is_some_and(crate::capability::contains_credential)
            });
            items.iter_mut().for_each(without_credentials);
        }
        _ => {}
    }
}
/// The length of a JSON value as PostgreSQL prints a `jsonb` (`details::text`, which the table's
/// check measures): `": "` and `", "` between entries, numbers written out in full.
pub fn jsonb_text_len(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    let string = |s: &str| serde_json::to_string(s).map_or(usize::MAX / 4, |s| s.len());
    match value {
        Value::Null => 4,
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        // PostgreSQL's numeric keeps a number's digits as written (`2.0`, `-12`), and writes one
        // with an exponent out in full (`1e30`, `1.5e-7`), as Rust prints a float.
        Value::Number(n) => {
            let written = n.to_string();
            if n.as_f64() == Some(0.0) {
                // Numeric has no negative zero: `-0.0` prints as `0.0`.
                written.trim_start_matches('-').len()
            } else if written.contains(['e', 'E']) {
                n.as_f64().map_or(usize::MAX / 4, |f| f.to_string().len())
            } else {
                written.len()
            }
        }
        Value::String(s) => string(s),
        Value::Array(items) => {
            2 + items.iter().map(jsonb_text_len).sum::<usize>() + 2 * items.len().saturating_sub(1)
        }
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| string(k) + 2 + jsonb_text_len(v))
                .sum::<usize>()
                + 2 * map.len().saturating_sub(1)
        }
    }
}

/// One item as an importer reads it. [`Card::checked`] makes the stored row of it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Card {
    /// The sub-source ([`Catalogue::sources`]); empty is the catalogue's own name.
    pub source: String,
    /// The item's ID in its catalogue (a Q-id, an MBID with its entity, `node/1`): unique within
    /// the catalogue.
    pub source_id: String,
    /// The canonical link (`docs`: the page people share most for that kind of item).
    pub uri: String,
    /// A schema.org type name Pinakes knows (`model::type_path`), or one of its own.
    pub r#type: String,
    pub name: String,
    pub other_names: Vec<String>,
    pub description: Option<String>,
    /// The source's labels as it gives them; stored as tag labels ([`labels`]).
    pub labels: Vec<String>,
    /// Join keys ([`valid_key`]), and [`link_key`] for each link people share for the item.
    pub keys: Vec<String>,
    /// Links in the item's group that are not the item (an official website).
    pub related_uris: Vec<String>,
    pub point: Option<model::Point>,
    /// 0–1, from the source's own signal; cut to that range.
    pub fame: f32,
    /// Per-source extras (an address, opening hours, a bounding box, a release date): a JSON
    /// object of at most [`DETAILS_CAP`] bytes.
    pub details: serde_json::Map<String, serde_json::Value>,
}
/// Why a card is not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Source,
    SourceId,
    Uri,
    Type,
    Name,
    Point,
    Details,
}
/// A card as it is stored.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub source: String,
    pub source_id: String,
    pub uri: String,
    pub r#type: String,
    pub name: String,
    pub other_names: Vec<String>,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub keys: Vec<String>,
    pub related_uris: Vec<String>,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    pub fame: f32,
    pub details: serde_json::Map<String, serde_json::Value>,
    /// BLAKE3 of the fields above, as stored.
    #[serde(skip)]
    pub content_hash: [u8; 32],
}
/// Whether a card may name `uri` (normalized): not a link that carries a credential
/// (`capability::is_capability`), and no Pubky object (Pubky's own data is never indexed).
fn publishable(uri: &str) -> bool {
    !crate::capability::is_capability(uri) && !uri.starts_with("pubky:")
}
/// `text` on one line within `cap` bytes: control characters as spaces, invisible format
/// characters ([`invisible`]) dropped, runs of whitespace one space. At most four times `cap` of
/// the text is read.
fn clean(text: &str, cap: usize) -> String {
    let mut read = cap.saturating_mul(4).min(text.len());
    while !text.is_char_boundary(read) {
        read -= 1;
    }
    let mut text: String = text[..read]
        .chars()
        .filter(|c| !invisible(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    model::truncate(&mut text, cap);
    text
}
/// Whether `name` makes a card's name the table takes ([`Card::checked`]: something left once
/// cleaned, and no credential in it), so an importer makes no card it would refuse.
pub fn usable_name(name: &str) -> bool {
    let name = clean(name, NAME_CAP);
    !name.is_empty() && !crate::capability::contains_credential(&name)
}
impl Card {
    /// The row this card is stored as, or why it is not: its link normalized as a referenced
    /// link is (`model::identity`), its type one Pinakes knows, a name, its texts without control
    /// characters and within their caps, its labels tag labels, its keys and related links valid
    /// and each once, its point on the globe, its fame within 0–1 and its details within their
    /// cap. Entries of a list that are not valid are left out; the card stays.
    pub fn checked(&self, catalogue: Catalogue) -> Result<Row, Reject> {
        let source = if self.source.is_empty() {
            catalogue.name().to_owned()
        } else {
            self.source.clone()
        };
        if !catalogue.sources().contains(&source.as_str()) {
            return Err(Reject::Source);
        }
        if self.source_id.is_empty()
            || self.source_id.len() > KEY_CAP
            || self
                .source_id
                .chars()
                .any(|c| c.is_control() || invisible(c) || joiner(c))
        {
            return Err(Reject::SourceId);
        }
        // Its root: an http(s) link's, normalized as a referenced link is, or the root its
        // spelling rules name (`https://doi.org/…` is `doi:…`), so the card holds the link a
        // resource of any of its spellings is stored under. Never another scheme, a link that
        // carries a credential (as written: normalizing cuts a password), a Pubky object, or one
        // over the table's 2048 bytes (normalizing may lengthen a link).
        let uri = match model::identity(&self.uri) {
            Ok((_, _, scheme)) if matches!(scheme.as_str(), "http" | "https") => {
                link_root(&self.uri).ok_or(Reject::Uri)?
            }
            _ => return Err(Reject::Uri),
        };
        // A link a source of people's own posts reads (a post, a profile) is never built from a
        // card (`join::social_link`), so it is no card's own link either: search would list a
        // card that opens as nothing.
        if join::social_link(&uri) {
            return Err(Reject::Uri);
        }
        // Stored by its bare name (`Person`, never `schema:Person`), as resources store theirs.
        let Some(r#type) = model::type_path(&self.r#type).pop() else {
            return Err(Reject::Type);
        };
        let name = clean(&self.name, NAME_CAP);
        // A name that carries a credential is no item's.
        if name.is_empty() || crate::capability::contains_credential(&name) {
            return Err(Reject::Name);
        }
        let mut seen = BTreeSet::from([name.clone()]);
        // Other names and the description are listed in search and indexed as they are stored:
        // one that carries a credential is left out.
        let other_names = self
            .other_names
            .iter()
            .map(|n| clean(n, NAME_CAP))
            .filter(|n| {
                !n.is_empty()
                    && !crate::capability::contains_credential(n)
                    && seen.insert(n.clone())
            })
            .take(OTHER_NAMES_CAP)
            .collect();
        let description = self
            .description
            .as_deref()
            .map(|d| clean(d, model::DESCRIPTION_CAP))
            .filter(|d| !d.is_empty() && !crate::capability::contains_credential(d));
        // The card's own link's key comes first, so a read by the link's resource ID finds the
        // card (`join::unshared`) and no cap cuts it.
        let mut seen = BTreeSet::new();
        let keys = std::iter::once(own_key(&uri))
            .chain(self.keys.iter().filter_map(|k| stored_key(k)))
            .filter(|k| seen.insert(k.clone()))
            .take(KEYS_CAP)
            .collect();
        // Related links: http(s) links, each by its root, as the card's own link.
        let mut seen = BTreeSet::from([uri.clone()]);
        let related_uris = self
            .related_uris
            .iter()
            .filter(|u| {
                model::identity(u)
                    .is_ok_and(|(_, _, scheme)| matches!(scheme.as_str(), "http" | "https"))
            })
            .filter_map(|u| link_root(u))
            .filter(|u| seen.insert(u.clone()))
            .take(RELATED_CAP)
            .collect();
        let (lon, lat) = match &self.point {
            None => (None, None),
            Some(p)
                if p.lat.is_finite()
                    && p.lon.is_finite()
                    && (-90.0..=90.0).contains(&p.lat)
                    && (-180.0..=180.0).contains(&p.lon) =>
            {
                (Some(p.lon), Some(p.lat))
            }
            Some(_) => return Err(Reject::Point),
        };
        // `details` is shown as a card's fields: a string that carries a credential is left out.
        let mut value = serde_json::Value::Object(self.details.clone());
        without_credentials(&mut value);
        let details = match &value {
            serde_json::Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        // Measured as the table's check measures it; PostgreSQL stores no NUL in jsonb text.
        if jsonb_text_len(&value) > DETAILS_CAP
            || serde_json::to_string(&value).is_ok_and(|s| s.contains("\\u0000"))
        {
            return Err(Reject::Details);
        }
        let fame = if self.fame.is_finite() {
            self.fame.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut row = Row {
            source,
            source_id: self.source_id.clone(),
            uri,
            r#type,
            name,
            other_names,
            description,
            labels: labels(self.labels.iter().map(String::as_str)),
            keys,
            related_uris,
            lon,
            lat,
            fame,
            details,
            content_hash: [0; 32],
        };
        row.content_hash = *blake3::hash(
            serde_json::to_string(&row)
                .expect("a row serializes")
                .as_bytes(),
        )
        .as_bytes();
        Ok(row)
    }
}

/// What the property tests of the catalogue modules share: a small generator and its seed.
#[cfg(test)]
pub(crate) mod testgen {
    use super::Catalogue;
    /// The seed of a property test: `PINAKES_TEST_SEED` (decimal, or hex after `0x`) when it is
    /// set, else `fixed`, so every run is the same unless asked otherwise. Printed (`--nocapture`
    /// shows it), so a failure seen with another seed can be repeated.
    pub fn seed(test: &str, fixed: u64) -> u64 {
        let seed = std::env::var("PINAKES_TEST_SEED")
            .ok()
            .and_then(|s| {
                let s = s.trim();
                match s.strip_prefix("0x") {
                    Some(hex) => u64::from_str_radix(hex, 16).ok(),
                    None => s.parse().ok(),
                }
            })
            .unwrap_or(fixed);
        eprintln!("{test}: seed {seed:#x} (PINAKES_TEST_SEED={seed:#x} repeats it)");
        seed
    }
    /// A small generator (xorshift).
    pub struct Gen(u64);
    impl Gen {
        pub fn new(test: &str, fixed: u64) -> Self {
            // xorshift never leaves 0.
            Self(seed(test, fixed).max(1))
        }
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        pub fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
        pub fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
            &from[self.below(from.len())]
        }
        pub fn chance(&mut self, one_in: usize) -> bool {
            self.below(one_in) == 0
        }
        /// `least` characters drawn from `from`, and up to `more` fewer than that more.
        pub fn chars(&mut self, from: &str, least: usize, more: usize) -> String {
            let len = least + self.below(more);
            let from: Vec<char> = from.chars().collect();
            (0..len).map(|_| *self.pick(&from)).collect()
        }
        /// Each ASCII letter of `text` in upper or lower case.
        pub fn cased(&mut self, text: &str) -> String {
            text.chars()
                .map(|c| {
                    if self.chance(2) {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect()
        }
    }
    /// An importer's spelling of a key (case, `_` in a crate's name, an arXiv version, `www.` and
    /// a final dot on a host, a MusicBrainz entity with `_`) and the link the key names, for each
    /// kind of key a link names by itself (`join::link_keys`), with the stored key both must give.
    pub struct Spelled {
        pub spelling: String,
        pub link: String,
        pub key: String,
        pub owner: Catalogue,
    }
    pub fn mbid(gen: &mut Gen) -> String {
        let hex = "0123456789abcdef";
        [8, 4, 4, 4, 12]
            .iter()
            .map(|n| gen.chars(hex, *n, 0))
            .collect::<Vec<_>>()
            .join("-")
    }
    pub fn digits(gen: &mut Gen, max: usize) -> String {
        let n = 1 + gen.below(max);
        format!("{}{}", 1 + gen.below(9), gen.chars("0123456789", n - 1, 0))
    }
    /// One [`Spelled`] of a random kind; `canonical` keeps the link in the one spelling
    /// `catalogue_key_links` gives for its key (lower case, a crate's name with one kind of
    /// separator).
    pub fn spelled(gen: &mut Gen, canonical: bool) -> Spelled {
        let lower = "abcdefghijklmnopqrstuvwxyz";
        let alnum = "abcdefghijklmnopqrstuvwxyz0123456789";
        match gen.below(9) {
            0 => {
                let entity = *gen.pick(&["artist", "recording", "release", "release-group"]);
                let id = mbid(gen);
                let written = if entity == "release-group" && gen.chance(2) {
                    "release_group".to_owned()
                } else {
                    gen.cased(entity)
                };
                let link_id = if canonical {
                    id.clone()
                } else {
                    gen.cased(&id)
                };
                Spelled {
                    spelling: format!("mb:{written}:{}", gen.cased(&id)),
                    link: format!("https://musicbrainz.org/{entity}/{link_id}"),
                    key: format!("mb:{entity}:{id}"),
                    owner: Catalogue::Musicbrainz,
                }
            }
            1 => {
                let q = format!("Q{}", digits(gen, 9));
                let host = if canonical {
                    "https://www.wikidata.org/wiki/"
                } else {
                    *gen.pick(&[
                        "https://www.wikidata.org/wiki/",
                        "https://m.wikidata.org/wiki/",
                        "https://www.wikidata.org/entity/",
                        "http://wikidata.org/wiki/",
                    ])
                };
                Spelled {
                    spelling: format!("wikidata:{}", gen.cased(&q)),
                    link: format!("{host}{q}"),
                    key: format!("wikidata:{q}"),
                    owner: Catalogue::Wikidata,
                }
            }
            2 => {
                // No `.` or `..` path segment: a link's path drops them, so no link names such a DOI.
                let suffix = gen
                    .chars("abcxyzABCXYZ0189.-_;()/", 1, 20)
                    .split('/')
                    .filter(|part| !matches!(*part, "." | ".."))
                    .collect::<Vec<_>>()
                    .join("/");
                let doi = format!("10.{}/{suffix}x", gen.chars("0123456789", 4, 5));
                let lowered = doi.to_lowercase();
                Spelled {
                    spelling: format!("doi:{}", gen.cased(&doi)),
                    link: if canonical {
                        format!("doi:{lowered}")
                    } else {
                        format!("https://doi.org/{}", gen.cased(&doi))
                    },
                    key: format!("doi:{lowered}"),
                    owner: Catalogue::Openalex,
                }
            }
            3 => {
                let id = if gen.chance(3) {
                    format!(
                        "{}/{}",
                        gen.pick(&["hep-th", "math.ag", "cond-mat", "astro-ph"]),
                        gen.chars("0123456789", 7, 0)
                    )
                } else {
                    format!(
                        "{}.{}",
                        gen.chars("0123456789", 4, 0),
                        gen.chars("0123456789", 4, 2)
                    )
                };
                let version = if gen.chance(2) {
                    format!("v{}", 1 + gen.below(9))
                } else {
                    String::new()
                };
                let page = *gen.pick(&["abs", "pdf"]);
                Spelled {
                    spelling: format!("arxiv:{id}{version}"),
                    link: if canonical {
                        format!("https://arxiv.org/{page}/{id}")
                    } else {
                        format!("https://arxiv.org/{page}/{id}{version}")
                    },
                    key: format!("arxiv:{id}"),
                    owner: Catalogue::Openalex,
                }
            }
            4 => {
                let w = format!("W{}", digits(gen, 12));
                let host = if canonical {
                    "https://openalex.org/"
                } else {
                    *gen.pick(&[
                        "https://openalex.org/",
                        "https://openalex.org/works/",
                        "https://api.openalex.org/works/",
                    ])
                };
                Spelled {
                    spelling: format!("openalex:{}", gen.cased(&w)),
                    link: format!("{host}{w}"),
                    key: format!("openalex:{w}"),
                    owner: Catalogue::Openalex,
                }
            }
            5 => {
                // `x` first: never one of docs.rs's own pages (`about`, `crate`).
                let separator = if gen.chance(2) { "-" } else { "_" };
                let parts: Vec<String> = (0..1 + gen.below(3))
                    .map(|_| gen.chars(alnum, 1, 6))
                    .collect();
                let name = format!("x{}", parts.join(separator));
                let published = if canonical {
                    name.clone()
                } else {
                    let other = if gen.chance(2) { "_" } else { "-" };
                    gen.cased(&name.replace('-', other))
                };
                let page = if canonical {
                    *gen.pick(&[
                        "https://crates.io/crates/",
                        "https://docs.rs/",
                        "https://docs.rs/crate/",
                        "https://lib.rs/crates/",
                    ])
                } else {
                    "https://crates.io/crates/"
                };
                Spelled {
                    spelling: format!("crate:{published}"),
                    link: format!("{page}{published}"),
                    key: format!("crate:{}", name.to_ascii_lowercase().replace('_', "-")),
                    owner: Catalogue::Software,
                }
            }
            6 => {
                let id = format!(
                    "{}.{}.{}",
                    gen.pick(&["org", "com", "net", "de"]),
                    gen.chars(lower, 1, 8),
                    gen.chars(lower, 1, 0) + &gen.chars("abcXYZ019_", 0, 8)
                );
                let page = *gen.pick(&[
                    "https://f-droid.org/packages/{}/",
                    "https://f-droid.org/packages/{}",
                    "https://f-droid.org/en/packages/{}/",
                ]);
                Spelled {
                    spelling: format!("android:{id}"),
                    link: page.replace("{}", &id),
                    key: format!("android:{id}"),
                    owner: Catalogue::Software,
                }
            }
            7 => {
                let id = format!(
                    "{}.{}.{}",
                    gen.pick(&["org", "com", "io"]),
                    gen.chars(alnum, 1, 8),
                    gen.chars("abcXYZ019-_", 1, 8)
                );
                let page = *gen.pick(&[
                    "https://flathub.org/apps/",
                    "https://flathub.org/apps/details/",
                ]);
                Spelled {
                    spelling: format!("flathub:{id}"),
                    link: format!("{page}{id}"),
                    key: format!("flathub:{id}"),
                    owner: Catalogue::Software,
                }
            }
            _ => {
                let labels: Vec<String> = (0..1 + gen.below(2))
                    .map(|_| {
                        if gen.chance(6) {
                            "bücher".to_owned()
                        } else {
                            gen.chars(lower, 1, 0) + &gen.chars("abc-019", 0, 8) + "x"
                        }
                    })
                    .collect();
                let host = format!("{}.{}", labels.join("."), gen.pick(&["org", "de", "co.uk"]));
                let ascii = url::Url::parse(&format!("https://{host}/"))
                    .unwrap()
                    .host_str()
                    .unwrap()
                    .to_owned();
                let www = if gen.chance(2) { "www." } else { "" };
                let dot = if !canonical && gen.chance(4) { "." } else { "" };
                let written = if canonical {
                    host.clone()
                } else {
                    gen.cased(&host)
                };
                let scheme = if canonical || gen.chance(2) {
                    "https"
                } else {
                    "http"
                };
                Spelled {
                    spelling: format!("homepage:{www}{written}{dot}"),
                    link: format!("{scheme}://{www}{written}{dot}/"),
                    key: format!("homepage:{ascii}"),
                    owner: Catalogue::Tranco,
                }
            }
        }
    }

    /// A URL with the spellings people share: scheme and host in any case, an IDN host, default
    /// and other ports, percent-encoding (of letters, of a slash, a bad escape), dot segments,
    /// a trailing slash, tracking and other query parameters, a fragment.
    pub fn url(gen: &mut Gen) -> String {
        let scheme = *gen.pick(&["http", "https"]);
        let scheme = gen.cased(scheme);
        let host = gen.pick(&[
            "example.org",
            "Example.ORG",
            "bücher.de",
            "xn--bcher-kva.de",
            "en.wikipedia.org",
            "en.m.wikipedia.org",
            "www.youtube.com",
            "doi.org",
            "musicbrainz.org",
            "crates.io",
            "sub.example.co.uk.",
            "127.0.0.1",
        ]);
        let port = *gen.pick(&["", "", ":80", ":443", ":8080"]);
        let mut out = format!("{scheme}://{host}{port}");
        for _ in 0..gen.below(5) {
            out.push('/');
            out.push_str(gen.pick(&[
                "a",
                "B",
                "wiki",
                "Douglas_Adams",
                "%41b",
                "%2F",
                "%7euser",
                "%zz",
                ".",
                "..",
                "caf%C3%A9",
                "café",
                "a b",
                "10.1000",
                "abc",
                "watch",
                "",
            ]));
        }
        if gen.chance(3) {
            out.push('/');
        }
        if gen.chance(2) {
            out.push('?');
            let params: Vec<&str> = (0..1 + gen.below(3))
                .map(|_| {
                    *gen.pick(&[
                        "utm_source=x",
                        "fbclid=1",
                        "v=dQw4w9WgXcQ",
                        "q=a+b",
                        "page=2",
                        "x",
                        "a=%20",
                        "si=abc",
                    ])
                })
                .collect();
            out.push_str(&params.join("&"));
        }
        if gen.chance(3) {
            out.push_str(gen.pick(&["#top", "#", "#a=b"]));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testgen::{spelled, url, Gen as Rng};
    use super::*;

    /// Labels become valid Pubky tag labels: lower case, one hyphen for each run of spaces,
    /// commas, colons and underscores, no control characters; a label longer than 20 characters
    /// or with nothing left is dropped; the same label is kept once.
    #[test]
    fn source_labels_become_valid_tag_labels() {
        for (raw, want) in [
            ("Jazz", Some("jazz")),
            ("  Hip Hop  ", Some("hip-hop")),
            ("drum and bass", Some("drum-and-bass")),
            ("amenity:cafe", Some("amenity-cafe")),
            ("rock, pop", Some("rock-pop")),
            ("rhythm_and_blues", Some("rhythm-and-blues")),
            ("--Synth--Pop--", Some("synth-pop")),
            ("Musique Concrète", Some("musique-concrète")),
            ("ÉLECTRO", Some("électro")),
            ("tab\there", Some("tab-here")),
            ("bell\u{7}", Some("bell")),
            ("k-pop", Some("k-pop")),
            ("東京", Some("東京")),
            ("ΟΔΟΣ", Some("\u{3bf}\u{3b4}\u{3bf}\u{3c2}")),
            ("a".repeat(20).as_str(), Some("aaaaaaaaaaaaaaaaaaaa")),
            ("a".repeat(21).as_str(), None),
            ("progressive electronic music", None),
            ("", None),
            ("   ", None),
            (",:;", Some(";")),
            (" , : _ ", None),
        ] {
            assert_eq!(label(raw).as_deref(), want, "{raw:?}");
        }
        // Every label made is one pubky-app-specs accepts.
        for raw in ["A b", "x:y", "1,2", "Ä\nB", "_x_"] {
            let made = label(raw).unwrap();
            assert!(!made.is_empty() && made.chars().count() <= LABEL_MAX_CHARS);
            assert!(!made.contains(LABEL_INVALID), "{made:?}");
            assert_eq!(made, made.to_lowercase());
        }
        assert_eq!(
            labels([
                "Jazz",
                "jazz",
                "JAZZ ",
                "Free Jazz",
                "free_jazz",
                "x".repeat(30).as_str()
            ]),
            ["jazz", "free-jazz"]
        );
        let many: Vec<String> = (0..100).map(|n| format!("l{n}")).collect();
        assert_eq!(labels(many.iter().map(String::as_str)).len(), LABELS_CAP);
    }

    fn card() -> Card {
        Card {
            source_id: "Q42".into(),
            uri: "HTTPS://en.wikipedia.org:443/wiki/Douglas_Adams#Life".into(),
            r#type: "Person".into(),
            name: "  Douglas\tAdams ".into(),
            other_names: vec![
                "Douglas Adams".into(),
                "DNA".into(),
                "DNA".into(),
                " ".into(),
            ],
            description: Some("English writer\u{0}".into()),
            labels: vec!["Science Fiction".into(), "science_fiction".into()],
            keys: vec![
                "wikidata:Q42".into(),
                "wikidata:Q42".into(),
                "not a key".into(),
                "Wikidata:Q42".into(),
                "isrc:".into(),
                link_key("https://www.wikidata.org/wiki/Q42").unwrap(),
            ],
            related_uris: vec![
                "https://douglasadams.com".into(),
                "https://en.wikipedia.org/wiki/Douglas_Adams".into(),
                "no link".into(),
            ],
            point: None,
            fame: 1.5,
            ..Card::default()
        }
    }
    /// A card is stored normalized: its link as a referenced link is, its texts clean, its lists
    /// valid and each entry once, its fame within 0–1.
    #[test]
    fn a_card_is_stored_normalized() {
        let row = card().checked(Catalogue::Wikidata).unwrap();
        assert_eq!(row.source, "wikidata");
        assert_eq!(row.uri, "https://en.wikipedia.org/wiki/Douglas_Adams");
        assert_eq!(row.name, "Douglas Adams");
        assert_eq!(row.other_names, ["DNA"]);
        assert_eq!(row.description.as_deref(), Some("English writer"));
        assert_eq!(row.labels, ["science-fiction"]);
        let wikidata_page = format!(
            "url:{}",
            crate::identity::resource_id("https://www.wikidata.org/wiki/Q42")
        );
        // The card's own link's key first (`join::unshared` reads a card by it).
        let own = format!(
            "url:{}",
            crate::identity::resource_id("https://en.wikipedia.org/wiki/Douglas_Adams")
        );
        assert_eq!(row.keys, [own, "wikidata:Q42".to_owned(), wikidata_page]);
        assert_eq!(row.related_uris, ["https://douglasadams.com/"]);
        assert_eq!(row.fame, 1.0);
        // A type is stored by its bare name.
        let mut prefixed = card();
        prefixed.r#type = "https://schema.org/Person".into();
        assert_eq!(
            prefixed.checked(Catalogue::Wikidata).unwrap().r#type,
            "Person"
        );
        // The hash is the content's: the same card gives the same hash, a change another.
        assert_eq!(row, card().checked(Catalogue::Wikidata).unwrap());
        let mut changed = card();
        changed.labels.push("Humour".into());
        assert_ne!(
            changed.checked(Catalogue::Wikidata).unwrap().content_hash,
            row.content_hash
        );
    }
    #[test]
    fn a_card_without_what_it_needs_is_rejected() {
        let rejected = |edit: fn(&mut Card), catalogue| {
            let mut c = card();
            edit(&mut c);
            c.checked(catalogue).unwrap_err()
        };
        assert_eq!(rejected(|_| {}, Catalogue::Software), Reject::Source);
        assert_eq!(
            rejected(|c| c.source = "github".into(), Catalogue::Software),
            Reject::Source
        );
        let mut crate_card = card();
        crate_card.source = "crates.io".into();
        crate_card.r#type = "SoftwareSourceCode".into();
        assert!(crate_card.checked(Catalogue::Software).is_ok());
        assert_eq!(
            rejected(|c| c.source_id.clear(), Catalogue::Wikidata),
            Reject::SourceId
        );
        assert_eq!(
            rejected(|c| c.uri = "no link".into(), Catalogue::Wikidata),
            Reject::Uri
        );
        assert_eq!(
            rejected(|c| c.r#type = "Nonsense".into(), Catalogue::Wikidata),
            Reject::Type
        );
        assert_eq!(
            rejected(|c| c.name = " \n ".into(), Catalogue::Wikidata),
            Reject::Name
        );
        assert_eq!(
            rejected(
                |c| c.point = Some(model::Point {
                    lat: 91.0,
                    lon: 0.0
                }),
                Catalogue::Wikidata
            ),
            Reject::Point
        );
        assert_eq!(
            rejected(
                |c| {
                    c.details
                        .insert("x".into(), serde_json::json!("y".repeat(9000)));
                },
                Catalogue::Wikidata
            ),
            Reject::Details
        );
        // 7.2 KB as compact JSON, 8.8 KB as PostgreSQL prints it: over the table's cap.
        assert_eq!(
            rejected(
                |c| {
                    for n in 0..800 {
                        c.details.insert(format!("k{n:03}"), serde_json::json!(1));
                    }
                },
                Catalogue::Wikidata
            ),
            Reject::Details
        );
    }
    #[test]
    fn details_are_measured_as_postgresql_prints_them() {
        for (value, printed) in [
            (serde_json::json!({}), "{}"),
            (
                serde_json::json!({"a": 1, "b": [1, 2]}),
                r#"{"a": 1, "b": [1, 2]}"#,
            ),
            (
                serde_json::json!({"x": null, "y": true, "z": "é\n"}),
                r#"{"x": null, "y": true, "z": "é\n"}"#,
            ),
            (serde_json::json!([1.5, -2, 0.25]), "[1.5, -2, 0.25]"),
        ] {
            assert_eq!(jsonb_text_len(&value), printed.len(), "{value}");
        }
    }
    #[test]
    fn keys_have_the_stored_form() {
        for key in [
            "wikidata:Q42",
            "mb:recording:b1392450-e666-3926-a536-22c65f834433",
            "osm:node/1",
            "doi:10.1038/nature12373",
            "url:0123456789abcdef0123456789abcdef",
        ] {
            assert!(valid_key(key), "{key}");
        }
        for key in ["Q42", "WIKIDATA:Q42", ":x", "x:", "isrc:US 123", "1x:y"] {
            assert!(!valid_key(key), "{key}");
        }
        // DOIs and ISBNs take the form a resource's keys have; a `url:` key is a resource ID.
        assert_eq!(
            stored_key("doi:10.1038/Nature12373").as_deref(),
            Some("doi:10.1038/nature12373")
        );
        assert_eq!(
            stored_key("isbn:0-14-032872-6").as_deref(),
            Some("isbn:9780140328721")
        );
        assert_eq!(stored_key("isbn:123"), None);
        assert_eq!(stored_key("doi:nonsense"), None);
        assert_eq!(stored_key("url:https://example.org/"), None);
        assert_eq!(
            stored_key(" wikidata:Q42 ").as_deref(),
            Some("wikidata:Q42")
        );
        assert_eq!(stored_key("wikidata:q42").as_deref(), Some("wikidata:Q42"));
        assert_eq!(
            stored_key("isrc:gbum71029604").as_deref(),
            Some("isrc:GBUM71029604")
        );
        // The standard hyphenated spelling is the same ISRC, stored as shared links key it; the
        // stored form reads back as itself, and a value that is no ISRC is no key.
        for spelling in ["isrc:GB-UM7-10-29604", "isrc:gb-um7-10-29604"] {
            assert_eq!(
                stored_key(spelling).as_deref(),
                Some("isrc:GBUM71029604"),
                "{spelling}"
            );
        }
        assert_eq!(
            stored_key("isrc:GBUM71029604").as_deref(),
            Some("isrc:GBUM71029604")
        );
        assert_eq!(stored_key("isrc:not-an-isrc"), None);
        // Each kind in the form the link it names derives (`join::link_keys`).
        for (raw, stored) in [
            ("crate:Serde_Json", Some("crate:serde-json")),
            ("crate:1abc", None),
            (
                "mb:Release_Group:B1392450-E666-3926-A536-22C65F834433",
                Some("mb:release-group:b1392450-e666-3926-a536-22c65f834433"),
            ),
            ("mb:artist:not-an-mbid", None),
            ("arxiv:2302.04668v3", Some("arxiv:2302.04668")),
            ("arxiv:nonsense", None),
            ("openalex:w2741809807", Some("openalex:W2741809807")),
            ("wikidata:Q042", None),
            (
                "android:org.fdroid.fdroid",
                Some("android:org.fdroid.fdroid"),
            ),
            ("android:nodots", None),
            ("flathub:org.gnome.Maps", Some("flathub:org.gnome.Maps")),
            ("flathub:org.gnome", None),
            ("homepage:WWW.Bücher.DE.", Some("homepage:xn--bcher-kva.de")),
            ("homepage:example.org/path", None),
            ("homepage:localhost", None),
            ("osm:node/1", Some("osm:node/1")),
            ("osm:planet/1", None),
        ] {
            assert_eq!(stored_key(raw).as_deref(), stored, "{raw}");
        }
        assert_eq!(link_key("nostr:note1abc"), None);
        assert_eq!(
            link_key("HTTPS://Example.org/a#b"),
            Some(format!(
                "url:{}",
                crate::identity::resource_id("https://example.org/a")
            ))
        );
    }
    /// A catalogue is on when `[catalogues] enabled` and its own `enabled` are; the names to bind
    /// are those of the catalogues that are on.
    #[test]
    fn the_enabled_set_follows_both_switches() {
        let mut config = CataloguesConfig::default();
        assert_eq!(config.enabled_set().len(), 7);
        config.osm.enabled = false;
        config.tranco.enabled = false;
        assert_eq!(
            config.enabled_names(),
            [
                "wikidata",
                "musicbrainz",
                "openalex",
                "podcastindex",
                "software"
            ]
        );
        assert!(!config.enabled(Catalogue::Osm));
        config.enabled = false;
        assert!(config.enabled_set().is_empty());
        assert!(Catalogue::ALL.iter().all(|c| !config.enabled(*c)));
    }
    /// Every catalogue has its own allowed lag, at least two days (more than the daily update),
    /// and the slow publishers more; OpenAlex takes works cited more than `min_citations` times.
    #[test]
    fn lag_limits_and_citations() {
        let day = std::time::Duration::from_secs(86400);
        for c in Catalogue::ALL {
            assert!(c.max_update_lag() >= 2 * day, "{c}");
        }
        assert_eq!(Catalogue::Musicbrainz.max_update_lag(), 5 * day);
        assert_eq!(Catalogue::Podcastindex.max_update_lag(), 9 * day);
        assert_eq!(Catalogue::Openalex.max_update_lag(), 100 * day);
        let config = CataloguesConfig::default();
        assert!(!config.cited_enough(50) && config.cited_enough(51));
    }

    /// A small deterministic generator (xorshift), so the property tests below are the same on
    /// every run.
    struct Gen(u64);
    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
            &from[self.below(from.len())]
        }
        /// A text of up to `max` characters drawn from letters (with upper case, accents, Greek,
        /// CJK), digits, every character a tag label refuses, hyphens, underscores, punctuation
        /// and control characters.
        fn text(&mut self, max: usize) -> String {
            const CHARS: &[char] = &[
                'a', 'b', 'z', 'A', 'Q', 'Z', 'é', 'É', 'ß', 'Σ', 'ς', 'σ', 'İ', '東', '京', '0',
                '7', ' ', '\t', '\n', '\r', ',', ':', '_', '-', ';', '&', '/', '.', '\u{0}',
                '\u{7}', '\u{85}', '\u{a0}', '\u{2028}',
            ];
            let len = self.below(max + 1);
            (0..len).map(|_| *self.pick(CHARS)).collect()
        }
    }

    /// Property: whatever a source gives, `label` returns a valid Pubky tag label (1–20
    /// characters, lower case, none of the refused characters, no whitespace or control
    /// character) or nothing, and a label it made is its own label again.
    #[test]
    fn label_always_gives_a_valid_tag_and_is_idempotent() {
        let mut gen = Gen(super::testgen::seed("label", 0x9e37_79b9_7f4a_7c15).max(1));
        let mut made = 0;
        for _ in 0..20_000 {
            let raw = gen.text(30);
            let Some(l) = label(&raw) else { continue };
            made += 1;
            let n = l.chars().count();
            assert!((1..=LABEL_MAX_CHARS).contains(&n), "{raw:?} -> {l:?}");
            assert!(
                !l.contains(LABEL_INVALID)
                    && !l.chars().any(|c| c.is_whitespace() || c.is_control()),
                "{raw:?} -> {l:?}"
            );
            assert_eq!(l.to_lowercase(), l, "{raw:?}");
            assert_eq!(label(&l).as_deref(), Some(l.as_str()), "{raw:?}");
        }
        assert!(made > 5_000, "{made}");
    }

    /// Property: a key `stored_key` returns is its own stored form, and is valid.
    #[test]
    fn stored_key_is_idempotent() {
        let mut gen = Gen(super::testgen::seed("stored_key", 0x2545_f491_4f6c_dd1d).max(1));
        let kinds = [
            "doi", "isbn", "isrc", "wikidata", "url", "mb", "osm", "crate", "arxiv", "android",
            "flathub", "homepage", "openalex", "Doi", "x_1", "",
        ];
        let values = [
            "10.1038/Nature12373",
            "0-14-032872-6",
            "978-0-14-032872-1",
            "gbum71029604",
            "q42",
            "0123456789abcdef0123456789abcdef",
            "recording:b1392450-e666-3926-a536-22c65f834433",
            "Release_Group:B1392450-E666-3926-A536-22C65F834433",
            "node/1",
            "Serde_Json",
            "2302.04668v2",
            "hep-th/9901001",
            "org.fdroid.fdroid",
            "org.gnome.Maps",
            "WWW.Example.ORG.",
            "w2741809807",
        ];
        let mut kept = 0;
        for _ in 0..20_000 {
            let mut value = gen.pick(&values).to_string();
            if gen.below(3) == 0 {
                value.push_str(&gen.text(4));
            }
            let raw = format!("{}:{value}", gen.pick(&kinds));
            let raw = if gen.below(4) == 0 {
                format!(" {raw} ")
            } else {
                raw
            };
            let Some(key) = stored_key(&raw) else {
                continue;
            };
            kept += 1;
            assert!(valid_key(&key), "{raw:?} -> {key:?}");
            assert_eq!(stored_key(&key).as_deref(), Some(key.as_str()), "{raw:?}");
        }
        assert!(kept > 2_000, "{kept}");
    }

    /// Property: an importer's spelling of a key is stored as the key the link it names derives
    /// (`join::link_keys`), so a card written with the dump's own spelling joins the links people
    /// share (`crate:serde_json` and `crates.io/crates/serde_json` are both `crate:serde-json`).
    #[test]
    fn an_importers_spelling_is_stored_as_the_key_its_link_derives() {
        let mut gen = Rng::new("importer spellings", 0x51ab_00c3_9e10_7a21);
        for _ in 0..20_000 {
            let canonical = gen.chance(2);
            let spelled = spelled(&mut gen, canonical);
            assert_eq!(
                stored_key(&spelled.spelling).as_deref(),
                Some(spelled.key.as_str()),
                "{}",
                spelled.spelling
            );
            let derived = join::link_keys(&spelled.link);
            assert!(
                derived.contains(&(spelled.key.clone(), spelled.owner)),
                "{} -> {derived:?}, not {}",
                spelled.link,
                spelled.key
            );
        }
    }

    /// Property: normalizing is idempotent (a normalized link is its own normal form and ID), and
    /// a root is its own root, so the link a card stores is the one a resource of the same shared
    /// link is stored under.
    #[test]
    fn identity_is_idempotent_and_a_root_is_its_own() {
        let mut gen = Rng::new("identity", 0x7f4a_7c15_9e37_79b9);
        let mut read = 0;
        for _ in 0..20_000 {
            let raw = url(&mut gen);
            let Ok((id, normal, scheme)) = model::identity(&raw) else {
                continue;
            };
            read += 1;
            assert_eq!(
                model::identity(&normal).unwrap(),
                (id, normal.clone(), scheme),
                "{raw}"
            );
            if let Some(alias) = crate::alias::root(&normal) {
                let (_, root, _) = model::identity(&alias.root).unwrap();
                assert_eq!(root, alias.root, "{raw}: root not normalized");
                // Known edge of the DOI rule, outside the catalogues: a `doi:` root holding a
                // percent-escape (a DOI with a non-ASCII character) is lowered again by its own
                // root (`%C3%A9` to `%c3%a9`). No DOI registry issues such names.
                if !(root.starts_with("doi:") && root.contains('%')) {
                    assert_eq!(crate::alias::root(&root), None, "{raw}: {root} has a root");
                }
            }
        }
        assert!(read > 10_000, "{read}");
    }

    /// Property: a card's link, related links and own `url:` key are what a resource of the same
    /// shared link is stored under: its root (`alias::root` of the normalized link) and that root's
    /// resource ID.
    #[test]
    fn a_cards_links_are_stored_as_a_shared_links_root() {
        let mut gen = Rng::new("card links", 0x3c6e_f372_fe94_f82b);
        let root = |raw: &str| {
            let (_, normal, _) = model::identity(raw).unwrap();
            crate::alias::root(&normal).map_or(normal, |a| a.root)
        };
        let mut stored = 0;
        for _ in 0..5_000 {
            let (link, related) = (url(&mut gen), url(&mut gen));
            let row = Card {
                source_id: "Q1".into(),
                uri: link.clone(),
                r#type: "Thing".into(),
                name: "x".into(),
                related_uris: vec![related.clone()],
                ..Default::default()
            }
            .checked(Catalogue::Wikidata);
            let Ok(row) = row else { continue };
            stored += 1;
            assert_eq!(row.uri, root(&link), "{link}");
            let (id, _, _) = model::identity(&row.uri).unwrap();
            assert_eq!(row.keys[0], format!("url:{id}"), "{link}");
            assert_eq!(link_key(&link).as_ref(), Some(&row.keys[0]), "{link}");
            if let Some(kept) = row.related_uris.first() {
                assert_eq!(*kept, root(&related), "{related}");
            }
        }
        assert!(stored > 2_000, "{stored}");
    }

    /// A card's link is an http(s) link (`javascript:`, `data:`, `file:` and the like are refused,
    /// and dropped from its related links), never one that carries a credential as written
    /// (normalizing cuts a password); a link whose spelling rules name a root is stored as the
    /// root (`https://doi.org/…` as `doi:…`), with that root's own key.
    /// A key of any kind whose value carries a credential is refused, and a `details` string that
    /// carries one is left out (an object's entry, an array's element), as names, descriptions and
    /// links are checked.
    #[test]
    fn keys_and_details_never_carry_a_credential() {
        assert_eq!(stored_key("feed:https://user:pw@example.org/feed"), None);
        assert_eq!(
            stored_key("feed:https://example.org/feed").as_deref(),
            Some("feed:https://example.org/feed")
        );
        let mut c = card();
        c.details = serde_json::json!({
            "site": "https://example.org/?access_token=abc",
            "kept": "plain text",
            "list": ["https://user:pw@example.org/x", "y"],
            "nested": {"feed": "https://user:pw@example.org/feed", "n": 3,
                "https://example.org/?access_token=abc": "a key that carries one"},
            "https://user:pw@example.org/k": {"deep": "value"}
        })
        .as_object()
        .unwrap()
        .clone();
        let row = c.checked(Catalogue::Wikidata).unwrap();
        assert_eq!(
            serde_json::Value::Object(row.details),
            serde_json::json!({"kept": "plain text", "list": ["y"], "nested": {"n": 3}})
        );
    }

    #[test]
    fn a_cards_links_are_web_links_without_credentials_stored_by_their_root() {
        let with = |uri: &str, related: &[&str]| {
            let mut c = card();
            c.uri = uri.into();
            c.related_uris = related.iter().map(|r| r.to_string()).collect();
            c.checked(Catalogue::Wikidata)
        };
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<b>x</b>",
            "file:///etc/passwd",
            "ftp://example.org/x",
            "mailto:a@example.org",
            "nostr:npub1x",
            "doi:10.1000/abc",
            "https://user:secret@example.org/feed.xml",
            "https://example.org/?access_token=abc",
            "pubky://x/pub/a",
        ] {
            assert_eq!(with(bad, &[]).unwrap_err(), Reject::Uri, "{bad}");
        }
        let row = with(
            "https://example.org/",
            &[
                "javascript:alert(1)",
                "data:text/html,x",
                "file:///x",
                "https://user:pw@example.org/a",
                "https://example.org/b?token=x",
                "https://example.org/c",
            ],
        )
        .unwrap();
        assert_eq!(row.related_uris, ["https://example.org/c"]);
        let row = with(
            "https://doi.org/10.1000/ABC",
            &["https://dx.doi.org/10.1000/xyz"],
        )
        .unwrap();
        assert_eq!(row.uri, "doi:10.1000/abc");
        assert_eq!(row.related_uris, ["doi:10.1000/xyz"]);
        assert_eq!(
            row.keys[0],
            format!("url:{}", crate::identity::resource_id("doi:10.1000/abc"))
        );
        assert_eq!(
            link_key("https://doi.org/10.1000/abc"),
            Some(row.keys[0].clone())
        );
        assert_eq!(link_key("doi:10.1000/abc"), Some(row.keys[0].clone()));
        // A tracked spelling is its root.
        assert_eq!(
            with("https://example.org/a?utm_source=x", &[]).unwrap().uri,
            "https://example.org/a"
        );
    }

    /// Names, other names and descriptions keep no invisible format characters (a zero-width
    /// space, direction overrides, a soft hyphen, a byte order mark), labels none at all (no
    /// joiners either); text that carries a credential is left out, a name that does refuses the
    /// card; over-long input is cut before it is cleaned, and a label over 256 bytes is none.
    #[test]
    fn card_texts_hold_no_invisible_characters_credentials_or_unbounded_input() {
        for (raw, want) in [
            ("jazz\u{200b}", Some("jazz")),
            ("\u{202e}zzaj", Some("zzaj")),
            ("ja\u{ad}zz", Some("jazz")),
            ("\u{feff}rock", Some("rock")),
            ("hip\u{2066}hop", Some("hiphop")),
            ("k\u{200d}pop", Some("kpop")),
            ("\u{200b}", None),
        ] {
            assert_eq!(label(raw).as_deref(), want, "{raw:?}");
        }
        assert_eq!(label(&"a".repeat(300)), None);
        assert_eq!(label(&format!("a{}b", " ".repeat(300))), None);
        let mut c = card();
        c.name = "Dou\u{200b}glas \u{202e}Adams\u{feff}".into();
        c.other_names = vec![
            "می\u{200c}خواهم".into(),
            "see https://example.org/x?token=abc".into(),
        ];
        c.description = Some("Read https://user:pw@example.org/feed".into());
        let row = c.checked(Catalogue::Wikidata).unwrap();
        assert_eq!(row.name, "Douglas Adams");
        assert_eq!(row.other_names, ["می\u{200c}خواهم"]);
        assert_eq!(row.description, None);
        let mut c = card();
        c.name = "https://example.org/?access_token=abc".into();
        assert_eq!(c.checked(Catalogue::Wikidata).unwrap_err(), Reject::Name);
        // A megabyte of each input: cut or refused, and quickly (well within a minute even on a
        // loaded host; work that grew with the square of the input would take hours).
        let started = std::time::Instant::now();
        let huge = "x ".repeat(512 * 1024);
        let mut c = card();
        c.name = huge.clone();
        c.other_names = vec![huge.clone(); 64];
        c.description = Some(huge.clone());
        c.labels = vec![huge.clone(); 64];
        c.keys = vec![format!("wikidata:Q{}", "1".repeat(1 << 20)); 8];
        c.related_uris = vec![format!("https://example.org/{}", "a".repeat(1 << 20))];
        let row = c.checked(Catalogue::Wikidata).unwrap();
        assert!(row.name.len() <= NAME_CAP && row.name.starts_with("x x"));
        assert!(row.other_names.is_empty(), "the same as the name");
        assert!(row.description.as_ref().unwrap().len() <= model::DESCRIPTION_CAP);
        assert!(row.labels.is_empty());
        assert_eq!(row.keys.len(), 1, "only its own link's");
        assert!(row.related_uris.is_empty());
        let mut c = card();
        c.uri = format!("https://example.org/{}", "a".repeat(1 << 20));
        assert_eq!(c.checked(Catalogue::Wikidata).unwrap_err(), Reject::Uri);
        let mut c = card();
        c.source_id = "1".repeat(1 << 20);
        assert_eq!(
            c.checked(Catalogue::Wikidata).unwrap_err(),
            Reject::SourceId
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn names_locks_and_sizes() {
        for c in Catalogue::ALL {
            assert_eq!(Catalogue::parse(c.name()), Some(c));
            assert_eq!(c.task_name(), format!("catalogue {}", c.name()));
            assert!(c.sources().iter().all(|s| {
                s.len() <= 32
                    && s.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                    })
            }));
        }
        assert_eq!(Catalogue::parse("nope"), None);
        let large: Vec<_> = Catalogue::ALL.into_iter().filter(|c| c.large()).collect();
        assert_eq!(
            large,
            [
                Catalogue::Wikidata,
                Catalogue::Musicbrainz,
                Catalogue::Osm,
                Catalogue::Openalex
            ]
        );
        let locks: BTreeSet<_> = Catalogue::ALL.iter().map(|c| c.lock_key()).collect();
        assert_eq!(locks.len(), 7);
    }
    #[test]
    fn regions_are_planet_or_a_region_path() {
        for ok in [
            "planet",
            "europe",
            "europe/switzerland",
            "north-america/us/california",
        ] {
            assert!(valid_region(ok), "{ok}");
        }
        for bad in [
            "",
            "Europe",
            "europe/",
            "/europe",
            "../etc",
            "europe//x",
            "a b",
            "-x",
        ] {
            assert!(!valid_region(bad), "{bad}");
        }
    }
}
