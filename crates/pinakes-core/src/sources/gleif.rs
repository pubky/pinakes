//! GLEIF: Legal Entity Identifier records from the Global LEI Foundation's API. CC0; no key;
//! 60 requests per minute.
use super::{list, text, texts, web_url, Budget, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Gleif;

/// A valid LEI (ISO 17442: 18 characters, 2 check digits, MOD 97-10), in upper case.
pub fn lei(value: &str) -> Option<String> {
    let lei = value.trim().to_ascii_uppercase();
    // Bytes, not string slices: a link may carry any character.
    let b = lei.as_bytes();
    let shape = b.len() == 20
        && b[..18].iter().all(u8::is_ascii_alphanumeric)
        && b[18..].iter().all(u8::is_ascii_digit);
    if !shape {
        return None;
    }
    let mut rest = 0u32;
    for b in lei.bytes() {
        rest = if b.is_ascii_digit() {
            (rest * 10 + u32::from(b - b'0')) % 97
        } else {
            (rest * 100 + u32::from(b - b'A' + 10)) % 97
        };
    }
    (rest == 1).then_some(lei)
}

/// The LEI of `lei:{LEI}`, `https://www.gleif.org/lei/{LEI}` or
/// `https://api.gleif.org/api/v1/lei-records/{LEI}` (its sub-resources, such as a parent, are
/// other objects).
pub fn of_uri(uri: &str) -> Option<String> {
    if let Some(rest) = uri.strip_prefix("lei:") {
        return lei(rest);
    }
    let url = web_url(uri)?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (url.host_str()?, segments.as_slice()) {
        ("www.gleif.org" | "gleif.org", [.., "lei", id]) => lei(id),
        ("api.gleif.org", ["api", "v1", "lei-records", id]) => lei(id),
        _ => None,
    }
}

pub fn api_url(lei: &str) -> String {
    format!("https://api.gleif.org/api/v1/lei-records/{lei}")
}

/// The card of a record, or `None` when GLEIF has no such LEI.
pub fn card(
    mut doc: ResourceMeta,
    lei: &str,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let Some(v) = super::kit::json(status, body, &[404])? else {
        return Ok(None);
    };
    let a = &v["data"]["attributes"];
    anyhow::ensure!(a["lei"] == lei, "GLEIF answered for another LEI");
    let entity = &a["entity"];
    doc.set_type("Organization");
    doc.site = "GLEIF".into();
    doc.name = text(&entity["legalName"]["name"]);
    let address = &entity["legalAddress"];
    for (key, value) in [
        (
            "street",
            list(&texts(&address["addressLines"])).map(|s| s.replace("; ", ", ")),
        ),
        ("postcode", text(&address["postalCode"])),
        ("city", text(&address["city"])),
        (
            "country_code",
            text(&address["country"]).map(|c| c.to_ascii_lowercase()),
        ),
    ] {
        if let Some(value) = value {
            doc.address.insert(key.into(), value);
        }
    }
    let place = [text(&address["city"]), text(&address["country"])]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    let form = text(&entity["legalForm"]["id"]).or_else(|| text(&entity["legalForm"]["other"]));
    let about: Vec<String> = [
        Some(format!("LEI {lei}")),
        (!place.is_empty()).then_some(place),
        text(&entity["status"]).map(|s| s.to_lowercase()),
    ]
    .into_iter()
    .flatten()
    .collect();
    doc.description = Some(about.join(" · "));
    let date = |v: &Value| text(v).map(|s| s.chars().take(10).collect::<String>());
    for (key, value) in [
        ("lei", Some(lei.to_owned())),
        ("jurisdiction", text(&entity["jurisdiction"])),
        ("legal_form", form),
        ("entity_status", text(&entity["status"])),
        ("registration_status", text(&a["registration"]["status"])),
        ("registered_as", text(&entity["registeredAs"])),
        ("registered_at", text(&entity["registeredAt"]["id"])),
        ("bic", list(&texts(&a["bic"]))),
        ("opencorporates", text(&a["ocid"])),
        ("last_update", date(&a["registration"]["lastUpdateDate"])),
        (
            "page_url",
            Some(format!("https://search.gleif.org/#/record/{lei}")),
        ),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for Gleif {
    fn name(&self) -> &'static str {
        "gleif"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "gleif",
                uri: "lei:KY37LUS27QQX7BB93L28",
                card_type: "Organization",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "api.gleif.org",
                path: "/api/v1/lei-records/KY37LUS27QQX7BB93L28",
                query: &[],
                header: None,
                content_type: "application/vnd.api+json",
                file: "lei_KY37LUS27QQX7BB93L28_nestle.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = format!("lei:{}", of_uri(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.strip_prefix("lei:")
            .is_some_and(|rest| lei(rest).is_none())
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "lei",
            "jurisdiction",
            "legal_form",
            "entity_status",
            "registration_status",
            "registered_as",
            "registered_at",
            "bic",
            "opencorporates",
            "last_update",
            "page_url",
        ]
    }
    fn budgets(&self) -> &'static [Budget] {
        &[("api.gleif.org", 1.0, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "lei:KY37LUS27QQX7BB93L28",
                card_type: "Organization",
            },
            Example {
                uri: "https://api.gleif.org/api/v1/lei-records/5493001KJTIIGC8Y1R12",
                card_type: "Organization",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let lei = of_uri(uri).context("not an LEI")?;
        let f = cx
            .fetch(
                &api_url(&lei),
                &FetchOptions {
                    accept: Some("application/vnd.api+json".into()),
                    extra_types: &["application/vnd.api+json"],
                    ..Default::default()
                },
            )
            .await?;
        Ok(card(cx.card(uri, self), &lei, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("gleif", file)
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            lei("KY37LUS27QQX7BB93L28").as_deref(),
            Some("KY37LUS27QQX7BB93L28")
        );
        assert_eq!(
            lei("ky37lus27qqx7bb93l28").as_deref(),
            Some("KY37LUS27QQX7BB93L28")
        );
        assert_eq!(
            lei("5493001KJTIIGC8Y1R12").as_deref(),
            Some("5493001KJTIIGC8Y1R12")
        );
        for bad in [
            "KY37LUS27QQX7BB93L29",
            "KY37LUS27QQX7BB93L2",
            "KY37LUS27QQX7BB93LAB",
            "",
            "AAAAAAAAAAAAAAAAAá1",
            "AAAAAAAAAAAAAAAAAA1á",
        ] {
            assert_eq!(lei(bad), None, "{bad}");
        }
        for uri in [
            "https://www.gleif.org/lei/KY37LUS27QQX7BB93L28",
            "https://api.gleif.org/api/v1/lei-records/KY37LUS27QQX7BB93L28",
            "lei:ky37lus27qqx7bb93l28",
        ] {
            assert_eq!(
                Gleif.alias_root(uri).as_deref(),
                Some("lei:KY37LUS27QQX7BB93L28"),
                "{uri}"
            );
        }
        assert_eq!(Gleif.alias_root("lei:KY37LUS27QQX7BB93L28"), None);
        for other in [
            "direct-parent",
            "ultimate-parent",
            "direct-children",
            "isins",
        ] {
            let uri =
                format!("https://api.gleif.org/api/v1/lei-records/KY37LUS27QQX7BB93L28/{other}");
            assert!(!Gleif.reads(&uri), "{uri}");
        }
        assert!(Gleif.invalid("lei:KY37LUS27QQX7BB93L29"));
        // The search page names the record in its fragment, which a resource's identity drops.
        assert!(!Gleif.reads("https://search.gleif.org/#/record/KY37LUS27QQX7BB93L28"));
    }

    #[test]
    fn records() {
        let base = |uri: &str| ResourceMeta::new(uri, "gleif", 3600);
        let doc = card(
            base("lei:KY37LUS27QQX7BB93L28"),
            "KY37LUS27QQX7BB93L28",
            200,
            &read("lei_KY37LUS27QQX7BB93L28_nestle.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "Organization");
        assert_eq!(doc.name.as_deref(), Some("NESTLÉ S.A."));
        assert_eq!(doc.site, "GLEIF");
        assert_eq!(
            doc.description.as_deref(),
            Some("LEI KY37LUS27QQX7BB93L28 · VEVEY, CH · active")
        );
        assert_eq!(doc.address["city"], "VEVEY");
        assert_eq!(doc.address["country_code"], "ch");
        assert_eq!(doc.address["street"], "AVENUE NESTLE 55");
        let a = &doc.attributes;
        assert_eq!(a["registered_as"], "CHE-105.909.036");
        assert_eq!(a["legal_form"], "MVII");
        assert_eq!(a["bic"], "NESNCH22XXX");
        assert_eq!(a["registration_status"], "ISSUED");
        assert_eq!(a["last_update"], "2026-08-18");
        let doc = card(
            base("lei:5493001KJTIIGC8Y1R12"),
            "5493001KJTIIGC8Y1R12",
            200,
            &read("lei_5493001KJTIIGC8Y1R12_bloomberg_finance.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.attributes["jurisdiction"], "US-DE");
        assert!(card(
            base("lei:529900ZZZZZZZZZZZZ46"),
            "529900ZZZZZZZZZZZZ46",
            404,
            &read("lei_529900ZZZZZZZZZZZZ46_unknown_vndjson.json")
        )
        .unwrap()
        .is_none());
        let nestle = read("lei_KY37LUS27QQX7BB93L28_nestle.json");
        assert!(card(base("x:y"), "5493001KJTIIGC8Y1R12", 200, &nestle).is_err());
    }
}
