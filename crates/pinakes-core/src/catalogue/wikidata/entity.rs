//! One Wikidata entity (a line of the JSON dump, or an entity of a `wbgetentities` answer) as a
//! catalogue card.
//!
//! - **Kept:** items with an article in a Wikipedia edition ([`wikipedia_articles`]) and none of
//!   Wikimedia's own classes ([`WIKIMEDIA_PAGES`]: categories, disambiguation pages, templates,
//!   lists, …) as a direct `P31`.
//! - **Link:** the English article, else the article of the first edition in
//!   `wikipedia::LANGUAGES` the item has, else the edition whose site name sorts first.
//! - **Keys:** `wikidata:Q…`; each identifier property with exactly one best-rank valid value
//!   (MusicBrainz at the item's own level, OpenStreetMap, IMDb, ISBN, DOI); a `url:` key for each
//!   Wikipedia article and each page of a service the item's identifiers name as the item itself
//!   (maps, stores, film, book and music services), with the keys those pages name.
//! - **Related:** the official websites (P856).
//! - **Type** from the direct `P31` ([`TYPES`]), music by its identifiers, else `Place` with an
//!   Earth coordinate, else `Thing`; **fame** from the number of Wikipedia editions.
//! - **Labels:** the items its genres (P136) and classes (P31) name, resolved to their English
//!   labels through [`Labels`].
use crate::catalogue::Card;
use crate::hubs::wikidata as hub;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

/// The longest entity line read (bytes). The largest items of the dump are a few megabytes
/// (Germany's, Q183, 2.3 MB).
pub const LINE_CAP: usize = 32 << 20;
/// Lines longer than this are parsed one at a time in the process: a parsed line takes many
/// times its bytes, so parsers never hold several large ones at once.
const LARGE_LINE: usize = 1 << 20;
/// The most Wikipedia articles whose links a card keys (the big editions first): a well-known
/// item has an article in a hundred editions or more, and every key costs index space; a link to a
/// smaller edition's article is fetched as any shared link is.
pub const ARTICLE_KEYS: usize = 16;
/// The most official websites a card relates.
const RELATED_SITES: usize = 3;
/// The most labels one card takes from its genres and classes, and the most values read per
/// property.
const LABEL_ITEMS: usize = 16;
/// Fame is the number of Wikipedia editions on a log scale: an item in 300 or more is 1.
const FAME_EDITIONS: f64 = 300.0;
/// The other names a card keeps from the item's aliases and labels in other languages.
const OTHER_NAMES: usize = 15;
/// Languages a name and other names come from, in this order (then any other).
const NAME_LANGUAGES: [&str; 11] = [
    "en", "mul", "de", "fr", "es", "it", "nl", "pt", "ru", "ja", "zh",
];
/// Languages a description comes from, in this order.
const DESCRIPTION_LANGUAGES: [&str; 5] = ["en", "mul", "de", "fr", "es"];

/// Wikimedia's own classes: an item that is an instance of one is a page of the wikis (a
/// category, a disambiguation page, a template, a list, a project page, …), not a thing.
pub const WIKIMEDIA_PAGES: [&str; 20] = [
    "Q4167836",  // Wikimedia category
    "Q4167410",  // Wikimedia disambiguation page
    "Q11266439", // Wikimedia template
    "Q13406463", // Wikimedia list article
    "Q115595777",
    "Q22808320", // Wikimedia human name disambiguation page
    "Q11753321", // Wikimedia navigational template
    "Q23894233",
    "Q14204246", // Wikimedia project page
    "Q15184295", // Wikimedia module
    "Q17362920", // Wikimedia duplicated page
    "Q20010800",
    "Q59541917",
    "Q30432511",
    "Q15647814", // Wikimedia administration category
    "Q24046192",
    "Q56428020",
    "Q21528878", // Wikimedia redirect page
    "Q17633526", // Wikinews article
    "Q15407973", // Wikimedia disambiguation category (`music_enrichment::NOT_AN_ENTITY`)
];
/// Classes of genre items: the label of an item that is one of these is collected for the cards
/// that name it as a genre.
pub const GENRE_CLASSES: [&str; 7] = [
    "Q483394",   // genre
    "Q188451",   // music genre
    "Q201658",   // film genre
    "Q223393",   // literary genre
    "Q659563",   // video game genre
    "Q15961987", // television genre
    "Q1792379",  // art genre
];
/// Classes that make no label: the type already says it.
const NO_LABEL: [&str; 1] = ["Q5"];

