//! What the sources that ask a service with an app's client credentials (`tidal_isrc`,
//! `spotify_isrc`) log and store when the service refuses the app, rejects its token or is down:
//! never the secret, the pair's `Basic` form or the token. In a test binary of its own: a log
//! callsite's interest is cached for the whole process, so beside tests on other threads, which
//! have no subscriber, this test's lines could be dropped (as `osm_log.rs` says).
use base64::Engine;
use pinakes_core::{
    also_on::{self, SourceOptions},
    config::Config,
    resolver::egress::{Egress, FetchOptions, Fetched, Transport},
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
mod common;
use common::media::PublicDns;

/// The token both services issue.
const TOKEN: &str = "TOKEN-CANARY-0123456789";

/// What the two services answer: their token endpoints' status (200 issues `TOKEN`) and their
/// APIs'.
#[derive(Clone, Copy, Debug)]
struct Answers {
    token: u16,
    api: u16,
}
/// TIDAL's and Spotify's token endpoints and APIs; records each request's `Authorization` header
/// and every other host asked.
struct Services {
    answers: Answers,
    sent: Mutex<Vec<String>>,
    others: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl Transport for Services {
    async fn get(&self, url: &url::Url, addresses: &[SocketAddr]) -> anyhow::Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &url::Url,
        _: &[SocketAddr],
        options: &FetchOptions,
    ) -> anyhow::Result<Fetched> {
        let answer = |status: u16, body: String| Fetched {
            retry_after_secs: 120,
            ..common::fetched(
                url.clone(),
                status,
                "application/json".into(),
                body.into_bytes(),
            )
        };
        let authorization = options
            .headers
            .iter()
            .find(|(k, _)| *k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let host = url.host_str().unwrap_or("");
        Ok(match host {
            "accounts.spotify.com" | "auth.tidal.com" => {
                assert!(options.sensitive);
                self.sent.lock().unwrap().push(authorization);
                match self.answers.token {
                    200 => answer(
                        200,
                        format!(
                            r#"{{"access_token":"{TOKEN}","token_type":"Bearer","expires_in":3600}}"#
                        ),
                    ),
                    status => answer(status, r#"{"error":"invalid_client"}"#.into()),
                }
            }
            "api.spotify.com" | "openapi.tidal.com" => {
                assert!(options.sensitive);
                self.sent.lock().unwrap().push(authorization);
                answer(self.answers.api, r#"{"errors":[{"code":"TOLD"}]}"#.into())
            }
            _ => {
                self.others.lock().unwrap().push(host.to_owned());
                answer(404, "{}".into())
            }
        })
    }
}

/// A token endpoint that refuses the pair, an API that refuses the app (403) or rejects a fresh
/// token (401), and an outage of either (503): the pair went out on the wire, the refusal is
/// logged by the service's name, and no log line of any level and no row of any table holds the
/// secret, the pair's `Basic` form or the token.
#[sqlx::test(migrations = "../../migrations")]
async fn refusals_and_outages_log_and_store_no_credential(pool: sqlx::PgPool) {
    // (source, its section of the configuration, the name its lines carry)
    let services: Vec<(&str, &str, &str)> = [
        ("tidal_isrc", "tidal", "TIDAL"),
        ("spotify_isrc", "spotify", "Spotify"),
    ]
    .into_iter()
    .filter(|(source, ..)| pinakes_core::test_support::needs_also_on(&[source]))
    .collect();
    if services.is_empty() {
        return;
    }
    let sources: Vec<_> = also_on::registry()
        .into_iter()
        .filter(|s| services.iter().any(|(source, ..)| *source == s.id()))
        .collect();
    // An album on another service with a barcode key: both sources look it up by the barcode.
    let album = common::save_card(
        &pool,
        "https://www.deezer.com/album/320429237",
        "MusicAlbum",
    )
    .await;
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,'upc:4018939306052','test','test',now()+interval '1 day')")
        .bind(&album)
        .execute(&pool)
        .await
        .unwrap();
    let logs = common::Lines::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    // This thread's default: the test's runtime runs the lookups on it.
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut read = 0;
    for (n, (answers, refused)) in [
        (
            Answers {
                token: 400,
                api: 200,
            },
            true,
        ),
        (
            Answers {
                token: 200,
                api: 403,
            },
            true,
        ),
        (
            Answers {
                token: 200,
                api: 401,
            },
            true,
        ),
        (
            Answers {
                token: 200,
                api: 503,
            },
            false,
        ),
        (
            Answers {
                token: 503,
                api: 200,
            },
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        // A pair of its own: tokens, refusals and waits are kept per pair.
        let (id, secret) = (format!("client-id-{n}"), format!("SECRET-CANARY-{n}"));
        let mut config = Config::default();
        for (source, section, _) in &services {
            config.sections.set(
                section,
                serde_json::json!({"client_id": id, "client_secret": secret}),
            );
            config.also_on.sources.insert(
                (*source).into(),
                SourceOptions {
                    enabled: Some(true),
                    ..Default::default()
                },
            );
        }
        config.validate().unwrap();
        let transport = Arc::new(Services {
            answers,
            sent: Mutex::default(),
            others: Mutex::default(),
        });
        let cx = also_on::Context {
            egress: Egress::with_backends(
                common::unpaced_egress_config(),
                Arc::new(PublicDns),
                transport.clone(),
            ),
            config: Arc::new(config),
        };
        sqlx::query("INSERT INTO also_on_dirty(resource_id) VALUES($1) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
            .bind(&album)
            .execute(&pool)
            .await
            .unwrap();
        also_on::drain(&pool, &cx, &sources, 100).await.unwrap();
        // The credential was used: the pair at each token endpoint, the token at each API that
        // was reached.
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
        let sent = transport.sent.lock().unwrap().clone();
        assert_eq!(
            sent.iter()
                .filter(|a| **a == format!("Basic {basic}"))
                .count(),
            services.len(),
            "{answers:?}: {sent:?}"
        );
        assert_eq!(
            sent.iter()
                .filter(|a| **a == format!("Bearer {TOKEN}"))
                .count(),
            if answers.token == 200 {
                services.len()
            } else {
                0
            },
            "{answers:?}: {sent:?}"
        );
        assert!(
            transport.others.lock().unwrap().is_empty(),
            "{answers:?}: {:?}",
            transport.others.lock().unwrap()
        );
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        // A refusal of the credentials is logged, by the service's name.
        if refused {
            for (.., label) in &services {
                assert!(
                    logged[read..].contains(&format!("{label} refused the client credentials")),
                    "{answers:?}: {}",
                    &logged[read..]
                );
            }
        }
        read = logged.len();
        // Every row of every table, as text.
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT quote_ident(table_name) FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for credential in [secret.as_str(), basic.as_str(), TOKEN] {
            assert!(!logged.contains(credential), "{answers:?}: {logged}");
            for table in &tables {
                let found: bool = sqlx::query_scalar(&format!(
                    "SELECT EXISTS(SELECT 1 FROM {table} t WHERE strpos(t::text,$1)>0)"
                ))
                .bind(credential)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert!(!found, "{answers:?}: {table}");
            }
        }
    }
}
