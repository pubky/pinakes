//! The `own_ids` also-on source: an official card's own ids (a DOI under its service's own
//! prefix, the Wikidata item its record names) are its keys, so the card joins the group of the
//! `doi:` card of its DOI, or of the Wikidata item's card. Any other id stays a fact.
mod common;

use common::media::*;
use pinakes_core::{
    also_on::{self, own_ids::OwnIds, AlsoOnConfig, Found, Key, Source},
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    sources::{kit::OwnId, REGISTRY},
    store,
};
use serde_json::Value;
use sqlx::PgPool;

/// Every official source that declares its own DOIs, with their prefix.
fn doi_sources() -> Vec<(&'static str, &'static str)> {
    REGISTRY
        .iter()
        .flat_map(|s| {
            s.own_ids().iter().filter_map(|o| match o {
                OwnId::Doi(prefix) => Some((s.name(), *prefix)),
                OwnId::Wikidata => None,
            })
        })
        .collect()
}
/// Every official source that declares the Wikidata item its record names.
fn wikidata_sources() -> Vec<&'static str> {
    REGISTRY
        .iter()
        .filter(|s| s.own_ids().contains(&OwnId::Wikidata))
        .map(|s| s.name())
        .collect()
}

/// A card of `source` for `uri` with these facts.
fn card(source: &str, uri: &str, facts: &[(&str, &str)]) -> ResourceMeta {
    let mut doc = ResourceMeta::new(uri, source, 3600);
    doc.set_type("CreativeWork");
    doc.name = Some("A record".into());
    for (name, value) in facts {
        doc.attributes.insert((*name).into(), (*value).into());
    }
    doc
}

#[tokio::test]
async fn only_a_services_own_doi_is_a_key() {
    let cx = cx(Recorded::new(), Config::default());
    for (source, prefix) in doi_sources() {
        let uri = "https://example.org/records/7790295";
        let own = format!("{prefix}7790295");
        // A publisher's DOI the depositor typed, beside the service's own.
        let doc = card(
            source,
            uri,
            &[("doi", &format!("10.1175/2010mwr3413.1; {own}"))],
        );
        let s = subject(uri, Some("CreativeWork"), Some(doc));
        assert!(OwnIds.applies(&s), "{source}");
        let found = OwnIds.lookup(&cx, &s).await.unwrap();
        assert_eq!(keys(&found), [format!("doi:{own}")], "{source}");
        assert_eq!(found.keys[0].via, source);
        assert!(found.links.is_empty() && found.facts.is_empty());
        // Only the typed one: no key.
        let doc = card(source, uri, &[("doi", "10.1175/2010mwr3413.1")]);
        let s = subject(uri, Some("CreativeWork"), Some(doc));
        assert!(!OwnIds.applies(&s), "{source}");
        assert!(OwnIds.lookup(&cx, &s).await.unwrap().keys.is_empty());
    }
}

/// An id an official source does not declare its own (an ISBN, an InChIKey, a DOI or Wikidata
/// item of a source that declares none) is a fact and no key.
#[tokio::test]
async fn an_id_the_source_does_not_declare_is_no_key() {
    let cx = cx(Recorded::new(), Config::default());
    let facts = [
        ("doi", "10.2210/pdb4hhb/pdb"),
        ("wikidata", "Q42"),
        ("isbn", "9780140449136"),
        ("openlibrary", "OL7355481M"),
        ("inchikey", "BSYNRYMUTXBXSQ-UHFFFAOYSA-N"),
    ];
    let undeclared = REGISTRY
        .iter()
        .filter(|s| s.own_ids().is_empty())
        .map(|s| s.name())
        .chain(["web", "jsonld", "doi"]);
    for source in undeclared {
        let uri = "https://example.org/records/1";
        let s = subject(uri, Some("CreativeWork"), Some(card(source, uri, &facts)));
        assert!(!OwnIds.applies(&s), "{source}");
        assert!(OwnIds.lookup(&cx, &s).await.unwrap().keys.is_empty());
    }
    // A source declaring DOIs takes none of these, nor a Wikidata item.
    for (source, _) in doi_sources() {
        let uri = "https://example.org/records/1";
        let s = subject(uri, Some("CreativeWork"), Some(card(source, uri, &facts)));
        assert!(!OwnIds.applies(&s), "{source}");
    }
}

