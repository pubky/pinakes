//! Discogs artists, masters (an album across its editions) and releases (one edition), read from
//! its API (`api.discogs.com`): its web pages answer bots with a wall. No key: 25 requests a
//! minute. An optional personal token (`[also_on.sources.discogs] token`) adds images to the
//! cards; it never reaches a log or an error.
use super::{text, Budget, Context, Example, Source};
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

/// The personal token the daemon started with (`configure`).
static TOKEN: RwLock<Option<crate::event::adapters::SecretKey>> = RwLock::new(None);

/// Record `[also_on.sources.discogs] token` for the card route.
fn configure(config: &crate::config::Config) {
    *TOKEN.write().unwrap_or_else(|e| e.into_inner()) = token(config);
}
/// The configured token, if any.
pub fn token(config: &crate::config::Config) -> Option<crate::event::adapters::SecretKey> {
    config
        .also_on
        .sources
        .get("discogs")
        .and_then(|s| s.token.clone())
        .filter(|t| !t.is_empty())
}

/// Discogs' record of one object, `None` when it has none, and whether the token was used. The
/// answer is shared for an hour, so the card and the also-on lookup ask once. A token Discogs
/// refuses (401, 403: expired or mistyped) is dropped, with one warning: that request is asked
/// again without it, and later ones go without it from the start, so cards still work without
/// images.
pub async fn record(
    egress: &Egress,
    kind: Kind,
    id: u64,
    token: Option<&str>,
) -> Result<(Option<Value>, bool)> {
    let token = token.filter(|t| !refused(t));
    let mut f = fetch(egress, kind, id, token).await?;
    let mut authenticated = token.is_some();
    if let (Some(token), 401 | 403) = (token, f.status) {
        super::token_refused("discogs");
        if refuse(token) {
            tracing::warn!(
                status = f.status,
                "Discogs refused the configured token; reading without it (no images)"
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
/// The tokens Discogs has refused since the daemon started, by hash (never the token itself).
static REFUSED: std::sync::Mutex<Vec<[u8; 32]>> = std::sync::Mutex::new(Vec::new());
fn refused(token: &str) -> bool {
    let hash = *blake3::hash(token.as_bytes()).as_bytes();
    REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&hash)
}
/// Remember a refused token; true the first time.
fn refuse(token: &str) -> bool {
    let hash = *blake3::hash(token.as_bytes()).as_bytes();
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
    token: Option<&str>,
) -> Result<crate::resolver::egress::Fetched> {
    let options = FetchOptions {
        accept: Some("application/json".into()),
        headers: token
            .map(|t| vec![("authorization", format!("Discogs token={t}"))])
            .unwrap_or_default(),
        sensitive: token.is_some(),
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
/// A barcode as GTIN-13: an EAN-8, UPC-A or EAN-13 (spaces and dashes dropped) whose check
/// digit is right.
pub fn gtin13(code: &str) -> Option<String> {
    let digits: String = code.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    if !matches!(digits.len(), 8 | 12 | 13) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let code = format!("{digits:0>13}");
    let d: Vec<u32> = code.bytes().map(|b| u32::from(b - b'0')).collect();
    let sum: u32 = d[..12]
        .iter()
        .enumerate()
        .map(|(i, n)| if i % 2 == 0 { *n } else { n * 3 })
        .sum();
    ((10 - sum % 10) % 10 == d[12]).then_some(code)
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
/// country). The primary image only with a token (`images`).
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
        let token = TOKEN.read().unwrap_or_else(|e| e.into_inner()).clone();
        let (record, authenticated) = record(cx.egress, kind, id, token.as_deref()).await?;
        Ok(record
            .and_then(|v| card(cx.card(uri, self), kind, &v, authenticated))
            .unwrap_or_else(|| cx.missing(uri, self)))
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
        let error = record(&egress, Kind::Artist, 1, Some("s3cret-token"))
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
        let (master, authenticated) = record(&egress, Kind::Master, 21491, Some("expired"))
            .await
            .unwrap();
        assert_eq!(master.unwrap()["title"], "OK Computer");
        assert!(!authenticated);
        // Once refused, the token is not sent again.
        let (_, authenticated) = record(&egress, Kind::Master, 21492, Some("expired"))
            .await
            .unwrap();
        assert!(!authenticated);
        assert_eq!(api.authorized.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
