//! The `spotify_isrc` also-on source through Egress, against the Spotify answers of
//! `common::spotify_api`: another service's track found on Spotify by its ISRC, an album by its
//! barcode, each read back; refusals and rate limits keep what Spotify gave before.
use pinakes_core::{
    also_on::{self, SourceOptions},
    model::ResourceMeta,
    store,
};
mod common;
use common::{
    save_card as save,
    spotify_api::{
        config, run, Answers, ALBUM, MISSING_UPC_ALBUM, OTHER_ISRC_TRACK, OTHER_UPC_ALBUM,
        PAGED_UPC_ALBUM, TRACK,
    },
    strings as rows,
};

/// A track on another service with an ISRC key is looked up on Spotify by it (`spotify_isrc`):
/// the Spotify tracks carrying exactly that ISRC become links keyed by the ISRC, a track the
/// search lists with another ISRC is left out, Spotify's own tracks are not asked, a track with
/// no ISRC key asks nothing, and the switch turns it off.
#[sqlx::test(migrations = "../../migrations")]
async fn a_track_with_an_isrc_key_gets_its_spotify_link(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"spotify_isrc") {
        return;
    }
    // TIDAL's own page states its ISRC (a key: TIDAL vouches for it) and its length.
    let tidal = "https://tidal.com/track/534050211";
    let tidal_id = store::ensure_resource(&pool, tidal).await.unwrap();
    let mut doc = ResourceMeta::new(tidal, "jsonld", 86400);
    doc.set_type("MusicRecording");
    doc.attributes.insert("isrc".into(), "GBUM71029604".into());
    doc.attributes.insert("duration".into(), "PT5M55S".into());
    store::save_document(&pool, &tidal_id, 0, &doc)
        .await
        .unwrap();
    // A TIDAL page without an ISRC, and Spotify's own track.
    let plain = save(&pool, "https://tidal.com/track/551040646", "MusicRecording").await;
    let own = save(
        &pool,
        &format!("https://open.spotify.com/track/{TRACK}"),
        "MusicRecording",
    )
    .await;
    let seen = run(
        &pool,
        config(Some(("isrc-id", "isrc-secret"))),
        Answers::default(),
    )
    .await;
    let searches: Vec<&String> = seen
        .iter()
        .filter(|s| s.starts_with("api.spotify.com/v1/search?"))
        .collect();
    assert_eq!(
        searches,
        ["api.spotify.com/v1/search?q=isrc%3AGBUM71029604&type=track&limit=10"],
        "{seen:?}"
    );
    let links = rows(
        &pool,
        "SELECT uri||' '||coalesce(key,'-')||' '||source||' '||via FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc' ORDER BY uri",
        &tidal_id,
    )
    .await;
    assert_eq!(
        links,
        [format!(
            "https://open.spotify.com/track/{TRACK} isrc:GBUM71029604 spotify_isrc spotify_isrc"
        )]
    );
    assert!(!links.iter().any(|l| l.contains(OTHER_ISRC_TRACK)));
    for id in [&plain, &own] {
        assert!(rows(
            &pool,
            "SELECT uri FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc'",
            id
        )
        .await
        .is_empty());
    }
    // Switched off: nothing asks the search, and the rows it found stop showing.
    let mut off = config(Some(("isrc-id", "isrc-secret")));
    off.also_on.sources.insert(
        "spotify_isrc".into(),
        SourceOptions {
            enabled: Some(false),
            ..Default::default()
        },
    );
    off.validate().unwrap();
    sqlx::query("DELETE FROM derived_link WHERE source='spotify_isrc'")
        .execute(&pool)
        .await
        .unwrap();
    let seen = run(&pool, off, Answers::default()).await;
    assert!(!seen.iter().any(|s| s.contains("/v1/search?")), "{seen:?}");
}

