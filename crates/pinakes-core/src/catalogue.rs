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

pub mod run;
pub mod store;

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
/// OpenAlex: works cited at least `min_citations` times.
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

/// A source's label (a genre, an OSM category, a topic) as a valid Pubky tag label, so one tap on
/// a suggested tag writes a tag every client accepts: lower case, each run of whitespace, commas,
/// colons and underscores one hyphen, no control characters, no hyphens at either end. `None`
/// when nothing is left or the label is longer than [`LABEL_MAX_CHARS`]: a cut label would say
/// something else.
pub fn label(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut gap = false;
    // The whole text at once, as a client lower-cases a tag (a final Σ becomes ς).
    for c in raw.to_lowercase().chars() {
        if c.is_whitespace() || LABEL_INVALID.contains(&c) || c == '_' || c == '-' {
            gap = true;
        } else if c.is_control() {
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
/// of the normalized link (`identity::resource_id`), the form a resource's grouping keys use. An
/// http(s) link only.
pub fn link_key(uri: &str) -> Option<String> {
    let (id, _, scheme) = model::identity(uri).ok()?;
    matches!(scheme.as_str(), "http" | "https").then(|| format!("url:{id}"))
}
/// A join key in the stored form: `kind:value`, the kind lower case (`wikidata`, `mb`, `isrc`,
/// `osm`, `doi`, `url`), the value non-empty, no whitespace or control character, at most
/// [`KEY_CAP`] bytes.
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
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A key as it is stored: [`valid_key`], with a DOI lower case and an ISBN as its ISBN-13, the
/// forms a resource's keys take, and a `url:` key a resource ID; `None` for one that is not valid.
pub fn stored_key(key: &str) -> Option<String> {
    let key = key.trim();
    if !valid_key(key) {
        return None;
    }
    let (kind, value) = key.split_once(':')?;
    match kind {
        "doi" => crate::resolver::cards::doi(key).map(|doi| format!("doi:{doi}")),
        "isbn" => crate::resolver::cards::isbn(key)
            .map(|isbn| format!("isbn:{}", crate::resolver::cards::isbn13(&isbn))),
        "url" => model::valid_id(value).then(|| key.to_owned()),
        _ => Some(key.to_owned()),
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
            if written.contains(['e', 'E']) {
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
fn clean(text: &str, cap: usize) -> String {
    let mut text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    model::truncate(&mut text, cap);
    text
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
            || self.source_id.chars().any(char::is_control)
        {
            return Err(Reject::SourceId);
        }
        let (_, uri, _) = model::identity(&self.uri).map_err(|_| Reject::Uri)?;
        // Normalizing may lengthen a link (percent-encoding); the table holds 2048 bytes.
        if uri.len() > 2048 {
            return Err(Reject::Uri);
        }
        // Stored by its bare name (`Person`, never `schema:Person`), as resources store theirs.
        let Some(r#type) = model::type_path(&self.r#type).pop() else {
            return Err(Reject::Type);
        };
        let name = clean(&self.name, NAME_CAP);
        if name.is_empty() {
            return Err(Reject::Name);
        }
        let mut seen = BTreeSet::from([name.clone()]);
        let other_names = self
            .other_names
            .iter()
            .map(|n| clean(n, NAME_CAP))
            .filter(|n| !n.is_empty() && seen.insert(n.clone()))
            .take(OTHER_NAMES_CAP)
            .collect();
        let description = self
            .description
            .as_deref()
            .map(|d| clean(d, model::DESCRIPTION_CAP))
            .filter(|d| !d.is_empty());
        let mut seen = BTreeSet::new();
        let keys = self
            .keys
            .iter()
            .filter_map(|k| stored_key(k))
            .filter(|k| seen.insert(k.clone()))
            .take(KEYS_CAP)
            .collect();
        let mut seen = BTreeSet::from([uri.clone()]);
        let related_uris = self
            .related_uris
            .iter()
            .filter_map(|u| model::identity(u).ok().map(|(_, u, _)| u))
            .filter(|u| u.len() <= 2048 && seen.insert(u.clone()))
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
        let details = self.details.clone();
        let value = serde_json::Value::Object(details.clone());
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

#[cfg(test)]
mod tests {
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
        assert_eq!(row.keys, ["wikidata:Q42".to_owned(), wikidata_page]);
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
