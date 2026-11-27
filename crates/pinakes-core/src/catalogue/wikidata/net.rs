//! What the Wikidata importer asks of Wikimedia: the dump directory, the dump itself (ranges of
//! it, on two connections, or as a stream), its checksums, the recent changes and the changed
//! items.
//!
//! Every request goes to a fixed host (`dumps.wikimedia.org`, `www.wikidata.org`, or the URL an
//! operator names with `pinakesd catalogue import --from`) through the catalogues' client
//! ([`fetch::client`]: Pinakes' User-Agent with the operator's contact, no redirects, no proxy,
//! `Retry-After` waited for), one at a time per kind (the dump's two connections are the one
//! exception Wikimedia allows), and bounded answers; the dump's bytes come through
//! [`bulk::Download`]. A failure is a [`Failure`]: its class and the host, never what an answer
//! said.
use crate::catalogue::{bulk, failure::Failure, fetch};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::time::Duration;

pub use crate::catalogue::bulk::SLOW_WINDOW;
pub use crate::catalogue::fetch::host;

/// Where Wikidata's weekly dumps are listed.
pub const DUMPS: &str = "https://dumps.wikimedia.org/wikidatawiki/entities/";
/// Wikidata's API.
pub const API: &str = "https://www.wikidata.org/w/api.php";
/// The most dated directories looked at for the newest full dump.
const DIRECTORIES: usize = 6;
/// The largest answer read from the API (a `wbgetentities` of 50 large items is a few MB).
pub const API_CAP: usize = 64 << 20;
/// The largest directory page or checksum file read.
const PAGE_CAP: usize = 4 << 20;
/// How often a `maxlag` refusal is waited out before the run fails.
const TRIES: u32 = 6;
/// The least time between two API requests of the importer (short answers never go back to
/// back). In the daemon they also take the egress's turn for `www.wikidata.org`
/// ([`share_egress_pace`]): one request at a time with the resolver's, at its 200 a minute.
const API_INTERVAL: Duration = Duration::from_millis(300);
/// The first wait before a broken dump read is asked for again (it doubles each time).
const RETRY_WAIT: Duration = Duration::from_secs(2);

fn failure(class: &'static str, url: &str) -> Failure {
    Failure::new(class).at(host(url))
}

/// One weekly dump: its date (`YYYYMMDD`), the URL of its JSON file, its size and validator.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dump {
    pub date: String,
    pub url: String,
    pub size: u64,
    /// The `ETag` (or `Last-Modified`) the file was first read with: a file that changed under
    /// a resumed read is read again from its start.
    pub validator: String,
}
impl Dump {
    /// The dump's date as a time: its day's start (UTC). The dump holds the items as they were
    /// while it was written, from that day on, so changes are read again from then.
    pub fn time(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let day = chrono::NaiveDate::parse_from_str(&self.date, "%Y%m%d").ok()?;
        Some(day.and_hms_opt(0, 0, 0)?.and_utc())
    }
    /// The dump's checksum file (`wikidata-<date>-sha1sums.txt` beside it).
    pub fn sums_url(&self) -> Option<String> {
        let (dir, _) = self.url.rsplit_once('/')?;
        Some(format!("{dir}/wikidata-{}-sha1sums.txt", self.date))
    }
    /// The file's name.
    pub fn file_name(&self) -> String {
        format!("wikidata-{}-all.json.bz2", self.date)
    }
}

/// What a HEAD of a dump file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub size: u64,
    /// Its `ETag`, else its `Last-Modified`; empty when it has neither.
    pub validator: String,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}
/// A HEAD of the dump file, or `None` when there is none (404). One without a size is no file
/// (`download`).
pub async fn head(client: &fetch::Client, url: &str) -> Result<Option<Head>> {
    let Some(head) = client.head(url).await? else {
        return Ok(None);
    };
    let size = head
        .size
        .filter(|n| *n > 0)
        .ok_or_else(|| failure("download", url))?;
    Ok(Some(Head {
        size,
        validator: head.validator,
        modified: head.modified,
    }))
}

