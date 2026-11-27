//! The search query set: every parameter and combination pubky.app sends, over a fixed set of
//! cards, asked of one build twice: once with the text index (as the daemon serves `q`) and once
//! reading Postgres alone (as before the index, and as a search does while the daemon's index is
//! not open yet). A query without text, or with a social filter, gives the same page both ways; a
//! text query finds through the index every card Postgres ranks in its top 20, and its expected
//! card first.
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    model::{Point, ResourceMeta},
    search_index::SearchIndex,
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
mod common;
use common::author;

/// A card: its number, name, description, type, tag label, taggers, point and start.
type Spec = (
    usize,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    usize,
    Option<(f64, f64)>,
    Option<&'static str>,
);
const CARDS: &[Spec] = &[
    (
        1,
        "Daft Punk",
        "French electronic music duo.",
        "MusicGroup",
        "electronic",
        6,
        None,
        None,
    ),
    (
        2,
        "Discovery",
        "Studio album by Daft Punk.",
        "MusicAlbum",
        "electronic",
        3,
        None,
        None,
    ),
    (
        3,
        "Around the World",
        "Single by Daft Punk from Homework.",
        "MusicRecording",
        "house",
        2,
        None,
        None,
    ),
    (
        4,
        "Kraftwerk",
        "German electronic music band.",
        "MusicGroup",
        "electronic",
        4,
        None,
        None,
    ),
    (
        5,
        "Music for Airports",
        "Ambient album by Brian Eno.",
        "MusicAlbum",
        "ambient",
        2,
        None,
        None,
    ),
    (
        6,
        "Kind of Blue",
        "Jazz album by Miles Davis.",
        "MusicAlbum",
        "jazz",
        5,
        None,
        None,
    ),
    (
        7,
        "So What",
        "Opening track of Kind of Blue.",
        "MusicRecording",
        "jazz",
        1,
        None,
        None,
    ),
    (
        8,
        "Björk",
        "Icelandic singer and songwriter.",
        "Person",
        "music",
        3,
        None,
        None,
    ),
    (
        9,
        "Sigur Rós",
        "Post-rock band from Reykjavík.",
        "MusicGroup",
        "postrock",
        2,
        None,
        None,
    ),
    (
        10,
        "The Beatles",
        "English rock band from Liverpool.",
        "MusicGroup",
        "rock",
        8,
        None,
        None,
    ),
    (
        11,
        "Abbey Road",
        "Eleventh studio album by the Beatles.",
        "MusicAlbum",
        "rock",
        4,
        None,
        None,
    ),
    (
        12,
        "Beat It",
        "Song by Michael Jackson.",
        "MusicRecording",
        "pop",
        2,
        None,
        None,
    ),
    (
        13,
        "Zürich Hauptbahnhof",
        "Main railway station of Zürich.",
        "TrainStation",
        "travel",
        3,
        Some((47.3779, 8.5403)),
        None,
    ),
    (
        14,
        "Café Odeon",
        "Historic café in Zürich.",
        "CafeOrCoffeeShop",
        "coffee",
        2,
        Some((47.3664, 8.5452)),
        None,
    ),
    (
        15,
        "Kunsthaus Zürich",
        "Art museum.",
        "Museum",
        "art",
        2,
        Some((47.3703, 8.5482)),
        None,
    ),
    (
        16,
        "Bern Bear Park",
        "Bears by the Aare river in Bern.",
        "Park",
        "bears",
        1,
        Some((46.9480, 7.4598)),
        None,
    ),
    (
        17,
        "Lake Geneva promenade",
        "Walk along Lac Léman in Lausanne.",
        "Place",
        "lake",
        1,
        Some((46.5070, 6.6264)),
        None,
    ),
    (
        18,
        "Café du Soleil",
        "Restaurant in Geneva known for fondue.",
        "Restaurant",
        "fondue",
        2,
        Some((46.2010, 6.1530)),
        None,
    ),
    (
        19,
        "São Paulo Museum of Art",
        "Art museum on Avenida Paulista.",
        "Museum",
        "art",
        1,
        Some((-23.5614, -46.6559)),
        None,
    ),
    (
        20,
        "İzmir Clock Tower",
        "Landmark in İzmir.",
        "LandmarksOrHistoricalBuildings",
        "travel",
        1,
        Some((38.4189, 27.1287)),
        None,
    ),
    (
        21,
        "Rust Meetup Zürich",
        "Monthly meetup about the Rust programming language.",
        "Event",
        "rust",
        3,
        Some((47.3769, 8.5417)),
        Some("2026-12-03T18:00:00Z"),
    ),
    (
        22,
        "Bitcoin Conference Lugano",
        "Plan B forum on Bitcoin.",
        "Event",
        "bitcoin",
        4,
        Some((46.0037, 8.9511)),
        Some("2027-03-20T09:00:00Z"),
    ),
    (
        23,
        "Jazz Night at Moods",
        "Live jazz in Zürich.",
        "MusicEvent",
        "jazz",
        2,
        Some((47.3890, 8.5170)),
        Some("2026-12-10T20:00:00Z"),
    ),
    (
        24,
        "Street Parade",
        "Techno parade around the lake of Zürich.",
        "Festival",
        "techno",
        5,
        Some((47.3667, 8.5450)),
        Some("2027-08-14T13:00:00Z"),
    ),
    (
        25,
        "lightningnetwork/lnd",
        "Lightning Network Daemon.",
        "SoftwareApplication",
        "lightning",
        2,
        None,
        None,
    ),
    (
        26,
        "btcpayserver/btcpayserver",
        "Self-hosted payment server.",
        "SoftwareApplication",
        "bitcoin",
        3,
        None,
        None,
    ),
    (
        27,
        "BTC Map",
        "Find places to spend bitcoin.",
        "WebApplication",
        "bitcoin",
        2,
        None,
        None,
    ),
    (
        28,
        "VLC media player",
        "Plays most music and video files.",
        "SoftwareApplication",
        "video",
        5,
        None,
        None,
    ),
    (
        29,
        "The Rust Programming Language",
        "The book about Rust.",
        "Book",
        "rust",
        6,
        None,
        None,
    ),
    (
        30,
        "Rustlings",
        "Small exercises to get you used to reading and writing Rust code.",
        "SoftwareApplication",
        "rust",
        2,
        None,
        None,
    ),
    (
        31,
        "Tokio",
        "An asynchronous runtime for Rust.",
        "SoftwareApplication",
        "rust",
        3,
        None,
        None,
    ),
    (
        32,
        "PostgreSQL",
        "The world's most advanced open source relational database.",
        "SoftwareApplication",
        "database",
        4,
        None,
        None,
    ),
    (
        33,
        "Pubky App",
        "Social app on Pubky.",
        "WebApplication",
        "pubky",
        7,
        None,
        None,
    ),
    (
        34,
        "Synonym",
        "Company building Pubky.",
        "Organization",
        "pubky",
        2,
        None,
        None,
    ),
    (
        35,
        "Lex Fridman Podcast",
        "Conversations about science, technology, history, philosophy and music.",
        "PodcastSeries",
        "podcast",
        6,
        None,
        None,
    ),
    (
        36,
        "The Bitcoin Standard",
        "Book by Saifedean Ammous.",
        "Book",
        "bitcoin",
        3,
        None,
        None,
    ),
    (
        37,
        "Mastering Bitcoin",
        "Programming the open blockchain.",
        "Book",
        "bitcoin",
        2,
        None,
        None,
    ),
    (
        38,
        "Why Zürich is expensive",
        "An article about rents in Zürich.",
        "Article",
        "housing",
        1,
        None,
        None,
    ),
    (
        39,
        "How to make fondue",
        "Recipe with Gruyère and Vacherin.",
        "Recipe",
        "fondue",
        2,
        None,
        None,
    ),
    (
        40,
        "Café culture in Vienna",
        "Article on Viennese coffee houses.",
        "Article",
        "coffee",
        1,
        None,
        None,
    ),
    (
        41,
        "Moby-Dick",
        "Novel by Herman Melville.",
        "Book",
        "novel",
        2,
        None,
        None,
    ),
    (
        42,
        "Pride and Prejudice",
        "Novel by Jane Austen.",
        "Book",
        "novel",
        3,
        None,
        None,
    ),
    (
        43,
        "東京タワー",
        "東京の電波塔。",
        "LandmarksOrHistoricalBuildings",
        "tokyo",
        1,
        Some((35.6586, 139.7454)),
        None,
    ),
    (
        44,
        "我的AI助手",
        "聊天。",
        "SoftwareApplication",
        "assistant",
        1,
        None,
        None,
    ),
    (
        45,
        "Москва",
        "Столица России.",
        "City",
        "russia",
        1,
        Some((55.7558, 37.6173)),
        None,
    ),
    (
        46,
        "Ελληνικά",
        "Greek language page.",
        "WebPage",
        "greek",
        1,
        None,
        None,
    ),
    (
        47,
        "C++ reference",
        "Documentation of the C++ standard library.",
        "WebPage",
        "cpp",
        2,
        None,
        None,
    ),
    (
        48,
        "Node.js",
        "JavaScript runtime.",
        "SoftwareApplication",
        "javascript",
        3,
        None,
        None,
    ),
    (
        49,
        "Coffee",
        "A brewed drink.",
        "WebPage",
        "coffee",
        1,
        None,
        None,
    ),
    (
        50,
        "Coffee grinder reviews",
        "Reviews of burr grinders for coffee.",
        "Article",
        "coffee",
        1,
        None,
        None,
    ),
    (
        51,
        "Daft",
        "A word page.",
        "WebPage",
        "words",
        1,
        None,
        None,
    ),
    (
        52,
        "Daft Punk tribute night",
        "A cover band page.",
        "WebPage",
        "covers",
        1,
        None,
        None,
    ),
];

