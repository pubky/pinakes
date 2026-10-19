//! Swiss Parliament business items (Curia Vista: motions, postulates, bills, interpellations…)
//! from the Parliament's OData service, by affair id. Open data; no key.
use super::{list, text, web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct SwissParliament;

pub const ATTRIBUTION: &str = "Source: Swiss Parliament, Parliamentary Services (parlament.ch)";

/// The fields a card needs; the full item carries several long texts.
const SELECT: &str = "ID,Language,BusinessShortNumber,BusinessTypeName,BusinessTypeAbbreviation,\
Title,SubmittedBy,BusinessStatusText,BusinessStatusDate,SubmissionDate,SubmissionCouncilName,\
ResponsibleDepartmentAbbreviation,TagNames";

/// An affair id: the four-digit year and a four-digit number (`22.3765` is `20223765`).
fn affair(value: &str) -> Option<u32> {
    (value.len() == 8
        && (value.starts_with("19") || value.starts_with("20"))
        && value.bytes().all(|b| b.is_ascii_digit()))
    .then(|| value.parse().ok())
    .flatten()
}

/// The affair id of a parlament.ch business page, its legacy page or the OData entity.
pub fn of_uri(uri: &str) -> Option<u32> {
    let url = web_url(uri)?;
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.into_owned())
    };
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (url.host_str()?, segments.as_slice()) {
        (
            "www.parlament.ch" | "parlament.ch",
            [lang, "ratsbetrieb", "suche-curia-vista", "geschaeft"],
        ) if matches!(*lang, "de" | "fr" | "it" | "en" | "rm") => affair(&query("AffairId")?),
        (
            "www.parlament.ch" | "parlament.ch",
            [lang, "suche", "seiten" | "pages", "geschaefte.aspx"],
        ) if matches!(*lang, "d" | "f" | "i") => affair(&query("gesch_id")?),
        ("ws.parlament.ch", [svc, entity]) if svc.eq_ignore_ascii_case("odata.svc") => {
            let inner = entity.strip_prefix("Business(")?.strip_suffix(')')?;
            let id = inner
                .split(',')
                .find_map(|p| p.trim().strip_prefix("ID="))?;
            affair(id)
        }
        _ => None,
    }
}

pub fn root(id: u32) -> String {
    format!("https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId={id}")
}

/// `/Date(1655337600000)/` as `2022-06-16` (UTC).
fn odata_date(v: &Value) -> Option<String> {
    let ms: i64 = v
        .as_str()?
        .strip_prefix("/Date(")?
        .strip_suffix(")/")?
        .parse()
        .ok()?;
    let at = chrono::DateTime::from_timestamp_millis(ms)?;
    Some(at.format("%Y-%m-%d").to_string())
}

/// The card of a business item, or `None` when the service has no such item.
pub fn card(
    mut doc: ResourceMeta,
    id: u32,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    match status {
        200 => {}
        404 => return Ok(None),
        s => anyhow::bail!("parlament.ch answered {s}"),
    }
    let v: Value = serde_json::from_slice(body)?;
    let d = &v["d"];
    anyhow::ensure!(
        d["ID"].as_u64() == Some(u64::from(id)),
        "the service answered for another item"
    );
    doc.set_type("Legislation");
    doc.attribution = Some(ATTRIBUTION.into());
    doc.name = Some(text(&d["Title"]).context("no title")?);
    let about: Vec<String> = [
        [
            text(&d["BusinessTypeAbbreviation"]),
            text(&d["BusinessShortNumber"]),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" "),
        text(&d["SubmittedBy"]).unwrap_or_default(),
        text(&d["SubmissionCouncilName"]).unwrap_or_default(),
        text(&d["BusinessStatusText"]).unwrap_or_default(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();
    doc.description = (!about.is_empty()).then(|| about.join(" · "));
    let tags: Vec<String> = text(&d["TagNames"])
        .map(|t| t.split('|').map(str::to_owned).collect())
        .unwrap_or_default();
    for (key, value) in [
        ("business_number", text(&d["BusinessShortNumber"])),
        ("business_type", text(&d["BusinessTypeName"])),
        ("status", text(&d["BusinessStatusText"])),
        ("status_date", odata_date(&d["BusinessStatusDate"])),
        ("submitted_by", text(&d["SubmittedBy"])),
        ("submission_date", odata_date(&d["SubmissionDate"])),
        ("council", text(&d["SubmissionCouncilName"])),
        ("department", text(&d["ResponsibleDepartmentAbbreviation"])),
        ("tags", list(&tags)),
        ("page_url", Some(root(id))),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for SwissParliament {
    fn name(&self) -> &'static str {
        "swiss_parliament"
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
            "business_number",
            "business_type",
            "status",
            "status_date",
            "submitted_by",
            "submission_date",
            "council",
            "department",
            "tags",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId=20223765",
            card_type: "Legislation",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not a business item")?;
        let url = format!(
            "https://ws.parlament.ch/odata.svc/Business(ID={id},Language='DE')?$select={SELECT}&$format=json"
        );
        let f = cx
            .fetch(
                &url,
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
        crate::sources::fixture::read("swiss_parliament", file)
    }

    #[test]
    fn addresses() {
        let root = root(20223765);
        for uri in [
            "https://www.parlament.ch/fr/ratsbetrieb/suche-curia-vista/geschaeft?AffairId=20223765",
            "https://www.parlament.ch/en/ratsbetrieb/suche-curia-vista/geschaeft?affairid=20223765",
            "https://www.parlament.ch/d/suche/seiten/geschaefte.aspx?gesch_id=20223765",
            "https://ws.parlament.ch/OData.svc/Business(ID=20223765,Language='FR')",
        ] {
            assert_eq!(
                SwissParliament.alias_root(uri).as_deref(),
                Some(root.as_str()),
                "{uri}"
            );
        }
        assert_eq!(SwissParliament.alias_root(&root), None);
        for uri in [
            "https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId=223765",
            "https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista",
            "https://www.parlament.ch/xx/ratsbetrieb/suche-curia-vista/geschaeft?AffairId=20223765",
        ] {
            assert!(!SwissParliament.reads(uri), "{uri}");
        }
    }

    #[test]
    fn business_card() {
        let base = ResourceMeta::new(&root(20223765), "swiss_parliament", 3600);
        let raw = read("business_20223765_DE_select.json");
        let doc = card(base.clone(), 20223765, 200, &raw).unwrap().unwrap();
        assert_eq!(doc.type_name, "Legislation");
        assert!(doc
            .name
            .as_deref()
            .unwrap()
            .starts_with("Unter welchen Bedingungen kann Arbeitszeitreduktion"));
        assert_eq!(
            doc.description.as_deref(),
            Some("Po. 22.3765 · Glättli Balthasar · Nationalrat · Erledigt")
        );
        let a = &doc.attributes;
        assert_eq!(a["business_number"], "22.3765");
        assert_eq!(a["business_type"], "Postulat");
        assert_eq!(a["submission_date"], "2022-06-16");
        assert_eq!(a["status_date"], "2024-02-28");
        assert_eq!(a["department"], "WBF");
        assert!(a["tags"].starts_with("Wirtschaft; Soziale Fragen"));
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));
        assert!(card(base.clone(), 20223766, 200, &raw).is_err());
        let missing = read("business_99999999_DE_missing.json");
        assert!(card(base, 99999999, 404, &missing).unwrap().is_none());
    }
}
