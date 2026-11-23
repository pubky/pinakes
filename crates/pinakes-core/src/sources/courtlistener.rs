//! US court opinions from CourtListener (Free Law Project), by opinion cluster id, through its
//! search API with the operator's token (`[sources.courtlistener] token`, or
//! `PINAKES_COURTLISTENER_TOKEN`): the API answers no request without one (401), and the opinion
//! pages answer bots with a challenge. Without a token the source is off and opinion links get
//! ordinary web cards. The token goes only to `www.courtlistener.com`, never through a redirect,
//! and never appears in logs, errors or cards. A token CourtListener refuses (401, or a 403 whose
//! JSON `detail` names the token) turns the source off for an hour (`Refusals`), and
//! `pinakes_source_token_refusals_total{source="courtlistener"}` counts it. Any other 403 is a
//! failure of that link alone. Case law is in the public domain.
use super::{list, text, texts, Budget, Context, Declined, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct Courtlistener;

/// CourtListener's refusals of the configured token.
static REFUSALS: super::Refusals = super::Refusals::new("courtlistener");

/// The search request with `token`.
fn options(token: &str) -> FetchOptions {
    FetchOptions {
        accept: Some("application/json".into()),
        headers: vec![("authorization", format!("Token {token}"))],
        // Errors and cache keys never carry the header; a redirect is answered, not followed.
        sensitive: true,
        keep_redirect: true,
        // A 403's JSON says whether the token is what was refused.
        error_json: true,
        ..Default::default()
    }
}

pub const ATTRIBUTION: &str = "Source: CourtListener, Free Law Project";

fn cluster(value: &str) -> Option<u64> {
    super::kit::number(value, 12)
}

/// The cluster id of `/opinion/{id}[/{slug}/…]` or `/api/rest/v{3,4}/clusters/{id}/`.
pub fn of_uri(uri: &str) -> Option<u64> {
    let url = super::kit::web_url(uri)?;
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

/// What CourtListener answered for one opinion cluster.
#[derive(Debug)]
pub enum Answer {
    Card(Box<ResourceMeta>),
    Missing,
    /// The token was refused (401, or 403 naming it).
    Refused,
}

/// An opinion cluster's card from the search API with `token`.
pub async fn read(cx: &Context<'_>, doc: ResourceMeta, id: u64, token: &str) -> Result<Answer> {
    // Through the egress itself: a source's own fetch (`Context::fetch`) refuses the header.
    let f = cx
        .egress
        .fetch_with(
            &format!(
                "https://www.courtlistener.com/api/rest/v4/search/?type=o&q=cluster_id%3A{id}"
            ),
            &options(token),
        )
        .await?;
    // A 403 is a refusal of the token only when CourtListener's own answer says so ("Invalid
    // token.", "Authentication credentials were not provided."); another 403 (a forbidden
    // object, a firewall's page) is that link's failure.
    if f.status == 401 || (f.status == 403 && super::names_the_key(&f.body, "detail")) {
        return Ok(Answer::Refused);
    }
    super::retry(&f)?;
    Ok(match card(doc, id, f.status, &f.body)? {
        Some(card) => Answer::Card(Box::new(card)),
        None => Answer::Missing,
    })
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
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "courtlistener",
                uri: "https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/",
                card_type: "CreativeWork",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "www.courtlistener.com",
                path: "/api/rest/v4/search/",
                query: &[],
                // The mock run's token (`examples/e2e_stack.rs`).
                header: Some("authorization"),
                content_type: "application/json",
                file: "search_o_cluster_id_2812209_obergefell.json",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn takes_token(&self) -> bool {
        true
    }
    fn token_variable(&self) -> Option<&'static str> {
        Some("PINAKES_COURTLISTENER_TOKEN")
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
        // Five requests a minute, its anonymous limit when it had one: a token's own limit may
        // be higher, which the operator can set (`limits.host_rates`).
        &[("www.courtlistener.com", 5.0 / 60.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[Example {
            uri: "https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/",
            card_type: "CreativeWork",
        }]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let id = of_uri(uri).context("not an opinion")?;
        let Some(token) = super::token(self.name()) else {
            return Err(Declined.into());
        };
        resolve_with(cx, uri, id, &token, &REFUSALS).await
    }
}

/// The card of opinion cluster `id` at `uri` with `token`, `Declined` while `refusals` holds it.
async fn resolve_with(
    cx: &Context<'_>,
    uri: &str,
    id: u64,
    token: &str,
    refusals: &super::Refusals,
) -> Result<ResourceMeta> {
    refusals.check()?;
    match read(cx, cx.card(uri, &Courtlistener), id, token).await? {
        Answer::Card(doc) => Ok(*doc),
        Answer::Missing => Ok(cx.missing(uri, &Courtlistener)),
        Answer::Refused => {
            if refusals.refused() {
                tracing::warn!(
                    "CourtListener refused the configured token; its links get web cards for an hour"
                );
            }
            Err(Declined.into())
        }
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

    /// A transport that answers as CourtListener's search does: the result with the right token,
    /// and its recorded 401 without one or with another; it remembers each Authorization header.
    struct Api {
        seen: std::sync::Mutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl crate::resolver::egress::Transport for Api {
        async fn get(
            &self,
            url: &url::Url,
            addresses: &[std::net::SocketAddr],
        ) -> Result<crate::resolver::egress::Fetched> {
            self.get_with(url, addresses, &FetchOptions::default())
                .await
        }
        async fn get_with(
            &self,
            url: &url::Url,
            _: &[std::net::SocketAddr],
            options: &FetchOptions,
        ) -> Result<crate::resolver::egress::Fetched> {
            let authorization = options
                .headers
                .iter()
                .find(|(name, _)| *name == "authorization")
                .map(|(_, v)| v.clone());
            self.seen.lock().unwrap().push(authorization.clone());
            let (status, file) = match (authorization.as_deref(), url.query()) {
                (Some("Token good-token"), Some(q)) if q.ends_with("2812209") => {
                    (200, "search_o_cluster_id_2812209_obergefell.json")
                }
                (Some("Token good-token"), _) => {
                    (200, "search_o_cluster_id_999999999_missing.json")
                }
                (Some("Token forbidden-token"), _) => {
                    return Ok(crate::resolver::egress::Fetched::answer(
                        url.clone(),
                        403,
                        "application/json".into(),
                        br#"{"detail":"You do not have permission to perform this action."}"#
                            .to_vec(),
                    ))
                }
                (Some("Token invalid-token"), _) => {
                    return Ok(crate::resolver::egress::Fetched::answer(
                        url.clone(),
                        403,
                        "application/json".into(),
                        br#"{"detail":"Invalid token."}"#.to_vec(),
                    ))
                }
                _ => (401, "search_unauthenticated_401.json"),
            };
            Ok(crate::resolver::egress::Fetched::answer(
                url.clone(),
                status,
                "application/json".into(),
                read(file),
            ))
        }
    }

    /// The search answers only with a token (its 401 is recorded): the card comes with the
    /// token, sent as `Authorization: Token …`; a refused token is `Refused`, never a parse error.
    #[tokio::test]
    async fn the_search_is_read_with_the_token() {
        let api = std::sync::Arc::new(Api {
            seen: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(crate::sources::fixture::PublicDns),
            api.clone(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = || cx.card(&root(2812209), &Courtlistener);
        let Answer::Card(card) = super::read(&cx, doc(), 2812209, "good-token")
            .await
            .unwrap()
        else {
            panic!("no card");
        };
        assert_eq!(card.name.as_deref(), Some("Obergefell v. Hodges"));
        assert!(matches!(
            super::read(&cx, doc(), 999999999, "good-token")
                .await
                .unwrap(),
            Answer::Missing
        ));
        assert!(matches!(
            super::read(&cx, doc(), 2812209, "stale-token")
                .await
                .unwrap(),
            Answer::Refused
        ));
        assert_eq!(
            api.seen.lock().unwrap().as_slice(),
            [
                Some("Token good-token".to_owned()),
                Some("Token good-token".to_owned()),
                Some("Token stale-token".to_owned())
            ]
        );
    }

    /// A 403 is a refusal only when its `detail` names the token; a refusal switches the token
    /// off for an hour (links are `Declined` meanwhile), and it is used again after.
    #[tokio::test(start_paused = true)]
    async fn a_refused_token_is_off_for_an_hour_and_only_a_403_naming_it_counts() {
        let api = std::sync::Arc::new(Api {
            seen: Default::default(),
        });
        let egress = crate::resolver::egress::Egress::with_backends(
            Default::default(),
            std::sync::Arc::new(crate::sources::fixture::PublicDns),
            api.clone(),
        );
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let uri = root(2812209);
        let declined = |r: Result<ResourceMeta>| r.is_err_and(|e| e.is::<Declined>());
        // Another 403 is this link's failure, and no refusal.
        let refusals = crate::sources::Refusals::new("courtlistener");
        let error = resolve_with(&cx, &uri, 2812209, "forbidden-token", &refusals)
            .await
            .unwrap_err();
        assert!(!error.is::<Declined>());
        assert!(!refusals.latched());
        // A 403 naming the token is a refusal: off for an hour, nothing asked meanwhile.
        assert!(declined(
            resolve_with(&cx, &uri, 2812209, "invalid-token", &refusals).await
        ));
        assert!(refusals.latched());
        let asked = api.seen.lock().unwrap().len();
        tokio::time::advance(std::time::Duration::from_secs(3599)).await;
        assert!(declined(
            resolve_with(&cx, &uri, 2812209, "good-token", &refusals).await
        ));
        assert_eq!(api.seen.lock().unwrap().len(), asked);
        // After the hour the token is used again.
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let doc = resolve_with(&cx, &uri, 2812209, "good-token", &refusals)
            .await
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Obergefell v. Hodges"));
    }
}
