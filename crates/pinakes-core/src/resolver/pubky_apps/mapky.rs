//! Mapky (maps, mapky.app): routes, geotagged captures, capture sequences, incident reports,
//! place reviews, posts and the older place collections. Its file records, blobs and tags are
//! never fetched. Reviews and posts are about OpenStreetMap places: their client link is the
//! place's page.
use super::{read, Client, Kind, Object, Profile, Read};
use crate::model::{LocationSource, OpenIn, ResourceMeta};
use serde_json::Value;

pub static PROFILE: Profile = Profile {
    namespaces: &["mapky.app"],
    kinds: &[
        Kind {
            path: "routes/{id}",
            read: route,
            open_in: &[Client {
                client: "Mapky",
                url: "https://mapky.app/route/{owner}/{id}",
            }],
        },
        Kind {
            path: "geo_captures/{id}",
            read: capture,
            open_in: &[Client {
                client: "Mapky",
                url: "https://mapky.app/capture/{owner}/{id}",
            }],
        },
        Kind {
            path: "sequences/{id}",
            read: sequence,
            open_in: &[Client {
                client: "Mapky",
                url: "https://mapky.app/sequence/{owner}/{id}",
            }],
        },
        Kind {
            path: "incidents/{id}",
            read: incident,
            open_in: &[Client {
                client: "Mapky",
                url: "https://mapky.app/incident/{owner}/{id}",
            }],
        },
        Kind {
            path: "reviews/{id}",
            read: review,
            open_in: &[],
        },
        Kind {
            path: "posts/{id}",
            read: post,
            open_in: &[],
        },
        Kind {
            path: "collections/{id}",
            read: collection,
            open_in: &[],
        },
    ],
    facts: &[
        "activity",
        "distance_m",
        "elevation_gain_m",
        "duration_s",
        "capture_kind",
        "incident_type",
        "severity",
        "rating",
        "place",
        "items",
    ],
};

/// The Mapky page of an OpenStreetMap place.
fn place_page(osm_type: &str, osm_id: u64) -> OpenIn {
    OpenIn {
        client: "Mapky".into(),
        url: format!("https://mapky.app/place/{osm_type}/{osm_id}"),
    }
}
/// The place a review or post is about: an OpenStreetMap URL, or (in older posts) `{osm_type,
/// osm_id}`. Recorded as a fact (`place`, the URL), with its Mapky page as the client link.
fn about(place: &Value, doc: &mut ResourceMeta) {
    let element = match place {
        Value::String(url) => read::osm_element(url),
        Value::Object(_) => {
            let kind = place.get("osm_type").and_then(Value::as_str).unwrap_or("");
            let id = place.get("osm_id").and_then(Value::as_u64).unwrap_or(0);
            read::osm_element(&format!("https://www.openstreetmap.org/{kind}/{id}"))
        }
        _ => None,
    };
    if let Some((kind, id)) = element {
        doc.attributes.insert(
            "place".into(),
            format!("https://www.openstreetmap.org/{kind}/{id}"),
        );
        doc.open_in.push(place_page(kind, id));
    }
}
fn located(doc: &mut ResourceMeta, point: Option<crate::model::Point>) {
    if let Some(point) = point {
        doc.location = Some(point);
        doc.location_source = Some(LocationSource::Page);
    }
}
fn fact(doc: &mut ResourceMeta, key: &str, value: Option<String>) {
    if let Some(value) = value {
        doc.attributes.insert(key.into(), value);
    }
}

/// A route: its name, description and image, its start as the point, and its activity,
/// distance, climb and duration.
fn route(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(name) = read::text(v, &["name"]) else {
        return Read::NoCard;
    };
    doc.set_type("Trip");
    doc.name = Some(name);
    doc.description = read::text(v, &["description"]);
    doc.image = read::text(v, &["image_uri"]).and_then(|l| object.link(&l));
    located(doc, v["waypoints"].get(0).and_then(read::point));
    fact(doc, "activity", read::text(v, &["activity"]));
    fact(
        doc,
        "distance_m",
        v.get("distance_m").and_then(read::number),
    );
    fact(
        doc,
        "elevation_gain_m",
        v.get("elevation_gain_m").and_then(read::number),
    );
    fact(
        doc,
        "duration_s",
        v.get("estimated_duration_s").and_then(read::number),
    );
    Read::Card
}

