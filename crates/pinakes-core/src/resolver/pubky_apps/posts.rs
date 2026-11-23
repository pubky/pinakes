//! App posts: the kinds apps write as posts on the social post envelope under their own
//! namespace, `/pub/<namespace>/v1/posts/{id}/{editId}.json`. A post comes on the resource feed
//! as a `post` line naming its versionless URI; the resolver lists its versions and reads the
//! latest (`Kind::versions`), checks it with pubky-social-specs' `PostEnvelope<K>`, and reads it
//! by its namespace and kind. The social specs' own posts (`/pub/social/…`: events, rsvps and the
//! rest) are Nexus's and never fetched, availability included, now
//! a social kind.
//!
//! - `maps` `route`: a `Trip` card, its box from its coordinate waypoints.
//! - `music` `session`: a `BroadcastEvent` card of the track it embeds, from its start to its
//!   expiry; `music` `listening` (now playing): a `ListenAction` card of the track until its
//!   expiry. Each is named by its optional `name`, else generically ("Listening session", "Now
//!   playing"), with its author as a fact, as clients name them. A card
//!   with an expiry is read again when it passes (`refresh_by`), and
//!   then has none. A `listen`
//!   has no card: it counts on its track, and one card per play would be noise and a
//!   play-by-play history in a second index.
//! - Any other namespace (`ANY`): the general card from the content's standard fields, with the
//!   namespace and kind.
use super::{Client, Kind, Object, Profile, Read};
use crate::model::ResourceMeta;
use chrono::{DateTime, Utc};
use pubky_social_specs::{
    traits::Validatable, AllowedSchemes, ContentReference, PostEnvelope, PostKind, ValidationCtx,
    PUB_CTX, VALIDATION_LIMITS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The posts of a namespace: one path, a folder of versions.
const fn posts(read: fn(&Object, &Value, &mut ResourceMeta) -> Read) -> [Kind; 1] {
    [Kind {
        path: "v1/posts/{id}",
        read,
        open_in: &[] as &[Client],
        versions: true,
    }]
}
const MAPS_KINDS: [Kind; 1] = posts(maps);
const MUSIC_KINDS: [Kind; 1] = posts(music);
const ANY_KINDS: [Kind; 1] = [Kind {
    path: "{namespace}/v1/posts/{id}",
    read: any,
    open_in: &[] as &[Client],
    versions: true,
}];
/// The facts the post cards publish.
const FACTS: &[&str] = &[
    "namespace",
    "kind",
    "activity",
    "distance_m",
    "duration_s",
    "elevation_gain_m",
    "elevation_loss_m",
    "waypoints",
    "track",
    "host",
    "service",
    "author",
];
pub static MAPS: Profile = Profile {
    namespaces: &["maps"],
    kinds: &MAPS_KINDS,
    facts: FACTS,
    links_off: None,
};
pub static MUSIC: Profile = Profile {
    namespaces: &["music"],
    kinds: &MUSIC_KINDS,
    facts: FACTS,
    links_off: None,
};
/// The posts of every namespace no profile reads: not in the registry, the route of last resort
/// for a post (`super::route`), its path from below `/pub/`.
pub static ANY: Profile = Profile {
    namespaces: &[],
    kinds: &ANY_KINDS,
    facts: FACTS,
    links_off: None,
};

/// Any kind token, for the envelope's checks: the kind's content is the profile's to read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppKind(pub String);
impl PostKind for AppKind {
    fn is_known(&self) -> bool {
        (1..=32).contains(&self.0.len())
            && self
                .0
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    }
    fn validate_content(&self, _: &PostEnvelope<Self>, _: &ValidationCtx) -> Result<(), String> {
        Ok(())
    }
    /// The references inside the content the profiles read (a route's waypoints, a session's
    /// host), so the envelope's gate refuses a private or non-canonical one as it does the
    /// envelope's own.
    fn content_references(&self, post: &PostEnvelope<Self>) -> Vec<ContentReference> {
        let Ok(content) = serde_json::from_str::<Value>(&post.content) else {
            return vec![];
        };
        let reference = |field: String, uri: &Value| {
            uri.as_str().map(|uri| ContentReference {
                field,
                uri: uri.to_owned(),
                schemes: AllowedSchemes::Universal,
                max_code_points: VALIDATION_LIMITS.reference_uri_max_length,
            })
        };
        let list = |key: &str| {
            content[key]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(i, item)| reference(format!("{key}[{i}].uri"), &item["uri"]))
                .collect::<Vec<_>>()
        };
        match self.0.as_str() {
            "route" => list("waypoints"),
            "session" => reference("host".into(), &content["host"])
                .into_iter()
                .collect(),
            _ => vec![],
        }
    }
}