/// The schema.org type of an item by its direct class (`P31`); the first class of the item in
/// this table wins. Every type is one `model::type_path` knows (tested).
pub const TYPES: &[(&str, &str)] = &[
    ("Q5", "Person"),
    ("Q11424", "Movie"),
    ("Q24869", "Movie"),
    ("Q202866", "Movie"),
    ("Q506240", "Movie"),
    ("Q93204", "Movie"),
    ("Q29168811", "Movie"),
    ("Q24862", "Movie"),
    ("Q17517379", "Movie"),
    ("Q226730", "Movie"),
    ("Q5398426", "TVSeries"),
    ("Q581714", "TVSeries"),
    ("Q63952888", "TVSeries"),
    ("Q1259759", "TVSeries"),
    ("Q117467246", "TVSeries"),
    ("Q15416", "TVSeries"),
    ("Q526877", "TVSeries"),
    ("Q3464665", "TVSeason"),
    ("Q21191270", "TVEpisode"),
    ("Q1983062", "TVEpisode"),
    ("Q1261214", "TVEpisode"),
    ("Q482994", "MusicAlbum"),
    ("Q208569", "MusicAlbum"),
    ("Q209939", "MusicAlbum"),
    ("Q222910", "MusicAlbum"),
    ("Q169930", "MusicAlbum"),
    ("Q134556", "MusicRecording"),
    ("Q7366", "MusicRecording"),
    ("Q105543609", "MusicComposition"),
    ("Q207628", "MusicComposition"),
    ("Q215380", "MusicGroup"),
    ("Q5741069", "MusicGroup"),
    ("Q2088357", "MusicGroup"),
    ("Q571", "Book"),
    ("Q7725634", "Book"),
    ("Q47461344", "Book"),
    ("Q8261", "Book"),
    ("Q1667921", "BookSeries"),
    ("Q7889", "VideoGame"),
    ("Q11410", "Game"),
    ("Q131436", "Game"),
    ("Q16521", "Taxon"),
    ("Q13442814", "ScholarlyArticle"),
    ("Q4830453", "Organization"),
    ("Q43229", "Organization"),
    ("Q6881511", "Organization"),
    ("Q163740", "Organization"),
    ("Q7278", "Organization"),
    ("Q783794", "Corporation"),
    ("Q891723", "Corporation"),
    ("Q327333", "GovernmentOrganization"),
    ("Q4438121", "SportsOrganization"),
    ("Q476028", "SportsTeam"),
    ("Q12973014", "SportsTeam"),
    ("Q3918", "CollegeOrUniversity"),
    ("Q875538", "CollegeOrUniversity"),
    ("Q3914", "School"),
    ("Q159334", "School"),
    ("Q9842", "School"),
    ("Q11032", "Newspaper"),
    ("Q1110794", "Newspaper"),
    ("Q1002697", "Periodical"),
    ("Q5633421", "Periodical"),
    ("Q35127", "WebSite"),
    ("Q7397", "SoftwareApplication"),
    ("Q166142", "SoftwareApplication"),
    ("Q9143", "ComputerLanguage"),
    ("Q34770", "Language"),
    ("Q1288568", "Language"),
    ("Q12136", "MedicalCondition"),
    ("Q12140", "Drug"),
    ("Q11173", "ChemicalSubstance"),
    ("Q7187", "Gene"),
    ("Q8054", "Protein"),
    ("Q431289", "Brand"),
    ("Q12737077", "Occupation"),
    ("Q28640", "Occupation"),
    ("Q2424752", "Product"),
    ("Q3305213", "Painting"),
    ("Q860861", "Sculpture"),
    ("Q24634210", "PodcastSeries"),
    ("Q1656682", "Event"),
    ("Q178561", "Event"),
    ("Q40231", "Event"),
    ("Q27020041", "SportsEvent"),
    ("Q18608583", "SportsEvent"),
    ("Q13406554", "SportsEvent"),
    ("Q132241", "Festival"),
    ("Q6256", "Country"),
    ("Q3624078", "Country"),
    ("Q35657", "State"),
    ("Q107390", "State"),
    ("Q515", "City"),
    ("Q1549591", "City"),
    ("Q5119", "City"),
    ("Q3957", "City"),
    ("Q532", "City"),
    ("Q486972", "City"),
    ("Q1093829", "City"),
    ("Q15284", "AdministrativeArea"),
    ("Q747074", "AdministrativeArea"),
    ("Q484170", "AdministrativeArea"),
    ("Q262166", "AdministrativeArea"),
    ("Q70208", "AdministrativeArea"),
    ("Q3184121", "AdministrativeArea"),
    ("Q2039348", "AdministrativeArea"),
    ("Q8502", "Mountain"),
    ("Q54050", "Mountain"),
    ("Q8072", "Volcano"),
    ("Q4022", "RiverBodyOfWater"),
    ("Q23397", "LakeBodyOfWater"),
    ("Q165", "SeaBodyOfWater"),
    ("Q9430", "OceanBodyOfWater"),
    ("Q23442", "Landform"),
    ("Q33506", "Museum"),
    ("Q207694", "Museum"),
    ("Q16970", "Church"),
    ("Q2977", "Church"),
    ("Q32815", "Mosque"),
    ("Q34627", "Synagogue"),
    ("Q44613", "PlaceOfWorship"),
    ("Q44539", "PlaceOfWorship"),
    ("Q23413", "LandmarksOrHistoricalBuildings"),
    ("Q16560", "LandmarksOrHistoricalBuildings"),
    ("Q4989906", "LandmarksOrHistoricalBuildings"),
    ("Q1248784", "Airport"),
    ("Q55488", "TrainStation"),
    ("Q483110", "StadiumOrArena"),
    ("Q22698", "Park"),
    ("Q46169", "Park"),
    ("Q12280", "Bridge"),
    ("Q39614", "Cemetery"),
    ("Q16917", "Hospital"),
    ("Q7075", "Library"),
    ("Q27686", "Hotel"),
    ("Q11707", "Restaurant"),
    ("Q43501", "Zoo"),
    ("Q40080", "Beach"),
];

/// The site names of wikis that are not a Wikipedia edition though their name ends in `wiki`.
const NOT_WIKIPEDIA: [&str; 17] = [
    "commonswiki",
    "specieswiki",
    "metawiki",
    "mediawikiwiki",
    "wikidatawiki",
    "sourceswiki",
    "outreachwiki",
    "wikimaniawiki",
    "incubatorwiki",
    "foundationwiki",
    "wikifunctionswiki",
    "abstractwiki",
    "testwiki",
    "testwikidatawiki",
    "strategywiki",
    "officewiki",
    "betawikiversitywiki",
];

/// A Wikipedia article an item has: the edition's language code, as its host names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Article {
    pub site: String,
    pub lang: String,
    /// The canonical link (`wikipedia::parse`).
    pub uri: String,
}

/// The edition a sitelink's site name (`enwiki`, `zh_yuewiki`) is, if it is a Wikipedia.
pub fn wikipedia_lang(site: &str) -> Option<String> {
    if NOT_WIKIPEDIA.contains(&site) {
        return None;
    }
    let code = site.strip_suffix("wiki")?;
    if !(2..=16).contains(&code.len())
        || !code.starts_with(|c: char| c.is_ascii_lowercase())
        || !code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || code.split('_').any(|part| part.is_empty())
    {
        return None;
    }
    // The one edition whose host is not its site name with `-` for `_`.
    Some(match code {
        "be_x_old" => "be-tarask".to_owned(),
        code => code.replace('_', "-"),
    })
}

