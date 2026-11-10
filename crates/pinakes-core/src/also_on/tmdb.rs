//! TMDB (The Movie Database): a film's or series' poster and overview, by the title's exact IDs,
//! for a card that has no image or description of its own.
//!
//! A typed card (`Movie` or `TVSeries`) is looked up by its IMDb title ID (an `imdb:` key, which
//! `films` finds for an IMDb, Netflix, Trakt or other film link) and its Wikidata item (a
//! `wikidata:` key, or the card's own item link), each with TMDB's `GET /3/find/{id}`
//! (`external_source=imdb_id` or `wikidata_id`); never by a title. An answer names a title only
//! when it holds exactly one movie or series; two IDs that name different titles give nothing.
//! What TMDB says is recorded as facts (`tmdb_title`, `movie/{id}` or `tv/{id}`; `tmdb_poster`;
//! `tmdb_overview`), and the card takes them when it is typed ([`Tmdb::fill_card`]): the poster
//! as `image` when it has none, the overview as `description` when it has none, each credited to
//! TMDB (`credits[]`, provider `tmdb`), and only when its type agrees (a `Movie` card a movie, a
//! `TVSeries` card a series).
//!
//! TMDB's terms let its data be cached for six months at most. Its facts are read again with the
//! source's lifetime (30 days) and expire 60 days after the last read that answered, so a card
//! never holds a TMDB value read longer ago than that: a transient failure keeps them until then,
//! the card's next reading drops them after, and the maintenance pass
//! (`also_on::refill`) drops them from cards that are not read again. A card's value is read
//! from the facts, never kept from an earlier reading (`ResourceMeta::keep_enriched`).
//!
//! Asked only with the operator's credential (`TMDB_API_KEY`): without it, or with
//! `[tmdb] enabled = false`, the source is switched off (its facts hidden, `api.themoviedb.org`
//! refused) and `/v1/info` names no TMDB attribution.
use super::{Attribution, Context, Fact, Found, Source, Subject};
use crate::{
    event::adapters::SecretKey,
    model::{Credit, CreditField, CreditProvider, ResourceMeta},
    resolver::egress::FetchOptions,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const API: &str = "https://api.themoviedb.org/3";
/// TMDB's image host and path (`images.secure_base_url` of `GET /3/configuration`).
const IMAGES: &str = "https://image.tmdb.org/t/p/";
/// The poster size a card shows, one of TMDB's `poster_sizes` (`w92` … `w780`, `original`):
/// 500 pixels wide, near the width of the Commons thumbnails cards show
/// (`limits.music_image_width`, 640), where `original` may be several megabytes.
const POSTER_SIZE: &str = "w500";
/// How long a TMDB answer stays in the egress cache (seconds): one hour.
const CACHE_SECS: u64 = 3600;
/// The notice TMDB's terms ask an application to show, verbatim.
pub const NOTICE: &str = "This product uses the TMDB API but is not endorsed or certified by TMDB.";
/// TMDB's logo (its logos and attribution page).
pub const LOGO: &str = "https://www.themoviedb.org/assets/2/v4/logos/v2/blue_short-8e7b30f73a4020692ccca9c88bafe5dcb6f8a62a4c6bc55cd9ba82bb2cd95f6c.svg";
const SITE: &str = "https://www.themoviedb.org";
/// The facts a card takes.
const FACTS: [&str; 4] = [
    "tmdb_title",
    "tmdb_poster",
    "tmdb_overview",
    "tmdb_overview_cut",
];
/// The longest `refresh.retention_days` while TMDB is on: change lines keep a card's TMDB values
/// that long, and a value is at most 60 days old when it leaves the card.
pub const RETENTION_DAYS_CAP: u32 = 120;
/// The longest overview a fact holds (bytes).
const FACT_CAP: usize = 512;

/// `[tmdb]`: the TMDB credential (`TMDB_API_KEY`) and a switch. TMDB is asked only with the
/// credential and while `enabled`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TmdbConfig {
    pub enabled: bool,
    /// An API Read Access Token (recommended, sent as `Authorization: Bearer`) or a v3 API key
    /// (32 hexadecimal digits, sent as `api_key=` on the wire alone).
    pub api_key: Option<SecretKey>,
}
impl Default for TmdbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key: None,
        }
    }
}
/// A TMDB credential, by its form.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// An API Read Access Token: a JSON Web Token.
    Token(String),
    /// A v3 API key: 32 hexadecimal digits.
    Key(String),
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Token(_) => "Token([REDACTED])",
            Self::Key(_) => "Key([REDACTED])",
        })
    }
}
impl Credential {
    /// The credential `value` is, by its form; `None` for an empty value or another form.
    pub fn of(value: &str) -> Option<Self> {
        let value = value.trim();
        let base64url = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        let parts: Vec<&str> = value.split('.').collect();
        if parts.len() == 3 && value.starts_with("eyJ") && parts.iter().all(|p| base64url(p)) {
            Some(Self::Token(value.to_owned()))
        } else if value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(Self::Key(value.to_owned()))
        } else {
            None
        }
    }
    /// A request's options with this credential: a token as a header, a key in the query on
    /// the wire alone. The answers are cached by the URL alone: a new credential keeps them.
    pub fn options(&self) -> FetchOptions {
        let mut options = FetchOptions {
            accept: Some("application/json".into()),
            sensitive: true,
            cache_tag: Some("tmdb".into()),
            // A refused credential is the credential's, not the URL's answer.
            uncached: &[401],
            ..Default::default()
        };
        match self {
            Self::Token(token) => options
                .headers
                .push(("authorization", format!("Bearer {token}"))),
            Self::Key(key) => options.query_secret = Some(("api_key", key.clone())),
        }
        options
    }
}
impl TmdbConfig {
    /// The credential TMDB is asked with, while it is on.
    pub fn credential(&self) -> Option<Credential> {
        self.enabled
            .then(|| self.api_key.as_deref().and_then(Credential::of))
            .flatten()
    }
}
impl super::OwnSection for TmdbConfig {
    const NAME: &'static str = "tmdb";
    const VARIABLES: &'static [(&'static str, &'static str)] = &[("TMDB_API_KEY", "api_key")];
    fn check(&self) -> Result<()> {
        // Never names the value.
        anyhow::ensure!(
            self.api_key
                .as_deref()
                .is_none_or(|k| k.trim().is_empty() || Credential::of(k).is_some()),
            "tmdb.api_key (TMDB_API_KEY) is neither a TMDB API Read Access Token nor a v3 API key"
        );
        Ok(())
    }
}

