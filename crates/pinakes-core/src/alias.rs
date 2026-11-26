//! Aliases: the same object under another URI, resolved deterministically to one resource.
//!
//! An alias keeps its own resource ID and references (Nexus's identity is unchanged), but it is
//! never fetched: its references count for the root, and reads, lookups, changes and occurrences
//! answer with the root. Groups are a different thing (`groups`): evidence that two objects are
//! one, never a spelling.
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};
use url::Url;

/// A query parameter Pinakes drops from a link as tracking ([`untracked`]): one that only tracks
/// how the link was shared and never changes the page. It matches a parameter whose name,
/// percent-decoded, is `param` (or starts with it, `prefix`), on every host when `hosts` is empty,
/// else only on those hosts and their subdomains (elsewhere the name may carry identity or
/// access). `data/spellings.json` lists them (`tracking`), in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tracking {
    pub param: &'static str,
    pub prefix: bool,
    pub hosts: &'static [&'static str],
}
impl Tracking {
    const fn any(param: &'static str) -> Self {
        Self {
            param,
            prefix: false,
            hosts: &[],
        }
    }
    const fn on(hosts: &'static [&'static str], param: &'static str) -> Self {
        Self {
            param,
            prefix: false,
            hosts,
        }
    }
    /// Whether a parameter of this name on this host is this tracking parameter.
    pub fn matches(&self, host: &str, name: &str) -> bool {
        let named = if self.prefix {
            name.starts_with(self.param)
        } else {
            name == self.param
        };
        named
            && (self.hosts.is_empty()
                || self
                    .hosts
                    .iter()
                    .any(|h| host == *h || host.ends_with(&format!(".{h}"))))
    }
}
const MEETUP: &[&str] = &["meetup.com"];
const LUMA: &[&str] = &["luma.com", "lu.ma"];
const SHARE_ID: &[&str] = &["spotify.com", "youtube.com", "youtu.be", "soundcloud.com"];
const YOUTUBE: &[&str] = &["youtube.com", "youtu.be"];
const INSTAGRAM: &[&str] = &["instagram.com"];
const X: &[&str] = &["x.com", "twitter.com"];
/// Every tracking parameter Pinakes drops: `utm_*` and the click and mail ids on every host, and
/// the share parameters of the platforms that add them to a shared link.
pub const TRACKING: &[Tracking] = &[
    Tracking {
        param: "utm_",
        prefix: true,
        hosts: &[],
    },
    Tracking::any("fbclid"),
    Tracking::any("gclid"),
    Tracking::any("dclid"),
    Tracking::any("msclkid"),
    Tracking::any("igshid"),
    Tracking::any("mc_cid"),
    Tracking::any("mc_eid"),
    Tracking::any("_gl"),
    Tracking::on(MEETUP, "recId"),
    Tracking::on(MEETUP, "recSource"),
    Tracking::on(MEETUP, "searchId"),
    Tracking::on(MEETUP, "eventOrigin"),
    Tracking::on(LUMA, "tk"),
    Tracking::on(SHARE_ID, "si"),
    Tracking::on(YOUTUBE, "feature"),
    Tracking::on(INSTAGRAM, "igsh"),
    Tracking::on(X, "s"),
    Tracking::on(X, "t"),
    Tracking::on(X, "ref_src"),
    Tracking::on(X, "ref_url"),
];
#[derive(Debug, PartialEq)]
pub struct Alias {
    pub root: String,
    /// `cursor`, `osm`, `identifier`, `service` (a music service's, Wikipedia's or a Wikidata
    /// item's URL forms), `tracking`, `canonical` (the page's own canonical) or `source` (an
    /// official API's answer, or a podcast feed naming its episode's page on its own site:
    /// `episodes::join_page`). The last two are no rule of the URI.
    pub basis: &'static str,
}
/// The root of a URI known from the URI alone, when it differs from the URI itself.
pub fn root(uri: &str) -> Option<Alias> {
    rooted(uri).map(|(alias, _)| alias)
}
/// A rule of [`rooted`] that is neither a music service's nor an official source's ([`Named`]). A
/// new rule gets a variant here, which `tests/spellings.rs` makes list and give vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    Cursor,
    Osm,
    Isbn,
    Doi,
    Wikipedia,
    Wikidata,
    MusicBrainz,
    Tracking,
}
impl Rule {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Osm => "osm",
            Self::Isbn => "isbn",
            Self::Doi => "doi",
            Self::Wikipedia => "wikipedia",
            Self::Wikidata => "wikidata",
            Self::MusicBrainz => "musicbrainz",
            Self::Tracking => "tracking",
        }
    }
}
/// The rule that named a root ([`rooted`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Core(Rule),
    /// An official source's spellings, by the source's name.
    Source(&'static str),
    Music(crate::music::Service),
}
impl Named {
    /// The rule's name in `data/spellings.json`: a [`Rule`]'s, `source/{name}` or
    /// `music/{service}` (`music::Service::token`).
    pub fn name(self) -> String {
        match self {
            Self::Core(rule) => rule.name().to_owned(),
            Self::Source(name) => format!("source/{name}"),
            Self::Music(service) => format!("music/{}", service.token()),
        }
    }
}
/// [`root`] with the rule that named it. `data/spellings.json` lists examples of each.
pub fn rooted(uri: &str) -> Option<(Alias, Named)> {
    use Named::Core;
    let (_, uri, _) = crate::model::identity(uri).ok()?;
    let found = if let Some(root) = crate::event::adapters::cursor::canonical_root(&uri) {
        Some((root, "cursor", Core(Rule::Cursor)))
    } else if let Some(osm) = crate::resolver::osm::parse_id(&uri) {
        Some((osm.uri(), "osm", Core(Rule::Osm)))
    } else if let Some(isbn) =
        crate::resolver::cards::isbn(&uri).or_else(|| crate::resolver::cards::isbn_of_page(&uri))
    {
        Some((
            format!("isbn:{}", crate::resolver::cards::isbn13(&isbn)),
            "identifier",
            Core(Rule::Isbn),
        ))
    } else if let Some(doi) = crate::resolver::cards::doi(&uri) {
        Some((format!("doi:{doi}"), "identifier", Core(Rule::Doi)))
    } else if let Some((source, root)) = crate::sources::alias_root_by(&uri) {
        Some((root, "identifier", Named::Source(source)))
    } else if let Some(music) = crate::music::parse(&uri) {
        Some((music.canonical, "service", Named::Music(music.service)))
    } else if let Some(article) = crate::wikipedia::parse(&uri) {
        Some((article.canonical, "service", Core(Rule::Wikipedia)))
    } else if let Some((_, item)) = crate::hubs::wikidata::item_link(&uri) {
        Some((item, "service", Core(Rule::Wikidata)))
    } else if let Some((_, _, page)) = crate::hubs::musicbrainz::entity_link(&uri) {
        Some((page, "service", Core(Rule::MusicBrainz)))
    } else if let Some((_, page)) = crate::hubs::musicbrainz::isrc_link(&uri) {
        Some((page, "service", Core(Rule::MusicBrainz)))
    } else {
        untracked(&uri).map(|root| (root, "tracking", Core(Rule::Tracking)))
    }?;
    let (_, root, _) = crate::model::identity(&found.0).ok()?;
    (root != uri).then_some((
        Alias {
            root,
            basis: found.1,
        },
        found.2,
    ))
}
/// The web page Pinakes links for an identifier, built from the identifier alone, with the rule
/// that builds it: a join key's page (`wikidata:{Q-id}`, `mb:{kind}:{mbid}`, `isrc:{ISRC}`,
/// `osm:{type}/{id}`), or the page of a card whose root is no web link (`doi:…`, `isbn:…`, a
/// source's own scheme such as `artic:…` or `btc:…`), in any spelling of that root, whichever
/// sources are on. `data/spellings.json` (`pages`) lists examples of each. `None` for a web link
/// (its own page) and for an identifier with no single page of its own (an MBID without its
/// entity, a barcode, …).
pub fn web_page(identifier: &str) -> Option<(String, Named)> {
    use Named::Core;
    let identifier = identifier.trim();
    let mut key = identifier.to_owned();
    if let Ok((_, written, scheme)) = crate::model::identity(identifier) {
        // A key's kind in any letter case (`WIKIDATA:Q42`), as a stored link writes it.
        key.clone_from(&written);
        if matches!(scheme.as_str(), "http" | "https") {
            return None;
        }
        // No switch is read: a DOI in any letter case, and a source's spelling of its root
        // (`root_page_by` folds it with the source's own rule).
        if let Some(number) = written
            .strip_prefix("doi:")
            .and_then(|_| crate::resolver::cards::doi(&written))
        {
            // Its page reads back as the DOI (`doi_page` encodes `%`, `?` and `#`); one that
            // would not has no page.
            let (_, page, _) =
                crate::model::identity(&crate::resolver::cards::doi_page(&number)).ok()?;
            return (crate::resolver::cards::doi(&page).as_deref() == Some(number.as_str()))
                .then_some((page, Core(Rule::Doi)));
        }
        // An ISBN's page is Open Library's ISBN page, under its ISBN-13, which folds back to it.
        if let Some(number) = written
            .strip_prefix("isbn:")
            .and_then(|_| crate::resolver::cards::isbn(&written))
        {
            return Some((
                crate::resolver::cards::isbn_page(&crate::resolver::cards::isbn13(&number)),
                Core(Rule::Isbn),
            ));
        }
        if let Some((source, page)) = crate::sources::root_page_by(&written) {
            return Some((page, Named::Source(source)));
        }
    }
    let key = crate::catalogue::stored_key(&key)?;
    let (kind, value) = key.split_once(':')?;
    let (page, rule) = match kind {
        "wikidata" => (
            crate::hubs::wikidata::item_page(&crate::hubs::qid(value)?),
            Rule::Wikidata,
        ),
        "mb" => {
            let (written, mbid) = value.split_once(':')?;
            let kind = crate::hubs::MbKind::parse(written).filter(|k| k.as_str() == written)?;
            let mbid = crate::hubs::mbid(mbid)?;
            (
                crate::hubs::musicbrainz::entity_page(kind, &mbid),
                Rule::MusicBrainz,
            )
        }
        "isrc" => (
            crate::hubs::musicbrainz::isrc_page(&crate::music::isrc(value)?),
            Rule::MusicBrainz,
        ),
        "osm" => {
            crate::lookups::search_key("osm", value)?;
            let osm =
                crate::resolver::osm::parse_id(&format!("https://www.openstreetmap.org/{value}"))?;
            (osm.uri(), Rule::Osm)
        }
        _ => return None,
    };
    Some((page, Core(rule)))
}
/// The version of the URI-only spelling rules (`/v1/info` `spelling_rules`): the first 16 hex
/// characters of the BLAKE3 hash of `data/spellings.json`, the tracking parameters, the rules'
/// test vectors and the identifier pages, which a test holds equal to [`TRACKING`] and to what
/// [`rooted`] and [`web_page`] answer.
pub fn spelling_rules() -> &'static str {
    static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        blake3::hash(include_bytes!("../../../data/spellings.json")).to_hex()[..16].to_owned()
    });
    &VERSION
}
/// The URI without its tracking parameters, if it had any. Parameters are dropped from the raw
/// query string, so every kept value keeps its own encoding.
pub(crate) fn untracked(uri: &str) -> Option<String> {
    let mut u = Url::parse(uri).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str().unwrap_or_default().to_owned();
    let tracking = |piece: &str| {
        let name = piece.split('=').next().unwrap_or_default();
        let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
        TRACKING.iter().any(|t| t.matches(&host, &name))
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
/// Platforms that give one event page two spellings in one directory, its number and its token:
/// the hosts, the path of an event page (`*` stands for one non-empty segment, the last one for
/// the event) and the segments there that name no event (Meetup: a date's number,
/// `/g/events/316908442/`, and its token, `/g/events/frxnbvyjcqbmb/`; `/g/events/ical/` is the
/// group's calendar).
const EVENT_SPELLINGS: &[(&[&str], &str, &[&str])] = &[(&["meetup.com"], "/*/events/*", &["ical"])];
/// The page's own head canonical as an alias on a platform of [`EVENT_SPELLINGS`]: on the
/// page's host and in its event directory (the same group's `/events/`), naming another event
/// segment. The platform gives each event page these two spellings; a canonical to another
/// group, another host or any other path is a claim and never aliases (`canonical`).
pub fn event_spelling(uri: &str, canonical: &str) -> Option<Alias> {
    let (_, uri, _) = crate::model::identity(uri).ok()?;
    let (_, root, _) = crate::model::identity(canonical).ok()?;
    let (page, named) = (Url::parse(&uri).ok()?, Url::parse(&root).ok()?);
    let host = page.host_str()?;
    let (_, pattern, reserved) = EVENT_SPELLINGS.iter().find(|(hosts, _, _)| {
        hosts
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    })?;
    let event = |u: &Url| -> Option<(String, String)> {
        let path = u.path().trim_end_matches('/');
        if !matches!(u.scheme(), "http" | "https")
            || u.query().is_some()
            || !crate::event::adapters::canary_link_matches(pattern, path)
        {
            return None;
        }
        let (directory, segment) = path.rsplit_once('/')?;
        (segment.bytes().all(|b| b.is_ascii_alphanumeric()) && !reserved.contains(&segment))
            .then(|| (directory.to_owned(), segment.to_owned()))
    };
    let number = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let (directory, own) = event(&page)?;
    let (other_directory, other) = event(&named)?;
    // One number and one token: two dates' numbers (or tokens) are two dates.
    (named.host_str() == Some(host)
        && named.port() == page.port()
        && named.scheme() == page.scheme()
        && other_directory == directory
        && number(&own) != number(&other))
    .then_some(Alias {
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
            "https://example.org/a?si=1&s=2&t=3&feature=4&igsh=5",
            "https://example.org/a?xutm_source=1&FBCLID=2",
        ] {
            assert_eq!(r(uri), None, "{uri}");
        }
        // Every tracking parameter is dropped where it tracks (with another value kept), and a
        // host-scoped one is kept on any other host.
        for t in TRACKING {
            let name = if t.prefix {
                format!("{}campaign", t.param)
            } else {
                t.param.to_owned()
            };
            for host in t.hosts.iter().copied().chain(["example.org"]) {
                let link = format!("https://sub.{host}/a/b?keep=1&{name}=x");
                let dropped = untracked(&link);
                if t.hosts.is_empty() || host != "example.org" {
                    assert_eq!(
                        dropped.as_deref(),
                        Some(format!("https://sub.{host}/a/b?keep=1").as_str()),
                        "{link}"
                    );
                } else {
                    assert_eq!(dropped, None, "{link}");
                }
            }
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
        // Open Library's ISBN page, in any ISBN spelling, is the book's card.
        for uri in [
            "https://openlibrary.org/isbn/9780140328721",
            "https://openlibrary.org/isbn/0140328726/",
            "https://www.openlibrary.org/isbn/978-0-14-032872-1",
            "http://openlibrary.org/isbn/0-14-032872-6",
        ] {
            assert_eq!(
                r(uri),
                Some(("isbn:9780140328721".into(), "identifier")),
                "{uri}"
            );
        }
        for uri in [
            "https://openlibrary.org/isbn/9780140328721.json",
            "https://openlibrary.org/isbn/9780140328722",
            "https://openlibrary.org/isbn/9780140328721/x",
            "https://openlibrary.example/isbn/9780140328721",
        ] {
            assert_ne!(
                r(uri).map(|a| a.0),
                Some("isbn:9780140328721".into()),
                "{uri}"
            );
        }
        assert_eq!(
            web_page("isbn:0-14-032872-6").map(|(p, n)| (p, n.name())),
            Some((
                "https://openlibrary.org/isbn/9780140328721".into(),
                "isbn".into()
            ))
        );
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

    /// A Meetup date's page and its token spelling are one event in both directions; a
    /// canonical to another group, another host, another path or a query is not.
    #[test]
    fn a_meetup_dates_number_and_token_are_spellings_of_one_page() {
        let numeric = "https://www.meetup.com/bitcoin-meetup-switzerland/events/316908442/";
        let token = "https://www.meetup.com/bitcoin-meetup-switzerland/events/frxnbvyjcqbmb/";
        let root = |a: Option<Alias>| a.map(|a| (a.root, a.basis));
        assert_eq!(
            root(event_spelling(numeric, token)),
            Some((token.into(), "canonical"))
        );
        assert_eq!(
            root(event_spelling(token, numeric)),
            Some((numeric.into(), "canonical"))
        );
        assert_eq!(
            root(event_spelling(
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/316908442",
                token
            )),
            Some((token.into(), "canonical"))
        );
        // Not a spelling the generic rule accepts: another path.
        assert_eq!(canonical(numeric, token), None);
        for (uri, target) in [
            (numeric, numeric),
            (
                numeric,
                "https://www.meetup.com/another-group/events/frxnbvyjcqbmb/",
            ),
            (
                numeric,
                "https://meetup.example.com/bitcoin-meetup-switzerland/events/frxnbvyjcqbmb/",
            ),
            (
                numeric,
                "http://www.meetup.com/bitcoin-meetup-switzerland/events/frxnbvyjcqbmb/",
            ),
            (
                numeric,
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/frxnbvyjcqbmb/?x=1",
            ),
            (
                numeric,
                "https://www.meetup.com/bitcoin-meetup-switzerland/",
            ),
            (
                numeric,
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/ical/x/",
            ),
            // Two numbers, two tokens, the group's calendar: never one date.
            (
                numeric,
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/316908445/",
            ),
            (
                token,
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/frxnbvykccbjb/",
            ),
            (
                numeric,
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/ical/",
            ),
            (
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/ical/",
                numeric,
            ),
            (
                "https://www.meetup.com/bitcoin-meetup-switzerland/",
                "https://www.meetup.com/bitcoin-meetup-switzerland/events/frxnbvyjcqbmb/",
            ),
            (
                "https://events.example.com/g/events/316908442/",
                "https://events.example.com/g/events/frxnbvyjcqbmb/",
            ),
        ] {
            assert_eq!(event_spelling(uri, target), None, "{uri} → {target}");
        }
    }
}
