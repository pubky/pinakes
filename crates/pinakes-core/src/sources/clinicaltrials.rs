//! Clinical studies from the ClinicalTrials.gov API (v2), read by the NCT number in the link.
//!
//! Every clinicaltrials.gov link of one study, current or legacy (`/ct2/show/…`, the classic
//! site), and its API URL are aliases of one identifier: `nct:{NCT number}`. The registry asks
//! to be credited with the date of its data, which is the record's last update.
use super::kit::{host_and_segments, int, json, text, texts, time, web_url, Card, Link, Request};
use super::{Example, Source};
use crate::model::ResourceMeta;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
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

/// The NCT number a ClinicalTrials.gov link names; the path decides, and queries are ignored
/// except a past version's (`?tab=history&a={n}`), which is not the study itself.
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
        _ => return None,
    };
    // One past version of the record (`?tab=history&a=3`) is a dated version, not the study.
    let version = web_url(uri)?.query_pairs().any(|(k, _)| k == "a");
    (!version).then(|| nct(id)).flatten()
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
        "https://clinicaltrials.gov/api/v2/studies?filter.ids={}&fields={FIELDS}&pageSize=2",
        self::nct(nct)?
    ))
}

/// The NCT number of a root (`nct:NCT04368728`, in upper case).
pub fn from_root(uri: &str) -> Option<String> {
    let id = uri.strip_prefix("nct:")?;
    nct(id).filter(|n| n == id)
}

/// The NCT number of a root or a ClinicalTrials.gov link.
pub fn target(uri: &str) -> Option<String> {
    from_root(uri).or_else(|| from_uri(uri))
}

/// A date given as `YYYY-MM-DD` (a month-only date gives nothing).
fn full_date(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.as_str().filter(|s| s.len() == 10).and_then(|_| time(v))
}