/// A movie or a series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Movie,
    Tv,
}
impl Kind {
    fn path(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Tv => "tv",
        }
    }
    /// The card type that takes a title of this kind.
    fn card_type(self) -> &'static str {
        match self {
            Self::Movie => "Movie",
            Self::Tv => "TVSeries",
        }
    }
}
/// One title as TMDB's answer names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Title {
    pub kind: Kind,
    pub id: u64,
    /// The poster's path (`/abc.jpg`).
    pub poster: Option<String>,
    pub overview: Option<String>,
}
/// The request for the title with this external ID (`imdb_id` or `wikidata_id`).
pub fn find_url(id: &str, source: &str) -> String {
    format!("{API}/find/{id}?external_source={source}")
}
/// A poster path as TMDB gives it: `/`, then a file name of letters, digits, `_`, `-` and `.`.
fn poster_path(v: &str) -> bool {
    v.strip_prefix('/').is_some_and(|name| {
        (1..=100).contains(&name.len())
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    })
}
/// The one title a `/find` answer names: exactly one entry in `movie_results` and `tv_results`
/// together; `None` for none or several.
pub fn parse_find(v: &Value) -> Option<Title> {
    let list = |name: &str| v[name].as_array().map(Vec::as_slice).unwrap_or_default();
    let (kind, item) = match (list("movie_results"), list("tv_results")) {
        ([one], []) => (Kind::Movie, one),
        ([], [one]) => (Kind::Tv, one),
        _ => return None,
    };
    let overview = item["overview"]
        .as_str()
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());
    Some(Title {
        kind,
        id: item["id"].as_u64().filter(|id| *id > 0)?,
        poster: item["poster_path"]
            .as_str()
            .filter(|p| poster_path(p))
            .map(str::to_owned),
        overview,
    })
}
/// A title's facts: what a card takes ([`Tmdb::fill_card`]). An overview longer than a fact
/// holds is cut at a sentence or a word, and says so (`tmdb_overview_cut`).
pub fn facts(title: &Title) -> Vec<Fact> {
    let mut facts = vec![Fact::new(
        "tmdb_title",
        format!("{}/{}", title.kind.path(), title.id),
    )];
    facts.extend(
        title
            .poster
            .as_ref()
            .map(|p| Fact::new("tmdb_poster", p.as_str())),
    );
    if let Some(overview) = &title.overview {
        let (text, cut) = crate::music_enrichment::excerpt(overview, FACT_CAP);
        facts.push(Fact::new("tmdb_overview", text));
        if cut {
            facts.push(Fact::new("tmdb_overview_cut", "true"));
        }
    }
    facts
}
/// The title its facts name, `None` when they name none or several.
fn from_facts(facts: &[Fact]) -> Option<(Title, bool)> {
    let one = |name: &str| -> Option<&str> {
        let mut values = facts.iter().filter(|f| f.name == name);
        match (values.next(), values.next()) {
            (Some(f), None) => Some(f.value.as_str()),
            _ => None,
        }
    };
    let (kind, id) = one("tmdb_title")?.split_once('/')?;
    let kind = match kind {
        "movie" => Kind::Movie,
        "tv" => Kind::Tv,
        _ => return None,
    };
    let id = id.parse().ok().filter(|id| *id > 0)?;
    Some((
        Title {
            kind,
            id,
            poster: one("tmdb_poster")
                .filter(|p| poster_path(p))
                .map(str::to_owned),
            overview: one("tmdb_overview").map(str::to_owned),
        },
        one("tmdb_overview_cut").is_some(),
    ))
}