/// Save a referenced resource with `doc` and the keys one also-on source found for it.
async fn save(pool: &PgPool, uri: &str, doc: &ResourceMeta, source: &str, found: Found) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    store::save_document(pool, &id, 0, doc).await.unwrap();
    also_on::record(
        pool,
        &id,
        source,
        found,
        std::time::Duration::from_secs(86400),
        &AlsoOnConfig::default(),
    )
    .await
    .unwrap();
    pinakes_core::groups::reconcile(pool, &id, &Default::default()).await;
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','record',$3,now())")
        .bind(&id)
        .bind("a".repeat(52))
        .bind(format!("/pub/app/tags/{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

/// The official card of `source` at `uri` with `facts`, its `own_ids` keys recorded.
async fn save_record(pool: &PgPool, source: &str, uri: &str, facts: &[(&str, &str)]) -> String {
    let cx = cx(Recorded::new(), Config::default());
    let doc = card(source, uri, facts);
    let found = OwnIds
        .lookup(&cx, &subject(uri, Some("CreativeWork"), Some(doc.clone())))
        .await
        .unwrap();
    save(pool, uri, &doc, "own_ids", found).await
}

/// A hub's card of its own key (a `doi:` card, a Wikidata item), the key recorded as its own
/// also-on source records it.
async fn save_hub(pool: &PgPool, uri: &str, route: &str, source: &str, key: &str) -> String {
    let mut doc = ResourceMeta::new(uri, route, 3600);
    doc.set_type("CreativeWork");
    doc.name = Some("A record".into());
    let found = Found {
        keys: vec![Key {
            key: key.into(),
            via: source.into(),
        }],
        ..Default::default()
    };
    save(pool, uri, &doc, source, found).await
}

/// Each member's group, read from it: the same group of `ids`, whichever member is read.
async fn assert_one_group(pool: &PgPool, ids: &[&str], what: &str) {
    common::drain_groups(pool, &Default::default()).await;
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let mut groups = vec![];
    for id in ids {
        let card: Value = get(&app, &format!("/v1/resources/{id}")).await;
        assert_eq!(card["group"]["size"], ids.len(), "{what}: {card}");
        for other in ids.iter().filter(|o| *o != id) {
            assert!(
                card["also_on"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["id"] == *other),
                "{what}: {card}"
            );
        }
        groups.push(card["group"]["id"].clone());
    }
    groups.dedup();
    assert_eq!(groups.len(), 1, "{what}: {groups:?}");
}

/// A record and the `doi:` card of its own DOI are one group, read from either.
#[sqlx::test(migrations = "../../migrations")]
async fn a_record_joins_the_group_of_its_own_doi(pool: PgPool) {
    for (n, (source, prefix)) in doi_sources().into_iter().enumerate() {
        let own = format!("{prefix}{}", 7790295 + n);
        let record = save_record(
            &pool,
            source,
            &format!("https://example.org/{source}/{n}"),
            &[("doi", &own)],
        )
        .await;
        let doi = save_hub(
            &pool,
            &format!("doi:{own}"),
            "doi",
            "datacite",
            &format!("doi:{own}"),
        )
        .await;
        assert_one_group(&pool, &[&record, &doi], source).await;
    }
}

/// A record and the Wikidata item its record names are one group, read from either.
#[sqlx::test(migrations = "../../migrations")]
async fn a_record_joins_the_group_of_its_wikidata_item(pool: PgPool) {
    for (n, source) in wikidata_sources().into_iter().enumerate() {
        let q = format!("Q{}", 18689458 + n);
        let record = save_record(
            &pool,
            source,
            &format!("https://example.org/{source}/{n}"),
            &[("wikidata", &q)],
        )
        .await;
        let item = save_hub(
            &pool,
            &format!("https://www.wikidata.org/wiki/{q}"),
            "wikidata",
            "wikidata_item",
            &format!("wikidata:{q}"),
        )
        .await;
        assert_one_group(&pool, &[&record, &item], source).await;
    }
}

async fn get(app: &axum::Router, path: &str) -> Value {
    let (status, value) = common::get_from(app, "127.0.0.1:1111", path).await;
    assert_eq!(status, 200, "{path}");
    value
}
