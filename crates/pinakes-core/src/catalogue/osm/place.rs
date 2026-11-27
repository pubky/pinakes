//! Which OpenStreetMap objects become cards, and the card of each: the filter and the fields,
//! from the object's tags and geometry alone.
//!
//! Kept: named objects with a category tag (`amenity`, `shop`, `tourism`, `leisure`,
//! `historic`, `natural`, `office`, `craft`, `place`, `public_transport`, `healthcare`, `sport`,
//! or a railway station, halt or tram stop), without transport stop positions, signposts,
//! farms, localities, boundary stones and parking; and the useful points
//! ([`USEFUL`]: parking, ATMs, toilets, charging stations and others), named or not, unless
//! private. An unnamed useful point is named by its kind and its brand or operator
//! (`ATM · UBS`), else by its kind (`Parking`).
use crate::catalogue::Card;
use crate::model::Point;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The category keys, in the order a category is read from.
pub const CATEGORY_KEYS: &[&str] = &[
    "amenity",
    "shop",
    "tourism",
    "leisure",
    "historic",
    "natural",
    "office",
    "craft",
    "place",
    "public_transport",
    "healthcare",
    "sport",
];
/// `railway` values that are places people go to.
const RAILWAY: &[&str] = &["station", "halt", "tram_stop"];
/// Categories left out even when named.
const LEFT_OUT: &[(&str, &str)] = &[
    ("public_transport", "stop_position"),
    ("place", "farm"),
    ("place", "locality"),
    ("historic", "boundary_stone"),
    ("amenity", "parking_entrance"),
];
/// `information` values of signposts (`tourism=information`).
const SIGNPOSTS: &[&str] = &["guidepost", "route_marker"];
/// The useful points: kept named or not (unless private), with the name an unnamed one gets.
pub const USEFUL: &[(&str, &str)] = &[
    ("parking", "Parking"),
    ("atm", "ATM"),
    ("toilets", "Toilets"),
    ("charging_station", "Charging station"),
    ("fuel", "Fuel"),
    ("drinking_water", "Drinking water"),
    ("pharmacy", "Pharmacy"),
    ("bank", "Bank"),
    ("bicycle_rental", "Bike rental"),
    ("car_sharing", "Car sharing"),
    ("recycling", "Recycling"),
    ("post_box", "Post box"),
];
/// Tags whose values are also names to search by.
const OTHER_NAME_KEYS: &[&str] = &[
    "alt_name",
    "official_name",
    "short_name",
    "old_name",
    "int_name",
    "loc_name",
    "nat_name",
    "reg_name",
];
/// Tags whose values become labels, besides the category's own value.
const LABEL_KEYS: &[&str] = &["cuisine", "sport", "religion", "denomination"];
/// The longest tag value a card keeps.
pub const VALUE_CAP: usize = 255;

/// An object's tags, the first value of each key (keys and values cut to [`VALUE_CAP`]).
pub type Tags<'a> = BTreeMap<&'a str, &'a str>;
pub fn tags<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Tags<'a> {
    let mut tags = Tags::new();
    for (k, v) in pairs {
        // A value with control characters is left out: it is no text to show. (Characters that
        // hide in text are taken out as the file is read: [`clean`].)
        if k.len() <= VALUE_CAP && !v.is_empty() && !v.chars().any(|c| c.is_control() || hidden(c))
        {
            tags.entry(k).or_insert(cut(v));
        }
    }
    tags
}
/// Characters that hide in text or change the direction of the text around them: the zero-width
/// space, the direction marks, embeddings, overrides and isolates, the word joiner and the byte
/// order mark. The zero-width non-joiner and joiner (U+200C, U+200D) are ordinary spelling in
/// Persian and in Indic scripts' conjuncts, and stay.
fn hidden(c: char) -> bool {
    matches!(c, '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
}
/// `s` without the characters [`hidden`] names: a zero-width space between Khmer or Thai words,
/// a direction mark in a Hebrew or Arabic name, are taken out and the name kept.
pub fn clean(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().any(hidden) {
        s.chars().filter(|c| !hidden(*c)).collect::<String>().into()
    } else {
        s.into()
    }
}
fn cut(s: &str) -> &str {
    if s.len() <= VALUE_CAP {
        return s;
    }
    let mut end = VALUE_CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
fn name_of<'a>(tags: &Tags<'a>) -> Option<&'a str> {
    tags.get("name").map(|n| n.trim()).filter(|n| !n.is_empty())
}
fn private(tags: &Tags<'_>) -> bool {
    matches!(tags.get("access").copied(), Some("private" | "no"))
}

