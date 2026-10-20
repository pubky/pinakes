//! Discogs artists, masters (an album across its editions) and releases (one edition), read from
//! its API (`api.discogs.com`): its web pages answer bots with a wall. No key: 25 requests a
//! minute. Images need a token, so cards have none.
use super::{text, web_url, Budget, Context, Example, Source};
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

pub struct Discogs;

const API: &str = "https://api.discogs.com";

/// What a Discogs link names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Artist,
    /// An album: every edition of it (a MusicBrainz release group).
    Master,
    /// One edition (a MusicBrainz release).
    Release,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Artist => "artist",
            Self::Master => "master",
            Self::Release => "release",
        }
    }
}

/// The kind and ID of `www.discogs.com/{lang/}?{artist,master,release}/{id}[-slug]`.
pub fn of_uri(uri: &str) -> Option<(Kind, u64)> {
    let url = web_url(uri)?;
    if !matches!(url.host_str()?, "www.discogs.com" | "discogs.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    // A language path (`de`, `pt_BR`) before the kind.
    let language = |s: &str| {
        let (lang, region) = s.split_once('_').unwrap_or((s, "AA"));
        lang.len() == 2
            && lang.bytes().all(|b| b.is_ascii_lowercase())
            && region.len() == 2
            && region.bytes().all(|b| b.is_ascii_uppercase())
    };
    let segments = match segments.as_slice() {
        [lang, rest @ ..] if language(lang) => rest,
        all => all,
    };
    let [kind, id] = segments else {
        return None;
    };
    let kind = match *kind {
        "artist" => Kind::Artist,
        "master" => Kind::Master,
        "release" => Kind::Release,
        _ => return None,
    };
    let id = id.split('-').next()?;
    if id.is_empty()
        || id.len() > 12
        || id.starts_with('0')
        || !id.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((kind, id.parse().ok()?))
}

/// The one address of a Discogs object.
pub fn root(kind: Kind, id: u64) -> String {
    format!("https://www.discogs.com/{}/{id}", kind.as_str())
}

/// Discogs' record of one object, `None` when it has none. The answer is shared for an hour,
/// so the card and the also-on lookup ask once.
pub async fn record(
    egress: &Egress,
    kind: Kind,
    id: u64,
    token: Option<&str>,
) -> Result<Option<Value>> {
    let options = FetchOptions {
        accept: Some("application/json".into()),
        headers: token
            .map(|t| vec![("authorization", format!("Discogs token={t}"))])
            .unwrap_or_default(),
        sensitive: token.is_some(),
        ..Default::default()
    };
    let url = format!("{API}/{}s/{id}", kind.as_str());
    let f = egress.discover(&url, &options, 3600).await?;
    match f.status {
        200 => Ok(Some(serde_json::from_slice(&f.body)?)),
        404 => Ok(None),
        408 | 429 | 500..=599 => Err(RetryLater {
            class: "upstream",
            seconds: f.retry_after_secs.clamp(60, 3600),
        }
        .into()),
        status => anyhow::bail!("Discogs answered {status}"),
    }
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
        let record = record(cx.egress, kind, id, None).await?;
        Ok(record
            .and_then(|v| card(cx.card(uri, self), kind, &v, false))
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
        assert_eq!(
            recorded.asked(),
            [
                "https://api.discogs.com/masters/21491",
                "https://api.discogs.com/masters/999999999",
                "https://api.discogs.com/artists/1",
            ]
        );
    }
}
