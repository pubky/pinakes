//! Wikidata items: identifier properties for places, apps and games, and Wikipedia sitelinks.
//!
//! Only best-rank statements count (preferred if any, else normal; never deprecated). A
//! property with more than one best-rank value after validation is ambiguous and adds nothing.
use super::{body, encode_path, plain, web_url, Key, Link, Request};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// The most items one `wbgetentities` request may name.
pub const MAX_IDS: usize = 50;
const VIA: &str = "wikidata";

/// A Wikidata item id, `Q` and a number without leading zeros.
pub fn qid(value: &str) -> Option<&str> {
    let digits = value.strip_prefix('Q')?;
    (plain(digits, 12, |c| c.is_ascii_digit()) && !digits.starts_with('0')).then_some(value)
}
pub fn key(qid: &str) -> Key {
    Key::new("wikidata", qid)
}

/// Claims and sitelink URLs for up to `MAX_IDS` items.
pub fn entities_request(ids: &[&str]) -> Option<Request> {
    if ids.is_empty() || ids.len() > MAX_IDS || ids.iter().any(|id| qid(id).is_none()) {
        return None;
    }
    Request::get(&format!(
        "https://www.wikidata.org/w/api.php?action=wbgetentities&format=json\
         &props=claims%7Csitelinks%2Furls&ids={}",
        ids.join("%7C")
    ))
}

/// The items of a `wbgetentities` response by their own id. Missing items are left out; a
/// redirected id appears under the id it redirects to.
pub fn parse_entities(status: u16, raw: &[u8]) -> Result<BTreeMap<String, Value>> {
    let raw = body(status, raw, &[])?.context("no body")?;
    let mut value: Value = serde_json::from_slice(raw)?;
    if let Some(error) = value.get("error") {
        anyhow::bail!("wikidata error: {}", error["code"]);
    }
    let entities = value
        .get_mut("entities")
        .and_then(Value::as_object_mut)
        .context("no entities")?;
    Ok(std::mem::take(entities)
        .into_values()
        .filter(|e| e.get("missing").is_none())
        .filter_map(|e| Some((qid(e["id"].as_str()?)?.to_owned(), e)))
        .collect())
}

/// The best-rank string values of a property.
pub fn best_values<'a>(item: &'a Value, pid: &str) -> Vec<&'a str> {
    let claims = item["claims"][pid]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let ranked = |rank: &str| -> Vec<&'a str> {
        claims
            .iter()
            .filter(|c| c["rank"] == rank && c["mainsnak"]["snaktype"] == "value")
            .filter_map(|c| c["mainsnak"]["datavalue"]["value"].as_str())
            .collect()
    };
    let preferred = ranked("preferred");
    if preferred.is_empty() {
        ranked("normal")
    } else {
        preferred
    }
}

/// How a property's value becomes a link.
#[derive(Clone, Copy, Debug)]
pub enum Format {
    /// The value is a web address.
    Url,
    /// `$1` is replaced by the value, percent-encoded (`/` kept; validators reject it where it
    /// does not belong).
    Template(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Property {
    pub pid: &'static str,
    pub site: &'static str,
    pub format: Format,
    valid: fn(&str) -> bool,
}

fn digits(v: &str) -> bool {
    plain(v, 20, |c| c.is_ascii_digit()) && !v.starts_with('0')
}
/// A slug or package name: letters, digits and `-_.`.
fn slug(v: &str) -> bool {
    plain(v, 200, |c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}
/// Any single path segment, encoded when used.
fn segment(v: &str) -> bool {
    plain(v, 200, |c| {
        !c.is_control() && !c.is_whitespace() && !"/?#".contains(c)
    })
}
fn any_url(_: &str) -> bool {
    true
}
fn gog(v: &str) -> bool {
    v.split_once('/').is_some_and(|(kind, name)| {
        matches!(kind, "game" | "movie")
            && plain(name, 200, |c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
            })
    })
}
fn itch(v: &str) -> bool {
    web_url(v).is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str()
                .is_some_and(|h| h.ends_with(".itch.io") && h.len() > 8)
    })
}

const fn p(
    pid: &'static str,
    site: &'static str,
    format: Format,
    valid: fn(&str) -> bool,
) -> Property {
    Property {
        pid,
        site,
        format,
        valid,
    }
}
use Format::{Template as T, Url as U};

