//! The text index: every search unit's words, tags, type, source, place and time in an embedded
//! Tantivy index, so a search with `q` reads its best candidates there instead of ranking every
//! match in Postgres ([`SearchIndex::candidates`]). The index is derived data: Postgres stays the
//! source of truth, the search loads the candidates from it and checks every filter again, and a
//! missing or unreadable index is rebuilt from it.
//!
//! Writers never call the index. Triggers on the tables a unit is read from queue the resource
//! (`search_index_queue`); one pass every `limits.search_index_commit_secs` reads the units of
//! the queued resources again and commits them ([`SearchIndex::sync`]). An index in memory (the
//! tests' and the e2e harness', [`SearchIndex::in_memory`]) runs that pass before each search
//! instead, so a search sees every write before it.
//!
//! On disk (`search.index_dir`) the directory holds one generation of the index at a time, named
//! by its `CURRENT` file; a rebuild writes the next generation beside it and then switches
//! `CURRENT` to it ([`reindex`]).
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, HashSet},
    ops::Bound,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tantivy::{
    collector::TopDocs,
    query::{
        BooleanQuery, BoostQuery, ConstScoreQuery, ExistsQuery, FuzzyTermQuery, Occur, PhraseQuery,
        Query, RangeQuery, RegexQuery, TermQuery, TermSetQuery,
    },
    schema::{
        Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value,
        FAST, INDEXED, STORED, STRING,
    },
    tokenizer::{
        AsciiFoldingFilter, LowerCaser, NgramTokenizer, RawTokenizer, RemoveLongFilter,
        TextAnalyzer, Token, TokenStream, Tokenizer,
    },
    DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentReader, TantivyDocument,
    Term,
};

/// The version of the index's fields and analyzers, kept in each commit: an index written with
/// another is rebuilt.
const FORMAT: u32 = 3;
/// How much a match in each field weighs (BM25 times this): the name most, then people's tags,
/// other names and source labels, then the description.
const NAME_BOOST: Score = 3.0;
const TAGS_BOOST: Score = 2.0;
const OTHER_NAMES_BOOST: Score = 1.5;
const LABELS_BOOST: Score = 1.5;
const DESCRIPTION_BOOST: Score = 1.0;
/// The whole text equal to the whole name (case, accents and spacing aside), on top of its words:
/// a fixed score, whatever the corpus' word frequencies, above the activity of a thousand taggers
/// at [`ACTIVITY_WEIGHT`].
const EXACT_NAME_SCORE: Score = 20.0;
/// Every word of the text in the name, in order.
const NAME_PHRASE_BOOST: Score = 1.0;
/// The last word of the text as the start of a word in the name (`beatl` finds `Beatles`): a
/// fixed score, since the text may still be typed, above what a description word scores alone.
const PREFIX_SCORE: Score = 10.0;
/// The text inside the name, across words (Postgres' `ILIKE '%text%'`), proposed to Postgres.
const SUBSTRING_SCORE: Score = 8.0;
/// Each three-letter piece a name shares with the text (Postgres' trigram similarity, `pg_trgm`),
/// proposed to Postgres.
const SIMILAR_PIECE_SCORE: Score = 0.5;
/// The share of documents above which a three-letter piece is too common to ask for, once it is
/// in at least [`COMMON_PIECE_DOCS`] documents.
const COMMON_PIECE: f64 = 0.05;
const COMMON_PIECE_DOCS: u64 = 10_000;
/// `pg_trgm`'s similarity threshold (`%`): shared pieces over the pieces of both. A name holds at
/// least the pieces it shares, so a name similar so shares at least 0.3 of the text's pieces: the
/// least the index asks for.
const SIMILARITY: f64 = 0.3;
/// How much social activity counts against text relevance in a text search's rank:
/// `relevance × (1 + fame) + ACTIVITY_WEIGHT × activity`. The activity score is already on a
/// log scale (`ln(1 + taggers)` and the like, halved every 30 days), so each further tag adds
/// less than the one before, and a thousand taggers (about 6.9) stay below one exact name match.
pub const ACTIVITY_WEIGHT: f64 = 1.0;
/// The activity score's half-life, as `resource_score` and the search's ranking decay it.
const HALF_LIFE_SECS: f64 = 30.0 * 86400.0;
/// The H3 resolutions of a unit's map cells, coarse to fine: about 1,770 km², 36 km² and
/// 0.74 km² per cell. A `near` circle is covered by the finest of them that needs at most
/// [`MAX_CELLS`] cells.
pub const CELL_RESOLUTIONS: [h3o::Resolution; 3] = [
    h3o::Resolution::Four,
    h3o::Resolution::Six,
    h3o::Resolution::Eight,
];
const MAX_CELLS: usize = 1100;
/// Catalogue cards a pass reads again, of a catalogue whose import has finished: about three
/// seconds of work, so the queues wait no longer.
const REREAD_PART: i64 = 20_000;
/// Queue rows read per pass.
const QUEUE_BATCH: i64 = 20_000;
/// The longest word the index keeps (bytes); a longer one is left out of the words.
const LONGEST_WORD: usize = 64;
/// The share of deleted documents at which a segment is merged without them.
const DELETED_SHARE_BEFORE_MERGE: f32 = 0.05;
/// Units a rebuild hands its writer at a time.
const ADD_BATCH: usize = 1000;
/// Units between two progress reports of a rebuild.
const PROGRESS_EVERY: u64 = 10_000;
/// The memory of the writer that commits the queue, and of each of a rebuild's threads.
const WRITER_BYTES: usize = 64_000_000;
const REBUILD_THREADS: usize = 4;
/// The tokenizers' names.
const WORDS: &str = "pinakes_words";
const EXACT: &str = "pinakes_exact";
const GRAMS: &str = "pinakes_grams";
const GRAMS2: &str = "pinakes_grams2";
const GRAMS3: &str = "pinakes_grams3";
const PIECES: &str = "pinakes_pieces";

/// The index's fields.
#[derive(Clone, Copy)]
struct Fields {
    unit_id: Field,
    resource_id: Field,
    name: Field,
    name_exact: Field,
    name_grams: Field,
    name_pieces: Field,
    other_names: Field,
    description: Field,
    tags_text: Field,
    labels_text: Field,
    tag: Field,
    label: Field,
    type_name: Field,
    type_path: Field,
    host: Field,
    category: Field,
    venue: Field,
    source: Field,
    catalogue: Field,
    import: Field,
    has_text: Field,
    referenced: Field,
    cells: [Field; 3],
    lat: Field,
    lon: Field,
    start_at: Field,
    end_at: Field,
    fame: Field,
    activity: Field,
    activity_at: Field,
    newest_at: Field,
}

fn schema() -> (Schema, Fields) {
    let mut s = Schema::builder();
    let text = |tokenizer: &str, positions: bool| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(tokenizer)
                .set_index_option(if positions {
                    IndexRecordOption::WithFreqsAndPositions
                } else {
                    IndexRecordOption::WithFreqs
                }),
        )
    };
    let number = || NumericOptions::default().set_indexed().set_fast();
    let fields = Fields {
        unit_id: s.add_text_field("unit_id", STRING | STORED),
        resource_id: s.add_text_field("resource_id", STRING | STORED),
        name: s.add_text_field("name", text(WORDS, true)),
        name_exact: s.add_text_field("name_exact", text(EXACT, false)),
        name_grams: s.add_text_field("name_grams", text(GRAMS, false)),
        name_pieces: s.add_text_field("name_pieces", text(PIECES, false)),
        other_names: s.add_text_field("other_names", text(WORDS, true)),
        description: s.add_text_field("description", text(WORDS, true)),
        tags_text: s.add_text_field("tags_text", text(WORDS, true)),
        labels_text: s.add_text_field("labels_text", text(WORDS, true)),
        tag: s.add_text_field("tag", STRING),
        label: s.add_text_field("label", STRING),
        type_name: s.add_text_field("type", STRING | FAST),
        type_path: s.add_text_field("type_path", STRING),
        host: s.add_text_field("host", STRING),
        category: s.add_text_field("category", STRING),
        venue: s.add_text_field("venue", STRING),
        source: s.add_text_field("source", STRING),
        catalogue: s.add_text_field("catalogue", STRING),
        import: s.add_text_field("import", STRING),
        has_text: s.add_u64_field("has_text", INDEXED),
        referenced: s.add_u64_field("referenced", INDEXED | FAST),
        cells: [
            s.add_u64_field("cell_coarse", INDEXED),
            s.add_u64_field("cell_middle", INDEXED),
            s.add_u64_field("cell_fine", INDEXED),
        ],
        lat: s.add_f64_field("lat", number()),
        lon: s.add_f64_field("lon", number()),
        start_at: s.add_i64_field("start_at", number()),
        end_at: s.add_i64_field("end_at", number()),
        fame: s.add_f64_field("fame", FAST),
        activity: s.add_f64_field("activity", FAST),
        activity_at: s.add_i64_field("activity_at", FAST),
        newest_at: s.add_i64_field("newest_at", number()),
    };
    (s.build(), fields)
}

/// Words as search reads them (`api::word_char`): letters, digits and the marks words are
/// written with, apart at every other character; a character of a script written without spaces
/// (Chinese, Japanese, Thai, ...) is a word of its own, so a run of them matches as a phrase and a
/// Latin word right after one starts there (`AI` in `我的AI助手`).
#[derive(Clone, Default)]
struct WordTokenizer {
    token: Token,
}
struct WordStream<'a> {
    text: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    token: &'a mut Token,
}
impl Tokenizer for WordTokenizer {
    type TokenStream<'a> = WordStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> WordStream<'a> {
        self.token.reset();
        WordStream {
            text,
            chars: text.char_indices().peekable(),
            token: &mut self.token,
        }
    }
}
impl TokenStream for WordStream<'_> {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        self.token.position = self.token.position.wrapping_add(1);
        while let Some((from, c)) = self.chars.next() {
            let end = if crate::api::unspaced_script(c) && c.is_alphanumeric() {
                from + c.len_utf8()
            } else if crate::api::word_char(c) {
                let mut end = from + c.len_utf8();
                while let Some(&(at, next)) = self.chars.peek() {
                    if !crate::api::word_char(next) {
                        break;
                    }
                    end = at + next.len_utf8();
                    self.chars.next();
                }
                end
            } else {
                continue;
            };
            self.token.offset_from = from;
            self.token.offset_to = end;
            // Lowercased here, without the combining accents (`İ` lowers to `i` and a dot above;
            // an accent written apart stays off its letter), so folding finds the plain letter.
            for c in self.text[from..end].chars().flat_map(char::to_lowercase) {
                if !matches!(u32::from(c), 0x0300..=0x036F) {
                    self.token.text.push(c);
                }
            }
            if self.token.text.is_empty() {
                continue;
            }
            return true;
        }
        false
    }
    fn token(&self) -> &Token {
        self.token
    }
    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}