/// The host a card's link is on, by its type, so `host=` has something to tell apart.
fn host(type_name: &str) -> &'static str {
    match type_name {
        t if t.starts_with("Music") || t == "Person" => "music.example.org",
        "Book" | "Article" | "Recipe" | "PodcastSeries" => "read.example.org",
        "SoftwareApplication" | "WebApplication" | "Organization" => "code.example.org",
        _ => "places.example.org",
    }
}

async fn cards(pool: &PgPool) -> HashMap<usize, String> {
    let mut ids = HashMap::new();
    for &(n, name, description, type_name, label, taggers, point, start) in CARDS {
        let uri = format!("https://{}/card/{n}", host(type_name));
        let id = store::ensure_resource(pool, &uri).await.unwrap();
        let mut doc = ResourceMeta::new(&uri, "web", 86400);
        doc.name = Some(name.into());
        doc.description = Some(description.into());
        doc.set_type(type_name);
        doc.location = point.map(|(lat, lon)| Point { lat, lon });
        doc.start = start.map(|s| s.parse().unwrap());
        store::save_document(pool, &id, 0, &doc).await.unwrap();
        for a in 0..taggers {
            sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag',$3,$4,now()-make_interval(hours=>$5))")
                .bind(&id)
                .bind(author(n * 100 + a))
                .bind(label)
                .bind(format!("/pub/app/tags/{n}-{a}"))
                .bind(n as i32)
                .execute(pool)
                .await
                .unwrap();
        }
        let mut tx = pool.begin().await.unwrap();
        store::recompute_scores(&mut tx, std::slice::from_ref(&id))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        ids.insert(n, id);
    }
    ids
}

