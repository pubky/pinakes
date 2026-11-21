use super::{time, Attendance, EventStatus, Offers, Organizer, Venue};
use crate::{
    model::{LocationSource, Point, ResourceMeta},
    resolver::web::origin_url,
};
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

pub fn string(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.get("@value").and_then(Value::as_str).map(str::to_owned))
}
fn first(v: &Value) -> &Value {
    v.as_array().and_then(|v| v.first()).unwrap_or(v)
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|v| v.parse().ok()))
        .filter(|n| n.is_finite())
}
pub fn point(v: &Value) -> Option<Point> {
    let v = v.get("geo").unwrap_or(v);
    let lat = number(&v["latitude"])?;
    let lon = number(&v["longitude"])?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some(Point { lat, lon })
}
pub fn image(v: &Value, base: &Url) -> Option<String> {
    string(v)
        .or_else(|| string(&v["url"]))
        .or_else(|| string(&v["contentUrl"]))
        .or_else(|| v.as_array()?.iter().find_map(|v| image(v, base)))
        .and_then(|s| origin_url(base, &s))
}
fn fill<T>(dst: &mut Option<T>, src: Option<T>) {
    if dst.is_none() {
        *dst = src;
    }
}
fn is_event(v: &Value) -> bool {
    let kind = &v["@type"];
    kind.as_str()
        .into_iter()
        .chain(
            kind.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .any(|s| crate::model::type_path(s).iter().any(|s| s == "Event"))
}
/// Repair raw control characters in strings and trailing commas only, without evaluating JS.
pub fn lenient_json(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }
    let mut out = String::with_capacity(text.len());
    let mut quoted = false;
    let mut escaped = false;
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => quoted = false,
                c if c < ' ' => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                    continue;
                }
                _ => {}
            }
        } else if c == '"' {
            quoted = true;
        } else if c == ','
            && chars[i + 1..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| matches!(c, '}' | ']'))
        {
            continue;
        }
        out.push(c);
    }
    serde_json::from_str(&out).ok()
}
/// Every event node in `v`, in document order (an event's own nested entities aside), each
/// with whether it sits in a list of several main entities.
fn event_nodes<'a>(v: &'a Value, out: &mut Vec<(&'a Value, bool)>, in_list: bool) {
    if let Some(values) = v.as_array() {
        values.iter().for_each(|v| event_nodes(v, out, in_list));
    } else if is_event(v) {
        out.push((v, in_list));
    } else {
        for key in ["@graph", "mainEntity"] {
            if let Some(nested) = v.get(key) {
                event_nodes(
                    nested,
                    out,
                    in_list || (key == "mainEntity" && main_list(nested)),
                );
            }
        }
    }
}
/// Whether a `mainEntity` value lists several entities.
pub fn main_list(main: &Value) -> bool {
    main.as_array().is_some_and(|a| a.len() > 1)
}
/// The event nodes of a page's structured data (`values`), each with whether it sits in a list
/// of several main entities; the page's addresses; and the kinds the page lists among them
/// (`listed_kinds`: "Event" when it lists several events, an artist's concerts).
type Listed<'a> = (
    Vec<(&'a Value, bool)>,
    Vec<String>,
    std::collections::BTreeSet<&'static str>,
);
fn events_listed<'a>(values: &[&'a Value], base: &Url, pages: &[String]) -> Listed<'a> {
    let mut nodes = vec![];
    for v in values {
        event_nodes(v, &mut nodes, false);
    }
    let pages: Vec<String> = if pages.is_empty() {
        page_identity(base.as_str()).into_iter().collect()
    } else {
        pages.to_vec()
    };
    let entries: Vec<&Value> = nodes
        .iter()
        .filter(|(n, in_list)| entry(n, base, &pages, *in_list))
        .map(|(n, _)| *n)
        .collect();
    let kinds = listed_kinds(&entries, base, &pages);
    (nodes, pages, kinds)
}
/// Whether a page's structured data lists several events (an artist's or a venue's concerts,
/// each naming its own page): such a page is a list, and no reading of it (its JSON-LD, a
/// platform's embedded state) yields an event. `pages`: the page's addresses
/// (`page_addresses`; empty, the base alone).
pub fn lists_events(values: &[&Value], base: &Url, pages: &[String]) -> bool {
    events_listed(values, base, pages).2.contains("Event")
}
/// The page's addresses, as what names the page is judged by (`page_identity`): the one asked
/// for, the one fetched, its head canonical links and its `og:url` (an AMP page's event names
/// the canonical).
pub fn page_addresses(uri: &str, fetched: &Url, html: &Html) -> Vec<String> {
    let select = |s: &str| Selector::parse(s).unwrap();
    [uri.to_owned(), fetched.to_string()]
        .into_iter()
        .chain(
            html.select(&select("head link[rel~=canonical][href]"))
                .filter_map(|l| origin_url(fetched, l.value().attr("href")?)),
        )
        .chain(
            html.select(&select("head meta[property='og:url'][content]"))
                .filter_map(|m| origin_url(fetched, m.value().attr("content")?)),
        )
        .filter_map(|u| page_identity(&u))
        .collect()
}
pub fn jsonld(doc: &mut ResourceMeta, v: &Value, base: &Url) {
    jsonld_all(doc, &[v], base, &[]);
}
/// The event of a page's structured data (`values`: its scripts, read together): the page's own
/// event (its url is one of the page's addresses, `pages`: `page_addresses`, or the base alone
/// when empty) over another the page lists, else its one event; a page listing several events
/// and none its own (an artist's or a venue's concerts: `listed_kinds`) is a list, not its first
/// entry, and yields no event.
pub fn jsonld_all(doc: &mut ResourceMeta, values: &[&Value], base: &Url, pages: &[String]) {
    let (nodes, pages, kinds) = events_listed(values, base, pages);
    let Some(v) = nodes
        .iter()
        .find(|(n, _)| names_page_in(n, base, &pages) == Some(true))
        .or_else(|| {
            nodes.iter().find(|(n, in_list)| {
                !(entry(n, base, &pages, *in_list) && listed(n, base, &pages, &kinds))
            })
        })
        .map(|(n, _)| *n)
    else {
        return;
    };
    let kind = v["@type"]
        .as_str()
        .or_else(|| {
            v["@type"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .find(|s| crate::model::type_path(s).contains(&"Event".into()))
        })
        .unwrap_or("Event");
    if !doc.type_path.contains(&"Event".into()) {
        doc.set_type(kind);
        doc.source = "jsonld".into();
    }
    fields(doc, v, base, false);
    // The same event in other scripts (a theme's and a plugin's node, one naming it, one dating
    // it, whatever `@id` each gives it): what the chosen node lacks comes from them, in order,
    // as reading each script in turn once did.
    let key = entry_key(v, base);
    for (other, _) in nodes.iter().filter(|(n, _)| !std::ptr::eq(*n, v)) {
        if key.is_some() && entry_key(other, base) == key {
            fields(doc, other, base, false);
        }
    }
}
pub fn activity(doc: &mut ResourceMeta, v: &Value, base: &Url) -> bool {
    if v["type"] != "Event" {
        return false;
    }
    if !doc.type_path.contains(&"Event".into()) {
        doc.set_type("Event");
        doc.source = "activitypub".into();
    }
    fields(doc, v, base, true);
    true
}
fn fields(doc: &mut ResourceMeta, v: &Value, base: &Url, ap: bool) {
    // A page script's JSON-LD arrives decoded (`web::decode_json_strings`); JSON fetched as a
    // body is the publisher's text as it is.
    let text = string;
    fill(&mut doc.name, text(&v["name"]));
    fill(
        &mut doc.description,
        text(&v["description"])
            .and_then(crate::resolver::html::plain_text)
            .or_else(|| text(&v["summary"]))
            .or_else(|| {
                string(&v["content"]).and_then(|s| {
                    let html = crate::resolver::html::fragment(&s).ok()?;
                    Some(html.root_element().text().collect())
                })
            }),
    );
    fill(
        &mut doc.image,
        image(&v["image"], base).or_else(|| {
            v["attachment"]
                .as_array()?
                .iter()
                .filter(|v| {
                    v["mediaType"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("image/"))
                        || v["name"] == "Banner"
                })
                .find_map(|v| image(v, base))
        }),
    );
    if ap {
        fill(
            &mut doc.event.tz,
            string(&v["mz:timezone"])
                .or_else(|| string(&v["timezone"]))
                .and_then(|s| time::zone(&s).map(|s| s.to_string())),
        );
        fill(&mut doc.event.join_mode, string(&v["joinMode"]));
    }
    for (key, end) in [
        (if ap { "startTime" } else { "startDate" }, false),
        (if ap { "endTime" } else { "endDate" }, true),
    ] {
        if let Some(s) = v[key].as_str() {
            set_time(doc, s, end);
        }
    }
    if doc.event.all_day == Some(false) && doc.event.tz.is_none() {
        if let Some(end) = v[if ap { "endTime" } else { "endDate" }]
            .as_str()
            .map(str::trim)
            .filter(|s| s.len() == 10 || s.len() == 8)
        {
            if let Some(start) = v[if ap { "startTime" } else { "startDate" }]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
            {
                use chrono::TimeZone;
                doc.end = time::naive(end)
                    .and_then(|d| d.checked_add_signed(chrono::Duration::days(1)))
                    .and_then(|d| start.offset().from_local_datetime(&d).single())
                    .map(|d| d.to_utc());
                doc.event.end_local = None;
            }
        }
    }
    fill(
        &mut doc.event.event_status,
        v["eventStatus"]
            .as_str()
            .or_else(|| v["ical:status"].as_str())
            .or_else(|| v["status"].as_str())
            .and_then(EventStatus::parse),
    );
    let loc = first(&v["location"]);
    let virtual_location = loc["@type"] == "VirtualLocation" || loc["type"] == "VirtualLocation";
    let mode = v["eventAttendanceMode"].as_str().unwrap_or("");
    fill(
        &mut doc.event.attendance,
        if mode.ends_with("MixedEventAttendanceMode") {
            Some(Attendance::Mixed)
        } else if virtual_location
            || mode.ends_with("OnlineEventAttendanceMode")
            || v["isOnline"] == true
        {
            Some(Attendance::Online)
        } else if mode.ends_with("OfflineEventAttendanceMode") || v["isOnline"] == false {
            Some(Attendance::Offline)
        } else {
            None
        },
    );
    if doc.event.attendance != Some(Attendance::Online) {
        let address = loc.get("address").unwrap_or(loc);
        let mut venue = Venue {
            name: string(&loc["name"]),
            ..Default::default()
        };
        for (k, dst) in [
            ("streetAddress", "street"),
            ("addressLocality", "city"),
            ("postalCode", "postalcode"),
            ("addressCountry", "country"),
            ("addressRegion", "region"),
        ] {
            if let Some(s) = string(&address[k])
                .or_else(|| string(&address[k]["name"]))
                .filter(|s| !s.trim().is_empty())
            {
                venue.address.insert(dst.into(), s);
            }
        }
        venue.text = string(loc).or_else(|| string(address)).unwrap_or_else(|| {
            venue
                .name
                .iter()
                .chain(venue.address.values())
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        });
        venue.location = point(loc);
        venue.location_source = venue.location.as_ref().map(|_| LocationSource::Page);
        if !venue.text.is_empty() || venue.location.is_some() {
            if let Some(existing) = &mut doc.event.venue {
                if existing.text.is_empty() {
                    existing.text = venue.text;
                }
                fill(&mut existing.name, venue.name);
                for (k, v) in venue.address {
                    existing.address.entry(k).or_insert(v);
                }
                fill(&mut existing.location, venue.location);
                fill(&mut existing.location_source, venue.location_source);
            } else {
                doc.event.venue = Some(venue);
            }
        }
        fill(
            &mut doc.location,
            doc.event.venue.as_ref().and_then(|v| v.location.clone()),
        );
        if doc.location.is_some() {
            fill(&mut doc.location_source, Some(LocationSource::Page));
        }
    }
    let organizer = first(
        v.get("organizer")
            .or_else(|| v.get("attributedTo"))
            .or_else(|| v.get("actor"))
            .unwrap_or(&Value::Null),
    );
    let name = string(&organizer["name"]).or_else(|| string(organizer));
    fill(
        &mut doc.event.organizer,
        name.map(|name| Organizer {
            url: string(&organizer["url"])
                .and_then(|s| origin_url(base, &s))
                .or_else(|| {
                    Url::parse(&name)
                        .ok()
                        .and_then(|u| origin_url(base, u.as_str()))
                }),
            name,
        }),
    );
    if doc.event.performers.is_empty() && owns_page(v, base) {
        doc.event.performers = performers(v, base);
    }
    if let Some(offer) = v.get("offers") {
        let values: Vec<&Value> = offer
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_else(|| vec![offer]);
        let prices: Vec<f64> = values
            .iter()
            .flat_map(|v| {
                [
                    number(&v["price"]),
                    number(&v["lowPrice"]),
                    number(&v["highPrice"]),
                ]
            })
            .flatten()
            .collect();
        fill(
            &mut doc.event.offers,
            Some(Offers {
                low_price: prices.iter().copied().reduce(f64::min),
                high_price: prices.iter().copied().reduce(f64::max),
                currency: values.iter().find_map(|v| string(&v["priceCurrency"])),
            }),
        );
    }
}
/// Whether an event node is the page's own: the page it names is this page (tracking
/// parameters and fragments aside). Another event a page describes beside its own (a venue's
/// next show, a related date) never donates to the page's event.
pub fn owns_page(v: &Value, base: &Url) -> bool {
    names_page(v, base) == Some(true)
}
/// The kinds of content a page may list several of: a page whose structured data holds more
/// than one entry of one of these lists them (an artist's concerts, a FAQ's questions, a
/// category's products, a blog's posts). An entry (`entry`) is an entity naming another page
/// (its `url`, else `@id`, is another address), or one among several listed as a page's main
/// entity; an entity naming no page is the page's own, as a product page's variants or a
/// theme's and a plugin's node for one event are. The entries are parts of the page, never its
/// subject: the card is the page's own entity, else the page.
pub const LISTED: [&str; 8] = [
    "Event",
    "Question",
    "Product",
    "Article",
    "Place",
    "MediaObject",
    "JobPosting",
    "Recipe",
];
/// The kind (`LISTED`) a node is of, by its type's path.
pub fn kind_of(v: &Value) -> Option<&'static str> {
    let kind = v.get("@type")?;
    let names: Vec<&str> = kind
        .as_str()
        .into_iter()
        .chain(
            kind.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .collect();
    LISTED.iter().copied().find(|k| {
        names
            .iter()
            .any(|n| crate::model::type_path(n).iter().any(|t| t == k))
    })
}
/// What tells one entry from another of its kind, and makes two nodes one entity: the identity
/// of its `url` (`page_identity`, as the web route identifies a node), else its `@id` (a
/// fragment such as `#event` or `#pen`, kept), else its name, lowercased. Two nodes with the
/// same (a theme's and a plugin's node for one event, one with an `@id` and one without) are
/// one entry; a node with none is its own.
fn entry_key(v: &Value, base: &Url) -> Option<String> {
    v.get("url")
        .and_then(Value::as_str)
        .and_then(|u| origin_url(base, u))
        .and_then(|u| page_identity(&u))
        .map(|id| format!("url:{id}"))
        .or_else(|| {
            ["@id", "name"]
                .into_iter()
                .find_map(|f| v.get(f).and_then(Value::as_str))
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
        })
}
/// Whether a node may be an entry of a list the page holds (`LISTED`): it names another page
/// (none of the page's addresses, `pages`), or sits in a list of several main entities
/// (`in_list`), and is not the page's own.
pub fn entry(v: &Value, base: &Url, pages: &[String], in_list: bool) -> bool {
    (in_list || names_page_in(v, base, pages) == Some(false)) && !names_this_page(v, base, pages)
}
/// The kinds `entries` (nodes `entry` admits) list: those with more than one distinct entry.
pub fn listed_kinds(
    entries: &[&Value],
    base: &Url,
    pages: &[String],
) -> std::collections::BTreeSet<&'static str> {
    let mut by_kind: std::collections::BTreeMap<&str, std::collections::BTreeSet<String>> =
        Default::default();
    for (i, v) in entries.iter().enumerate() {
        if let Some(kind) = kind_of(v).filter(|_| !names_this_page(v, base, pages)) {
            by_kind
                .entry(kind)
                .or_default()
                .insert(entry_key(v, base).unwrap_or_else(|| format!("#{i}")));
        }
    }
    by_kind
        .into_iter()
        .filter(|(_, e)| e.len() > 1)
        .map(|(k, _)| k)
        .collect()
}
/// Whether an entry (`entry`) is of a kind the page lists (`listed_kinds`).
pub fn listed(
    v: &Value,
    base: &Url,
    pages: &[String],
    kinds: &std::collections::BTreeSet<&'static str>,
) -> bool {
    kind_of(v).is_some_and(|k| kinds.contains(k)) && !names_this_page(v, base, pages)
}
/// Whether a node is the page's own for a list: its url (else @id) is one of the page's
/// addresses (`pages`), or its `mainEntityOfPage` (a URL, a page node, or an array of either) is.
fn names_this_page(v: &Value, base: &Url, pages: &[String]) -> bool {
    if names_page_in(v, base, pages) == Some(true) {
        return true;
    }
    let Some(main) = v.get("mainEntityOfPage") else {
        return false;
    };
    main.as_array()
        .map_or_else(|| vec![main], |a| a.iter().take(16).collect())
        .into_iter()
        .any(|p| match p {
            Value::String(u) => names_page_in(&serde_json::json!({ "url": u }), base, pages),
            Value::Object(_) => names_page_in(p, base, pages),
            _ => None,
        } == Some(true))
}
/// Whether the page an event node names (its `url`, else its `@id`) is this page; `None` when
/// it names none. An explicit `url` decides: a local `@id` such as `#next-week` resolves to
/// this page whatever event it is, so it never outweighs a `url` naming another.
pub fn names_page(v: &Value, base: &Url) -> Option<bool> {
    // A node that names no page costs no address work.
    if !["url", "@id"]
        .into_iter()
        .any(|f| v.get(f).is_some_and(Value::is_string))
    {
        return None;
    }
    let page = page_identity(base.as_str())?;
    names_page_in(v, base, std::slice::from_ref(&page))
}
/// The identity of a page's address (its alias root, without a fragment), as `names_page`
/// compares addresses.
pub fn page_identity(u: &str) -> Option<String> {
    let mut u = Url::parse(u).ok()?;
    u.set_fragment(None);
    let u = crate::alias::root(u.as_str()).map_or_else(|| u.to_string(), |a| a.root);
    crate::model::identity(&u).ok().map(|v| v.0)
}
/// `names_page` over a page's several addresses (`pages`: identities, `page_identity`, of the
/// address asked for, the one fetched and its head canonical).
pub fn names_page_in(v: &Value, base: &Url, pages: &[String]) -> Option<bool> {
    ["url", "@id"]
        .into_iter()
        .filter_map(|f| v.get(f).and_then(Value::as_str))
        .filter_map(|u| origin_url(base, u))
        .find_map(|u| page_identity(&u))
        .map(|id| pages.contains(&id))
}
/// How much of a line-up a page's parse looks at, so a hostile one costs little: the first 64
/// entries, of which 17 performers with a name, and of each the first 256 link values, of which
/// the first 64 distinct links (values that are no link, such as nulls, nested arrays or other
/// schemes, take no place among the 64), of which 9 are kept. The 17 and 9 are one above what an
/// event keeps (`EventFields::bound`: 16 performers, 8 links), so trimming still has one to drop.
const LINEUP_ENTRIES: usize = 64;
const LINEUP_PERFORMERS: usize = 17;
const PERFORMER_LINK_VALUES: usize = 256;
const PERFORMER_LINKS_READ: usize = 64;
const PERFORMER_LINKS: usize = 9;

