//! The `spotify` also-on source through Egress, against the Spotify answers of
//! `common::spotify_api`.
use pinakes_core::also_on;
mod common;
use common::{
    save_card as save,
    spotify_api::{
        config, run, spotify_requests, Answers, ALBUM, KEYS, SHARED_NEITHER, SHARED_PICKED, TRACK,
    },
    strings as rows,
};

/// A Spotify album's barcode joins Apple Music and Deezer; a track's ISRC, which MusicBrainz
/// gives to one recording, is its key.
#[sqlx::test(migrations = "../../migrations")]
async fn a_spotify_barcode_joins_apple_music_and_deezer(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !["deezer", "itunes"]
        .iter()
        .all(|id| registered.contains(id))
    {
        return;
    }
    let album = save(
        &pool,
        &format!("https://open.spotify.com/album/{ALBUM}"),
        "MusicAlbum",
    )
    .await;
    let track = save(
        &pool,
        &format!("https://open.spotify.com/track/{TRACK}"),
        "MusicRecording",
    )
    .await;
    let apple = save(
        &pool,
        "https://music.apple.com/album/1680053261",
        "MusicAlbum",
    )
    .await;
    let seen = run(
        &pool,
        config(Some(("join-id", "join-secret"))),
        Answers::default(),
    )
    .await;
    // One token, then one request per album or track.
    let mut asked: Vec<String> = spotify_requests(&seen).into_iter().cloned().collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            "accounts.spotify.com/api/token?".to_owned(),
            format!("api.spotify.com/v1/albums/{ALBUM}?"),
            format!("api.spotify.com/v1/tracks/{TRACK}?"),
        ],
        "{seen:?}"
    );
    assert_eq!(
        rows(&pool, KEYS, &album).await,
        ["upc:4018939306052 spotify"]
    );
    assert!(
        rows(&pool, KEYS, &track)
            .await
            .contains(&"isrc:GBUM71029604 spotify".to_owned()),
        "{seen:?}"
    );
    assert_eq!(
        rows(
            &pool,
            "SELECT name||' '||value FROM resource_fact WHERE resource_id=$1 AND source='spotify'",
            &track
        )
        .await,
        ["isrc GBUM71029604"]
    );
    common::drain_groups(&pool, &Default::default()).await;
    let group = "SELECT coalesce(group_id,'-') FROM group_unit WHERE resource_id=$1";
    let joined = rows(&pool, group, &album).await;
    assert_ne!(joined, ["-"]);
    assert_eq!(rows(&pool, group, &apple).await, joined);
}

#[sqlx::test(migrations = "../../migrations")]
async fn without_credentials_nothing_asks_spotify(pool: sqlx::PgPool) {
    let album = save(
        &pool,
        &format!("https://open.spotify.com/album/{ALBUM}"),
        "MusicAlbum",
    )
    .await;
    let seen = run(&pool, config(None), Answers::default()).await;
    assert!(spotify_requests(&seen).is_empty(), "{seen:?}");
    assert!(rows(&pool, KEYS, &album).await.is_empty());
}

/// Credentials the token endpoint refuses, and an app the API refuses (403, as for an owner
/// without Premium): a refusal of the credentials, which fails no run and asks Spotify nothing
/// while it is held; what Spotify gave before stays.
#[sqlx::test(migrations = "../../migrations")]
async fn refused_credentials_keep_what_spotify_gave(pool: sqlx::PgPool) {
    let album = save(
        &pool,
        &format!("https://open.spotify.com/album/{ALBUM}"),
        "MusicAlbum",
    )
    .await;
    run(
        &pool,
        config(Some(("first-id", "first-secret"))),
        Answers::default(),
    )
    .await;
    assert_eq!(
        rows(&pool, KEYS, &album).await,
        ["upc:4018939306052 spotify"]
    );
    let queued = || async {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT last_error FROM also_on_dirty WHERE resource_id=$1",
        )
        .bind(&album)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    for (pair, answers, asked) in [
        (
            ("refused-id", "refused-secret"),
            Answers {
                refuse: true,
                ..Default::default()
            },
            vec!["accounts.spotify.com/api/token?".to_owned()],
        ),
        (
            ("premium-id", "premium-secret"),
            Answers {
                api_status: Some(403),
                ..Default::default()
            },
            vec![
                "accounts.spotify.com/api/token?".to_owned(),
                format!("api.spotify.com/v1/albums/{ALBUM}?"),
            ],
        ),
    ] {
        let before = common::token_refusals("spotify");
        let seen = run(&pool, config(Some(pair)), answers.clone()).await;
        assert_eq!(
            spotify_requests(&seen),
            asked.iter().collect::<Vec<_>>(),
            "{seen:?}"
        );
        // Counted where an alert reads it (other tests here may count too: at least one).
        assert!(common::token_refusals("spotify") > before);
        assert_eq!(
            rows(&pool, KEYS, &album).await,
            ["upc:4018939306052 spotify"]
        );
        assert_eq!(queued().await, None);
        // The refusal is kept five minutes: a run within them asks Spotify nothing.
        let seen = run(&pool, config(Some(pair)), answers).await;
        assert!(spotify_requests(&seen).is_empty(), "{seen:?}");
    }
}

/// An ISRC MusicBrainz gives to several recordings is never Spotify's key: the recording the
/// track's length picks is (its MBID), else nothing; the ISRC is a fact either way.
#[sqlx::test(migrations = "../../migrations")]
async fn a_shared_isrc_is_never_a_spotify_key(pool: sqlx::PgPool) {
    let picked = save(
        &pool,
        &format!("https://open.spotify.com/track/{SHARED_PICKED}"),
        "MusicRecording",
    )
    .await;
    let neither = save(
        &pool,
        &format!("https://open.spotify.com/track/{SHARED_NEITHER}"),
        "MusicRecording",
    )
    .await;
    run(
        &pool,
        config(Some(("shared-id", "shared-secret"))),
        Answers::default(),
    )
    .await;
    let spotify_keys =
        "SELECT key FROM resource_key WHERE resource_id=$1 AND source='spotify' ORDER BY key";
    assert_eq!(
        rows(&pool, spotify_keys, &picked).await,
        ["mb:recording:8f3471b5-7e6a-48da-86a9-c1c07a0f47ae"]
    );
    assert!(rows(&pool, spotify_keys, &neither).await.is_empty());
    let any_isrc_key = "SELECT key FROM resource_key WHERE resource_id=$1 AND key LIKE 'isrc:%'";
    let facts =
        "SELECT name||' '||value FROM resource_fact WHERE resource_id=$1 AND source='spotify'";
    for id in [&picked, &neither] {
        assert!(rows(&pool, any_isrc_key, id).await.is_empty());
        assert_eq!(rows(&pool, facts, id).await, ["isrc GBARL9300135"]);
    }
}
