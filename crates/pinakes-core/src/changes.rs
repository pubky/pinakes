use crate::model::{ResourceMeta, Status};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::BTreeSet;

pub const CLASSES: &[&str] = &[
    "created",
    "rescheduled",
    "relocated",
    "cancelled",
    "uncancelled",
    "renamed",
    "details",
    "deleted",
    "aliased",
];
/// Fetch clocks and body length are observations, not changes to the resource's facts.
pub fn normalize(doc: &ResourceMeta) -> Value {
    let mut value = serde_json::to_value(doc).expect("typed document");
    let object = value.as_object_mut().unwrap();
    object.remove("fetched_at");
    object.remove("ttl_secs");
    object.remove("warnings");
    if let Some(attrs) = object.get_mut("attributes").and_then(Value::as_object_mut) {
        attrs.remove("content_length");
    }
    if let Some(aliases) = object.get_mut("aliases").and_then(Value::as_array_mut) {
        aliases.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        aliases.dedup();
    }
    value
}
#[derive(Debug, PartialEq, Eq)]
pub struct Diff {
    pub class: &'static str,
    pub fields: Vec<String>,
}
/// Whether the author of a post opted out of indexing (its `opted_out` fact).
fn opted_out(doc: &ResourceMeta) -> bool {
    doc.attributes.get("opted_out").is_some_and(|v| v == "true")
}
/// Whether a stored card is a social post found gone. Its document says so, whatever a failed
/// refresh set the row's status to since; gone wins over an opt-out the post had.
pub(crate) fn gone_social(doc: &ResourceMeta) -> bool {
    doc.status == Status::Missing && crate::resolver::cards::social(doc)
}
/// Whether a stored card keeps none of its post's text, nor do its spellings: a social post
/// found gone, or one whose author opted out of indexing.
pub(crate) fn scrubbed(doc: &ResourceMeta) -> bool {
    opted_out(doc) || gone_social(doc)
}
/// The one type of every gone social post's card, whatever the post was: its type was the
/// post's read too.
pub const GONE_POST_TYPE: &str = "SocialMediaPosting";
/// Whether `doc` has a gone social post's own type (`gone_card`): each source's social rule
/// accepts it, so the card stays known as a gone social post.
pub(crate) fn gone_post_type(doc: &ResourceMeta) -> bool {
    doc.status == Status::Missing && doc.type_name == GONE_POST_TYPE
}
/// What a gone social post's card keeps: nothing of the post. Only its link (URL, site, the
/// service the URL names), the source that read it, its status and when it was last read, with
/// the one type every gone post has (`GONE_POST_TYPE`). The source and that type are what keep
/// it known as a gone social post (`cards::social` reads only those), so a spelling that joins
/// it later is scrubbed too (`alias::assign`).
pub(crate) fn gone_card(doc: &ResourceMeta) -> ResourceMeta {
    let mut card = ResourceMeta {
        url: doc.url.clone(),
        site: doc.site.clone(),
        service: doc.service.clone(),
        source: doc.source.clone(),
        fetched_at: doc.fetched_at,
        ttl_secs: doc.ttl_secs,
        status: doc.status.clone(),
        ..Default::default()
    };
    card.set_type(GONE_POST_TYPE);
    card
}
/// Delete what the also-on lookups of `ids` found from their cards: facts, keys and links. The
/// rows deleted.
async fn forget_derived(tx: &mut Transaction<'_, Postgres>, ids: &[String]) -> Result<u64> {
    let mut deleted = 0;
    for table in [
        "resource_fact",
        "resource_key",
        "derived_link",
        "link_release",
    ] {
        deleted += sqlx::query(&format!("DELETE FROM {table} WHERE resource_id=ANY($1)"))
            .bind(ids)
            .execute(&mut **tx)
            .await?
            .rows_affected();
    }
    Ok(deleted)
}
/// Drop a gone social post's also-on mark, which the write of its card just set: it runs no
/// lookups (`also_on`). Reextract and a failed refresh queue it again; the worker then skips it.
async fn unqueue_also_on(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM also_on_dirty WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// The most spellings one statement of `scrub_spellings` names.
const SPELLINGS_PER_STATEMENT: usize = 500;
/// Delete what spellings of a scrubbed post (`scrubbed`) kept of it, as when they were saved
/// before they became its aliases: every change line but the `aliased` one naming its root,
/// their stored cards (`{}`, as an OSM spelling's; an alias's card is never served, its root's
/// is) and what was derived from them (`forget_derived`). The caller holds the alias lock. The
/// rows changed.
pub(crate) async fn scrub_spellings(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[String],
) -> Result<u64> {
    let mut changed = 0;
    for ids in ids.chunks(SPELLINGS_PER_STATEMENT) {
        changed += sqlx::query(
            "DELETE FROM resource_change WHERE class<>'aliased' AND resource_id=ANY($1)",
        )
        .bind(ids)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        changed += sqlx::query("UPDATE resource SET name=NULL,category=NULL,point=NULL,doc='{}'::jsonb WHERE id=ANY($1) AND (doc<>'{}'::jsonb OR name IS NOT NULL)")
            .bind(ids)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        changed += forget_derived(tx, ids).await?;
    }
    Ok(changed)
}
/// `scrub_spellings` of every alias of `root`, fetched SPELLINGS_PER_STATEMENT at a time from one
/// cursor over the root's index entries, so a post with very many spellings never holds them all
/// and its index entries are each read once. The caller holds the alias lock, so none joins or
/// leaves meanwhile. The rows changed.
async fn scrub_aliases_of(tx: &mut Transaction<'_, Postgres>, root: &str) -> Result<u64> {
    sqlx::query("DECLARE scrub_aliases NO SCROLL CURSOR FOR SELECT alias_id FROM resource_alias WHERE root_id=$1")
        .bind(root)
        .execute(&mut **tx)
        .await?;
    let mut changed = 0;
    loop {
        let ids: Vec<String> = sqlx::query_scalar(&format!(
            "FETCH FORWARD {SPELLINGS_PER_STATEMENT} FROM scrub_aliases"
        ))
        .fetch_all(&mut **tx)
        .await?;
        if ids.is_empty() {
            break;
        }
        changed += scrub_spellings(tx, &ids).await?;
    }
    sqlx::query("CLOSE scrub_aliases")
        .execute(&mut **tx)
        .await?;
    Ok(changed)
}
fn cancelled(v: &Value) -> bool {
    v["status"] == "missing" || v["event_status"] == "cancelled"
}
pub fn diff(before: Option<&Value>, after: Option<&Value>) -> Option<Diff> {
    let keys = before
        .into_iter()
        .chain(after)
        .filter_map(Value::as_object)
        .flat_map(|v| v.keys().cloned())
        .collect::<BTreeSet<_>>();
    let fields = keys
        .into_iter()
        .filter(|k| before.and_then(|v| v.get(k)) != after.and_then(|v| v.get(k)))
        .collect::<Vec<_>>();
    if fields.is_empty() {
        return None;
    }
    let class = match (before, after) {
        (None, _) => "created",
        (_, None) => "deleted",
        (Some(before), Some(after)) if !cancelled(before) && cancelled(after) => "cancelled",
        (Some(before), Some(after)) if cancelled(before) && !cancelled(after) => "uncancelled",
        _ if fields.iter().any(|k| matches!(k.as_str(), "start" | "end")) => "rescheduled",
        _ if fields
            .iter()
            .any(|k| matches!(k.as_str(), "location" | "venue" | "venue_id")) =>
        {
            "relocated"
        }
        _ if fields.iter().any(|k| k == "name") => "renamed",
        _ => "details",
    };
    Some(Diff { class, fields })
}
async fn lock_sequence(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // Commit order must equal sequence order, or a cursor could skip an uncommitted lower seq.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::locks::CHANGE_SEQUENCE)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn snapshot(
    tx: &mut Transaction<'_, Postgres>,
    osm_type: &str,
    osm_id: i64,
    value: &Value,
) -> Result<Value> {
    let version: String = sqlx::query_scalar("SELECT md5($1::jsonb::text)")
        .bind(value)
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query("UPDATE osm_place SET versions=versions || jsonb_build_object($3::text,$4::jsonb) WHERE osm_type=$1 AND osm_id=$2")
        .bind(osm_type).bind(osm_id).bind(&version).bind(value).execute(&mut **tx).await?;
    Ok(json!({"osm_id":format!("{osm_type}/{osm_id}"),"version":version}))
}
/// An occurrence key (`["<uid>","<recurrence id>"]`, as change lines, fetch state and group
/// units hold it) with its UID as it is stored (`event::stored_uid`): the key as written when
/// its UID holds no link that carries a credential. The recurrence id is kept as written,
/// whatever its JSON.
pub fn stored_key(key: &str) -> String {
    match serde_json::from_str::<(String, Value)>(key) {
        Ok((uid, rid)) if crate::capability::contains_credential(&uid) => {
            serde_json::to_string(&(crate::event::stored_uid(uid), rid))
                .unwrap_or_else(|_| key.to_owned())
        }
        _ => key.to_owned(),
    }
}
pub(crate) async fn append(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    source: &str,
    before: Option<Value>,
    after: Option<Value>,
    diff: Diff,
    occurrence: Option<&str>,
) -> Result<()> {
    // A change line names no credential but its resource's own link (`capability::strip`): the
    // link the resource is read by, never a `url` a stored document says.
    let own: String = sqlx::query_scalar("SELECT uri FROM resource WHERE id=$1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or_default();
    let [before, after] = [before, after].map(|value| {
        value.map(|mut value| {
            crate::capability::strip::<Value>(&mut value, &own);
            value
        })
    });
    let osm = before
        .as_ref()
        .or(after.as_ref())
        .and_then(|v| v["osm_id"].as_str())
        .and_then(|v| {
            crate::resolver::osm::parse_id(&format!("https://www.openstreetmap.org/{v}"))
        });
    let (before, after) = if source == "osm" {
        let osm = osm.as_ref().context("OSM change missing identity")?;
        let before = match before {
            Some(v) => Some(snapshot(tx, &osm.osm_type, osm.id, &v).await?),
            None => None,
        };
        let after = match after {
            Some(v) => Some(snapshot(tx, &osm.osm_type, osm.id, &v).await?),
            None => None,
        };
        (before, after)
    } else {
        (before, after)
    };
    // A change line names its occurrence by the UID as it is stored (`stored_key`), whichever
    // writer passed the key.
    let occurrence = occurrence.map(stored_key);
    lock_sequence(tx).await?;
    sqlx::query("INSERT INTO resource_change(resource_id,class,fields,before,after,source,osm_type,osm_id,at,occurrence_key) VALUES($1,$2,$3,$4,$5,$6,$7,$8,clock_timestamp(),$9)")
        .bind(id).bind(diff.class).bind(diff.fields).bind(before).bind(after).bind(source).bind(osm.as_ref().map(|o|&o.osm_type)).bind(osm.as_ref().map(|o|o.id)).bind(occurrence.as_deref()).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO source_state(source,change_totals) VALUES($1,jsonb_build_object($2::text,1)) ON CONFLICT(source) DO UPDATE SET change_totals=source_state.change_totals || jsonb_build_object($2::text,coalesce((source_state.change_totals->>$2)::bigint,0)+1)")
        .bind(source).bind(diff.class).execute(&mut **tx).await?;
    Ok(())
}
/// Rewrite a stored card in place by `change` (whether it changed it), given what `read` reads
/// once the card's row is locked, with its change line like any save's, and nothing else: its schedule stays (a final or dead-lettered card stays so). An
/// OSM place's card is never rewritten. Whether it was.
pub(crate) async fn rewrite<T, F: std::future::Future<Output = Result<T>>>(
    pool: &PgPool,
    id: &str,
    read: impl FnOnce() -> F,
    change: impl FnOnce(&mut ResourceMeta, T) -> bool,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
    let row = sqlx::query("SELECT coalesce(r.resolver,r.doc->>'source','') AS source,r.doc FROM resource r WHERE r.id=$1 AND r.osm_id IS NULL AND r.doc IS NOT NULL FOR UPDATE OF r")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else { return Ok(false) };
    let source: String = row.try_get("source")?;
    let Ok(mut doc) = serde_json::from_value::<ResourceMeta>(row.try_get("doc")?) else {
        return Ok(false);
    };
    // What the change depends on, read now that the row is locked: a writer that changed it
    // before this lock (a save, a source's run, which lock the row too) is seen.
    let read = read().await?;
    let before = normalize(&doc);
    if !change(&mut doc, read) {
        return Ok(false);
    }
    let after = normalize(&doc);
    sqlx::query("UPDATE resource SET doc=$2 WHERE id=$1")
        .bind(id)
        .bind(serde_json::to_value(&doc)?)
        .execute(&mut *tx)
        .await?;
    if let Some(diff) = diff(Some(&before), Some(&after)) {
        append(&mut tx, id, &source, Some(before), Some(after), diff, None).await?;
    }
    tx.commit().await?;
    Ok(true)
}
/// Clear a resource's card for good: its document becomes an empty one with status `error`,
/// the change is recorded like any other, and the resource is final with `last_error` (a page
/// too complex to parse). Unlike `save`, nothing of the old card is kept: no drift guard holds
/// it, and no retained snapshot. `pinakesd reextract` revives the resource.
pub async fn clear(pool: &PgPool, id: &str, generation: i64, last_error: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
    let row = sqlx::query("SELECT r.uri,r.resolver,r.doc AS accepted FROM resource r WHERE r.id=$1 AND r.generation=$2 AND r.osm_id IS NULL FOR UPDATE OF r")
        .bind(id)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else { return Ok(()) };
    let uri: String = row.try_get("uri")?;
    let source = row
        .try_get::<Option<String>, _>("resolver")?
        .unwrap_or_default();
    let mut doc = ResourceMeta::new(&uri, &source, 0);
    doc.status = Status::Error;
    let before = row
        .try_get::<Option<Value>, _>("accepted")?
        .and_then(|v| serde_json::from_value::<ResourceMeta>(v).ok())
        .map(|d| normalize(&d));
    let after = normalize(&doc);
    crate::store::write_document(&mut tx, id, generation, &doc).await?;
    sqlx::query(
        "UPDATE resource SET last_error=$3,attempts=attempts+1 WHERE id=$1 AND generation=$2",
    )
    .bind(id)
    .bind(generation)
    .bind(last_error)
    .execute(&mut *tx)
    .await?;
    if let Some(diff) = diff(before.as_ref(), Some(&after)) {
        append(&mut tx, id, &source, before, Some(after), diff, None).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn save(
    pool: &PgPool,
    id: &str,
    generation: i64,
    doc: &ResourceMeta,
    cfg: &crate::config::QueueConfig,
) -> Result<()> {
    let bounded = crate::event::bound_instants(doc);
    let doc = bounded.as_ref().unwrap_or(doc);
    // Read before the document's copy below, which leaves out what is never stored.
    let withheld = doc.withheld;
    let episodes = doc.episodes.clone();
    let mut prepared = if let Some(container) = &doc.container {
        Some(
            crate::event::storage::prepare(pool, id, container, doc.fetched_at, doc.ttl_secs)
                .await?,
        )
    } else {
        None
    };
    let adapter_source = crate::event::adapters::definition(&doc.source).map(|d| d.id.as_str());
    if let (Some(plan), Some(source)) = (&mut prepared, adapter_source) {
        plan.source = source.into();
    }
    // A post whose author opted out of indexing keeps no text in its history: what was kept
    // before goes now, and its lines carry only their kind, fields and time.
    let opted_out = opted_out(doc);
    let mut tx = pool.begin().await?;
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
    if opted_out || doc.status == Status::Missing {
        // It, or a social post found gone (known only from the stored card, read below), also
        // clears its aliases' rows, after its own.
        crate::alias::lock(&mut tx).await?;
    }
    let row=sqlx::query("SELECT r.*,CASE WHEN r.osm_id IS NULL THEN r.doc ELSE o.versions->r.osm_version END AS accepted FROM resource r LEFT JOIN osm_place o ON r.osm_type=o.osm_type AND r.osm_id=o.osm_id WHERE r.id=$1 AND r.generation=$2 FOR UPDATE OF r")
        .bind(id).bind(generation).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(()) };
    let own: String = row.try_get("uri")?;
    let source = adapter_source
        .map(str::to_owned)
        .or(row.try_get::<Option<String>, _>("resolver")?)
        .unwrap_or_else(|| doc.source.clone());
    if matches!(
        doc.status,
        Status::Unreachable | Status::Error | Status::Pending
    ) {
        let attempt = row.try_get::<i32, _>("attempts")? as u32 + 1;
        sqlx::query("UPDATE resource SET status=$2,lease_until=NULL,priority_at=NULL,attempts=attempts+1,timeout_streak=0,dead_letter=$4,last_error='http_4xx',next_attempt_at=now()+$3*interval '1 second' WHERE id=$1")
            .bind(id).bind(doc.status.as_str()).bind(crate::queue::backoff(cfg,attempt) as f64).bind(attempt>=cfg.max_failures).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let accepted: Option<Value> = row.try_get("accepted")?;
    // A never-observed object can be missing without having been created or cancelled.
    let before = accepted.filter(|v| v.get("type").is_some() && row.get::<bool, _>("ever_existed"));
    // Both drift guards read the stored row's type path, so a page cannot shed `Event` in the
    // same refresh to slip past either one: a page that lost its start for a fetch (a consent
    // wall, a date that failed to parse) keeps its card. A reading that decided the page is no
    // event, and says why (`not_event`: it lists several events, it is a creative work), is a
    // re-interpretation, not a loss: it replaces the card, type and all.
    let stored_event = row
        .try_get::<Option<Vec<String>>, _>("type_path")?
        .is_some_and(|path| path.iter().any(|t| t == "Event"));
    let decided_no_event =
        doc.event.not_event.is_some() && !doc.type_path.iter().any(|t| t == "Event");
    if doc.status == Status::Ok
        && doc.start.is_none()
        && before.as_ref().is_some_and(|old| old["start"].is_string())
        && stored_event
        && !decided_no_event
    {
        crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
        return Err(crate::resolver::RetryLater {
            class: "drift",
            seconds: 3600,
        }
        .into());
    }
    // The transient container is serde-skipped: copy only the small parent document,
    // without cloning thousands of already-prepared occurrence documents.
    let mut doc: ResourceMeta = serde_json::from_value(serde_json::to_value(doc)?)?;
    // A show's episodes ride with this reading; a stored document retained below carries none.
    doc.episodes = episodes;
    if let Some(plan) = prepared.as_ref().filter(|p| !p.warnings.is_empty()) {
        doc.event.warnings.extend(plan.warnings.iter().cloned());
        doc.event.warnings.sort();
        doc.event.warnings.dedup();
        doc.event.warnings.truncate(16);
    }
    let social_missing = doc.status == Status::Missing
        && before.as_ref().is_some_and(|v| {
            serde_json::from_value::<ResourceMeta>(v.clone())
                .ok()
                .as_ref()
                .is_some_and(crate::resolver::cards::social)
        });
    let app_missing = doc.status == Status::Missing
        && doc.source == "pubky"
        && crate::resolver::pubky_apps::known(&doc.url);
    if social_missing || opted_out {
        sqlx::query("DELETE FROM resource_change WHERE resource_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if social_missing || opted_out {
        // Spellings saved before they became aliases of this post kept its text too.
        scrub_aliases_of(&mut tx, id).await?;
    }
    if social_missing {
        forget_derived(&mut tx, &[id.to_owned()]).await?;
    }
    if doc.status == Status::Missing {
        let fetched = chrono::DateTime::from_timestamp_millis(doc.fetched_at)
            .context("invalid fetch time")?;
        let last: Option<chrono::DateTime<chrono::Utc>> = row.try_get("last_missing_at")?;
        let streak: i32 = row.try_get("missing_streak")?;
        let streak = if crate::resolver::cards::invalid_identifier(&doc.url) || withheld {
            2
        } else if last.is_some_and(|last| last >= fetched) {
            streak
        } else {
            streak + 1
        };
        sqlx::query("UPDATE resource SET missing_streak=$2,last_missing_at=greatest(last_missing_at,$3) WHERE id=$1")
            .bind(id)
            .bind(streak)
            .bind(fetched)
            .execute(&mut *tx)
            .await?;
        if streak < 2 {
            // Record the empty snapshot on this first parent observation too, so the
            // next observation confirms both the parent and its mutable occurrences.
            if let Some(plan) = &prepared {
                if crate::event::storage::apply(&mut tx, id, plan).await? == Some(true) {
                    if plan.exceeds_limit(0) {
                        plan.append_summary(&mut tx, id, None).await?;
                    } else {
                        plan.append_changes(&mut tx, id).await?;
                    }
                }
            }
            // One missing answer is not yet `missing`; a second fresh fetch confirms it. A link
            // with an accepted card keeps it meanwhile and is asked again after
            // `missing_ttl_secs`. One with none yet (a new preview, say) has nothing to keep and
            // shows only `pending`: it is asked again after the queue's first backoff, never an
            // hour later. Either wait is recorded (`missing_unconfirmed`), not charged.
            let retry = if before.is_some() {
                cfg.missing_ttl_secs.max(3600)
            } else {
                cfg.backoff_secs.max(1)
            };
            sqlx::query("UPDATE resource SET lease_until=NULL,priority_at=NULL,attempts=0,timeout_streak=0,last_error='missing_unconfirmed',refresh_at=now()+$2*interval '1 second',next_attempt_at=now()+$2*interval '1 second' WHERE id=$1").bind(id).bind(retry as f64).execute(&mut *tx).await?;
            crate::telemetry::resolution_deferred("missing_unconfirmed");
            tracing::info!(
                resource = %id,
                retry_secs = retry,
                "a missing answer waits for its confirming fetch"
            );
            let retry = retry as f64;
            if source == "osm" {
                sqlx::query("UPDATE osm_place SET refresh_at=least(refresh_at,now()+$3*interval '1 second') WHERE osm_type=$1 AND osm_id=$2").bind(row.try_get::<Option<String>,_>("osm_type")?).bind(row.try_get::<Option<i64>,_>("osm_id")?).bind(retry).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            return Ok(());
        }
        // A Pubky app's object without a card (withheld by its content, gone, or at a path its
        // profile leaves out) keeps nothing of its earlier card, in its document or in its
        // history: an unlisted room or a paused listing shows nothing it used to.
        if app_missing {
            sqlx::query("UPDATE resource_change SET before=NULL,after=NULL WHERE resource_id=$1 AND (before IS NOT NULL OR after IS NOT NULL)")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(before) = before.as_ref().filter(|_| !app_missing) {
            let mut retained = before.clone();
            retained["fetched_at"] = json!(doc.fetched_at);
            retained["ttl_secs"] = json!(doc.ttl_secs);
            retained["status"] = json!("missing");
            doc = serde_json::from_value(retained)?;
            // A gone social post keeps nothing of the post (`gone_card`).
            if social_missing {
                doc = gone_card(&doc);
            }
        }
    } else {
        sqlx::query("UPDATE resource SET missing_streak=0,last_missing_at=NULL,timeout_streak=0 WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    doc.container = None;
    if let Some(plan) = &prepared {
        let applied = crate::event::storage::apply(&mut tx, id, plan).await?;
        if applied != Some(true) {
            sqlx::query("UPDATE resource SET lease_until=NULL,priority_at=NULL,dead_letter=coalesce($3,dead_letter),next_attempt_at=now()+$2*interval '1 second' WHERE id=$1").bind(id).bind(if applied==Some(false){plan.retry_secs as f64}else{doc.ttl_secs.max(3600) as f64}).bind((applied==Some(false)).then_some(plan.dead_letter)).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(());
        }
    }
    if let Some(old) = before
        .as_ref()
        .and_then(|v| serde_json::from_value::<ResourceMeta>(v.clone()).ok())
    {
        let had_start = doc.start.is_some();
        crate::event::storage::preserve_venue(&mut doc, &old);
        // It may re-derive the times under the stored zone, after `bound_instants`.
        crate::event::clear_instants(&mut doc);
        // Agree with the drift guard above, and with the occurrence path, which keeps its stored
        // row: a start this late pass drops never replaces a storable stored start.
        if had_start
            && doc.start.is_none()
            && old.start.and_then(crate::event::time::bounded).is_some()
            && stored_event
            && !decided_no_event
        {
            crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "drift");
            return Err(crate::resolver::RetryLater {
                class: "drift",
                seconds: 3600,
            }
            .into());
        }
    }
    let before = before
        .map(|v| serde_json::from_value::<ResourceMeta>(v).map(|d| normalize(&d)))
        .transpose()?;
    // The last step before the write: nothing stored names a credential but the resource's own
    // link, whatever put it in (this reading, a kept card, a preserved venue).
    crate::capability::strip_document(&mut doc, &own)?;
    let after = normalize(&doc);
    crate::store::write_document(&mut tx, id, generation, &doc).await?;
    if social_missing {
        unqueue_also_on(&mut tx, id).await?;
    }
    let diff = diff(before.as_ref(), Some(&after))
        .filter(|_| !social_missing && (before.is_some() || doc.status != Status::Missing));
    if let Some(plan) = prepared
        .as_ref()
        .filter(|p| p.exceeds_limit(usize::from(diff.is_some())))
    {
        plan.append_summary(&mut tx, id, diff.as_ref()).await?;
    } else {
        if let Some(plan) = prepared {
            plan.append_changes(&mut tx, id).await?;
        }
        if let Some(diff) = diff {
            let (before, after) = if opted_out || app_missing {
                (None, None)
            } else {
                (before, Some(after))
            };
            append(&mut tx, id, &source, before, after, diff, None).await?;
        }
    }
    tx.commit().await?;
    // Grouping is derived work. Its failure must never roll back accepted facts or charge the resolver.
    crate::groups::reconcile(pool, id, &cfg.groups).await;
    Ok(())
}
/// Scrub what rows stored before a scrub ran still keep of posts gone or opted out
/// (`scrubbed`), in batches of 500 roots, as a save would have: a gone post's change lines and
/// also-on results are deleted, its card becomes `gone_card` and it is regrouped; an opted-out
/// post's lines lose their documents (its card stays, as a save leaves it); either one's aliases
/// keep nothing of it (`scrub_spellings`). A post both opted out and gone is gone. A gone post is
/// found by its stored card, also when a failed refresh set its row's status since. Run by
/// `pinakesd reextract`, as a post whose refetch fails never reaches `save`. The posts it
/// changed.
pub async fn scrub_stored(pool: &PgPool) -> Result<u64> {
    let (mut last, mut scrubbed_posts) = (String::new(), 0u64);
    loop {
        let rows: Vec<(String, Value)> = sqlx::query_as("SELECT r.id,r.doc FROM resource r WHERE r.id>$1 AND (r.status='missing' OR r.doc->>'status'='missing' OR r.doc->'attributes'->>'opted_out'='true') AND NOT EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) ORDER BY r.id LIMIT 500")
            .bind(&last)
            .fetch_all(pool)
            .await?;
        let Some((end, _)) = rows.last() else {
            return Ok(scrubbed_posts);
        };
        last = end.clone();
        let scrubbed_doc = |doc: Value| {
            serde_json::from_value::<ResourceMeta>(doc)
                .ok()
                .filter(scrubbed)
        };
        for (id, doc) in rows {
            if scrubbed_doc(doc).is_none() {
                continue;
            }
            let mut tx = pool.begin().await?;
            crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
            crate::alias::lock(&mut tx).await?;
            // Read again under its lock: a save may have revived it since.
            let doc: Option<Value> =
                sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1 FOR UPDATE")
                    .bind(&id)
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some(doc) = doc.and_then(scrubbed_doc) else {
                continue;
            };
            let ids = [id.clone()];
            let mut changed = if !gone_social(&doc) {
                sqlx::query("UPDATE resource_change SET before=NULL,after=NULL WHERE resource_id=$1 AND (before IS NOT NULL OR after IS NOT NULL)")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
            } else {
                let lines = sqlx::query("DELETE FROM resource_change WHERE resource_id=$1")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                let card = gone_card(&doc);
                let kept = serde_json::to_value(&card)?;
                let card = sqlx::query("UPDATE resource SET type=$3,type_path=$4,name=NULL,category=NULL,point=NULL,start_at=NULL,end_at=NULL,venue_id=NULL,doc=$2 WHERE id=$1 AND (doc IS DISTINCT FROM $2 OR name IS NOT NULL OR type IS DISTINCT FROM $3)")
                    .bind(&id)
                    .bind(&kept)
                    .bind(&card.type_name)
                    .bind(&card.type_path)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                let derived = forget_derived(&mut tx, &ids).await?;
                crate::episodes::clear(&mut tx, &id).await?;
                unqueue_also_on(&mut tx, &id).await?;
                if card + derived > 0 {
                    // Its group was formed from the card it no longer has.
                    sqlx::query(
                        "INSERT INTO group_dirty(resource_id) VALUES($1) ON CONFLICT DO NOTHING",
                    )
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                }
                lines + card + derived
            };
            changed += scrub_aliases_of(&mut tx, &id).await?;
            tx.commit().await?;
            scrubbed_posts += u64::from(changed > 0);
        }
    }
}
/// How long one cleanup run deletes for at most: it goes on until a batch comes back short or this
/// runs out, so cleanup keeps up with what the preview cap admits.
const CLEANUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
/// The pause between two cleanup batches, so the writers waiting on the write order get in.
const CLEANUP_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);
/// Delete resources orphaned for `grace_days`. A derived child's grace counts from its link's
/// expiry, not from its creation: a lapsed link (a calendar that failed for a day, an event link
/// past its cap) keeps its child, and its row for a later revival, through the same grace.
pub async fn delete_orphans(pool: &PgPool, grace_days: u32) -> Result<u64> {
    delete_orphans_within(pool, grace_days, grace_days.saturating_mul(24), || false).await
}
/// `delete_orphans`, with a resource nothing ever referenced (one only previewed) kept for
/// `preview_grace_hours` instead, at most `grace_days`. Such a resource still has the orphan time
/// it was created with; a reference clears it, and losing the last reference sets a later one.
/// Between two batches it stops once `stop` says so (the daemon shutting down).
pub async fn delete_orphans_within(
    pool: &PgPool,
    grace_days: u32,
    preview_grace_hours: u32,
    stop: impl Fn() -> bool,
) -> Result<u64> {
    // 100 to a transaction: every other writer waits while one holds the write order exclusively,
    // and goes in during the pause between two.
    let started = std::time::Instant::now();
    let mut deleted = 0;
    loop {
        let batch = delete_orphan_batch(pool, grace_days, preview_grace_hours, 100).await?;
        deleted += batch;
        if batch < 100 || started.elapsed() >= CLEANUP_BUDGET || stop() {
            break;
        }
        tokio::time::sleep(CLEANUP_PAUSE).await;
    }
    Ok(deleted)
}
async fn delete_orphan_batch(
    pool: &PgPool,
    grace_days: u32,
    preview_grace_hours: u32,
    limit: i64,
) -> Result<u64> {
    let mut tx = pool.begin().await?;
    // Orphans are many resources' rows, and deleting them reaches their references, aliases and
    // links.
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Exclusive).await?;
    let rows=sqlx::query("SELECT r.id,r.resolver,r.ever_existed,d.doc FROM resource r JOIN resource_document d ON d.id=r.id WHERE r.orphaned_at<now()-least($1*24,$3)*interval '1 hour' AND (r.orphaned_at=r.created_at OR r.orphaned_at<now()-$1*interval '1 day') AND NOT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id) AND NOT EXISTS(SELECT 1 FROM resource_derivation d JOIN resource p ON p.id=d.parent_id WHERE (d.child_id=r.id OR d.child_id IN(SELECT alias_id FROM resource_alias WHERE root_id=r.id)) AND (d.expires_at IS NULL OR d.expires_at>now()-$1*interval '1 day') AND (p.orphaned_at IS NULL OR p.orphaned_at>=now()-$1*interval '1 day' OR EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=p.id))) AND NOT EXISTS(SELECT 1 FROM resource v WHERE v.venue_id=r.id) AND NOT EXISTS(SELECT 1 FROM occurrence o WHERE o.venue_id=r.id) AND (r.lease_until IS NULL OR r.lease_until<now()) FOR UPDATE OF r SKIP LOCKED LIMIT $2")
        .bind(grace_days as f64).bind(limit).bind(preview_grace_hours as f64).fetch_all(&mut *tx).await?;
    for row in &rows {
        let id: String = row.try_get("id")?;
        let source: Option<String> = row.try_get("resolver")?;
        let doc: Value = row.try_get("doc")?;
        if let Some(doc) = serde_json::from_value::<ResourceMeta>(doc)
            .ok()
            .filter(|_| row.get::<bool, _>("ever_existed"))
        {
            let before = normalize(&doc);
            let change = diff(Some(&before), None).unwrap();
            append(
                &mut tx,
                &id,
                source.as_deref().unwrap_or("unknown"),
                Some(before),
                None,
                change,
                None,
            )
            .await?;
        }
        // What it held (its derived links and their alias roots, its venue, its occurrences'
        // venues) was never a preview: with its derivation and venue rows gone, a held resource
        // nothing referenced keeps the full grace from its creation, not the preview grace.
        sqlx::query("UPDATE resource SET orphaned_at=created_at+interval '1 microsecond' WHERE orphaned_at=created_at AND (id IN (SELECT child_id FROM resource_derivation WHERE parent_id=$1) OR id IN (SELECT a.root_id FROM resource_alias a JOIN resource_derivation d ON d.child_id=a.alias_id WHERE d.parent_id=$1) OR id=(SELECT venue_id FROM resource WHERE id=$1) OR id IN (SELECT venue_id FROM occurrence WHERE resource_id=$1))")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        // Its hint pairs cascade away with it: the recordings and units that named them refill.
        crate::same_recording::before_delete(&mut tx, &id).await?;
        crate::same_event::before_delete(&mut tx, &id).await?;
        sqlx::query("DELETE FROM resource WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(rows.len() as u64)
}
/// Delete change lines older than `days`, and those holding a value of a provider whose values
/// are kept for less (`CreditProvider::change_days_cap`) older than that cap; then the OSM versions and occurrences nothing
/// refers to any more.
pub async fn retain(pool: &PgPool, days: u32) -> Result<()> {
    // The change history is kept to its days even when an occurrence step fails.
    let occurrences = retain_occurrences(pool).await;
    let mut tx = pool.begin().await?;
    lock_sequence(&mut tx).await?;
    // The sequence is the last lock a writer takes, so holding it this never waits for a line a
    // writer holds (an opted-out post deleting its history before it appends): those lines go
    // with that writer, or at the next run.
    sqlx::query("DELETE FROM resource_change WHERE seq IN (SELECT seq FROM resource_change WHERE at<now()-$1*interval '1 day' FOR UPDATE SKIP LOCKED)")
        .bind(days as f64)
        .execute(&mut *tx)
        .await?;
    // A line that holds a value of a provider whose values are kept for less goes after
    // that many days (`CreditProvider::change_days_cap`; TMDB's 120), whatever `days` says and
    // whether or not the provider's source is on now.
    for provider in crate::model::CreditProvider::FROM_FACTS {
        let Some(cap) = provider.change_days_cap().filter(|cap| *cap < days) else {
            continue;
        };
        sqlx::query("DELETE FROM resource_change WHERE seq IN (SELECT seq FROM resource_change r WHERE at<now()-$1*interval '1 day' AND EXISTS(SELECT 1 FROM jsonb_array_elements(CASE jsonb_typeof(r.before->'credits') WHEN 'array' THEN r.before->'credits' ELSE '[]' END || CASE jsonb_typeof(r.after->'credits') WHEN 'array' THEN r.after->'credits' ELSE '[]' END) c WHERE c->>'provider'=$2) FOR UPDATE SKIP LOCKED)")
            .bind(cap as f64)
            .bind(provider.name())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    // Preserve any historical version still referenced by a retained change or accepted resource.
    // Lock candidate rows before inspecting references so a concurrent append cannot lose its snapshot.
    let mut tx = pool.begin().await?;
    let candidates = sqlx::query("SELECT osm_type,osm_id,doc,fetch_doc FROM osm_place WHERE (SELECT count(*) FROM jsonb_object_keys(versions))>1 FOR UPDATE SKIP LOCKED").fetch_all(&mut *tx).await?;
    for row in candidates {
        let kind: String = row.get("osm_type");
        let id: i64 = row.get("osm_id");
        let current = normalize(&serde_json::from_value(row.get("doc"))?);
        let fetched = row
            .get::<Option<Value>, _>("fetch_doc")
            .map(|value| serde_json::from_value(value).map(|doc| normalize(&doc)))
            .transpose()?;
        sqlx::query("WITH kept AS (SELECT coalesce(jsonb_object_agg(v.key,v.value),'{}'::jsonb) AS versions FROM osm_place o CROSS JOIN LATERAL jsonb_each(o.versions) v WHERE o.osm_type=$1 AND o.osm_id=$2 AND (v.key=md5($3::jsonb::text) OR v.key=md5($4::jsonb::text) OR EXISTS(SELECT 1 FROM resource r WHERE r.osm_type=o.osm_type AND r.osm_id=o.osm_id AND r.osm_version=v.key) OR EXISTS(SELECT 1 FROM resource r WHERE r.doc#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND r.doc#>>'{venue,osm_version}'=v.key) OR EXISTS(SELECT 1 FROM occurrence x WHERE x.doc#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND x.doc#>>'{venue,osm_version}'=v.key) OR EXISTS(SELECT 1 FROM resource_change c WHERE (c.before#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND c.before#>>'{venue,osm_version}'=v.key) OR (c.after#>>'{venue,osm_id}'=o.osm_type||'/'||o.osm_id AND c.after#>>'{venue,osm_version}'=v.key)) OR EXISTS(SELECT 1 FROM resource_change c WHERE c.osm_type=o.osm_type AND c.osm_id=o.osm_id AND (c.before->>'version'=v.key OR c.after->>'version'=v.key)))) UPDATE osm_place o SET versions=k.versions FROM kept k WHERE o.osm_type=$1 AND o.osm_id=$2 AND o.versions IS DISTINCT FROM k.versions").bind(kind).bind(id).bind(current).bind(fetched).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    occurrences.map(|_| ())
}
/// Delete expired occurrences one resource at a time, in a save's order: the write order shared,
/// the resource's row, then its occurrences. Their triggers lock the owning resource's row, so
/// one statement over every resource would lock many resources' rows in an order of its own.
async fn retain_occurrences(pool: &PgPool) -> Result<u64> {
    let owners: Vec<String> = sqlx::query_scalar("SELECT DISTINCT o.resource_id FROM occurrence o JOIN event_fetch e ON e.resource_id=o.resource_id WHERE (o.status IN ('cancelled','superseded') OR o.start_at<e.horizon_start) AND coalesce(o.end_at,o.start_at)<now()-e.retention_days*interval '1 day' ORDER BY 1")
        .fetch_all(pool)
        .await?;
    let mut deleted = 0;
    for id in owners {
        let mut tx = pool.begin().await?;
        crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
        let found = sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR UPDATE")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
        if found.is_none() {
            continue;
        }
        deleted += sqlx::query("DELETE FROM occurrence o USING event_fetch e WHERE o.resource_id=$1 AND e.resource_id=$1 AND (o.status IN ('cancelled','superseded') OR o.start_at<e.horizon_start) AND coalesce(o.end_at,o.start_at)<now()-e.retention_days*interval '1 day'")
            .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
    }
    Ok(deleted)
}
#[cfg(test)]
mod tests {
    use super::*;
    /// Every social card, of every type its source gives, has the one gone type once gone and
    /// stays known as a gone social post, so a spelling that joins it later is scrubbed.
    #[test]
    fn a_gone_social_card_has_one_type_and_stays_social() {
        let mut cards: Vec<ResourceMeta> = crate::sources::REGISTRY
            .iter()
            .flat_map(|source| {
                source.examples().iter().map(|example| {
                    let mut doc = ResourceMeta::new(example.uri, source.name(), 3600);
                    doc.set_type(example.card_type);
                    doc
                })
            })
            .collect();
        for (source, type_name) in [
            ("nostr", "Article"),
            ("activitypub", "CreativeWork"),
            ("pubky", "SocialMediaPosting"),
        ] {
            let mut doc = ResourceMeta::new("https://example.social/1", source, 3600);
            doc.set_type(type_name);
            cards.push(doc);
        }
        let mut social = Vec::new();
        for mut doc in cards.into_iter().filter(crate::resolver::cards::social) {
            doc.name = Some("A post".into());
            doc.status = Status::Missing;
            let gone = gone_card(&doc);
            assert_eq!(
                (gone.type_name.as_str(), gone.name.as_deref()),
                (GONE_POST_TYPE, None),
                "{}",
                doc.url
            );
            assert!(
                gone_social(&gone),
                "{} {} once gone",
                doc.source,
                doc.type_name
            );
            social.push(doc.type_name);
        }
        assert!(social.iter().any(|t| t == "VideoObject"), "{social:?}");
    }
    #[test]
    fn normalized_diff_classes_and_fetch_noise() {
        let mut before = ResourceMeta::new("https://example.org", "web", 10);
        before.name = Some("One".into());
        let mut after = before.clone();
        after.fetched_at += 1;
        after.ttl_secs = 99;
        after
            .attributes
            .insert("content_length".into(), "999".into());
        assert!(diff(Some(&normalize(&before)), Some(&normalize(&after))).is_none());
        let base = normalize(&before);
        for (field, value, class) in [
            ("name", json!("Two"), "renamed"),
            ("start", json!("2026-10-15T12:00:00Z"), "rescheduled"),
            ("end", json!("2026-10-15T13:00:00Z"), "rescheduled"),
            ("location", json!({"lat":1,"lon":2}), "relocated"),
            ("status", json!("missing"), "cancelled"),
            ("description", json!("Details"), "details"),
        ] {
            let mut after = base.clone();
            after[field] = value;
            assert_eq!(diff(Some(&base), Some(&after)).unwrap().class, class);
        }
        assert_eq!(diff(None, Some(&base)).unwrap().class, "created");
        assert_eq!(diff(Some(&base), None).unwrap().class, "deleted");
        let mut missing = base.clone();
        missing["status"] = json!("missing");
        assert_eq!(
            diff(Some(&missing), Some(&base)).unwrap().class,
            "uncancelled"
        );
    }
    /// A change line names its occurrence by the UID as it is stored, whichever writer passes
    /// the key: a UID that holds a link carrying a credential is written by its stand-in, a
    /// plain one as it is.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_change_line_names_its_occurrence_by_the_stored_uid(pool: PgPool) {
        let uri = "https://example.org/feed.ics";
        let id = crate::store::ensure_resource(&pool, uri).await.unwrap();
        let secret = "https://cal.example/event?token=SECRET";
        for uid in [secret, "plain-uid"] {
            let key = serde_json::to_string(&(uid, "")).unwrap();
            let mut tx = pool.begin().await.unwrap();
            append(
                &mut tx,
                &id,
                "ics",
                None,
                None,
                Diff {
                    class: "created",
                    fields: vec![],
                },
                Some(&key),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT occurrence_key FROM resource_change WHERE resource_id=$1 ORDER BY seq",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            keys,
            [
                stored_key(&serde_json::to_string(&(secret, "")).unwrap()),
                serde_json::to_string(&("plain-uid", "")).unwrap()
            ]
        );
        assert!(keys[0].starts_with("[\"uid:"), "{keys:?}");
        assert!(!keys.concat().contains("SECRET"), "{keys:?}");
        assert_eq!(stored_key("not a key"), "not a key");
    }
}