/// A text's three-letter pieces as `pg_trgm` takes them: each word (letters and digits),
/// lowercased, with two spaces before it and one after (`cat` is `  c`, ` ca`, `cat`, `at `).
fn pieces(text: &str) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let padded: Vec<char> = "  "
            .chars()
            .chain(word.chars().flat_map(char::to_lowercase))
            .chain(" ".chars())
            .collect();
        for piece in padded.windows(3) {
            out.insert(piece.iter().collect::<String>());
        }
    }
    out.into_iter().collect()
}
/// [`pieces`] as a tokenizer, for the name's pieces.
#[derive(Clone, Default)]
struct PieceTokenizer {
    pieces: Vec<String>,
    token: Token,
}
struct PieceStream<'a> {
    pieces: std::vec::IntoIter<String>,
    token: &'a mut Token,
}
impl Tokenizer for PieceTokenizer {
    type TokenStream<'a> = PieceStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> PieceStream<'a> {
        self.token.reset();
        self.pieces = pieces(text);
        PieceStream {
            pieces: std::mem::take(&mut self.pieces).into_iter(),
            token: &mut self.token,
        }
    }
}
impl TokenStream for PieceStream<'_> {
    fn advance(&mut self) -> bool {
        match self.pieces.next() {
            Some(piece) => {
                self.token.text = piece;
                self.token.position = self.token.position.wrapping_add(1);
                true
            }
            None => false,
        }
    }
    fn token(&self) -> &Token {
        self.token
    }
    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}
/// Words as the index keeps them ([`WordTokenizer`]), lowercased and folded to ASCII where a
/// letter has a plain form (`Zürich` and `İzmir` are `zurich` and `izmir`).
fn words() -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(RemoveLongFilter::limit(LONGEST_WORD))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}
fn folded(tokenizer: impl tantivy::tokenizer::Tokenizer) -> TextAnalyzer {
    TextAnalyzer::builder(tokenizer)
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}
fn register(index: &Index) {
    let tokenizers = index.tokenizers();
    tokenizers.register(WORDS, words());
    tokenizers.register(EXACT, folded(RawTokenizer::default()));
    let grams = |min, max| NgramTokenizer::new(min, max, false).expect("valid n-gram sizes");
    tokenizers.register(GRAMS, folded(grams(2, 3)));
    tokenizers.register(GRAMS2, folded(grams(2, 2)));
    tokenizers.register(GRAMS3, folded(grams(3, 3)));
    tokenizers.register(
        PIECES,
        TextAnalyzer::builder(PieceTokenizer::default()).build(),
    );
}
/// The tokens `analyzer` makes of `text`.
fn tokens(index: &Index, analyzer: &str, text: &str) -> Vec<String> {
    let mut analyzer = index
        .tokenizers()
        .get(analyzer)
        .expect("registered tokenizer");
    let mut stream = analyzer.token_stream(text);
    let mut out = vec![];
    while let Some(token) = stream.next() {
        out.push(token.text.clone());
    }
    out
}
/// A name as one exact token: its words apart by single spaces.
fn squashed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What the commit records: the format, and the queue position the index covers.
#[derive(serde::Serialize, serde::Deserialize)]
struct Payload {
    format: u32,
    /// Every transaction below this ID had finished when the index read the queue; the ones at
    /// or above it may be read again.
    boundary: u64,
    /// The catalogues whose cards the index holds, each with the end of the full import it read
    /// them after (`catalogue_state.last_import_at`, empty when none had finished).
    #[serde(default)]
    catalogues: BTreeMap<String, String>,
    /// The database the index was built from ([`database_identity`]).
    #[serde(default)]
    database: String,
}
/// The database's identity for the index: the token made with it (`search_index_database`) and
/// its ID in the cluster (`pg_database.oid`). A copy restored elsewhere, or another database on the
/// same cluster, has another.
async fn database_identity(db: impl sqlx::PgExecutor<'_>) -> Result<String> {
    Ok(sqlx::query_scalar(
        "SELECT (SELECT token::text FROM search_index_database WHERE id=1)||':'||(SELECT oid::text FROM pg_database WHERE datname=current_database())",
    )
    .fetch_one(db)
    .await?)
}

/// One search unit as the index keeps it, read from Postgres.
#[derive(sqlx::FromRow)]
struct Unit {
    unit_id: String,
    resource_id: String,
    name: Option<String>,
    description: Option<String>,
    other: Option<String>,
    type_name: Option<String>,
    type_path: Option<Vec<String>>,
    host: Option<String>,
    category: Option<String>,
    venue_id: Option<String>,
    resolver: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    start_at: Option<DateTime<Utc>>,
    end_at: Option<DateTime<Utc>>,
    score: Option<f64>,
    scored_at: Option<DateTime<Utc>>,
    newest_at: Option<DateTime<Utc>>,
    tags: Vec<String>,
}
/// The units search can find by text: those with a reference that counts, and never a post whose
/// author has not opted in to search (it is never found by its text). An OSM place's address
/// line counts as its description, as its Postgres text does.
const UNITS: &str = "SELECT r.unit_id, r.id AS resource_id, r.name, r.doc->>'description' AS description, p.display_name AS other, r.type AS type_name, r.type_path, r.host, r.category, r.venue_id, r.resolver, ST_Y(r.point) AS lat, ST_X(r.point) AS lon, r.start_at, r.end_at, s.score, s.updated_at AS scored_at, s.newest_at, ARRAY(SELECT DISTINCT f.label FROM effective_reference f WHERE f.resource_id=r.id AND f.label IS NOT NULL ORDER BY f.label) AS tags FROM search_unit r LEFT JOIN resource_score s ON s.resource_id=r.id LEFT JOIN osm_place p ON p.osm_type=r.osm_type AND p.osm_id=r.osm_id WHERE coalesce(r.doc->'attributes'->>'opted_out','')<>'true' AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id)";