/// `spotify_isrc` is on by default: with the Spotify pair set and no switch, it asks the search.
/// A refusal (403) and a rate limit (429) keep the links an earlier run found; switched off, those
/// links stop showing on the card while their rows stay.
#[sqlx::test(migrations = "../../migrations")]
async fn spotify_isrc_is_on_by_default_and_keeps_its_rows_through_refusals(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"spotify_isrc") {
        return;
    }
    let tidal = "https://tidal.com/track/534050211";
    let tidal_id = store::ensure_resource(&pool, tidal).await.unwrap();
    let mut doc = ResourceMeta::new(tidal, "jsonld", 86400);
    doc.set_type("MusicRecording");
    doc.attributes.insert("isrc".into(), "GBUM71029604".into());
    doc.attributes.insert("duration".into(), "PT5M55S".into());
    store::save_document(&pool, &tidal_id, 0, &doc)
        .await
        .unwrap();
    // On by default: the pair alone turns it on.
    let mut by_default = config(Some(("isrc-default-id", "isrc-default-secret")));
    by_default.also_on.sources.remove("spotify_isrc");
    by_default.validate().unwrap();
    let seen = run(&pool, by_default, Answers::default()).await;
    assert!(seen.iter().any(|s| s.contains("/v1/search?")), "{seen:?}");
    let links = |pool: sqlx::PgPool, id: String| async move {
        sqlx::query_scalar::<_, String>(
            "SELECT uri FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc' ORDER BY uri",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    // The test config (switched on explicitly) finds the same.
    let on = config(Some(("isrc-default-id", "isrc-default-secret")));
    run(&pool, on.clone(), Answers::default()).await;
    let expected = [format!("https://open.spotify.com/track/{TRACK}")];
    assert_eq!(links(pool.clone(), tidal_id.clone()).await, expected);
    // A refusal and a rate limit keep it (each with a pair of its own: a refusal holds the
    // pair's slot for five minutes, a rate limit for the time asked).
    for (status, pair) in [
        (403, ("isrc-refused-id", "isrc-refused-secret")),
        (429, ("isrc-busy-id", "isrc-busy-secret")),
    ] {
        let seen = run(
            &pool,
            config(Some(pair)),
            Answers {
                api_status: Some(status),
                ..Default::default()
            },
        )
        .await;
        assert!(
            seen.iter().any(|s| s.contains("spotify.com")),
            "{status}: {seen:?}"
        );
        assert_eq!(
            links(pool.clone(), tidal_id.clone()).await,
            expected,
            "{status}"
        );
    }
    // Switched off: the rows stay, the card no longer shows them.
    let card = |pool: sqlx::PgPool, id: String| async move {
        let card: serde_json::Value = sqlx::query_scalar("SELECT also_on($1,$1,8,true)")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        card.as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["uri"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    also_on::configure(&pool, &on).await.unwrap();
    assert!(
        card(pool.clone(), tidal_id.clone())
            .await
            .contains(&expected[0]),
        "{:?}",
        card(pool.clone(), tidal_id.clone()).await
    );
    let mut off = config(Some(("isrc-default-id", "isrc-default-secret")));
    off.also_on.sources.insert(
        "spotify_isrc".into(),
        SourceOptions {
            enabled: Some(false),
            ..Default::default()
        },
    );
    off.validate().unwrap();
    also_on::configure(&pool, &off).await.unwrap();
    assert_eq!(links(pool.clone(), tidal_id.clone()).await, expected);
    assert!(
        !card(pool.clone(), tidal_id.clone())
            .await
            .contains(&expected[0]),
        "{:?}",
        card(pool.clone(), tidal_id.clone()).await
    );
}

/// An album on another service with a barcode key is looked up on Spotify by it (`spotify_isrc`):
/// a UPC-A by its 12 digits, an EAN-13 whole; the search's albums (three at most) are read back
/// one by one (`/v1/albums/{id}`: Spotify answers no batch read to an app in development mode),
/// and only the one whose own barcode is the key becomes a link keyed by it. A 403 on an album's
/// read and a refusal of the app keep it.
#[sqlx::test(migrations = "../../migrations")]
async fn an_album_with_a_barcode_key_gets_its_spotify_album(pool: sqlx::PgPool) {
    let registered: Vec<&str> = also_on::registry().iter().map(|s| s.id()).collect();
    if !registered.contains(&"spotify_isrc") {
        return;
    }
    let keyed = |uri: &'static str, upc: &'static str| {
        let pool = pool.clone();
        async move {
            let id = save(&pool, uri, "MusicAlbum").await;
            sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) VALUES($1,$2,'test','test',now()+interval '1 day')")
                .bind(&id)
                .bind(upc)
                .execute(&pool)
                .await
                .unwrap();
            id
        }
    };
    // HYDRA 3D on Deezer (its EAN-13), and Random Access Memories on Deezer (a UPC-A, which this
    // Spotify's search lists among more it found).
    let hydra = keyed(
        "https://www.deezer.com/album/320429237",
        "upc:4018939306052",
    )
    .await;
    let ram = keyed("https://www.deezer.com/album/6575789", "upc:0886443927087").await;
    // Underclass Hero on Deezer: of its two candidates, one carries the barcode and one gives no
    // album (404), which might have been another: no link.
    let underclass = keyed("https://www.deezer.com/album/125431", "upc:0602517416987").await;
    let seen = run(
        &pool,
        config(Some(("upc-id", "upc-secret"))),
        Answers::default(),
    )
    .await;
    let mut asked: Vec<String> = seen
        .iter()
        .filter(|s| {
            s.starts_with("api.spotify.com/v1/search?")
                || s.starts_with("api.spotify.com/v1/albums/")
        })
        .cloned()
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            format!("api.spotify.com/v1/albums/{ALBUM}?"),
            format!("api.spotify.com/v1/albums/{MISSING_UPC_ALBUM}?"),
            format!("api.spotify.com/v1/albums/{OTHER_UPC_ALBUM}?"),
            "api.spotify.com/v1/search?q=upc%3A4018939306052&type=album&limit=3".to_owned(),
            "api.spotify.com/v1/search?q=upc%3A602517416987&type=album&limit=3".to_owned(),
            "api.spotify.com/v1/search?q=upc%3A886443927087&type=album&limit=3".to_owned(),
        ],
        "{seen:?}"
    );
    let links = "SELECT uri||' '||coalesce(key,'-')||' '||via FROM derived_link WHERE resource_id=$1 AND source='spotify_isrc' ORDER BY uri";
    let expected = [format!(
        "https://open.spotify.com/album/{ALBUM} upc:4018939306052 spotify_isrc"
    )];
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    // Random Access Memories: a search that found more albums than it lists names none, and its
    // album is not read.
    assert!(rows(&pool, links, &ram).await.is_empty());
    assert!(rows(&pool, links, &underclass).await.is_empty());
    assert!(
        !seen.iter().any(|s| s.contains(PAGED_UPC_ALBUM)),
        "{seen:?}"
    );
    let queued = || async {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT last_error FROM also_on_dirty WHERE resource_id=$1",
        )
        .bind(&hydra)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    // A 403 on one album's read alone: the link stays, the lookup is retried, and the pair is not
    // refused: its next run asks Spotify again.
    let pair = ("upc-album-id", "upc-album-secret");
    run(
        &pool,
        config(Some(pair)),
        Answers {
            album_status: Some(403),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    assert_eq!(queued().await.as_deref(), Some("forbidden_request"));
    let seen = run(&pool, config(Some(pair)), Answers::default()).await;
    assert!(
        seen.iter()
            .any(|s| s.starts_with("api.spotify.com/v1/search?q=upc")),
        "{seen:?}"
    );
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    // Spotify refusing the app (no Premium): the link stays, and no run fails.
    run(
        &pool,
        config(Some(("upc-refused-id", "upc-refused-secret"))),
        Answers {
            api_status: Some(403),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(rows(&pool, links, &hydra).await, expected);
    assert_eq!(queued().await, None);
}