/// The post's envelope, checked as pubky-social-specs checks it (its id the path's).
fn checked(object: &Object, v: &Value) -> Option<PostEnvelope<AppKind>> {
    let bytes = serde_json::to_vec(v).ok()?;
    <PostEnvelope<AppKind> as Validatable>::try_from(&bytes, object.var("id"), &PUB_CTX).ok()
}
/// The post's checked envelope and its content as JSON.
fn envelope(object: &Object, v: &Value) -> Option<(PostEnvelope<AppKind>, Value)> {
    let envelope = checked(object, v)?;
    let content = serde_json::from_str(&envelope.content).ok()?;
    Some((envelope, content))
}
fn fact(doc: &mut ResourceMeta, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        doc.attributes.insert(key.into(), value);
    }
}
/// A text member, trimmed, within `max` code points; none with a NUL, which no stored text
/// may hold.
fn text(v: &Value, key: &str, max: usize) -> Option<String> {
    v[key]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.chars().count() <= max && !t.contains('\0'))
        .map(str::to_owned)
}

fn maps(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    match envelope(object, v) {
        Some((e, content)) if e.kind.0 == "route" => route(&content, doc),
        _ => Read::NoCard,
    }
}
fn music(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    match envelope(object, v) {
        Some((e, content)) if e.kind.0 == "session" => session(object, &e, &content, doc),
        Some((e, content)) if e.kind.0 == "listening" => listening(object, &e, &content, doc),
        _ => Read::NoCard,
    }
}
/// A post of a namespace no profile reads: the general card (`super::general`) of its content,
/// a JSON object's standard fields or a text's excerpt, with its namespace and kind. A namespace
/// no stored text may hold (decoded from the path: a control character, a NUL among them) or
/// over 100 characters gives no card.
fn any(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let namespace = object.var("namespace");
    if namespace.chars().count() > 100 || namespace.chars().any(char::is_control) {
        return Read::NoCard;
    }
    let Some(envelope) = checked(object, v) else {
        return Read::NoCard;
    };
    let content = serde_json::from_str::<Value>(&envelope.content)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "content": envelope.content }));
    super::general(object.uri, &content, doc);
    fact(doc, "namespace", Some(namespace.into()));
    fact(doc, "kind", Some(envelope.kind.0));
    Read::Card
}

