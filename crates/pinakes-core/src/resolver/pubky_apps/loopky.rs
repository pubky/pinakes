//! Loopky (flashcards, loopky.app): a published deck's manifest. Its cards, media, tags and the
//! reader's own state (`srs/`, `subscriptions/`, `settings.json`) are never fetched.
use super::{read, Client, Kind, Object, Profile, Read};
use crate::model::ResourceMeta;
use serde_json::Value;

pub static PROFILE: Profile = Profile {
    namespaces: &["loopky"],
    kinds: &[Kind {
        path: "decks/{deck}/manifest.json",
        read: deck,
        open_in: &[Client {
            client: "Loopky",
            url: "https://loopky.app/deck/?author={owner}&id={deck}",
        }],
    }],
    facts: &["card_count", "front_lang", "back_lang", "topics"],
    links_off: None,
};

/// A deck: its title, description and cover. A manifest another key claims, or one still being
/// uploaded (`incomplete`), has no card.
fn deck(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    if v.get("author_pubky").and_then(Value::as_str) != Some(object.owner)
        || v.get("incomplete").and_then(Value::as_bool) == Some(true)
    {
        return Read::NoCard;
    }
    let Some(title) = read::text(v, &["title"]) else {
        return Read::NoCard;
    };
    doc.set_type("LearningResource");
    doc.name = Some(title);
    doc.description = read::text(v, &["description"]);
    // The cover: a web image, a Pubky file, or a file in the deck's own folder.
    let cover = &v["cover_image_ref"];
    doc.image = read::text(cover, &["url"])
        .filter(|u| u.starts_with("https://"))
        .or_else(|| read::text(cover, &["uri"]).filter(|u| u.starts_with("pubky://")))
        .or_else(|| read::text(cover, &["path"]))
        .and_then(|link| object.link(&link));
    if let Some(n) = v.get("card_count").and_then(read::number) {
        doc.attributes.insert("card_count".into(), n);
    }
    for key in ["front_lang", "back_lang"] {
        if let Some(lang) = read::text(v, &[key]) {
            doc.attributes.insert(key.into(), lang);
        }
    }
    let topics: Vec<&str> = v["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .take(10)
        .collect();
    if !topics.is_empty() {
        doc.attributes.insert("topics".into(), topics.join("; "));
    }
    Read::Card
}