/// The IMDb title ID and the Wikidata item a subject is looked up by: each the one of its
/// kind, `Err` when the subject has several of a kind (no one title).
#[allow(clippy::type_complexity)]
fn ids(subject: &Subject) -> Result<(Option<String>, Option<String>), ()> {
    let one = |mut values: Vec<String>| {
        values.sort();
        values.dedup();
        match values.len() {
            0 => Ok(None),
            1 => Ok(values.pop()),
            _ => Err(()),
        }
    };
    let imdb = one(subject
        .keys_in("imdb")
        .filter(|tt| super::media::imdb(tt))
        .map(str::to_owned)
        .collect())?;
    let qid = one(subject
        .keys_in("wikidata")
        .filter_map(crate::hubs::qid)
        .chain(crate::hubs::wikidata::item_link(&subject.uri).map(|(qid, _)| qid))
        .collect())?;
    Ok((imdb, qid))
}
/// Whether a card holds a value of its own in `field`: one TMDB did not give.
fn own(doc: &ResourceMeta, field: CreditField) -> bool {
    let value = match field {
        CreditField::Image => doc.image.as_deref(),
        CreditField::Description => doc.description.as_deref(),
    };
    value.is_some_and(|v| !v.trim().is_empty())
        && !doc
            .credits
            .iter()
            .any(|c| c.field == field && c.provider == CreditProvider::Tmdb)
}
/// Whether a link is on TMDB itself: its card is TMDB's own page, never filled from TMDB's API.
fn on_tmdb(uri: &str) -> bool {
    url::Url::parse(uri).is_ok_and(|u| {
        u.host_str()
            .is_some_and(|h| h == "themoviedb.org" || h.ends_with(".themoviedb.org"))
    })
}

/// The source.
pub struct Tmdb;

impl Tmdb {
    /// The one title TMDB names for `id`; an error is a transient failure (a 429 or 5xx is a
    /// retry) or a refused credential.
    async fn find(
        cx: &Context,
        credential: &Credential,
        id: &str,
        source: &str,
    ) -> Result<Option<Title>> {
        let fetched = cx
            .egress
            .discover(&find_url(id, source), &credential.options(), CACHE_SECS)
            .await?;
        match fetched.status {
            401 => {
                crate::telemetry::also_on_service_error("tmdb", "refused");
                tracing::warn!("TMDB refused the credential");
                anyhow::bail!("TMDB refused the credential")
            }
            status => Ok(super::media::json(status, &fetched.body, &[404])?
                .as_ref()
                .and_then(parse_find)),
        }
    }
}