/// A point from a `geo:` URI.
fn geo(uri: &str) -> Option<crate::model::Point> {
    let (lat, lon) = uri.strip_prefix("geo:")?.split_once(',')?;
    let (lat, lon) = (lat.parse::<f64>().ok()?, lon.parse::<f64>().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then_some(crate::model::Point { lat, lon })
}
/// A route: its name, description, first waypoint, the box of its coordinate waypoints, and its
/// measures.
fn route(content: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(name) = text(content, "name", 100) else {
        return Read::NoCard;
    };
    let Some(waypoints) = content["waypoints"].as_array().and_then(|w| {
        w.iter()
            .map(|w| w["uri"].as_str().map(str::to_owned))
            .collect::<Option<Vec<String>>>()
    }) else {
        return Read::NoCard;
    };
    if !(2..=100).contains(&waypoints.len()) {
        return Read::NoCard;
    }
    doc.set_type("Trip");
    doc.name = Some(name);
    doc.description = text(content, "description", 2000);
    doc.location = waypoints.first().and_then(|w| geo(w));
    let points: Vec<_> = waypoints.iter().filter_map(|w| geo(w)).collect();
    if !points.is_empty() {
        let (mut w, mut s, mut e, mut n) = (180.0f64, 90.0f64, -180.0f64, -90.0f64);
        for p in &points {
            (w, s, e, n) = (w.min(p.lon), s.min(p.lat), e.max(p.lon), n.max(p.lat));
        }
        doc.bbox = Some([w, s, e, n]);
    }
    fact(doc, "activity", text(content, "activity", 20));
    for key in [
        "distance_m",
        "duration_s",
        "elevation_gain_m",
        "elevation_loss_m",
    ] {
        fact(doc, key, content[key].as_u64().map(|n| n.to_string()));
    }
    fact(doc, "waypoints", Some(waypoints.len().to_string()));
    fact(doc, "kind", Some("route".into()));
    Read::Card
}
/// An instant in microseconds since the epoch, as the music specs write `started_at` and
/// `expires_at`.
fn instant_us(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_micros(v.as_i64()?)
}
/// Whether a card ending at `end` is still current at the read; if so, it is read again when it
/// ends (`refresh_by`, which the queue keeps over its usual TTL, at least a minute away), and
/// then has none.
fn current(object: &Object, end: DateTime<Utc>, doc: &mut ResourceMeta) -> bool {
    if end <= object.now {
        return false;
    }
    doc.refresh_by = Some(end);
    true
}
/// A music card's title: its optional `name`, else `generic`, as clients name it; never empty.
/// Its author (the owner's key) is a fact, so a card shows whose session it is either way.
fn named(object: &Object, content: &Value, generic: &str, doc: &mut ResourceMeta) {
    doc.name = Some(text(content, "name", 100).unwrap_or_else(|| generic.into()));
    fact(doc, "author", Some(object.owner.into()));
}
/// The longest a `listening` is current: "now playing" for longer is no longer now (the music
/// specs cap a listening at a day).
const LISTENING_MAX: chrono::TimeDelta = chrono::TimeDelta::days(1);
/// A session: a broadcast of the track it embeds, from `started_at` until `expires_at`, with its
/// host and the service it plays on as facts. Named by its optional `name`, else "Listening
/// session" (`named`). Once `expires_at` passes it has no card.
fn session(
    object: &Object,
    envelope: &PostEnvelope<AppKind>,
    content: &Value,
    doc: &mut ResourceMeta,
) -> Read {
    let (Some(track), Some(start)) = (
        envelope.embed.as_deref(),
        instant_us(&content["started_at"]),
    ) else {
        return Read::NoCard;
    };
    let end = match &content["expires_at"] {
        Value::Null => None,
        expires => match instant_us(expires) {
            Some(end) if end >= start && current(object, end, doc) => Some(end),
            _ => return Read::NoCard,
        },
    };
    doc.set_type("BroadcastEvent");
    named(object, content, "Listening session", doc);
    doc.start = Some(start);
    doc.end = end;
    fact(doc, "track", Some(track.into()));
    fact(doc, "host", text(content, "host", 1024));
    fact(doc, "service", text(&content["source"], "service", 100));
    fact(doc, "kind", Some("session".into()));
    Read::Card
}
/// A listening (now playing): the track it embeds, from `started_at` until `expires_at`, with the
/// service it plays on, named by its optional `name`, else "Now playing" (`named`). It is current
/// from `started_at` (a minute's grace for the writer's clock) until `expires_at`, at most
/// `LISTENING_MAX` after its start: before and after, it has no card.
fn listening(
    object: &Object,
    envelope: &PostEnvelope<AppKind>,
    content: &Value,
    doc: &mut ResourceMeta,
) -> Read {
    let (Some(track), Some(start), Some(end)) = (
        envelope.embed.as_deref(),
        instant_us(&content["started_at"]),
        instant_us(&content["expires_at"]),
    ) else {
        return Read::NoCard;
    };
    if end < start
        || end - start > LISTENING_MAX
        || start > object.now + chrono::TimeDelta::minutes(1)
        || !current(object, end, doc)
    {
        return Read::NoCard;
    }
    doc.set_type("ListenAction");
    named(object, content, "Now playing", doc);
    doc.start = Some(start);
    doc.end = Some(end);
    fact(doc, "track", Some(track.into()));
    fact(doc, "service", text(&content["source"], "service", 100));
    fact(doc, "kind", Some("listening".into()));
    Read::Card
}