/// The study card, or `None` when the registry has no such NCT number.
pub fn parse(status: u16, raw: &[u8], asked: &str) -> Result<Option<Card>> {
    let Some(v) = json(status, raw, &[])? else {
        return Ok(None);
    };
    let studies = v["studies"].as_array().context("no studies")?;
    // The study whose number, or one of whose former numbers (`nctIdAliases`), is the one asked.
    let names = |s: &&Value| {
        let id = &s["protocolSection"]["identificationModule"];
        id["nctId"].as_str().and_then(self::nct).as_deref() == Some(asked)
            || texts(&id["nctIdAliases"])
                .iter()
                .any(|a| self::nct(a).as_deref() == Some(asked))
    };
    let found: Vec<&Value> = studies.iter().filter(names).collect();
    let v = match found.as_slice() {
        [] if studies.is_empty() => return Ok(None),
        [study] if studies.len() == 1 => *study,
        _ => bail!("ClinicalTrials.gov answered for another study"),
    };
    let p = &v["protocolSection"];
    let id = &p["identificationModule"];
    // A former number's card is the study's own: the link becomes its alias.
    let nct = id["nctId"]
        .as_str()
        .and_then(self::nct)
        .context("no NCT number")?;
    let nct = nct.as_str();
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

/// The source: ClinicalTrials.gov studies, one request per card.
pub struct ClinicalTrials;

#[async_trait]
impl Source for ClinicalTrials {
    fn name(&self) -> &'static str {
        "clinicaltrials"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "clinicaltrials",
                uri: "https://clinicaltrials.gov/study/NCT04368728",
                card_type: "MedicalTrial",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "clinicaltrials.gov",
                path: "/api/v2/studies",
                query: &[("filter.ids", "NCT04368728")],
                header: None,
                content_type: "application/json",
                file: "search-NCT04368728.json.gz",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        target(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = identifier(&target(uri)?);
        (root != uri).then_some(root)
    }
    fn invalid(&self, uri: &str) -> bool {
        uri.starts_with("nct:") && from_root(uri).is_none()
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "page_url",
            "official_title",
            "overall_status",
            "start_date",
            "start_date_type",
            "completion_date",
            "completion_date_type",
            "last_update_posted",
            "study_type",
            "phases",
            "enrollment",
            "conditions",
            "interventions",
            "sponsor",
            "has_results",
            "pmids",
        ]
    }
    fn budgets(&self) -> &'static [super::Budget] {
        &[("clinicaltrials.gov", 0.8, None)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://clinicaltrials.gov/study/NCT04368728",
            card_type: "MedicalTrial",
        }]
    }
    async fn resolve(&self, cx: &super::Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let Some(nct) = target(uri) else {
            return Ok(cx.missing(uri, self));
        };
        let f = super::kit::fetch(cx, &request(&nct).context("no request")?).await?;
        Ok(match parse(f.status, &f.body, &nct)? {
            Some(card) => card.into_doc(cx.card(uri, self), SITE),
            None => cx.missing(uri, self),
        })
    }
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
        // Past versions of the record are dated versions, not the study.
        for uri in [
            "https://clinicaltrials.gov/study/NCT04368728?tab=history&a=3",
            "https://clinicaltrials.gov/study/NCT04368728/history/3",
        ] {
            assert_eq!(from_uri(uri), None, "{uri}");
        }
        assert_eq!(page(PFIZER), "https://clinicaltrials.gov/study/NCT04368728");
        assert_eq!(
            request("nct04368728").unwrap().url.as_str(),
            "https://clinicaltrials.gov/api/v2/studies?filter.ids=NCT04368728&fields=protocolSection,hasResults&pageSize=2"
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
        let card = parse(200, &read("search-NCT04368728.json.gz"), PFIZER)
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

        let moderna = parse(200, &read("search-NCT04470427.json.gz"), "NCT04470427")
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
        let card = parse(200, &read("search-NCT01278719.json"), "NCT01278719")
            .unwrap()
            .unwrap();
        assert_eq!((card.start, card.end), (None, None));
        assert_eq!(card.details["start_date"], "2010-01");
        assert_eq!(card.details["completion_date"], "2013-06");
    }

    #[test]
    fn a_former_number_is_the_study() {
        // NCT04482322 was merged into NCT04140292: the search answers with that study, which
        // lists the former number, so the card is the study's and the link becomes its alias.
        let card = parse(200, &read("search-NCT04482322.json.gz"), "NCT04482322")
            .unwrap()
            .unwrap();
        assert_eq!(card.id, "nct:NCT04140292");
        assert_eq!(card.url, "https://clinicaltrials.gov/study/NCT04140292");
        // Its own number reads the same study.
        let same = parse(200, &read("search-NCT04482322.json.gz"), "NCT04140292")
            .unwrap()
            .unwrap();
        assert_eq!(same.id, card.id);
        // A study that neither is nor was the number asked is no answer.
        assert!(parse(200, &read("search-NCT04482322.json.gz"), "NCT04368728").is_err());
        assert_eq!(
            request("nct04482322").unwrap().url.as_str(),
            format!(
                "https://clinicaltrials.gov/api/v2/studies?filter.ids=NCT04482322&fields={FIELDS}&pageSize=2"
            )
        );
    }

    #[test]
    fn missing_and_errors() {
        assert_eq!(
            parse(200, &read("search-NCT99999999.json"), "NCT99999999").unwrap(),
            None
        );
        assert!(parse(200, &read("search-NCT04470427.json.gz"), PFIZER).is_err());
        assert!(parse(429, b"", PFIZER).is_err());
        assert!(parse(503, b"", PFIZER).is_err());
        assert!(parse(200, b"not json", PFIZER).is_err());
    }

    #[test]
    fn roots_and_aliases() {
        let c = ClinicalTrials;
        assert!(c.reads("nct:NCT04368728"));
        assert_eq!(c.alias_root("nct:NCT04368728"), None);
        assert!(!c.invalid("nct:NCT04368728"));
        assert_eq!(
            c.alias_root("https://classic.clinicaltrials.gov/ct2/show/NCT04368728?term=x")
                .as_deref(),
            Some("nct:NCT04368728")
        );
        for uri in ["nct:nct04368728", "nct:NCT0436872", "nct:04368728", "nct:"] {
            assert!(c.invalid(uri), "{uri}");
            assert!(!c.reads(uri), "{uri}");
        }
        for example in c.examples() {
            assert!(c.reads(example.uri), "{}", example.uri);
        }
    }

    #[test]
    fn documents() {
        for (file, nct) in [
            ("search-NCT04368728.json.gz", PFIZER),
            ("search-NCT04470427.json.gz", "NCT04470427"),
            ("search-NCT01278719.json", "NCT01278719"),
            ("search-NCT04482322.json.gz", "NCT04482322"),
        ] {
            let card = parse(200, &read(file), nct).unwrap().unwrap();
            let doc = crate::sources::kit::checked(&ClinicalTrials, &card);
            assert_eq!(doc.attributes["page_url"], card.url);
            assert_eq!(doc.source_root.as_deref(), Some(card.id.as_str()));
        }
    }
}
