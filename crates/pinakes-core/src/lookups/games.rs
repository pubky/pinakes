//! Games from a Steam appid: ProtonDB (its summary is the existence check), PCGamingWiki (its
//! appid redirect names the page), SteamDB (a template; never fetched), and GOG through GOG
//! GamesDB, keeping only its `gog` and `steam` rows.
use super::{body, plain, web_url, Key, Link, Request};
use anyhow::{Context, Result};
use serde_json::Value;

/// A Steam appid: a positive number of up to ten digits.
pub fn appid(value: &str) -> Option<u64> {
    number(value, 10)
}
/// A positive number of up to `max` digits, without leading zeros.
fn number(value: &str, max: usize) -> Option<u64> {
    (plain(value, max, |c| c.is_ascii_digit()) && !value.starts_with('0'))
        .then(|| value.parse().ok())
        .flatten()
}

/// The appid of a game's own page: its Steam store page (`/app/{id}`, with or without its name),
/// its Steam Community hub, SteamDB or ProtonDB page (`/app/{id}`), or PCGamingWiki's appid
/// lookup. A page under the game (a discussion, a review, a guide, workshop or news items, a
/// SteamDB subpage) is another object and names no game.
pub fn appid_from_uri(uri: &str) -> Option<u64> {
    let url = web_url(uri)?;
    let host = url.host_str()?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let slug = |s: &str| plain(s, 200, |c| c.is_ascii_alphanumeric() || "_-.%".contains(c));
    match (host, segments.as_slice()) {
        ("store.steampowered.com", ["app", id]) => appid(id),
        ("store.steampowered.com", ["app", id, name]) if slug(name) => appid(id),
        (
            "steamcommunity.com" | "steamdb.info" | "www.protondb.com" | "protondb.com",
            ["app", id],
        ) => appid(id),
        ("www.pcgamingwiki.com" | "pcgamingwiki.com", ["api", "appid.php"]) => url
            .query_pairs()
            .find(|(k, _)| k == "appid")
            .and_then(|(_, v)| appid(&v)),
        _ => None,
    }
}

pub fn steam_key(appid: u64) -> Key {
    Key::new("steam", &appid.to_string())
}
pub fn gog_key(product: u64) -> Key {
    Key::new("gog", &product.to_string())
}

/// The links made from the appid alone: the Steam store page and SteamDB.
pub fn templates(appid: u64) -> Vec<Link> {
    vec![
        Link::new(
            "Steam",
            format!("https://store.steampowered.com/app/{appid}/"),
            "steam",
        ),
        Link::new(
            "SteamDB",
            format!("https://steamdb.info/app/{appid}/"),
            "steam",
        ),
    ]
}

pub fn protondb_request(appid: u64) -> Option<Request> {
    Request::get(&format!(
        "https://www.protondb.com/api/v1/reports/summaries/{appid}.json"
    ))
}

/// ProtonDB's page, when it has a summary for the game.
pub fn parse_protondb(status: u16, raw: &[u8], appid: u64) -> Result<Option<Link>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let summary: Value = serde_json::from_slice(raw)?;
    Ok(summary["tier"].as_str().map(|_| {
        Link::new(
            "ProtonDB",
            format!("https://www.protondb.com/app/{appid}"),
            "protondb",
        )
    }))
}

/// PCGamingWiki's appid lookup. Its redirect is the answer, so it is not followed.
pub fn pcgamingwiki_request(appid: u64) -> Option<Request> {
    let mut r = Request::get(&format!(
        "https://www.pcgamingwiki.com/api/appid.php?appid={appid}"
    ))?;
    r.follow_redirects = false;
    Some(r)
}

