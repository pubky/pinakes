//! Greenhouse job boards' postings, read from the public Job Board API by board and job id. The
//! board pages carry no structured data. No key.
use super::{text, web_url, Context, Example, Source};
use crate::{
    kinds::jobs::{self, Posting, Salary},
    model::ResourceMeta,
    resolver::egress::FetchOptions,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Greenhouse;

/// A board token: lower-case letters, digits, `-` and `_`.
fn board(value: &str) -> Option<&str> {
    ((1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'))
    .then_some(value)
}
fn job(value: &str) -> Option<u64> {
    ((1..=16).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
        .filter(|n| *n > 0)
}

/// The board and job of `job-boards.greenhouse.io/{board}/jobs/{id}`, its `boards.` spelling, its
/// application form (`/embed/job_app?for={board}&token={id}`) or the API's address.
pub fn of_uri(uri: &str) -> Option<(String, u64)> {
    let url = web_url(uri)?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let query = |k: &str| {
        url.query_pairs()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.into_owned())
    };
    let (b, id) = match (url.host_str()?, segments.as_slice()) {
        ("job-boards.greenhouse.io" | "boards.greenhouse.io", ["embed", "job_app"]) => {
            (query("for")?, query("token")?)
        }
        ("job-boards.greenhouse.io" | "boards.greenhouse.io", [b, "jobs", id]) => {
            ((*b).to_owned(), (*id).to_owned())
        }
        ("boards-api.greenhouse.io", ["v1", "boards", b, "jobs", id]) => {
            ((*b).to_owned(), (*id).to_owned())
        }
        _ => return None,
    };
    Some((board(&b)?.to_owned(), job(&id)?))
}

pub fn root(board: &str, id: u64) -> String {
    format!("https://job-boards.greenhouse.io/{board}/jobs/{id}")
}

/// The card of a posting, or `None` when the board has no such posting (closed or never posted).
pub fn card(
    mut doc: ResourceMeta,
    id: u64,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    match status {
        200 => {}
        404 => return Ok(None),
        s => anyhow::bail!("Greenhouse answered {s}"),
    }
    let v: Value = serde_json::from_slice(body)?;
    anyhow::ensure!(
        v["id"].as_u64() == Some(id),
        "Greenhouse answered for another posting"
    );
    // "London, UK; Remote-Friendly, United States": the first place that is not remote is the
    // workplace; a remote one names its regions.
    let places: Vec<String> = text(&v["location"]["name"])
        .map(|l| {
            l.split(';')
                .take(jobs::LISTED)
                .map(|p| p.trim().to_owned())
                .collect()
        })
        .unwrap_or_default();
    let remote: Vec<Vec<String>> = places
        .iter()
        .filter_map(|p| jobs::remote_place(p))
        .collect();
    let workplace = places
        .iter()
        .find(|p| !p.is_empty() && jobs::remote_place(p).is_none())
        .and_then(|p| jobs::workplace(&Value::String(p.clone()), &Value::Null));
    let pay = &v["pay_input_ranges"][0];
    let cents = |k: &str| pay[k].as_f64().map(|c| c / 100.0);
    Posting {
        title: text(&v["title"]),
        // The content is HTML, escaped once more.
        description: text(&v["content"])
            .and_then(|c| jobs::excerpt(&crate::resolver::web::decoded(&c))),
        organization: text(&v["company_name"]),
        remote: !remote.is_empty(),
        regions: remote.into_iter().flatten().collect(),
        salary: Salary {
            min: cents("min_cents"),
            max: cents("max_cents"),
            currency: text(&pay["currency_type"]),
            unit: None,
        },
        posted: text(&v["first_published"]),
        valid_through: text(&v["application_deadline"]),
        apply_url: text(&v["absolute_url"]).filter(|u| web_url(u).is_some()),
        workplace,
        ..Default::default()
    }
    .apply(&mut doc);
    anyhow::ensure!(doc.name.is_some(), "no title");
    Ok(Some(doc))
}

#[async_trait]
impl Source for Greenhouse {
    fn name(&self) -> &'static str {
        "greenhouse"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "greenhouse",
                uri: "https://job-boards.greenhouse.io/gitlab/jobs/8638232002",
                card_type: "JobPosting",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "boards-api.greenhouse.io",
                path: "/v1/boards/gitlab/jobs/8638232002",
                query: &[("pay_transparency", "true")],
                header: None,
                content_type: "application/json",
                file: "greenhouse-gitlab-8638232002.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let (board, id) = of_uri(uri)?;
        let root = root(&board, id);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        jobs::FACTS
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://job-boards.greenhouse.io/gitlab/jobs/8638232002",
                card_type: "JobPosting",
            },
            Example {
                uri: "https://boards.greenhouse.io/figma/jobs/5783812004",
                card_type: "JobPosting",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let (board, id) = of_uri(uri).context("not a Greenhouse posting")?;
        let f = cx
            .fetch(
                &format!(
                    "https://boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}?pay_transparency=true"
                ),
                &FetchOptions {
                    accept: Some("application/json".into()),
                    ..Default::default()
                },
            )
            .await?;
        let doc = cx.card(uri, self);
        Ok(
            crate::resolver::html::blocking(move || card(doc, id, f.status, &f.body))
                .await??
                .unwrap_or_else(|| cx.missing(uri, self)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("greenhouse", file)
    }
    fn base(board: &str, id: u64) -> ResourceMeta {
        ResourceMeta::new(&root(board, id), "greenhouse", 3600)
    }

    #[test]
    fn addresses() {
        for uri in [
            "https://boards.greenhouse.io/gitlab/jobs/8638232002",
            "https://job-boards.greenhouse.io/gitlab/jobs/8638232002?gh_src=x",
            "https://boards.greenhouse.io/embed/job_app?for=gitlab&token=8638232002",
            "https://boards-api.greenhouse.io/v1/boards/gitlab/jobs/8638232002",
        ] {
            assert_eq!(
                Greenhouse.alias_root(uri).as_deref(),
                Some("https://job-boards.greenhouse.io/gitlab/jobs/8638232002"),
                "{uri}"
            );
        }
        assert_eq!(
            Greenhouse.alias_root("https://job-boards.greenhouse.io/gitlab/jobs/8638232002"),
            None
        );
        for uri in [
            "https://job-boards.greenhouse.io/gitlab",
            "https://job-boards.greenhouse.io/GitLab/jobs/1",
            "https://job-boards.greenhouse.io/gitlab/jobs/x",
            "https://www.greenhouse.io/gitlab/jobs/1",
        ] {
            assert!(!Greenhouse.reads(uri), "{uri}");
        }
    }

    #[test]
    fn postings() {
        let doc = card(
            base("gitlab", 8638232002),
            8638232002,
            200,
            &read("greenhouse-gitlab-8638232002.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "JobPosting");
        assert_eq!(doc.name.as_deref(), Some("AI Transformation Owner, CRO"));
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("GitLab is the intelligent orchestration platform"));
        assert_eq!(doc.start.unwrap().to_rfc3339(), "2026-07-22T17:38:40+00:00");
        assert_eq!(doc.end, None);
        let a = &doc.attributes;
        assert_eq!(a["hiring_organization"], "GitLab");
        assert_eq!(a["remote"], "true");
        assert_eq!(a["remote_regions"], "United States");
        assert_eq!((&*a["salary_min"], &*a["salary_max"]), ("139200", "235200"));
        assert_eq!(a["salary_currency"], "USD");
        assert!(!a.contains_key("salary_unit"));
        assert_eq!(
            a["apply_url"],
            "https://job-boards.greenhouse.io/gitlab/jobs/8638232002"
        );
        assert!(doc.event.venue.is_none());

        let doc = card(
            base("figma", 5783812004),
            5783812004,
            200,
            &read("greenhouse-figma-5783812004.json"),
        )
        .unwrap()
        .unwrap();
        assert!(!doc.attributes.contains_key("remote"));
        assert!(!doc.attributes.contains_key("salary_min"));
        assert_eq!(doc.event.venue.as_ref().unwrap().text, "Berlin, Germany");

        let doc = card(
            base("anthropic", 5183044008),
            5183044008,
            200,
            &read("greenhouse-anthropic-5183044008.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.event.venue.as_ref().unwrap().text, "London, UK");
        assert_eq!(doc.attributes["remote"], "true");
        assert_eq!(doc.attributes["remote_regions"], "United States");

        assert!(
            card(base("gitlab", 1), 1, 404, &read("greenhouse-missing.json"))
                .unwrap()
                .is_none()
        );
        assert!(card(
            base("gitlab", 1),
            1,
            200,
            &read("greenhouse-gitlab-8638232002.json")
        )
        .is_err());
        assert!(doc.clone().bounded(crate::sources::attributes()).is_ok());

        // A pay range with a maximum only is no salary, as on the JSON-LD route.
        let mut v: Value =
            serde_json::from_slice(&read("greenhouse-gitlab-8638232002.json")).unwrap();
        v["pay_input_ranges"][0]
            .as_object_mut()
            .unwrap()
            .remove("min_cents");
        let doc = card(
            base("gitlab", 8638232002),
            8638232002,
            200,
            &serde_json::to_vec(&v).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(!doc.attributes.keys().any(|k| k.starts_with("salary_")));
    }

    #[test]
    fn a_location_is_read_for_its_first_32_places() {
        let with = |name: String| {
            let mut v: Value =
                serde_json::from_slice(&read("greenhouse-figma-5783812004.json")).unwrap();
            v["location"]["name"] = name.into();
            card(
                base("figma", 5783812004),
                5783812004,
                200,
                &serde_json::to_vec(&v).unwrap(),
            )
            .unwrap()
            .unwrap()
        };
        let remote = |n: usize| (0..n).map(|i| format!("Remote, R{i}")).collect::<Vec<_>>();
        // 5,000 remote places: the first 32 are read, each for its region.
        let doc = with(remote(5000).join("; "));
        let regions: Vec<String> = (0..jobs::LISTED).map(|i| format!("R{i}")).collect();
        assert_eq!(doc.attributes["remote_regions"], regions.join("; "));
        // The workplace as the 32nd place is read; as the 33rd it is not.
        let doc = with(
            [remote(31), vec!["Berlin, Germany".into()]]
                .concat()
                .join("; "),
        );
        assert_eq!(doc.event.venue.as_ref().unwrap().text, "Berlin, Germany");
        let doc = with(
            [remote(32), vec!["Berlin, Germany".into()]]
                .concat()
                .join("; "),
        );
        assert!(doc.event.venue.is_none());
        assert_eq!(doc.attributes["remote_regions"], regions.join("; "));
    }
}
