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
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use url::Url;
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
        let key = format!(
            "{}:{}",
            url,
            options
                .json_body
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default()
        );
        if let Some(saved) = self.responses.lock().unwrap().get(&key) {
            return saved.clone().map_err(anyhow::Error::msg);
        }
        ensure!(
            !self.offline,
            "offline probe has no captured response for {url}"
        );
        let result = PinnedTransport.get_with(url, addresses, options).await;
        let mut record = json!({"uri":url,"at":chrono::Utc::now(),"user_agent":options.user_agent,"feed_hint":options.feed_hint,"json_body":options.json_body});
        match &result {
            Ok(f) => {
                let hash = blake3::hash(&f.body).to_hex().to_string();
                let file = self.root.join(format!("{hash}.raw"));
                if !file.exists() {
                    std::fs::write(&file, &f.body)?;
                }
                record["status"] = json!(f.status);
                record["content_type"] = json!(f.content_type);
                record["bytes"] = json!(f.body.len());
                record["blake3"] = json!(hash);
                record["path"] = json!(file);
                record["location"] = json!(f.location);
                record["validators"] = json!(f.validators);
            }
            Err(e) => record["error"] = json!(e.to_string()),
        }
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
                truncated: false,
                url: uri.parse()?,
                status: v["status"].as_u64().unwrap_or(200) as u16,
                content_type: v["content_type"].as_str().unwrap_or("text/html").into(),
                body: std::fs::read(v["path"].as_str().unwrap())?,
                retry_after_secs: 60,
                location: v["location"].as_str().map(str::to_owned),
                validators: serde_json::from_value(v["validators"].clone()).unwrap_or_default(),
            })
        };
        let key = format!(
            "{}:{}",
            uri,
            v["json_body"]
                .as_object()
                .map(|_| v["json_body"].to_string())
                .unwrap_or_default()
        );
        captures.responses.lock().unwrap().insert(key, result);
    }
    let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
    let cfg = EventConfig {
        geocoding: false,
        ..Default::default()
    };
    let resolver = PlatformResolver {
        web: WebResolver {
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
        let mut result =
            json!({"id":id,"uri":uri,"at":chrono::Utc::now(),"ua_profile":m.ua_profile});
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            resolver.probe_manifest(uri, &m),
        )
        .await
        {
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
            json!({"id":id,"status":result.pointer("/document/status"),"type":result.pointer("/document/type"),"error":result["error"],"missing":result["missing_fields"],"occurrences":result["occurrences"]})
        );
        results.push(result);
        std::fs::write(
            root.join("results.json"),
            serde_json::to_vec_pretty(&results)?,
        )?;
    }
    std::fs::write(
        root.join("results.json"),
        serde_json::to_vec_pretty(&results)?,
    )?;
    Ok(())
}
