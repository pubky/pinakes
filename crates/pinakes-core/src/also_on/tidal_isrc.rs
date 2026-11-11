//! Another service's track or album on TIDAL, by its ISRC or barcode: for a track with an `isrc:`
//! key, the TIDAL tracks that carry exactly that ISRC (`GET /v2/tracks?filter[isrc]=`); for an
//! album with a `upc:` key, the one TIDAL album that carries exactly that barcode
//! (`GET /v2/albums?filter[barcodeId]=`). Each answer is read back: only an item whose own ISRC or
//! barcode is the value asked counts. The links are keyed by the ISRC or barcode, as TIDAL's own
//! record of its own item is an exact source of it. Several tracks are the recording on several
//! releases (`editions` places them, a card shows one per service); several albums for one
//! barcode name no one album, and give nothing.
//!
//! Off by default: TIDAL's Developer Guidelines forbid integrating TIDAL content with another
//! service's without TIDAL's written approval (docs/sources). Asked with the same credentials,
//! country and token slot as `tidal` (`[tidal]`), one request per value, each answer shared for an
//! hour by credential pair. TIDAL's own links are not asked. It runs after every source that keys
//! an ISRC or a barcode (a service's own record, the hubs, `discogs`, `deezer`) and before
//! `editions`, which places its links.
use super::{
    tidal::{self, TidalConfig, API},
    Context, Found, Source, Subject,
};
use crate::{
    music::{self, Level, Service},
    music_sources::{derived, join_level, own_isrc},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// The most TIDAL tracks one ISRC gives a card (the recording on several releases).
const MAX_LINKS: usize = 5;
/// The most ISRC or barcode keys of one track or album asked (a track's own ISRC first).
const MAX_VALUES: usize = 2;

/// A track by its ISRC or an album by its barcode, as the API names its collection, its filter,
/// the attribute read back and the key's namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Album,
    Track,
}
impl Kind {
    fn of(link: &music::Link) -> Option<Self> {
        if link.service == Service::Tidal {
            return None;
        }
        match join_level(link)? {
            Level::Album => Some(Self::Album),
            Level::Track => Some(Self::Track),
            _ => None,
        }
    }
    fn level(self) -> Level {
        match self {
            Self::Album => Level::Album,
            Self::Track => Level::Track,
        }
    }
    fn collection(self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Track => "tracks",
        }
    }
    /// The filter's name, `[` and `]` escaped as the URL carries them.
    fn filter(self) -> &'static str {
        match self {
            Self::Album => "filter%5BbarcodeId%5D",
            Self::Track => "filter%5Bisrc%5D",
        }
    }
    fn attribute(self) -> &'static str {
        match self {
            Self::Album => "barcodeId",
            Self::Track => "isrc",
        }
    }
    fn namespace(self) -> &'static str {
        match self {
            Self::Album => "upc",
            Self::Track => "isrc",
        }
    }
    fn normal(self, value: &str) -> Option<String> {
        match self {
            Self::Album => crate::barcode::gtin13(value),
            Self::Track => music::isrc(value),
        }
    }
    /// The URL of an item's page: the canonical form `music::parse` gives it.
    fn page(self, id: &str) -> String {
        match self {
            Self::Album => format!("https://tidal.com/album/{id}"),
            Self::Track => format!("https://tidal.com/track/{id}"),
        }
    }
}

pub struct TidalIsrc;
#[async_trait]
impl Source for TidalIsrc {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        // The host's pace is every TIDAL source's: one budget, shared.
        &[("openapi.tidal.com", 2.0)]
    }
    fn id(&self) -> &'static str {
        "tidal_isrc"
    }
    fn section(&self) -> Option<super::Section> {
        Some(super::Section::of::<TidalConfig>())
    }
    fn enabled_by_default(&self) -> bool {
        false
    }
    fn ready(&self, config: &crate::config::Config) -> bool {
        config.section::<TidalConfig>().pair().is_some()
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri)
            .and_then(|l| Kind::of(&l))
            .is_some_and(|kind| subject.keys_in(kind.namespace()).next().is_some())
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(kind) = music::parse(&subject.uri).and_then(|l| Kind::of(&l)) else {
            return Ok(Found::default());
        };
        let namespace = kind.namespace();
        // A track's own ISRC first, then the hubs'; only values that are keys (a shared ISRC is
        // a fact, and TIDAL would answer with any of its recordings).
        let own = own_isrc(subject)
            .filter(|o| kind == Kind::Track && subject.keys_in(namespace).any(|k| k == o.as_str()));
        let mut values: Vec<&str> = own.as_deref().into_iter().collect();
        values.extend(
            subject
                .keys_in(namespace)
                .filter(|v| Some(*v) != own.as_deref()),
        );
        let country = &cx.config.section::<TidalConfig>().country;
        let mut found = Found::default();
        for value in values.into_iter().take(MAX_VALUES) {
            let Some(value) = kind.normal(value) else {
                continue;
            };
            let url = format!(
                "{API}/{}?countryCode={country}&{}={value}",
                kind.collection(),
                kind.filter()
            );
            let Some(answer) = tidal::api_object(cx, &url).await? else {
                continue;
            };
            let ids = matching(&answer, kind, &value);
            // One barcode on several albums names none of them, nor does an answer with a further
            // page, which may hold another.
            if kind == Kind::Album && (ids.len() != 1 || paged(&answer)) {
                continue;
            }
            for id in ids {
                let Some(link) = derived(&kind.page(&id), kind.level(), "tidal_isrc") else {
                    continue;
                };
                if found.links.iter().any(|l| l.uri == link.uri) {
                    continue;
                }
                found.links.push(link.keyed(format!("{namespace}:{value}")));
                if found.links.len() >= MAX_LINKS {
                    return Ok(found);
                }
            }
        }
        Ok(found)
    }
}