/// The item's Wikipedia articles, in the order cards rank editions: English, then
/// `wikipedia::LANGUAGES`, then by site name.
pub fn wikipedia_articles(entity: &Value) -> Vec<Article> {
    let mut articles: Vec<Article> = entity["sitelinks"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(site, link)| {
            let lang = wikipedia_lang(site)?;
            let title = link["title"].as_str()?;
            if title.is_empty() || title.len() > 512 {
                return None;
            }
            let encoded = percent_encoding::utf8_percent_encode(
                &title.replace(' ', "_"),
                percent_encoding::NON_ALPHANUMERIC,
            )
            .to_string();
            let article =
                crate::wikipedia::parse(&format!("https://{lang}.wikipedia.org/wiki/{encoded}"))?;
            // Only a link that reads back as itself: a title whose canonical form a URL parser
            // reads otherwise (`..\`, where `\` is `/` in a URL) keys nothing it could be shared as.
            crate::wikipedia::parse(&article.canonical)
                .filter(|again| again.canonical == article.canonical)?;
            Some(Article {
                site: site.clone(),
                lang,
                uri: article.canonical,
            })
        })
        .collect();
    let rank = |a: &Article| {
        crate::wikipedia::LANGUAGES
            .iter()
            .position(|l| *l == a.lang)
            .unwrap_or(crate::wikipedia::LANGUAGES.len())
    };
    articles.sort_by(|a, b| (rank(a), &a.site).cmp(&(rank(b), &b.site)));
    articles
}

/// The best-rank values of a property (preferred if any, else normal; never deprecated), at
/// most `cap`.
fn best<'a>(entity: &'a Value, property: &str, cap: usize) -> Vec<&'a Value> {
    let claims = entity["claims"][property]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let ranked = |rank: &str| -> Vec<&'a Value> {
        claims
            .iter()
            .filter(|c| c["rank"] == rank && c["mainsnak"]["snaktype"] == "value")
            .map(|c| &c["mainsnak"]["datavalue"]["value"])
            .take(cap)
            .collect()
    };
    let preferred = ranked("preferred");
    if preferred.is_empty() {
        ranked("normal")
    } else {
        preferred
    }
}
/// The items a property names at its best rank, in order, each once.
fn best_items(entity: &Value, property: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    best(entity, property, LABEL_ITEMS * 4)
        .into_iter()
        .filter_map(|v| hub_qid(v["id"].as_str()?))
        .filter(|q| seen.insert(q.clone()))
        .collect()
}
fn hub_qid(s: &str) -> Option<String> {
    crate::hubs::qid(s)
}
/// The one valid best-rank text value of a property: none when there are none or several
/// different ones.
fn one_text(
    entity: &Value,
    property: &str,
    valid: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let values: BTreeSet<String> = best(entity, property, 64)
        .into_iter()
        .filter_map(|v| valid(v.as_str()?))
        .collect();
    match values.len() {
        1 => values.into_iter().next(),
        _ => None,
    }
}
/// A positive decimal number without leading zeros, at most 20 digits.
fn digits(v: &str) -> Option<String> {
    (!v.is_empty() && v.len() <= 20 && !v.starts_with('0') && v.bytes().all(|b| b.is_ascii_digit()))
        .then(|| v.to_owned())
}
/// An IMDb ID: `tt` (a title) or `nm` (a person) and 7 to 10 digits.
fn imdb(v: &str) -> Option<String> {
    let digits = v.strip_prefix("tt").or_else(|| v.strip_prefix("nm"))?;
    ((7..=10).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| v.to_owned())
}

/// A text value of a language map (`labels`, `descriptions`) in the first of `langs` it has.
fn text_in<'a>(map: &'a Value, langs: &[&str]) -> Option<&'a str> {
    langs
        .iter()
        .find_map(|l| map[*l]["value"].as_str().filter(|v| !v.trim().is_empty()))
}

/// What one entity line gives.
#[derive(Debug, Default)]
pub struct Parsed {
    /// The card, when the entity is a kept item.
    pub card: Option<Card>,
    /// The items its labels come from, in order (genres, then classes).
    pub label_items: Vec<u32>,
    /// The entity's own label, when it is a class or a genre others may name: `None` inside is
    /// an item whose label makes no tag label.
    pub label: Option<(u32, Option<Box<str>>)>,
    /// What kind of line it was.
    pub kind: Kind,
}
/// What kind of entity line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// An item kept as a card.
    Kept,
    /// An item with no Wikipedia article (or no line worth parsing).
    #[default]
    NoArticle,
    /// An item that is one of Wikimedia's own pages.
    WikimediaPage,
    /// An item with an article but no card (no name, a link Pinakes can't use).
    Unusable,
    /// Not an item (a property, a lexeme) or not one entity (`[`, `]`).
    Other,
    /// Not JSON, or larger than [`LINE_CAP`].
    Bad,
}

/// The number of a Q-id (`Q42` -> 42).
pub fn qid_number(qid: &str) -> Option<u32> {
    let digits = qid.strip_prefix('Q')?;
    if digits.is_empty() || digits.len() > 10 || digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

/// Labels of items, as tag labels; an item mapped to `None` has none (its label is too long, or
/// it has no English one).
pub type Labels = HashMap<u32, Option<Box<str>>>;

/// Whether a dump line can be skipped without parsing: it names no site (so has no sitelink),
/// is not a subclass, and names no genre class. A line it can't rule out is parsed.
pub fn worth_parsing(line: &[u8]) -> bool {
    use memchr::memmem::Finder;
    static FINDERS: std::sync::LazyLock<Vec<Finder<'static>>> = std::sync::LazyLock::new(|| {
        ["\"site\"".to_owned(), "\"P279\"".to_owned()]
            .into_iter()
            .chain(GENRE_CLASSES.iter().map(|q| format!("\"{q}\"")))
            .map(|needle| Finder::new(needle.as_bytes()).into_owned())
            .collect()
    });
    FINDERS.iter().any(|f| f.find(line).is_some())
}

/// One dump line (with or without its trailing comma) as what it gives; `labels` resolves the
/// labels known so far.
pub fn parse_line(line: &[u8], labels: &Labels) -> Parsed {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = line.strip_suffix(b",").unwrap_or(line);
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[" || trimmed == b"]" {
        return Parsed {
            kind: Kind::Other,
            ..Parsed::default()
        };
    }
    if trimmed.len() > LINE_CAP {
        return Parsed {
            kind: Kind::Bad,
            ..Parsed::default()
        };
    }
    if !worth_parsing(trimmed) {
        return Parsed::default();
    }
    static LARGE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time =
        (trimmed.len() > LARGE_LINE).then(|| LARGE.lock().unwrap_or_else(|e| e.into_inner()));
    match serde_json::from_slice::<Value>(trimmed) {
        Ok(entity) => parse_entity(entity, labels),
        Err(_) => Parsed {
            kind: Kind::Bad,
            ..Parsed::default()
        },
    }
}

