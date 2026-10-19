//! Clinical studies from the ClinicalTrials.gov API (v2), read by the NCT number in the link.
//!
//! Every clinicaltrials.gov link of one study, current or legacy (`/ct2/show/…`, the classic
//! site), and its API URL are aliases of one identifier: `nct:{NCT number}`. The registry asks
//! to be credited with the date of its data, which is the record's last update.
use super::kit::{host_and_segments, int, json, text, texts, time, Card, Link, Request};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

pub const SITE: &str = "ClinicalTrials.gov";
const VIA: &str = "clinicaltrials";
/// The parts of a study record the card reads (the results section can be megabytes).
const FIELDS: &str = "protocolSection,hasResults";
/// How many PubMed references a card keeps.
const MAX_PMIDS: usize = 10;

/// An NCT number (`NCT` and 8 digits), in upper case.
pub fn nct(value: &str) -> Option<String> {
    let value = value.to_ascii_uppercase();
    let digits = value.strip_prefix("NCT")?;
    (digits.len() == 8 && digits.bytes().all(|b| b.is_ascii_digit())).then_some(value)
}

/// The NCT number a ClinicalTrials.gov link names; the path decides, queries are ignored.
pub fn from_uri(uri: &str) -> Option<String> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    let id = match (host.as_str(), s.as_slice()) {
        ("clinicaltrials.gov" | "beta.clinicaltrials.gov", ["study", id])
        | ("clinicaltrials.gov", ["study", id, "history"])
        | ("clinicaltrials.gov", ["show", id])
        | ("clinicaltrials.gov", ["api", "v2", "studies", id])
        | ("clinicaltrials.gov" | "classic.clinicaltrials.gov", ["ct2", "show", id])
        | (
            "clinicaltrials.gov" | "classic.clinicaltrials.gov",
            ["ct2", "show", "record" | "study" | "results", id],
        ) => id,
        ("clinicaltrials.gov", ["study", id, "history", version])
            if version.bytes().all(|b| b.is_ascii_digit()) =>
        {
            id
        }
        _ => return None,
    };
    nct(id)
}

/// The identifier every link form of the study is an alias of.
pub fn identifier(nct: &str) -> String {
    format!("nct:{nct}")
}

/// The study's page on ClinicalTrials.gov.
pub fn page(nct: &str) -> String {
    format!("https://clinicaltrials.gov/study/{nct}")
}

/// The study's API record, without the results section.
pub fn request(nct: &str) -> Option<Request> {
    Request::get(&format!(
        "https://clinicaltrials.gov/api/v2/studies/{}?fields={FIELDS}",
        self::nct(nct)?
    ))
}

/// A date given as `YYYY-MM-DD` (a month-only date gives nothing).
fn full_date(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.as_str().filter(|s| s.len() == 10).and_then(|_| time(v))
}