/// The dated directories a dumps index page lists (`20261123/`), newest first.
pub fn dated_directories(page: &str) -> Vec<String> {
    let mut dates: Vec<String> = page
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter_map(|href| href.strip_suffix('/'))
        .filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
        .filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y%m%d").is_ok())
        .map(str::to_owned)
        .collect();
    dates.sort();
    dates.dedup();
    dates.reverse();
    dates
}

/// The newest dump whose full JSON file is published under `base`.
pub async fn latest_dump(client: &fetch::Client, base: &str) -> Result<Dump> {
    let page = client
        .small(base, PAGE_CAP)
        .await?
        .ok_or_else(|| failure("download", base))?;
    let page = String::from_utf8_lossy(&page);
    for date in dated_directories(&page).into_iter().take(DIRECTORIES) {
        let url = format!(
            "{}/{date}/wikidata-{date}-all.json.bz2",
            base.trim_end_matches('/')
        );
        if let Some(Head {
            size, validator, ..
        }) = head(client, &url).await?
        {
            return Ok(Dump {
                date,
                url,
                size,
                validator,
            });
        }
    }
    Err(failure("no_dump", base).into())
}

/// The SHA-1 a checksum file lists for `file`, if it lists it.
pub fn listed_sha1(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == file
            && sum.len() == 40
            && sum.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| sum.to_ascii_lowercase())
    })
}
/// The SHA-1 the dump's checksum file lists for it; none when the file or the line is missing.
pub async fn dump_sha1(client: &fetch::Client, dump: &Dump) -> Result<Option<String>> {
    let Some(url) = dump.sums_url() else {
        return Ok(None);
    };
    let Some(sums) = client.small(&url, PAGE_CAP).await? else {
        return Ok(None);
    };
    Ok(listed_sha1(
        &String::from_utf8_lossy(&sums),
        &dump.file_name(),
    ))
}

/// The dump's bytes from `from` (to `to`, when given) as a [`bulk::Download`] of the known file
/// (its size and validator): a broken connection is taken up again where it stopped, a file that
/// changed fails `dump_changed`, one gone `dump_gone`, a server slower than 8 MB in `slow` of
/// waiting `download`. Asked for at its first read, on the reading thread.
pub fn stream(
    client: &fetch::Client,
    dump: &Dump,
    from: u64,
    to: Option<u64>,
    slow: Duration,
) -> Result<bulk::Download> {
    let url = url::Url::parse(&dump.url).map_err(|_| failure("cursor", &dump.url))?;
    let download = bulk::Download::new(
        client.clone(),
        tokio::runtime::Handle::current(),
        url,
        dump.size,
    )
    .known(dump.size, &dump.validator)
    .from(from)
    .retry_wait(RETRY_WAIT)
    .slow(slow)
    .no_deadline();
    Ok(match to {
        Some(to) => download.until(to),
        None => download,
    })
}

/// Bytes `from..to` of the dump ([`stream`]), written to `file` at their offset as they come;
/// returns the offset reached (`to` when whole).
pub async fn read_range(
    client: &fetch::Client,
    dump: &Dump,
    file: &std::path::Path,
    from: u64,
    to: u64,
    slow: Duration,
) -> Result<u64> {
    use std::io::{Read as _, Seek as _, Write as _};
    use std::os::unix::fs::OpenOptionsExt as _;
    let disk = |path: &std::path::Path| Failure::new("disk").at(path.display().to_string());
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        // A link put in the file's place is not followed.
        .custom_flags(libc::O_NOFOLLOW)
        .open(file)
        .with_context(|| disk(file))?;
    if dump.validator.is_empty() {
        // Without a validator a file replaced between two ranges would be spliced.
        return Err(failure("download", &dump.url).into());
    }
    let mut body = stream(client, dump, from, Some(to), slow)?;
    let path = file.to_owned();
    tokio::task::spawn_blocking(move || -> Result<u64> {
        out.seek(std::io::SeekFrom::Start(from))
            .with_context(|| disk(&path))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = match body.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    return Err(failure_in(&e)
                        .unwrap_or_else(|| Failure::new("download"))
                        .into())
                }
            };
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).with_context(|| disk(&path))?;
        }
        out.flush().with_context(|| disk(&path))?;
        out.sync_data().with_context(|| disk(&path))?;
        Ok(body.offset())
    })
    .await
    .map_err(|_| Failure::new("panic"))?
}