/// One entity as what it gives.
pub fn parse_entity(entity: Value, labels: &Labels) -> Parsed {
    let mut parsed = Parsed::default();
    if entity["type"] != "item" {
        parsed.kind = Kind::Other;
        return parsed;
    }
    let Some(qid) = entity["id"].as_str().and_then(hub_qid) else {
        parsed.kind = Kind::Other;
        return parsed;
    };
    let Some(number) = qid_number(&qid) else {
        parsed.kind = Kind::Other;
        return parsed;
    };
    let classes = best_items(&entity, "P31");
    // A class or a genre: its label is kept for the cards that name it.
    let genre = classes.iter().any(|c| GENRE_CLASSES.contains(&c.as_str()));
    if genre || !best(&entity, "P279", 1).is_empty() {
        let label = text_in(&entity["labels"], &["en", "mul"])
            .and_then(crate::catalogue::label)
            .map(String::into_boxed_str);
        parsed.label = Some((number, label));
    }
    let articles = wikipedia_articles(&entity);
    if articles.is_empty() {
        parsed.kind = Kind::NoArticle;
        return parsed;
    }
    if classes
        .iter()
        .any(|c| WIKIMEDIA_PAGES.contains(&c.as_str()))
    {
        parsed.kind = Kind::WikimediaPage;
        return parsed;
    }
    match card(entity, &qid, &classes, &articles) {
        Some((card, label_items)) => {
            parsed.card = Some(resolve(card, &label_items, labels).0);
            parsed.label_items = label_items;
            parsed.kind = Kind::Kept;
        }
        None => parsed.kind = Kind::Unusable,
    }
    parsed
}

/// The card with its labels from `items` as far as `labels` knows them; whether every item was
/// known.
pub fn resolve(mut card: Card, items: &[u32], labels: &Labels) -> (Card, bool) {
    let mut complete = true;
    let mut raw: Vec<&str> = Vec::new();
    for item in items {
        match labels.get(item) {
            Some(Some(label)) => raw.push(label),
            Some(None) => {}
            None => complete = false,
        }
    }
    card.labels = crate::catalogue::labels(raw)
        .into_iter()
        .take(LABEL_ITEMS)
        .collect();
    (card, complete)
}

/// The music level of an item by its identifiers (`music_enrichment::ItemKind`).
fn music_level(item: &hub::Item) -> Option<crate::music::Level> {
    use crate::music::Level;
    use crate::music_enrichment::ItemKind;
    match ItemKind::of(item) {
        ItemKind::Artist => Some(Level::Artist),
        ItemKind::ReleaseGroup | ItemKind::WorkAndGroup => Some(Level::Album),
        ItemKind::Recording => Some(Level::Track),
        _ => None,
    }
}

/// The item as `hubs::wikidata` reads it (its best-rank identifiers), for the tables other
/// sources share.
fn hub_item(entity: Value, qid: &str) -> (hub::Item, Value) {
    let mut wrapped = serde_json::json!({ "entities": {} });
    wrapped["entities"][qid] = entity;
    let item = hub::parse_entities(&wrapped).pop().unwrap_or_default();
    let entity = wrapped["entities"][qid].take();
    (item, entity)
}