/// An event node's performers (`performer`, one or several): a name each, with the links the
/// page gives for it (`url`, `sameAs`); a bare name is a performer without links, a nameless
/// entry nothing.
pub fn performers(v: &Value, base: &Url) -> Vec<super::Performer> {
    let nodes: Vec<&Value> = match v.get("performer") {
        Some(Value::Array(all)) => all.iter().take(LINEUP_ENTRIES).collect(),
        Some(one) if !one.is_null() => vec![one],
        _ => vec![],
    };
    nodes
        .into_iter()
        .filter_map(|p| {
            let name = string(&p["name"]).or_else(|| string(p))?;
            let name = name.trim().to_owned();
            if name.is_empty() {
                return None;
            }
            let mut links = vec![];
            let mut seen = std::collections::HashSet::new();
            let values = ["url", "sameAs"]
                .into_iter()
                .flat_map(|field| field_values(p, field))
                .take(PERFORMER_LINK_VALUES);
            for value in values {
                if links.len() == PERFORMER_LINKS_READ {
                    break;
                }
                if let Some(url) = string(value).and_then(|s| origin_url(base, &s)) {
                    if seen.insert(url.clone()) {
                        links.push(url);
                    }
                }
            }
            // What keys the performer first (a page MusicBrainz can relate to its artist), then
            // a MusicBrainz artist page (a fact), so trimming a big line-up drops the other links
            // before them.
            links.sort_by_key(|l| match crate::music_sources::artist_link(l) {
                Some(crate::music_sources::ArtistLink::Lookup(_)) => 0,
                Some(crate::music_sources::ArtistLink::Mbid(_)) => 1,
                None => 2,
            });
            links.truncate(PERFORMER_LINKS);
            Some(super::Performer { name, links })
        })
        .take(LINEUP_PERFORMERS)
        .collect()
}
/// A field's values, one or several, read in place: an array is never copied, so a cap on the
/// values read bounds the work however long the array is.
fn field_values<'a>(node: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    let (all, one) = match node.get(field) {
        Some(Value::Array(all)) => (all.as_slice(), None),
        one => (&[][..], one),
    };
    all.iter().chain(one)
}
/// Set the start or end from a page value, trimmed at both ends first (a padded date-only value
/// is still recognised as a date by its length).
pub fn set_time(doc: &mut ResourceMeta, s: &str, end: bool) {
    let s = s.trim();
    if (end && doc.end.is_some()) || (!end && doc.start.is_some()) {
        return;
    }
    let Some(value) = time::parse(s, doc.event.tz.as_deref()) else {
        return;
    };
    if end && (s.len() == 10 || s.len() == 8) && doc.event.all_day == Some(false) {
        if let Some(next) =
            time::naive(s).and_then(|d| d.checked_add_signed(chrono::Duration::days(1)))
        {
            let local = next.to_string();
            doc.end = time::parse(&local, doc.event.tz.as_deref());
            doc.event.end_local = Some(local);
        }
        return;
    }
    if end {
        doc.end = Some(value);
        if time::offset(s).is_none() {
            doc.event.end_local = Some(s.into());
        }
    } else {
        doc.start = Some(value);
        doc.event.all_day = Some(s.len() == 10 || s.len() == 8);
        if time::offset(s).is_none() {
            doc.event.start_local = Some(s.into());
        }
    }
}
pub fn microformats(doc: &mut ResourceMeta, html: &Html, base: &Url) {
    let Some(root) = html.select(&Selector::parse(".h-event").unwrap()).next() else {
        return;
    };
    if !doc.type_path.contains(&"Event".into()) {
        doc.set_type("Event");
        doc.source = "microformats".into();
    }
    let value = |s: &str| {
        root.select(&Selector::parse(s).unwrap()).next().map(|el| {
            ["datetime", "content", "href", "src", "title", "value"]
                .iter()
                .find_map(|a| el.value().attr(a))
                .map(str::to_owned)
                .unwrap_or_else(|| el.text().collect::<String>().trim().into())
        })
    };
    fill(&mut doc.name, value(".p-name"));
    fill(&mut doc.description, value(".p-summary, .p-description"));
    fill(
        &mut doc.image,
        value(".u-photo").and_then(|s| origin_url(base, &s)),
    );
    if let Some(s) = value(".dt-start") {
        set_time(doc, &s, false);
    }
    if let Some(s) = value(".dt-end") {
        set_time(doc, &s, true);
    }
    fill(
        &mut doc.event.venue,
        value(".p-location").map(|text| Venue {
            text,
            ..Default::default()
        }),
    );
    if let Some((lat, lon)) = value(".p-latitude")
        .and_then(|s| s.parse::<f64>().ok())
        .zip(value(".p-longitude").and_then(|s| s.parse::<f64>().ok()))
    {
        let p = point(&serde_json::json!({"latitude":lat,"longitude":lon}));
        fill(&mut doc.location, p.clone());
        if let Some(v) = &mut doc.event.venue {
            fill(&mut v.location, p);
            v.location_source = Some(LocationSource::Page);
        }
    }
}
pub fn finalize(doc: &mut ResourceMeta, coordinates: bool) {
    if !doc.type_path.contains(&"Event".into()) {
        return;
    }
    if doc.event.attendance == Some(Attendance::Online) {
        doc.location = None;
        doc.location_source = None;
        doc.event.venue = None;
    }
    if coordinates && doc.event.tz.is_none() {
        doc.event.tz = doc.location.as_ref().and_then(time::coordinate_zone);
        if doc.event.tz.is_some() {
            doc.event.timezone_attribution = Some(time::ATTRIBUTION.into());
        }
    }
    if let Some(s) = &doc.event.start_local {
        doc.start = time::parse(s, doc.event.tz.as_deref());
    }
    if let Some(s) = &doc.event.end_local {
        doc.end = time::parse(s, doc.event.tz.as_deref());
    }
    if doc
        .end
        .zip(doc.start)
        .is_some_and(|(end, start)| end < start)
    {
        doc.end = None;
        doc.event.warnings.push("invalid_end".into());
    }
    doc.event.event_status.get_or_insert(EventStatus::Scheduled);
    doc.event.attendance.get_or_insert(Attendance::Offline);
    if doc.location.is_some() {
        doc.location_source.get_or_insert(LocationSource::Page);
    }
}