/// A geotagged capture: its caption and point. A photo is a Photograph, other media their own
/// type; the media file itself is another object and is not read.
fn capture(_: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(point) = read::point(v) else {
        return Read::NoCard;
    };
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("photo");
    doc.set_type(match kind {
        "photo" | "panorama" => "Photograph",
        "video" | "video360" => "VideoObject",
        "audio" => "AudioObject",
        "model3d" => "3DModel",
        _ => "MediaObject",
    });
    doc.description = read::text(v, &["caption"]);
    located(doc, Some(point));
    fact(doc, "capture_kind", Some(kind.to_owned()));
    Read::Card
}

/// A sequence of captures: its name and description.
fn sequence(_: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    doc.set_type("ImageGallery");
    doc.name = read::text(v, &["name"]);
    doc.description = read::text(v, &["description"]);
    if doc.name.is_none() && doc.description.is_none() {
        return Read::NoCard;
    }
    Read::Card
}

/// An incident report: its type, severity, description and point. An expired report has no
/// card: it described the road as it was.
fn incident(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let expires = v.get("expires_at").and_then(read::micros);
    let expired = expires.is_some_and(|t| t <= object.now);
    let (Some(point), Some(kind)) = (read::point(v), read::text(v, &["incident_type"])) else {
        return Read::NoCard;
    };
    if expired {
        return Read::NoCard;
    }
    doc.set_type("SpecialAnnouncement");
    doc.description = read::text(v, &["description"]);
    located(doc, Some(point));
    fact(doc, "incident_type", Some(kind));
    fact(doc, "severity", read::text(v, &["severity"]));
    // Its card goes when it expires: it is fetched again then.
    doc.refresh_by = expires;
    Read::Card
}

/// A place review: its text and rating (1 to 10), about the place it names.
fn review(_: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(rating) = v
        .get("rating")
        .and_then(Value::as_u64)
        .filter(|r| (1..=10).contains(r))
    else {
        return Read::NoCard;
    };
    doc.set_type("Review");
    doc.description = read::text(v, &["content"]);
    fact(doc, "rating", Some(rating.to_string()));
    about(&v["place"], doc);
    Read::Card
}

/// A post: its text, as social content, with the place it is about. A post that is a collection
/// (`kind: collection`) is read as one; an older post that names a `place` may be a review.
fn post(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    if v.get("kind").and_then(Value::as_str) == Some("collection") {
        let content = v
            .get("content")
            .and_then(Value::as_str)
            .and_then(|c| serde_json::from_str::<Value>(c).ok());
        return match content.filter(Value::is_object) {
            Some(content) => collection(object, &content, doc),
            None => Read::NoCard,
        };
    }
    if v.get("place").is_some() {
        if v.get("kind").and_then(Value::as_str) == Some("review") {
            return review(object, v, doc);
        }
        about(&v["place"], doc);
    } else if let Some(parent) = v.get("parent").and_then(Value::as_str) {
        if read::osm_element(parent).is_some() {
            about(&Value::String(parent.into()), doc);
        } else if let Some(link) = resource_page(parent) {
            doc.open_in.push(link);
        }
    }
    // Social content (`cards::social`): a short excerpt, purged with its history once the post
    // is gone.
    let Some(content) = read::text(v, &["content"]) else {
        return Read::NoCard;
    };
    doc.set_type("SocialMediaPosting");
    doc.description = Some(excerpt(content));
    Read::Card
}
fn excerpt(text: String) -> String {
    text.chars().take(200).collect()
}
/// The Mapky page of a post's parent: a route, capture, sequence or incident.
fn resource_page(parent: &str) -> Option<OpenIn> {
    let url = url::Url::parse(parent).ok()?;
    if url.scheme() != "pubky" {
        return None;
    }
    let owner = url.host_str()?;
    let segments = crate::resolver::path_segments(&url)?;
    let [public, app, kind, id] = segments.as_slice() else {
        return None;
    };
    let page = match kind.as_str() {
        "routes" => "route",
        "geo_captures" => "capture",
        "sequences" => "sequence",
        "incidents" => "incident",
        _ => return None,
    };
    if public != "pub" || app != "mapky.app" || id.is_empty() {
        return None;
    }
    Some(OpenIn {
        client: "Mapky".into(),
        url: format!("https://mapky.app/{page}/{owner}/{}", super::encode(id)),
    })
}

/// A collection of places: its name, description, image and number of places.
fn collection(object: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    let Some(name) = read::text(v, &["name"]) else {
        return Read::NoCard;
    };
    doc.set_type("Collection");
    doc.name = Some(name);
    doc.description = read::text(v, &["description"]);
    doc.image = read::text(v, &["image_uri", "cover_image"]).and_then(|l| object.link(&l));
    if let Some(items) = v.get("items").and_then(Value::as_array) {
        fact(doc, "items", Some(items.len().to_string()));
    }
    Read::Card
}
