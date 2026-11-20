//! The `itunes` also-on source.
use crate::{
    also_on::{itunes_api, Context, Found, Link, Source, Subject},
    music::{self, Level, Service},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::music_sources::*;

/// Apple Music's album for an album's barcode keys, and an Apple Music track's preview, from
/// iTunes.
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
        apple_track(&subject.uri).is_some()
            || music::parse(&subject.uri).is_some_and(|l| {
                join_level(&l) == Some(Level::Album) && l.service != Service::AppleMusic
            }) && subject.keys_in("upc").next().is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let mut found = Found::default();
        if let Some(track) = apple_track(&subject.uri) {
            if let Some(preview) = itunes_preview(cx, &track).await? {
                crate::also_on::media::hear(&mut found, subject, preview);
            }
            return Ok(found);
        }
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
    Ok(itunes_api::answer(cx, &url)
        .await?
        .and_then(|v| itunes_parse(&v)))
}
/// The track an Apple Music track link names.
fn apple_track(uri: &str) -> Option<String> {
    music::parse(uri)
        .filter(|l| l.service == Service::AppleMusic && l.level == Level::Track)
        .map(|l| l.id)
}
/// iTunes' 30-second preview of an Apple Music track (`lookup?id=`, in the configured
/// storefront), when it is a public stream.
async fn itunes_preview(cx: &Context, track: &str) -> Result<Option<crate::model::Stream>> {
    Ok(itunes_api::lookup(cx, track)
        .await?
        .and_then(|v| preview_parse(&v, track)))
}
/// The preview of the one song an iTunes lookup answers for `track` (an AAC `.m4a`, or an
/// `.mp3`): never another track's, nor a music video's.
pub fn preview_parse(v: &Value, track: &str) -> Option<crate::model::Stream> {
    let songs: Vec<&Value> = v["results"]
        .as_array()?
        .iter()
        .filter(|r| {
            r["wrapperType"] == "track"
                && r["kind"] == "song"
                && r["trackId"].as_u64().map(|id| id.to_string()).as_deref() == Some(track)
        })
        .collect();
    let [song] = songs.as_slice() else {
        return None;
    };
    let url = song["previewUrl"].as_str()?;
    let path = url::Url::parse(url).ok()?.path().to_ascii_lowercase();
    let media_type = if path.ends_with(".m4a") {
        "audio/mp4"
    } else if path.ends_with(".mp3") {
        "audio/mpeg"
    } else {
        return None;
    };
    crate::model::Stream::public(url, media_type, None, true)
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

    /// iTunes' real answer (`text/javascript`) passes the egress checks: the lookup admits that
    /// content type, which the egress otherwise refuses as `policy`.
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
        let fetched = read(itunes_api::options()).await.unwrap();
        assert_eq!(fetched.status, 200);
        let v: Value = serde_json::from_slice(fetched.body.trim_ascii()).unwrap();
        assert!(itunes_parse(&v).is_none());
        assert!(
            read(Default::default()).await.is_err(),
            "refused without the admission"
        );
        server.abort();
    }

    /// An Apple Music track's preview: the one song iTunes answers for it (Radiohead's
    /// "Paranoid Android", recorded), never another track's, a music video's or an odd format.
    #[test]
    fn a_track_s_preview_is_its_own_song_s() {
        let v = fixture("itunes-song-1097861770.json");
        let preview = preview_parse(&v, "1097861770").unwrap();
        assert_eq!(
            preview.url,
            "https://audio-ssl.itunes.apple.com/itunes-assets/AudioPreview211/v4/67/6c/04/676c04b5-624a-6101-2d63-dc9c971e2a4e/mzaf_680078368934998544.plus.aac.p.m4a"
        );
        assert_eq!(preview.media_type, "audio/mp4");
        assert_eq!(preview.duration_s, None);
        assert!(preview.preview);
        assert_eq!(preview_parse(&v, "1097861769"), None, "another track");
        let mut video = v.clone();
        video["results"][0]["kind"] = "music-video".into();
        assert_eq!(preview_parse(&video, "1097861770"), None);
        let mut odd = v.clone();
        odd["results"][0]["previewUrl"] = "https://audio-ssl.itunes.apple.com/a/b.m4v".into();
        assert_eq!(preview_parse(&odd, "1097861770"), None);
        assert_eq!(
            apple_track("https://music.apple.com/us/album/ok-computer/1097861387?i=1097861770")
                .as_deref(),
            Some("1097861770")
        );
        assert_eq!(
            apple_track("https://music.apple.com/us/album/ok-computer/1097861387"),
            None
        );
    }

    #[test]
    fn itunes_counts_only_one_album() {
        let link = itunes_parse(&fixture("itunes-upc.json")).unwrap();
        assert_eq!(link.uri, "https://music.apple.com/album/1097861387");
        assert_eq!(link.via, "itunes");
        assert!(itunes_parse(&fixture("itunes-upc-many.json")).is_none());
    }
}