#[async_trait]
impl Source for Tmdb {
    fn id(&self) -> &'static str {
        "tmdb"
    }
    fn section(&self) -> Option<super::Section> {
        Some(super::Section::of::<TmdbConfig>())
    }
    fn ready(&self, config: &crate::config::Config) -> bool {
        config.section::<TmdbConfig>().credential().is_some()
    }
    fn off_until_ready(&self) -> bool {
        true
    }
    fn budgets(&self) -> &'static [(&'static str, f64)] {
        // TMDB's upper limit sits "somewhere in the 40 requests per second range"
        // (developer.themoviedb.org/docs/rate-limiting): at most a tenth of it (the default
        // `limits.host_requests_per_second`, 1/s, stays lower).
        &[("api.themoviedb.org", 4.0)]
    }
    fn api_hosts(&self) -> &'static [&'static str] {
        &["api.themoviedb.org"]
    }
    fn fills(&self) -> &'static [&'static str] {
        &FACTS
    }
    fn retention_days_cap(&self) -> Option<u32> {
        // A value read up to 60 days ago, kept 120 more: six months.
        Some(RETENTION_DAYS_CAP)
    }
    fn fills_as(&self) -> Option<CreditProvider> {
        Some(CreditProvider::Tmdb)
    }
    fn attribution(&self) -> Option<Attribution> {
        Some(Attribution {
            notice: NOTICE,
            logo: LOGO,
            url: SITE,
        })
    }
    /// A resolved film or series card with an IMDb ID or a Wikidata item, and an image or a
    /// description to fill (one TMDB gave counts as none); never a card of a TMDB page.
    fn applies(&self, subject: &Subject) -> bool {
        let Some(doc) = &subject.doc else {
            return false;
        };
        subject.status == "ok"
            && !on_tmdb(&subject.uri)
            && CreditProvider::Tmdb
                .filled_types()
                .contains(&doc.type_name.as_str())
            && !(own(doc, CreditField::Image) && own(doc, CreditField::Description))
            && matches!(ids(subject), Ok((Some(_), _) | (_, Some(_))))
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(credential) = cx.config.section::<TmdbConfig>().credential() else {
            return Ok(Found::default());
        };
        let Ok((imdb, qid)) = ids(subject) else {
            return Ok(Found::default());
        };
        let mut titles = vec![];
        if let Some(tt) = &imdb {
            titles.push(Self::find(cx, &credential, tt, "imdb_id").await?);
        }
        if let Some(qid) = &qid {
            titles.push(Self::find(cx, &credential, qid, "wikidata_id").await?);
        }
        let mut named = titles.into_iter().flatten();
        let title = match (named.next(), named.next()) {
            (Some(one), None) => one,
            // Two IDs, one title.
            (Some(one), Some(two)) if (one.kind, one.id) == (two.kind, two.id) => one,
            _ => return Ok(Found::default()),
        };
        // A title of another type is no answer: nothing of it is kept.
        if subject.doc.as_ref().map(|d| d.type_name.as_str()) != Some(title.kind.card_type()) {
            return Ok(Found::default());
        }
        let facts = facts(&title);
        // The card is read again now when what it would take changed.
        let reread = subject.doc.as_ref().is_some_and(|doc| {
            let mut after = doc.clone();
            self.fill_card(&facts, &mut after);
            (&after.image, &after.description, &after.credits)
                != (&doc.image, &doc.description, &doc.credits)
        });
        Ok(Found {
            facts,
            reread,
            ..Default::default()
        })
    }
    fn fill_card(&self, facts: &[Fact], doc: &mut ResourceMeta) -> bool {
        // What TMDB gave before goes first.
        for credit in doc
            .credits
            .iter()
            .filter(|c| c.provider == CreditProvider::Tmdb)
        {
            match credit.field {
                CreditField::Image => doc.image = None,
                CreditField::Description => doc.description = None,
            }
        }
        doc.credits.retain(|c| c.provider != CreditProvider::Tmdb);
        let Some((title, cut)) = from_facts(facts) else {
            return false;
        };
        if doc.type_name != title.kind.card_type() {
            return false;
        }
        let credit = |field| Credit {
            field,
            provider: CreditProvider::Tmdb,
            source_url: format!("{SITE}/{}/{}", title.kind.path(), title.id),
            author: None,
            license: String::new(),
            license_url: None,
            lang: (field == CreditField::Description).then(|| "en".to_owned()),
            excerpted: field == CreditField::Description && cut,
            modified: false,
            subject: None,
        };
        let mut wrote = false;
        if !own(doc, CreditField::Image) {
            if let (Some(path), Some(credit)) =
                (&title.poster, credit(CreditField::Image).bounded())
            {
                doc.image = Some(format!("{IMAGES}{POSTER_SIZE}{path}"));
                doc.credits.retain(|c| c.field != CreditField::Image);
                doc.credits.push(credit);
                wrote = true;
            }
        }
        if !own(doc, CreditField::Description) {
            let text = title
                .overview
                .filter(|t| !crate::capability::contains_credential(t));
            if let (Some(text), Some(credit)) = (text, credit(CreditField::Description).bounded()) {
                doc.description = Some(text);
                doc.credits.retain(|c| c.field != CreditField::Description);
                doc.credits.push(credit);
                wrote = true;
            }
        }
        doc.credits.sort_by_key(|c| c.field);
        wrote
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_is_known_by_its_form_and_never_printed() {
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJhdWQiOiJ0ZXN0In0.c2lnbmF0dXJl";
        let key = "0123456789abcdef0123456789ABCDEF";
        assert!(matches!(Credential::of(token), Some(Credential::Token(_))));
        assert!(matches!(Credential::of(key), Some(Credential::Key(_))));
        for other in ["", "  ", "abc", "eyJ.only", &key[1..], "eyJa.b.c d"] {
            assert_eq!(Credential::of(other), None, "{other}");
        }
        for credential in [Credential::of(token).unwrap(), Credential::of(key).unwrap()] {
            let shown = format!("{credential:?} {:?}", credential.options());
            assert!(!shown.contains(token) && !shown.contains(key), "{shown}");
        }
        // A token is a header; a key rides in the query on the wire alone.
        let options = Credential::of(token).unwrap().options();
        assert_eq!(
            options.headers,
            [("authorization", format!("Bearer {token}"))]
        );
        assert!(options.query_secret.is_none() && options.sensitive);
        let options = Credential::of(key).unwrap().options();
        assert!(options.headers.is_empty() && options.sensitive);
        let url = url::Url::parse(&find_url("tt0816692", "imdb_id")).unwrap();
        let wire = crate::resolver::egress::wire_url(&url, &options);
        assert_eq!(
            wire.as_str(),
            format!("{}&api_key={key}", find_url("tt0816692", "imdb_id"))
        );
        assert!(!url.as_str().contains(key));
        // Off, or a value of neither form: no credential.
        let config = |enabled, key: &str| TmdbConfig {
            enabled,
            api_key: Some(key.into()),
        };
        assert!(config(true, token).credential().is_some());
        assert!(config(false, token).credential().is_none());
        assert!(config(true, "").credential().is_none());
        use crate::also_on::OwnSection;
        assert!(config(true, "").check().is_ok());
        let refused = config(true, "not-a-key").check().unwrap_err().to_string();
        assert!(!refused.contains("not-a-key"), "{refused}");
    }

    #[test]
    fn an_answer_names_one_title_or_none() {
        let movie = serde_json::json!({"id": 157336, "title": "Interstellar", "poster_path": "/gEU2QniE6E77NI6lCU6MxlNBvIx.jpg", "overview": " The adventures of\n a group. "});
        let tv = serde_json::json!({"id": 1396, "name": "Breaking Bad", "poster_path": "/ztkUQFLlC19CCMYHW9o1zWhJRNq.jpg", "overview": ""});
        let answer = |m: Vec<Value>, t: Vec<Value>| serde_json::json!({"movie_results": m, "tv_results": t, "person_results": [], "tv_episode_results": [], "tv_season_results": []});
        assert_eq!(
            parse_find(&answer(vec![movie.clone()], vec![])),
            Some(Title {
                kind: Kind::Movie,
                id: 157336,
                poster: Some("/gEU2QniE6E77NI6lCU6MxlNBvIx.jpg".into()),
                overview: Some("The adventures of a group.".into()),
            })
        );
        let series = parse_find(&answer(vec![], vec![tv.clone()])).unwrap();
        assert_eq!((series.kind, series.overview), (Kind::Tv, None));
        for none in [
            answer(vec![], vec![]),
            answer(vec![movie.clone(), movie.clone()], vec![]),
            answer(vec![movie.clone()], vec![tv.clone()]),
            serde_json::json!({}),
        ] {
            assert_eq!(parse_find(&none), None, "{none}");
        }
        // A poster path that is not a file name is none.
        for path in ["x.jpg", "/../x.jpg", "/a/b.jpg", "/", "/x.jpg?sig=1"] {
            let odd = serde_json::json!({"id": 1, "poster_path": path});
            assert_eq!(
                parse_find(&answer(vec![odd], vec![])).unwrap().poster,
                None,
                "{path}"
            );
        }
        // Facts and back.
        let title = parse_find(&answer(vec![movie], vec![])).unwrap();
        assert_eq!(from_facts(&facts(&title)), Some((title, false)));
        let long = Title {
            kind: Kind::Tv,
            id: 1,
            poster: None,
            overview: Some("A sentence. ".repeat(80).trim().to_owned()),
        };
        let facts = facts(&long);
        let (back, cut) = from_facts(&facts).unwrap();
        assert!(cut && back.overview.unwrap().len() <= FACT_CAP);
    }

    fn card(kind: &str) -> ResourceMeta {
        let mut doc = ResourceMeta::new("https://www.netflix.com/title/70305903", "web", 60);
        doc.set_type(kind);
        doc
    }
    fn found() -> Vec<Fact> {
        facts(&Title {
            kind: Kind::Movie,
            id: 157336,
            poster: Some("/p.jpg".into()),
            overview: Some("A film.".into()),
        })
    }

    /// A card takes the poster and the overview when it has none of its own, credited to TMDB,
    /// and only a title of its type; a second fill gives the same card, and a fill without facts
    /// takes back what TMDB gave.
    #[test]
    fn a_card_takes_what_it_lacks_of_its_own_type() {
        let mut doc = card("Movie");
        assert!(Tmdb.fill_card(&found(), &mut doc));
        assert_eq!(
            doc.image.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/p.jpg")
        );
        assert_eq!(doc.description.as_deref(), Some("A film."));
        assert_eq!(
            serde_json::to_value(&doc.credits).unwrap(),
            serde_json::json!([
                {"field": "image", "provider": "tmdb", "source_url": "https://www.themoviedb.org/movie/157336", "author": null, "license": null, "license_url": null, "lang": null, "excerpted": false, "modified": false, "subject": null},
                {"field": "description", "provider": "tmdb", "source_url": "https://www.themoviedb.org/movie/157336", "author": null, "license": null, "license_url": null, "lang": "en", "excerpted": false, "modified": false, "subject": null},
            ])
        );
        let again = doc.clone();
        assert!(Tmdb.fill_card(&found(), &mut doc));
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            serde_json::to_value(&again).unwrap()
        );
        assert!(!Tmdb.fill_card(&[], &mut doc));
        assert_eq!(
            (doc.image, doc.description, doc.credits),
            (None, None, vec![])
        );
        // Its own image and text stay.
        let mut doc = card("Movie");
        doc.image = Some("https://example.org/own.jpg".into());
        doc.description = Some("Own text.".into());
        assert!(!Tmdb.fill_card(&found(), &mut doc));
        assert_eq!(doc.image.as_deref(), Some("https://example.org/own.jpg"));
        assert!(doc.credits.is_empty());
        // Another type takes nothing.
        let mut doc = card("TVSeries");
        assert!(!Tmdb.fill_card(&found(), &mut doc));
        assert_eq!((doc.image, doc.credits), (None, vec![]));
    }
}
