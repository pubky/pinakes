//! One production-policy probe per adapter, with reusable response captures. No queue writes.
use anyhow::{ensure, Result};
use pinakes_core::{
    config::{EgressConfig, EventConfig},
    event::{
        adapters::{self, service::PlatformResolver},
        service::EventService,
    },
    resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, PinnedTransport, SystemDns, Transport},
        osm::OsmResolver,
        web::WebResolver,
        Resolver,
    },
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};
use url::Url;
/// A captured body with the keys its publisher sends every visitor replaced by placeholders
/// (`scripts/scrub-publisher-keys`), before it is hashed, saved or read.
fn scrub_publisher_keys(body: &[u8]) -> Result<Vec<u8>> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/scrub-publisher-keys");
    let mut child = Command::new("python3")
        .arg("-I")
        .arg(script)
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    // The script reads to the end before it writes, so the whole body goes in first.
    child.stdin.take().expect("piped stdin").write_all(body)?;
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "scripts/scrub-publisher-keys failed");
    Ok(out.stdout)
}
/// A URL or message the probe records, scrubbed like a body.
fn scrub_text(text: &str) -> Result<String> {
    Ok(String::from_utf8(scrub_publisher_keys(text.as_bytes())?)?)
}
/// The key a capture is kept and replayed by: the request's URL and JSON body in their scrubbed
/// form, so a URL as `requests.json` records it (or a recorded redirect's scrubbed `location`)
/// finds the capture its live request made.
fn capture_key(uri: &str, json_body: Option<&Value>) -> Result<String> {
    scrub_text(&format!(
        "{uri}:{}",
        json_body.map(Value::to_string).unwrap_or_default()
    ))
}
/// Scrubs a live result's body, saves it under `root` and returns what `requests.json` keeps of
/// the request: its URL, JSON body, redirect `location` and error scrubbed like the bodies. The
/// live `location` stays as served: the egress follows it, key and all.
fn record_request(
    url: &Url,
    options: &FetchOptions,
    result: &mut Result<Fetched>,
    root: &Path,
) -> Result<Value> {
    let json_body = match &options.json_body {
        Some(body) => Some(serde_json::from_str::<Value>(&scrub_text(
            &body.to_string(),
        )?)?),
        None => None,
    };
    let mut record = json!({"uri":scrub_text(url.as_str())?,"at":chrono::Utc::now(),"user_agent":options.user_agent,"feed_hint":options.feed_hint,"json_body":json_body});
    match result {
        Ok(f) => {
            f.body = scrub_publisher_keys(&f.body)?;
            let hash = blake3::hash(&f.body).to_hex().to_string();
            let file = root.join(format!("{hash}.raw"));
            if !file.exists() {
                std::fs::write(&file, &f.body)?;
            }
            record["status"] = json!(f.status);
            record["content_type"] = json!(f.content_type);
            record["bytes"] = json!(f.body.len());
            record["blake3"] = json!(hash);
            record["path"] = json!(file);
            record["location"] = json!(f.location.as_deref().map(scrub_text).transpose()?);
            record["validators"] = json!(f.validators);
        }
        Err(e) => record["error"] = json!(scrub_text(&e.to_string())?),
    }
    Ok(record)
}
struct Captures {
    offline: bool,
    root: PathBuf,
    responses: Mutex<HashMap<String, Result<Fetched, String>>>,
    requests: Mutex<Vec<Value>>,
}
#[async_trait::async_trait]
impl Transport for Captures {
    async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(url, addresses, &Default::default()).await
    }
    async fn get_with(
        &self,
        url: &Url,
        addresses: &[SocketAddr],
        options: &FetchOptions,
    ) -> Result<Fetched> {
        ensure!(!options.sensitive, "probe refuses configured secrets");
        let key = capture_key(url.as_str(), options.json_body.as_ref())?;
        if let Some(saved) = self.responses.lock().unwrap().get(&key) {
            return saved.clone().map_err(anyhow::Error::msg);
        }
        ensure!(
            !self.offline,
            "offline probe has no captured response for {url}"
        );
        let mut result = PinnedTransport.get_with(url, addresses, options).await;
        let record = record_request(url, options, &mut result, &self.root)?;
        self.requests.lock().unwrap().push(record);
        std::fs::write(
            self.root.join("requests.json"),
            serde_json::to_vec_pretty(&*self.requests.lock().unwrap())?,
        )?;
        self.responses
            .lock()
            .unwrap()
            .insert(key, result.as_ref().cloned().map_err(ToString::to_string));
        result
    }
}
struct ProbeDns {
    offline: bool,
}
#[async_trait::async_trait]
impl Dns for ProbeDns {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        if self.offline {
            Ok(vec![SocketAddr::new("93.184.216.34".parse()?, port)])
        } else {
            SystemDns.lookup(host, port).await
        }
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "adapter_probe MANIFEST.json REPLAYS.json OUTPUT_DIR"
    );
    let manifest: Vec<Value> = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let replays: Vec<Value> = serde_json::from_slice(&std::fs::read(&args[2])?)?;
    let root = PathBuf::from(&args[3]);
    ensure!(!root.exists(), "output directory already exists");
    std::fs::create_dir_all(&root)?;
    let offline = std::env::var("PINAKES_PROBE_OFFLINE").is_ok_and(|v| v == "1");
    let captures = Arc::new(Captures {
        offline,
        root: root.clone(),
        responses: Mutex::default(),
        requests: Mutex::default(),
    });
    for v in &replays {
        let uri = v["uri"].as_str().unwrap();
        let result = if let Some(e) = v["error"].as_str() {
            Err(e.to_owned())
        } else {
            Ok(Fetched {
                permanent_redirect: false,
                truncated: false,
                url: uri.parse()?,
                status: v["status"].as_u64().unwrap_or(200) as u16,
                content_type: v["content_type"].as_str().unwrap_or("text/html").into(),
                body: std::fs::read(v["path"].as_str().unwrap())?,
                retry_after_secs: 60,
                location: v["location"].as_str().map(str::to_owned),
                validators: serde_json::from_value(v["validators"].clone()).unwrap_or_default(),
                charset: None,
            })
        };
        let key = capture_key(uri, v["json_body"].as_object().map(|_| &v["json_body"]))?;
        captures.responses.lock().unwrap().insert(key, result);
    }
    let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let resolver = PlatformResolver {
        web: WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            config: Default::default(),
            egress: Egress::with_backends(
                EgressConfig::default(),
                Arc::new(ProbeDns { offline }),
                captures.clone(),
            ),
            events: Some(Arc::new(EventService {
                pool: pool.clone(),
                config: cfg.clone(),
                osm: Arc::new(OsmResolver::new(pool, Default::default())?),
            })),
        },
    };
    let mut results = vec![];
    for v in manifest {
        let id = v["id"].as_str().unwrap();
        let d = adapters::definition(id).unwrap();
        let m = adapters::manifest(d, &cfg);
        let Some(uri) = v["uri"].as_str() else {
            results.push(json!({"id":id,"skip":v["note"]}));
            continue;
        };
        let mut result = json!({"id":id,"uri":uri,"at":chrono::Utc::now()});
        // A platform's canary is read as the canary reads it: a listing's first event link.
        let canary = m.canary.as_ref().is_some_and(|c| c.as_str() == uri);
        let probe = async {
            let target = if canary {
                resolver.canary_target(&m).await?
            } else {
                uri.to_owned()
            };
            let doc = resolver.probe_manifest(&target, &m).await?;
            anyhow::Ok((target, doc))
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(15), probe).await;
        let outcome = outcome.map(|r| {
            r.map(|(target, doc)| {
                result["target"] = json!(target);
                doc
            })
        });
        match outcome {
            Ok(Ok(doc)) => {
                result["document"] = json!(doc);
                result["missing_fields"] = json!(m.missing_fields(&doc));
                if let Some(c) = doc.container {
                    result["occurrences"] = json!(c.occurrences.len());
                    result["samples"] = json!(c
                        .occurrences
                        .iter()
                        .take(2)
                        .map(|o| &o.doc)
                        .collect::<Vec<_>>());
                }
            }
            Ok(Err(e)) => result["error"] = json!(e.to_string()),
            Err(e) => result["error"] = json!(e.to_string()),
        }
        println!(
            "{}",
            scrub_text(&json!({"id":id,"status":result.pointer("/document/status"),"type":result.pointer("/document/type"),"error":result["error"],"missing":result["missing_fields"],"occurrences":result["occurrences"]}).to_string())?
        );
        results.push(result);
        write_results(&root, &results)?;
    }
    write_results(&root, &results)?;
    Ok(())
}
/// `results.json`: the probes' documents, targets and errors, scrubbed like the captures.
fn write_results(root: &Path, results: &[Value]) -> Result<()> {
    let text = String::from_utf8(serde_json::to_vec_pretty(results)?)?;
    std::fs::write(root.join("results.json"), scrub_text(&text)?)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A presigned request URL, a redirect to a keyed map URL and an error naming the URL are
    /// recorded without their keys, and the scrubbed URLs replay to the captures their live
    /// requests made.
    #[test]
    fn a_request_record_keeps_no_publisher_key_and_replays_by_its_scrubbed_url() {
        // Built at run time, so no source file holds a key-shaped value.
        let key_id = format!("AKIA{}", "Q7".repeat(8));
        let maps = format!("AIza{}", "Q".repeat(35));
        let url: Url = format!(
            "https://b.s3.amazonaws.com/f?X-Amz-Credential={key_id}%2F20261016&X-Amz-Signature=SIGNATURE1"
        )
        .parse()
        .unwrap();
        let location = format!("https://maps.example/staticmap?center=1,2&key={maps}");
        let root = tempfile::tempdir().unwrap();
        let mut result = Ok(Fetched {
            permanent_redirect: false,
            truncated: false,
            url: url.clone(),
            status: 302,
            content_type: "text/html".into(),
            body: format!("<a href=\"{location}\">moved</a>").into_bytes(),
            retry_after_secs: 60,
            location: Some(location.clone()),
            validators: Default::default(),
            charset: None,
        });
        let record =
            record_request(&url, &FetchOptions::default(), &mut result, root.path()).unwrap();
        let saved = std::fs::read_to_string(record["path"].as_str().unwrap()).unwrap();
        for text in [record.to_string(), saved] {
            for value in [&key_id, &maps, "SIGNATURE1"] {
                assert!(!text.contains(value), "{value} recorded: {text}");
            }
        }
        assert_eq!(
            record["uri"],
            "https://b.s3.amazonaws.com/f?X-Amz-Credential=REDACTED%2F20261016&X-Amz-Signature=REDACTED"
        );
        assert_eq!(
            record["location"],
            "https://maps.example/staticmap?center=1,2&key=REDACTED"
        );
        // The live redirect is followed as served; only the record is scrubbed.
        let fetched = result.unwrap();
        assert_eq!(fetched.location.as_deref(), Some(location.as_str()));
        assert!(!String::from_utf8(fetched.body).unwrap().contains(&maps));
        // Replay: the recorded URI finds the live request's capture, and a replayed redirect to
        // the scrubbed location finds the capture of the live redirect.
        assert_eq!(
            capture_key(record["uri"].as_str().unwrap(), None).unwrap(),
            capture_key(url.as_str(), None).unwrap()
        );
        let next: Url = record["location"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            capture_key(next.as_str(), None).unwrap(),
            capture_key(&location, None).unwrap()
        );
        let mut failed = Err(anyhow::anyhow!("request to {url} failed"));
        let record =
            record_request(&url, &FetchOptions::default(), &mut failed, root.path()).unwrap();
        let error = record["error"].as_str().unwrap();
        assert!(error.contains("X-Amz-Credential=REDACTED") && !error.contains(&key_id));
        // A JSON body is recorded scrubbed and still replays to the live request's capture.
        let options = FetchOptions {
            json_body: Some(json!({"query": "x", "apiKey": maps})),
            ..Default::default()
        };
        let mut failed = Err(anyhow::anyhow!("no answer"));
        let record = record_request(&url, &options, &mut failed, root.path()).unwrap();
        assert_eq!(
            record["json_body"],
            json!({"query": "x", "apiKey": "REDACTED"})
        );
        assert_eq!(
            capture_key(record["uri"].as_str().unwrap(), Some(&record["json_body"])).unwrap(),
            capture_key(url.as_str(), options.json_body.as_ref()).unwrap()
        );
        // results.json is scrubbed like the captures.
        write_results(root.path(), &[json!({"target": location})]).unwrap();
        let results = std::fs::read_to_string(root.path().join("results.json")).unwrap();
        assert!(results.contains("key=REDACTED") && !results.contains(&maps));
    }
}
