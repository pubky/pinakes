//! One GET per supplied URL, through production egress, saved for offline parser replay.
use pinakes_core::{
    config::{EgressConfig, EventConfig},
    event::{calendar, page},
    model::ResourceMeta,
    resolver::egress::{Egress, FetchOptions, Fetched},
};
use serde_json::{json, Value};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 3,
        "event_probe MANIFEST.json OUTPUT_DIRECTORY"
    );
    let manifest: Vec<Value> = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    std::fs::create_dir_all(&args[2])?;
    let egress = Egress::new(EgressConfig::default());
    let mut results = vec![];
    for entry in manifest {
        let name = entry["name"].as_str().unwrap();
        let uri = entry["uri"].as_str().unwrap();
        let kind = entry["kind"].as_str().unwrap();
        let options = FetchOptions {
            user_agent: entry["user_agent"].as_str().map(str::to_owned),
            feed_cap: (kind == "calendar" || entry["feed_hint"] == true)
                .then_some(32 * 1024 * 1024),
            feed_hint: kind == "calendar" || entry["feed_hint"] == true,
            accept: (kind == "ap").then(|| "application/activity+json".into()),
            ..Default::default()
        };
        let mut result = json!({"name":name,"uri":uri,"kind":kind,"at":chrono::Utc::now()});
        let fetch = if let Some(path) = entry["replay"].as_str() {
            result["replayed_from"] = json!(path);
            Ok(Fetched {
                truncated: false,
                url: uri.parse()?,
                status: 200,
                content_type: entry["content_type"]
                    .as_str()
                    .unwrap_or("text/plain")
                    .into(),
                body: std::fs::read(path)?,
                validators: Default::default(),
                location: None,
                retry_after_secs: 60,
            })
        } else {
            egress.fetch_with(uri, &options).await
        };
        let now = entry["now"]
            .as_str()
            .map(str::parse)
            .transpose()?
            .unwrap_or_else(chrono::Utc::now);
        result["horizon_at"] = json!(now);
        match fetch {
            Err(error) => result["error"] = json!(error.to_string()),
            Ok(f) => {
                std::fs::write(format!("{}/{name}.raw", args[2]), &f.body)?;
                result["http_status"] = json!(f.status);
                result["final_url"] = json!(f.url);
                result["bytes"] = json!(f.body.len());
                result["content_type"] = json!(f.content_type);
                result["blake3"] = json!(blake3::hash(&f.body).to_hex().as_str());
                if f.status == 200 {
                    let parsed = if kind == "calendar" {
                        calendar::parse(uri, &f.body, &EventConfig::default(), now)
                    } else if let Some(adapter) = entry["adapter"].as_str() {
                        if adapter == "wikidata" {
                            pinakes_core::event::adapters::service::wiki_value(
                                uri,
                                &serde_json::from_slice(&f.body)?,
                            )
                        } else {
                            pinakes_core::event::adapters::state::parse(adapter, uri, &f.body)
                                .ok_or_else(|| anyhow::anyhow!("no platform event"))
                        }
                    } else {
                        let mut d = ResourceMeta::new(uri, "web", 3600);
                        let text = String::from_utf8_lossy(&f.body);
                        if kind == "ap" {
                            if let Ok(v) = serde_json::from_str(&text) {
                                page::activity(&mut d, &v, &f.url);
                            }
                        } else {
                            let html = scraper::Html::parse_document(&text);
                            for node in html.select(
                                &scraper::Selector::parse("script[type='application/ld+json']")
                                    .unwrap(),
                            ) {
                                if let Some(v) = page::lenient_json(&node.inner_html()) {
                                    page::jsonld(&mut d, &v, &f.url);
                                }
                            }
                            page::microformats(&mut d, &html, &f.url);
                        }
                        page::finalize(&mut d, true);
                        d.bounded(&[])
                    };
                    match parsed {
                        Ok(doc) => {
                            result["document"] = serde_json::to_value(&doc)?;
                            if let Some(c) = doc.container {
                                result["occurrences"] = json!(c.occurrences.len());
                                result["warnings"] = json!(c.warnings);
                                result["samples"]=json!(c.occurrences.iter().take(3).map(|o|json!({"uid":o.uid,"recurrence_id":o.recurrence_id,"document":o.doc})).collect::<Vec<_>>());
                            }
                            result["rrule_lines"] = json!(String::from_utf8_lossy(&f.body)
                                .lines()
                                .filter(|s| s.starts_with("RRULE:"))
                                .count());
                        }
                        Err(e) => result["parse_error"] = json!(e.to_string()),
                    }
                }
            }
        }
        println!(
            "{}",
            json!({"name":name,"http_status":result["http_status"],"error":result["error"],"parse_error":result["parse_error"],"occurrences":result["occurrences"]})
        );
        results.push(result);
    }
    std::fs::write(
        format!("{}/results.json", args[2]),
        serde_json::to_vec_pretty(&results)?,
    )?;
    Ok(())
}
