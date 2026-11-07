//! Aliases: the same object under another URI, resolved deterministically to one resource.
//!
//! An alias keeps its own resource ID and references (Nexus's identity is unchanged), but it is
//! never fetched: its references count for the root, and reads, lookups, changes and occurrences
//! answer with the root. Groups are a different thing (`groups`): evidence that two objects are
//! one, never a spelling.
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};
use url::Url;

/// Query parameters that only track how a link was shared, on every host (with `utm_*`).
const TRACKING: &[&str] = &[
    "fbclid", "gclid", "dclid", "msclkid", "igshid", "mc_cid", "mc_eid", "_gl",
];
/// Parameters that track only on these platforms; elsewhere they may carry identity or access.
const PLATFORM_TRACKING: &[(&[&str], &[&str])] = &[
    (
        &["meetup.com"],
        &["recId", "recSource", "searchId", "eventOrigin"],
    ),
    (&["luma.com", "lu.ma"], &["tk"]),
];
#[derive(Debug, PartialEq)]
pub struct Alias {
    pub root: String,
    /// `cursor`, `osm`, `identifier`, `service` (a music service's, Wikipedia's or a Wikidata
    /// item's URL forms), `tracking`, `canonical` (the page's own canonical) or `source` (an
    /// official API's answer). The last two are no rule of the URI.
    pub basis: &'static str,
}
/// The root of a URI known from the URI alone, when it differs from the URI itself.
pub fn root(uri: &str) -> Option<Alias> {
    let (_, uri, _) = crate::model::identity(uri).ok()?;
    let found = if let Some(root) = crate::event::adapters::cursor::canonical_root(&uri) {
        Some((root, "cursor"))
    } else if let Some(osm) = crate::resolver::osm::parse_id(&uri) {
        Some((osm.uri(), "osm"))
    } else if let Some(isbn) = crate::resolver::cards::isbn(&uri) {
        Some((
            format!("isbn:{}", crate::resolver::cards::isbn13(&isbn)),
            "identifier",
        ))
    } else if let Some(doi) = crate::resolver::cards::doi(&uri) {
        Some((format!("doi:{doi}"), "identifier"))
    } else if let Some(root) = crate::sources::alias_root(&uri) {
        Some((root, "identifier"))
    } else if let Some(music) = crate::music::parse(&uri) {
        Some((music.canonical, "service"))
    } else if let Some(article) = crate::wikipedia::parse(&uri) {
        Some((article.canonical, "service"))
    } else if let Some((_, item)) = crate::hubs::wikidata::item_link(&uri) {
        Some((item, "service"))
    } else if let Some((_, _, page)) = crate::hubs::musicbrainz::entity_link(&uri) {
        Some((page, "service"))
    } else if let Some((_, page)) = crate::hubs::musicbrainz::isrc_link(&uri) {
        Some((page, "service"))
    } else {
        untracked(&uri).map(|root| (root, "tracking"))
    }?;
    let (_, root, _) = crate::model::identity(&found.0).ok()?;
    (root != uri).then_some(Alias {
        root,
        basis: found.1,
    })
}
/// The URI without its tracking parameters, if it had any. Parameters are dropped from the raw
/// query string, so every kept value keeps its own encoding.
fn untracked(uri: &str) -> Option<String> {
    let mut u = Url::parse(uri).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str().unwrap_or_default().to_owned();
    let platform: Vec<&str> = PLATFORM_TRACKING
        .iter()
        .filter(|(hosts, _)| {
            hosts
                .iter()
                .any(|h| host == *h || host.ends_with(&format!(".{h}")))
        })
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    let tracking = |piece: &str| {
        let name = piece.split('=').next().unwrap_or_default();
        let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
        name.starts_with("utm_")
            || TRACKING.contains(&name.as_ref())
            || platform.contains(&name.as_ref())
    };
    let pieces: Vec<&str> = u.query()?.split('&').collect();
    let kept: Vec<&str> = pieces.iter().copied().filter(|p| !tracking(p)).collect();
    if kept.len() == pieces.len() {
        return None;
    }
    let kept = kept.join("&");
    u.set_query((!kept.is_empty()).then_some(kept.as_str()));
    Some(u.to_string())
}
/// The page's own canonical (or permanent redirect) as an alias: it may differ from the page
/// only by a trailing slash and tracking parameters (`untracked`). A canonical to another path
/// (a homepage, a sibling page) or another query (`/events/?event_id=42` naming `/events/`, a
/// cookie wall's `?error=…`) is a claim, not a spelling, and never aliases.
pub fn canonical(uri: &str, canonical: &str) -> Option<Alias> {
    let (_, uri, _) = crate::model::identity(uri).ok()?;
    let (_, root, _) = crate::model::identity(canonical).ok()?;
    let root = self::root(&root).map_or(root, |a| a.root);
    let spelling = |s: &str| {
        let mut u = Url::parse(&untracked(s).unwrap_or_else(|| s.to_owned())).ok()?;
        if !matches!(u.scheme(), "http" | "https") {
            return None;
        }
        let path = u.path().trim_end_matches('/').to_owned();
        u.set_path(&path);
        Some(u.to_string())
    };
    (root != uri && spelling(&uri)? == spelling(&root)?).then_some(Alias {
        root,
        basis: "canonical",
    })
}
/// The root an official API named for `uri` (`ResourceMeta::source_root`), when it is another
/// URI. The API's own record is the evidence, as an identifier is; its basis is `source`, since
/// no rule of the URI names it (`reextract` keeps it).
pub fn source(uri: &str, root: &str) -> Option<Alias> {
    let (_, uri, _) = crate::model::identity(uri).ok()?;
    let (_, root, _) = crate::model::identity(root).ok()?;
    (root != uri).then_some(Alias {
        root,
        basis: "source",
    })
}
/// The lock every alias assignment takes, so an alias never commits against a root that has
/// meanwhile become an alias itself (reads follow one hop). An assignment locks the alias's row,
/// then its root's: a transaction that may assign one, or that locks rows of several resources
/// in an order of its own (an ingested batch, an opt-out clearing its aliases), takes this lock
/// after the write order (`store::lock_writers`) and before its first row lock.
pub async fn lock(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::ALIAS)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// The change line of a new alias, written by `record` just before its transaction commits:
/// the change sequence is the last lock a transaction takes (`changes::append`).
#[derive(Debug)]
pub struct Line {
    id: String,
    root: String,
    type_path: Vec<String>,
}
/// Write the change lines of a transaction's new aliases.
pub async fn record(tx: &mut Transaction<'_, Postgres>, lines: Vec<Line>) -> Result<()> {
    for line in lines {
        // Clients that cached the resource learn which root answers for it now. The line carries
        // the root's type (or the alias's own while the root is unfetched) for `?type=` filters.
        crate::changes::append(
            tx,
            &line.id,
            "alias",
            None,
            Some(serde_json::json!({ "root": line.root, "type_path": line.type_path })),
            crate::changes::Diff {
                class: "aliased",
                fields: vec!["root".into()],
            },
            None,
        )
        .await?;
    }
    Ok(())
}
/// An alias row is answered by its root and never fetched again, so it holds no failure of its
/// own: its error, retry count and dead letter are cleared, and a failure status (`error`,
/// `unreachable`, `missing`) becomes `pending`. A row read fine before it became an alias stays
/// `ok`. Binds the alias IDs as `$1`.
pub const CLEAR_QUEUE: &str = "UPDATE resource SET status=CASE WHEN status IN ('error','unreachable','missing') THEN 'pending' ELSE status END,last_error=NULL,attempts=0,timeout_streak=0,denial_streak=0,denied_since=NULL,missing_streak=0,dead_letter=false,final=false WHERE id=ANY($1) AND (status IN ('error','unreachable','missing') OR last_error IS NOT NULL OR dead_letter OR final OR attempts>0 OR timeout_streak>0 OR denial_streak>0 OR missing_streak>0)";