/// Why an object is kept, and its category (`key/value`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kept {
    /// A named place.
    Named(String),
    /// A useful point, with the name an unnamed one gets.
    Useful(String, &'static str),
}
impl Kept {
    pub fn category(&self) -> &str {
        match self {
            Kept::Named(c) | Kept::Useful(c, _) => c,
        }
    }
}

/// Whether the object is kept: its category, or none.
pub fn kept(tags: &Tags<'_>) -> Option<Kept> {
    if tags.get("public_transport") == Some(&"stop_position") {
        return None;
    }
    if let Some(amenity) = tags.get("amenity") {
        if let Some((_, kind)) = USEFUL.iter().find(|(v, _)| v == amenity) {
            return (!private(tags)).then(|| Kept::Useful(format!("amenity/{amenity}"), kind));
        }
    }
    name_of(tags)?;
    let category = CATEGORY_KEYS
        .iter()
        .find_map(|k| {
            tags.get(k)
                .filter(|v| !matches!(**v, "no" | "yes" | ""))
                .map(|v| (*k, *v))
        })
        .or_else(|| {
            tags.get("railway")
                .filter(|v| RAILWAY.contains(v))
                .map(|v| ("railway", *v))
        })?;
    if LEFT_OUT.contains(&category)
        || (category == ("tourism", "information")
            && tags
                .get("information")
                .is_some_and(|v| SIGNPOSTS.contains(v)))
    {
        return None;
    }
    // A value with a separator (`restaurant;bar`) keeps its first part.
    let value = category.1.split(';').next().unwrap_or_default().trim();
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-:".contains(&b))
    {
        return None;
    }
    Some(Kept::Named(format!("{}/{value}", category.0)))
}