/// The run's failure a read error holds (a download's), if any.
pub fn failure_in(error: &std::io::Error) -> Option<Failure> {
    error.get_ref()?.downcast_ref::<Failure>().cloned()
}

/// An API answer as JSON: a rate limit, a server error and Wikimedia's `maxlag` refusal are
/// waited out and tried again; another error fails the run (`api`).
pub async fn api(client: &fetch::Client, base: &str, query: &[(&str, &str)]) -> Result<Value> {
    api_capped(client, base, query, API_CAP).await
}
/// [`api`], with answers of at most `cap` bytes (larger: `size`).
pub async fn api_capped(
    client: &fetch::Client,
    base: &str,
    query: &[(&str, &str)],
    cap: usize,
) -> Result<Value> {
    let mut url = url::Url::parse(base).map_err(|_| failure("api", base))?;
    url.query_pairs_mut()
        .extend_pairs(query)
        .append_pair("format", "json")
        .append_pair("formatversion", "2")
        .append_pair("maxlag", "5");
    let url = url.to_string();
    let mut tries = 0;
    loop {
        tries += 1;
        pace().await;
        let read = async {
            // The egress's turn is held for the exchange and its body, never for a wait.
            let (response, turn) = client.send(&url, &[], EGRESS.get()).await?;
            let wait = fetch::retry_after(&response);
            if !response.status().is_success() {
                return Err(anyhow::Error::new(failure("api", &url)));
            }
            let raw = fetch::small_body(response, cap, &url)
                .await?
                .unwrap_or_default();
            drop(turn);
            Ok((wait, raw))
        };
        let (wait, raw) = tokio::time::timeout(ANSWER_DEADLINE, read)
            .await
            .unwrap_or_else(|_| Err(failure("timeout", &url).into()))?;
        let value: Value = serde_json::from_slice(&raw).map_err(|_| failure("api", &url))?;
        if value["error"]["code"] == "maxlag" {
            let wait = wait.unwrap_or(Duration::from_secs(5));
            // Wikidata still lagging after the tries: a wait it asks for, not a fault.
            if wait > fetch::LONGEST_WAIT || tries >= TRIES {
                if wait > fetch::LONGEST_WAIT {
                    fetch::hold(&url, wait);
                }
                return Err(failure("rate_limited", &url).into());
            }
            tracing::info!(host = %crate::resolver::log_host(&url), wait_secs = wait.as_secs(), "Wikidata asks to wait (maxlag); trying again");
            tokio::time::sleep(wait).await;
            continue;
        }
        if value.get("error").is_some() {
            return Err(failure("api", &url).into());
        }
        return Ok(value);
    }
}
/// The longest an API answer may take, all of it.
const ANSWER_DEADLINE: Duration = Duration::from_secs(600);

/// The daemon's egress ([`share_egress_pace`]): the API requests take its turn for their host.
static EGRESS: std::sync::OnceLock<crate::resolver::egress::Egress> = std::sync::OnceLock::new();
/// Let the API requests take `egress`'s turn for their host (`www.wikidata.org`), so they go one
/// at a time with the resolver's and at the pace it keeps for the host. Set once, by the daemon;
/// a by-hand run paces only by [`API_INTERVAL`].
pub fn share_egress_pace(egress: crate::resolver::egress::Egress) {
    let _ = EGRESS.set(egress);
}

/// Waits until [`API_INTERVAL`] has passed since the last API request of this process.
async fn pace() {
    static LAST: tokio::sync::Mutex<Option<tokio::time::Instant>> =
        tokio::sync::Mutex::const_new(None);
    let mut last = LAST.lock().await;
    if let Some(at) = *last {
        tokio::time::sleep_until(at + API_INTERVAL).await;
    }
    *last = Some(tokio::time::Instant::now());
}