/// The wiki page the lookup redirects to; an unknown appid answers without a redirect.
pub fn parse_pcgamingwiki(status: u16, location: Option<&str>) -> Result<Option<Link>> {
    match status {
        301 | 302 | 303 | 307 | 308 => {
            let url = web_url(location.context("redirect without location")?)
                .context("invalid location")?;
            let ok = url.scheme() == "https"
                && url.host_str() == Some("www.pcgamingwiki.com")
                && url.path().len() > "/wiki/".len()
                && url.path().starts_with("/wiki/");
            Ok(ok.then(|| Link::new("PCGamingWiki", url.to_string(), "pcgamingwiki")))
        }
        200 | 404 => Ok(None),
        s => anyhow::bail!("unexpected status {s}"),
    }
}

/// GOG GamesDB's record for a Steam release.
pub fn gog_gamesdb_request(appid: u64) -> Option<Request> {
    Request::get(&format!(
        "https://gamesdb.gog.com/platforms/steam/external_releases/{appid}"
    ))
}

/// The GOG product of a Steam game: the record must list this appid among its Steam rows and
/// exactly one numeric GOG row. Every other platform row is ignored.
pub fn parse_gog_gamesdb(status: u16, raw: &[u8], appid: u64) -> Result<Option<u64>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let record: Value = serde_json::from_slice(raw)?;
    let releases = record["game"]["releases"]
        .as_array()
        .context("no releases")?;
    let ids = |platform: &str| -> Vec<u64> {
        let mut ids: Vec<u64> = releases
            .iter()
            .filter(|r| r["platform_id"] == platform)
            .filter_map(|r| number(r["external_id"].as_str()?, 15))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    if !ids("steam").contains(&appid) {
        return Ok(None);
    }
    Ok(match ids("gog").as_slice() {
        [one] => Some(*one),
        _ => None,
    })
}

/// GOG's product record, which names the store page.
pub fn gog_product_request(product: u64) -> Option<Request> {
    Request::get(&format!("https://api.gog.com/products/{product}"))
}