/// Text from a tag value: words of the value, its first letter upper case (`fast_food` -> `Fast
/// food`).
pub fn humanize(value: &str) -> String {
    let words = value.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// An object's geometry: its point and, for a way or relation, its bounding box
/// `[min_lon, min_lat, max_lon, max_lat]`.
#[derive(Clone, Debug, Default)]
pub struct Geometry {
    pub point: Option<Point>,
    pub bbox: Option<[f64; 4]>,
}

/// The fame of a place from its kind and its population: 0–1. A place's Wikidata item's fame
/// replaces it when higher (`osm::with_wikidata_fame`).
pub fn rank_fame(category: &str, tags: &Tags<'_>) -> f32 {
    let base: f32 = match category {
        "place/country" => 0.9,
        "place/state" => 0.7,
        "place/region" | "place/province" => 0.6,
        "place/city" => 0.6,
        "place/island" => 0.35,
        "place/town" => 0.4,
        "place/village" | "place/municipality" => 0.25,
        "place/suburb" | "place/borough" => 0.2,
        "place/quarter" | "place/neighbourhood" | "place/hamlet" | "place/archipelago" => 0.1,
        "natural/peak" | "natural/volcano" | "natural/glacier" | "tourism/attraction"
        | "tourism/museum" | "tourism/zoo" | "tourism/theme_park" | "historic/castle"
        | "historic/monument" | "leisure/stadium" | "railway/station" => 0.1,
        _ if category.starts_with("place/") => 0.08,
        _ => 0.05,
    };
    let population = tags
        .get("population")
        .and_then(|p| p.replace([',', '.', ' '], "").parse::<f64>().ok())
        .filter(|p| p.is_finite() && *p > 0.0);
    let by_population = population.map_or(0.0, |p| ((1.0 + p).ln() / (1.0e8f64).ln()) as f32);
    let notable = if tags.contains_key("wikipedia") || tags.contains_key("wikidata") {
        0.15
    } else {
        0.0
    };
    base.max(by_population.min(0.9)).max(notable)
}

/// The Q-id of the object's `wikidata` tag, when it names one item.
pub fn wikidata(tags: &Tags<'_>) -> Option<String> {
    crate::lookups::places::wikidata_tag(tags.get("wikidata")?).map(str::to_owned)
}

/// The card of a kept object (`kind` `node`, `way` or `relation`).
pub fn card(kind: &str, id: i64, tags: &Tags<'_>, kept: &Kept, geometry: Geometry) -> Card {
    let category = kept.category();
    let (_, value) = category.split_once('/').unwrap_or((category, ""));
    let label = |k: &str| tags.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let name = match (name_of(tags), kept) {
        (Some(name), _) => name.to_owned(),
        (None, Kept::Useful(_, kind)) => match label("brand").or_else(|| label("operator")) {
            Some(by) => format!("{kind} · {by}"),
            None => (*kind).to_owned(),
        },
        // Named places have a name.
        (None, Kept::Named(_)) => humanize(value),
    };
    let mut other_names = vec![];
    for (k, v) in tags {
        if k.starts_with("name:") || OTHER_NAME_KEYS.contains(k) {
            other_names.extend(
                v.split(';')
                    .map(str::trim)
                    .filter(|n| !n.is_empty() && *n != name),
            );
        }
    }
    other_names.sort_by_key(|n| !tags.get("name:en").is_some_and(|en| en == n));
    let mut seen = std::collections::BTreeSet::new();
    other_names.retain(|n| seen.insert(*n));
    other_names.truncate(crate::catalogue::OTHER_NAMES_CAP);
    let mut labels = vec![value.to_owned()];
    for k in LABEL_KEYS {
        if let Some(v) = tags.get(k) {
            labels.extend(v.split(';').map(|s| s.trim().replace('_', " ")));
        }
    }
    for diet in ["vegan", "vegetarian"] {
        if matches!(
            tags.get(format!("diet:{diet}").as_str()).copied(),
            Some("yes" | "only")
        ) {
            labels.push(diet.to_owned());
        }
    }
    let city = label("addr:city");
    let description = match city {
        Some(city) => format!("{} in {city}", humanize(value)),
        None => humanize(value),
    };
    let mut address = Map::new();
    for (field, tag) in [
        ("street", "addr:street"),
        ("house_number", "addr:housenumber"),
        ("postcode", "addr:postcode"),
        ("city", "addr:city"),
        ("country_code", "addr:country"),
    ] {
        if let Some(v) = label(tag) {
            address.insert(field.into(), v.into());
        }
    }
    // The tags a place page shows, as the OSM route keeps them; a value naming a credential
    // (a website with a key in its query) is left out.
    let mut kept_tags = Map::new();
    for k in crate::resolver::osm::ATTRIBUTES {
        if let Some(v) = label(k) {
            if !crate::capability::names_credential(v) {
                kept_tags.insert((*k).into(), v.into());
            }
        }
    }
    let mut details = Map::new();
    details.insert("category".into(), category.into());
    if name_of(tags).is_none() {
        details.insert("unnamed".into(), true.into());
    }
    if !address.is_empty() {
        details.insert("address".into(), Value::Object(address));
    }
    if !kept_tags.is_empty() {
        details.insert("tags".into(), Value::Object(kept_tags));
    }
    if let Some(b) = geometry.bbox.filter(|_| kind != "node") {
        details.insert("bbox".into(), json!(b));
    }
    let related_uris = ["website", "contact:website", "url"]
        .iter()
        .filter_map(|k| label(k))
        .filter(|w| w.starts_with("https://") || w.starts_with("http://"))
        .filter(|w| !crate::capability::names_credential(w))
        .take(1)
        .map(str::to_owned)
        .collect();
    // Within the details' cap: the tags go first, then the address.
    for drop in ["tags", "address"] {
        if crate::catalogue::jsonb_text_len(&Value::Object(details.clone()))
            <= crate::catalogue::DETAILS_CAP
        {
            break;
        }
        details.remove(drop);
    }
    let fame = if matches!(kept, Kept::Useful(..)) && name_of(tags).is_none() {
        0.0
    } else {
        rank_fame(category, tags)
    };
    Card {
        source: String::new(),
        source_id: format!("{kind}/{id}"),
        uri: format!("https://www.openstreetmap.org/{kind}/{id}"),
        r#type: crate::resolver::osm::schema_type(category).to_owned(),
        name,
        other_names: other_names.into_iter().map(str::to_owned).collect(),
        description: Some(description),
        labels,
        keys: vec![format!("osm:{kind}/{id}")],
        related_uris,
        point: geometry.point,
        fame,
        details,
    }
}

/// The card of an object, when it is kept.
pub fn read(kind: &str, id: i64, tags: &Tags<'_>, geometry: Geometry) -> Option<Card> {
    let kept = kept(tags)?;
    Some(card(kind, id, tags, &kept, geometry))
}

/// Names compared the way the place rules compare them: lower case, letters and digits only,
/// accents folded (`Café Zürich` and `cafe zurich` are one name).
pub fn folded(name: &str) -> String {
    use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, RawTokenizer, TextAnalyzer};
    thread_local! {
        static ANALYZER: std::cell::RefCell<TextAnalyzer> = std::cell::RefCell::new(
            TextAnalyzer::builder(RawTokenizer::default())
                .filter(LowerCaser)
                .filter(AsciiFoldingFilter)
                .build(),
        );
    }
    let mut out = String::new();
    ANALYZER.with_borrow_mut(|analyzer| {
        analyzer.token_stream(name).process(&mut |token| {
            out.extend(token.text.chars().filter(|c| c.is_alphanumeric()));
        });
    });
    out
}

