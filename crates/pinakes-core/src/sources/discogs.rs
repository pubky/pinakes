//! Discogs artists, masters (an album across its editions) and releases (one edition), read from
//! its API (`api.discogs.com`): its web pages answer bots with a wall. No key: 25 requests a
//! minute. An optional personal token (`[also_on.sources.discogs] token`), or an app's consumer
//! key and secret (`key` and `secret`, or `PINAKES_DISCOGS_CONSUMER_KEY` and
//! `PINAKES_DISCOGS_CONSUMER_SECRET`), gives 60 a minute and images on the cards; the token wins
//! when both are set. Neither ever reaches a log or an error.
use super::{text, Budget, Context, Example, Source};
use crate::event::adapters::SecretKey;
use crate::{
    model::ResourceMeta,
    resolver::{
        egress::{Egress, FetchOptions},
        RetryLater,
    },
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::RwLock;

pub struct Discogs;

const API: &str = "https://api.discogs.com";

pub use crate::music::discogs::{of_link, of_uri, root, Kind};

/// The credential the daemon started with (`configure`).
static CREDENTIAL: RwLock<Option<Credential>> = RwLock::new(None);

/// Record `[also_on.sources.discogs]`'s credential for the card route.
fn configure(config: &crate::config::Config) {
    *CREDENTIAL.write().unwrap_or_else(|e| e.into_inner()) = credential(config);
}
/// How Discogs is asked with credentials: a personal token, or an app's consumer key and secret.
/// Either gives 60 requests a minute and images.
#[derive(Clone)]
pub enum Credential {
    Token(SecretKey),
    App { key: SecretKey, secret: SecretKey },
}
impl Credential {
    /// The `Authorization` header Discogs reads.
    fn header(&self) -> String {
        match self {
            Self::Token(token) => format!("Discogs token={}", &**token),
            Self::App { key, secret } => format!("Discogs key={}, secret={}", &**key, &**secret),
        }
    }
}
/// The configured credential: the token when it is set, else the key and secret when both are.
pub fn credential(config: &crate::config::Config) -> Option<Credential> {
    let options = config.also_on.sources.get("discogs")?;
    let set = |v: &Option<SecretKey>| v.clone().filter(|v| !v.trim().is_empty());
    match (set(&options.token), set(&options.key), set(&options.secret)) {
        (Some(token), _, _) => Some(Credential::Token(token)),
        (None, Some(key), Some(secret)) => Some(Credential::App { key, secret }),
        _ => None,
    }
}

/// Discogs' record of one object, `None` when it has none, and whether the credential was used.
/// The answer is shared for an hour, so the card and the also-on lookup ask once. A credential
/// Discogs refuses (401, 403: expired, revoked or mistyped) is dropped, with one warning: that
/// request is asked again without it, and later ones go without it from the start, so cards
/// still work without images.
pub async fn record(
    egress: &Egress,
    kind: Kind,
    id: u64,
    credential: Option<&Credential>,
) -> Result<(Option<Value>, bool)> {
    let header = credential.map(Credential::header).filter(|h| !refused(h));
    let mut f = fetch(egress, kind, id, header.as_deref()).await?;
    let mut authenticated = header.is_some();
    if let (Some(header), 401 | 403) = (header.as_deref(), f.status) {
        super::token_refused("discogs");
        if refuse(header) {
            tracing::warn!(
                status = f.status,
                "Discogs refused the configured credential; reading without it (no images)"
            );
        }
        f = fetch(egress, kind, id, None).await?;
        authenticated = false;
    }
    match f.status {
        200 => Ok((Some(serde_json::from_slice(&f.body)?), authenticated)),
        404 => Ok((None, authenticated)),
        408 | 429 | 500..=599 => Err(RetryLater {
            class: "upstream",
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into()),
        status => anyhow::bail!("Discogs answered {status}"),
    }
}
/// The credentials Discogs has refused since the daemon started, by the hash of their header
/// (never the credential itself).
static REFUSED: std::sync::Mutex<Vec<[u8; 32]>> = std::sync::Mutex::new(Vec::new());
fn refused(header: &str) -> bool {
    let hash = *blake3::hash(header.as_bytes()).as_bytes();
    REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&hash)
}
/// Remember a refused credential; true the first time.
fn refuse(header: &str) -> bool {
    let hash = *blake3::hash(header.as_bytes()).as_bytes();
    let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    !refused.contains(&hash) && {
        refused.push(hash);
        true
    }
}
async fn fetch(
    egress: &Egress,
    kind: Kind,
    id: u64,
    header: Option<&str>,
) -> Result<crate::resolver::egress::Fetched> {
    let options = FetchOptions {
        accept: Some("application/json".into()),
        headers: header
            .map(|h| vec![("authorization", h.to_owned())])
            .unwrap_or_default(),
        sensitive: header.is_some(),
        ..Default::default()
    };
    let url = format!("{API}/{}s/{id}", kind.as_str());
    egress.discover(&url, &options, 3600).await
}

/// A name without Discogs' number for namesakes (`Radiohead (2)`).
fn plain_name(name: &str) -> &str {
    let name = name.trim();
    match name.rsplit_once(" (") {
        Some((base, n))
            if n.strip_suffix(')')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
        {
            base
        }
        _ => name,
    }
}
/// A master's or release's artist credit: each name as credited, with its join word.
pub fn credit(v: &Value) -> Option<String> {
    let mut credit = String::new();
    for a in v["artists"].as_array()?.iter().take(8) {
        let name = text(&a["anv"]).or_else(|| text(&a["name"]))?;
        credit.push_str(plain_name(&name));
        match text(&a["join"]).as_deref() {
            Some(",") => credit.push_str(", "),
            Some(join) => {
                credit.push(' ');
                credit.push_str(join);
                credit.push(' ');
            }
            None => credit.push(' '),
        }
    }
    let credit = credit.trim();
    (!credit.is_empty()).then(|| credit.to_owned())
}
/// A release's format: the first format's name and descriptions (`CD, Album, Stereo`).
pub fn format(v: &Value) -> Option<String> {
    let f = v["formats"].as_array()?.first()?;
    let mut parts = vec![text(&f["name"])?];
    parts.extend(super::texts(&f["descriptions"]));
    Some(parts.join(", "))
}
/// A barcode as GTIN-13 (`barcode::gtin13`), spaces and dashes dropped: Discogs gives barcodes
/// as printed.
pub fn gtin13(code: &str) -> Option<String> {
    let digits: String = code.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    crate::barcode::gtin13(&digits)
}
/// A release's barcodes, as GTIN-13.
pub fn barcodes(v: &Value) -> Vec<String> {
    let mut codes: Vec<String> = v["identifiers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "Barcode")
        .filter_map(|i| gtin13(i["value"].as_str()?))
        .collect();
    codes.sort();
    codes.dedup();
    codes
}
fn year(v: &Value) -> Option<String> {
    v["year"]
        .as_u64()
        .filter(|y| (1000..=9999).contains(y))
        .map(|y| y.to_string())
}

/// The card of a Discogs record: an artist is a `MusicGroup`, a master or release a
/// `MusicAlbum` described by its artist credit and year (a release also by its format and
/// country). The primary image only with a credential (`images`).
pub fn card(mut doc: ResourceMeta, kind: Kind, v: &Value, images: bool) -> Option<ResourceMeta> {
    doc.site = "Discogs".into();
    if kind == Kind::Artist {
        doc.set_type("MusicGroup");
        doc.name = Some(plain_name(&text(&v["name"])?).to_owned());
    } else {
        doc.set_type("MusicAlbum");
        doc.name = Some(text(&v["title"])?);
        let mut parts: Vec<String> = credit(v).into_iter().collect();
        let mut facts = vec![("artist", credit(v)), ("year", year(v))];
        if kind == Kind::Release {
            parts.extend(format(v));
            parts.extend(text(&v["country"]));
            facts.extend([
                ("format", format(v)),
                ("country", text(&v["country"])),
                ("upc", super::list(&barcodes(v))),
                (
                    "master",
                    v["master_id"]
                        .as_u64()
                        .filter(|id| *id > 0)
                        .map(|id| root(Kind::Master, id)),
                ),
            ]);
        }
        parts.extend(year(v));
        doc.description = (!parts.is_empty()).then(|| parts.join(" · "));
        for (key, value) in facts {
            if let Some(value) = value {
                doc.attributes.insert(key.into(), value);
            }
        }
    }
    if images {
        let all = v["images"].as_array();
        doc.image = all
            .and_then(|i| {
                i.iter()
                    .find(|i| i["type"] == "primary")
                    .or_else(|| i.first())
            })
            .and_then(|i| i["uri"].as_str())
            .filter(|u| u.starts_with("https://"))
            .map(str::to_owned);
    }
    Some(doc)
}

#[async_trait]
impl Source for Discogs {
    fn name(&self) -> &'static str {
        "discogs"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "discogs",
                uri: "https://www.discogs.com/master/21491-Radiohead-OK-Computer",
                card_type: "MusicAlbum",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "api.discogs.com",
                path: "/masters/21491",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "master-21491.json",
            }],
        })
    }
    fn configure(&self, config: &crate::config::Config) {
        configure(config);
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let (kind, id) = of_uri(uri)?;
        let root = root(kind, id);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &["artist", "year", "format", "country", "upc", "master"]
    }
    fn budgets(&self) -> &'static [Budget] {
        &[("api.discogs.com", 25.0 / 60.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.discogs.com/master/21491-Radiohead-OK-Computer",
                card_type: "MusicAlbum",
            },
            Example {
                uri: "https://www.discogs.com/release/83182-Radiohead-OK-Computer",
                card_type: "MusicAlbum",
            },
            Example {
                uri: "https://www.discogs.com/artist/3840-Radiohead",
                card_type: "MusicGroup",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let (kind, id) = of_uri(uri).ok_or_else(|| anyhow::anyhow!("not a Discogs link"))?;
        let credential = CREDENTIAL.read().unwrap_or_else(|e| e.into_inner()).clone();
        let (record, authenticated) = record(cx.egress, kind, id, credential.as_ref()).await?;
        Ok(record
            .and_then(|v| card(cx.card(uri, self), kind, &v, authenticated))
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

/// The also-on half of this source, `discogs`.
pub mod also_on {
    use crate::{
        also_on::{self, Context, Source, Subject},
        lookups::secondary,
    };
    use anyhow::Result;
    use async_trait::async_trait;

    /// A Discogs link's own ID as the key `discogs:{kind}:{id}`, and a release's barcodes from its
    /// record (the card's request, shared) as `upc:` keys and facts.
    pub struct Discogs;

    #[async_trait]
    impl Source for Discogs {
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            &[
                ("discogs:artist", "1"),
                ("discogs:master", "21491"),
                ("discogs:release", "249504"),
                ("upc", "0602498688373"),
            ]
        }
        fn single_namespaces(&self) -> &'static [&'static str] {
            &["discogs:artist", "discogs:master", "discogs:release"]
        }
        fn search_key(&self, namespace: &str, value: &str) -> Option<String> {
            let (kind, id) = value.split_once(':').filter(|_| namespace == "discogs")?;
            (matches!(kind, "artist" | "master" | "release") && crate::lookups::digits(id))
                .then(|| format!("discogs:{value}"))
        }
        fn id(&self) -> &'static str {
            "discogs"
        }
        fn takes_token(&self) -> bool {
            true
        }
        fn app_key_variables(&self) -> Option<(&'static str, &'static str)> {
            Some((
                "PINAKES_DISCOGS_CONSUMER_KEY",
                "PINAKES_DISCOGS_CONSUMER_SECRET",
            ))
        }
        fn applies(&self, subject: &Subject) -> bool {
            super::of_uri(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<also_on::Found> {
            let Some((kind, id)) = super::of_uri(&subject.uri) else {
                return Ok(also_on::Found::default());
            };
            let key = |key: String| also_on::Key {
                key,
                via: "discogs".into(),
            };
            let mut found = also_on::Found {
                keys: vec![key(format!("discogs:{}:{id}", kind.as_str()))],
                ..Default::default()
            };
            if kind == super::Kind::Release {
                // The ID is the key; the record only adds the barcodes.
                let credential = super::credential(&cx.config);
                let record = super::record(&cx.egress, kind, id, credential.as_ref()).await;
                if let Some((Some(v), _)) = secondary("discogs", &mut found.retry_after, record) {
                    let barcodes = super::barcodes(&v);
                    // A box set lists each disc's barcode: only a release with one barcode keys it.
                    if let [upc] = barcodes.as_slice() {
                        found.keys.push(key(format!("upc:{upc}")));
                    }
                    for upc in barcodes.into_iter().take(4) {
                        found.facts.push(also_on::Fact::new("upc", upc));
                    }
                }
            }
            Ok(found)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lookups::fixture::{context, subject, JSON};

        #[tokio::test(start_paused = true)]
        async fn discogs_keys_its_id_and_a_releases_barcodes() {
            let release = crate::sources::fixture::read("discogs", "release-83182.json");
            // The same release listing a second disc's barcode, as a box set does.
            let mut boxed: serde_json::Value = serde_json::from_slice(&release).unwrap();
            boxed["id"] = 1.into();
            boxed["identifiers"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"type": "Barcode", "value": "0724384260521"}));
            let cx = context(vec![
                ("https://api.discogs.com/releases/83182", 200, JSON, release),
                (
                    "https://api.discogs.com/releases/1",
                    200,
                    JSON,
                    serde_json::to_vec(&boxed).unwrap(),
                ),
            ]);
            let keys = |found: also_on::Found| -> Vec<String> {
                found.keys.into_iter().map(|k| k.key).collect()
            };
            let mut s = subject("https://www.discogs.com/release/83182", None);
            assert!(Discogs.applies(&s));
            assert_eq!(
                keys(Discogs.lookup(&cx, &s).await.unwrap()),
                ["discogs:release:83182", "upc:0724385522925"]
            );
            // Two barcodes: facts, no key, so a box set never joins its discs' albums.
            s.uri = "https://www.discogs.com/release/1".into();
            let found = Discogs.lookup(&cx, &s).await.unwrap();
            assert_eq!(found.facts.len(), 2);
            assert_eq!(keys(found), ["discogs:release:1"]);
            // A master's or artist's key needs no request.
            s.uri = "https://www.discogs.com/master/21491".into();
            assert_eq!(
                keys(Discogs.lookup(&cx, &s).await.unwrap()),
                ["discogs:master:21491"]
            );
            s.uri = "https://www.discogs.com/label/1".into();
            assert!(!Discogs.applies(&s));
        }

        /// Its key namespace, as searches name it, and its barcode reader on any answer.
        #[test]
        fn keys_and_malformed_records() {
            use crate::lookups::{fixture, search_key};
            assert_eq!(
                search_key("discogs", "master:21491").as_deref(),
                Some("discogs:master:21491")
            );
            assert_eq!(search_key("discogs", "label:1"), None);
            assert_eq!(search_key("discogs", "release:083182"), None);
            for (_, raw) in fixture::malformed_answers() {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) {
                    let _ = super::super::barcodes(&v);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Value {
        serde_json::from_slice(&crate::sources::fixture::read("discogs", file)).unwrap()
    }
    fn base(uri: &str) -> ResourceMeta {
        ResourceMeta::new(uri, "discogs", 3600)
    }

    #[test]
    fn addresses() {
        for (uri, root) in [
            (
                "https://www.discogs.com/master/21491-Radiohead-OK-Computer",
                "https://www.discogs.com/master/21491",
            ),
            (
                "https://discogs.com/de/artist/3840-Radiohead/",
                "https://www.discogs.com/artist/3840",
            ),
            (
                "http://www.discogs.com/pt_BR/release/83182",
                "https://www.discogs.com/release/83182",
            ),
        ] {
            assert_eq!(Discogs.alias_root(uri).as_deref(), Some(root), "{uri}");
            assert_eq!(
                crate::music::parse(uri).map(|l| l.canonical).as_deref(),
                Some(root)
            );
        }
        assert_eq!(
            Discogs.alias_root("https://www.discogs.com/artist/3840"),
            None
        );
        for uri in [
            "https://www.discogs.com/label/1-Parlophone",
            "https://www.discogs.com/release/083182",
            "https://www.discogs.com/release/x-Radiohead",
            "https://www.discogs.com/release/83182/image/1",
            "https://www.discogs.com/sell/release/83182",
            "https://api.discogs.com/releases/83182",
            "https://www.discogs.com.evil.example/release/83182",
        ] {
            assert!(!Discogs.reads(uri), "{uri}");
        }
        let link = crate::music::parse("https://www.discogs.com/master/21491").unwrap();
        assert_eq!(
            (link.level, link.id.as_str()),
            (crate::music::Level::Album, "master/21491")
        );
        assert_eq!(of_link(&link), Some((Kind::Master, 21491)));
    }

    #[test]
    fn cards_say_what_the_record_says() {
        let uri = "https://www.discogs.com/master/21491";
        let d = card(base(uri), Kind::Master, &read("master-21491.json"), false).unwrap();
        assert_eq!(
            (
                d.type_name.as_str(),
                d.name.as_deref(),
                d.description.as_deref(),
                d.site.as_str()
            ),
            (
                "MusicAlbum",
                Some("OK Computer"),
                Some("Radiohead · 1997"),
                "Discogs"
            )
        );
        assert_eq!(d.image, None, "no image without a token");
        let release = read("release-83182.json");
        let uri = "https://www.discogs.com/release/83182";
        let d = card(base(uri), Kind::Release, &release, true).unwrap();
        assert_eq!(
            d.description.as_deref(),
            Some("Radiohead · CD, Album, Stereo · Europe · 1997")
        );
        assert!(d
            .image
            .as_deref()
            .is_some_and(|i| i.starts_with("https://i.discogs.com/")));
        let a = &d.attributes;
        assert_eq!(a["upc"], "0724385522925");
        assert_eq!(a["master"], "https://www.discogs.com/master/21491");
        assert_eq!(a["country"], "Europe");
        let uri = "https://www.discogs.com/artist/3840";
        let d = card(base(uri), Kind::Artist, &read("artist-3840.json"), false).unwrap();
        assert_eq!(
            (d.type_name.as_str(), d.name.as_deref()),
            ("MusicGroup", Some("Radiohead"))
        );
        assert!(d.attributes.is_empty());
        assert_eq!(
            credit(&read("release-36457659.json")).as_deref(),
            Some("Tame Impala & Jennie")
        );
        assert_eq!(plain_name("Radiohead (2)"), "Radiohead");
        assert_eq!(plain_name("Sunn O)))"), "Sunn O)))");
    }

    #[test]
    fn barcodes_are_checked() {
        assert_eq!(gtin13("7 24385 52292 5").as_deref(), Some("0724385522925"));
        assert_eq!(gtin13("0724385522925").as_deref(), Some("0724385522925"));
        assert_eq!(gtin13("96385074").as_deref(), Some("0000096385074"));
        assert_eq!(gtin13("724385522926"), None, "wrong check digit");
        assert_eq!(gtin13("72438552292"), None);
        assert_eq!(gtin13("LC 0299"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_card_reads_the_api_and_a_token_stays_out_of_errors() {
        let (egress, recorded) = crate::sources::fixture::egress(vec![
            (
                "https://api.discogs.com/masters/21491",
                200,
                "application/json",
                crate::sources::fixture::read("discogs", "master-21491.json"),
            ),
            (
                "https://api.discogs.com/masters/999999999",
                404,
                "application/json",
                crate::sources::fixture::read("discogs", "master-missing.json"),
            ),
            (
                "https://api.discogs.com/artists/1",
                403,
                "text/html",
                b"walled".to_vec(),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let d = Discogs
            .resolve(
                &cx,
                "https://www.discogs.com/master/21491-Radiohead-OK-Computer",
            )
            .await
            .unwrap();
        assert_eq!(d.name.as_deref(), Some("OK Computer"));
        let d = Discogs
            .resolve(&cx, "https://www.discogs.com/master/999999999")
            .await
            .unwrap();
        assert_eq!(d.status, crate::model::Status::Missing);
        let token = Credential::Token("s3cret-token".into());
        let error = record(&egress, Kind::Artist, 1, Some(&token))
            .await
            .unwrap_err();
        assert!(!format!("{error:?}").contains("s3cret"), "{error:?}");
        // Refused with the token, then without it.
        assert_eq!(
            recorded.asked(),
            [
                "https://api.discogs.com/masters/21491",
                "https://api.discogs.com/masters/999999999",
                "https://api.discogs.com/artists/1",
                "https://api.discogs.com/artists/1",
            ]
        );
    }

    /// An app's consumer key and secret are a credential, from the file or their own variables;
    /// the token wins when both are set; neither shows in the config's forms; a key with a comma
    /// (which would split the header) is refused.
    #[test]
    fn an_app_key_and_secret_are_a_credential_and_the_token_wins() {
        use crate::config::{apply_named_env, Config};
        let vars = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        let mut value = Config::defaults().unwrap();
        apply_named_env(
            &mut value,
            vars(&[
                ("PINAKES_DISCOGS_CONSUMER_KEY", "ABCDEFKEY"),
                ("PINAKES_DISCOGS_CONSUMER_SECRET", "ABCDEFSECRET"),
            ]),
        )
        .unwrap();
        let cfg = Config::from_value(value.clone()).unwrap();
        assert_eq!(
            credential(&cfg).map(|c| c.header()).as_deref(),
            Some("Discogs key=ABCDEFKEY, secret=ABCDEFSECRET")
        );
        let shown = format!("{cfg:?} {}", serde_json::to_string(&cfg).unwrap());
        assert!(!shown.contains("ABCDEF"), "{shown}");
        // The token wins.
        value["also_on"]["sources"]["discogs"]["token"] = "ABCDEFTOKEN".into();
        let cfg = Config::from_value(value).unwrap();
        assert_eq!(
            credential(&cfg).map(|c| c.header()).as_deref(),
            Some("Discogs token=ABCDEFTOKEN")
        );
        // A key without its secret is no credential; the file's value stays when the variable
        // is set too.
        let mut value = Config::defaults().unwrap();
        value["also_on"]["sources"]["discogs"] = serde_json::json!({"key": "FROMFILE"});
        apply_named_env(
            &mut value,
            vars(&[("PINAKES_DISCOGS_CONSUMER_KEY", "FROMENV")]),
        )
        .unwrap();
        assert_eq!(value["also_on"]["sources"]["discogs"]["key"], "FROMFILE");
        assert!(credential(&Config::from_value(value).unwrap()).is_none());
        let mut value = Config::defaults().unwrap();
        value["also_on"]["sources"]["discogs"] = serde_json::json!({"key": "a,b", "secret": "s"});
        assert!(Config::from_value(value).is_err());
        // Only Discogs takes one.
        let mut value = Config::defaults().unwrap();
        value["also_on"]["sources"]["deezer"] = serde_json::json!({"key": "k", "secret": "s"});
        assert!(Config::from_value(value).is_err());
    }

    /// The app's pair goes in Discogs' own header form, with the request kept out of logs.
    #[tokio::test(start_paused = true)]
    async fn an_app_key_and_secret_are_sent_as_discogs_reads_them() {
        #[derive(Default)]
        struct Seen(std::sync::Mutex<Vec<(String, bool)>>);
        #[async_trait::async_trait]
        impl crate::resolver::egress::Transport for Seen {
            async fn get(
                &self,
                url: &url::Url,
                addresses: &[std::net::SocketAddr],
            ) -> Result<crate::resolver::egress::Fetched> {
                self.get_with(url, addresses, &FetchOptions::default())
                    .await
            }
            async fn get_with(
                &self,
                url: &url::Url,
                _: &[std::net::SocketAddr],
                options: &FetchOptions,
            ) -> Result<crate::resolver::egress::Fetched> {
                let header = options
                    .headers
                    .iter()
                    .find(|(name, _)| *name == "authorization")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                self.0.lock().unwrap().push((header, options.sensitive));
                Ok(crate::resolver::egress::Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    validators: Default::default(),
                    url: url.clone(),
                    content_type: "application/json".into(),
                    status: 200,
                    retry_after_secs: 60,
                    location: None,
                    body: crate::sources::fixture::read("discogs", "master-21491.json"),
                })
            }
        }
        let seen = std::sync::Arc::new(Seen::default());
        let egress = Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(crate::sources::fixture::PublicDns),
            seen.clone(),
        );
        let app = Credential::App {
            key: "app-key".into(),
            secret: "app-secret".into(),
        };
        let (master, authenticated) = record(&egress, Kind::Master, 21491, Some(&app))
            .await
            .unwrap();
        assert_eq!(master.unwrap()["title"], "OK Computer");
        assert!(authenticated);
        assert_eq!(
            *seen.0.lock().unwrap(),
            [("Discogs key=app-key, secret=app-secret".to_owned(), true)]
        );
    }

    /// A token Discogs refuses (expired, mistyped) is dropped: the record comes without it.
    #[tokio::test(start_paused = true)]
    async fn a_refused_token_falls_back_to_reading_without_it() {
        #[derive(Default)]
        struct Refusing {
            authorized: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl crate::resolver::egress::Transport for Refusing {
            async fn get(
                &self,
                url: &url::Url,
                addresses: &[std::net::SocketAddr],
            ) -> Result<crate::resolver::egress::Fetched> {
                self.get_with(url, addresses, &FetchOptions::default())
                    .await
            }
            async fn get_with(
                &self,
                url: &url::Url,
                _: &[std::net::SocketAddr],
                options: &FetchOptions,
            ) -> Result<crate::resolver::egress::Fetched> {
                let authorized = options
                    .headers
                    .iter()
                    .any(|(name, _)| *name == "authorization");
                if authorized {
                    self.authorized
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(crate::resolver::egress::Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    validators: Default::default(),
                    url: url.clone(),
                    content_type: "application/json".into(),
                    status: if authorized { 401 } else { 200 },
                    retry_after_secs: 60,
                    location: None,
                    body: if authorized {
                        br#"{"message": "You must authenticate to access this resource."}"#.to_vec()
                    } else {
                        crate::sources::fixture::read("discogs", "master-21491.json")
                    },
                })
            }
        }
        let api = std::sync::Arc::new(Refusing::default());
        let egress = Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(crate::sources::fixture::PublicDns),
            api.clone(),
        );
        let expired = Credential::Token("expired".into());
        let (master, authenticated) = record(&egress, Kind::Master, 21491, Some(&expired))
            .await
            .unwrap();
        assert_eq!(master.unwrap()["title"], "OK Computer");
        assert!(!authenticated);
        // Once refused, the token is not sent again.
        let (_, authenticated) = record(&egress, Kind::Master, 21492, Some(&expired))
            .await
            .unwrap();
        assert!(!authenticated);
        assert_eq!(api.authorized.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