/// Whether an answer has a further page (`links.next`).
fn paged(answer: &Value) -> bool {
    answer["links"]["next"].as_str().is_some()
}

/// The IDs of the items of an answer that carry exactly `value` (normalised), in TIDAL's order
/// with those TIDAL offers for streaming first; each once.
fn matching(answer: &Value, kind: Kind, value: &str) -> Vec<String> {
    let mut items: Vec<(bool, String)> = answer["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["type"].as_str() == Some(kind.collection()))
        .filter(|i| {
            i["attributes"][kind.attribute()]
                .as_str()
                .and_then(|v| kind.normal(v))
                .as_deref()
                == Some(value)
        })
        .filter_map(|i| {
            let id = i["id"]
                .as_str()
                .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))?;
            let streams = i["attributes"]["availability"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "STREAM"));
            Some((!streams, id.to_owned()))
        })
        .collect();
    // A stable sort: TIDAL's order within each.
    items.sort_by_key(|(later, _)| *later);
    let mut ids: Vec<String> = Vec::new();
    for (_, id) in items {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/media/tidal_isrc/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn only_other_services_tracks_and_albums_apply() {
        let kind = |uri: &str| music::parse(uri).and_then(|l| Kind::of(&l));
        assert_eq!(
            kind("https://www.deezer.com/track/1171872"),
            Some(Kind::Track)
        );
        assert_eq!(
            kind("https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa"),
            Some(Kind::Album)
        );
        // A video counts as its recording.
        assert_eq!(
            kind("https://www.youtube.com/watch?v=bSnlKl_PoQU"),
            Some(Kind::Track)
        );
        assert_eq!(kind("https://tidal.com/track/3524506"), None);
        assert_eq!(kind("https://tidal.com/album/3524504"), None);
        assert_eq!(kind("https://www.deezer.com/artist/27"), None);
    }

    /// Walking Disaster's ISRC: eight TIDAL tracks carry it; those TIDAL streams come first.
    #[test]
    fn an_isrc_reads_back_and_streamed_tracks_come_first() {
        let v = fixture("tracks-isrc-USUM70740306.json");
        assert_eq!(
            matching(&v, Kind::Track, "USUM70740306"),
            [
                "31067660",
                "3524506",
                "199571302",
                "199286892",
                "56947135",
                "91915057",
                "34769779",
                "34768810"
            ]
        );
        // An answer for another ISRC counts for nothing.
        assert!(matching(&v, Kind::Track, "GBUM71029604").is_empty());
        let mut other = v.clone();
        for item in other["data"].as_array_mut().unwrap() {
            item["attributes"]["isrc"] = "GBUM71029604".into();
        }
        assert!(matching(&other, Kind::Track, "USUM70740306").is_empty());
        assert!(matching(
            &fixture("tracks-isrc-missing.json"),
            Kind::Track,
            "QZZZZ2600001"
        )
        .is_empty());
        // Several ISRCs at once: one track each.
        let batch = fixture("tracks-isrc-batch.json");
        assert_eq!(matching(&batch, Kind::Track, "USUM70740306"), ["3524506"]);
        assert_eq!(matching(&batch, Kind::Track, "PLL431541907"), ["77646168"]);
    }

    /// TIDAL keeps a barcode as GTIN-14 (`00602517416987`): read back as GTIN-13. A code
    /// several albums carry gives several IDs, which the lookup refuses.
    #[test]
    fn a_barcode_reads_back_in_any_spelling() {
        let v = fixture("albums-barcode-0602517416987.json");
        assert_eq!(matching(&v, Kind::Album, "0602517416987"), ["3524504"]);
        assert!(matching(&v, Kind::Album, "0886443927087").is_empty());
        let v = fixture("albums-barcode-0886443927087.json");
        assert_eq!(matching(&v, Kind::Album, "0886443927087"), ["20115556"]);
        let several = fixture("albums-barcode-0000000000000.json");
        assert!(matching(&several, Kind::Album, "0000000000000").len() > 1);
        // One match on a first page with a next one: the lookup refuses it (`paged`).
        let one = fixture("albums-barcode-0602517416987.json");
        assert!(!paged(&one));
        let mut first = one.clone();
        first["links"]["next"] =
            "/albums?filter%5BbarcodeId%5D=0602517416987&countryCode=US&page%5Bcursor%5D=x".into();
        assert!(paged(&first));
        assert_eq!(matching(&first, Kind::Album, "0602517416987"), ["3524504"]);
    }
}