/// The card of a kept item and the items its labels come from.
fn card(
    entity: Value,
    qid: &str,
    classes: &[String],
    articles: &[Article],
) -> Option<(Card, Vec<u32>)> {
    let (item, entity) = hub_item(entity, qid);
    // English first, then the biggest editions: `wikipedia_articles` ranks them so.
    let canonical = &articles[0];
    let labels = &entity["labels"];
    let name = text_in(labels, &NAME_LANGUAGES)
        .map(str::to_owned)
        .or_else(|| {
            labels
                .as_object()
                .into_iter()
                .flatten()
                .find_map(|(_, v)| v["value"].as_str().filter(|v| !v.trim().is_empty()))
                .map(str::to_owned)
        })
        .or_else(|| crate::wikipedia::parse(&canonical.uri).map(|a| a.title.replace('_', " ")))?;
    // Other names: English aliases, then the labels of the big languages, then the rest.
    let mut seen = BTreeSet::from([name.to_lowercase()]);
    let mut other_names = Vec::new();
    let aliases = entity["aliases"]["en"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["value"].as_str());
    let in_languages = NAME_LANGUAGES
        .iter()
        .filter_map(|l| labels[*l]["value"].as_str());
    let others = labels
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, v)| v["value"].as_str());
    for text in aliases.chain(in_languages).chain(others) {
        if other_names.len() >= OTHER_NAMES {
            break;
        }
        if !text.trim().is_empty() && seen.insert(text.to_lowercase()) {
            other_names.push(text.to_owned());
        }
    }
    let description = text_in(&entity["descriptions"], &DESCRIPTION_LANGUAGES).map(str::to_owned);
    let level = music_level(&item);
    let r#type = item_type(classes, level, point(&entity).is_some());
    let mut keys: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    if let Some(key) = crate::hubs::key::wikidata(qid) {
        keys.push(key);
    }
    // Music at the item's own level: its MusicBrainz key and its service pages.
    if let Some(level) = level {
        let found = crate::music_sources::found(&item, level);
        keys.extend(found.keys.into_iter().map(|k| k.key));
        links.extend(found.links.into_iter().map(|l| l.uri));
    }
    for (property, kind) in [("P402", "relation"), ("P10689", "way"), ("P11693", "node")] {
        if let Some(id) = one_text(&entity, property, digits) {
            keys.push(format!("osm:{kind}/{id}"));
        }
    }
    match one_text(&entity, "P345", imdb) {
        Some(tt) if tt.starts_with("tt") => keys.push(format!("imdb:{tt}")),
        Some(nm) => links.push(format!("https://www.imdb.com/name/{nm}/")),
        None => {}
    }
    let isbns: BTreeSet<String> = ["P212", "P957"]
        .iter()
        .filter_map(|p| {
            one_text(&entity, p, |v| {
                crate::catalogue::stored_key(&format!("isbn:{v}"))
            })
        })
        .collect();
    if let [isbn] = isbns.into_iter().collect::<Vec<_>>().as_slice() {
        keys.push(isbn.clone());
    }
    if let Some(doi) = one_text(&entity, "P356", |v| {
        crate::catalogue::stored_key(&format!("doi:{v}"))
    }) {
        keys.push(doi);
    }
    // Pages of the item on other services, by its identifiers.
    use crate::lookups::wikidata::{self as lookup, APP, GAME, PLACE};
    for table in [PLACE, APP, GAME] {
        links.extend(
            lookup::links(&entity, table)
                .into_iter()
                .filter(|l| l.site != "Official website")
                .map(|l| l.uri),
        );
    }
    let path = crate::model::type_path(&r#type);
    let is = |t: &str| path.iter().any(|p| p == t);
    // A film or television item by its type, or by an IMDb title or TMDB id whatever its class
    // (one the type table does not list): its film pages key it.
    let film_ids = best(&entity, "P345", 8)
        .iter()
        .any(|v| v.as_str().is_some_and(|t| t.starts_with("tt")))
        || ["P4947", "P4983"]
            .iter()
            .any(|p| !best(&entity, p, 1).is_empty());
    // The table of the item's own kind, and the films table beside it for an item with an IMDb
    // title or a TMDB id (a book or a podcast keeps its own pages too).
    let film_type = is("Movie") || is("TVSeries") || is("TVSeason") || is("TVEpisode");
    let mut media_tables: Vec<&[crate::also_on::media::Property]> = Vec::new();
    if is("Book") || is("BookSeries") {
        media_tables.push(crate::also_on::books::WIKIDATA);
    } else if is("PodcastSeries") {
        media_tables.push(crate::also_on::podcasts::WIKIDATA);
    }
    if film_type || film_ids {
        media_tables.push(crate::also_on::films::WIKIDATA);
    }
    for table in &media_tables {
        links.extend(
            crate::also_on::media::links(&item, table)
                .into_iter()
                .map(|l| l.uri),
        );
    }
    // The articles first: a card keeps at most `KEYS_CAP` keys, and its editions' articles are
    // the links shared most.
    for article in articles.iter().take(ARTICLE_KEYS) {
        keys.extend(crate::catalogue::link_key(&article.uri));
    }
    // A service page joins the card by its link, and by the keys it names.
    let mut link_set = BTreeSet::new();
    for link in links {
        if !link_set.insert(link.clone()) {
            continue;
        }
        keys.extend(
            crate::catalogue::join::link_keys(&link)
                .into_iter()
                .map(|(key, _)| key),
        );
        keys.extend(crate::catalogue::link_key(&link));
    }
    let related_uris: Vec<String> = best(&entity, "P856", RELATED_SITES * 2)
        .into_iter()
        .filter_map(Value::as_str)
        .filter(|u| {
            url::Url::parse(u)
                .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        })
        .take(RELATED_SITES)
        .map(str::to_owned)
        .collect();
    let editions = articles.len() as f64;
    let fame = ((1.0 + editions).ln() / (1.0 + FAME_EDITIONS).ln()).min(1.0) as f32;
    let genres = best_items(&entity, "P136");
    let label_items: Vec<u32> = genres
        .iter()
        .chain(classes.iter().filter(|c| !NO_LABEL.contains(&c.as_str())))
        .filter_map(|q| qid_number(q))
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let label_items = label_items
        .into_iter()
        .filter(|q| seen.insert(*q))
        .take(LABEL_ITEMS)
        .collect();
    Some((
        Card {
            source: String::new(),
            source_id: qid.to_owned(),
            uri: canonical.uri.clone(),
            r#type,
            name,
            other_names,
            description,
            labels: Vec::new(),
            keys,
            related_uris,
            point: point(&entity),
            fame,
            details: serde_json::Map::new(),
        },
        label_items,
    ))
}

/// The item's coordinate on Earth (P625 with globe Q2), the first at the best rank.
fn point(entity: &Value) -> Option<crate::model::Point> {
    best(entity, "P625", 8).into_iter().find_map(|v| {
        let globe = v["globe"]
            .as_str()
            .unwrap_or("http://www.wikidata.org/entity/Q2");
        if !globe.ends_with("/entity/Q2") {
            return None;
        }
        let (lat, lon) = (v["latitude"].as_f64()?, v["longitude"].as_f64()?);
        ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
            .then_some(crate::model::Point { lat, lon })
    })
}

