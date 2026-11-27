//! Apple Maps links: a place's name and point read from the link itself. Nothing is fetched:
//! Apple's pages and APIs are never asked.
use super::{Context, Example, Source};
use crate::{
    model::ResourceMeta,
    resolver::geo::{self, LocationLink},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct AppleMaps;

const SITE: &str = "Apple Maps";

/// What an Apple Maps link says of its place: a name, a point, or both.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub name: Option<String>,
    pub point: Option<LocationLink>,
}

/// The place an Apple Maps link names, on `maps.apple.com` at `/` or `/place`:
/// - the name: `name`, else `q`, else `address` (the first that names something);
/// - the point: `coordinate`, else `ll` (`lat,lon`), else a `q` that is coordinates.
///
/// A name that is only coordinates is the point's. `auid` and `place-id` are not read (Apple's
/// own IDs name nothing without Apple's API), nor `sll` and `near` (where a search looks).
/// `None` for directions (`saddr`, `daddr`, `dirflg`) and any link that names neither a name nor
/// a point.
pub fn place(uri: &str) -> Option<Place> {
    let url = super::kit::web_url(uri)?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host != "maps.apple.com" || !matches!(url.path(), "" | "/" | "/place" | "/place/") {
        return None;
    }
    if url
        .query_pairs()
        .any(|(k, _)| matches!(k.as_ref(), "saddr" | "daddr" | "dirflg"))
    {
        return None;
    }
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let texts: Vec<(Option<String>, Option<LocationLink>)> = ["name", "q", "address"]
        .iter()
        .filter_map(|k| query(k))
        .map(|t| geo::text_place(&t))
        .collect();
    let name = texts.iter().find_map(|(name, _)| name.clone());
    let point = ["coordinate", "ll"]
        .iter()
        .filter_map(|k| query(k))
        .find_map(|v| {
            let (lat, lon) = geo::coordinates(&v)?;
            LocationLink::from_text(lat, lon, None)
        })
        .or_else(|| texts.into_iter().find_map(|(_, point)| point));
    (name.is_some() || point.is_some()).then_some(Place { name, point })
}