/// The links a place card takes from its item (besides Wikipedia).
pub const PLACE: &[Property] = &[
    p("P856", "Official website", U, any_url),
    p(
        "P3749",
        "Google Maps",
        T("https://www.google.com/maps?cid=$1"),
        digits,
    ),
    p(
        "P10046",
        "Apple Maps",
        T("https://maps.apple.com/place?auid=$1"),
        digits,
    ),
    p(
        "P3134",
        "Tripadvisor",
        T("https://www.tripadvisor.com/$1"),
        segment,
    ),
    p("P3108", "Yelp", T("https://www.yelp.com/biz/$1"), segment),
];
/// App stores and package catalogues for an app's item.
pub const APP: &[Property] = &[
    p(
        "P3861",
        "App Store",
        T("https://apps.apple.com/app/id$1"),
        digits,
    ),
    p(
        "P3418",
        "Google Play",
        T("https://play.google.com/store/apps/details?id=$1"),
        slug,
    ),
    p(
        "P3597",
        "F-Droid",
        T("https://f-droid.org/packages/$1"),
        slug,
    ),
    p("P4655", "Flathub", T("https://flathub.org/apps/$1"), slug),
    p("P4435", "Snap Store", T("https://snapcraft.io/$1"), slug),
    p(
        "P8443",
        "Homebrew",
        T("https://formulae.brew.sh/formula/$1"),
        |v| plain(v, 100, |c| c.is_ascii_alphanumeric() || "-_.@+".contains(c)),
    ),
];
/// Stores and game databases for a game's item.
pub const GAME: &[Property] = &[
    p(
        "P1733",
        "Steam",
        T("https://store.steampowered.com/app/$1/"),
        digits,
    ),
    p("P2725", "GOG", T("https://www.gog.com/$1"), gog),
    p(
        "P6278",
        "Epic Games Store",
        T("https://store.epicgames.com/p/$1"),
        slug,
    ),
    p("P7294", "itch.io", U, itch),
    p("P5794", "IGDB", T("https://www.igdb.com/games/$1"), slug),
    p(
        "P11688",
        "MobyGames",
        T("https://www.mobygames.com/game/$1/"),
        digits,
    ),
    p(
        "P6337",
        "PCGamingWiki",
        T("https://www.pcgamingwiki.com/wiki/$1"),
        segment,
    ),
    p(
        "P2816",
        "HowLongToBeat",
        T("https://howlongtobeat.com/game/$1"),
        digits,
    ),
];

/// The one valid best-rank value of a property, or `None` when there is none or several.
pub fn single<'a>(item: &'a Value, property: &Property) -> Option<&'a str> {
    let mut values: Vec<&str> = best_values(item, property.pid)
        .into_iter()
        .filter(|v| (property.valid)(v))
        .collect();
    values.dedup();
    match values.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// The links an item has for these properties.
pub fn links(item: &Value, properties: &[Property]) -> Vec<Link> {
    properties
        .iter()
        .filter_map(|property| {
            let value = single(item, property)?;
            let uri = match property.format {
                Format::Url => web_url(value)?.to_string(),
                Format::Template(t) => t.replace("$1", &encode_path(value)),
            };
            Some(Link::new(property.site, uri, VIA))
        })
        .collect()
}

/// Sitelinks that are never Wikipedia language editions.
const NOT_WIKIPEDIA: &[&str] = &[
    "commonswiki",
    "metawiki",
    "mediawikiwiki",
    "specieswiki",
    "sourceswiki",
    "wikidatawiki",
    "wikifunctionswiki",
    "incubatorwiki",
    "outreachwiki",
    "wikimaniawiki",
];

/// The item's Wikipedia articles, one per language edition, ordered by site id.
pub fn wikipedia(item: &Value) -> Vec<Link> {
    let Some(sitelinks) = item["sitelinks"].as_object() else {
        return Vec::new();
    };
    sitelinks
        .iter()
        .filter(|(site, _)| {
            site.strip_suffix("wiki")
                .is_some_and(|lang| plain(lang, 20, |c| c.is_ascii_lowercase() || c == '_'))
                && !NOT_WIKIPEDIA.contains(&site.as_str())
        })
        .filter_map(|(_, link)| {
            let url = web_url(link["url"].as_str()?)?;
            url.host_str()?
                .ends_with(".wikipedia.org")
                .then(|| Link::new("Wikipedia", url.to_string(), VIA))
        })
        .collect()
}