/// The query, with each value percent-encoded.
fn path(query: &str) -> String {
    let encoded: Vec<String> = query
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap();
            format!(
                "{key}={}",
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            )
        })
        .collect();
    format!("/v1/search?{}", encoded.join("&"))
}
/// A page without its cursor, which each daemon signs with its own key: whether there is one.
fn uncursored(page: &Value) -> Value {
    let mut page = page.clone();
    page["cursor"] = page["cursor"].is_null().into();
    page
}
fn ids_of(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Text queries, each with the card that must come first (`0`: no single best card).
const TEXT: &[(&str, usize)] = &[
    ("q=daft punk", 1),
    ("q=Daft Punk", 1),
    ("q=  daft   punk  ", 1),
    ("q=\"daft punk\"", 1),
    ("q=daft", 51),
    ("q=daft -punk", 51),
    ("q=punk", 1),
    ("q=dafty punk", 1),
    ("q=daft or kraftwerk", 0),
    ("q=discovery", 2),
    ("q=around the world", 3),
    ("q=homework", 3),
    ("q=kraftwerk", 4),
    ("q=kraftwerg", 4),
    ("q=kraftwrek", 4),
    ("q=electronic music", 0),
    ("q=music for airports", 5),
    ("q=airports", 5),
    ("q=brian eno", 5),
    ("q=kind of blue", 6),
    ("q=miles davis", 6),
    ("q=so what", 7),
    ("q=bjork", 8),
    ("q=Björk", 8),
    ("q=bjrk", 8),
    ("q=sigur ros", 9),
    ("q=reykjavik", 9),
    ("q=beatles", 10),
    ("q=the beatles", 10),
    ("q=beatl", 10),
    ("q=abbey road", 11),
    ("q=beat", 12),
    ("q=michael jackson", 12),
    ("q=zurich", 0),
    ("q=zürich", 0),
    ("q=zürich hauptbahnhof", 13),
    ("q=hauptbahnhof", 13),
    ("q=railway station", 13),
    ("q=cafe odeon", 14),
    ("q=café", 0),
    ("q=kunsthaus", 15),
    ("q=art museum", 19),
    ("q=sao paulo", 19),
    ("q=São Paulo", 19),
    ("q=bern", 16),
    ("q=bears", 16),
    ("q=geneva", 17),
    ("q=leman", 17),
    ("q=fondue", 39),
    ("q=izmir", 20),
    ("q=İzmir", 20),
    ("q=clock tower", 20),
    ("q=rust", 0),
    ("q=rust programming language", 29),
    ("q=rustlings", 30),
    ("q=tokio", 31),
    ("q=postgres", 32),
    ("q=postgresql", 32),
    ("q=relational database", 32),
    ("q=pubky", 33),
    ("q=pubky app", 33),
    ("q=synonym", 34),
    ("q=lex fridman", 35),
    ("q=podcast", 35),
    ("q=bitcoin", 0),
    ("q=bitcoin standard", 36),
    ("q=mastering bitcoin", 37),
    ("q=btc", 27),
    ("q=lnd", 25),
    ("q=lightning", 25),
    ("q=vlc", 28),
    ("q=media player", 28),
    ("q=moby dick", 41),
    ("q=moby-dick", 41),
    ("q=melville", 41),
    ("q=pride and prejudice", 42),
    ("q=austen", 42),
    ("q=jane austen novel", 42),
    ("q=novel", 0),
    ("q=東京", 43),
    ("q=東京タワー", 43),
    ("q=タワー", 43),
    ("q=ai", 0),
    ("q=助手", 44),
    ("q=москва", 45),
    ("q=Москва", 45),
    ("q=Ελληνικά", 46),
    ("q=c++", 47),
    ("q=node.js", 48),
    ("q=nodejs", 0),
    ("q=coffee", 49),
    ("q=coffee grinder", 50),
    ("q=grinder", 50),
    ("q=burr", 50),
    ("q=vienna coffee", 40),
    ("q=meetup", 21),
    ("q=jazz", 23),
    ("q=jazz night", 23),
    ("q=street parade", 24),
    ("q=techno", 24),
    ("q=plan b forum", 22),
    ("q=lugano", 22),
    ("q=xyzzy", 0),
    ("q=qqqqqq", 0),
    ("q=!!!", 0),
    // With filters the index holds.
    ("q=daft&type=MusicGroup", 1),
    ("q=daft punk&type=MusicAlbum,MusicRecording", 0),
    ("q=album&exclude_type=MusicAlbum", 0),
    ("q=zürich&type=Museum", 15),
    ("q=zürich&type=Event", 21),
    ("q=rust&tags=rust", 0),
    ("q=bitcoin&tags=bitcoin,rust", 0),
    ("q=bitcoin&tags=bitcoin&match=all", 0),
    ("q=coffee&tags=coffee", 49),
    ("q=rust&host=code.example.org", 0),
    ("q=rust&host=read.example.org", 29),
    ("q=zürich&near=47.3769,8.5417&radius=2000", 0),
    ("q=zürich&near=47.3769,8.5417&radius=2000&sort=distance", 21),
    ("q=café&near=46.2,6.15&radius=5000", 18),
    ("q=museum&bbox=-50,-30,-40,-20", 19),
    (
        "q=jazz&from=2026-12-01T00:00:00Z&to=2026-12-31T00:00:00Z",
        23,
    ),
    ("q=meetup&sort=start", 21),
    ("q=zürich&sort=start", 21),
    ("q=rust&sort=activity", 29),
    ("q=rust&sort=recent", 0),
    ("q=bitcoin&sort=trending&window=7d", 0),
    ("q=daft&collapse=group", 51),
    ("q=daft&facet=type,tags", 51),
    ("q=daft&referenced=true", 51),
    ("q=daft punk&limit=1", 1),
    ("q=coffee&limit=2&facet=type,tags,host", 49),
];
/// Queries without text, or with a social filter: Postgres answers them both ways.
const PLAIN: &[&str] = &[
    "type=MusicGroup",
    "type=Place",
    "type=Event&sort=start",
    "exclude_type=MusicAlbum",
    "tags=rust",
    "tags=rust,bitcoin",
    "tags=rust,bitcoin&match=all",
    "sort=recent",
    "sort=activity",
    "sort=trending&window=7d",
    "near=47.3769,8.5417&radius=2000",
    "near=47.3769,8.5417&radius=2000&sort=distance",
    "bbox=5,45,11,48",
    "from=2026-12-01T00:00:00Z",
    "sort=start&from=2026-11-01T00:00:00Z",
    "facet=type,tags",
    "collapse=group",
    "host=music.example.org",
    "limit=5",
    "referenced=true",
    "type=SoftwareApplication&facet=type,tags,host,category",
];

#[sqlx::test(migrations = "../../migrations")]
async fn the_query_set_finds_what_postgres_finds(pool: PgPool) {
    let ids = cards(&pool).await;
    let name: HashMap<&String, &str> = ids.iter().map(|(n, id)| (id, CARDS[n - 1].1)).collect();
    // Some 300 requests from one client.
    let mut config = Config::default();
    config.api.requests_per_minute = 10_000;
    let state = AppState::new(pool.clone(), config.clone()).unwrap();
    let index = state.search_index.clone();
    let indexed = api::router(state);
    // Every catalogue off: with the catalogue tables empty, every answer is the same.
    let mut off_config = config.clone();
    off_config.catalogues.enabled = false;
    let off = api::router(AppState::new(pool.clone(), off_config).unwrap());
    let mut state = AppState::new(pool.clone(), config).unwrap();
    // A daemon's index that is not open: every search reads Postgres alone.
    state.search_index = SearchIndex::on_disk("/nonexistent/pinakes-search-index");
    let postgres = api::router(state);
    let social = [
        format!("author={}", author(100)),
        format!("q=daft&author={}", author(100)),
        format!("q=rust&author={}&sort=recent", author(2900)),
        format!("related={}", ids[&1]),
        format!(
            "in_collection=pubky://{}/pub/pubky.app/posts/0033SSE3B1FQ0",
            author(1)
        ),
        format!(
            "q=daft&in_collection=pubky://{}/pub/pubky.app/posts/0033SSE3B1FQ0",
            author(1)
        ),
        "q=coffee&key=wikidata:Q8486&type=WebPage".to_owned(),
    ];
    let mut report = vec![];
    let mut checked = 0;
    for query in PLAIN.iter().map(|q| q.to_string()).chain(social) {
        let a = common::get(&indexed, &path(&query)).await;
        let b = common::get(&postgres, &path(&query)).await;
        assert_eq!(ids_of(&a), ids_of(&b), "{query}");
        assert_eq!(
            uncursored(&a),
            uncursored(&common::get(&off, &path(&query)).await),
            "{query}: catalogues off"
        );
        assert_eq!(a["facets"], b["facets"], "{query}");
        assert_eq!(a["cursor"].is_null(), b["cursor"].is_null(), "{query}");
        checked += 1;
    }
    for (query, first) in TEXT {
        let wide = |q: &str| {
            if q.contains("limit=") {
                q.to_owned()
            } else {
                format!("{q}&limit=50")
            }
        };
        let page = common::get(&indexed, &path(&wide(query))).await;
        assert_eq!(
            uncursored(&page),
            uncursored(&common::get(&off, &path(&wide(query))).await),
            "{query}: catalogues off"
        );
        let found = ids_of(&page);
        let before = ids_of(&common::get(&postgres, &path(&wide(query))).await);
        let top: Vec<&String> = before.iter().take(20).collect();
        // Postgres' name similarity also finds names holding a word the text leaves out
        // (`-punk`); the index leaves them out.
        let negated: Vec<String> = query
            .split(['=', ' ', '&'])
            .filter_map(|w| w.strip_prefix('-'))
            .map(str::to_lowercase)
            .collect();
        let missing: Vec<&str> = top
            .iter()
            .filter(|id| !found.contains(id))
            .map(|id| name[id])
            .filter(|n| {
                !negated
                    .iter()
                    .any(|w| n.to_lowercase().contains(w.as_str()))
            })
            .collect();
        assert!(missing.is_empty(), "{query}: not found {missing:?}");
        if *first > 0 {
            assert_eq!(
                found.first().map(|id| name[id]),
                Some(CARDS[first - 1].1),
                "{query}: {:?}",
                found.iter().map(|id| name[id]).collect::<Vec<_>>()
            );
        }
        if found.iter().take(top.len()).collect::<Vec<_>>() != top {
            report.push(format!(
                "{query}: index {:?} / postgres {:?}",
                found.iter().map(|id| name[id]).collect::<Vec<_>>(),
                top.iter().map(|id| name[*id]).collect::<Vec<_>>()
            ));
        }
        checked += 1;
    }
    assert!(checked >= 100, "{checked} queries");
    // The index route answered every text query (else both sides read Postgres).
    assert!(index.is_open());
    assert!(
        index.served() as usize >= TEXT.len(),
        "{} served",
        index.served()
    );
    // Texts with `or`: the index reads `or` as main's index does, joining the items around it
    // (`daft punk or kraftwerk` is daft, and punk or kraftwerk), where Postgres alone binds it
    // looser (`daft punk`, or `kraftwerk`); a text cut inside a chain of `or`s leaves the chain
    // out. Each answer is the same with every catalogue off; what Postgres alone finds besides is
    // printed, main's reading and not a loss.
    let chain = std::iter::repeat_n("daft", 19)
        .chain(["kraftwerk"])
        .collect::<Vec<_>>()
        .join(" or ");
    let or_texts = [
        "q=daft punk or kraftwerk".to_owned(),
        "q=daft -punk or kraftwerk".to_owned(),
        "q=-punk or kraftwerk".to_owned(),
        "q=\"daft punk\"or kraftwerk".to_owned(),
        format!("q={chain}"),
    ];
    for query in &or_texts {
        let page = common::get(&indexed, &path(query)).await;
        assert_eq!(
            uncursored(&page),
            uncursored(&common::get(&off, &path(query)).await),
            "{query}: catalogues off"
        );
        let found = ids_of(&page);
        let alone: Vec<&str> = ids_of(&common::get(&postgres, &path(query)).await)
            .iter()
            .filter(|id| !found.contains(id))
            .map(|id| name[id])
            .collect();
        println!("or, main's reading: {query}: Postgres alone also finds {alone:?}");
    }
    // The chain of 20, cut at 32 items, is read whole as main's index reads it (no cut): the same
    // units as `daft or kraftwerk`.
    let sorted = |page: &Value| {
        let mut ids = ids_of(page);
        ids.sort();
        ids
    };
    let whole = sorted(&common::get(&indexed, &path(&format!("q={chain}&limit=50"))).await);
    let short = sorted(&common::get(&indexed, &path("q=daft or kraftwerk&limit=50")).await);
    assert!(!short.is_empty());
    assert_eq!(whole, short, "the chain of 20 as main reads it");
    // The orders that differ (BM25 and the activity's weight against the former tiers), for the
    // record: run with --nocapture to read them.
    for line in report {
        println!("order: {line}");
    }
}