/// Make `id` an alias of `alias.root`: the root row exists and is queued, the alias row is
/// never fetched again. An ID that is itself a root keeps its own aliases pointing at it. It
/// takes `lock`; the caller passes the returned line to `record` before committing.
pub async fn assign(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    alias: &Alias,
) -> Result<Option<Line>> {
    lock(tx).await?;
    let (named, root_uri, _) = crate::model::identity(&alias.root)?;
    // A root is never itself an alias: one that became one passes the alias to its own root.
    let hop: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
            .bind(&named)
            .fetch_optional(&mut **tx)
            .await?;
    let root_id = hop.clone().unwrap_or(named);
    if root_id == id {
        return Ok(None);
    }
    if hop.is_none() {
        crate::store::insert_plain(tx, &root_id, &root_uri).await?;
    }
    let previous: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM resource_alias WHERE alias_id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,$3) ON CONFLICT(alias_id) DO UPDATE SET root_id=excluded.root_id,basis=excluded.basis")
        .bind(id).bind(&root_id).bind(alias.basis).execute(&mut **tx).await?;
    let line = if previous.as_deref() != Some(root_id.as_str()) {
        let type_path: Vec<String> = sqlx::query_scalar(
            "SELECT coalesce((SELECT nullif(type_path,'{}') FROM resource WHERE id=$1),(SELECT type_path FROM resource WHERE id=$2),'{}')",
        )
        .bind(&root_id)
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        Some(Line {
            id: id.to_owned(),
            root: root_id.clone(),
            type_path,
        })
    } else {
        None
    };
    // Aliases of the alias follow it to the root.
    let followers: Vec<String> = sqlx::query_scalar(
        "UPDATE resource_alias SET root_id=$2 WHERE root_id=$1 RETURNING alias_id",
    )
    .bind(id)
    .bind(&root_id)
    .fetch_all(&mut **tx)
    .await?;
    // An OSM spelling drops its OSM envelope: only the root holds the object. The generation
    // bump fences any job still running on the alias. This statement must keep naming a column
    // of the `group_resource_revision` trigger (`doc`, `osm_version`): the trigger fires on the
    // columns an UPDATE names, not on changed values, and its `group_revision` bump is what fails
    // a concurrent grouping run's fence on the alias.
    sqlx::query("UPDATE resource SET generation=generation+1,resolver=NULL,lease_until=NULL,priority_at=NULL,osm_type=NULL,osm_id=NULL,osm_version=NULL,doc=CASE WHEN osm_id IS NULL THEN doc ELSE '{}'::jsonb END,type_path=CASE WHEN osm_id IS NULL THEN type_path ELSE '{}'::text[] END WHERE id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(CLEAR_QUEUE)
        .bind(vec![id.to_owned()])
        .execute(&mut **tx)
        .await?;
    // A spelling that joins a post already gone or opted out keeps none of its text, as the
    // post's own aliases kept none when it was scrubbed.
    let root_doc: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1 AND doc IS NOT NULL")
            .bind(&root_id)
            .fetch_optional(&mut **tx)
            .await?;
    if root_doc
        .and_then(|doc| serde_json::from_value::<crate::model::ResourceMeta>(doc).ok())
        .is_some_and(|doc| crate::changes::scrubbed(&doc))
    {
        let mut spellings = followers;
        spellings.push(id.to_owned());
        crate::changes::scrub_spellings(tx, &spellings).await?;
    }
    // The root is wanted now; its own schedule continues after its first fetch.
    sqlx::query("UPDATE resource SET priority_at=coalesce(priority_at,now()) WHERE id=$1 AND resolved_at IS NULL")
        .bind(&root_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM occurrence WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM event_fetch WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    // The members of the group it leaves, and its root, may show other links now.
    let members: Vec<String> = sqlx::query_scalar("SELECT m.resource_id FROM group_unit u JOIN group_unit m USING(group_id) WHERE u.resource_id=$1 AND m.resource_id<>$1 UNION SELECT $2")
        .bind(id)
        .bind(&root_id)
        .fetch_all(&mut **tx)
        .await?;
    crate::also_on::requeue_placed(tx, &members).await?;
    sqlx::query("DELETE FROM group_unit WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM resource_group WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    // The root it left (when it moved) no longer counts its references either.
    let mut rescored: Vec<String> = [Some(id.to_owned()), Some(root_id), previous]
        .into_iter()
        .flatten()
        .collect();
    rescored.sort_unstable();
    rescored.dedup();
    crate::store::recompute_scores(tx, &rescored).await?;
    Ok(line)
}
/// Make an alias answer for itself again, when no rule names its root any more (a source that
/// read its URI as another object's spelling now reads it at its own address): its row and its
/// old root are regrouped and rescored, and its line names its own ID as the root that answers
/// for it. `None` when it was no alias. It takes `lock`; the caller re-queues it on its route and
/// passes the line to `record` before committing.
pub async fn release(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<Option<Line>> {
    lock(tx).await?;
    let root: Option<String> =
        sqlx::query_scalar("DELETE FROM resource_alias WHERE alias_id=$1 RETURNING root_id")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(root) = root else {
        return Ok(None);
    };
    let both = vec![id.to_owned(), root];
    sqlx::query(
        "INSERT INTO group_dirty(resource_id) SELECT unnest($1::text[]) ON CONFLICT DO NOTHING",
    )
    .bind(&both)
    .execute(&mut **tx)
    .await?;
    crate::store::recompute_scores(tx, &both).await?;
    let type_path: Vec<String> =
        sqlx::query_scalar("SELECT coalesce(type_path,'{}') FROM resource WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    Ok(Some(Line {
        id: id.to_owned(),
        root: id.to_owned(),
        type_path,
    }))
}
/// The root of a resource ID, or the ID itself.
pub async fn resolve_id(pool: &PgPool, id: &str) -> Result<String> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)",
    )
    .bind(id)
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_api_spellings_and_named_roots() {
        let r = |u: &str| root(u).map(|a| (a.root, a.basis));
        // An official source's own spelling (the Federal Register's, in a build that has it).
        if crate::sources::by_name("federal_register").is_some() {
            assert_eq!(
                r("https://www.federalregister.gov/d/2026-18291"),
                Some(("fr:2026-18291".into(), "identifier"))
            );
            assert_eq!(r("fr:2026-18291"), None);
        }
        let named = source("https://example.org/a/", "https://example.org/b/").unwrap();
        assert_eq!(
            (named.root.as_str(), named.basis),
            ("https://example.org/b/", "source")
        );
        assert_eq!(
            source("https://example.org/a", "HTTPS://EXAMPLE.ORG/a"),
            None
        );
    }
    #[test]
    fn spellings_resolve_to_one_root() {
        let r = |u: &str| root(u).map(|a| (a.root, a.basis));
        assert_eq!(
            r("https://www.meetup.com/g/events/123/?recId=1&recSource=x&searchId=2&eventOrigin=find_page"),
            Some(("https://www.meetup.com/g/events/123/".into(), "tracking"))
        );
        assert_eq!(
            r("https://example.org/a?id=7&utm_source=x&fbclid=y"),
            Some(("https://example.org/a?id=7".into(), "tracking"))
        );
        assert_eq!(r("https://example.org/a?id=7"), None);
        // Platform parameters track only on their platform; kept values keep their encoding.
        assert_eq!(
            r("https://lu.ma/party?tk=abc"),
            Some(("https://lu.ma/party".into(), "tracking"))
        );
        for uri in [
            "https://example.org/a?tk=secret",
            "https://example.org/a?recId=1",
            "https://lu.ma/party?recId=1",
        ] {
            assert_eq!(r(uri), None, "{uri}");
        }
        let (e9, e8) = (
            r("https://example.org/s?q=%E9&utm_source=x"),
            r("https://example.org/s?q=%E8&utm_source=x"),
        );
        assert_eq!(e9, Some(("https://example.org/s?q=%E9".into(), "tracking")));
        assert_ne!(e9, e8);
        assert_eq!(
            r("https://osm.org/node/240109189/"),
            Some(("https://www.openstreetmap.org/node/240109189".into(), "osm"))
        );
        // An Apple Music link in any storefront, or none: one object.
        for uri in [
            "https://music.apple.com/ch/song/bohemian-rhapsody/6781024437",
            "https://music.apple.com/us/album/a-night-at-the-opera/6781024026?i=6781024437",
            "https://itunes.apple.com/gb/album/id6781024026?i=6781024437",
        ] {
            assert_eq!(
                r(uri),
                Some(("https://music.apple.com/song/6781024437".into(), "service")),
                "{uri}"
            );
        }
        assert_eq!(r("https://music.apple.com/song/6781024437"), None);
        assert_eq!(r("https://www.openstreetmap.org/node/240109189"), None);
        // A MusicBrainz page in any spelling, its web service record too.
        let recording = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69";
        assert_eq!(
            r("https://beta.musicbrainz.org/ws/2/recording/B1A9C0E9-D987-4042-AE91-78D6A3267D69?fmt=json"),
            Some((recording.into(), "service"))
        );
        assert_eq!(r(recording), None);
        assert_eq!(
            r("http://musicbrainz.org/isrc/gbum71029604/"),
            Some((
                "https://musicbrainz.org/isrc/GBUM71029604".into(),
                "service"
            ))
        );
        assert_eq!(
            r("isbn:0140328726"),
            Some(("isbn:9780140328721".into(), "identifier"))
        );
        assert_eq!(r("isbn:9780140328721"), None);
        assert_eq!(
            r("https://doi.org/10.1038/NATURE12373"),
            Some(("doi:10.1038/nature12373".into(), "identifier"))
        );
        assert_eq!(r("doi:10.1038/nature12373"), None);
        assert_eq!(
            r("https://api.btcmap.org/v4/events?updated_since=2026-01-01"),
            Some(("https://api.btcmap.org/v4/events".into(), "cursor"))
        );
        for uri in [
            "https://www.openstreetmap.org/node/240109189",
            "isbn:9780140328721",
            "https://www.meetup.com/g/events/123/",
        ] {
            assert_eq!(root(uri), None, "{uri}");
        }
    }
    /// Every spelling of a Spotify link is the one `https://open.spotify.com/{kind}/{id}`
    /// MusicBrainz stores (its URL lookup knows no other: docs/sources/spotify.md), so the
    /// `musicbrainz` source asks for that one.
    #[test]
    fn every_spotify_spelling_is_the_one_musicbrainz_stores() {
        // The resource a spelling is: its root, else itself as identity writes it (a host's
        // case is no alias).
        let resolved = |uri: &str| {
            root(uri)
                .map(|a| a.root)
                .or_else(|| crate::model::identity(uri).ok().map(|(_, uri, _)| uri))
        };
        let track = "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq";
        for uri in [
            "https://open.spotify.com/intl-de/track/6LgJvl0Xdtc73RJ1mmpotq",
            "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq?si=4f1e2d3c4b5a6978",
            "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq?si=a&context=spotify%3Aalbum%3Ax",
            "https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq/",
            "https://play.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
            "http://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq",
            "https://OPEN.SPOTIFY.COM/track/6LgJvl0Xdtc73RJ1mmpotq",
            "https://open.spotify.com/embed/track/6LgJvl0Xdtc73RJ1mmpotq?utm_source=generator",
            "spotify:track:6LgJvl0Xdtc73RJ1mmpotq",
            // The retired player's form, which MusicBrainz' URL cleanup still folds.
            "https://embed.spotify.com/?uri=spotify:track:6LgJvl0Xdtc73RJ1mmpotq",
        ] {
            assert_eq!(resolved(uri), Some(track.into()), "{uri}");
        }
        assert_eq!(root(track), None);
        for (uri, canonical) in [
            (
                "https://open.spotify.com/intl-fr/album/6dVIqQ8qmQ5GBnJ9shOYGE?si=x",
                "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE",
            ),
            (
                "https://open.spotify.com/embed/album/6dVIqQ8qmQ5GBnJ9shOYGE",
                "https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE",
            ),
            (
                "spotify:artist:4Z8W4fKeB5YxbusRsdQVPb",
                "https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb",
            ),
            (
                "https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb/",
                "https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb",
            ),
        ] {
            assert_eq!(resolved(uri), Some(canonical.into()), "{uri}");
        }
    }
    #[test]
    fn only_a_same_path_canonical_is_an_alias() {
        let c = |u: &str, c: &str| canonical(u, c).map(|a| a.root);
        assert_eq!(
            c(
                "https://shop.example/item/7?utm_source=feed",
                "https://shop.example/item/7"
            ),
            Some("https://shop.example/item/7".into())
        );
        assert_eq!(
            c(
                "https://shop.example/item/7/",
                "https://shop.example/item/7"
            ),
            Some("https://shop.example/item/7".into())
        );

        // A homepage, a sibling path, another host or scheme is a claim, never a spelling.
        for (uri, target) in [
            ("https://thebiz.pro/episodes/1/a", "https://thebiz.pro/"),
            ("https://pubky.org/eli5/", "https://pubky.org/introduction/"),
            ("https://docs.pubky.org/x", "https://pubky.org/x"),
            ("http://shop.example/item/7", "https://shop.example/item/7"),
            ("https://shop.example/item/7", "https://shop.example/item/7"),
            (
                "https://www.nature.com/articles/nature12373",
                "https://www.nature.com/articles/nature12373?error=cookies_not_supported&code=5",
            ),
            (
                "https://shop.example/item/7?a=1",
                "https://shop.example/item/7?a=2",
            ),
            // A permalink canonical drops a parameter that names the object.
            (
                "https://wp.example/events/?event_id=42",
                "https://wp.example/events/",
            ),
            (
                "https://shop.example/item/7?ref=home",
                "https://shop.example/item/7",
            ),
            (
                "https://shop.example/item?id=7&ref=home",
                "https://shop.example/item?id=7",
            ),
        ] {
            assert_eq!(c(uri, target), None, "{uri} → {target}");
        }
    }
}
