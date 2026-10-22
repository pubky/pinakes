//! The Pubky marketplace (the shop at pubky.app/shop): a seller's shop, listings and drops, under
//! pubky.app's own namespace. Its media files, reviews, encrypted deliverables, keys and proofs
//! are never fetched.
use super::{read, Client, Kind, Object, Profile, Read};
use crate::model::ResourceMeta;
use serde_json::Value;

pub static PROFILE: Profile = Profile {
    namespaces: &["pubky.app/marketplace/v1"],
    kinds: &[
        Kind {
            path: "shop.json",
            read: shop,
            open_in: &[Client {
                client: "Pubky Shop",
                url: "https://pubky.app/shop/marketplace/shop/{owner}",
            }],
        },
        Kind {
            path: "listings/{listing}",
            read: listing,
            open_in: &[Client {
                client: "Pubky Shop",
                url: "https://pubky.app/shop/marketplace/listing/{owner}/{listing}",
            }],
        },
        Kind {
            path: "drops/{drop}",
            read: drop,
            open_in: &[Client {
                client: "Pubky Shop",
                url: "https://pubky.app/shop/marketplace/drop/{owner}/{drop}",
            }],
        },
    ],
    facts: &["price", "condition", "category", "country"],
};

/// A record of the marketplace's own kind, written by the path's owner.
fn record(object: &Object, v: &Value, kind: &str) -> bool {
    v.get("recordType").and_then(Value::as_str) == Some(kind)
        && v.get("ownerPubky").and_then(Value::as_str) == Some(object.owner)
}
fn country(v: &Value, doc: &mut ResourceMeta) {
    if let Some(code) = read::text(&v["location"], &["countryCode"])
        .filter(|c| c.len() == 2 && c.chars().all(|c| c.is_ascii_alphabetic()))
    {
        doc.attributes.insert("country".into(), code.to_uppercase());
    }
}

/// A shop: its name, bio and avatar.
fn shop(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(name) = read::text(v, &["name"]).filter(|_| record(object, v, "shop")) else {
        return Read::NoCard;
    };
    doc.set_type("OnlineStore");
    doc.name = Some(name);
    doc.description = read::text(v, &["bio"]);
    doc.image = read::text(v, &["avatarUrl"]).and_then(|l| object.link(&l));
    country(v, doc);
    Read::Card
}

/// A listing on sale: its title, description, first image and price. A paused, ended or removed
/// listing has no card, and neither has an auction whose end has passed; an adult-only listing
/// shows no image.
fn listing(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let sale = &v["sale"];
    let auction_end = sale
        .get("endsAt")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        .filter(|_| sale.get("format").and_then(Value::as_str) == Some("auction"));
    let ended = auction_end.is_some_and(|end| end <= object.now);
    if !record(object, v, "listing")
        || v.get("listingId").and_then(Value::as_str) != Some(object.var("listing"))
        || v.get("state").and_then(Value::as_str) != Some("active")
        || ended
    {
        return Read::NoCard;
    }
    let Some(title) = read::text(v, &["title"]) else {
        return Read::NoCard;
    };
    doc.set_type("Product");
    doc.name = Some(title);
    doc.description = read::text(v, &["description"]);
    if v.get("adultOnly").and_then(Value::as_bool) != Some(true) {
        doc.image = v["media"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m.get("type").and_then(Value::as_str) == Some("image"))
            .and_then(|m| read::text(m, &["url"]))
            .and_then(|l| object.link(&l));
    }
    // The price: a fixed one, or an auction's starting one.
    if let Some(price) = sale
        .get("unitPrice")
        .or_else(|| sale.get("startingPrice"))
        .and_then(read::money)
    {
        doc.attributes.insert("price".into(), price);
    }
    for (key, field) in [("condition", "condition"), ("category", "categoryId")] {
        if let Some(value) = read::text(v, &[field]) {
            doc.attributes.insert(key.into(), value);
        }
    }
    country(v, doc);
    // Its card goes when the auction ends: it is fetched again then.
    doc.refresh_by = auction_end;
    Read::Card
}

/// A drop (a release of some of the seller's listings at a time): its title, description, first
/// image and times.
fn drop(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    if !record(object, v, "drop")
        || v.get("dropId").and_then(Value::as_str) != Some(object.var("drop"))
    {
        return Read::NoCard;
    }
    let Some(title) = read::text(v, &["title"]) else {
        return Read::NoCard;
    };
    let time = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    doc.set_type("SaleEvent");
    doc.name = Some(title);
    doc.description = read::text(v, &["description"]);
    doc.image = v["media"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .and_then(|l| object.link(l));
    doc.start = time("startsAt");
    doc.end = time("endsAt").filter(|end| doc.start.is_none_or(|start| *end > start));
    Read::Card
}