/// One catalogue card as the index keeps it.
#[derive(sqlx::FromRow)]
struct CardRow {
    id: i64,
    catalogue: String,
    source: String,
    source_id: String,
    uri: String,
    type_name: String,
    name: String,
    other_names: Vec<String>,
    description: Option<String>,
    labels: Vec<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    fame: f32,
}
/// A catalogue card as the index reads it ([`CardRow`]).
macro_rules! card_columns {
    () => {
        "SELECT c.id, c.catalogue, c.source, c.source_id, c.uri, c.type AS type_name, c.name, c.other_names, c.description, c.labels, ST_Y(c.point) AS lat, ST_X(c.point) AS lon, c.fame FROM catalogue_card c"
    };
}
/// The cards of the catalogues given.
const CARDS: &str = concat!(card_columns!(), " WHERE c.catalogue=ANY($1)");
/// The next part of one catalogue's cards after a source ID, in the order of its unique
/// `(catalogue, source_id)` index, so each part reads only its own rows.
const CARDS_PART: &str = concat!(
    card_columns!(),
    " WHERE c.catalogue=$1 AND c.source_id>$2 ORDER BY c.source_id LIMIT $3"
);
/// The index's ID of a catalogue card's document (its unit ID is the resource ID its link would
/// have, [`card_unit`]).
fn card_key(id: i64) -> String {
    format!("catalogue:{id}")
}
/// The unit ID of a catalogue card: the resource ID of its canonical link, so the card is the same
/// unit once someone references the link.
pub fn card_unit(uri: &str) -> Option<String> {
    crate::model::identity(uri).ok().map(|(id, _, _)| id)
}
fn card_document(f: &Fields, card: &CardRow, import: &str) -> Option<TantivyDocument> {
    let unit = card_unit(&card.uri)?;
    let mut doc = TantivyDocument::default();
    doc.add_text(f.unit_id, &unit);
    doc.add_text(f.resource_id, card_key(card.id));
    let name = card.name.trim();
    if !name.is_empty() {
        doc.add_text(f.name, name);
        doc.add_text(f.name_exact, squashed(name));
        doc.add_text(f.name_grams, name);
        doc.add_text(f.name_pieces, name);
    }
    for other in &card.other_names {
        doc.add_text(f.other_names, other);
    }
    if let Some(description) = card.description.as_deref().filter(|d| !d.trim().is_empty()) {
        doc.add_text(f.description, description);
    }
    for label in &card.labels {
        doc.add_text(f.label, label);
        doc.add_text(f.labels_text, label);
    }
    doc.add_text(f.type_name, &card.type_name);
    for t in crate::model::type_path(&card.type_name) {
        doc.add_text(f.type_path, t);
    }
    if let Some(host) = url::Url::parse(&card.uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    {
        doc.add_text(f.host, host);
    }
    doc.add_text(f.source, &card.source);
    doc.add_text(f.catalogue, &card.catalogue);
    doc.add_text(f.import, import);
    doc.add_u64(f.referenced, 0);
    doc.add_f64(f.fame, f64::from(card.fame).clamp(0.0, 1.0));
    doc.add_f64(f.activity, 0.0);
    doc.add_i64(f.activity_at, 0);
    if let (Some(lat), Some(lon)) = (card.lat, card.lon) {
        doc.add_f64(f.lat, lat);
        doc.add_f64(f.lon, lon);
        if let Ok(point) = h3o::LatLng::new(lat, lon) {
            for (field, resolution) in f.cells.iter().zip(CELL_RESOLUTIONS) {
                doc.add_u64(*field, u64::from(point.to_cell(resolution)));
            }
        }
    }
    Some(doc)
}
fn document(f: &Fields, unit: &Unit) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_text(f.unit_id, &unit.unit_id);
    doc.add_text(f.resource_id, &unit.resource_id);
    fn present(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    if let Some(name) = present(&unit.name) {
        doc.add_text(f.name, name);
        doc.add_text(f.name_exact, squashed(name));
        doc.add_text(f.name_grams, name);
        doc.add_text(f.name_pieces, name);
    }
    if let Some(description) = present(&unit.description) {
        doc.add_text(f.description, description);
    }
    if let Some(other) = present(&unit.other) {
        doc.add_text(f.description, other);
    }
    for tag in &unit.tags {
        doc.add_text(f.tag, tag);
        doc.add_text(f.tags_text, tag);
    }
    if let Some(type_name) = present(&unit.type_name) {
        doc.add_text(f.type_name, type_name);
    }
    for t in unit.type_path.iter().flatten() {
        doc.add_text(f.type_path, t);
    }
    for (field, value) in [
        (f.host, &unit.host),
        (f.category, &unit.category),
        (f.venue, &unit.venue_id),
        (f.source, &unit.resolver),
    ] {
        if let Some(value) = value {
            doc.add_text(field, value);
        }
    }
    doc.add_u64(f.referenced, 1);
    if has_text(unit) {
        doc.add_u64(f.has_text, 1);
    }
    doc.add_f64(f.fame, 0.0);
    doc.add_f64(f.activity, unit.score.unwrap_or(0.0));
    doc.add_i64(f.activity_at, unit.scored_at.map_or(0, |t| t.timestamp()));
    if let Some(newest) = unit.newest_at {
        doc.add_i64(f.newest_at, newest.timestamp());
    }
    if let Some(start) = unit.start_at {
        doc.add_i64(f.start_at, start.timestamp());
    }
    if let Some(end) = unit.end_at {
        doc.add_i64(f.end_at, end.timestamp());
    }
    if let (Some(lat), Some(lon)) = (unit.lat, unit.lon) {
        doc.add_f64(f.lat, lat);
        doc.add_f64(f.lon, lon);
        if let Ok(point) = h3o::LatLng::new(lat, lon) {
            for (field, resolution) in f.cells.iter().zip(CELL_RESOLUTIONS) {
                doc.add_u64(*field, u64::from(point.to_cell(resolution)));
            }
        }
    }
    doc
}

/// An open index: its reader, the writer that commits the queue, and how far it read the queue.
struct Open {
    index: Index,
    fields: Fields,
    reader: IndexReader,
    writer: std::sync::Mutex<IndexWriter>,
    progress: std::sync::Mutex<Progress>,
    /// [`Payload::catalogues`] as the next commit records it.
    catalogues: std::sync::Mutex<BTreeMap<String, String>>,
    /// The last catalogue queue row an in-memory index read.
    catalogue_seq: AtomicU64,
    /// The database the index belongs to ([`database_identity`]).
    database: String,
    /// The catalogue being read again, in parts ([`Reread`]).
    reread: std::sync::Mutex<Option<Reread>>,
    /// Merges after a removal ([`purge`]), for tests.
    purges: AtomicU64,
    /// Searches that read the proposals (the text inside names, similar names), for tests.
    proposed: AtomicU64,
}
/// A catalogue read again after a full import, a part per pass ([`SearchIndex::sync_catalogues`]):
/// its documents are rewritten in place, card by card, marked with the import read (`marker`);
/// at the end those of cards gone since (not rewritten) are deleted and the marker recorded.
#[derive(Clone, Debug)]
struct Reread {
    catalogue: String,
    marker: String,
    /// The source ID of the last card rewritten (empty before the first part).
    after: String,
}
#[derive(Clone, Default)]
struct Progress {
    boundary: u64,
    /// Transactions at or above `boundary` whose rows were read already (all of them: a
    /// transaction's rows become visible together).
    applied: HashSet<u64>,
}
impl Open {
    fn new(
        index: Index,
        boundary: u64,
        catalogues: BTreeMap<String, String>,
        database: String,
    ) -> Result<Self> {
        register(&index);
        let fields = schema().1;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer: IndexWriter = index.writer_with_num_threads(1, WRITER_BYTES)?;
        // Segments with deleted documents merge early, so a changed card's old words leave the
        // files soon; [`Open::replace`] merges at once after a removal.
        let mut policy = tantivy::indexer::LogMergePolicy::default();
        policy.set_del_docs_ratio_before_merge(DELETED_SHARE_BEFORE_MERGE);
        writer.set_merge_policy(Box::new(policy));
        Ok(Self {
            index,
            fields,
            reader,
            writer: std::sync::Mutex::new(writer),
            progress: std::sync::Mutex::new(Progress {
                boundary,
                applied: HashSet::new(),
            }),
            catalogues: std::sync::Mutex::new(catalogues),
            catalogue_seq: AtomicU64::new(0),
            database,
            reread: std::sync::Mutex::new(None),
            purges: AtomicU64::new(0),
            proposed: AtomicU64::new(0),
        })
    }
    /// What the next commit records, with `boundary`.
    fn stamp(&self, boundary: u64) -> Payload {
        Payload {
            format: FORMAT,
            boundary,
            catalogues: self.catalogues.lock().expect("markers").clone(),
            database: self.database.clone(),
        }
    }
}

/// Where the index lives.
enum Home {
    /// In memory, read up before each search.
    Memory,
    /// In this directory, committed by the daemon's pass.
    Disk(PathBuf),
}

/// The text index of one instance ([module docs](self)).
pub struct SearchIndex {
    home: Home,
    open: std::sync::RwLock<Option<Arc<Open>>>,
    /// One pass (or build) at a time.
    pass: tokio::sync::Mutex<()>,
    /// Moves with every commit, so a search page's ETag changes when the index does.
    revision: AtomicU64,
    /// Queue rows read at a time ([`QUEUE_BATCH`]).
    batch: i64,
    /// Catalogue cards read again per pass ([`REREAD_PART`]).
    reread_part: i64,
    /// Text searches answered.
    served: AtomicU64,
    /// The catalogues whose cards the index holds: those switched on.
    catalogues: std::sync::RwLock<Vec<String>>,
}
impl SearchIndex {
    /// Hold the cards of these catalogues (the ones switched on, `config.catalogues`); the
    /// cards of any other leave the index at the next pass.
    pub fn set_catalogues(&self, names: &[&str]) {
        *self.catalogues.write().expect("catalogues lock") =
            names.iter().map(|n| n.to_string()).collect();
    }
    fn catalogues(&self) -> Vec<String> {
        self.catalogues.read().expect("catalogues lock").clone()
    }
    /// An index in memory, built from the database on its first search and read up before
    /// every search after: what `AppState::new` holds until the daemon sets its own.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self::with_home(Home::Memory))
    }
    /// The daemon's index in `dir`, opened (or built) by [`SearchIndex::run`]. Until then a
    /// search with `q` reads Postgres alone.
    pub fn on_disk(dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self::with_home(Home::Disk(dir.into())))
    }
    /// [`SearchIndex::in_memory`] reading the queue `batch` rows at a time (tests).
    #[doc(hidden)]
    pub fn in_memory_with_batch(batch: i64) -> Arc<Self> {
        Arc::new(Self {
            batch,
            ..Self::with_home(Home::Memory)
        })
    }
    /// [`SearchIndex::in_memory`] reading a catalogue again `part` cards per pass (tests).
    #[doc(hidden)]
    pub fn in_memory_with_reread_part(part: i64) -> Arc<Self> {
        Arc::new(Self {
            reread_part: part,
            ..Self::with_home(Home::Memory)
        })
    }
    /// How many merges removals caused (tests).
    #[doc(hidden)]
    pub fn purges(&self) -> u64 {
        self.current()
            .map_or(0, |open| open.purges.load(Ordering::SeqCst))
    }
    /// How many searches read the proposals (tests).
    #[doc(hidden)]
    pub fn proposed(&self) -> u64 {
        self.current()
            .map_or(0, |open| open.proposed.load(Ordering::SeqCst))
    }
    /// [`SearchIndex::on_disk`] reading the queue `batch` rows at a time (tests).
    #[doc(hidden)]
    pub fn on_disk_with_batch(dir: impl Into<PathBuf>, batch: i64) -> Arc<Self> {
        Arc::new(Self {
            batch,
            ..Self::with_home(Home::Disk(dir.into()))
        })
    }
    fn with_home(home: Home) -> Self {
        Self {
            batch: QUEUE_BATCH,
            reread_part: REREAD_PART,
            served: AtomicU64::new(0),
            catalogues: std::sync::RwLock::new(vec![]),
            home,
            open: std::sync::RwLock::new(None),
            pass: tokio::sync::Mutex::new(()),
            // Unique to this start (milliseconds, then a count), so an ETag from before a restart
            // never matches a page of the new index.
            revision: AtomicU64::new(
                (chrono::Utc::now().timestamp_millis().max(0) as u64).saturating_mul(1_000_000),
            ),
        }
    }
    fn current(&self) -> Option<Arc<Open>> {
        self.open.read().expect("index lock").clone()
    }
    fn set(&self, open: Open) {
        *self.open.write().expect("index lock") = Some(Arc::new(open));
        self.revision.fetch_add(1, Ordering::SeqCst);
    }
    /// Whether any segment of the index files, deleted documents included, still holds `word`
    /// in a name or description (tests).
    #[doc(hidden)]
    pub fn holds_word(&self, word: &str) -> bool {
        let Some(open) = self.current() else {
            return false;
        };
        let searcher = open.reader.searcher();
        [open.fields.name, open.fields.description]
            .into_iter()
            .any(|field| {
                searcher.segment_readers().iter().any(|segment| {
                    segment
                        .inverted_index(field)
                        .ok()
                        .and_then(|inverted| inverted.terms().get(word.as_bytes()).ok().flatten())
                        .is_some()
                })
            })
    }
    /// How many text searches the index answered (tests).
    #[doc(hidden)]
    pub fn served(&self) -> u64 {
        self.served.load(Ordering::SeqCst)
    }
    /// Whether the index is open (on disk: opened or built by [`SearchIndex::run`]).
    pub fn is_open(&self) -> bool {
        self.current().is_some()
    }
    /// Changes with every commit and rebuild.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }
    /// Open the index on disk, or build it from the database when it is missing, unreadable or
    /// of another format (logged), then commit the queue every `period` until `shutdown`.
    /// `passed` is set to the time of each pass that read the queue without an error, `open` to 1
    /// once the index is open.
    pub async fn run(
        self: Arc<Self>,
        pool: PgPool,
        period: Duration,
        passed: prometheus::IntGauge,
        open: prometheus::IntGauge,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let Home::Disk(dir) = &self.home else {
            return;
        };
        loop {
            let opened = tokio::select! {
                _ = shutdown.changed() => return,
                opened = self.open_or_build(&pool, dir) => opened,
            };
            match opened {
                Ok(()) => {
                    open.set(1);
                    break;
                }
                Err(error) => {
                    tracing::error!(error = %crate::resolver::log_error(&error), dir = %dir.display(), "the text index could not be opened or built; text search reads Postgres until it is");
                    // The next build reads every unit again, from a later snapshot: every row this
                    // sees (those of finished transactions) is covered by it, so the queue does
                    // not grow while closed.
                    let trimmed = async {
                        sqlx::query("DELETE FROM search_index_queue")
                            .execute(&pool)
                            .await?;
                        sqlx::query("DELETE FROM search_index_catalogue_queue")
                            .execute(&pool)
                            .await
                    };
                    if let Err(error) = trimmed.await {
                        tracing::warn!(error = %crate::resolver::log_error(&error), "the text index queues were not trimmed");
                    }
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                    }
                }
            }
        }
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                synced = self.sync(&pool) => match synced {
                    Ok(()) => passed.set(chrono::Utc::now().timestamp()),
                    Err(error) => tracing::warn!(error = %crate::resolver::log_error(&error), "the text index pass will retry"),
                },
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(period) => {}
            }
        }
    }
    async fn open_or_build(&self, pool: &PgPool, dir: &Path) -> Result<()> {
        let _pass = self.pass.lock().await;
        let xmin: String =
            sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
                .fetch_one(pool)
                .await?;
        let xmin: u64 = xmin.parse().context("transaction ID")?;
        let database = database_identity(pool).await?;
        match open_current(dir) {
            // Another database's index: another token or database ID, or a boundary past this
            // cluster's transactions (a copy restored into another cluster starts them anew).
            Ok(Some((_, payload)))
                if payload.format == FORMAT
                    && (payload.database != database || payload.boundary > xmin) =>
            {
                tracing::warn!(dir = %dir.display(), "the text index was built from another database; rebuilding it from this one")
            }
            Ok(Some((index, payload))) if payload.format == FORMAT => {
                self.set(Open::new(
                    index,
                    payload.boundary,
                    payload.catalogues,
                    database,
                )?);
                tracing::info!(dir = %dir.display(), "text index opened");
                return Ok(());
            }
            Ok(Some(_)) => {
                tracing::warn!(dir = %dir.display(), "the text index has another format; rebuilding it from the database")
            }
            Ok(None) => {
                tracing::warn!(dir = %dir.display(), "no text index; building it from the database")
            }
            Err(error) => {
                tracing::warn!(error = %crate::resolver::log_error(&error), dir = %dir.display(), "the text index is unreadable; rebuilding it from the database")
            }
        }
        let started = std::time::Instant::now();
        let (index, boundary, units, markers) =
            build_generation(pool, dir, &self.catalogues(), |n| {
                tracing::info!(units = n, "building the text index")
            })
            .await?;
        tracing::info!(
            units,
            secs = started.elapsed().as_secs(),
            "text index built"
        );
        self.set(Open::new(index, boundary, markers, database)?);
        // The build read every transaction below its boundary, and every catalogue card.
        sqlx::query("DELETE FROM search_index_queue WHERE xid<$1::text::xid8")
            .bind(boundary.to_string())
            .execute(pool)
            .await?;
        Ok(())
    }
    /// The open index; in memory, built from the database the first time.
    async fn ready(&self, pool: &PgPool) -> Result<Option<Arc<Open>>> {
        if let Some(open) = self.current() {
            return Ok(Some(open));
        }
        if let Home::Memory = self.home {
            let _pass = self.pass.lock().await;
            if let Some(open) = self.current() {
                return Ok(Some(open));
            }
            let index = Index::create_in_ram(schema().0);
            register(&index);
            let (boundary, _, markers) = fill(pool, &index, 1, &self.catalogues(), |_| {}).await?;
            let database = database_identity(pool).await?;
            self.set(Open::new(index, boundary, markers, database)?);
        }
        Ok(self.current())
    }
    /// One pass: read the units of every resource queued by a transaction that finished since the
    /// last pass, replace their documents and commit. On disk, then delete the queue rows the
    /// commit covers.
    pub async fn sync(&self, pool: &PgPool) -> Result<()> {
        let Some(open) = self.ready(pool).await? else {
            return Ok(());
        };
        let _pass = self.pass.lock().await;
        if let Home::Disk(_) = self.home {
            return self.drain(pool, &open).await;
        }
        let Progress {
            boundary,
            mut applied,
        } = open.progress.lock().expect("progress").clone();
        // The first read's snapshot: every transaction below its xmin had finished, so its rows
        // are read by the time the batches below run dry.
        let mut first_xmin: Option<u64> = None;
        loop {
            let skip: Vec<String> = applied.iter().map(u64::to_string).collect();
            let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
                "WITH s AS (SELECT pg_snapshot_xmin(pg_current_snapshot())::text AS xmin) SELECT s.xmin, q.xid::text, q.resource_id FROM s LEFT JOIN LATERAL (SELECT xid, resource_id FROM search_index_queue WHERE xid>=$1::text::xid8 AND xid<>ALL($3::text[]::xid8[]) ORDER BY xid LIMIT $2) q ON true",
            )
            .bind(boundary.to_string())
            .bind(self.batch)
            .bind(&skip)
            .fetch_all(pool)
            .await?;
            let xmin: u64 = rows
                .first()
                .context("no snapshot")?
                .0
                .parse()
                .context("transaction ID")?;
            let xmin = *first_xmin.get_or_insert(xmin);
            let truncated = rows.len() as i64 >= self.batch;
            let mut queued: BTreeMap<u64, HashSet<String>> = BTreeMap::new();
            for (_, xid, resource) in rows {
                if let (Some(xid), Some(resource)) = (xid, resource) {
                    queued
                        .entry(xid.parse().context("transaction ID")?)
                        .or_default()
                        .insert(resource);
                }
            }
            if truncated {
                // The last transaction read may have more rows: all of them.
                if let Some((&last, _)) = queued.iter().next_back() {
                    let more: Vec<String> = sqlx::query_scalar(
                        "SELECT resource_id FROM search_index_queue WHERE xid=$1::text::xid8",
                    )
                    .bind(last.to_string())
                    .fetch_all(pool)
                    .await?;
                    queued.entry(last).or_default().extend(more);
                }
            }
            let ids = self
                .with_roots(pool, queued.values().flatten().cloned())
                .await?;
            // The last batch's commit records the new boundary; one before it, the old.
            let recorded = if truncated { boundary } else { xmin };
            if !ids.is_empty() {
                self.commit_units(pool, &open, ids, recorded).await?;
            }
            applied.extend(queued.keys().copied());
            if !truncated {
                break;
            }
        }
        let next = first_xmin.unwrap_or(boundary);
        {
            let mut progress = open.progress.lock().expect("progress");
            applied.retain(|xid| *xid >= next);
            *progress = Progress {
                boundary: next,
                applied,
            };
        }
        self.sync_catalogues(pool, &open).await
    }
    /// The daemon's pass: the queue's rows in order, a batch at a time, until it runs dry; each
    /// batch's resources read again and committed, then its rows deleted. A row of a transaction
    /// still running is not visible yet and stays for a later pass.
    async fn drain(&self, pool: &PgPool, open: &Arc<Open>) -> Result<()> {
        let boundary = committed_boundary(&open.index).unwrap_or(0);
        loop {
            let rows: Vec<(i64, String)> = sqlx::query_as(
                "SELECT seq, resource_id FROM search_index_queue ORDER BY seq LIMIT $1",
            )
            .bind(self.batch)
            .fetch_all(pool)
            .await?;
            if rows.is_empty() {
                break;
            }
            let full = rows.len() as i64 >= self.batch;
            let seqs: Vec<i64> = rows.iter().map(|(seq, _)| *seq).collect();
            let ids = self
                .with_roots(pool, rows.into_iter().map(|(_, id)| id))
                .await?;
            self.commit_units(pool, open, ids, boundary).await?;
            sqlx::query("DELETE FROM search_index_queue WHERE seq=ANY($1)")
                .bind(&seqs)
                .execute(pool)
                .await?;
            if !full {
                break;
            }
        }
        self.sync_catalogues(pool, open).await
    }
    /// The catalogue cards' part of a pass. A catalogue switched off: its documents leave the
    /// index (its cards already left every search, which filters by the catalogues that are on).
    /// Then the cards an update run changed or removed, or a full import changed in place
    /// (`search_index_catalogue_queue`), a batch at a time. Then one part of a catalogue whose
    /// full import finished since the index read it ([`Reread`]), so a long re-read never holds
    /// the queues back for more than a part.
    async fn sync_catalogues(&self, pool: &PgPool, open: &Arc<Open>) -> Result<()> {
        let enabled = self.catalogues();
        let recorded = open.catalogues.lock().expect("markers").clone();
        // The catalogues that are off and still have documents: those recorded, and one switched
        // off during its first re-read (documents, no marker yet), even across a restart.
        let off: Vec<String> = {
            let searcher = open.reader.searcher();
            crate::catalogue::Catalogue::ALL
                .iter()
                .map(|c| c.name().to_owned())
                .filter(|c| !enabled.contains(c))
                .filter(|c| {
                    recorded.contains_key(c)
                        || searcher
                            .doc_freq(&Term::from_field_text(open.fields.catalogue, c))
                            .is_ok_and(|n| n > 0)
                })
                .collect()
        };
        if !off.is_empty() {
            let removing = open.clone();
            tokio::task::spawn_blocking(move || removing.remove_catalogues(&off)).await??;
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        // One batch of the catalogue queue per pass: an import or update run that writes faster
        // than the index reads holds back neither the resource queue nor the re-read.
        'drain: {
            let after = match self.home {
                Home::Disk(_) => 0,
                Home::Memory => open.catalogue_seq.load(Ordering::SeqCst) as i64,
            };
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT seq, card_id FROM search_index_catalogue_queue WHERE seq>$1 ORDER BY seq LIMIT $2",
            )
            .bind(after)
            .bind(self.batch)
            .fetch_all(pool)
            .await?;
            let Some(&(last, _)) = rows.last() else {
                break 'drain;
            };
            let mut ids: Vec<i64> = rows.iter().map(|(_, id)| *id).collect();
            ids.sort();
            ids.dedup();
            let cards: Vec<CardRow> = sqlx::query_as(&format!("{CARDS} AND c.id=ANY($2)"))
                .bind(&enabled)
                .bind(&ids)
                .fetch_all(pool)
                .await?;
            let applying = open.clone();
            let boundary = committed_boundary(&open.index).unwrap_or(0);
            tokio::task::spawn_blocking(move || applying.replace_cards(&ids, &cards, boundary))
                .await??;
            self.revision.fetch_add(1, Ordering::SeqCst);
            match self.home {
                Home::Disk(_) => {
                    let seqs: Vec<i64> = rows.iter().map(|(seq, _)| *seq).collect();
                    sqlx::query("DELETE FROM search_index_catalogue_queue WHERE seq=ANY($1)")
                        .bind(&seqs)
                        .execute(pool)
                        .await?;
                }
                Home::Memory => open.catalogue_seq.store(last as u64, Ordering::SeqCst),
            }
        }
        // One part of a re-read: the one under way, else a catalogue with a newer import.
        let current = open.reread.lock().expect("reread").clone();
        let reread = match current {
            Some(reread) if enabled.contains(&reread.catalogue) => Some(reread),
            _ => {
                let markers = import_markers(pool, &enabled).await?;
                enabled
                    .iter()
                    .find(|c| recorded.get(*c) != markers.get(*c))
                    .map(|c| Reread {
                        catalogue: c.clone(),
                        marker: markers.get(c).cloned().unwrap_or_default(),
                        after: String::new(),
                    })
            }
        };
        *open.reread.lock().expect("reread") = reread.clone();
        let Some(reread) = reread else {
            return Ok(());
        };
        let cards: Vec<CardRow> = sqlx::query_as(CARDS_PART)
            .bind(&reread.catalogue)
            .bind(&reread.after)
            .bind(self.reread_part)
            .fetch_all(pool)
            .await?;
        let done = (cards.len() as i64) < self.reread_part;
        let after = cards
            .last()
            .map_or(reread.after.clone(), |c| c.source_id.clone());
        let rewriting = open.clone();
        let part = reread.clone();
        tokio::task::spawn_blocking(move || rewriting.reread_part(&part, &cards, done)).await??;
        self.revision.fetch_add(1, Ordering::SeqCst);
        *open.reread.lock().expect("reread") = (!done).then_some(Reread { after, ..reread });
        Ok(())
    }
    /// `ids`, each once, with the roots of those that are aliases (an alias's references count
    /// for its root).
    async fn with_roots(
        &self,
        pool: &PgPool,
        ids: impl Iterator<Item = String>,
    ) -> Result<Vec<String>> {
        let mut ids: Vec<String> = ids.collect::<HashSet<_>>().into_iter().collect();
        if !ids.is_empty() {
            let roots: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT root_id FROM resource_alias WHERE alias_id=ANY($1)",
            )
            .bind(&ids)
            .fetch_all(pool)
            .await?;
            ids.extend(roots);
            ids.sort();
            ids.dedup();
        }
        Ok(ids)
    }
    /// Read the units of `ids` (a batch of [`QUEUE_BATCH`] resources at a time), replace their
    /// documents and commit, recording `boundary`.
    async fn commit_units(
        &self,
        pool: &PgPool,
        open: &Arc<Open>,
        ids: Vec<String>,
        boundary: u64,
    ) -> Result<()> {
        for chunk in ids.chunks(self.batch.max(1) as usize) {
            let chunk = chunk.to_vec();
            let units: Vec<Unit> = sqlx::query_as(&format!("{UNITS} AND r.id=ANY($1)"))
                .bind(&chunk)
                .fetch_all(pool)
                .await?;
            let committing = open.clone();
            tokio::task::spawn_blocking(move || committing.replace(&chunk, &units, boundary))
                .await??;
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    /// The best units for a text search, at most `cap`, in the order [`TextOrder`] asks for,
    /// with every filter the index holds applied; `None` when the daemon's index is not open
    /// yet (the search then reads Postgres alone).
    pub async fn candidates(
        &self,
        pool: &PgPool,
        query: TextQuery,
        cap: usize,
    ) -> Result<Option<Candidates>> {
        if let Home::Memory = self.home {
            self.sync(pool).await?;
        }
        // Read before the search: a commit during it moves the revision past this page's.
        let revision = self.revision();
        let Some(open) = self.current() else {
            return Ok(None);
        };
        let units = tokio::task::spawn_blocking(move || open.search(&query, cap)).await??;
        self.served.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Candidates { units, revision }))
    }
}
/// The boundary the index's last commit recorded.
fn committed_boundary(index: &Index) -> Option<u64> {
    let payload: Payload =
        serde_json::from_str(index.load_metas().ok()?.payload.as_deref()?).ok()?;
    Some(payload.boundary)
}
impl Open {
    /// Replace the documents of `resources` with `units` and commit, recording `boundary`.
    fn replace(&self, resources: &[String], units: &[Unit], boundary: u64) -> Result<()> {
        // The segments holding a document with text of these resources, before they are
        // replaced: if one loses its text, only those segments are merged.
        let texts = self.text_documents(resources)?;
        let mut writer = self.writer.lock().expect("writer lock");
        let result = (|| -> Result<()> {
            for id in resources {
                writer.delete_term(Term::from_field_text(self.fields.resource_id, id));
            }
            for unit in units {
                writer.add_document(document(&self.fields, unit))?;
            }
            commit(&mut writer, &self.stamp(boundary))?;
            Ok(())
        })();
        if result.is_err() {
            // Nothing half done stays for the next commit.
            let _ = writer.rollback();
        }
        result?;
        self.reader.reload()?;
        // A resource whose document had text and now has none (a post found gone, emptied or opted
        // out, a card no longer searchable): merge the segments that held its document now, so its
        // words leave the index files, not only its searches. A pass that only adds or changes
        // documents merges nothing.
        let lost: Vec<&String> = texts
            .iter()
            .filter(|(id, _)| !units.iter().any(|u| &u.resource_id == id && has_text(u)))
            .map(|(id, _)| id)
            .collect();
        // Every segment that holds one of its documents, the current and earlier ones (deleted,
        // their words still in the files).
        let lost: Vec<tantivy::index::SegmentId> = if lost.is_empty() {
            vec![]
        } else {
            let searcher = self.reader.searcher();
            searcher
                .segment_readers()
                .iter()
                .filter(|segment| {
                    segment
                        .inverted_index(self.fields.resource_id)
                        .ok()
                        .is_some_and(|inverted| {
                            lost.iter().any(|id| {
                                inverted
                                    .terms()
                                    .get(str::as_bytes(id))
                                    .ok()
                                    .flatten()
                                    .is_some()
                            })
                        })
                })
                .map(|segment| segment.segment_id())
                .collect()
        };
        if !lost.is_empty() {
            merge(&self.index, &mut writer, &self.reader, &lost)?;
            self.purges.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    /// Of `resources`, those with a document that has text, each with its segment.
    fn text_documents(
        &self,
        resources: &[String],
    ) -> Result<Vec<(String, tantivy::index::SegmentId)>> {
        let searcher = self.reader.searcher();
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermSetQuery::new(
                    resources
                        .iter()
                        .map(|id| Term::from_field_text(self.fields.resource_id, id)),
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.fields.has_text, 1),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let found = searcher.search(&query, &tantivy::collector::DocSetCollector)?;
        found
            .into_iter()
            .map(|address| {
                let doc: TantivyDocument = searcher.doc(address)?;
                let id = doc
                    .get_first(self.fields.resource_id)
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .context("stored ID")?;
                Ok((
                    id,
                    searcher.segment_reader(address.segment_ord).segment_id(),
                ))
            })
            .collect()
    }
}
impl Open {
    /// Delete the documents of the catalogues `off`, stop recording them, commit, and merge their
    /// words out of the files (a catalogue switched off is rare; its segments may be large).
    fn remove_catalogues(&self, off: &[String]) -> Result<()> {
        let mut writer = self.writer.lock().expect("writer lock");
        let boundary = committed_boundary(&self.index).unwrap_or(0);
        let before = self.catalogues.lock().expect("markers").clone();
        for catalogue in off {
            writer.delete_term(Term::from_field_text(self.fields.catalogue, catalogue));
            self.catalogues.lock().expect("markers").remove(catalogue);
        }
        let result = commit(&mut writer, &self.stamp(boundary));
        if let Err(error) = result {
            *self.catalogues.lock().expect("markers") = before;
            let _ = writer.rollback();
            return Err(error);
        }
        self.reader.reload()?;
        let segments = segments_with_deletes(&self.index)?;
        merge(&self.index, &mut writer, &self.reader, &segments)?;
        self.purges.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    /// One part of `reread`: rewrite the documents of `cards`, marked with its import; when it is
    /// the last part, delete the catalogue's documents of other imports (cards gone since) and
    /// record the import. One commit.
    fn reread_part(&self, reread: &Reread, cards: &[CardRow], last: bool) -> Result<()> {
        let mut writer = self.writer.lock().expect("writer lock");
        let boundary = committed_boundary(&self.index).unwrap_or(0);
        let before = self.catalogues.lock().expect("markers").clone();
        let result = (|| -> Result<()> {
            for card in cards {
                writer.delete_term(Term::from_field_text(
                    self.fields.resource_id,
                    &card_key(card.id),
                ));
                if let Some(doc) = card_document(&self.fields, card, &reread.marker) {
                    writer.add_document(doc)?;
                }
            }
            if last {
                writer.delete_query(Box::new(BooleanQuery::new(vec![
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.catalogue, &reread.catalogue),
                            IndexRecordOption::Basic,
                        )),
                    ),
                    (
                        Occur::MustNot,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.import, &reread.marker),
                            IndexRecordOption::Basic,
                        )),
                    ),
                ])))?;
                self.catalogues
                    .lock()
                    .expect("markers")
                    .insert(reread.catalogue.clone(), reread.marker.clone());
            }
            commit(&mut writer, &self.stamp(boundary))
        })();
        if let Err(error) = result {
            *self.catalogues.lock().expect("markers") = before;
            let _ = writer.rollback();
            return Err(error);
        }
        self.reader.reload()?;
        Ok(())
    }
    /// The import a catalogue's documents are marked with now: the one being read again, else the
    /// one recorded.
    fn marker(&self, catalogue: &str) -> String {
        match &*self.reread.lock().expect("reread") {
            Some(reread) if reread.catalogue == catalogue => reread.marker.clone(),
            _ => self
                .catalogues
                .lock()
                .expect("markers")
                .get(catalogue)
                .cloned()
                .unwrap_or_default(),
        }
    }
}
impl Open {
    /// Replace the documents of the catalogue cards `ids` with `cards` (those still there, of a
    /// catalogue that is on) and commit.
    fn replace_cards(&self, ids: &[i64], cards: &[CardRow], boundary: u64) -> Result<()> {
        let mut writer = self.writer.lock().expect("writer lock");
        let result = (|| -> Result<()> {
            for id in ids {
                writer.delete_term(Term::from_field_text(
                    self.fields.resource_id,
                    &card_key(*id),
                ));
            }
            for card in cards {
                if let Some(doc) = card_document(&self.fields, card, &self.marker(&card.catalogue))
                {
                    writer.add_document(doc)?;
                }
            }
            commit(&mut writer, &self.stamp(boundary))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = writer.rollback();
        }
        result?;
        self.reader.reload()?;
        Ok(())
    }
}
/// The committed segments that hold deleted documents.
fn segments_with_deletes(index: &Index) -> Result<Vec<tantivy::index::SegmentId>> {
    Ok(index
        .searchable_segment_metas()?
        .into_iter()
        .filter(|m| m.num_deleted_docs() > 0)
        .map(|m| m.id())
        .collect())
}
/// Merge `segments` (those still committed), so the deleted documents' words leave the index
/// files. A background merge may take one of them meanwhile: then the segments are read again,
/// up to three times.
fn merge(
    index: &Index,
    writer: &mut IndexWriter,
    reader: &IndexReader,
    segments: &[tantivy::index::SegmentId],
) -> Result<()> {
    let mut wanted: Vec<tantivy::index::SegmentId> = segments.to_vec();
    for attempt in 1..=3 {
        let committed: HashSet<tantivy::index::SegmentId> =
            index.searchable_segment_ids()?.into_iter().collect();
        wanted.retain(|s| committed.contains(s));
        if wanted.is_empty() {
            break;
        }
        match writer.merge(&wanted).wait() {
            Ok(_) => break,
            Err(error) if attempt < 3 => {
                tracing::debug!(error = %crate::resolver::log_error(&error), "a text index merge raced another; reading the segments again");
                // A segment merged meanwhile left: what took its documents still holds them.
                wanted = segments_with_deletes(index)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    reader.reload()?;
    Ok(())
}
/// Whether a unit has words a text search finds it by.
fn has_text(unit: &Unit) -> bool {
    [&unit.name, &unit.description, &unit.other]
        .into_iter()
        .any(|t| t.as_deref().is_some_and(|t| !t.trim().is_empty()))
}
fn commit(writer: &mut IndexWriter, payload: &Payload) -> Result<()> {
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&serde_json::to_string(payload)?);
    prepared.commit()?;
    Ok(())
}

/// The file in the index directory that names the generation in use.
const CURRENT: &str = "CURRENT";
/// The generation `CURRENT` names, opened, with its payload; `None` when there is none.
fn open_current(dir: &Path) -> Result<Option<(Index, Payload)>> {
    let name = match std::fs::read_to_string(dir.join(CURRENT)) {
        Ok(name) => name.trim().to_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(generation_name(&name), "CURRENT names no generation");
    let index = Index::open_in_dir(dir.join(&name))?;
    register(&index);
    anyhow::ensure!(index.schema() == schema().0, "another schema");
    let payload = serde_json::from_str(
        index
            .load_metas()?
            .payload
            .as_deref()
            .context("no payload")?,
    )?;
    // Every segment must read: a missing or damaged file fails here, not on a search.
    let reader = index.reader()?;
    for segment in reader.searcher().segment_readers() {
        segment.get_store_reader(0)?;
    }
    Ok(Some((index, payload)))
}
fn generation_name(name: &str) -> bool {
    name.strip_prefix("gen-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
/// Build a new generation in `dir` from the database, switch `CURRENT` to it and remove the
/// other generations. Returns the index, its boundary and its units.
async fn build_generation(
    pool: &PgPool,
    dir: &Path,
    catalogues: &[String],
    progress: impl FnMut(u64) + Send,
) -> Result<(Index, u64, u64, BTreeMap<String, String>)> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // A generation `CURRENT` does not name is left from a build that did not finish.
    let current = std::fs::read_to_string(dir.join(CURRENT)).unwrap_or_default();
    remove_generations(dir, current.trim());
    let name = format!(
        "gen-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
    );
    let path = dir.join(&name);
    std::fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
    let built = async {
        let index = Index::create_in_dir(&path, schema().0)?;
        register(&index);
        let filled = fill(pool, &index, REBUILD_THREADS, catalogues, progress).await?;
        anyhow::Ok(filled)
    }
    .await;
    let (boundary, units, markers) = match built {
        Ok(built) => built,
        Err(error) => {
            // No partial generation stays on the disk.
            let _ = std::fs::remove_dir_all(&path);
            return Err(error);
        }
    };
    // Switch: write the new name beside CURRENT, then move it over (one rename).
    let next = dir.join(format!("{CURRENT}.next"));
    std::fs::write(&next, format!("{name}\n"))?;
    std::fs::File::open(&next)?.sync_all()?;
    std::fs::rename(&next, dir.join(CURRENT))?;
    remove_generations(dir, &name);
    let index = Index::open_in_dir(&path)?;
    register(&index);
    Ok((index, boundary, units, markers))
}
/// Remove every generation in `dir` but `keep`.
fn remove_generations(dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let other = entry.file_name().to_string_lossy().into_owned();
        if other != keep && generation_name(&other) && entry.path().is_dir() {
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(error = %crate::resolver::log_error(&error), generation = other, "an old text index generation was not removed");
            }
        }
    }
}
/// Every unit into the empty `index` from one snapshot, committed with the snapshot's
/// boundary. Returns the boundary and the units written.
async fn fill(
    pool: &PgPool,
    index: &Index,
    threads: usize,
    catalogues: &[String],
    mut progress: impl FnMut(u64) + Send,
) -> Result<(u64, u64, BTreeMap<String, String>)> {
    let fields = schema().1;
    let writer: Arc<IndexWriter> =
        Arc::new(index.writer_with_num_threads(threads, threads * WRITER_BYTES)?);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let boundary: String =
        sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
            .fetch_one(&mut *tx)
            .await?;
    let boundary: u64 = boundary.parse().context("transaction ID")?;
    let markers = import_markers(&mut *tx, catalogues).await?;
    let database = database_identity(&mut *tx).await?;
    let mut units = sqlx::query_as::<_, Unit>(UNITS).fetch(&mut *tx);
    let mut written = 0u64;
    let mut batch = Vec::with_capacity(ADD_BATCH);
    // Documents are added off the async threads: the writer blocks while its threads catch up.
    let add = |batch: Vec<Unit>| {
        let writer = writer.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            for unit in &batch {
                writer.add_document(document(&fields, unit))?;
            }
            Ok(())
        })
    };
    while let Some(unit) = units.try_next().await? {
        batch.push(unit);
        written += 1;
        if batch.len() == ADD_BATCH {
            add(std::mem::take(&mut batch)).await??;
        }
        if written.is_multiple_of(PROGRESS_EVERY) {
            progress(written);
        }
    }
    add(batch).await??;
    drop(units);
    // The cards of the catalogues that are on, from the same snapshot.
    let mut cards = sqlx::query_as::<_, CardRow>(CARDS)
        .bind(catalogues)
        .fetch(&mut *tx);
    let mut batch = Vec::with_capacity(ADD_BATCH);
    let add_cards = |batch: Vec<CardRow>| {
        let writer = writer.clone();
        let recorded_markers = markers.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            for card in &batch {
                let import = recorded_markers
                    .get(&card.catalogue)
                    .map_or("", String::as_str);
                if let Some(doc) = card_document(&fields, card, import) {
                    writer.add_document(doc)?;
                }
            }
            Ok(())
        })
    };
    while let Some(card) = cards.try_next().await? {
        batch.push(card);
        written += 1;
        if batch.len() == ADD_BATCH {
            add_cards(std::mem::take(&mut batch)).await??;
        }
        if written.is_multiple_of(PROGRESS_EVERY) {
            progress(written);
        }
    }
    add_cards(batch).await??;
    drop(cards);
    tx.commit().await?;
    let mut writer = Arc::try_unwrap(writer).map_err(|_| anyhow::anyhow!("writer still shared"))?;
    let payload = Payload {
        format: FORMAT,
        boundary,
        catalogues: markers.clone(),
        database,
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        commit(&mut writer, &payload)?;
        writer.wait_merging_threads()?;
        Ok(())
    })
    .await??;
    Ok((boundary, written, markers))
}
/// For each catalogue given, the end of its last full import (empty when none has finished).
async fn import_markers(
    db: impl sqlx::PgExecutor<'_>,
    catalogues: &[String],
) -> Result<BTreeMap<String, String>> {
    let found: Vec<(String, String)> = sqlx::query_as(
        "SELECT catalogue, coalesce(last_import_at::text,'') FROM catalogue_state WHERE catalogue=ANY($1)",
    )
    .bind(catalogues)
    .fetch_all(db)
    .await?;
    let mut markers: BTreeMap<String, String> = catalogues
        .iter()
        .map(|c| (c.clone(), String::new()))
        .collect();
    markers.extend(found);
    Ok(markers)
}
/// `pinakesd reindex`: build the index in `dir` from the database into a new generation and
/// switch to it; `progress` hears the units written so far. Returns the units written.
pub async fn reindex(
    pool: &PgPool,
    dir: &Path,
    catalogues: &[String],
    progress: impl FnMut(u64) + Send,
) -> Result<u64> {
    let (_, _, units, _) = build_generation(pool, dir, catalogues, progress).await?;
    Ok(units)
}