/// The study card, or `None` when the registry has no such NCT number.
pub fn parse(status: u16, raw: &[u8], nct: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[404])? else {
        return Ok(None);
    };
    let p = &v["protocolSection"];
    let id = &p["identificationModule"];
    ensure!(
        id["nctId"].as_str().and_then(self::nct).as_deref() == Some(nct),
        "ClinicalTrials.gov answered for another study"
    );
    let mut card = Card::new("MedicalTrial", identifier(nct), page(nct));
    card.name = Some(
        text(&id["briefTitle"])
            .or_else(|| text(&id["officialTitle"]))
            .context("no title")?,
    );
    card.description = text(&p["descriptionModule"]["briefSummary"]);
    card.detail("official_title", text(&id["officialTitle"]));

    let status = &p["statusModule"];
    let (start, completion) = (&status["startDateStruct"], &status["completionDateStruct"]);
    card.start = full_date(&start["date"]);
    card.end = full_date(&completion["date"]);
    card.detail("overall_status", text(&status["overallStatus"]));
    card.detail("start_date", text(&start["date"]));
    card.detail("start_date_type", text(&start["type"]));
    card.detail("completion_date", text(&completion["date"]));
    card.detail("completion_date_type", text(&completion["type"]));
    let updated = text(&status["lastUpdatePostDateStruct"]["date"]);
    card.detail("last_update_posted", updated.clone());

    let design = &p["designModule"];
    card.detail("study_type", text(&design["studyType"]));
    card.detail("phases", texts(&design["phases"]));
    card.detail("enrollment", int(&design["enrollmentInfo"]["count"]));
    card.detail("conditions", texts(&p["conditionsModule"]["conditions"]));
    let interventions: Vec<String> = p["armsInterventionsModule"]["interventions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| text(&i["name"]))
        .collect();
    card.detail("interventions", interventions);
    card.detail(
        "sponsor",
        text(&p["sponsorCollaboratorsModule"]["leadSponsor"]["name"]),
    );
    card.detail("has_results", v["hasResults"].as_bool());
    let pmids: Vec<String> = p["referencesModule"]["references"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| text(&r["pmid"]))
        .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))
        .take(MAX_PMIDS)
        .collect();
    card.detail("pmids", pmids);

    card.attribution = Some(match updated {
        Some(date) => format!("ClinicalTrials.gov, data as of {date}"),
        None => "ClinicalTrials.gov".into(),
    });
    card.link(Link::new(
        "WHO ICTRP",
        format!("https://trialsearch.who.int/Trial2.aspx?TrialID={nct}"),
        VIA,
    ));
    Ok(Some(card))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kit::fixture;

    const PFIZER: &str = "NCT04368728";

    fn read(name: &str) -> Vec<u8> {
        fixture::read("clinicaltrials", name)
    }

    #[test]
    fn link_forms() {
        for uri in [
            "https://clinicaltrials.gov/study/NCT04368728",
            "https://www.clinicaltrials.gov/study/NCT04368728?tab=results",
            "https://clinicaltrials.gov/study/nct04368728",
            "https://clinicaltrials.gov/study/NCT04368728/history",
            "https://clinicaltrials.gov/study/NCT04368728/history/3",
            "https://beta.clinicaltrials.gov/study/NCT04368728",
            "https://clinicaltrials.gov/ct2/show/NCT04368728",
            "https://clinicaltrials.gov/ct2/show/NCT04368728?term=vaccine&draw=2#locn",
            "https://clinicaltrials.gov/ct2/show/record/NCT04368728",
            "https://clinicaltrials.gov/ct2/show/study/NCT04368728",
            "https://clinicaltrials.gov/ct2/show/results/NCT04368728",
            "https://classic.clinicaltrials.gov/ct2/show/NCT04368728",
            "https://classic.clinicaltrials.gov/ct2/show/record/NCT04368728",
            "https://clinicaltrials.gov/show/NCT04368728",
            "http://clinicaltrials.gov/show/NCT04368728",
            "https://clinicaltrials.gov/api/v2/studies/NCT04368728",
        ] {
            assert_eq!(
                from_uri(uri).map(|n| identifier(&n)).as_deref(),
                Some("nct:NCT04368728"),
                "{uri}"
            );
        }
        assert_eq!(page(PFIZER), "https://clinicaltrials.gov/study/NCT04368728");
        assert_eq!(
            request("nct04368728").unwrap().url.as_str(),
            "https://clinicaltrials.gov/api/v2/studies/NCT04368728?fields=protocolSection,hasResults"
        );
    }

    #[test]
    fn invalid_links() {
        for uri in [
            "https://clinicaltrials.gov/study/NCT0436872",
            "https://clinicaltrials.gov/study/NCT043687280",
            "https://clinicaltrials.gov/study/04368728",
            "https://clinicaltrials.gov/study/NCT04368728/foo",
            "https://clinicaltrials.gov/search?term=NCT04368728",
            "https://clinicaltrials.gov/ct2/results?term=NCT04368728",
            "https://clinicaltrials.gov/api/v2/studies?query.term=NCT04368728",
            "https://classic.clinicaltrials.gov/study/NCT04368728",
            "https://trialsearch.who.int/Trial2.aspx?TrialID=NCT04368728",
            "https://clinicaltrials.example/study/NCT04368728",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn study() {
        let card = parse(200, &read("NCT04368728.json.gz"), PFIZER)
            .unwrap()
            .unwrap();
        assert_eq!(
            (card.type_name, card.id.as_str(), card.url.as_str()),
            (
                "MedicalTrial",
                "nct:NCT04368728",
                "https://clinicaltrials.gov/study/NCT04368728"
            )
        );
        assert_eq!(
            card.name.as_deref(),
            Some("Study to Describe the Safety, Tolerability, Immunogenicity, and Efficacy of RNA Vaccine Candidates Against COVID-19 in Healthy Individuals")
        );
        assert!(card
            .description
            .as_deref()
            .unwrap()
            .starts_with("This is a Phase 1/2/3, randomized, placebo-controlled"));
        assert_eq!(
            card.start.unwrap().to_rfc3339(),
            "2020-04-29T00:00:00+00:00"
        );
        assert_eq!(card.end.unwrap().to_rfc3339(), "2023-02-10T00:00:00+00:00");
        assert_eq!(card.details["overall_status"], "COMPLETED");
        assert_eq!(
            card.details["phases"],
            serde_json::json!(["PHASE2", "PHASE3"])
        );
        assert_eq!(card.details["study_type"], "INTERVENTIONAL");
        assert_eq!(card.details["sponsor"], "BioNTech SE");
        assert_eq!(card.details["enrollment"], 46969);
        assert_eq!(card.details["start_date_type"], "ACTUAL");
        assert_eq!(card.details["has_results"], true);
        assert_eq!(card.details["interventions"][1], "BNT162b2");
        assert_eq!(card.details["conditions"][1], "COVID-19");
        assert_eq!(card.details["pmids"][0], "40396505");
        assert_eq!(card.details["pmids"].as_array().unwrap().len(), MAX_PMIDS);
        assert_eq!(
            card.attribution.as_deref(),
            Some("ClinicalTrials.gov, data as of 2026-03-25")
        );
        assert_eq!(
            card.also_on[0].uri,
            "https://trialsearch.who.int/Trial2.aspx?TrialID=NCT04368728"
        );
        assert!(card.keys.is_empty());

        let moderna = parse(200, &read("NCT04470427.json.gz"), "NCT04470427")
            .unwrap()
            .unwrap();
        assert_eq!(moderna.details["sponsor"], "ModernaTX, Inc.");
        assert_eq!(
            moderna.details["interventions"],
            serde_json::json!(["mRNA-1273", "Placebo"])
        );
        assert_eq!(
            moderna.end.unwrap().to_rfc3339(),
            "2022-12-29T00:00:00+00:00"
        );
    }

    #[test]
    fn month_only_dates_stay_in_details() {
        let card = parse(200, &read("NCT01278719.json"), "NCT01278719")
            .unwrap()
            .unwrap();
        assert_eq!((card.start, card.end), (None, None));
        assert_eq!(card.details["start_date"], "2010-01");
        assert_eq!(card.details["completion_date"], "2013-06");
    }

    #[test]
    fn missing_and_errors() {
        assert_eq!(
            parse(404, &read("missing.txt"), "NCT99999999").unwrap(),
            None
        );
        assert!(parse(200, &read("NCT04470427.json.gz"), PFIZER).is_err());
        assert!(parse(429, b"", PFIZER).is_err());
        assert!(parse(503, b"", PFIZER).is_err());
        assert!(parse(200, b"not json", PFIZER).is_err());
    }
}
