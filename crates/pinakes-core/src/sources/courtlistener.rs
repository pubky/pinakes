//! US court opinions from CourtListener (Free Law Project), by opinion cluster id, through its
//! keyless search API. Anonymous use is limited to 5 requests per minute. Case law is in the
//! public domain.
use super::{list, text, texts, web_url, Budget, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Courtlistener;

pub const ATTRIBUTION: &str = "Source: CourtListener, Free Law Project";

fn cluster(value: &str) -> Option<u64> {
    (!value.is_empty() && value.len() <= 12 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
        .filter(|n| *n > 0)
}

/// The cluster id of `/opinion/{id}[/{slug}/…]` or `/api/rest/v{3,4}/clusters/{id}/`.
pub fn of_uri(uri: &str) -> Option<u64> {
    let url = web_url(uri)?;
    if !matches!(
        url.host_str()?,
        "www.courtlistener.com" | "courtlistener.com"
    ) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["opinion", id, ..] => cluster(id),
        ["api", "rest", "v3" | "v4", "clusters", id] => cluster(id),
        _ => None,
    }
}

pub fn root(id: u64) -> String {
    format!("https://www.courtlistener.com/opinion/{id}/")
}

/// The card of an opinion cluster from its search result, or `None` when there is none.
pub fn card(
    mut doc: ResourceMeta,
    id: u64,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    anyhow::ensure!(status == 200, "CourtListener answered {status}");
    let v: Value = serde_json::from_slice(body)?;
    let Some(hit) = v["results"]
        .as_array()
        .context("no results")?
        .iter()
        .find(|r| r["cluster_id"].as_u64() == Some(id))
    else {
        return Ok(None);
    };
    doc.set_type("CreativeWork");
    doc.attribution = Some(ATTRIBUTION.into());
    doc.name = Some(text(&hit["caseName"]).context("no case name")?);
    let citations = texts(&hit["citation"]);
    let about: Vec<String> = [
        text(&hit["court"]),
        text(&hit["dateFiled"]),
        citations.first().cloned(),
    ]
    .into_iter()
    .flatten()
    .collect();
    doc.description = (!about.is_empty()).then(|| about.join(" · "));
    for (key, value) in [
        ("court", text(&hit["court"])),
        ("date_filed", text(&hit["dateFiled"])),
        ("citations", list(&citations)),
        ("docket_number", text(&hit["docketNumber"])),
        ("case_name_full", text(&hit["caseNameFull"])),
        ("judge", text(&hit["judge"])),
        ("precedential_status", text(&hit["status"])),
        (
            "page_url",
            text(&hit["absolute_url"])
                .filter(|p| p.starts_with("/opinion/"))
                .map(|p| format!("https://www.courtlistener.com{p}")),
        ),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for Courtlistener {
    fn name(&self) -> &'static str {
        "courtlistener"
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = root(of_uri(uri)?);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "court",
            "date_filed",
            "citations",
            "docket_number",
            "case_name_full",
            "judge",
            "precedential_status",
            "page_url",
        ]
    }
    fn budgets(&self) -> &'static [Budget] {
        // 5 requests per minute without a token.
        &[("www.courtlistener.com", 5.0 / 60.0, Some(1))]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/",
            card_type: "CreativeWork",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not an opinion")?;
        let f = cx
            .fetch(
                &format!(
                    "https://www.courtlistener.com/api/rest/v4/search/?type=o&q=cluster_id%3A{id}"
                ),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(card(cx.card(uri, self), id, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("courtlistener", file)
    }

    #[test]
    fn addresses() {
        for uri in [
            "https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/",
            "https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/authorities/",
            "https://www.courtlistener.com/opinion/2812209",
            "https://www.courtlistener.com/api/rest/v4/clusters/2812209/",
        ] {
            assert_eq!(
                Courtlistener.alias_root(uri).as_deref(),
                Some("https://www.courtlistener.com/opinion/2812209/"),
                "{uri}"
            );
        }
        assert_eq!(Courtlistener.alias_root(&root(2812209)), None);
        for uri in [
            "https://www.courtlistener.com/docket/2668808/obergefell-v-hodges/",
            "https://www.courtlistener.com/opinion/x/",
            "https://example.com/opinion/2812209/",
        ] {
            assert!(!Courtlistener.reads(uri), "{uri}");
        }
    }

    #[test]
    fn opinion_card() {
        let base = ResourceMeta::new(&root(2812209), "courtlistener", 3600);
        let raw = read("search_o_cluster_id_2812209_obergefell.json");
        let doc = card(base.clone(), 2812209, 200, &raw).unwrap().unwrap();
        assert_eq!(doc.type_name, "CreativeWork");
        assert_eq!(doc.name.as_deref(), Some("Obergefell v. Hodges"));
        assert_eq!(
            doc.description.as_deref(),
            Some("Supreme Court of the United States · 2015-06-26 · 192 L. Ed. 2d 609")
        );
        let a = &doc.attributes;
        assert!(a["citations"].contains("576 U.S. 644"));
        assert_eq!(a["docket_number"], "14-556");
        assert_eq!(
            a["page_url"],
            "https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/"
        );
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        // A result for another cluster is not this one.
        assert!(card(base.clone(), 1, 200, &raw).unwrap().is_none());
        let none = read("search_o_cluster_id_999999999_missing.json");
        assert!(card(base.clone(), 999999999, 200, &none).unwrap().is_none());
        assert!(card(base, 2812209, 401, b"{}").is_err());
    }
}