/// How a text search's candidates are ordered, so that the best ones for the page's own order
/// are the ones kept.
#[derive(Clone, Copy, Debug)]
pub enum TextOrder {
    /// Text relevance times (1 + fame), plus [`ACTIVITY_WEIGHT`] times the activity score.
    Relevance,
    /// The activity score.
    Activity,
    /// The newest reference first.
    Recent,
    /// The earliest start first.
    Start,
    /// The nearest to this point first (latitude, longitude).
    Distance(f64, f64),
}
/// A text search: its text, its order, the instant activity decays to, and its filters.
#[derive(Clone, Debug, Default)]
pub struct TextQuery {
    pub text: String,
    pub order: Option<TextOrder>,
    pub at: DateTime<Utc>,
    /// Any of these types, each with its subtypes.
    pub types: Vec<String>,
    /// None of these types, nor their subtypes; a unit with no type is left out too.
    pub exclude_types: Vec<String>,
    /// People's tags: any of them, or every one with `all_tags`.
    pub tags: Vec<String>,
    pub all_tags: bool,
    pub hosts: Vec<String>,
    pub category: Option<String>,
    pub venue: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Units with a start only.
    pub started: bool,
    /// A circle: latitude, longitude, radius in metres (the exact distance is Postgres').
    pub near: Option<(f64, f64, f64)>,
    /// Boxes (west, south, east, north), any of them.
    pub boxes: Vec<[f64; 4]>,
    /// Units with a reference newer than this.
    pub referenced_since: Option<DateTime<Utc>>,
    /// Referenced units only.
    pub referenced: bool,
    /// The catalogues whose cards may be listed (switched on, and the search lets catalogue
    /// cards in); empty: referenced units only.
    pub catalogues: Vec<String>,
    /// Leave out job postings that ended by this instant (whole seconds before it, so the index
    /// never leaves out one Postgres keeps).
    pub open_jobs_at: Option<DateTime<Utc>>,
    /// Whether the text may match inside a name's words or a similar name, as Postgres' `ILIKE`
    /// and `%`: not for a short text of letters and digits, which matches only where words start.
    pub inside_words: bool,
}
/// A text search's candidates, and the index revision they were read at (for the page's ETag).
#[derive(Clone, Debug)]
pub struct Candidates {
    pub units: Vec<Candidate>,
    pub revision: u64,
}
/// One candidate: its unit, its resource, and its text relevance times (1 + fame).
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub unit_id: String,
    pub resource_id: String,
    pub relevance: f64,
    /// Found by its words (as the index reads them); `false`: proposed by the text inside its
    /// name or its name's pieces, for Postgres to check as it matches text.
    pub words: bool,
    /// A catalogue card's ID (`catalogue_card.id`); `None` for a referenced unit.
    pub card: Option<i64>,
}