pub fn merge_missing(doc: &mut ResourceMeta, fallback: &ResourceMeta) -> anyhow::Result<()> {
    fn fill(a: &mut Value, b: &Value) {
        if a.is_null() {
            *a = b.clone();
        } else if let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object()) {
            for (k, v) in b {
                fill(a.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    }
    // A document's performers keep their own provenance; ones taken from `fallback` bring its.
    let own = (!doc.event.performers.is_empty()).then(|| doc.event.performers_from.clone());
    let mut event = serde_json::to_value(&doc.event)?;
    fill(&mut event, &serde_json::to_value(&fallback.event)?);
    doc.event = serde_json::from_value(event)?;
    if let Some(from) = own {
        doc.event.performers_from = from;
    }
    if doc.start.is_none() {
        doc.start = fallback.start;
    }
    if doc.end.is_none() {
        doc.end = fallback.end;
    }
    if doc.name.is_none() {
        doc.name = fallback.name.clone();
    }
    if doc.description.is_none() {
        doc.description = fallback.description.clone();
    }
    if doc.image.is_none() {
        doc.image = fallback.image.clone();
    }
    if doc.location.is_none() {
        doc.location = fallback.location.clone();
        doc.location_source = fallback.location_source.clone();
    }
    Ok(())
}

#[cfg(test)]
mod performer_tests {
    use super::*;
    /// The event service reads performers from the page's own node only: a node whose URL is
    /// the page (tracking parameters aside), never another event the page describes.
    #[test]
    fn performers_come_from_the_pages_own_node() {
        let base = Url::parse("https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena")
            .unwrap();
        let performer = serde_json::json!([{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"}]);
        let own = serde_json::json!({"@type":"MusicEvent","name":"Radiohead @ RAC Arena","url":"https://www.songkick.com/concerts/43402127-radiohead-at-rac-arena?utm_medium=organic&utm_source=microformat","startDate":"2027-05-03T19:30:00","performer":performer});
        let other = serde_json::json!({"@type":"MusicEvent","name":"Next","url":"https://www.songkick.com/concerts/43402211-radiohead-at-rac-arena","startDate":"2027-05-04T19:30:00","performer":performer});
        let anonymous = serde_json::json!({"@type":"MusicEvent","name":"Anonymous","startDate":"2027-05-04T19:30:00","performer":performer});
        assert!(owns_page(&own, &base));
        assert!(!owns_page(&other, &base));
        assert!(!owns_page(&anonymous, &base));
        assert_eq!(names_page(&anonymous, &base), None);
        let mut doc = ResourceMeta::new(base.as_str(), "web", 3600);
        jsonld(&mut doc, &other, &base);
        assert!(doc.event.performers.is_empty());
        jsonld(&mut doc, &own, &base);
        assert_eq!(doc.event.performers.len(), 1);
        assert_eq!(doc.event.performers[0].name, "Radiohead");
        // Listed after another event, the page's own is the one read.
        let mut doc = ResourceMeta::new(base.as_str(), "web", 3600);
        jsonld(
            &mut doc,
            &serde_json::json!({"@graph": [other, own]}),
            &base,
        );
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ RAC Arena"));
        assert_eq!(doc.event.performers.len(), 1);
    }
    /// An explicit `url` decides whose event a node is: a local `@id` resolves to the page it
    /// sits on, whichever event it names.
    #[test]
    fn a_fragment_id_never_outweighs_a_url_naming_another_page() {
        let base = Url::parse("https://venue.example/tonight").unwrap();
        let node = |fields: serde_json::Value| {
            let mut v = serde_json::json!({"@type":"MusicEvent","name":"Next week","startDate":"2027-05-10T19:30:00","performer":[{"@type":"MusicGroup","name":"Radiohead","sameAs":"https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"}]});
            v.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            v
        };
        let next_week = node(serde_json::json!({"url":"/next-week","@id":"#next-week"}));
        assert!(!owns_page(&next_week, &base));
        let mut doc = ResourceMeta::new(base.as_str(), "web", 3600);
        jsonld(&mut doc, &next_week, &base);
        assert!(doc.event.performers.is_empty());
        // The page's own: its url names it, tracking aside, whatever its fragment `@id`; a
        // fragment `@id` alone names the page too.
        for own in [
            serde_json::json!({"url":"/tonight?utm_source=feed","@id":"#event"}),
            serde_json::json!({"@id":"https://venue.example/tonight#event"}),
            serde_json::json!({"@id":"#event"}),
        ] {
            assert!(owns_page(&node(own.clone()), &base), "{own}");
        }
    }
    /// A performer's links come in the order that keys it: a page MusicBrainz relates, then its
    /// MusicBrainz page (a fact), then the rest; trimming a big line-up drops from the end, so
    /// the link MusicBrainz is asked about is the last to go.
    #[test]
    fn a_performers_keying_link_comes_first_and_survives_trimming() {
        let base = Url::parse("https://tickets.example/show").unwrap();
        let node = serde_json::json!({"@type":"MusicEvent","url":"https://tickets.example/show","performer":[{"name":"Radiohead","url":"https://tickets.example/artists/radiohead","sameAs":["https://www.instagram.com/radiohead","https://www.songkick.com/artists/253846-radiohead","https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"]}]});
        let found = performers(&node, &base);
        assert_eq!(
            found[0].links,
            [
                "https://www.songkick.com/artists/253846-radiohead",
                "https://musicbrainz.org/artist/a74b1b7f-71a5-4011-9441-d0b5e4122711",
                "https://tickets.example/artists/radiohead",
                "https://www.instagram.com/radiohead",
            ]
        );
        let mut fields = super::super::EventFields {
            performers: found,
            ..Default::default()
        };
        while fields.performers[0].links.len() > 1 {
            assert!(fields.shrink_performers());
        }
        assert_eq!(
            fields.performers[0].links,
            ["https://www.songkick.com/artists/253846-radiohead"]
        );
    }

    /// A hostile line-up costs little: the parse looks at a bounded part of it and keeps one
    /// performer and one link more than an event stores, the keying link among them.
    #[test]
    fn a_hostile_line_up_is_read_in_bounds() {
        let base = Url::parse("https://tickets.example/show").unwrap();
        let mut same_as: Vec<String> = (0..1000)
            .map(|n| format!("https://tickets.example/fans/{n}"))
            .collect();
        same_as.insert(
            40,
            "https://www.songkick.com/artists/253846-radiohead".into(),
        );
        let performer = serde_json::json!({"name":"Radiohead","sameAs":same_as});
        let node = serde_json::json!({"@type":"MusicEvent","url":"https://tickets.example/show","performer":vec![performer; 1000]});
        let found = performers(&node, &base);
        assert_eq!(found.len(), 17);
        for p in &found {
            assert_eq!(p.links.len(), 9);
            assert_eq!(
                p.links[0],
                "https://www.songkick.com/artists/253846-radiohead"
            );
        }
    }

    /// The caps count `url` first, then `sameAs`, each one value or an array: 64 distinct links,
    /// among at most 256 values. A value that is no link takes no place among the 64, so junk
    /// can't push the page's keying link out; it only uses up the 256.
    #[test]
    fn performer_link_values_are_capped_across_fields() {
        let base = Url::parse("https://tickets.example/show").unwrap();
        let radiohead = "https://www.songkick.com/artists/253846-radiohead";
        let filler = |n: usize| -> Vec<String> {
            (0..n)
                .map(|i| format!("https://tickets.example/fans/{i}"))
                .collect()
        };
        // 63 `url` values leave room for one `sameAs` value; 64 leave none.
        for (urls, read) in [(63, true), (64, false)] {
            let node = serde_json::json!({"performer":{"name":"Radiohead","url":filler(urls),"sameAs":[radiohead,"https://tickets.example/more"]}});
            let links = &performers(&node, &base)[0].links;
            assert_eq!(links.len(), 9, "{urls}");
            assert_eq!(links[0] == radiohead, read, "{urls}: {links:?}");
        }
        let node = serde_json::json!({"performer":{"name":"Radiohead","url":"https://tickets.example/radiohead","sameAs":radiohead}});
        assert_eq!(
            performers(&node, &base)[0].links,
            [radiohead, "https://tickets.example/radiohead"]
        );
        // 224 values before it, of one distinct link: nulls, nested arrays (never read inside),
        // numbers, other schemes and one link many times. Its Songkick page is still read.
        let mut junk = vec![serde_json::Value::Null; 64];
        junk.extend(vec![
            serde_json::json!(["https://tickets.example/nested"]);
            32
        ]);
        junk.extend(vec![serde_json::json!(7); 32]);
        junk.extend(vec![serde_json::json!("ftp://tickets.example/x"); 16]);
        junk.extend(vec![serde_json::json!("mailto:fans@tickets.example"); 16]);
        junk.extend(vec![serde_json::json!("https://tickets.example/again"); 64]);
        let node =
            serde_json::json!({"performer":{"name":"Radiohead","url":junk,"sameAs":radiohead}});
        assert_eq!(
            performers(&node, &base)[0].links,
            [radiohead, "https://tickets.example/again"]
        );
        // The scan stops after 256 values, links or not.
        let node = serde_json::json!({"performer":{"name":"Radiohead","url":vec![serde_json::Value::Null; 256],"sameAs":radiohead}});
        assert!(performers(&node, &base)[0].links.is_empty());
    }

    /// A merge never pairs one document's performers with another's provenance: performers keep
    /// their own (or none), and ones taken from the fallback bring the fallback's.
    #[test]
    fn performers_keep_their_own_provenance_in_a_merge() {
        let performer = super::super::Performer {
            name: "Radiohead".into(),
            links: vec!["https://www.songkick.com/artists/253846".into()],
        };
        let mut fallback = ResourceMeta::new("https://www.songkick.com/concerts/1", "web", 60);
        fallback.event.performers = vec![performer.clone()];
        fallback.event.performers_from = Some("https://www.songkick.com/concerts/1".into());
        let mut own = ResourceMeta::new("https://www.songkick.com/concerts/1", "web", 60);
        own.event.performers = vec![performer];
        merge_missing(&mut own, &fallback).unwrap();
        assert_eq!(own.event.performers_from(), None);
        let mut none = ResourceMeta::new("https://www.songkick.com/concerts/1", "web", 60);
        merge_missing(&mut none, &fallback).unwrap();
        assert_eq!(none.event.performers.len(), 1);
        assert_eq!(
            none.event.performers_from(),
            Some("https://www.songkick.com/concerts/1")
        );
    }

    /// Performers read on a page whose fetch ended on a link carrying a credential keep only its
    /// origin as their provenance: the host is all that is read from it.
    #[test]
    fn performers_read_past_a_link_that_carries_a_credential_keep_its_origin() {
        let mut card = ResourceMeta::new("https://www.songkick.com/concerts/1", "web", 60);
        card.event.performers = vec![super::super::Performer {
            name: "Radiohead".into(),
            links: vec!["https://www.songkick.com/artists/253846".into()],
        }];
        card.event
            .read_from(&crate::resolver::egress::Fetched::answer(
                url::Url::parse("https://www.songkick.com/concerts/1?token=SECRET").unwrap(),
                200,
                "text/html".into(),
                vec![],
            ));
        assert_eq!(
            card.event.performers_from(),
            Some("https://www.songkick.com/")
        );
    }

    /// The provenance is stored as `performers_fetched_from`; a card stored with another key for
    /// it (an address it was asked for, not one its fetch ended on) has none when read back.
    #[test]
    fn only_the_stored_provenance_of_a_fetch_is_read_back() {
        let mut card = ResourceMeta::new("https://www.songkick.com/concerts/11-stored", "web", 60);
        card.event.performers = vec![super::super::Performer {
            name: "Radiohead".into(),
            links: vec!["https://www.songkick.com/artists/253846".into()],
        }];
        let mut stored = serde_json::to_value(&card).unwrap();
        stored["performers_from"] = "https://www.songkick.com/concerts/11-stored".into();
        let doc: ResourceMeta = serde_json::from_value(stored).unwrap();
        assert_eq!(doc.event.performers.len(), 1);
        assert_eq!(doc.event.performers_from(), None);
        let mut doc = doc;
        doc.event.performers_from = Some("https://www.songkick.com/concerts/11-stored".into());
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            json["performers_fetched_from"],
            "https://www.songkick.com/concerts/11-stored"
        );
        assert!(json.get("performers_from").is_none());
        let back: ResourceMeta = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.event.performers_from(),
            Some("https://www.songkick.com/concerts/11-stored")
        );
    }

    #[test]
    fn a_page_listing_several_events_yields_none_but_its_own() {
        let base = Url::parse("https://gigs.example/artists/radiohead").unwrap();
        let concert = |n: u32, url: &str| serde_json::json!({"@type":"MusicEvent","name":format!("Radiohead @ Arena {n}"),"url":url,"startDate":"2027-05-01T19:30:00"});
        let listed = |n: u32| concert(n, &format!("https://gigs.example/concerts/{n}"));
        let read = |values: &[&Value]| {
            let mut doc = ResourceMeta::new(base.as_str(), "web", 100);
            jsonld_all(&mut doc, values, &base, &[]);
            doc
        };
        // An artist's concerts, none the page: a list, no event.
        let doc = read(&[&listed(1), &listed(2)]);
        assert!(!doc.type_path.contains(&"Event".into()), "{doc:?}");
        assert!(doc.start.is_none());
        // One event in two scripts (a theme's and a plugin's): the event.
        let doc = read(&[&listed(1), &listed(1)]);
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 1"));
        assert!(doc.type_path.contains(&"Event".into()));
        // The page's own among those it lists: its own.
        let doc = read(&[&listed(1), &concert(9, base.as_str()), &listed(2)]);
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 9"));
        // One listed event naming another page, as before: the event.
        let doc = read(&[&listed(1)]);
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 1"));
        // Two scripts describing one event (its url), the first naming it, the second dating
        // and describing it: the event has all three, as reading each in turn once gave.
        let named = serde_json::json!({"@type":"MusicEvent","name":"Radiohead @ Arena 1","url":"https://gigs.example/concerts/1"});
        let dated = serde_json::json!({"@type":"MusicEvent","url":"https://gigs.example/concerts/1","startDate":"2027-05-01T19:30:00","description":"Doors at seven."});
        let doc = read(&[&named, &dated]);
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 1"));
        assert!(doc.start.is_some(), "{doc:?}");
        assert_eq!(doc.description.as_deref(), Some("Doors at seven."));
        // The same with an `@id` on one of them (`#event`) and none on the other, and the url
        // spelt with a tracking parameter on one: one event still.
        let with_id = serde_json::json!({"@type":"MusicEvent","@id":"#event","name":"Radiohead @ Arena 1","url":"https://gigs.example/concerts/1?utm_source=share"});
        let doc = read(&[&with_id, &dated]);
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 1"));
        assert!(doc.start.is_some(), "{doc:?}");
        assert_eq!(doc.description.as_deref(), Some("Doors at seven."));
        let doc = read(&[&dated, &with_id]);
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 1"));
        assert!(doc.start.is_some());
        // The event under the page's canonical address: its own, not a listing, with another
        // event beside it.
        let amp = Url::parse("https://gigs.example/concerts/1/amp").unwrap();
        let other = serde_json::json!({"@type":"MusicEvent","name":"Other night","url":"https://gigs.example/concerts/2","startDate":"2027-05-02T19:30:00"});
        let mut doc = ResourceMeta::new(amp.as_str(), "web", 100);
        let pages = vec![
            page_identity(amp.as_str()).unwrap(),
            page_identity("https://gigs.example/concerts/1").unwrap(),
        ];
        jsonld_all(&mut doc, &[&named, &other], &amp, &pages);
        assert_eq!(doc.name.as_deref(), Some("Radiohead @ Arena 1"));
        assert!(doc.type_path.contains(&"Event".into()));
        // Events naming no page are the page's own (a theme's and a plugin's, or two
        // listings of one night): the first, as before; several as a page's main entity list
        // are a list.
        let unnamed = |n: u32| serde_json::json!({"@type":"MusicEvent","name":format!("Night {n}"),"startDate":"2027-05-01T19:30:00"});
        let doc = read(&[&unnamed(1), &unnamed(2)]);
        assert_eq!(doc.name.as_deref(), Some("Night 1"));
        let page = serde_json::json!({"@type":"WebPage","mainEntity":[unnamed(1), unnamed(2)]});
        let doc = read(&[&page]);
        assert!(!doc.type_path.contains(&"Event".into()), "{doc:?}");
    }
}
