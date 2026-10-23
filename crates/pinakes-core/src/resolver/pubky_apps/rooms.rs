//! Pubky Rooms (chat rooms): a listed room. Members, messages, reactions, bans, mutes, tags and
//! the nickname file are never fetched.
use super::{read, Kind, Object, Profile, Read};
use crate::model::ResourceMeta;
use serde_json::Value;

pub static PROFILE: Profile = Profile {
    namespaces: &["pubky-rooms"],
    kinds: &[Kind {
        path: "rooms/{room}",
        read: room,
        // No client link until rooms.pubky.app is live: the client's address today is a hosting
        // provider's subdomain, which anyone could take over once the app leaves it.
        open_in: &[],
    }],
    facts: &[],
};

/// A room: its name and topic. Only a listed (`public`) room has a card; an unlisted one is left
/// out of discovery by its creator.
fn room(_: &Object, v: &Value, doc: &mut ResourceMeta) -> Read {
    if v.get("v").and_then(Value::as_u64) != Some(1)
        || v.get("visibility").and_then(Value::as_str) != Some("public")
    {
        return Read::NoCard;
    }
    let Some(name) = read::text(v, &["name"]) else {
        return Read::NoCard;
    };
    doc.set_type("Conversation");
    doc.name = Some(name);
    doc.description = read::text(v, &["topic"]);
    Read::Card
}
