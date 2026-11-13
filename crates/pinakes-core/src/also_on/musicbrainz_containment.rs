//! Exact recording→release containment, deliberately never a grouping key or Also on link.
use super::{Context, Fact, Found, Source, Subject};
use crate::hubs::{self, musicbrainz};
use anyhow::Result;
use async_trait::async_trait;

pub struct MusicBrainzContainment;
#[async_trait]
impl Source for MusicBrainzContainment {
    fn id(&self) -> &'static str {
        "musicbrainz_containment"
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.type_name.as_deref() == Some("MusicRecording")
            && subject.keys_in("mb:recording").count() == 1
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        if !self.applies(subject) {
            return Ok(Found::default());
        }
        let Some(recording) = subject.keys_in("mb:recording").next().and_then(hubs::mbid) else {
            return Ok(Found::default());
        };
        let releases = musicbrainz::recording_releases(
            &cx.egress,
            cx.config.also_on.musicbrainz_base(),
            &recording,
        )
        .await?;
        let ids: std::collections::BTreeSet<_> = releases
            .iter()
            .filter_map(|r| hubs::mbid(r["id"].as_str()?))
            .collect();
        if ids.len() > 8 {
            tracing::info!(
                recording,
                releases = ids.len(),
                cap = 8,
                "recording containment capped"
            );
        }
        Ok(Found {
            facts: ids
                .into_iter()
                .take(8)
                .map(|id| {
                    Fact::new(
                        "track_of_album",
                        format!("https://musicbrainz.org/release/{id}"),
                    )
                })
                .collect(),
            ..Default::default()
        })
    }
}
