//! The `performers` also-on source.
use crate::{
    also_on::{Context, Found, Key, Source, Subject},
    hubs::{self, musicbrainz, MbKind},
    music_sources::{artist_link, ArtistLink},
};
use anyhow::Result;
use async_trait::async_trait;

/// The host a link's service is known by: lower case, without `www.`.
fn service_host(url: &str) -> Option<String> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_owned())
}

/// The performers an event page names in its own structured data: each one's name is a fact,
/// and its artist identity a key (`mb:artist:…`) when the page is on the service of one of its
/// links for the performer (a Songkick concert naming a Songkick artist page, Bandsintown's and a
/// music service's own event pages the same) and MusicBrainz relates that link to exactly one
/// artist. Any other identity is the page's choice, so a fact (`performer_mbid`): the artist
/// MusicBrainz relates another link to, or a MusicBrainz artist page the page names. A performer
/// without one keeps its first link as the fact `performer_link`. A name alone is never a key. Events group only
/// with events, so the key lists the artist's events (`key=mbid:…&type=MusicEvent`) and never
/// joins a concert to the artist's own pages.
pub struct PerformersSource;
#[async_trait]
impl Source for PerformersSource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        crate::music_sources::KEY_FORMS
    }
    fn id(&self) -> &'static str {
        "performers"
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.doc.as_ref().is_some_and(|d| {
            !d.event.performers.is_empty() && d.type_path.iter().any(|t| t == "Event")
        })
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        self.read(cx, subject).await
    }
}
impl PerformersSource {
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(doc) = &subject.doc else {
            return Ok(Found::default());
        };
        let base = cx.config.also_on.musicbrainz_base();
        let mut found = Found::default();
        let mut keyed = std::collections::BTreeSet::new();
        // A key only from MusicBrainz's own answer; the MBID is a fact too.
        let mut key = |found: &mut Found, id: &str| {
            if let Some(key) = hubs::key::mb(MbKind::Artist, id) {
                if keyed.insert(key.clone()) {
                    found.keys.push(Key {
                        key,
                        via: "musicbrainz".into(),
                    });
                }
            }
            found
                .facts
                .push(crate::also_on::Fact::new("performer_mbid", id));
        };
        // What the page says: each performer's name, the links MusicBrainz may relate to its
        // artist (at most two), and a MusicBrainz artist page it names. That page is the page's
        // own claim, so it is a fact at most, never a key: any page can name any artist's page.
        let mut performers: Vec<(Option<String>, Vec<String>)> = vec![];
        for performer in doc.event.performers.iter().take(16) {
            found
                .facts
                .push(crate::also_on::Fact::new("performer", &performer.name));
            let mut links = vec![];
            let mut claimed = None;
            for l in &performer.links {
                match artist_link(l) {
                    Some(ArtistLink::Mbid(id)) => {
                        claimed.get_or_insert(id);
                    }
                    Some(ArtistLink::Lookup(url)) if !links.contains(&url) => links.push(url),
                    _ => {}
                }
            }
            links.truncate(2);
            performers.push((claimed, links));
        }
        // A key only where the event page speaks for the link's own service: a Songkick concert
        // naming a Songkick artist page (Bandsintown and a music service's own event pages the
        // same). Any other page chose the identity MusicBrainz translates, so it is a fact. The
        // page is the host its fetch ended on, after redirects; a stored page that doesn't say
        // is no service's.
        let page = doc.event.performers_from().and_then(service_host);
        let owned = |url: &str| page.is_some() && service_host(url) == page;
        // Then MusicBrainz: every performer's links in one request, and only that one (a link
        // its answer leaves out is unknown this run). A failed request, a busy MusicBrainz's or
        // an answer over its cap included, fails the run of a page that owns a link: the
        // previous rows stay and it retries later (recording what was found would replace the
        // rows without those performers' keys until the next run, a month away).
        let all: Vec<String> = performers
            .iter()
            .flat_map(|(_, links)| links)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let answers = match all.as_slice() {
            [] => Ok(vec![]),
            [one] => musicbrainz::url(&cx.egress, base, one)
                .await
                .map(|t| vec![t]),
            _ => musicbrainz::urls(&cx.egress, base, &all).await,
        };
        let answers: std::collections::BTreeMap<&String, musicbrainz::UrlTargets> = match answers {
            Ok(answers) => all
                .iter()
                .zip(answers)
                .filter_map(|(link, t)| Some((link, t?)))
                .collect(),
            // A page that owns none of its performers' links can be keyed by no answer: what it
            // says is recorded without MusicBrainz (a key an earlier rule gave goes now), and
            // MusicBrainz is asked again after its retry time. A page that owns one keeps its
            // previous rows until MusicBrainz answers.
            Err(e)
                if !performers
                    .iter()
                    .any(|(_, links)| links.iter().any(|u| owned(u))) =>
            {
                let wait = crate::also_on::media::retry_wait(&e);
                found.retry_after = Some(std::time::Duration::from_secs(wait));
                Default::default()
            }
            Err(e) => return Err(e),
        };
        for (claimed, links) in performers {
            // The artist MusicBrainz relates a link to, when exactly one.
            let artist = |url: &String| match answers
                .get(url)?
                .targets
                .iter()
                .filter(|(k, _)| *k == MbKind::Artist)
                .map(|(_, id)| id)
                .collect::<Vec<_>>()[..]
            {
                [id] => Some(id.clone()),
                _ => None,
            };
            let keyed = links.iter().filter(|u| owned(u)).find_map(artist);
            // One fact beside the name (sixteen performers fit the facts cap): the artist
            // MusicBrainz names, else the one the page names, else the link itself.
            match (keyed, links.iter().find_map(artist).or(claimed)) {
                (Some(id), _) => key(&mut found, &id),
                (None, Some(id)) => found
                    .facts
                    .push(crate::also_on::Fact::new("performer_mbid", id)),
                (None, None) => {
                    if let Some(url) = links.first() {
                        found
                            .facts
                            .push(crate::also_on::Fact::new("performer_link", url));
                    }
                }
            }
        }
        Ok(found)
    }
}
