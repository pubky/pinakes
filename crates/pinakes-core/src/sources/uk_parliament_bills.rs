//! UK Parliament bills from the Bills API, by bill id. Open Parliament Licence v3.0; no key.
use super::{list, text, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct UkParliamentBills;

pub const ATTRIBUTION: &str =
    "Contains Parliamentary information licensed under the Open Parliament Licence v3.0.";

fn id(value: &str) -> Option<u32> {
    super::kit::number(value, 7)
}

/// The bill id of `https://bills.parliament.uk/bills/{id}` or one of its tabs
/// (`/bills/{id}/stages`, `/publications`, `/news`, …), or `…/api/v1/Bills/{id}`. Deeper pages,
/// such as one amendment, are other objects.
pub fn of_uri(uri: &str) -> Option<u32> {
    let url = web_url(uri)?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (url.host_str()?, segments.as_slice()) {
        ("bills.parliament.uk", [bills, n] | [bills, n, _])
            if bills.eq_ignore_ascii_case("bills") =>
        {
            id(n)
        }
        ("bills-api.parliament.uk", ["api", "v1", bills, n])
            if bills.eq_ignore_ascii_case("bills") =>
        {
            id(n)
        }
        _ => None,
    }
}

pub fn root(id: u32) -> String {
    format!("https://bills.parliament.uk/bills/{id}")
}

/// The Bills API's bill types (`/api/v1/BillTypes`), which change rarely.
fn bill_type(id: u64) -> Option<&'static str> {
    Some(match id {
        1 => "Government Bill",
        2 => "Private Members' Bill (Starting in the House of Lords)",
        3 => "Consolidation Bill",
        4 => "Hybrid Bill",
        5 => "Private Members' Bill (under the Ten Minute Rule)",
        6 => "Private Bill",
        7 => "Private Members' Bill (Ballot Bill)",
        8 => "Private Members' Bill (Presentation Bill)",
        9 => "Draft Bill",
        10 => "Supply and Appropriation Bill",
        _ => return None,
    })
}

/// The card of a bill, or `None` when the API has no such bill.
pub fn card(
    mut doc: ResourceMeta,
    id: u32,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    let Some(v) = super::kit::json(status, body, &[404])? else {
        return Ok(None);
    };
    anyhow::ensure!(
        v["billId"].as_u64() == Some(u64::from(id)),
        "the API answered for another bill"
    );
    doc.set_type("Legislation");
    doc.attribution = Some(ATTRIBUTION.into());
    doc.name = text(&v["shortTitle"]).context("no title")?.into();
    doc.description = text(&v["longTitle"]);
    let sponsors: Vec<String> = v["sponsors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| text(&s["member"]["name"]).or_else(|| text(&s["organisation"]["name"])))
        .collect();
    let flag = |key: &str| (v[key] == true).then(|| "true".to_owned());
    for (key, value) in [
        (
            "bill_type",
            v["billTypeId"]
                .as_u64()
                .and_then(bill_type)
                .map(str::to_owned),
        ),
        ("current_stage", text(&v["currentStage"]["description"])),
        ("current_house", text(&v["currentHouse"])),
        ("originating_house", text(&v["originatingHouse"])),
        ("is_act", flag("isAct")),
        ("defeated", flag("isDefeated")),
        (
            "withdrawn",
            text(&v["billWithdrawn"]).map(|d| d.chars().take(10).collect()),
        ),
        ("former_title", text(&v["formerShortTitle"])),
        ("sponsors", list(&sponsors)),
        (
            "last_update",
            text(&v["lastUpdate"]).map(|d| d.chars().take(10).collect()),
        ),
        ("page_url", Some(root(id))),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for UkParliamentBills {
    fn name(&self) -> &'static str {
        "uk_parliament_bills"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "uk_parliament_bills",
                uri: "https://bills.parliament.uk/bills/3737",
                card_type: "Legislation",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "bills-api.parliament.uk",
                path: "/api/v1/Bills/3737",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "bill_3737_employment_rights_act_2025.json",
            }],
        })
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
            "bill_type",
            "current_stage",
            "current_house",
            "originating_house",
            "is_act",
            "defeated",
            "withdrawn",
            "former_title",
            "sponsors",
            "last_update",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://bills.parliament.uk/bills/3737",
            card_type: "Legislation",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not a bill")?;
        let f = cx
            .fetch(
                &format!("https://bills-api.parliament.uk/api/v1/Bills/{id}"),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    extra_types: &["text/plain"],
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
        crate::sources::fixture::read("uk_parliament_bills", file)
    }

    #[test]
    fn addresses() {
        for uri in [
            "https://bills.parliament.uk/bills/3737/stages",
            "https://bills.parliament.uk/bills/3737/publications",
            "https://bills-api.parliament.uk/api/v1/Bills/3737",
        ] {
            assert_eq!(
                UkParliamentBills.alias_root(uri).as_deref(),
                Some("https://bills.parliament.uk/bills/3737"),
                "{uri}"
            );
        }
        assert_eq!(
            UkParliamentBills.alias_root("https://bills.parliament.uk/bills/3737"),
            None
        );
        for uri in [
            "https://bills.parliament.uk/bills",
            "https://bills.parliament.uk/bills/0",
            "https://bills.parliament.uk/bills/x",
            "https://services.parliament.uk/bills/2017-19/x.html",
            "https://bills.parliament.uk/bills/3737/stages/20388/amendments",
            "https://bills.parliament.uk/bills/3737/publications/12345/amendment-paper",
            "https://bills-api.parliament.uk/api/v1/Bills/3737/Stages",
        ] {
            assert!(!UkParliamentBills.reads(uri), "{uri}");
        }
    }

    #[test]
    fn bill_card() {
        let base = ResourceMeta::new(&root(3737), "uk_parliament_bills", 3600);
        let raw = read("bill_3737_employment_rights_act_2025.json");
        let doc = card(base.clone(), 3737, 200, &raw).unwrap().unwrap();
        assert_eq!(doc.type_name, "Legislation");
        assert_eq!(doc.name.as_deref(), Some("Employment Rights Act 2025"));
        assert!(doc
            .description
            .unwrap()
            .starts_with("A Bill to make provision"));
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        let a = &doc.attributes;
        assert_eq!(a["bill_type"], "Government Bill");
        assert_eq!(a["current_stage"], "Royal Assent");
        assert_eq!(a["is_act"], "true");
        assert!(!a.contains_key("defeated"));
        assert_eq!(a["sponsors"].split("; ").next(), Some("Jonathan Reynolds"));
        assert_eq!(a["last_update"], "2025-12-22");
        assert!(card(base.clone(), 3738, 200, &raw).is_err());
        let missing = read("bill_999999_missing.txt");
        assert!(card(base, 999999, 404, &missing).unwrap().is_none());
    }
}