/// The item's type: its first class in [`TYPES`], else music by its level, else `Place` with a
/// point, else `Thing`.
pub fn item_type(classes: &[String], level: Option<crate::music::Level>, point: bool) -> String {
    use crate::music::Level;
    if let Some(t) = classes
        .iter()
        .find_map(|c| TYPES.iter().find(|(q, _)| q == c).map(|(_, t)| *t))
    {
        return t.to_owned();
    }
    match level {
        Some(Level::Artist) => "MusicGroup",
        Some(Level::Album) => "MusicAlbum",
        Some(Level::Track) => "MusicRecording",
        _ if point => "Place",
        _ => "Thing",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The fixture's entity lines (`tests/fixtures/wikidata`), by Q-id.
    fn fixture() -> std::collections::BTreeMap<String, Vec<u8>> {
        let packed = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wikidata/dump-sample.json.bz2"
        ))
        .unwrap();
        let mut text = Vec::new();
        std::io::Read::read_to_end(&mut bzip2::read::BzDecoder::new(&packed[..]), &mut text)
            .unwrap();
        text.split(|b| *b == b'\n')
            .filter(|l| l.starts_with(b"{"))
            .map(|l| {
                let v: Value = serde_json::from_slice(l.strip_suffix(b",").unwrap_or(l)).unwrap();
                (v["id"].as_str().unwrap().to_owned(), l.to_vec())
            })
            .collect()
    }
    fn claim(value: Value) -> Value {
        serde_json::json!({"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": value}}})
    }
    fn entity(claims: Value) -> Value {
        serde_json::json!({"type": "item", "id": "Q7", "labels": {"en": {"value": "Seven"}},
            "claims": claims, "sitelinks": {"enwiki": {"site": "enwiki", "title": "Seven"}}})
    }

    /// Every type of the table is one Pinakes knows, each class is listed once, and Wikimedia's
    /// own classes and the genre classes are Q-ids.
    #[test]
    fn the_type_table_names_known_types() {
        let mut seen = BTreeSet::new();
        for (class, t) in TYPES {
            assert!(!crate::model::type_path(t).is_empty(), "{t}");
            assert!(
                crate::hubs::qid(class).is_some() && seen.insert(*class),
                "{class}"
            );
        }
        for q in WIKIMEDIA_PAGES.iter().chain(&GENRE_CLASSES) {
            assert!(crate::hubs::qid(q).is_some(), "{q}");
        }
    }

    #[test]
    fn wikipedia_editions_by_site_name() {
        for (site, lang) in [
            ("enwiki", Some("en")),
            ("zh_yuewiki", Some("zh-yue")),
            ("be_x_oldwiki", Some("be-tarask")),
            ("simplewiki", Some("simple")),
            ("commonswiki", None),
            ("wikidatawiki", None),
            ("enwikiquote", None),
            ("enwikisource", None),
            ("Enwiki", None),
            ("wiki", None),
            ("e_wiki", None),
        ] {
            assert_eq!(wikipedia_lang(site).as_deref(), lang, "{site}");
        }
    }

    /// The English article is the link; without one, the biggest edition the item has.
    #[test]
    fn the_canonical_link_is_english_else_the_biggest_edition() {
        let labels = Labels::new();
        let card = |sitelinks: Value| {
            let mut e = entity(serde_json::json!({}));
            e["sitelinks"] = sitelinks;
            parse_entity(e, &labels).card.map(|c| c.uri)
        };
        assert_eq!(
            card(serde_json::json!({"dewiki": {"site": "dewiki", "title": "Sieben"}, "enwiki": {"site": "enwiki", "title": "Seven (number)"}})).as_deref(),
            Some("https://en.wikipedia.org/wiki/Seven_(number)")
        );
        assert_eq!(
            card(serde_json::json!({"cebwiki": {"site": "cebwiki", "title": "A"}, "frwiki": {"site": "frwiki", "title": "Sept ?"}, "dewiki": {"site": "dewiki", "title": "Sieben"}})).as_deref(),
            Some("https://de.wikipedia.org/wiki/Sieben")
        );
        assert_eq!(
            card(serde_json::json!({"cebwiki": {"site": "cebwiki", "title": "A"}, "alswiki": {"site": "alswiki", "title": "B"}})).as_deref(),
            Some("https://als.wikipedia.org/wiki/B")
        );
        assert_eq!(
            card(serde_json::json!({"frwiki": {"site": "frwiki", "title": "Sept ?"}})).as_deref(),
            Some("https://fr.wikipedia.org/wiki/Sept_%3F")
        );
        assert_eq!(
            card(serde_json::json!({"commonswiki": {"site": "commonswiki", "title": "x"}})),
            None
        );
    }

    /// A key comes only from a property with one best-rank valid value: two values (or two
    /// spellings) are no key, a deprecated one doesn't count, a preferred one wins.
    #[test]
    fn keys_come_from_one_best_rank_value() {
        let labels = Labels::new();
        let keys = |claims: Value| parse_entity(entity(claims), &labels).card.unwrap().keys;
        let one = keys(serde_json::json!({"P402": [claim("1682248".into())]}));
        assert!(one.contains(&"osm:relation/1682248".to_owned()), "{one:?}");
        let two = keys(serde_json::json!({"P402": [claim("1".into()), claim("2".into())]}));
        assert!(!two.iter().any(|k| k.starts_with("osm:")), "{two:?}");
        let mut deprecated = claim("2".into());
        deprecated["rank"] = "deprecated".into();
        let mut preferred = claim("3".into());
        preferred["rank"] = "preferred".into();
        let ranked = keys(
            serde_json::json!({"P402": [claim("1".into()), deprecated.clone()], "P10689": [claim("5".into()), preferred]}),
        );
        assert!(
            ranked.contains(&"osm:relation/1".to_owned())
                && ranked.contains(&"osm:way/3".to_owned()),
            "{ranked:?}"
        );
        let bad = keys(
            serde_json::json!({"P402": [claim("01".into())], "P345": [claim("tt12".into())], "P356": [claim("not a doi".into())], "P212": [claim("123".into())]}),
        );
        assert_eq!(
            bad.iter()
                .filter(|k| !k.starts_with("url:"))
                .collect::<Vec<_>>(),
            ["wikidata:Q7"]
        );
        let isbn = keys(
            serde_json::json!({"P212": [claim("978-0-14-032872-1".into())], "P957": [claim("0-14-032872-6".into())], "P356": [claim("10.1038/Nature12373".into())]}),
        );
        assert!(
            isbn.contains(&"isbn:9780140328721".to_owned())
                && isbn.contains(&"doi:10.1038/nature12373".to_owned()),
            "{isbn:?}"
        );
        let other = keys(
            serde_json::json!({"P212": [claim("978-0-14-032872-1".into())], "P957": [claim("0-306-40615-2".into())]}),
        );
        assert!(
            !other.iter().any(|k| k.starts_with("isbn:")),
            "two books' ISBNs: {other:?}"
        );
    }

    /// Labels: genres then classes, each as a tag label, the human class left out; an item not
    /// known yet makes the card incomplete.
    #[test]
    fn labels_come_from_genres_and_classes() {
        let mut labels = Labels::new();
        labels.insert(11399, Some("rock-music".into()));
        labels.insert(482994, Some("album".into()));
        labels.insert(9, None);
        let e = entity(serde_json::json!({
            "P136": [claim(serde_json::json!({"id": "Q11399"})), claim(serde_json::json!({"id": "Q9"}))],
            "P31": [claim(serde_json::json!({"id": "Q5"})), claim(serde_json::json!({"id": "Q482994"})), claim(serde_json::json!({"id": "Q404"}))]
        }));
        let parsed = parse_entity(e, &labels);
        assert_eq!(parsed.label_items, [11399, 9, 482994, 404]);
        let card = parsed.card.unwrap();
        assert_eq!(card.labels, ["rock-music", "album"]);
        assert_eq!(card.r#type, "Person");
        let (_, complete) = resolve(card.clone(), &parsed.label_items, &labels);
        assert!(!complete);
        labels.insert(404, Some("thing".into()));
        let (card, complete) = resolve(card, &parsed.label_items, &labels);
        assert!(complete);
        assert_eq!(card.labels, ["rock-music", "album", "thing"]);
    }

    /// A class or a genre gives its own label for the cards naming it, also without an article.
    #[test]
    fn classes_and_genres_give_their_labels() {
        let labels = Labels::new();
        let mut genre =
            entity(serde_json::json!({"P31": [claim(serde_json::json!({"id": "Q188451"}))]}));
        genre["id"] = "Q11399".into();
        genre["labels"] = serde_json::json!({"en": {"value": "Rock Music"}});
        genre["sitelinks"] = serde_json::json!({});
        let parsed = parse_entity(genre, &labels);
        assert_eq!(parsed.label, Some((11399, Some("rock-music".into()))));
        assert_eq!(parsed.kind, Kind::NoArticle);
        let mut class =
            entity(serde_json::json!({"P279": [claim(serde_json::json!({"id": "Q1"}))]}));
        class["labels"] =
            serde_json::json!({"mul": {"value": "a label far longer than twenty characters"}});
        assert_eq!(parse_entity(class, &labels).label, Some((7, None)));
        assert_eq!(
            parse_entity(entity(serde_json::json!({})), &labels).label,
            None
        );
    }

    /// The real entities of the fixture: what each kind of line gives.
    /// An item with only a TMDB movie id (no IMDb title, a class the type table doesn't list) is
    /// keyed by its TMDB page, so a shared TMDB link joins it; its articles' keys come before its
    /// service pages' (a card keeps at most `KEYS_CAP` keys).
    #[test]
    fn a_tmdb_id_keys_the_tmdb_page_after_the_articles() {
        let labels = Labels::new();
        let id = |v: &str| {
            serde_json::json!({"rank": "normal", "mainsnak": {"snaktype": "value",
                "datatype": "external-id", "datavalue": {"value": v, "type": "string"}}})
        };
        let mut e = entity(serde_json::json!({
            "P31": [claim(serde_json::json!({"id": "Q99999999"}))],
            "P4947": [id("603")]
        }));
        e["sitelinks"]["dewiki"] = serde_json::json!({"site": "dewiki", "title": "Sieben"});
        let card = parse_entity(e.clone(), &labels).card.unwrap();
        let tmdb = crate::catalogue::link_key("https://www.themoviedb.org/movie/603").unwrap();
        // A TV id alike: its TMDB series page.
        let mut tv = e;
        tv["claims"] = serde_json::json!({
            "P31": [claim(serde_json::json!({"id": "Q99999999"}))],
            "P4983": [id("1396")]
        });
        let tv = parse_entity(tv, &labels).card.unwrap();
        let series = crate::catalogue::link_key("https://www.themoviedb.org/tv/1396").unwrap();
        assert!(tv.keys.contains(&series), "{:?}", tv.keys);
        let at = |k: &String| card.keys.iter().position(|x| x == k).unwrap();
        assert!(card.keys.contains(&tmdb), "{:?}", card.keys);
        for article in [
            "https://en.wikipedia.org/wiki/Seven",
            "https://de.wikipedia.org/wiki/Sieben",
        ] {
            let key = crate::catalogue::link_key(article).unwrap();
            assert!(at(&key) < at(&tmdb), "{:?}", card.keys);
        }
    }

    /// A podcast (or a book) with an IMDb title keeps its own service pages as keys beside its
    /// IMDb page.
    #[test]
    fn a_podcast_with_an_imdb_title_keeps_its_podcast_pages() {
        let labels = Labels::new();
        // Identifiers as the dump has them: external ids.
        let id = |v: &str| {
            serde_json::json!({"rank": "normal", "mainsnak": {"snaktype": "value",
                "datatype": "external-id", "datavalue": {"value": v, "type": "string"}}})
        };
        let e = entity(serde_json::json!({
            "P31": [claim(serde_json::json!({"id": "Q24634210"}))],
            "P5916": [id("0123456789abcdefABCDEF")],
            "P345": [id("tt0000001")]
        }));
        let card = parse_entity(e, &labels).card.unwrap();
        assert_eq!(card.r#type, "PodcastSeries");
        for page in [
            "https://open.spotify.com/show/0123456789abcdefABCDEF",
            "https://www.imdb.com/title/tt0000001/",
        ] {
            let key = crate::catalogue::link_key(page).unwrap();
            assert!(card.keys.contains(&key), "{page} {:?}", card.keys);
        }
    }

    /// Real film and television items of the classes the type table lists
    /// (`fixtures/wikidata/films.json`, read from the API) take their type, and their IMDb title
    /// page keys the card, so a shared IMDb link joins it; an item of a class the table doesn't
    /// list is keyed by its IMDb page all the same.
    #[test]
    fn real_films_and_series_are_typed_and_keyed_by_their_imdb_page() {
        let films: std::collections::BTreeMap<String, Value> = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/wikidata/films.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let labels = Labels::new();
        let card_of = |entity: &Value| {
            parse_line(&serde_json::to_vec(entity).unwrap(), &labels)
                .card
                .unwrap()
        };
        // The title's IMDb page; none for an id that names a part of a title (a season's
        // `tt…/episodes/?season=3`).
        let imdb_key = |entity: &Value| {
            let id = entity["claims"]["P345"][0]["mainsnak"]["datavalue"]["value"]
                .as_str()
                .unwrap()
                .to_owned();
            (id.len() > 2 && id[2..].bytes().all(|b| b.is_ascii_digit())).then(|| {
                crate::catalogue::link_key(&format!("https://www.imdb.com/title/{id}/")).unwrap()
            })
        };
        for (class, r#type) in [
            ("Q29168811", "Movie"),
            ("Q24862", "Movie"),
            ("Q17517379", "Movie"),
            ("Q226730", "Movie"),
            ("Q1259759", "TVSeries"),
            ("Q117467246", "TVSeries"),
            ("Q15416", "TVSeries"),
            ("Q526877", "TVSeries"),
            ("Q3464665", "TVSeason"),
        ] {
            let entity = &films[class];
            let card = card_of(entity);
            assert_eq!(card.r#type, r#type, "{class}");
            if let Some(key) = imdb_key(entity) {
                assert!(card.keys.contains(&key), "{class} {:?}", card.keys);
            }
        }
        let mut other = films["Q29168811"].clone();
        other["claims"]["P31"] = serde_json::json!([claim(serde_json::json!({"id": "Q99999999"}))]);
        let card = card_of(&other);
        assert!(!crate::model::type_path(&card.r#type)
            .iter()
            .any(|t| t == "Movie"));
        assert!(
            card.keys.contains(&imdb_key(&other).unwrap()),
            "{:?}",
            card.keys
        );
    }

    #[test]
    fn real_entities_become_the_cards_they_should() {
        let lines = fixture();
        let labels = Labels::new();
        let parse = |id: &str| parse_line(&lines[id], &labels);
        assert_eq!(parse("Q5063").kind, Kind::NoArticle);
        assert_eq!(parse("Q2041").kind, Kind::WikimediaPage);
        assert_eq!(parse("Q3263").kind, Kind::WikimediaPage);
        assert_eq!(parse("Q4829").kind, Kind::WikimediaPage);
        assert_eq!(parse("P136").kind, Kind::Other);
        let adams = parse("Q42").card.unwrap();
        assert_eq!(adams.uri, "https://en.wikipedia.org/wiki/Douglas_Adams");
        assert!(adams.other_names.len() <= 15 && !adams.other_names.is_empty());
        assert_eq!(
            adams.description.as_deref().map(|d| d.contains("writer")),
            Some(true)
        );
        let row = adams
            .checked(crate::catalogue::Catalogue::Wikidata)
            .unwrap();
        assert!(row.keys.len() <= crate::catalogue::KEYS_CAP);
        // The ARTICLE_KEYS biggest editions are keyed.
        let articles = row.keys.iter().filter(|k| k.starts_with("url:")).count();
        assert!(articles >= ARTICLE_KEYS, "{articles}");
        let app = parse("Q171477").card.unwrap();
        assert!(
            app.keys.contains(&"android:org.videolan.vlc".to_owned())
                || app.keys.iter().any(|k| k.starts_with("url:")),
            "{:?}",
            app.keys
        );
        for (id, line) in &lines {
            let parsed = parse_line(line, &labels);
            if let Some(card) = parsed.card {
                assert!(
                    card.checked(crate::catalogue::Catalogue::Wikidata).is_ok(),
                    "{id}"
                );
                assert_eq!(card.source_id, *id);
            }
        }
    }

    #[test]
    fn lines_without_a_site_a_subclass_or_a_genre_are_not_parsed() {
        assert!(!worth_parsing(
            br#"{"type":"item","id":"Q1","sitelinks":{}}"#
        ));
        assert!(worth_parsing(
            br#"{"sitelinks":{"enwiki":{"site":"enwiki","title":"A"}}}"#
        ));
        assert!(worth_parsing(br#"{"claims":{"P279":[]}}"#));
        assert!(worth_parsing(br#"{"x":{"id":"Q188451"}}"#));
        let labels = Labels::new();
        for line in [&b"["[..], b"]", b"", b"  ", b"\n"] {
            assert_eq!(parse_line(line, &labels).kind, Kind::Other);
        }
        assert_eq!(parse_line(br#"{"site" nope"#, &labels).kind, Kind::Bad);
        assert_eq!(parse_line(br#"{"type":"item","id":"Q0","sitelinks":{"enwiki":{"site":"enwiki","title":"A"}}},"#, &labels).kind, Kind::Other);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

        /// Fuzz: any bytes, and any real line cut or with bytes changed, parse to a kind without
        /// a panic, and a card made is one the table stores.
        #[test]
        fn any_line_parses_without_a_panic(
            noise in proptest::collection::vec(any::<u8>(), 0..512),
            pick in 0usize..42,
            cut in 0.0f64..1.0,
            flips in proptest::collection::vec((0.0f64..1.0, any::<u8>()), 0..8),
        ) {
            let labels = Labels::new();
            let _ = parse_line(&noise, &labels);
            let lines: Vec<Vec<u8>> = FIXTURE.values().cloned().collect();
            let mut line = lines[pick % lines.len()].clone();
            line.truncate(((line.len() as f64) * cut) as usize);
            for (at, b) in flips {
                if !line.is_empty() {
                    let i = ((line.len() - 1) as f64 * at) as usize;
                    line[i] = b;
                }
            }
            if let Some(card) = parse_line(&line, &labels).card {
                prop_assert!(card.checked(crate::catalogue::Catalogue::Wikidata).is_ok());
            }
        }

        /// Any title of an article makes a canonical link that `wikipedia::parse` reads back
        /// as itself, whose link key is the card's own.
        #[test]
        fn article_links_are_their_own_canonical_form(
            // MediaWiki titles never hold these, nor a `%` (a percent-escape is no title).
            title in "[^#<>\\[\\]{}|%\\x00-\\x1f]{1,60}",
            lang in prop::sample::select(vec!["enwiki", "dewiki", "zh_yuewiki", "be_x_oldwiki", "simplewiki"]),
        ) {
            let e = serde_json::json!({"sitelinks": {lang: {"site": lang, "title": title}}});
            for a in wikipedia_articles(&e) {
                let again = crate::wikipedia::parse(&a.uri).unwrap();
                prop_assert_eq!(&again.canonical, &a.uri);
                prop_assert!(crate::catalogue::link_key(&a.uri).is_some());
            }
        }

        /// Whatever labels the label items carry, a card's labels are valid tag labels, each
        /// once, at most 16.
        #[test]
        fn resolved_labels_are_valid_tag_labels(raw in proptest::collection::vec("\\PC{0,30}", 0..40)) {
            let mut labels = Labels::new();
            let items: Vec<u32> = (1..=raw.len() as u32).collect();
            for (n, l) in items.iter().zip(&raw) {
                labels.insert(*n, crate::catalogue::label(l).map(String::into_boxed_str));
            }
            let (card, _) = resolve(Card::default(), &items, &labels);
            prop_assert!(card.labels.len() <= LABEL_ITEMS);
            let set: BTreeSet<String> = card.labels.iter().cloned().collect();
            prop_assert_eq!(set.len(), card.labels.len());
            for l in &card.labels {
                let made = crate::catalogue::label(l);
                prop_assert_eq!(made.as_deref(), Some(l.as_str()));
            }
        }
    }

    static FIXTURE: std::sync::LazyLock<std::collections::BTreeMap<String, Vec<u8>>> =
        std::sync::LazyLock::new(fixture);
}
