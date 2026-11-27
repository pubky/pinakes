//! With every catalogue off, what this step's map-link readers change is the answers of map
//! services' links only (the coordinator's decision, `decisions.md`): a link elsewhere is read by
//! the same source and resolver as before, has no Google place CID and no location link. Over
//! every source's own examples and every link in the test fixtures.
//!
//! The other half of the decision (nothing reads the OSM catalogue's rows while it is off: cards
//! by link and by ID, a shared OSM link's card, the "maybe same" join, renames, clusters, the
//! dump) is pinned in `catalogue_osm.rs`, each test with the catalogue off at its end.
use pinakes_core::{lookups::places, resolver::geo, sources};

/// Whether `uri` is a map service's link: Google's (any of its domains, and its short links),
/// Apple Maps, Organic Maps, OpenStreetMap and OsmAnd map views, or a `geo:` URI.
fn map_service(uri: &str) -> bool {
    if uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("geo:")) {
        return true;
    }
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    geo::google_host(&host).is_some()
        || matches!(
            host.as_str(),
            "maps.app.goo.gl"
                | "goo.gl"
                | "maps.apple.com"
                | "omaps.app"
                | "www.openstreetmap.org"
                | "openstreetmap.org"
                | "osm.org"
                | "www.osm.org"
                | "osmand.net"
                | "www.osmand.net"
        )
}

fn fixture_links(dir: &std::path::Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            fixture_links(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            strings(&value, out);
        }
    }
}
fn strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s)
            if (s.starts_with("https://") || s.starts_with("http://")) && s.len() < 2048 =>
        {
            out.push(s.clone())
        }
        serde_json::Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        serde_json::Value::Object(o) => o.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

#[test]
fn only_map_service_links_meet_the_map_link_readers() {
    let mut links: Vec<String> = sources::REGISTRY
        .iter()
        .flat_map(|s| s.examples().iter().map(|e| e.uri.to_owned()))
        .collect();
    fixture_links(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        &mut links,
    );
    // Near misses: Google's and Apple's other services, OSM's other pages, look-alike hosts.
    links.extend(
        [
            "https://www.google.com/search?q=Eiffel+Tower",
            "https://docs.google.com/document/d/1/edit",
            "https://www.apple.com/maps/",
            "https://apps.apple.com/app/id915056765",
            "https://wiki.openstreetmap.org/wiki/Key:amenity",
            "https://maps.google.com.evil.example/maps/place/X/data=!1s0x1:0x2",
            "https://www.google.evil.example/maps/place/X/@1,2,3z",
            "https://maps.apple.com.evil.example/?q=X&ll=1,2",
            "https://omaps.app.evil.example/B4srhdHVVt/X",
            "https://goo.gl.evil.example/maps/abc",
        ]
        .map(str::to_owned),
    );
    links.sort();
    links.dedup();
    let (mut checked, mut on_maps) = (0, 0);
    for uri in &links {
        if map_service(uri) {
            on_maps += 1;
            continue;
        }
        checked += 1;
        let reader = sources::reading(uri).map(|s| s.name());
        assert!(
            !matches!(reader, Some("google_maps" | "apple_maps")),
            "{uri} is read by {reader:?}"
        );
        assert_eq!(geo::google_cid(uri), None, "{uri}");
        assert!(
            places::place_page(uri).is_none_or(|(p, _, _)| p != "P3749"),
            "{uri}"
        );
        assert!(geo::location_link(uri).is_none(), "{uri}");
    }
    assert!(checked > 500 && on_maps > 10, "{checked} {on_maps}");
}