/// One change the recent changes list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The item's Q-id (the page title).
    pub qid: String,
    pub at: chrono::DateTime<chrono::Utc>,
}
/// A page of recent changes and where the next one starts (`rccontinue`), if any.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub changes: Vec<Change>,
    pub next: Option<String>,
}
/// The recent changes of items (namespace 0) from `start` to `end`, oldest first: edits, new
/// items and log entries (deletions, restores, merges), 500 a page.
pub async fn recent_changes(
    client: &fetch::Client,
    base: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    next: Option<&str>,
) -> Result<Changes> {
    let start = start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end = end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut query = vec![
        ("action", "query"),
        ("list", "recentchanges"),
        ("rcnamespace", "0"),
        ("rctype", "edit|new|log"),
        ("rcprop", "title|timestamp"),
        ("rcdir", "newer"),
        ("rclimit", "500"),
        ("rcstart", start.as_str()),
        ("rcend", end.as_str()),
    ];
    if let Some(next) = next {
        query.push(("rccontinue", next));
    }
    let value = api(client, base, &query).await?;
    // An answer without its list (warnings only, another shape) is no page: read as an empty
    // last one, the window's changes would be skipped.
    if !value["query"]["recentchanges"].is_array() {
        return Err(failure("api", base).into());
    }
    Ok(parse_changes(&value))
}
/// A `list=recentchanges` answer.
pub fn parse_changes(value: &Value) -> Changes {
    let changes = value["query"]["recentchanges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let qid = crate::hubs::qid(c["title"].as_str()?)?;
            let at = chrono::DateTime::parse_from_rfc3339(c["timestamp"].as_str()?)
                .ok()?
                .with_timezone(&chrono::Utc);
            Some(Change { qid, at })
        })
        .collect();
    let next = value["continue"]["rccontinue"]
        .as_str()
        .filter(|s| s.len() <= 256)
        .map(str::to_owned);
    Changes { changes, next }
}

