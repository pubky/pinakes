//! The media sources' test harness: recorded answers from `tests/fixtures/media` (each family's
//! `provenance.json` names the URL of every file), served through Egress. Each media source's
//! tests are its own file (`also_on_books.rs`, `also_on_films.rs`, `also_on_podcasts.rs`).
use pinakes_core::{
    also_on::{Context, Found, Subject},
    config::{Config, EgressConfig},
    model::ResourceMeta,
    resolver::egress::{Dns, Egress, FetchOptions, Fetched, Transport},
};

use std::{
    collections::BTreeMap,
    io::Read,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

pub struct PublicDns;
#[async_trait::async_trait]
impl Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
    }
}

/// One recorded answer.
#[derive(Clone)]
pub struct Answer {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Answers every recorded URL as it was recorded, a Wikidata search nobody recorded with no
/// hits, and anything else with a test failure; records each request.
pub struct Recorded {
    pub answers: BTreeMap<String, Answer>,
    pub seen: Mutex<Vec<String>>,
    /// The Podcast Index key each Podcast Index request carried.
    pub signed_with: Mutex<Vec<Option<String>>>,
    /// URLs (by a part of them) whose answer is over its read's cap.
    pub too_large: Vec<String>,
}
pub fn read(family: &str, file: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/media/{family}/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    if file.ends_with(".gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(raw.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        out
    } else {
        raw
    }
}
impl Recorded {
    pub fn new() -> Arc<Self> {
        let mut answers = BTreeMap::new();
        // Every directory's recorded answers: each source's while it is in this build (its
        // fixtures go with it), and the shared ones (`shared/`).
        let root = format!("{}/tests/fixtures/media", env!("CARGO_MANIFEST_DIR"));
        let mut families: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("provenance.json").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        families.sort();
        for family in &families {
            let provenance: serde_json::Value =
                serde_json::from_slice(&read(family, "provenance.json")).unwrap();
            let entries = provenance.as_object().unwrap();
            for (name, p) in entries {
                // A refusal or a synthetic answer is served only where a test asks for it
                // (`with_recorded`).
                if name.ends_with("-401") || name.ends_with("-copy") {
                    continue;
                }
                let mut url = p["url"].as_str().unwrap().to_owned();
                // A followed redirect's answer is served at its target.
                if let Some(first) = name.strip_suffix("-followed") {
                    url = entries[first]["location"].as_str().unwrap().to_owned();
                }
                answers.insert(
                    url,
                    Answer {
                        status: p["status"].as_u64().unwrap() as u16,
                        location: p["location"].as_str().map(str::to_owned),
                        content_type: p["content_type"].as_str().unwrap_or("").to_owned(),
                        body: read(family, p["file"].as_str().unwrap()),
                    },
                );
            }
        }
        Arc::new(Self {
            answers,
            seen: Mutex::new(Vec::new()),
            signed_with: Mutex::new(Vec::new()),
            too_large: vec![],
        })
    }
    /// The same, with the URLs holding one of `parts` answering over their read's cap.
    pub fn with_too_large(parts: &[&str]) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        recorded.too_large = parts.iter().map(|p| (*p).to_owned()).collect();
        Arc::new(recorded)
    }
    /// The same, with recorded answers not served by default, each in place of its URL's.
    pub fn with_recorded(more: &[(&str, &str)]) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        for (family, name) in more {
            let provenance: serde_json::Value =
                serde_json::from_slice(&read(family, "provenance.json")).unwrap();
            let p = &provenance[*name];
            recorded.answers.insert(
                p["url"].as_str().unwrap().into(),
                Answer {
                    status: p["status"].as_u64().unwrap() as u16,
                    location: None,
                    content_type: p["content_type"].as_str().unwrap_or("").to_owned(),
                    body: read(family, p["file"].as_str().unwrap()),
                },
            );
        }
        Arc::new(recorded)
    }
    /// The same, with more 200 answers: a URL, its content type and its body.
    pub fn with_bodies(more: &[(&str, &str, String)]) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        for (url, content_type, body) in more {
            recorded.answers.insert(
                (*url).into(),
                Answer {
                    status: 200,
                    location: None,
                    content_type: (*content_type).into(),
                    body: body.as_bytes().to_vec(),
                },
            );
        }
        Arc::new(recorded)
    }
    /// The same, with more answers, each in place of its URL's: a URL, its status, its content
    /// type and its body.
    pub fn with_answers(more: &[(&str, u16, &str, Vec<u8>)]) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        for (url, status, content_type, body) in more {
            recorded.answers.insert(
                (*url).into(),
                Answer {
                    status: *status,
                    location: None,
                    content_type: (*content_type).into(),
                    body: body.clone(),
                },
            );
        }
        Arc::new(recorded)
    }
    /// The same, with more answers: a URL and its status, and its `Location` or HTML body.
    pub fn with(more: &[(&str, u16, &str)]) -> Arc<Self> {
        let mut recorded = Arc::into_inner(Self::new()).unwrap();
        for (url, status, then) in more {
            let redirect = (300..400).contains(status);
            recorded.answers.insert(
                (*url).into(),
                Answer {
                    status: *status,
                    location: redirect.then(|| (*then).into()),
                    content_type: "text/html".into(),
                    body: if redirect {
                        Vec::new()
                    } else {
                        then.as_bytes().to_vec()
                    },
                },
            );
        }
        Arc::new(recorded)
    }
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}
#[async_trait::async_trait]
impl Transport for Recorded {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        self.seen.lock().unwrap().push(url.to_string());
        if self
            .too_large
            .iter()
            .any(|p| url.as_str().contains(p.as_str()))
        {
            return Err(anyhow::Error::new(
                pinakes_core::resolver::egress::BodyTooLarge {
                    cap: 8 * 1024 * 1024,
                },
            ));
        }
        if url.host_str() == Some("unreadable.example") {
            anyhow::bail!("body over the cap");
        }
        if url.host_str() == Some("api.podcastindex.org") {
            assert!(options.sensitive);
            let header = |name: &str| {
                options
                    .headers
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            };
            assert_eq!(header("Authorization").map(|h| h.len()), Some(40));
            self.signed_with.lock().unwrap().push(header("X-Auth-Key"));
        }
        let param = |u: &url::Url, k: &str| {
            u.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
        };
        let query = |k: &str| param(url, k).is_some_and(|v| !v.is_empty());
        // A Wikidata search is the recorded one for the same statement, however it is spelled.
        let search = param(url, "srsearch").and_then(|asked| {
            self.answers.iter().find_map(|(u, a)| {
                let u = url::Url::parse(u).ok()?;
                (param(&u, "srsearch") == Some(asked.clone())).then(|| a.clone())
            })
        });
        let answer = match self.answers.get(url.as_str()).cloned().or(search) {
            Some(answer) => answer,
            // The recorded entities answer holds every item the tests read.
            None if url.host_str() == Some("www.wikidata.org") && query("ids") => self
                .answers
                .iter()
                .find(|(u, _)| u.contains("action=wbgetentities"))
                .unwrap()
                .1
                .clone(),
            None if url.host_str() == Some("www.wikidata.org") && query("srsearch") => Answer {
                status: 200,
                location: None,
                content_type: "application/json".into(),
                body: br#"{"batchcomplete":"","query":{"search":[]}}"#.to_vec(),
            },
            None => panic!("unrecorded request {url}"),
        };
        Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            status: answer.status,
            retry_after_secs: 0,
            location: answer.location,
            content_type: answer.content_type,
            body: answer.body,
        })
    }
}