#[async_trait]
impl Source for AppleMaps {
    fn name(&self) -> &'static str {
        "apple_maps"
    }
    fn reads(&self, uri: &str) -> bool {
        place(uri).is_some()
    }
    fn facts(&self) -> &'static [&'static str] {
        geo::ATTRIBUTES
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://maps.apple.com/place?address=Champ%20de%20Mars,%205%20Avenue%20Anatole%20France,%2075007%20Paris,%20France&coordinate=48.858260,2.294500&name=Eiffel%20Tower",
                card_type: "Place",
            },
            Example {
                uri: "https://maps.apple.com/?q=Eiffel+Tower&ll=48.858260,2.294500",
                card_type: "Place",
            },
            Example {
                uri: "https://maps.apple.com/?ll=50.894967,4.341626",
                card_type: "Place",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let place = place(uri).context("not an Apple Maps link")?;
        Ok(geo::place_card(
            cx.card(uri, self),
            SITE,
            place.name,
            place.point.as_ref(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LocationSource;

    /// Real link forms (`tests/fixtures/sources/apple_maps/links.json`: Apple's map links
    /// reference and the app's share links), with the name and point each card takes from the
    /// link alone.
    #[test]
    fn place_links_name_their_place_and_point() {
        let links: serde_json::Value =
            serde_json::from_slice(&crate::sources::fixture::read("apple_maps", "links.json"))
                .unwrap();
        let links = links.as_array().unwrap();
        assert!(links.len() >= 8);
        for link in links {
            let uri = link["uri"].as_str().unwrap();
            let point = link["point"]
                .as_array()
                .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()));
            let place = place(uri).unwrap_or_else(|| panic!("{uri}"));
            let doc = geo::place_card(
                ResourceMeta::new(uri, "apple_maps", 60),
                SITE,
                place.name,
                place.point.as_ref(),
            );
            assert_eq!(doc.type_name, "Place");
            assert_eq!(doc.site, "Apple Maps");
            assert_eq!(doc.name.as_deref(), link["name"].as_str(), "{uri}");
            assert_eq!(
                doc.location.as_ref().map(|p| (p.lat, p.lon)),
                point,
                "{uri}"
            );
            assert_eq!(
                doc.attributes.get("precision").map(String::as_str),
                link["precision"].as_str(),
                "{uri}"
            );
            assert_eq!(
                matches!(doc.location_source, Some(LocationSource::Uri)),
                point.is_some()
            );
            assert_eq!(crate::resolver::classify(uri), Some("official"), "{uri}");
            doc.bounded(crate::sources::attributes()).unwrap();
        }
    }

    /// Links that name no place or point stay on their route: an Apple ID alone, directions,
    /// searches, other hosts and paths.
    #[test]
    fn other_links_are_not_read() {
        for uri in [
            "https://maps.apple.com/place?auid=1218585579701879383",
            "https://maps.apple.com/place?place-id=IA0BF9F2CF3F76A1C",
            "https://maps.apple.com/?saddr=Cupertino&daddr=San+Francisco",
            "https://maps.apple.com/?daddr=San+Francisco&ll=37.77,-122.42",
            "https://maps.apple.com/directions?destination=San+Francisco",
            "https://maps.apple.com/?ll=91,0",
            "https://maps.apple.com/?q=%20",
            "https://maps.apple.com/",
            "https://maps.apple.com/search?query=Eiffel+Tower",
            "https://maps.apple.com.evil.example/?q=Eiffel+Tower",
            "https://maps.apple.com:8443/?q=Eiffel+Tower",
            "https://user@maps.apple.com/?q=Eiffel+Tower",
            "https://www.apple.com/maps/?q=Eiffel+Tower",
        ] {
            assert!(!AppleMaps.reads(uri), "{uri}");
        }
    }

    /// Property: on 20,000 random and malformed spellings of real Apple Maps links, the
    /// reader never panics, and a link read gives a bounded name without control characters
    /// and a point in range.
    #[test]
    fn random_links_never_panic_and_read_bounded_places() {
        const BASES: &[&str] = &[
            "https://maps.apple.com/place?address=Champ%20de%20Mars,%205%20Avenue%20Anatole%20France,%2075007%20Paris,%20France&coordinate=48.858260,2.294500&name=Eiffel%20Tower",
            "https://maps.apple.com/?address=Champ%20de%20Mars&auid=10934669624155823143&ll=48.858260,2.294500&lsp=9902&q=Eiffel%20Tower",
            "https://maps.apple.com/?q=Eiffel+Tower&ll=48.858260,2.294500",
            "https://maps.apple.com/?ll=50.894967,4.341626",
            "https://maps.apple.com/?address=1,Infinite+Loop,Cupertino,California",
        ];
        const PARTS: &[&str] = &[
            "https://maps.apple.com/",
            "https://maps.apple.com/place",
            "?",
            "&",
            "q=",
            "name=",
            "address=",
            "ll=",
            "coordinate=",
            "auid=",
            "daddr=",
            "48.85826",
            "-122.4",
            "91",
            "1e5",
            ",",
            "+",
            "%",
            "%2",
            "%20",
            "%C3%A9",
            "%FF",
            "%00",
            "%E2%80%AE",
            "%0A",
            "(",
            ")",
            "Eiffel",
            "é",
            "東京",
            "😀",
            "\u{301}",
            " ",
            "#",
            "/",
        ];
        let mut gen = geo::fuzz::Gen(0x2545_f491_4f6c_dd1d);
        let (mut read, mut unread) = (0, 0);
        for _ in 0..20_000 {
            let base = gen.pick(BASES);
            let uri = gen.mutate(base, PARTS);
            let Some(place) = place(&uri) else {
                unread += 1;
                continue;
            };
            read += 1;
            if let Some(name) = &place.name {
                assert!(
                    !name.is_empty()
                        && name.len() <= geo::NAME_BYTES
                        && name.trim() == name
                        && !name.chars().any(char::is_control),
                    "{uri:?} -> {name:?}"
                );
            }
            if let Some(p) = &place.point {
                assert!(
                    (-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon),
                    "{uri:?}"
                );
            }
            assert!(place.name.is_some() || place.point.is_some());
        }
        assert!(read > 3_000 && unread > 1_000, "{read} {unread}");
    }

    fn in_range(p: &LocationLink) -> bool {
        (-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon)
    }

    proptest::proptest! {
        /// Any query on `maps.apple.com`: no panic, and any point in range.
        #[test]
        fn any_link_reads_without_panic(
            path in proptest::sample::select(vec!["/", "/place", "/directions", ""]),
            query in "(\\?((q|ll|name|address|coordinate|auid|saddr|sll)=[@,.+%0-9A-Za-z()_-]{0,20}&?){0,4})?",
        ) {
            let uri = format!("https://maps.apple.com{path}{query}");
            if let Some(found) = place(&uri) {
                proptest::prop_assert!(found.name.is_some() || found.point.is_some());
                if let Some(p) = &found.point {
                    let shown = format!("{uri} {p:?}");
                    proptest::prop_assert!(in_range(p), "{}", shown);
                }
            }
        }
        /// A name and a point: both, whatever the parameters' order.
        #[test]
        fn a_named_point_gives_both(
            words in proptest::collection::vec("[A-Za-z]{1,10}", 1..4),
            lat in -90.0f64..=90.0,
            lon in -180.0f64..=180.0,
            name_first in proptest::bool::ANY,
        ) {
            let (name, ll) = (format!("q={}", words.join("+")), format!("ll={lat:.6},{lon:.6}"));
            let uri = if name_first { format!("https://maps.apple.com/?{name}&{ll}") } else { format!("https://maps.apple.com/?{ll}&{name}") };
            let found = place(&uri).expect("a place");
            let joined = words.join(" ");
            proptest::prop_assert_eq!(found.name.as_deref(), Some(joined.as_str()));
            let p = found.point.expect("a point");
            proptest::prop_assert!((p.lat - lat).abs() < 1e-6 && (p.lon - lon).abs() < 1e-6);
        }
    }
}