/// What `wbgetentities` said of the items asked for.
#[derive(Debug, Default)]
pub struct Entities {
    /// The items found, by their own Q-id (a redirected one under the item it now is).
    pub found: Vec<(String, Value)>,
    /// The items asked for that are gone (deleted) or now redirect to another.
    pub gone: Vec<String>,
    /// The items asked for the answer does not speak of (neither found, missing nor redirected):
    /// nothing is known of them.
    pub unaccounted: Vec<String>,
}
/// The props a full read of an item asks for (what a card is made of).
pub const FULL: &str = "info|labels|descriptions|aliases|claims|sitelinks";
/// The props that say whether an item is gone, redirected or has a Wikipedia article: a short
/// answer for any item.
pub const SITELINKS: &str = "info|sitelinks";
/// The items `qids` (at most 50) with `props` ([`FULL`], [`SITELINKS`]).
pub async fn entities(
    client: &fetch::Client,
    base: &str,
    qids: &[String],
    cap: usize,
    props: &str,
) -> Result<Entities> {
    let ids = qids.join("|");
    let value = api_capped(
        client,
        base,
        &[
            ("action", "wbgetentities"),
            ("ids", ids.as_str()),
            ("props", props),
        ],
        cap,
    )
    .await?;
    Ok(parse_entities(value, qids))
}
/// A `wbgetentities` answer for `asked`. An item is gone only when the answer says it is missing
/// or redirected; one it does not speak of is `unaccounted`.
pub fn parse_entities(mut value: Value, asked: &[String]) -> Entities {
    let mut out = Entities::default();
    let asked_set: std::collections::BTreeSet<&str> = asked.iter().map(String::as_str).collect();
    // The items asked for the answer speaks of: found, missing, or redirected to another.
    let mut answered: std::collections::BTreeSet<String> = Default::default();
    let entities = value.get_mut("entities").map(Value::take);
    if let Some(Value::Object(map)) = entities {
        for (key, entity) in map {
            if entity.get("missing").is_some() {
                if asked_set.contains(key.as_str()) {
                    answered.insert(key.clone());
                    out.gone.push(key);
                }
                continue;
            }
            let Some(id) = entity["id"].as_str().and_then(crate::hubs::qid) else {
                continue;
            };
            // An entity without its sitelinks is a degraded answer, not an item that lost its
            // articles: it accounts for nothing (asked again, then the run fails), and its card
            // stays.
            let sitelinks = entity.get("sitelinks");
            if !sitelinks.is_some_and(|l| l.is_object() || l.as_array().is_some_and(Vec::is_empty))
            {
                continue;
            }
            // An item asked for, or the item one asked for now redirects to (listed under its own
            // id with `redirects`, or under the id asked for); nothing else.
            let from = entity["redirects"]["from"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| key.clone());
            if !asked_set.contains(id.as_str()) && !asked_set.contains(from.as_str()) {
                continue;
            }
            if from != id && asked_set.contains(from.as_str()) {
                answered.insert(from.clone());
                out.gone.push(from);
            }
            if asked_set.contains(id.as_str()) {
                answered.insert(id.clone());
            }
            if out.found.iter().all(|(q, _)| *q != id) {
                out.found.push((id, entity));
            }
        }
    }
    out.unaccounted = asked
        .iter()
        .filter(|q| !answered.contains(*q))
        .cloned()
        .collect();
    out.gone.sort();
    out.gone.dedup();
    out
}
/// The English (else `mul`) labels of the items `qids` (at most 50), as tag labels.
pub async fn labels(
    client: &fetch::Client,
    base: &str,
    qids: &[String],
) -> Result<Vec<(String, Option<String>)>> {
    let ids = qids.join("|");
    let value = api(
        client,
        base,
        &[
            ("action", "wbgetentities"),
            ("ids", ids.as_str()),
            ("props", "labels"),
            ("languages", "en|mul"),
        ],
    )
    .await?;
    Ok(parse_labels(&value, qids))
}
/// A `wbgetentities` answer of labels: each item asked for, with its label as a tag label or
/// none (no English label, too long, or the item is gone).
pub fn parse_labels(value: &Value, asked: &[String]) -> Vec<(String, Option<String>)> {
    // By the id asked for: an entity listed under it, or one that says it was redirected from it.
    let mut by_asked: std::collections::BTreeMap<&str, &Value> = Default::default();
    for (key, entity) in value["entities"].as_object().into_iter().flatten() {
        by_asked.insert(key.as_str(), entity);
        if let Some(from) = entity["redirects"]["from"].as_str() {
            by_asked.insert(from, entity);
        }
    }
    asked
        .iter()
        .map(|qid| {
            let label = by_asked.get(qid.as_str()).and_then(|entity| {
                ["en", "mul"]
                    .iter()
                    .find_map(|l| entity["labels"][*l]["value"].as_str())
                    .and_then(crate::catalogue::label)
            });
            (qid.clone(), label)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A range is never written through a link put in the dump file's place.
    #[tokio::test]
    async fn a_range_is_not_written_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.txt");
        std::fs::write(&target, b"keep me").unwrap();
        let link = dir.path().join("wikidata-20261123-all.json.bz2");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dump = Dump {
            date: "20261123".into(),
            url: "https://dumps.wikimedia.org/x".into(),
            size: 10,
            validator: "\"v\"".into(),
        };
        let client = fetch::client("ops@example.org", &[]).unwrap();
        let error = read_range(&client, &dump, &link, 0, 10, SLOW_WINDOW)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Failure>().map(|f| f.class),
            Some("disk")
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
    }

    #[test]
    fn the_newest_dated_directory_comes_first() {
        let page = r#"<a href="../">../</a><a href="20261116/">20261116/</a>
            <a href="20261123/">x</a><a href="latest-all.json.bz2">l</a><a href="20261399/">bad</a>
            <a href="2026112/">short</a><a href="20261120/">y</a><a href="20261123/">again</a>"#;
        assert_eq!(
            dated_directories(page),
            ["20261123", "20261120", "20261116"]
        );
        assert!(dated_directories("<html></html>").is_empty());
    }
    #[test]
    fn checksums_name_their_file() {
        let sums = "eb604708a07d2d6ea2755ec575a209ba609225d6  wikidata-20261123-all.json.gz\n2652AE0ADA0F8A97A6EA6B7D78DC8EADD5AD41FF  wikidata-20261123-all.json.bz2\n";
        assert_eq!(
            listed_sha1(sums, "wikidata-20261123-all.json.bz2").as_deref(),
            Some("2652ae0ada0f8a97a6ea6b7d78dc8eadd5ad41ff")
        );
        assert_eq!(listed_sha1(sums, "wikidata-20261116-all.json.bz2"), None);
        assert_eq!(
            listed_sha1(
                "xyz  wikidata-20261123-all.json.bz2",
                "wikidata-20261123-all.json.bz2"
            ),
            None
        );
        let dump = Dump {
            date: "20261123".into(),
            url: "https://dumps.wikimedia.org/wikidatawiki/entities/20261123/wikidata-20261123-all.json.bz2".into(),
            size: 1,
            validator: String::new(),
        };
        assert_eq!(
            dump.sums_url().as_deref(),
            Some("https://dumps.wikimedia.org/wikidatawiki/entities/20261123/wikidata-20261123-sha1sums.txt")
        );
        assert_eq!(
            dump.time().unwrap().to_rfc3339(),
            "2026-11-23T00:00:00+00:00"
        );
    }
    /// The API's real answer for an item without any sitelink (`fixtures/wikidata`): the item
    /// is found, with no Wikipedia article; an empty list is read the same way.
    #[test]
    fn an_item_without_sitelinks_is_found_without_an_article() {
        let answer: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/wikidata/no-sitelinks.json"
        ))
        .unwrap();
        let asked = ["Q26778405".to_owned()];
        let got = parse_entities(answer.clone(), &asked);
        assert!(got.unaccounted.is_empty() && got.gone.is_empty());
        assert_eq!(got.found.len(), 1);
        assert!(super::super::entity::wikipedia_articles(&got.found[0].1).is_empty());
        let mut listed = answer;
        listed["entities"]["Q26778405"]["sitelinks"] = serde_json::json!([]);
        assert!(parse_entities(listed, &asked).unaccounted.is_empty());
    }
    #[test]
    fn recent_changes_and_entities_are_read_from_their_answers() {
        let page = serde_json::json!({
            "continue": {"rccontinue": "20261125101500|123", "continue": "-||"},
            "query": {"recentchanges": [
                {"type": "edit", "ns": 0, "title": "Q42", "timestamp": "2026-11-25T10:14:59Z"},
                {"type": "log", "ns": 0, "title": "Q0", "timestamp": "2026-11-25T10:15:00Z"},
                {"type": "new", "ns": 0, "title": "Property:P31", "timestamp": "2026-11-25T10:15:00Z"},
                {"type": "new", "ns": 0, "title": "Q7", "timestamp": "not a time"}
            ]}
        });
        let changes = parse_changes(&page);
        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].qid, "Q42");
        assert_eq!(changes.next.as_deref(), Some("20261125101500|123"));
        let answer = serde_json::json!({"entities": {
            "Q1": {"type": "item", "id": "Q1", "sitelinks": {}},
            "Q2": {"id": "Q2", "missing": ""},
            "Q30": {"type": "item", "id": "Q30", "sitelinks": {}, "redirects": {"from": "Q3", "to": "Q30"}},
            "Q99": {"type": "item", "id": "Q99", "sitelinks": {}}
        }});
        let asked = ["Q1".to_owned(), "Q2".to_owned(), "Q3".to_owned()];
        let got = parse_entities(answer, &asked);
        let found: Vec<&str> = got.found.iter().map(|(q, _)| q.as_str()).collect();
        // Q99, which nobody asked for, is not taken.
        assert_eq!(found, ["Q1", "Q30"]);
        assert_eq!(got.gone, ["Q2", "Q3"]);
        assert!(got.unaccounted.is_empty());
        // Items an answer leaves out are unaccounted for, never gone.
        let partial =
            serde_json::json!({"entities": {"Q1": {"type": "item", "id": "Q1", "sitelinks": {}}}});
        let got = parse_entities(partial, &asked);
        assert_eq!(
            (got.gone.len(), got.unaccounted),
            (0, vec!["Q2".to_owned(), "Q3".to_owned()])
        );
        let got = parse_entities(serde_json::json!({"x": 1}), &asked);
        assert_eq!(got.unaccounted, asked);
        // An entity without its sitelinks is no answer for it either.
        let bare = serde_json::json!({"entities": {
            "Q1": {"type": "item", "id": "Q1"},
            "Q2": {"id": "Q2", "missing": ""},
            "Q3": {"type": "item", "id": "Q3", "sitelinks": {}}
        }});
        let got = parse_entities(bare, &asked);
        assert_eq!(got.unaccounted, ["Q1"]);
        assert_eq!(got.gone, ["Q2"]);
        let labels = serde_json::json!({"entities": {
            "Q11399": {"labels": {"en": {"value": "Rock Music"}}},
            "Q9": {"labels": {"mul": {"value": "a label far too long for a tag label"}}}
        }});
        assert_eq!(
            parse_labels(&labels, &["Q11399".into(), "Q9".into(), "Q404".into()]),
            [
                ("Q11399".to_owned(), Some("rock-music".to_owned())),
                ("Q9".to_owned(), None),
                ("Q404".to_owned(), None)
            ]
        );
    }

    /// A JSON value of any shape, a few levels deep.
    fn json() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::from),
            "[A-Za-z0-9|_ :-]{0,12}".prop_map(Value::String),
            prop::sample::select(vec![
                "Q42",
                "Q0",
                "P31",
                "2026-11-25T10:00:00Z",
                "missing",
                "entities",
                "redirects"
            ])
            .prop_map(|s| Value::String(s.into())),
        ];
        leaf.prop_recursive(4, 64, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                prop::collection::btree_map(
                    prop::sample::select(vec![
                        "entities",
                        "query",
                        "recentchanges",
                        "continue",
                        "rccontinue",
                        "title",
                        "timestamp",
                        "id",
                        "labels",
                        "en",
                        "mul",
                        "value",
                        "redirects",
                        "from",
                        "missing",
                        "Q42",
                        "Q1"
                    ])
                    .prop_map(str::to_owned),
                    inner,
                    0..6
                )
                .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 2000, ..Default::default() })]
        /// Fuzz: API answers of any shape and pages of any text are read without a panic, and
        /// what they give is well-formed (Q-ids, an answer only for the items asked for).
        #[test]
        fn answers_of_any_shape_are_read_without_a_panic(value in json(), page in "\\PC{0,200}", sums in "[0-9a-f ]{0,80}\n?[a-z0-9.-]{0,40}") {
            let changes = parse_changes(&value);
            for c in &changes.changes {
                proptest::prop_assert!(crate::hubs::qid(&c.qid).is_some());
            }
            let asked = vec!["Q42".to_owned(), "Q1".to_owned()];
            let got = parse_entities(value.clone(), &asked);
            proptest::prop_assert!(got.found.len() <= 2 + asked.len());
            proptest::prop_assert!(got.gone.iter().all(|q| asked.contains(q)));
            proptest::prop_assert!(got.unaccounted.iter().all(|q| asked.contains(q) && !got.gone.contains(q)));
            let labels = parse_labels(&value, &asked);
            proptest::prop_assert_eq!(labels.len(), 2);
            for (_, l) in labels.iter().filter_map(|(q, l)| l.as_ref().map(|l| (q, l))) {
                let made = crate::catalogue::label(l);
                proptest::prop_assert_eq!(made.as_deref(), Some(l.as_str()));
            }
            for d in dated_directories(&page) {
                proptest::prop_assert!(d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()));
            }
            if let Some(sum) = listed_sha1(&sums, "x.json.bz2") {
                proptest::prop_assert_eq!(sum.len(), 40);
            }
        }
    }
}