/// One item of the text: words to find (one word, or several as a phrase), with its sign.
#[derive(Debug, PartialEq)]
enum Item {
    /// Words, and whether the word was written with letters and digits alone (`c++` is not), so
    /// it may be the start of a longer one.
    Words(Vec<String>, bool),
    Not(Vec<String>),
    /// `or` between the items around it.
    Or,
}
/// The text's items as Postgres' `websearch_to_tsquery` reads them: words, `"quoted phrases"`,
/// `-word` to leave out, and `or` between two items. Each word is split and folded as the index
/// splits the fields; a word that is several tokens (`pinakes-test`) is a phrase.
fn items(index: &Index, text: &str) -> Vec<Item> {
    let mut out = vec![];
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| !c.is_whitespace()) {
        rest = &rest[start..];
        let negated = rest.starts_with('-');
        if negated {
            rest = &rest[1..];
        }
        let (raw, quoted) = if let Some(inner) = rest.strip_prefix('"') {
            let end = inner.find('"').unwrap_or(inner.len());
            let raw = &inner[..end];
            rest = inner.get(end + 1..).unwrap_or("");
            (raw, true)
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let raw = &rest[..end];
            rest = &rest[end..];
            (raw, false)
        };
        if !quoted && !negated && raw.eq_ignore_ascii_case("or") {
            if !matches!(out.last(), None | Some(Item::Or)) {
                out.push(Item::Or);
            }
            continue;
        }
        let words = tokens(index, WORDS, raw);
        if !words.is_empty() {
            let whole = !quoted
                && raw.chars().all(|c| {
                    crate::api::word_char(c)
                        || (crate::api::unspaced_script(c) && c.is_alphanumeric())
                });
            out.push(if negated {
                Item::Not(words)
            } else {
                Item::Words(words, whole)
            });
        }
    }
    while matches!(out.last(), Some(Item::Or)) {
        out.pop();
    }
    out
}