/// Whether a new name is the same place's (a spelling fix, a small rename) rather than another
/// place's: one name's words run whole within the other's ("Engel", "Restaurant Engel"), or the
/// folded names differ by at most a fifth of their letters (and two at least).
pub fn same_name(old: &str, new: &str) -> bool {
    let (a, b) = (folded(old), folded(new));
    if a.is_empty() || b.is_empty() {
        return a == b;
    }
    // One name the other's words around it ("Engel", "Restaurant Engel"): whole words only, so
    // "Ox" is not "Fox & Hounds" and "Bar" not "Barbershop Hans".
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .map(folded)
            .filter(|w| !w.is_empty())
            .collect()
    };
    let (wa, wb) = (words(old), words(new));
    let within = |short: &[String], long: &[String]| {
        !short.is_empty() && long.windows(short.len()).any(|w| w == short)
    };
    if within(&wa, &wb) || within(&wb, &wa) {
        return true;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len());
    let allowed = (longest / 5).max(2);
    edits(&a, &b, allowed) <= allowed
}
/// The edit distance of two texts, counted up to `cap + 1`.
fn edits(a: &[char], b: &[char], cap: usize) -> usize {
    if a.len().abs_diff(b.len()) > cap {
        return cap + 1;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        let mut best = row[0];
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(here + 1);
            prev = here;
            best = best.min(row[j + 1]);
        }
        if best > cap {
            return cap + 1;
        }
    }
    row[b.len()]
}