/// The item a Wikidata link names (`www.wikidata.org/wiki/Q…`, `/entity/Q…`).
pub fn wikidata_item(uri: &str) -> Option<String> {
    let url = super::web_url(uri)?;
    if !matches!(
        url.host_str()?,
        "www.wikidata.org" | "wikidata.org" | "m.wikidata.org"
    ) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    match segments.as_slice() {
        ["wiki" | "entity", q] => qid(q).map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;

    fn items(name: &str) -> BTreeMap<String, Value> {
        parse_entities(200, &fixture::read(name)).unwrap()
    }
    fn sites(links: &[Link]) -> Vec<(&str, &str)> {
        links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str()))
            .collect()
    }

    #[test]
    fn request() {
        let r = entities_request(&["Q7024", "Q153306"]).unwrap();
        assert_eq!(
            r.url.as_str(),
            "https://www.wikidata.org/w/api.php?action=wbgetentities&format=json\
             &props=claims%7Csitelinks%2Furls&ids=Q7024%7CQ153306"
        );
        assert!(entities_request(&[]).is_none());
        assert!(entities_request(&["Q7024", "P31"]).is_none());
        assert!(entities_request(&["Q07024"]).is_none());
        assert!(entities_request(&vec!["Q1"; MAX_IDS + 1]).is_none());
    }

    #[test]
    fn errors() {
        assert!(parse_entities(429, b"").is_err());
        assert!(parse_entities(200, br#"{"error":{"code":"no-such-entity"}}"#).is_err());
        let map = parse_entities(200, br#"{"entities":{"Q404":{"id":"Q404","missing":""}}}"#);
        assert!(map.unwrap().is_empty());
    }

    #[test]
    fn places() {
        let items = items("wikidata-places.json.gz");
        assert_eq!(items.len(), 3);
        assert_eq!(best_values(&items["Q7024"], "P402"), ["2888417"]);
        assert_eq!(
            sites(&links(&items["Q153306"], PLACE)),
            [
                ("Official website", "https://www.mnw.art.pl/"),
                (
                    "Google Maps",
                    "https://www.google.com/maps?cid=514092630901203410"
                ),
                ("Tripadvisor", "https://www.tripadvisor.com/274996"),
                (
                    "Yelp",
                    "https://www.yelp.com/biz/muzeum-narodowe-warszawa-4"
                ),
            ]
        );
        assert_eq!(
            sites(&links(&items["Q38945047"], PLACE))[2],
            (
                "Apple Maps",
                "https://maps.apple.com/place?auid=1218585579701879383"
            )
        );
        // A place's app-store statements are not place links.
        assert_eq!(
            sites(&links(&items["Q7024"], PLACE)),
            [("Official website", "https://www.lugano.ch/")]
        );
        let wiki = wikipedia(&items["Q7024"]);
        assert!(wiki.len() > 100);
        assert!(wiki
            .iter()
            .all(|l| l.uri.contains(".wikipedia.org/") && l.via == "wikidata"));
        assert!(wiki
            .iter()
            .any(|l| l.uri == "https://de.wikipedia.org/wiki/Lugano"));
    }

    #[test]
    fn apps_and_games() {
        let items = items("wikidata-games-apps.json.gz");
        assert_eq!(
            sites(&links(&items["Q171477"], APP)),
            [
                ("App Store", "https://apps.apple.com/app/id650377962"),
                (
                    "Google Play",
                    "https://play.google.com/store/apps/details?id=org.videolan.vlc"
                ),
                ("F-Droid", "https://f-droid.org/packages/org.videolan.vlc"),
                ("Flathub", "https://flathub.org/apps/org.videolan.VLC"),
                ("Snap Store", "https://snapcraft.io/vlc"),
            ]
        );
        // GOG keeps the one value that fits its pattern; IGDB and MobyGames list two editions
        // each, which is ambiguous.
        assert_eq!(
            sites(&links(&items["Q23013817"], GAME)),
            [
                ("Steam", "https://store.steampowered.com/app/413150/"),
                ("GOG", "https://www.gog.com/game/stardew_valley"),
                (
                    "PCGamingWiki",
                    "https://www.pcgamingwiki.com/wiki/Stardew_Valley"
                ),
                ("HowLongToBeat", "https://howlongtobeat.com/game/34716"),
            ]
        );
    }

    #[test]
    fn ranks_and_validation() {
        let item: Value = serde_json::json!({"claims": {
            "P1733": [
                {"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": "10"}}},
                {"rank": "preferred", "mainsnak": {"snaktype": "value", "datavalue": {"value": "20"}}},
                {"rank": "deprecated", "mainsnak": {"snaktype": "value", "datavalue": {"value": "30"}}}
            ],
            "P2816": [{"rank": "normal", "mainsnak": {"snaktype": "somevalue"}}],
            "P6337": [{"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": "A/B"}}}],
            "P5794": [{"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": "x?y"}}}],
            "P7294": [{"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": "https://itch.io.example.com/"}}}],
            "P6278": [{"rank": "deprecated", "mainsnak": {"snaktype": "value", "datavalue": {"value": "old"}}}]
        }});
        assert_eq!(
            sites(&links(&item, GAME)),
            [("Steam", "https://store.steampowered.com/app/20/")]
        );
        let encoded: Value = serde_json::json!({"claims": {"P3108": [
            {"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": "café-zürich"}}}
        ]}});
        assert_eq!(
            links(&encoded, PLACE)[0].uri,
            "https://www.yelp.com/biz/caf%C3%A9-z%C3%BCrich"
        );
    }

    #[test]
    fn ids() {
        assert_eq!(qid("Q7024"), Some("Q7024"));
        for bad in ["Q", "Q0", "Q01", "q7024", "Q7024 ", "P31", "Q1;Q2"] {
            assert_eq!(qid(bad), None, "{bad}");
        }
        assert_eq!(key("Q7024").as_str(), "wikidata:Q7024");
    }

    /// An answer of the wrong shape is an error, never a panic.
    #[test]
    fn a_malformed_answer_is_an_error() {
        for raw in [&b"[]"[..], b"false", b"{\"entities\": []}", b"{}"] {
            assert!(
                parse_entities(200, raw).is_err(),
                "{}",
                String::from_utf8_lossy(raw)
            );
        }
    }
}