impl Open {
    fn search(&self, q: &TextQuery, cap: usize) -> Result<Vec<Candidate>> {
        let searcher = self.reader.searcher();
        let order = q.order.unwrap_or(TextOrder::Relevance);
        let now = q.at.timestamp() as f64;
        let items = items(&self.index, &q.text);
        let filters = self.filters(q);
        let words = self.words_query(&q.text, &items, &filters);
        let found = |query: &dyn Query, words: bool| -> Result<Vec<Candidate>> {
            searcher
                .search(query, &ranked(cap, order, now))?
                .into_iter()
                .map(|((_, relevance), address)| {
                    let doc: TantivyDocument = searcher.doc(address)?;
                    let text = |field| {
                        doc.get_first(field)
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .context("stored ID")
                    };
                    let resource_id = text(self.fields.resource_id)?;
                    let card = resource_id
                        .strip_prefix("catalogue:")
                        .and_then(|id| id.parse().ok());
                    Ok(Candidate {
                        unit_id: text(self.fields.unit_id)?,
                        resource_id,
                        relevance,
                        words,
                        card,
                    })
                })
                .collect()
        };
        let mut out = match &words {
            Some(query) => found(query.as_ref(), true)?,
            None => vec![],
        };
        // The text inside a name and similar names, as Postgres finds them, for a text that is
        // not short: proposed, and checked there; only while fewer matches than the candidates
        // asked for (`cap`) hold every word in their name, in any order and on any page: with
        // that many, the candidates are those names already.
        let few = match self.name_words_query(&items, &filters) {
            Some(names) => searcher.search(names.as_ref(), &tantivy::collector::Count)? < cap,
            None => true,
        };
        if (q.inside_words && few) || words.is_none() {
            if let Some(query) = self.proposals(&searcher, &q.text, &filters) {
                self.proposed.fetch_add(1, Ordering::SeqCst);
                let seen: HashSet<String> = out.iter().map(|c| c.unit_id.clone()).collect();
                out.extend(
                    found(query.as_ref(), false)?
                        .into_iter()
                        .filter(|c| !seen.contains(&c.unit_id)),
                );
            }
        }
        Ok(out)
    }
}
/// The best `cap` units in `order`, each with its key and its relevance times (1 + fame).
fn ranked(
    cap: usize,
    order: TextOrder,
    now: f64,
) -> impl tantivy::collector::Collector<Fruit = Vec<((f64, f64), tantivy::DocAddress)>> {
    TopDocs::with_limit(cap.max(1)).tweak_score(move |segment: &SegmentReader| {
        let fast = segment.fast_fields();
        let f64s = |name: &str| fast.f64(name).ok();
        let i64s = |name: &str| fast.i64(name).ok();
        let referenced = fast.u64("referenced").ok();
        let (fame, activity, activity_at, newest, start, lat, lon) = (
            f64s("fame"),
            f64s("activity"),
            i64s("activity_at"),
            i64s("newest_at"),
            i64s("start_at"),
            f64s("lat"),
            f64s("lon"),
        );
        move |doc: DocId, score: Score| -> (f64, f64) {
            let f =
                |c: &Option<tantivy::columnar::Column<f64>>| c.as_ref().and_then(|c| c.first(doc));
            let i =
                |c: &Option<tantivy::columnar::Column<i64>>| c.as_ref().and_then(|c| c.first(doc));
            let relevance = f64::from(score) * (1.0 + f(&fame).unwrap_or(0.0));
            let active = || {
                let age = (now - i(&activity_at).unwrap_or(0) as f64).max(0.0);
                f(&activity).unwrap_or(0.0) * (-std::f64::consts::LN_2 * age / HALF_LIFE_SECS).exp()
            };
            let key = match order {
                TextOrder::Relevance => relevance + ACTIVITY_WEIGHT * active(),
                // A catalogue card nobody referenced: after every referenced unit, the best known
                // first, as without text.
                TextOrder::Activity => match referenced.as_ref().and_then(|c| c.first(doc)) {
                    Some(0) => f(&fame).unwrap_or(0.0) - 1.0,
                    _ => active(),
                },
                TextOrder::Recent => i(&newest).unwrap_or(0) as f64,
                TextOrder::Start => -(i(&start).unwrap_or(i64::MAX) as f64),
                TextOrder::Distance(lat0, lon0) => match (f(&lat), f(&lon)) {
                    (Some(lat), Some(lon)) => -haversine_m(lat0, lon0, lat, lon),
                    _ => f64::NEG_INFINITY,
                },
            };
            (
                if key.is_nan() { f64::NEG_INFINITY } else { key },
                relevance,
            )
        }
    })
}