/// Metres between two points on the globe (haversine).
pub fn metres(a: &Point, b: &Point) -> f64 {
    let (la1, la2) = (a.lat.to_radians(), b.lat.to_radians());
    let dla = la2 - la1;
    let dlo = (b.lon - a.lon).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * h.sqrt().min(1.0).asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t<'a>(pairs: &'a [(&'a str, &'a str)]) -> Tags<'a> {
        tags(pairs.iter().copied())
    }
    fn at() -> Geometry {
        Geometry {
            point: Some(Point {
                lat: 47.14,
                lon: 9.52,
            }),
            bbox: None,
        }
    }

    #[test]
    fn the_filter_keeps_named_places_and_useful_points() {
        for (pairs, category) in [
            (
                &[("amenity", "cafe"), ("name", "Café")][..],
                Some("amenity/cafe"),
            ),
            (
                &[("shop", "bakery;cafe"), ("name", "B")],
                Some("shop/bakery"),
            ),
            (
                &[("railway", "station"), ("name", "Vaduz")],
                Some("railway/station"),
            ),
            (
                &[("place", "village"), ("name", "Triesen")],
                Some("place/village"),
            ),
            (&[("amenity", "atm")], Some("amenity/atm")),
            (
                &[("amenity", "parking"), ("access", "yes")],
                Some("amenity/parking"),
            ),
            (
                &[("amenity", "parking"), ("name", "P1")],
                Some("amenity/parking"),
            ),
            (
                &[("amenity", "toilets"), ("access", "customers")],
                Some("amenity/toilets"),
            ),
            // Left out.
            (&[("amenity", "parking"), ("access", "private")], None),
            (&[("amenity", "cafe")], None),
            (
                &[
                    ("public_transport", "stop_position"),
                    ("railway", "halt"),
                    ("name", "S"),
                ],
                None,
            ),
            // A stop position is left out whatever else it is tagged with.
            (
                &[
                    ("public_transport", "stop_position"),
                    ("amenity", "ticket_validator"),
                    ("name", "S"),
                ],
                None,
            ),
            (
                &[
                    ("tourism", "information"),
                    ("information", "guidepost"),
                    ("name", "G"),
                ],
                None,
            ),
            (&[("place", "farm"), ("name", "F")], None),
            (&[("place", "locality"), ("name", "L")], None),
            (&[("historic", "boundary_stone"), ("name", "S")], None),
            (&[("amenity", "bench")], None),
            (&[("amenity", "bicycle_parking")], None),
            (&[("amenity", "shelter")], None),
            (&[("amenity", "parking_entrance"), ("name", "E")], None),
            (&[("building", "yes"), ("name", "House")], None),
            (&[("highway", "residential"), ("name", "Street")], None),
            (&[("shop", "no"), ("name", "X")], None),
            (&[("shop", "<script>"), ("name", "X")], None),
        ] {
            assert_eq!(
                kept(&t(pairs)).map(|k| k.category().to_owned()).as_deref(),
                category,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn a_card_has_the_fields_a_place_page_needs() {
        let pairs = [
            ("amenity", "restaurant"),
            ("name", "Engel"),
            ("name:en", "Angel"),
            ("alt_name", "Gasthaus Engel;Engel Vaduz"),
            ("cuisine", "regional;pizza"),
            ("diet:vegetarian", "yes"),
            ("addr:street", "Städtle"),
            ("addr:housenumber", "13"),
            ("addr:postcode", "9490"),
            ("addr:city", "Vaduz"),
            ("opening_hours", "Mo-Su 10:00-23:00"),
            ("phone", "+423 236 17 17"),
            ("website", "https://www.hotel-engel.li/"),
            ("wheelchair", "yes"),
            ("wikidata", "Q123"),
        ];
        let card = read("node", 42, &t(&pairs), at()).unwrap();
        assert_eq!(card.source_id, "node/42");
        assert_eq!(card.uri, "https://www.openstreetmap.org/node/42");
        assert_eq!(card.r#type, "Restaurant");
        assert_eq!(card.name, "Engel");
        assert_eq!(card.other_names, ["Angel", "Gasthaus Engel", "Engel Vaduz"]);
        assert_eq!(card.description.as_deref(), Some("Restaurant in Vaduz"));
        assert_eq!(
            card.labels,
            ["restaurant", "regional", "pizza", "vegetarian"]
        );
        assert_eq!(card.keys, ["osm:node/42"]);
        assert_eq!(card.related_uris, ["https://www.hotel-engel.li/"]);
        assert_eq!(card.details["category"], "amenity/restaurant");
        assert_eq!(card.details["address"]["house_number"], "13");
        assert_eq!(card.details["tags"]["opening_hours"], "Mo-Su 10:00-23:00");
        assert_eq!(card.details["tags"]["wikidata"], "Q123");
        assert!(card.details.get("bbox").is_none());
        assert!((card.fame - 0.15).abs() < 1e-6, "{}", card.fame);
        let row = card.checked(crate::catalogue::Catalogue::Osm).unwrap();
        assert_eq!(
            row.labels,
            ["restaurant", "regional", "pizza", "vegetarian"]
        );
    }

    #[test]
    fn joiners_stay_and_other_names_are_each_once() {
        // Persian spells words with the zero-width non-joiner.
        let name = "کتاب\u{200c}فروشی";
        let card = read(
            "node",
            3,
            &t(&[
                ("shop", "books"),
                ("name", name),
                ("name:fa", name),
                ("name:de", "Buchladen"),
                ("name:fr", "Buchladen"),
            ]),
            at(),
        )
        .unwrap();
        assert_eq!(card.name, name);
        assert_eq!(card.other_names, ["Buchladen"]);
        // Characters that hide in text are taken out as a file is read, the name kept: a
        // zero-width space between Khmer words, a direction mark after a Hebrew name.
        assert_eq!(clean("ផ្សារ\u{200b}ធំ"), "ផ្សារធំ");
        assert_eq!(clean("קפה\u{200f}"), "קפה");
        assert_eq!(clean("a\u{202e}b\u{2066}c\u{feff}"), "abc");
        assert!(matches!(clean(name), std::borrow::Cow::Borrowed(_)));
        let khmer = clean("ផ្សារ\u{200b}ធំ").into_owned();
        let card = read(
            "node",
            5,
            &t(&[("shop", "books"), ("name", khmer.as_str())]),
            at(),
        )
        .unwrap();
        assert_eq!(card.name, "ផ្សារធំ");
        // A value that still holds one (not read from a file) is left out with its tag.
        assert!(read(
            "node",
            4,
            &t(&[("shop", "books"), ("name", "a\u{202e}b")]),
            at()
        )
        .is_none());
    }

    #[test]
    fn unnamed_points_are_named_by_kind_and_brand() {
        let card = read(
            "node",
            1,
            &t(&[("amenity", "atm"), ("operator", "UBS")]),
            at(),
        )
        .unwrap();
        assert_eq!(card.name, "ATM · UBS");
        assert_eq!(card.fame, 0.0);
        let card = read(
            "node",
            1,
            &t(&[
                ("amenity", "atm"),
                ("brand", "Raiffeisen"),
                ("operator", "X"),
            ]),
            at(),
        )
        .unwrap();
        assert_eq!(card.name, "ATM · Raiffeisen");
        let card = read(
            "way",
            7,
            &t(&[("amenity", "parking")]),
            Geometry {
                bbox: Some([9.5, 47.1, 9.6, 47.2]),
                ..at()
            },
        )
        .unwrap();
        assert_eq!(card.name, "Parking");
        assert_eq!(card.details["bbox"], json!([9.5, 47.1, 9.6, 47.2]));
        assert_eq!(card.r#type, "Place");
        // A credential in a website is never kept.
        let card = read(
            "node",
            2,
            &t(&[
                ("amenity", "fuel"),
                ("website", "https://x.example/?token=SECRET"),
            ]),
            at(),
        )
        .unwrap();
        assert!(!serde_json::to_string(&card.details)
            .unwrap()
            .contains("SECRET"));
        assert!(
            card.related_uris.iter().all(|u| !u.contains("SECRET")),
            "{:?}",
            card.related_uris
        );
    }

    #[test]
    fn fame_follows_kind_population_and_notability() {
        let city = t(&[
            ("place", "city"),
            ("name", "Zürich"),
            ("population", "421878"),
        ]);
        let village = t(&[
            ("place", "village"),
            ("name", "Planken"),
            ("population", "480"),
        ]);
        let cafe = t(&[("amenity", "cafe"), ("name", "C")]);
        let f = |tags: &Tags<'_>| rank_fame(kept(tags).unwrap().category(), tags);
        assert!(f(&city) > f(&village) && f(&village) > f(&cafe));
        assert!(f(&city) <= 0.9);
    }

    #[test]
    fn renames_and_spelling_fixes() {
        for (old, new, same) in [
            ("Café Zürich", "Cafe Zurich", true),
            ("Restaurant Engel", "Engel", true),
            ("Bäckerei Müller", "Bäckerei Mueller", true),
            ("Sushi Bar Hana", "Sushibar Hana", true),
            ("Café Zürich", "Burger King", false),
            ("Engel", "Adler", false),
            ("Rössli", "Kebab Haus", false),
            // Within another name only as whole words.
            ("Ox", "Fox & Hounds", false),
            ("Bar", "Barbershop Hans", false),
            ("Engel", "Gasthof Engel Vaduz", true),
        ] {
            assert_eq!(same_name(old, new), same, "{old} -> {new}");
        }
    }

    #[test]
    fn distances_on_the_globe() {
        let a = Point {
            lat: 47.0,
            lon: 9.0,
        };
        let b = Point {
            lat: 47.0,
            lon: 9.000_66,
        };
        assert!((metres(&a, &b) - 50.0).abs() < 1.0, "{}", metres(&a, &b));
    }

    proptest::proptest! {
        /// Any tags: a kept object's card is one the table takes, keyed and linked by its object
        /// alone (the canonical link reads back as the object, unchanged by normalisation), with
        /// labels that are valid tags.
        #[test]
        fn cards_keys_links_and_labels_hold(
            kind in proptest::sample::select(vec!["node", "way", "relation"]),
            id in 1i64..=i64::MAX,
            pairs in proptest::collection::vec(("(amenity|shop|name|place|tourism|brand|operator|cuisine|website|name:[a-z]{2}|alt_name|access|[a-z:_]{1,12})", "\\PC{0,40}"), 0..12),
        ) {
            let tags = tags(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            if let Some(card) = read(kind, id, &tags, at()) {
                let row = card.checked(crate::catalogue::Catalogue::Osm).expect("a valid card");
                proptest::prop_assert_eq!(&row.uri, &format!("https://www.openstreetmap.org/{kind}/{id}"));
                let object = crate::resolver::osm::parse_id(&row.uri).expect("an OSM link");
                proptest::prop_assert_eq!(object.key(), format!("{kind}/{id}"));
                let key = format!("osm:{kind}/{id}");
                proptest::prop_assert!(row.keys.contains(&key));
                for label in &row.labels {
                    let again = crate::catalogue::label(label);
                    proptest::prop_assert_eq!(again.as_deref(), Some(label.as_str()));
                }
                proptest::prop_assert!(crate::catalogue::jsonb_text_len(&Value::Object(row.details.clone())) <= crate::catalogue::DETAILS_CAP);
            }
        }

        /// Folding and the rename rule: a name is the same as itself however it is spelt in case
        /// and accents, and the rule is symmetric.
        #[test]
        fn names_fold_and_renames_are_symmetric(a in "\\PC{0,30}", b in "\\PC{0,30}") {
            proptest::prop_assert!(same_name(&a, &a.to_uppercase()) || folded(&a) != folded(&a.to_uppercase()));
            proptest::prop_assert_eq!(same_name(&a, &b), same_name(&b, &a));
        }
    }

    /// Random tags never panic the filter or the card, and every card a kept object makes is a
    /// card the table takes.
    #[test]
    fn random_tags_make_valid_cards() {
        let keys = [
            "amenity",
            "name",
            "shop",
            "brand",
            "operator",
            "access",
            "website",
            "name:de",
            "alt_name",
            "cuisine",
            "population",
            "addr:city",
            "wikidata",
            "place",
            "opening_hours",
        ];
        let values = [
            "atm",
            "cafe",
            "",
            ";",
            "x;y",
            "Zürich",
            "https://a.example/",
            "private",
            "village",
            "1e400",
            "Q1",
            "日本",
            "a\u{0}b",
            "🙂",
            &"v".repeat(400),
        ];
        let mut seed = 7u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        for _ in 0..20_000 {
            let n = next() % 8;
            let pairs: Vec<(&str, &str)> = (0..n)
                .map(|_| (keys[next() % keys.len()], values[next() % values.len()]))
                .collect();
            let tags = tags(pairs.iter().copied());
            if let Some(card) = read("way", 1 + (next() as i64 % 1000), &tags, at()) {
                assert!(
                    card.checked(crate::catalogue::Catalogue::Osm).is_ok(),
                    "{pairs:?}"
                );
            }
        }
    }
}