/// The GOG store page of a game product.
pub fn parse_gog_product(status: u16, raw: &[u8], product: u64) -> Result<Option<Link>> {
    let Some(raw) = body(status, raw, &[404])? else {
        return Ok(None);
    };
    let record: Value = serde_json::from_slice(raw)?;
    anyhow::ensure!(
        record["id"].as_u64() == Some(product),
        "GOG answered for another product"
    );
    if record["game_type"] != "game" || record["is_secret"] == true {
        return Ok(None);
    }
    Ok(record["links"]["product_card"]
        .as_str()
        .and_then(web_url)
        .filter(|u| {
            u.scheme() == "https"
                && u.host_str() == Some("www.gog.com")
                && u.path().contains("/game/")
        })
        .map(|u| Link::new("GOG", u.to_string(), "gog")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture;

    #[test]
    fn appids() {
        for (uri, id) in [
            ("https://store.steampowered.com/app/620/Portal_2/", 620),
            ("https://steamcommunity.com/app/413150", 413150),
            ("https://steamdb.info/app/620/", 620),
            ("https://www.protondb.com/app/620", 620),
            ("https://www.pcgamingwiki.com/api/appid.php?appid=620", 620),
        ] {
            assert_eq!(appid_from_uri(uri), Some(id), "{uri}");
        }
        for uri in [
            "https://store.steampowered.com/app/0620/",
            "https://store.steampowered.com/bundle/620/",
            "https://example.com/app/620",
            "https://store.steampowered.com/app/12345678901/",
            // Objects under a game are not the game. The store has no child object of the shape
            // `/app/{id}/{name}`: that is the game's page with its name; its reviews, news and
            // age check live elsewhere (`/app/{id}/{name}/reviews`, `/news/app/{id}`,
            // `/agecheck/app/{id}`).
            "https://steamcommunity.com/app/620/discussions/0/123456789/",
            "https://steamcommunity.com/app/620/reviews/",
            "https://steamcommunity.com/app/620/workshop/",
            "https://steamcommunity.com/app/620/allnews/",
            "https://steamcommunity.com/app/620/guides/",
            "https://store.steampowered.com/app/620/Portal_2/reviews/",
            "https://steamdb.info/app/620/charts/",
            "https://www.protondb.com/app/620/reports",
        ] {
            assert_eq!(appid_from_uri(uri), None, "{uri}");
        }
        for uri in [
            "https://store.steampowered.com/app/620",
            "https://steamcommunity.com/app/620/",
        ] {
            assert_eq!(appid_from_uri(uri), Some(620), "{uri}");
        }
        assert_eq!(steam_key(620).as_str(), "steam:620");
        assert_eq!(
            templates(620)
                .iter()
                .map(|l| l.uri.as_str())
                .collect::<Vec<_>>(),
            [
                "https://store.steampowered.com/app/620/",
                "https://steamdb.info/app/620/"
            ]
        );
    }

    #[test]
    fn protondb() {
        assert_eq!(
            protondb_request(620).unwrap().url.as_str(),
            "https://www.protondb.com/api/v1/reports/summaries/620.json"
        );
        let link = parse_protondb(200, &fixture::read("protondb-620.json"), 620)
            .unwrap()
            .unwrap();
        assert_eq!(link.uri, "https://www.protondb.com/app/620");
        assert_eq!(
            parse_protondb(404, &fixture::read("protondb-1.html"), 1).unwrap(),
            None
        );
        assert!(parse_protondb(503, b"", 1).is_err());
    }

    #[test]
    fn pcgamingwiki() {
        let r = pcgamingwiki_request(620).unwrap();
        assert!(!r.follow_redirects);
        // Recorded: 302 to the wiki page for 620, 200 without a redirect for 1.
        let link = parse_pcgamingwiki(302, Some("https://www.pcgamingwiki.com/wiki/Portal_2"))
            .unwrap()
            .unwrap();
        assert_eq!(link.uri, "https://www.pcgamingwiki.com/wiki/Portal_2");
        assert_eq!(parse_pcgamingwiki(200, None).unwrap(), None);
        assert_eq!(
            parse_pcgamingwiki(302, Some("https://example.com/wiki/X")).unwrap(),
            None
        );
        assert_eq!(
            parse_pcgamingwiki(302, Some("https://www.pcgamingwiki.com/")).unwrap(),
            None
        );
        assert!(parse_pcgamingwiki(302, None).is_err());
        assert!(parse_pcgamingwiki(429, None).is_err());
    }

    #[test]
    fn gog() {
        assert_eq!(
            gog_gamesdb_request(413150).unwrap().url.as_str(),
            "https://gamesdb.gog.com/platforms/steam/external_releases/413150"
        );
        let stardew = fixture::read("gog-gamesdb-steam-413150.json");
        assert_eq!(
            parse_gog_gamesdb(200, &stardew, 413150).unwrap(),
            Some(1453375253)
        );
        // The record must list the asked appid as a Steam row.
        assert_eq!(parse_gog_gamesdb(200, &stardew, 620).unwrap(), None);
        // The Witcher 3 has several GOG rows (editions, clients): ambiguous.
        let witcher = fixture::read("gog-gamesdb-steam-292030.json");
        assert_eq!(parse_gog_gamesdb(200, &witcher, 292030).unwrap(), None);
        let unknown = fixture::read("gog-gamesdb-steam-1.json");
        assert_eq!(parse_gog_gamesdb(404, &unknown, 1).unwrap(), None);

        let product = fixture::read("gog-product-1453375253.json");
        let link = parse_gog_product(200, &product, 1453375253)
            .unwrap()
            .unwrap();
        assert_eq!(
            (link.site.as_str(), link.uri.as_str()),
            ("GOG", "https://www.gog.com/game/stardew_valley")
        );
        assert!(parse_gog_product(200, &product, 1).is_err());
        assert_eq!(gog_key(1453375253).as_str(), "gog:1453375253");
    }
}