/// The great-circle distance between two points, in metres (the order of a distance search's
/// candidates; Postgres measures the page's own distances).
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().min(1.0).asin()
}
/// The cells of one of [`CELL_RESOLUTIONS`] (its position) that cover a circle: the finest
/// resolution whose disk of cells around the centre reaches past the radius in at most
/// [`MAX_CELLS`] cells. A ring of hexagons lies at least 1.5 edges further out than the one
/// before; the edge taken is 60% of the resolution's average, below its smallest.
pub fn cover(lat: f64, lon: f64, radius_m: f64) -> Option<(usize, Vec<u64>)> {
    let centre = h3o::LatLng::new(lat, lon).ok()?;
    for (position, resolution) in CELL_RESOLUTIONS.iter().enumerate().rev() {
        let edge = resolution.edge_length_m() * 0.6;
        let k = (radius_m / (1.5 * edge)).ceil() as u64 + 1;
        let cells = 3 * k * (k + 1) + 1;
        if cells as usize <= MAX_CELLS {
            let disk: Vec<u64> = centre
                .to_cell(*resolution)
                .grid_disk_safe(k as u32)
                .map(u64::from)
                .collect();
            return Some((position, disk));
        }
    }
    None
}

impl Open {
    /// The filters the index holds, each a clause every candidate must match without scoring.
    fn filters(&self, q: &TextQuery) -> Vec<(Occur, Box<dyn Query>)> {
        let f = &self.fields;
        let mut out: Vec<(Occur, Box<dyn Query>)> = vec![];
        let must = |query: Box<dyn Query>| -> (Occur, Box<dyn Query>) {
            (Occur::Must, Box::new(ConstScoreQuery::new(query, 0.0)))
        };
        let set = |field: Field, values: &[String]| -> Box<dyn Query> {
            Box::new(TermSetQuery::new(
                values.iter().map(|v| Term::from_field_text(field, v)),
            ))
        };
        let term = |field: Field, value: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, value),
                IndexRecordOption::Basic,
            ))
        };
        // Referenced units, and the cards of the catalogues the search may list.
        let referenced: Box<dyn Query> = Box::new(TermQuery::new(
            Term::from_field_u64(f.referenced, 1),
            IndexRecordOption::Basic,
        ));
        if q.referenced || q.catalogues.is_empty() {
            out.push(must(referenced));
        } else {
            out.push(must(Box::new(BooleanQuery::new(vec![
                (Occur::Should, referenced),
                (Occur::Should, set(f.catalogue, &q.catalogues)),
            ]))));
        }
        if !q.types.is_empty() {
            out.push(must(set(f.type_path, &q.types)));
        }
        if !q.exclude_types.is_empty() {
            out.push(must(Box::new(ExistsQuery::new("type".into(), false))));
            out.push((Occur::MustNot, set(f.type_path, &q.exclude_types)));
        }
        // A tag matches people's tags and source labels alike.
        let either = |tags: &[String]| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(vec![
                (Occur::Should, set(f.tag, tags)),
                (Occur::Should, set(f.label, tags)),
            ]))
        };
        if q.all_tags {
            for tag in &q.tags {
                out.push(must(either(std::slice::from_ref(tag))));
            }
        } else if !q.tags.is_empty() {
            out.push(must(either(&q.tags)));
        }
        if !q.hosts.is_empty() {
            out.push(must(set(f.host, &q.hosts)));
        }
        if let Some(category) = &q.category {
            out.push(must(term(f.category, category)));
        }
        if let Some(venue) = &q.venue {
            out.push(must(term(f.venue, venue)));
        }
        let i64_range = |field: Field, from: Option<i64>, to: Option<i64>| -> Box<dyn Query> {
            Box::new(RangeQuery::new(
                Bound::Included(Term::from_field_i64(field, from.unwrap_or(i64::MIN))),
                Bound::Included(Term::from_field_i64(field, to.unwrap_or(i64::MAX))),
            ))
        };
        if q.started || q.from.is_some() || q.to.is_some() {
            // Whole seconds that hold the bounds: Postgres compares the exact instants.
            out.push(must(i64_range(
                f.start_at,
                q.from.map(|t| t.timestamp() - 1),
                q.to.map(|t| t.timestamp() + 1),
            )));
        }
        if let Some(at) = q.open_jobs_at {
            out.push((
                Occur::MustNot,
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, term(f.type_path, "JobPosting")),
                    (
                        Occur::Must,
                        i64_range(f.end_at, None, Some(at.timestamp() - 1)),
                    ),
                ])),
            ));
        }
        if let Some(since) = q.referenced_since {
            out.push(must(i64_range(
                f.newest_at,
                Some(since.timestamp() - 1),
                None,
            )));
        }
        if let Some((lat, lon, radius)) = q.near {
            match cover(lat, lon, radius) {
                Some((position, cells)) => {
                    let field = f.cells[position];
                    out.push(must(Box::new(TermSetQuery::new(
                        cells.into_iter().map(|c| Term::from_field_u64(field, c)),
                    ))));
                }
                None => out.push(must(Box::new(ExistsQuery::new("lat".into(), false)))),
            }
        }
        if !q.boxes.is_empty() {
            let f64_range = |field: Field, low: f64, high: f64| -> Box<dyn Query> {
                Box::new(RangeQuery::new(
                    Bound::Included(Term::from_field_f64(field, low)),
                    Bound::Included(Term::from_field_f64(field, high)),
                ))
            };
            let boxes = q
                .boxes
                .iter()
                .map(|[west, south, east, north]| -> (Occur, Box<dyn Query>) {
                    (
                        Occur::Should,
                        Box::new(BooleanQuery::new(vec![
                            (Occur::Must, f64_range(f.lat, *south, *north)),
                            (Occur::Must, f64_range(f.lon, *west, *east)),
                        ])),
                    )
                })
                .collect();
            out.push(must(Box::new(BooleanQuery::new(boxes))));
        }
        out
    }
    /// The words of one item in one field, as a word or a phrase.
    fn words_in(&self, field: Field, words: &[String]) -> Box<dyn Query> {
        let terms: Vec<Term> = words
            .iter()
            .map(|w| Term::from_field_text(field, w))
            .collect();
        if terms.len() == 1 {
            Box::new(TermQuery::new(
                terms.into_iter().next().expect("one term"),
                IndexRecordOption::WithFreqs,
            ))
        } else {
            Box::new(PhraseQuery::new(terms))
        }
    }
    /// One item where it may match: the name, other names or the description; as the last item,
    /// also the start of a word in the name. People's tags and source labels holding it add to
    /// its score but never match alone.
    fn item_query(&self, words: &[String], last: bool) -> Box<dyn Query> {
        let f = &self.fields;
        let boosted = |field: Field, boost: Score| -> (Occur, Box<dyn Query>) {
            (
                Occur::Should,
                Box::new(BoostQuery::new(self.words_in(field, words), boost)),
            )
        };
        let mut matches: Vec<(Occur, Box<dyn Query>)> = vec![
            boosted(f.name, NAME_BOOST),
            boosted(f.other_names, OTHER_NAMES_BOOST),
            boosted(f.description, DESCRIPTION_BOOST),
        ];
        if let ([word], true) = (words, last) {
            for field in [f.name, f.other_names] {
                let prefix =
                    FuzzyTermQuery::new_prefix(Term::from_field_text(field, word), 0, false);
                matches.push((
                    Occur::Should,
                    Box::new(ConstScoreQuery::new(Box::new(prefix), PREFIX_SCORE)),
                ));
            }
        }
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, Box::new(BooleanQuery::new(matches))),
            boosted(f.tags_text, TAGS_BOOST),
            boosted(f.labels_text, LABELS_BOOST),
        ]))
    }
    /// The whole text inside the name (`ILIKE '%text%'`): every three characters of it (two for
    /// a text of two), across words; one character is any name holding it.
    fn substring(&self, text: &str) -> Option<Box<dyn Query>> {
        let f = &self.fields;
        let length = text.chars().count();
        let query: Box<dyn Query> = match length {
            0 => return None,
            1 => {
                let folded = tokens(&self.index, EXACT, text).pop()?;
                let pattern = format!(".*{}.*", regex_escape(&folded));
                Box::new(RegexQuery::from_pattern(&pattern, f.name_grams).ok()?)
            }
            _ => {
                let grams = tokens(&self.index, if length == 2 { GRAMS2 } else { GRAMS3 }, text);
                if grams.is_empty() {
                    return None;
                }
                Box::new(BooleanQuery::new(
                    grams
                        .into_iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .map(|g| -> (Occur, Box<dyn Query>) {
                            (
                                Occur::Must,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(f.name_grams, &g),
                                    IndexRecordOption::Basic,
                                )),
                            )
                        })
                        .collect(),
                ))
            }
        };
        Some(Box::new(ConstScoreQuery::new(query, SUBSTRING_SCORE)))
    }
    /// The text as a query of its words: every item (an `or` joins the items around it), none of
    /// the items to leave out, the filters; scored by the items' fields, more when the name is the
    /// whole text or holds its words in order. `None` for a text without words.
    fn words_query(
        &self,
        text: &str,
        items: &[Item],
        filters: &[(Occur, Box<dyn Query>)],
    ) -> Option<Box<dyn Query>> {
        let f = &self.fields;
        let last_words = items.iter().rposition(|i| matches!(i, Item::Words(..)))?;
        // Items joined by `or` form one group; every group must match.
        let mut groups: Vec<Vec<Box<dyn Query>>> = vec![];
        let mut joined = false;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![];
        for (position, item) in items.iter().enumerate() {
            match item {
                Item::Or => joined = true,
                Item::Words(words, whole) => {
                    let query = self.item_query(words, position == last_words && *whole);
                    match (joined, groups.last_mut()) {
                        (true, Some(group)) => group.push(query),
                        _ => groups.push(vec![query]),
                    }
                    joined = false;
                }
                Item::Not(words) => {
                    clauses.push((Occur::MustNot, self.item_query(words, false)));
                    joined = false;
                }
            }
        }
        for group in groups {
            let query: Box<dyn Query> = if group.len() == 1 {
                group.into_iter().next().expect("one")
            } else {
                Box::new(BooleanQuery::new(
                    group.into_iter().map(|q| (Occur::Should, q)).collect(),
                ))
            };
            clauses.push((Occur::Must, query));
        }
        // The name is the whole text, or holds its words in order.
        if let Some(exact) = tokens(&self.index, EXACT, &squashed(text)).pop() {
            clauses.push((
                Occur::Should,
                Box::new(ConstScoreQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(f.name_exact, &exact),
                        IndexRecordOption::Basic,
                    )),
                    EXACT_NAME_SCORE,
                )),
            ));
        }
        let plain: Vec<&String> = items
            .iter()
            .filter_map(|i| match i {
                Item::Words(w, _) => Some(w),
                _ => None,
            })
            .flatten()
            .collect();
        if plain.len() > 1 && !items.iter().any(|i| matches!(i, Item::Or)) {
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(PhraseQuery::new(
                        plain
                            .iter()
                            .map(|w| Term::from_field_text(f.name, w))
                            .collect(),
                    )),
                    NAME_PHRASE_BOOST,
                )),
            ));
        }
        clauses.extend(filters.iter().map(|(o, q)| (*o, q.box_clone())));
        Some(Box::new(BooleanQuery::new(clauses)))
    }
    /// The units whose name (or other names) holds every word of the text, with the filters; `None`
    /// when the text has `or` (it is not one set of words).
    fn name_words_query(
        &self,
        items: &[Item],
        filters: &[(Occur, Box<dyn Query>)],
    ) -> Option<Box<dyn Query>> {
        let f = &self.fields;
        if items.iter().any(|i| matches!(i, Item::Or)) {
            return None;
        }
        let in_name = |words: &[String]| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(vec![
                (Occur::Should, self.words_in(f.name, words)),
                (Occur::Should, self.words_in(f.other_names, words)),
            ]))
        };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = items
            .iter()
            .filter_map(|i| match i {
                Item::Words(words, _) => Some((Occur::Must, in_name(words))),
                Item::Not(words) => Some((Occur::MustNot, in_name(words))),
                Item::Or => None,
            })
            .collect();
        if !clauses.iter().any(|(o, _)| *o == Occur::Must) {
            return None;
        }
        clauses.extend(filters.iter().map(|(o, q)| (*o, q.box_clone())));
        Some(Box::new(BooleanQuery::new(clauses)))
    }
    /// Names Postgres may find the text in otherwise: the text inside the name, or a name that
    /// shares at least 0.3 of the text's three-letter pieces (every name `pg_trgm` takes
    /// for similar, and some it does not), with the filters. Postgres checks each.
    fn proposals(
        &self,
        searcher: &tantivy::Searcher,
        text: &str,
        filters: &[(Occur, Box<dyn Query>)],
    ) -> Option<Box<dyn Query>> {
        let f = &self.fields;
        let mut either: Vec<(Occur, Box<dyn Query>)> = vec![];
        if let Some(substring) = self.substring(text.trim()) {
            either.push((Occur::Should, substring));
        }
        let all = pieces(text);
        // A piece in more than COMMON_PIECE of the names (and COMMON_PIECE_DOCS) is left out and
        // the count asked for lowered by one each, to one at least: a similar name still meets it
        // with the rest, and the query reads only the rarer pieces' lists.
        let names = searcher.num_docs().max(1) as f64;
        let pieces: Vec<&String> = all
            .iter()
            .filter(|p| {
                searcher
                    .doc_freq(&Term::from_field_text(f.name_pieces, p))
                    .map_or(true, |n| {
                        n < COMMON_PIECE_DOCS || (n as f64) / names <= COMMON_PIECE
                    })
            })
            .collect();
        let least = ((all.len() as f64 * SIMILARITY).ceil() as usize)
            .saturating_sub(all.len() - pieces.len())
            .max(1);
        if !pieces.is_empty() {
            let similar = BooleanQuery::with_minimum_required_clauses(
                pieces
                    .iter()
                    .map(|p| -> (Occur, Box<dyn Query>) {
                        (
                            Occur::Should,
                            Box::new(ConstScoreQuery::new(
                                Box::new(TermQuery::new(
                                    Term::from_field_text(f.name_pieces, p),
                                    IndexRecordOption::Basic,
                                )),
                                SIMILAR_PIECE_SCORE,
                            )),
                        )
                    })
                    .collect(),
                least,
            );
            either.push((Occur::Should, Box::new(similar)));
        }
        if either.is_empty() {
            return None;
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> =
            vec![(Occur::Must, Box::new(BooleanQuery::new(either)))];
        clauses.extend(filters.iter().map(|(o, q)| (*o, q.box_clone())));
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}
/// `text` as a literal in a regular expression.
fn regex_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if !c.is_alphanumeric() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let index = Index::create_in_ram(schema().0);
        register(&index);
        index
    }
    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_text_reads_as_websearch_does() {
        let index = index();
        assert_eq!(
            items(
                &index,
                r#"Zürich "daft punk" -live or acoustic pinakes-test"#
            ),
            vec![
                Item::Words(words(&["zurich"]), true),
                Item::Words(words(&["daft", "punk"]), false),
                Item::Not(words(&["live"])),
                Item::Or,
                Item::Words(words(&["acoustic"]), true),
                Item::Words(words(&["pinakes", "test"]), false),
            ]
        );
        assert_eq!(
            items(&index, "or OR c++ or"),
            vec![Item::Words(words(&["c"]), false)]
        );
        assert!(items(&index, "!!! --").is_empty());
        assert_eq!(
            items(&index, "İzmir"),
            vec![Item::Words(words(&["izmir"]), true)]
        );
        // A script written without spaces: one word per character, a Latin word apart.
        assert_eq!(
            items(&index, "我的AI助手 x²zqy"),
            vec![
                Item::Words(words(&["我", "的", "ai", "助", "手"]), true),
                Item::Words(words(&["x2zqy"]), true),
            ]
        );
    }

    /// Random text of every kind of character the parser treats apart (quotes, `-`, signs,
    /// letters with and without accents, combining marks, scripts without spaces, emoji): the
    /// parser never panics, and the word tokenizer splits exactly as `api::word_char` says, each
    /// character of a script without spaces a word of its own.
    #[test]
    fn the_parser_and_the_tokenizer_take_any_text() {
        let palette: Vec<char> = "aZ09 \"-_+.,/:\u{301}éİßØ东京タ我AIภาษาעבריתالعربية😀™・\t\n²"
            .chars()
            .collect();
        let index = index();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..5000 {
            let length = (next() % 24) as usize;
            let text: String = (0..length)
                .map(|_| palette[(next() % palette.len() as u64) as usize])
                .collect();
            let _ = items(&index, &text);
            let _ = pieces(&text);
            // The reference split.
            let mut expected = vec![];
            let mut word = String::new();
            for c in text.chars() {
                if crate::api::unspaced_script(c) && c.is_alphanumeric() {
                    if !word.is_empty() {
                        expected.push(std::mem::take(&mut word));
                    }
                    expected.push(c.to_string());
                } else if crate::api::word_char(c) {
                    word.push(c);
                } else if !word.is_empty() {
                    expected.push(std::mem::take(&mut word));
                }
            }
            if !word.is_empty() {
                expected.push(word);
            }
            let expected: Vec<String> = expected
                .iter()
                .map(|w| {
                    w.chars()
                        .flat_map(char::to_lowercase)
                        .filter(|c| !matches!(u32::from(*c), 0x0300..=0x036F))
                        .collect::<String>()
                })
                .filter(|w| !w.is_empty())
                .collect();
            let mut tokenizer = WordTokenizer::default();
            let mut stream = tokenizer.token_stream(&text);
            let mut got = vec![];
            while stream.advance() {
                got.push(stream.token().text.clone());
            }
            assert_eq!(got, expected, "{text:?}");
        }
    }

    #[test]
    fn pieces_are_pg_trgm_s() {
        assert_eq!(pieces("Cat"), ["  c", " ca", "at ", "cat"]);
        assert_eq!(pieces("a-b"), ["  a", "  b", " a ", " b "]);
        assert!(pieces("!!!").is_empty());
    }

    #[test]
    fn a_circle_is_covered_by_cells_that_hold_it() {
        for (lat, lon, radius) in [
            (47.3769, 8.5417, 500.0),
            (47.3769, 8.5417, 20_000.0),
            (-33.86, 151.2, 200_000.0),
            (64.1, -21.9, 3_000.0),
            (0.0, 179.99, 50_000.0),
        ] {
            let (position, cells) = cover(lat, lon, radius).unwrap();
            assert!(cells.len() <= MAX_CELLS);
            let resolution = CELL_RESOLUTIONS[position];
            let cells: HashSet<u64> = cells.into_iter().collect();
            // Points on the circle's edge, in every direction, lie in a covering cell.
            for step in 0..72 {
                let bearing = (step as f64 * 5.0).to_radians();
                let d = radius / 6_371_008.8;
                let (p, l) = (lat.to_radians(), lon.to_radians());
                let p2 = (p.sin() * d.cos() + p.cos() * d.sin() * bearing.cos()).asin();
                let l2 =
                    l + (bearing.sin() * d.sin() * p.cos()).atan2(d.cos() - p.sin() * p2.sin());
                let mut lon2 = l2.to_degrees();
                if lon2 > 180.0 {
                    lon2 -= 360.0;
                }
                let point = h3o::LatLng::new(p2.to_degrees(), lon2).unwrap();
                assert!(
                    cells.contains(&u64::from(point.to_cell(resolution))),
                    "{lat},{lon} r={radius}: bearing {step}"
                );
            }
        }
    }

    #[test]
    fn distances_are_great_circle_metres() {
        let d = haversine_m(47.3769, 8.5417, 46.9480, 7.4474);
        assert!((d - 95_500.0).abs() < 1_500.0, "{d}");
        assert_eq!(haversine_m(1.0, 2.0, 1.0, 2.0), 0.0);
    }
}
