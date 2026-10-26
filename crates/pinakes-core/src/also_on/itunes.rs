//! The `itunes` also-on source.
use crate::{
    also_on::{Context, Found, Link, Source, Subject},
    music::{self, Level, Service},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::music_sources::*;

/// Apple Music's album for an album's barcode keys, from iTunes.
pub struct ItunesSource;
#[async_trait]
impl Source for ItunesSource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn id(&self) -> &'static str {
        "itunes"
    }
    fn applies(&self, subject: &Subject) -> bool {
        music::parse(&subject.uri).is_some_and(|l| {
            join_level(&l) == Some(Level::Album) && l.service != Service::AppleMusic
        }) && subject.keys_in("upc").next().is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        for upc in subject.keys_in("upc").take(2) {
            if let Some(apple) = itunes_album(cx, upc).await? {
                found.links.push(apple.keyed(format!("upc:{upc}")));
            }
        }
        Ok(found)
    }
}

/// Apple Music's album for a barcode: iTunes `lookup?upc=` in the configured storefront, when
/// exactly one album answers (an unknown code can return unrelated albums). Its results carry no
/// UPC, so unlike Deezer's the answer can't be read back against the code asked: exactly one
/// album is the only check, and `upc` is no single-object namespace that would catch a wrong one.
async fn itunes_album(cx: &Context, barcode: &str) -> Result<Option<Link>> {
    let storefront = &cx.config.also_on.apple_storefront;
    let url =
        format!("https://itunes.apple.com/lookup?upc={barcode}&entity=album&country={storefront}");
    let fetched = cx.egress.discover(&url, &itunes_options(), 3600).await?;
    match fetched.status {
        200 => {}
        429 | 500..=599 => {
            return Err(crate::resolver::RetryLater {
                class: "itunes_busy",
                seconds: fetched.retry_after_secs.clamp(60, 3600),
            }
            .into())
        }
        _ => return Ok(None),
    }
    // iTunes answers `text/javascript` with leading newlines.
    let v: Value = serde_json::from_slice(fetched.body.trim_ascii())?;
    Ok(itunes_parse(&v))
}
/// How iTunes is asked: for JSON, which it serves as `text/javascript`.
fn itunes_options() -> crate::resolver::egress::FetchOptions {
    crate::resolver::egress::FetchOptions {
        accept: Some("application/json".into()),
        extra_types: &["text/javascript"],
        ..Default::default()
    }
}
/// The one album of an iTunes lookup, as an Apple Music link.
pub fn itunes_parse(v: &Value) -> Option<Link> {
    let albums: Vec<&Value> = v["results"]
        .as_array()?
        .iter()
        .filter(|r| r["wrapperType"] == "collection" && r["collectionType"] == "Album")
        .collect();
    let [album] = albums.as_slice() else {
        return None;
    };
    derived(album["collectionViewUrl"].as_str()?, Level::Album, "itunes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/hubs/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(std::fs::read(path).unwrap().trim_ascii()).unwrap()
    }

    /// iTunes' real answer (`text/javascript`) passes the egress checks; without admitting it,
    /// the lookup was refused as `policy` and took MusicBrainz's answer down with it.
    #[tokio::test]
    async fn itunes_javascript_json_is_admitted() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/lookup",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    "\n\n\n{\"resultCount\":0,\"results\":[]}",
                )
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url: url::Url = format!("http://{addr}/lookup").parse().unwrap();
        let client = reqwest::Client::new();
        let read = |options: crate::resolver::egress::FetchOptions| {
            let (url, request) = (url.clone(), client.get(url.clone()));
            async move {
                crate::resolver::egress::read_response_with(
                    url,
                    request.send().await.unwrap(),
                    &options,
                )
                .await
            }
        };
        let fetched = read(itunes_options()).await.unwrap();
        assert_eq!(fetched.status, 200);
        let v: Value = serde_json::from_slice(fetched.body.trim_ascii()).unwrap();
        assert!(itunes_parse(&v).is_none());
        assert!(
            read(Default::default()).await.is_err(),
            "refused without the admission"
        );
        server.abort();
    }

    #[test]
    fn itunes_counts_only_one_album() {
        let link = itunes_parse(&fixture("itunes-upc.json")).unwrap();
        assert_eq!(link.uri, "https://music.apple.com/album/1097861387");
        assert_eq!(link.via, "itunes");
        assert!(itunes_parse(&fixture("itunes-upc-many.json")).is_none());
    }
}