pub fn cx(transport: Arc<Recorded>, config: Config) -> Context {
    Context {
        egress: Egress::with_backends(EgressConfig::default(), Arc::new(PublicDns), transport),
        config: Arc::new(config),
    }
}
pub fn subject(uri: &str, type_name: Option<&str>, doc: Option<ResourceMeta>) -> Subject {
    Subject {
        id: "r1".into(),
        uri: uri.into(),
        status: "ok".into(),
        type_name: type_name.map(str::to_owned),
        doc,
        keys: Vec::new(),
        first_seen_at: None,
    }
}
pub fn keys(found: &Found) -> Vec<&str> {
    found.keys.iter().map(|k| k.key.as_str()).collect()
}
/// The exact key the link at `uri` is named by.
pub fn key_of<'a>(found: &'a Found, uri: &str) -> Option<&'a str> {
    let link = found.links.iter().find(|l| l.uri == uri);
    link.unwrap_or_else(|| panic!("no link {uri}"))
        .key
        .as_deref()
}
pub fn links(found: &Found) -> Vec<(&str, &str, &str)> {
    found
        .links
        .iter()
        .map(|l| (l.site.as_str(), l.uri.as_str(), l.via.as_str()))
        .collect()
}
pub fn sites(found: &Found) -> Vec<&str> {
    found.links.iter().map(|l| l.site.as_str()).collect()
}
