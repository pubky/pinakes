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
    collections::{BTreeMap, HashMap, HashSet},
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
        BooleanQuery, BoostQuery, ConstScoreQuery, ExistsQuery, Occur, PhraseQuery, Query,
        RangeQuery, TermQuery, TermSetQuery,
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
const FORMAT: u32 = 5;
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
/// The most items (words, phrases, words to leave out) a text's query holds; the rest of the text
/// is read by Postgres alone, which checks every candidate.
const MAX_ITEMS: usize = 32;
/// The most three-letter pieces a similar-name proposal asks for: the rarest, the count needed
/// lowered by one for each left out.
const MAX_PIECES: usize = 64;
/// The most words of a name field a word start expands to, in all: of the first [`PREFIX_SCAN`]
/// words that start so in each segment (in dictionary order), those the most names hold.
const PREFIX_TERMS: usize = 1_000;
/// The most words of a name field starting with a word that are read per segment to choose them.
const PREFIX_SCAN: usize = 20_000;
/// The most postings the words a word start expands to hold, in all (summed over segments).
const PREFIX_POSTINGS: u64 = if cfg!(test) { 2_000 } else { 500_000 };
/// The most documents the rarest word of a phrase may be in for the index to match it as a phrase.
const PHRASE_POSTINGS: u64 = if cfg!(test) { 100 } else { 50_000 };
/// The most words of one phrase the index matches as a phrase; Postgres checks the whole phrase.
const MAX_PHRASE_WORDS: usize = 8;
/// The most removed resources whose merge a commit records ([`Payload::owed`]); past it the next
/// open merges every segment with deleted documents.
const OWED_CAP: usize = 10_000;
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
        unit_id: s.add_text_field("unit_id", STRING | STORED | FAST),
        resource_id: s.add_text_field("resource_id", STRING | STORED | FAST),
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
    /// The removed resources whose words a merge must still take out of the files ([`Purges`]),
    /// recorded with the commit that deleted them, so a restart before the merge finishes merges
    /// them at the next open; cleared by a commit once merged.
    #[serde(default)]
    owed: Vec<String>,
    /// A merge of every segment with deleted documents is owed (a catalogue switched off, or more
    /// owed resources than [`OWED_CAP`]).
    #[serde(default)]
    owed_all: bool,
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
    /// A card built from a catalogue card: the labels it shows, its card's other names and its
    /// card's fame (`fame`: `None` for any other card).
    labels: Vec<String>,
    other_names: Vec<String>,
    fame: Option<f32>,
}
/// The units search can find by text: those with a reference that counts, and never a post whose
/// author has not opted in to search (it is never found by its text). An OSM place's address
/// line counts as its description, as its Postgres text does. A card built from a catalogue card
/// (`resource.catalogue_card`) also has the labels its document shows, and its card's other names
/// and fame: the one unit of that thing, found and ranked as its card would be. Its document holds
/// the card's content hash (`catalogue.hash`), so any change of the card rewrites the document and
/// queues the unit again.
const UNITS: &str = "SELECT r.unit_id, r.id AS resource_id, r.name, r.doc->>'description' AS description, p.display_name AS other, r.type AS type_name, r.type_path, r.host, r.category, r.venue_id, r.resolver, ST_Y(r.point) AS lat, ST_X(r.point) AS lon, r.start_at, r.end_at, s.score, s.updated_at AS scored_at, s.newest_at, ARRAY(SELECT DISTINCT f.label FROM effective_reference f WHERE f.resource_id=r.id AND f.label IS NOT NULL ORDER BY f.label) AS tags, CASE WHEN b.fame IS NULL OR jsonb_typeof(r.doc->'labels')<>'array' THEN '{}'::text[] ELSE ARRAY(SELECT jsonb_array_elements_text(r.doc->'labels')) END AS labels, coalesce(b.other_names,'{}') AS other_names, b.fame FROM search_unit r LEFT JOIN resource_score s ON s.resource_id=r.id LEFT JOIN osm_place p ON p.osm_type=r.osm_type AND p.osm_id=r.osm_id LEFT JOIN LATERAL (SELECT c.fame, c.other_names FROM resource x JOIN catalogue_card c ON c.id=x.catalogue_card WHERE x.id=r.id AND r.occurrence IS NULL) b ON true WHERE coalesce(r.doc->'attributes'->>'opted_out','')<>'true' AND EXISTS (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id)";

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
    if let Some(host) = crate::catalogue::card_host(&card.uri) {
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
    for other in &unit.other_names {
        doc.add_text(f.other_names, other);
    }
    for label in &unit.labels {
        doc.add_text(f.label, label);
        doc.add_text(f.labels_text, label);
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
    doc.add_f64(
        f.fame,
        unit.fame
            .map_or(0.0, |fame| f64::from(fame).clamp(0.0, 1.0)),
    );
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
    /// Merges after a removal ([`Open::purge`]) that finished, for tests.
    purges: AtomicU64,
    /// The merge after a removal under way, and the removals still owed one ([`Purges`]).
    purging: std::sync::Mutex<Purges>,
    /// The thread waiting for the merge under way ([`SearchIndex::settle`]).
    merging: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Searches that read the proposals (the text inside names, similar names), for tests.
    proposed: AtomicU64,
    /// Re-reads of a catalogue begun, for tests.
    rereads: AtomicU64,
    /// Searches that counted the names holding every word (whether to read the proposals), for
    /// tests.
    counted: AtomicU64,
}
/// Sets [`Purges::running`] back to false when dropped, and owes again what the merge under way
/// did not finish (a merge that failed, or whose thread panicked).
struct Running<'a>(&'a std::sync::Mutex<Purges>);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Ok(mut purging) = self.0.lock() {
            purging.running = false;
            let merging = std::mem::take(&mut purging.merging);
            purging.resources.extend(merging);
            purging.catalogues |= std::mem::take(&mut purging.merging_catalogues);
        }
    }
}
/// How many times a merge after a removal is started before it is owed to the next pass.
const PURGE_ATTEMPTS: u32 = 5;
/// Removals whose words must still leave the index files ([`Open::purge`]): the resources whose
/// documents lost their text, and whether a catalogue was switched off (every segment with deleted
/// documents then merges). One merge runs at a time; a removal while it runs, or one whose merge
/// failed, waits for the next pass.
#[derive(Default)]
struct Purges {
    running: bool,
    resources: Vec<String>,
    catalogues: bool,
    /// What the merge under way takes out: owed until it finishes, and recorded as owed by every
    /// commit until then ([`Payload::owed`]).
    merging: Vec<String>,
    merging_catalogues: bool,
}
impl Purges {
    fn owed(&self) -> bool {
        !self.resources.is_empty() || self.catalogues
    }
    /// What a commit records as owed: the merges not started and the one under way.
    fn recorded(&self) -> (Vec<String>, bool) {
        let mut owed: Vec<String> = self
            .resources
            .iter()
            .chain(&self.merging)
            .cloned()
            .collect();
        owed.sort();
        owed.dedup();
        let all = self.catalogues || self.merging_catalogues || owed.len() > OWED_CAP;
        if owed.len() > OWED_CAP {
            owed.clear();
        }
        (owed, all)
    }
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
    /// An open index; `owed`: the merges its last commit recorded as owed ([`Payload::owed`],
    /// [`Payload::owed_all`]), started at the next pass.
    fn new(
        index: Index,
        boundary: u64,
        catalogues: BTreeMap<String, String>,
        database: String,
        owed: (Vec<String>, bool),
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
            purging: std::sync::Mutex::new(Purges {
                resources: owed.0,
                catalogues: owed.1,
                ..Purges::default()
            }),
            merging: std::sync::Mutex::new(None),
            proposed: AtomicU64::new(0),
            rereads: AtomicU64::new(0),
            counted: AtomicU64::new(0),
        })
    }
    /// What the next commit records, with `boundary`.
    fn stamp(&self, boundary: u64) -> Payload {
        let (owed, owed_all) = self.purging.lock().expect("purges").recorded();
        Payload {
            format: FORMAT,
            boundary,
            catalogues: self.catalogues.lock().expect("markers").clone(),
            database: self.database.clone(),
            owed,
            owed_all,
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
    /// The turns of text searches and each one's budget ([`SearchIndex::set_bounds`]).
    bounds: std::sync::RwLock<(Arc<Turns>, Duration)>,
}
/// The address a client's share of turns is counted by: an IPv4 address, or an IPv6 address's
/// /64 (one host's or one network's addresses, which a client may rotate through); an IPv4
/// address written as IPv6 (`::ffff:a.b.c.d`) is that IPv4 address.
pub(crate) fn share_key(client: std::net::IpAddr) -> std::net::IpAddr {
    match client {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => std::net::IpAddr::V4(v4),
            None => std::net::IpAddr::V6(std::net::Ipv6Addr::from(
                u128::from(v6) & !((1u128 << 64) - 1),
            )),
        },
        v4 => v4,
    }
}
/// The turns of text searches ([`SearchIndex::set_bounds`]), shared between clients: a search
/// waits for a free turn; a client past its share (half the turns, at least one) takes one only
/// while no other client under its share waits, so alone it takes every free turn. A waiting
/// search holds nothing.
struct Turns {
    /// The number of turns.
    limit: usize,
    state: std::sync::Mutex<TurnState>,
    /// Wakes the waiting searches when a turn is given back or a waiting search leaves.
    changed: tokio::sync::Notify,
}
#[derive(Default)]
struct TurnState {
    free: usize,
    /// The turns each client holds, by [`share_key`]; `None`: a search of no client.
    running: HashMap<Option<std::net::IpAddr>, usize>,
    /// The searches each client has waiting for a turn.
    waiting: HashMap<Option<std::net::IpAddr>, usize>,
}
impl TurnState {
    /// Whether `client` runs its share: never a search of no client.
    fn at_share(&self, client: Option<std::net::IpAddr>, share: usize) -> bool {
        client.is_some() && self.running.get(&client).copied().unwrap_or(0) >= share
    }
}
/// One turn of a text search, given back when dropped.
pub struct Turn {
    turns: Arc<Turns>,
    client: Option<std::net::IpAddr>,
}
impl Drop for Turn {
    fn drop(&mut self) {
        if let Ok(mut state) = self.turns.state.lock() {
            state.free += 1;
            if let Some(n) = state.running.get_mut(&self.client) {
                *n -= 1;
                if *n == 0 {
                    state.running.remove(&self.client);
                }
            }
        }
        self.turns.changed.notify_waiters();
    }
}
/// A search waiting for a turn, counted while it waits.
struct Waiting<'a> {
    turns: &'a Turns,
    client: Option<std::net::IpAddr>,
}
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.turns.state.lock() {
            if let Some(n) = state.waiting.get_mut(&self.client) {
                *n -= 1;
                if *n == 0 {
                    state.waiting.remove(&self.client);
                }
            }
        }
        // A search under its share leaving may let one past its share take a free turn.
        self.turns.changed.notify_waiters();
    }
}
impl Turns {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: std::sync::Mutex::new(TurnState {
                free: limit,
                ..Default::default()
            }),
            changed: tokio::sync::Notify::new(),
        })
    }
    /// A client's share: half the turns, at least one.
    fn share(&self) -> usize {
        (self.limit / 2).max(1)
    }
    /// A turn for a search of `client`, now when one is free and the client may take it.
    fn try_take(
        self: &Arc<Self>,
        state: &mut TurnState,
        client: Option<std::net::IpAddr>,
    ) -> Option<Turn> {
        let share = self.share();
        let others_under_share = state
            .waiting
            .iter()
            .any(|(other, n)| *other != client && *n > 0 && !state.at_share(*other, share));
        if state.free == 0 || (state.at_share(client, share) && others_under_share) {
            return None;
        }
        state.free -= 1;
        *state.running.entry(client).or_default() += 1;
        Some(Turn {
            turns: self.clone(),
            client,
        })
    }
    /// A turn for a search of `client` (its address), waited for until `deadline`; past it, why
    /// none came: `turn` (every turn taken) or `share` (one free, the client past its share while
    /// others waited).
    async fn take(
        self: &Arc<Self>,
        client: Option<std::net::IpAddr>,
        deadline: tokio::time::Instant,
    ) -> std::result::Result<Turn, &'static str> {
        let client = client.map(share_key);
        {
            let mut state = self.state.lock().expect("turns");
            if let Some(turn) = self.try_take(&mut state, client) {
                return Ok(turn);
            }
            *state.waiting.entry(client).or_default() += 1;
        }
        let _waiting = Waiting {
            turns: self,
            client,
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.state.lock().expect("turns");
                // Its own wait no longer counts against it once a turn is taken.
                if let Some(turn) = self.try_take(&mut state, client) {
                    return Ok(turn);
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let state = self.state.lock().expect("turns");
                return Err(if state.free == 0 { "turn" } else { "share" });
            }
        }
    }
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
    /// The IDs of the documents the index holds now (`catalogue:<id>` for a catalogue card), in
    /// order (tests).
    #[doc(hidden)]
    pub fn documents(&self) -> Vec<String> {
        let Some(open) = self.current() else {
            return vec![];
        };
        let searcher = open.reader.searcher();
        let mut out: Vec<String> = searcher
            .search(
                &tantivy::query::AllQuery,
                &tantivy::collector::DocSetCollector,
            )
            .map(|found| {
                found
                    .into_iter()
                    .filter_map(|address| {
                        let doc: TantivyDocument = searcher.doc(address).ok()?;
                        doc.get_first(open.fields.resource_id)
                            .and_then(|v| v.as_str().map(str::to_owned))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }
    /// How many searches counted the names holding every word (tests).
    #[doc(hidden)]
    pub fn counted(&self) -> u64 {
        self.current()
            .map_or(0, |open| open.counted.load(Ordering::SeqCst))
    }
    /// Merge every segment into one and wait for it (benchmarks: the largest segment a merge after a
    /// removal may have to rewrite).
    #[doc(hidden)]
    pub fn merge_all(&self) -> Result<()> {
        let Some(open) = self.current() else {
            return Ok(());
        };
        let segments = open.index.searchable_segment_ids()?;
        if segments.len() > 1 {
            open.writer
                .lock()
                .expect("writer lock")
                .merge(&segments)
                .wait()?;
        }
        open.reader.reload()?;
        Ok(())
    }
    /// The number of segments the index reads (benchmarks).
    #[doc(hidden)]
    pub fn segments(&self) -> usize {
        self.current()
            .map_or(0, |open| open.reader.searcher().segment_readers().len())
    }
    /// Wait for the merge after a removal under way, if any ([`Open::purge`]; tests).
    #[doc(hidden)]
    pub fn settle(&self) {
        if let Some(open) = self.current() {
            let waiting = open.merging.lock().expect("merging").take();
            if let Some(waiting) = waiting {
                let _ = waiting.join();
            }
        }
    }
    /// How many re-reads of a catalogue began (tests).
    #[doc(hidden)]
    pub fn rereads(&self) -> u64 {
        self.current()
            .map_or(0, |open| open.rereads.load(Ordering::SeqCst))
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
            // The daemon sets its own (`limits.search_index_*`); an index in memory (tests, the
            // e2e harness) waits long, so a loaded host answers no test with a 503.
            bounds: std::sync::RwLock::new((
                Turns::new(std::thread::available_parallelism().map_or(2, |n| n.get())),
                Duration::from_secs(30),
            )),
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
    /// `passed` is set to the time the index opens and of each pass after that read the queue
    /// without an error, `open` to 1 once the index is open.
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
                    // The last pass counts from the open: passes that fail from here on stall it.
                    passed.set(chrono::Utc::now().timestamp());
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
                let owed = (payload.owed, payload.owed_all);
                if !owed.0.is_empty() || owed.1 {
                    tracing::info!(
                        resources = owed.0.len(),
                        all = owed.1,
                        "the text index owes merges after removals from before its restart; they run at the next pass"
                    );
                }
                self.set(Open::new(
                    index,
                    payload.boundary,
                    payload.catalogues,
                    database,
                    owed,
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
        self.set(Open::new(
            index,
            boundary,
            markers,
            database,
            Default::default(),
        )?);
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
            self.set(Open::new(
                index,
                boundary,
                markers,
                database,
                Default::default(),
            )?);
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
        // A merge after a removal that failed, or was owed while another ran.
        if open.purge_owed() {
            let owing = open.clone();
            tokio::task::spawn_blocking(move || {
                let mut writer = owing.writer.lock().expect("writer lock");
                owing.purge(&mut writer, vec![], false)
            })
            .await??;
        }
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
                    recorded.contains_key(c) || open.has_documents(&searcher, c).unwrap_or(true)
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
        // One part of a re-read: the one under way, else a catalogue whose first full import
        // finished since the index read it. A later full import's inserted, changed and removed
        // cards come through the queue (`search_index_catalogue_queue`), so only its end is
        // recorded: the catalogue is not read again.
        let current = open.reread.lock().expect("reread").clone();
        let reread = match current {
            Some(reread) if enabled.contains(&reread.catalogue) => Some(reread),
            _ => {
                let markers = import_markers(pool, &enabled).await?;
                let mut reread = None;
                for c in enabled
                    .iter()
                    .filter(|c| recorded.get(*c) != markers.get(*c))
                {
                    let marker = markers.get(c).cloned().unwrap_or_default();
                    // A later import (its generation above 1) of a catalogue the index read
                    // after an import before: its cards came through the queue.
                    let read_before = recorded
                        .get(c)
                        .is_some_and(|m| marker_generation(m).is_some());
                    let later = marker_generation(&marker).is_some_and(|g| g > 1);
                    if read_before && later {
                        let recording = open.clone();
                        let (catalogue, at) = (c.clone(), marker.clone());
                        tokio::task::spawn_blocking(move || {
                            recording.record_marker(&catalogue, &at)
                        })
                        .await??;
                        tracing::info!(catalogue = %c, "a catalogue's later full import reached the text index through its queue");
                    } else if reread.is_none() {
                        open.rereads.fetch_add(1, Ordering::SeqCst);
                        tracing::info!(catalogue = %c, "the text index reads a catalogue's cards again after its first full import");
                        reread = Some(Reread {
                            catalogue: c.clone(),
                            marker,
                            after: String::new(),
                        });
                    }
                }
                reread
            }
        };
        *open.reread.lock().expect("reread") = reread.clone();
        let Some(reread) = reread else {
            crate::telemetry::search_index_reread(None, 0);
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
        let read = cards.len() as u64;
        let rewriting = open.clone();
        let part = reread.clone();
        tokio::task::spawn_blocking(move || rewriting.reread_part(&part, &cards, done)).await??;
        self.revision.fetch_add(1, Ordering::SeqCst);
        crate::telemetry::search_index_reread(Some(&reread.catalogue), read);
        if done {
            tracing::info!(catalogue = %reread.catalogue, "the text index read the catalogue's cards again");
            crate::telemetry::search_index_reread(None, 0);
        }
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
    /// yet or the index leaves the text to Postgres (the search then reads Postgres alone).
    pub async fn candidates(
        &self,
        pool: &PgPool,
        query: TextQuery,
        cap: usize,
    ) -> Result<Option<Candidates>> {
        Ok(self
            .candidates_for(pool, query, cap, None)
            .await?
            .candidates)
    }
    /// [`SearchIndex::candidates`] for a search by `client` (its address), which shares the turns
    /// with the others ([`Turns`]): a search that gets no turn within its budget, or runs past
    /// it, answers like a busy index ([`Busy`]). A text the index leaves to Postgres (one
    /// character found inside names) keeps its turn while Postgres reads it ([`Found::turn`]).
    pub async fn candidates_for(
        &self,
        pool: &PgPool,
        query: TextQuery,
        cap: usize,
        client: Option<std::net::IpAddr>,
    ) -> Result<Found> {
        if let Home::Memory = self.home {
            self.sync(pool).await?;
        }
        // Read before the search: a commit during it moves the revision past this page's.
        let revision = self.revision();
        let Some(open) = self.current() else {
            return Ok(Found::default());
        };
        // At most `limits.search_index_searches` searches run at once, each within
        // `limits.search_index_budget_ms`, its wait for a turn included. The turn goes with the
        // search into its thread: a search whose request is gone still holds it until it stops.
        let (turns, budget) = self.bounds();
        let deadline = std::time::Instant::now() + budget;
        let turn = turns
            .take(client, deadline.into())
            .await
            .map_err(|reason| {
                crate::telemetry::search_index_busy(reason);
                Busy
            })?;
        let budget = Budget::new(deadline);
        let (turn, units) = tokio::task::spawn_blocking(move || {
            let units = open.search(&query, cap, &budget);
            (turn, units)
        })
        .await?;
        let units = match units {
            Ok(Some(units)) => units,
            Ok(None) => {
                return Ok(Found {
                    candidates: None,
                    turn: Some(turn),
                    deadline: Some(deadline),
                })
            }
            Err(error) => {
                if error.is::<Busy>() {
                    crate::telemetry::search_index_busy("budget");
                }
                return Err(error);
            }
        };
        drop(turn);
        self.served.fetch_add(1, Ordering::SeqCst);
        Ok(Found {
            candidates: Some(Candidates { units, revision }),
            turn: None,
            deadline: None,
        })
    }
    /// Take one turn of the text searches for `client` (its address, counted as [`share_key`]
    /// counts it) now, whatever its share, and keep it until the guard is dropped (tests).
    #[doc(hidden)]
    pub fn hold_turn_for(&self, client: std::net::IpAddr) -> Option<Turn> {
        self.hold(Some(share_key(client)))
    }
    /// Take one turn of the text searches now and keep it until the guard is dropped (tests: a busy
    /// index).
    #[doc(hidden)]
    pub fn hold_turn(&self) -> Option<Turn> {
        self.hold(None)
    }
    fn hold(&self, client: Option<std::net::IpAddr>) -> Option<Turn> {
        let turns = self.bounds().0;
        let mut state = turns.state.lock().expect("turns");
        if state.free == 0 {
            return None;
        }
        state.free -= 1;
        *state.running.entry(client).or_default() += 1;
        drop(state);
        Some(Turn { turns, client })
    }
    /// Run at most `searches` text searches at once (0: one per CPU core), each within `budget`
    /// (`limits.search_index_searches`, `limits.search_index_budget_ms`).
    pub fn set_bounds(&self, searches: usize, budget: Duration) {
        let searches = match searches {
            0 => std::thread::available_parallelism().map_or(2, |n| n.get()),
            n => n,
        };
        *self.bounds.write().expect("bounds") = (Turns::new(searches), budget);
    }
    fn bounds(&self) -> (Arc<Turns>, Duration) {
        let (turns, budget) = &*self.bounds.read().expect("bounds");
        (turns.clone(), *budget)
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
    fn replace(
        self: &Arc<Self>,
        resources: &[String],
        units: &[Unit],
        boundary: u64,
    ) -> Result<()> {
        // The resources whose document had text and now has none (a post found gone, emptied or
        // opted out, a card no longer searchable): a merge of the segments that hold their
        // documents is owed, so their words leave the index files, not only its searches. Owed
        // before the commit that deletes them, so that commit records it ([`Payload::owed`]) and
        // a restart before the merge still merges them. A pass that only adds or changes
        // documents owes nothing.
        let texts = self.text_documents(resources)?;
        let lost: Vec<String> = texts
            .iter()
            .filter(|(id, _)| !units.iter().any(|u| &u.resource_id == id && has_text(u)))
            .map(|(id, _)| id.clone())
            .collect();
        let mut writer = self.writer.lock().expect("writer lock");
        self.owe(&lost);
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
            // Nothing half done stays for the next commit. The merge owed stays owed: it merges
            // nothing the documents' segments do not hold, and taking it back could take back one
            // a merge thread owed again meanwhile.
            let _ = writer.rollback();
        }
        result?;
        self.reader.reload()?;
        self.purge(&mut writer, vec![], false)?;
        Ok(())
    }
    /// Owe a merge for `resources`.
    fn owe(&self, resources: &[String]) {
        self.purging
            .lock()
            .expect("purges")
            .resources
            .extend(resources.iter().cloned());
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
    fn remove_catalogues(self: &Arc<Self>, off: &[String]) -> Result<()> {
        let mut writer = self.writer.lock().expect("writer lock");
        let boundary = committed_boundary(&self.index).unwrap_or(0);
        let had_documents = {
            let searcher = self.reader.searcher();
            off.iter()
                .any(|c| self.has_documents(&searcher, c).unwrap_or(true))
        };
        let before = self.catalogues.lock().expect("markers").clone();
        for catalogue in off {
            writer.delete_term(Term::from_field_text(self.fields.catalogue, catalogue));
            self.catalogues.lock().expect("markers").remove(catalogue);
        }
        // A catalogue that had no documents (never read, or read empty) leaves nothing to merge;
        // one that had owes a merge of every segment with deleted documents, recorded by this
        // commit ([`Payload::owed_all`]).
        // (A commit that fails leaves it owed: a merge too many, never one too few.)
        self.purging.lock().expect("purges").catalogues |= had_documents;
        let result = commit(&mut writer, &self.stamp(boundary));
        if let Err(error) = result {
            *self.catalogues.lock().expect("markers") = before;
            let _ = writer.rollback();
            return Err(error);
        }
        self.reader.reload()?;
        self.purge(&mut writer, vec![], false)?;
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
    /// Record `marker` as the full import of `catalogue` the index holds, without reading its cards
    /// again (its changes came through the queue). One commit.
    fn record_marker(&self, catalogue: &str, marker: &str) -> Result<()> {
        let mut writer = self.writer.lock().expect("writer lock");
        let before = self.catalogues.lock().expect("markers").clone();
        self.catalogues
            .lock()
            .expect("markers")
            .insert(catalogue.to_owned(), marker.to_owned());
        let boundary = committed_boundary(&self.index).unwrap_or(0);
        if let Err(error) = commit(&mut writer, &self.stamp(boundary)) {
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
impl Open {
    /// Owe a merge for the removed documents of `resources` (and of the catalogues switched off,
    /// `catalogues`), so their words leave the index files, not only its searches; then start the
    /// merge owed, unless one runs. It starts under the writer lock (`writer`) but runs and is
    /// waited for outside it: commits and searches go on meanwhile. One that fails is owed again,
    /// for the next pass ([`Open::purge_owed`]).
    fn purge(
        self: &Arc<Self>,
        writer: &mut IndexWriter,
        resources: Vec<String>,
        catalogues: bool,
    ) -> Result<()> {
        let (resources, catalogues) = {
            let mut purging = self.purging.lock().expect("purges");
            purging.resources.extend(resources);
            purging.catalogues |= catalogues;
            if purging.running || !purging.owed() {
                return Ok(());
            }
            purging.running = true;
            let resources = std::mem::take(&mut purging.resources);
            let catalogues = std::mem::take(&mut purging.catalogues);
            purging.merging = resources.clone();
            purging.merging_catalogues = catalogues;
            (resources, catalogues)
        };
        let merged = match self.start_merge(writer, &resources, catalogues) {
            Ok(Some(merged)) => merged,
            done => {
                // Nothing left to merge (the merge policy, or a merge before, took them): a commit
                // records that nothing is owed for them. The segments did not read: owed again.
                let failed = done.is_err();
                {
                    let mut purging = self.purging.lock().expect("purges");
                    purging.running = false;
                    purging.merging.clear();
                    purging.merging_catalogues = false;
                    if failed {
                        purging.resources.extend(resources);
                        purging.catalogues |= catalogues;
                    }
                }
                if !failed {
                    self.record_owed(writer)?;
                }
                return done.map(|_| ());
            }
        };
        let open = self.clone();
        let (owed, owed_catalogues) = (resources.clone(), catalogues);
        let spawned = std::thread::Builder::new()
            .name("text-index-purge".into())
            .spawn(move || {
                // `running` goes back to false however the wait ends (a panic included), so a later
                // pass can merge again.
                let _running = Running(&open.purging);
                open.wait_merge(merged, resources, catalogues)
            });
        let waiting = match spawned {
            Ok(waiting) => waiting,
            Err(error) => {
                let mut purging = self.purging.lock().expect("purges");
                purging.running = false;
                purging.merging.clear();
                purging.merging_catalogues = false;
                purging.resources.extend(owed);
                purging.catalogues |= owed_catalogues;
                return Err(error.into());
            }
        };
        // The earlier thread is done (`running` was clear, which its last action does): its handle
        // is let go, never joined here, under the writer lock.
        *self.merging.lock().expect("merging") = Some(waiting);
        Ok(())
    }
    /// Start merging the committed segments that hold the documents of `resources` (or, with
    /// `catalogues`, every one with deleted documents); `None` when there are none.
    fn start_merge(
        &self,
        writer: &mut IndexWriter,
        resources: &[String],
        catalogues: bool,
    ) -> Result<Option<tantivy::FutureResult<Option<tantivy::index::SegmentMeta>>>> {
        let segments = self.purged_segments(resources, catalogues)?;
        Ok((!segments.is_empty()).then(|| writer.merge(&segments)))
    }
    /// Wait, outside the writer lock, for the merge `merged` that [`Open::purge`] started. The
    /// merge policy may have taken one of its segments meanwhile: then, after a short wait for that
    /// merge, the segments are read again and merged, up to [`PURGE_ATTEMPTS`] times (the writer
    /// lock only to start each); after that the merge is owed to the next pass.
    fn wait_merge(
        &self,
        mut merged: tantivy::FutureResult<Option<tantivy::index::SegmentMeta>>,
        resources: Vec<String>,
        catalogues: bool,
    ) {
        let mut attempt = 1;
        let failed = loop {
            let error = match merged.wait() {
                Ok(_) => break None,
                Err(error) => anyhow::Error::from(error),
            };
            if attempt == PURGE_ATTEMPTS {
                break Some(error);
            }
            std::thread::sleep(std::time::Duration::from_millis(50 << attempt));
            attempt += 1;
            tracing::debug!(error = %crate::resolver::log_error(&error), "a text index merge raced another; reading the segments again");
            let again = self
                .reader
                .reload()
                .map_err(anyhow::Error::from)
                .and_then(|()| {
                    let mut writer = self.writer.lock().expect("writer lock");
                    self.start_merge(&mut writer, &resources, catalogues)
                });
            match again {
                Ok(Some(next)) => merged = next,
                // What took the segments holds the documents no more.
                Ok(None) => break None,
                Err(error) => break Some(error),
            }
        };
        // `running` stays set: the thread's guard clears it as its very last action, so no second
        // merge starts while this one still reloads or re-owes; it owes again what a failed merge
        // took (`Purges::merging`).
        match failed {
            None => {
                if let Err(error) = self.reader.reload() {
                    tracing::debug!(error = %crate::resolver::log_error(&error), "the text index read after a merge failed");
                }
                {
                    let mut purging = self.purging.lock().expect("purges");
                    purging.merging.clear();
                    purging.merging_catalogues = false;
                }
                // A commit records that the merged removals are owed no more.
                let mut writer = self.writer.lock().expect("writer lock");
                if let Err(error) = self.record_owed(&mut writer) {
                    tracing::warn!(error = %crate::resolver::log_error(&error), "the text index did not record a finished merge; it merges again after a restart");
                }
                self.purges.fetch_add(1, Ordering::SeqCst);
            }
            Some(error) => {
                tracing::warn!(resources = resources.len(), all = catalogues, error = %crate::resolver::log_error(&error), "a text index merge after a removal failed; it is tried again at the next pass");
            }
        }
    }
    /// Whether the index holds a live document of `catalogue` (deleted ones, still in the files until
    /// a merge, do not count: a catalogue switched off whose merge runs in the background is not
    /// removed again).
    fn has_documents(&self, searcher: &tantivy::Searcher, catalogue: &str) -> Result<bool> {
        let query = TermQuery::new(
            Term::from_field_text(self.fields.catalogue, catalogue),
            IndexRecordOption::Basic,
        );
        Ok(searcher.search(&query, &tantivy::collector::Count)? > 0)
    }
    /// Commit what is owed now ([`Payload::owed`]), with the boundary of the last commit: a merge
    /// that finished, or found nothing left, is owed no more after a restart.
    fn record_owed(&self, writer: &mut IndexWriter) -> Result<()> {
        let boundary = committed_boundary(&self.index).unwrap_or(0);
        let result = commit(writer, &self.stamp(boundary));
        if result.is_err() {
            let _ = writer.rollback();
        }
        result
    }
    /// Whether a merge is owed and none runs: the pass then starts it ([`Open::purge`]).
    fn purge_owed(&self) -> bool {
        let purging = self.purging.lock().expect("purges");
        !purging.running && purging.owed()
    }
    /// The committed segments that hold a document of `resources`, deleted ones included (their
    /// words are still in the files), or with `catalogues` every segment with deleted documents.
    fn purged_segments(
        &self,
        resources: &[String],
        catalogues: bool,
    ) -> Result<Vec<tantivy::index::SegmentId>> {
        // Read the segments as they are now: one a merge of Tantivy's own replaced since the last
        // read still holds a deleted document's words, under its new ID.
        self.reader.reload()?;
        let committed: HashSet<tantivy::index::SegmentId> =
            self.index.searchable_segment_ids()?.into_iter().collect();
        let mut wanted: Vec<tantivy::index::SegmentId> = if catalogues {
            segments_with_deletes(&self.index)?
        } else {
            vec![]
        };
        if !resources.is_empty() {
            let searcher = self.reader.searcher();
            wanted.extend(
                searcher
                    .segment_readers()
                    .iter()
                    .filter(|segment| {
                        segment
                            .inverted_index(self.fields.resource_id)
                            .ok()
                            .is_some_and(|inverted| {
                                resources.iter().any(|id| {
                                    inverted
                                        .terms()
                                        .get(str::as_bytes(id))
                                        .ok()
                                        .flatten()
                                        .is_some()
                                })
                            })
                    })
                    .map(|segment| segment.segment_id()),
            );
        }
        wanted.retain(|s| committed.contains(s));
        wanted.sort_by_key(|s| s.uuid_string());
        wanted.dedup();
        Ok(wanted)
    }
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
    // A new build holds no deleted documents: no merge is owed.
    let payload = Payload {
        format: FORMAT,
        boundary,
        catalogues: markers.clone(),
        database,
        owed: vec![],
        owed_all: false,
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        commit(&mut writer, &payload)?;
        writer.wait_merging_threads()?;
        Ok(())
    })
    .await??;
    Ok((boundary, written, markers))
}
/// The generation of the full import a marker names ([`import_markers`]); `None` for none (no
/// finished import, or a marker of an index from before the generation was recorded).
fn marker_generation(marker: &str) -> Option<i64> {
    marker.rsplit_once(" #").and_then(|(_, g)| g.parse().ok())
}
/// For each catalogue given, the end of its last full import (empty when none has finished).
async fn import_markers(
    db: impl sqlx::PgExecutor<'_>,
    catalogues: &[String],
) -> Result<BTreeMap<String, String>> {
    // The end of the last full import, with the import's generation after it: a catalogue dropped
    // and imported again starts at generation 1, a first import the index must read in full.
    let found: Vec<(String, String)> = sqlx::query_as(
        "SELECT catalogue, CASE WHEN last_import_at IS NULL THEN '' ELSE last_import_at::text||' #'||generation END FROM catalogue_state WHERE catalogue=ANY($1)",
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
    /// Whether the labels of cards built from catalogue cards match `tags` as people's tags do:
    /// only where the search may list catalogue cards (not with `referenced=true` or a social
    /// filter), so they never take candidate places Postgres then drops.
    pub labels: bool,
}
/// A text search's candidates, and the index revision they were read at (for the page's ETag).
#[derive(Clone, Debug)]
pub struct Candidates {
    pub units: Vec<Candidate>,
    pub revision: u64,
}
/// What [`SearchIndex::candidates_for`] found.
#[derive(Default)]
pub struct Found {
    /// The candidates; `None` when Postgres reads the text alone.
    pub candidates: Option<Candidates>,
    /// The turn the search keeps while Postgres reads a text the index left to it (one
    /// character found inside names): dropped when that read ends.
    pub turn: Option<Turn>,
    /// With the turn, the end of the search's budget: Postgres' read stops there too.
    pub deadline: Option<std::time::Instant>,
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
#[derive(Clone, Debug, PartialEq)]
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
    let push = |out: &mut Vec<Item>, item: Item| match item {
        // `or` between two items: never first, never twice.
        Item::Or => {
            if !matches!(out.last(), None | Some(Item::Or)) {
                out.push(Item::Or);
            }
        }
        item => out.push(item),
    };
    let mut out = vec![];
    let mut rest = text;
    // At most MAX_ITEMS items: a longer text's rest is Postgres' alone to check.
    while out.len() < MAX_ITEMS {
        let Some((item, after)) = next_item(index, rest) else {
            break;
        };
        rest = after;
        if let Some(item) = item {
            push(&mut out, item);
        }
    }
    // A cut inside an `or` chain (the next item past it that holds a word is `or`, a `-word` or a
    // piece of no word between, also right after a closing quote; or the last item kept is `or`):
    // the chain would be required in part, and what only a later item finds missed.
    let joins_next = {
        let mut ahead = rest;
        loop {
            match next_item(index, ahead) {
                Some((None | Some(Item::Not(..)), after)) => ahead = after,
                Some((Some(Item::Or), _)) => break true,
                _ => break false,
            }
        }
    };
    let or_last = matches!(out.last(), Some(Item::Or)) && !rest.trim().is_empty();
    if out.len() == MAX_ITEMS && (joins_next || or_last) {
        let kept = out.clone();
        drop_last_chain(&mut out);
        if !out.iter().any(|i| matches!(i, Item::Words(..))) {
            // The chain held every word to find: it is read on past the cut, whole, as main's
            // index read it (with no cut), up to MAX_PIECES words; a longer one leaves the query.
            out = kept;
            let mut words = out.iter().filter(|i| matches!(i, Item::Words(..))).count();
            let mut whole = true;
            while let Some((item, after)) = next_item(index, rest) {
                rest = after;
                match item {
                    None | Some(Item::Not(..)) => {}
                    Some(Item::Or) => push(&mut out, Item::Or),
                    Some(item) if matches!(out.last(), Some(Item::Or)) => {
                        words += 1;
                        if words > MAX_PIECES {
                            whole = false;
                            break;
                        }
                        out.push(item);
                    }
                    // A word `or` does not join: the chain ended (the rest, past the cut, is
                    // Postgres' alone to check).
                    Some(_) => break,
                }
            }
            if !whole {
                drop_last_chain(&mut out);
            }
        }
    }
    while matches!(out.last(), Some(Item::Or)) {
        out.pop();
    }
    out
}
/// One item at the start of `rest` and what follows it; `None` at the end, and no item for a piece
/// that holds no word to find.
fn next_item<'a>(index: &Index, rest: &'a str) -> Option<(Option<Item>, &'a str)> {
    let start = rest.find(|c: char| !c.is_whitespace())?;
    let mut rest = &rest[start..];
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
        return Some((Some(Item::Or), rest));
    }
    let words = tokens(index, WORDS, raw);
    if words.is_empty() {
        return Some((None, rest));
    }
    let whole = !quoted
        && raw.chars().all(|c| {
            crate::api::word_char(c) || (crate::api::unspaced_script(c) && c.is_alphanumeric())
        });
    Some((
        Some(if negated {
            Item::Not(words)
        } else {
            Item::Words(words, whole)
        }),
        rest,
    ))
}
/// Leaves out the last `or` chain of `out`: the items `or` joins as the query joins them (a
/// `-word` between them is no part of it, and goes too), back to the first. Leaving out more only
/// widens what the index finds.
fn drop_last_chain(out: &mut Vec<Item>) {
    while matches!(out.last(), Some(Item::Or | Item::Not(..))) {
        out.pop();
    }
    out.pop();
    loop {
        let mut at = out.len();
        while at > 0 && matches!(out[at - 1], Item::Not(..)) {
            at -= 1;
        }
        if at == 0 || !matches!(out[at - 1], Item::Or) {
            break;
        }
        out.truncate(at - 1);
        while matches!(out.last(), Some(Item::Not(..))) {
            out.pop();
        }
        out.pop();
    }
}

/// A text search's time: its deadline, and whether a part of it ran past it.
struct Budget {
    deadline: std::time::Instant,
    spent: std::sync::atomic::AtomicBool,
    /// Tests: the budget runs out after this many checks, whatever the time.
    #[cfg(test)]
    checks_left: Option<AtomicU64>,
}
impl Budget {
    fn new(deadline: std::time::Instant) -> Arc<Self> {
        Arc::new(Self {
            deadline,
            spent: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            checks_left: None,
        })
    }
    /// Whether the deadline has passed (and so the search is cut off).
    fn over(&self) -> bool {
        if self.spent.load(Ordering::Relaxed) {
            return true;
        }
        #[cfg(test)]
        if let Some(left) = &self.checks_left {
            if left.fetch_sub(1, Ordering::SeqCst) <= 1 {
                self.spent.store(true, Ordering::Relaxed);
                return true;
            }
        }
        let over = std::time::Instant::now() >= self.deadline;
        if over {
            self.spent.store(true, Ordering::Relaxed);
        }
        over
    }
    /// An error once the deadline has passed: the search stops there.
    fn check(&self) -> Result<()> {
        if self.over() {
            return Err(Busy.into());
        }
        Ok(())
    }
}
thread_local! {
    /// The budget of the search the thread runs ([`Open::search`]): every leaf query it builds
    /// checks it ([`leaf`]).
    static SEARCH_BUDGET: std::cell::RefCell<Option<Arc<Budget>>> = const { std::cell::RefCell::new(None) };
}
/// `query`, a leaf of a search's query (a word, a phrase, a set of words, a filter), stopping
/// once the thread's search budget runs out: every posting step checks it, so a part of the
/// query that matches nothing for long (a word left out of everything, an intersection) stops
/// too. Without a search under way, `query` as it is.
fn leaf(query: Box<dyn Query>) -> Box<dyn Query> {
    leaf_every(query, 1024)
}
/// [`leaf`], checking the budget every `every` posting steps (a phrase's step may read far).
fn leaf_every(query: Box<dyn Query>, every: u32) -> Box<dyn Query> {
    SEARCH_BUDGET.with(|budget| match &*budget.borrow() {
        Some(budget) => Box::new(Budgeted {
            inner: query,
            budget: budget.clone(),
            every,
        }),
        None => query,
    })
}
/// Sets the thread's search budget for as long as it lives.
struct InBudget;
impl InBudget {
    fn new(budget: &Arc<Budget>) -> Self {
        SEARCH_BUDGET.with(|b| *b.borrow_mut() = Some(budget.clone()));
        Self
    }
}
impl Drop for InBudget {
    fn drop(&mut self) {
        SEARCH_BUDGET.with(|b| *b.borrow_mut() = None);
    }
}
#[cfg(test)]
thread_local! {
    /// The steps budgeted scorers took on this thread (tests).
    static STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// The text index could not answer within `limits.search_index_budget_ms`: every turn was taken
/// for that long, or the search ran past it. The search answers 503.
#[derive(Debug)]
pub struct Busy;
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the text index is busy")
    }
}
impl std::error::Error for Busy {}
/// A query that stops matching once its search's [`Budget`] has run out: each segment checks it
/// before it starts, and every `every` posting steps.
#[derive(Debug)]
struct Budgeted {
    inner: Box<dyn Query>,
    budget: Arc<Budget>,
    every: u32,
}
impl Clone for Budgeted {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.box_clone(),
            budget: self.budget.clone(),
            every: self.every,
        }
    }
}
impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Budget")
    }
}
impl Query for Budgeted {
    fn weight(
        &self,
        scoring: tantivy::query::EnableScoring<'_>,
    ) -> tantivy::Result<Box<dyn tantivy::query::Weight>> {
        Ok(Box::new(BudgetedWeight {
            inner: self.inner.weight(scoring)?,
            budget: self.budget.clone(),
            every: self.every,
        }))
    }
    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.inner.query_terms(visitor)
    }
}
struct BudgetedWeight {
    inner: Box<dyn tantivy::query::Weight>,
    budget: Arc<Budget>,
    every: u32,
}
impl tantivy::query::Weight for BudgetedWeight {
    fn scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> tantivy::Result<Box<dyn tantivy::query::Scorer>> {
        if self.budget.over() {
            return Ok(Box::new(tantivy::query::EmptyScorer));
        }
        Ok(Box::new(BudgetedScorer {
            inner: self.inner.scorer(reader, boost)?,
            budget: self.budget.clone(),
            every: self.every.max(1),
            steps: 0,
            ended: false,
        }))
    }
    fn explain(
        &self,
        reader: &SegmentReader,
        doc: DocId,
    ) -> tantivy::Result<tantivy::query::Explanation> {
        self.inner.explain(reader, doc)
    }
}
struct BudgetedScorer {
    inner: Box<dyn tantivy::query::Scorer>,
    budget: Arc<Budget>,
    every: u32,
    steps: u32,
    ended: bool,
}
impl BudgetedScorer {
    fn spent(&mut self) -> bool {
        #[cfg(test)]
        STEPS.with(|n| n.set(n.get() + 1));
        self.steps = self.steps.wrapping_add(1);
        if !self.ended && self.steps.is_multiple_of(self.every) && self.budget.over() {
            self.ended = true;
        }
        self.ended
    }
}
impl tantivy::DocSet for BudgetedScorer {
    fn advance(&mut self) -> DocId {
        if self.spent() {
            return tantivy::TERMINATED;
        }
        self.inner.advance()
    }
    fn seek(&mut self, target: DocId) -> DocId {
        if self.spent() {
            return tantivy::TERMINATED;
        }
        self.inner.seek(target)
    }
    fn doc(&self) -> DocId {
        if self.ended {
            tantivy::TERMINATED
        } else {
            self.inner.doc()
        }
    }
    fn size_hint(&self) -> u32 {
        self.inner.size_hint()
    }
}
impl tantivy::query::Scorer for BudgetedScorer {
    fn score(&mut self) -> Score {
        self.inner.score()
    }
}

impl Open {
    /// The best `cap` units for `q`, within `budget`: `None` when the index does not serve the
    /// text (one character it would have to find inside names: Postgres reads it alone), an error
    /// ([`Busy`]) when the budget ran out.
    fn search(
        &self,
        q: &TextQuery,
        cap: usize,
        budget: &Arc<Budget>,
    ) -> Result<Option<Vec<Candidate>>> {
        // One character found inside names (`ILIKE '%c%'`) would read the whole dictionary of
        // the names' pieces: Postgres reads such a text alone. Decided before any index work.
        let items = items(&self.index, &q.text);
        let one_character = q.text.trim().chars().count() == 1;
        let has_words = items.iter().any(|i| matches!(i, Item::Words(..)));
        if one_character && (!has_words || q.inside_words) {
            return Ok(None);
        }
        budget.check()?;
        // Every leaf query built from here on checks the budget ([`leaf`]).
        let _in_budget = InBudget::new(budget);
        let searcher = self.reader.searcher();
        let order = q.order.unwrap_or(TextOrder::Relevance);
        let now = q.at.timestamp() as f64;
        let filters = self.filters(q);
        let words = self.words_query(&searcher, &q.text, &items, &filters);
        budget.check()?;
        // Each document's IDs from the fast fields (no stored document is read), the columns of a
        // segment opened once a candidate is in it.
        type Columns = (
            Option<tantivy::columnar::StrColumn>,
            Option<tantivy::columnar::StrColumn>,
        );
        let columns: std::cell::RefCell<HashMap<u32, Columns>> = Default::default();
        let ids_of = |segment: u32, doc: DocId| -> Result<(String, String)> {
            let mut columns = columns.borrow_mut();
            let (units, resources) = columns.entry(segment).or_insert_with(|| {
                let fast = searcher.segment_reader(segment).fast_fields();
                (
                    fast.str("unit_id").ok().flatten(),
                    fast.str("resource_id").ok().flatten(),
                )
            });
            let id = |column: &Option<tantivy::columnar::StrColumn>| -> Result<String> {
                let column = column.as_ref().context("an ID column")?;
                let ord = column.term_ords(doc).next().context("an ID")?;
                let mut out = String::new();
                column.ord_to_str(ord, &mut out)?;
                Ok(out)
            };
            Ok((id(units)?, id(resources)?))
        };
        let found = |query: Box<dyn Query>, words: bool| -> Result<Vec<Candidate>> {
            let query = Budgeted {
                inner: query,
                budget: budget.clone(),
                every: 1024,
            };
            let hits = searcher.search(&query, &ranked(cap, order, now))?;
            budget.check()?;
            hits.into_iter()
                .map(|((_, relevance), address)| {
                    let (unit_id, resource_id) = ids_of(address.segment_ord, address.doc_id)?;
                    let card = resource_id
                        .strip_prefix("catalogue:")
                        .and_then(|id| id.parse().ok());
                    Ok(Candidate {
                        unit_id,
                        resource_id,
                        relevance,
                        words,
                        card,
                    })
                })
                .collect()
        };
        let mut out = match words {
            Some(query) => found(query, true)?,
            None => vec![],
        };
        // The text inside a name and similar names, as Postgres finds them, for a text that is
        // not short: proposed, and checked there; only while fewer matches than the candidates
        // asked for (`cap`) hold every word in their name, in any order and on any page: with
        // that many, the candidates are those names already.
        // Counted only when the proposals may be read: without words they always are, and a text
        // that matches only where words start never needs them. The count stops at `cap`.
        let propose = !has_words
            || (q.inside_words
                && match self.name_words_query(&searcher, &items, &filters) {
                    Some(names) => {
                        self.counted.fetch_add(1, Ordering::SeqCst);
                        budget.check()?;
                        count_up_to(&searcher, names.as_ref(), cap, budget)? < cap
                    }
                    None => true,
                });
        if propose {
            budget.check()?;
            if let Some(query) = self.proposals(&searcher, &q.text, &filters) {
                self.proposed.fetch_add(1, Ordering::SeqCst);
                let seen: HashSet<String> = out.iter().map(|c| c.unit_id.clone()).collect();
                out.extend(
                    found(query, false)?
                        .into_iter()
                        .filter(|c| !seen.contains(&c.unit_id)),
                );
            }
        }
        Ok(Some(out))
    }
}
/// How many documents `query` matches, counting no further than `cap` (deleted ones left out).
fn count_up_to(
    searcher: &tantivy::Searcher,
    query: &dyn Query,
    cap: usize,
    budget: &Arc<Budget>,
) -> Result<usize> {
    use tantivy::DocSet;
    let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(
        searcher,
    ))?;
    let mut count = 0;
    for segment in searcher.segment_readers() {
        budget.check()?;
        let mut scorer = weight.scorer(segment, 1.0)?;
        let alive = segment.alive_bitset();
        let mut doc = scorer.doc();
        let mut steps = 0usize;
        while doc != tantivy::TERMINATED {
            if alive.is_none_or(|a| a.is_alive(doc)) {
                count += 1;
                if count >= cap {
                    return Ok(count);
                }
            }
            // Deleted documents count as steps too.
            steps += 1;
            if steps.is_multiple_of(4096) {
                budget.check()?;
            }
            doc = scorer.advance();
        }
    }
    Ok(count)
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
        // A tag matches people's tags, and source labels where they count as tags.
        let either = |tags: &[String]| -> Box<dyn Query> {
            if !q.labels {
                return set(f.tag, tags);
            }
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
        // Each filter stops with the search's budget.
        out.into_iter()
            .map(|(occur, query)| (occur, leaf(query)))
            .collect()
    }
    /// The words of one item in one field, as a word or a phrase.
    /// A phrase is matched as one in the index by its first [`MAX_PHRASE_WORDS`] words, checking
    /// the budget at every step, and as all of its words when every one is common (in
    /// [`COMMON_PIECE_DOCS`] and [`COMMON_PIECE`] of the documents: `"the the of"`) or its rarest
    /// is in more than [`PHRASE_POSTINGS`] documents (`"york new"`): one step of a phrase reads
    /// its words' lists until a document holds them in order, which for such words may be the
    /// whole lists. Postgres checks the whole phrase on every candidate.
    fn words_in(
        &self,
        searcher: &tantivy::Searcher,
        field: Field,
        words: &[String],
    ) -> Box<dyn Query> {
        let mut terms: Vec<Term> = words
            .iter()
            .take(MAX_PHRASE_WORDS)
            .map(|w| Term::from_field_text(field, w))
            .collect();
        let one = |term: Term| -> Box<dyn Query> {
            leaf(Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)))
        };
        if terms.len() == 1 {
            return one(terms.pop().expect("one term"));
        }
        let docs = searcher.num_docs().max(1) as f64;
        let common = terms.iter().all(|t| {
            searcher
                .doc_freq(t)
                .is_ok_and(|n| n >= COMMON_PIECE_DOCS && n as f64 / docs > COMMON_PIECE)
        });
        let rarest = terms
            .iter()
            .map(|t| searcher.doc_freq(t).unwrap_or(u64::MAX))
            .min()
            .unwrap_or(0);
        if common || rarest > PHRASE_POSTINGS {
            terms.sort();
            terms.dedup();
            return Box::new(BooleanQuery::new(
                terms.into_iter().map(|t| (Occur::Must, one(t))).collect(),
            ));
        }
        leaf_every(Box::new(PhraseQuery::new(terms)), 1)
    }
    /// The words of `field` that start with `word` (`word` too), at most [`PREFIX_TERMS`] in all:
    /// of the first [`PREFIX_SCAN`] such words of each segment, in dictionary order, those the
    /// most documents hold, each while the documents of those taken stay within
    /// [`PREFIX_POSTINGS`] in all (a word set reads every posting of its words in one go). A short
    /// start in a large dictionary expands to a bounded set of words; the scan stops with the
    /// search's budget.
    fn prefix_terms(&self, searcher: &tantivy::Searcher, field: Field, word: &str) -> Vec<Term> {
        let over = || SEARCH_BUDGET.with(|b| b.borrow().as_ref().is_some_and(|b| b.over()));
        let mut held: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        for segment in searcher.segment_readers() {
            if over() {
                break;
            }
            let Ok(inverted) = segment.inverted_index(field) else {
                continue;
            };
            let Ok(mut stream) = inverted.terms().range().ge(word.as_bytes()).into_stream() else {
                continue;
            };
            let mut read = 0;
            while read < PREFIX_SCAN && stream.advance() {
                if !stream.key().starts_with(word.as_bytes()) {
                    break;
                }
                read += 1;
                if read % 4096 == 0 && over() {
                    break;
                }
                *held.entry(stream.key().to_vec()).or_default() +=
                    u64::from(stream.value().doc_freq);
            }
        }
        let mut held: Vec<(u64, Vec<u8>)> = held.into_iter().map(|(k, n)| (n, k)).collect();
        held.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut postings = 0u64;
        held.retain(|(n, _)| {
            let fits = postings + n <= PREFIX_POSTINGS;
            if fits {
                postings += n;
            }
            fits
        });
        held.truncate(PREFIX_TERMS);
        held.into_iter()
            .filter_map(|(_, key)| {
                std::str::from_utf8(&key)
                    .ok()
                    .map(|text| Term::from_field_text(field, text))
            })
            .collect()
    }
    /// One item where it may match: the name, other names or the description; as the last item,
    /// also the start of a word in the name ([`Open::prefix_terms`]). People's tags and source
    /// labels holding it add to its score but never match alone.
    fn item_query(
        &self,
        searcher: &tantivy::Searcher,
        words: &[String],
        last: bool,
    ) -> Box<dyn Query> {
        let f = &self.fields;
        let boosted = |field: Field, boost: Score| -> (Occur, Box<dyn Query>) {
            (
                Occur::Should,
                Box::new(BoostQuery::new(
                    self.words_in(searcher, field, words),
                    boost,
                )),
            )
        };
        let mut matches: Vec<(Occur, Box<dyn Query>)> = vec![
            boosted(f.name, NAME_BOOST),
            boosted(f.other_names, OTHER_NAMES_BOOST),
            boosted(f.description, DESCRIPTION_BOOST),
        ];
        if let ([word], true) = (words, last) {
            for field in [f.name, f.other_names] {
                let terms = self.prefix_terms(searcher, field, word);
                if terms.is_empty() {
                    continue;
                }
                matches.push((
                    Occur::Should,
                    Box::new(ConstScoreQuery::new(
                        leaf(Box::new(TermSetQuery::new(terms))),
                        PREFIX_SCORE,
                    )),
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
    /// a text of two), across words. One character is not looked up here (it would read the
    /// whole dictionary of the names' pieces): [`Open::search`] leaves such a text to Postgres.
    fn substring(&self, text: &str) -> Option<Box<dyn Query>> {
        let f = &self.fields;
        let length = text.chars().count();
        let query: Box<dyn Query> = match length {
            0 | 1 => return None,
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
                                leaf(Box::new(TermQuery::new(
                                    Term::from_field_text(f.name_grams, &g),
                                    IndexRecordOption::Basic,
                                ))),
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
        searcher: &tantivy::Searcher,
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
                    let query = self.item_query(searcher, words, position == last_words && *whole);
                    match (joined, groups.last_mut()) {
                        (true, Some(group)) => group.push(query),
                        _ => groups.push(vec![query]),
                    }
                    joined = false;
                }
                Item::Not(words) => {
                    clauses.push((Occur::MustNot, self.item_query(searcher, words, false)));
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
                    leaf(Box::new(TermQuery::new(
                        Term::from_field_text(f.name_exact, &exact),
                        IndexRecordOption::Basic,
                    ))),
                    EXACT_NAME_SCORE,
                )),
            ));
        }
        let plain: Vec<String> = items
            .iter()
            .filter_map(|i| match i {
                Item::Words(w, _) => Some(w),
                _ => None,
            })
            .flatten()
            .cloned()
            .collect();
        if plain.len() > 1 && !items.iter().any(|i| matches!(i, Item::Or)) {
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    self.words_in(searcher, f.name, &plain),
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
        searcher: &tantivy::Searcher,
        items: &[Item],
        filters: &[(Occur, Box<dyn Query>)],
    ) -> Option<Box<dyn Query>> {
        let f = &self.fields;
        if items.iter().any(|i| matches!(i, Item::Or)) {
            return None;
        }
        let in_name = |words: &[String]| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(vec![
                (Occur::Should, self.words_in(searcher, f.name, words)),
                (Occur::Should, self.words_in(searcher, f.other_names, words)),
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
        // Of the rest, the MAX_PIECES rarest: the count asked for is lowered by one for each left
        // out too, so the query reads a bounded number of lists.
        let mut pieces: Vec<(u64, &String)> = all
            .iter()
            .filter_map(|p| {
                let n = searcher
                    .doc_freq(&Term::from_field_text(f.name_pieces, p))
                    .unwrap_or(0);
                (n < COMMON_PIECE_DOCS || (n as f64) / names <= COMMON_PIECE).then_some((n, p))
            })
            .collect();
        pieces.sort();
        pieces.truncate(MAX_PIECES);
        let pieces: Vec<&String> = pieces.into_iter().map(|(_, p)| p).collect();
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
                                leaf(Box::new(TermQuery::new(
                                    Term::from_field_text(f.name_pieces, p),
                                    IndexRecordOption::Basic,
                                ))),
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

    fn docs(open: &Open, docs: &[(&str, &str, &str)]) {
        let f = open.fields;
        let mut writer = open.writer.lock().unwrap();
        for (id, name, catalogue) in docs {
            let mut doc = TantivyDocument::default();
            doc.add_text(f.resource_id, *id);
            doc.add_text(f.name, *name);
            if !catalogue.is_empty() {
                doc.add_text(f.catalogue, *catalogue);
            }
            doc.add_u64(f.has_text, 1);
            writer.add_document(doc).unwrap();
        }
        commit(&mut writer, &open.stamp(0)).unwrap();
    }
    fn holds(open: &Open, word: &str) -> bool {
        let searcher = open.reader.searcher();
        searcher.segment_readers().iter().any(|segment| {
            segment
                .inverted_index(open.fields.name)
                .unwrap()
                .terms()
                .get(word.as_bytes())
                .unwrap()
                .is_some()
        })
    }

    /// A merge of Tantivy's own that replaced the segment since the reader last read it: the merge
    /// after a removal finds the document's words in the new segment, not nothing.
    #[test]
    fn a_merge_after_a_removal_reads_the_segments_as_they_are_now() {
        let open = Arc::new(
            Open::new(
                index(),
                0,
                BTreeMap::new(),
                String::new(),
                Default::default(),
            )
            .unwrap(),
        );
        docs(&open, &[("a", "aurora", ""), ("c", "cosmos", "")]);
        docs(&open, &[("b", "borealis", ""), ("d", "dawn", "")]);
        open.reader.reload().unwrap();
        {
            // Merged behind the reader's back, then the removal committed: the reader still knows
            // only the two old segments.
            let mut writer = open.writer.lock().unwrap();
            let segments = open.index.searchable_segment_ids().unwrap();
            writer.merge(&segments).wait().unwrap();
            writer.delete_term(Term::from_field_text(open.fields.resource_id, "a"));
            commit(&mut writer, &open.stamp(0)).unwrap();
            open.purge(&mut writer, vec!["a".into()], false).unwrap();
        }
        if let Some(waiting) = open.merging.lock().unwrap().take() {
            waiting.join().unwrap();
        }
        open.reader.reload().unwrap();
        assert!(
            !holds(&open, "aurora"),
            "the deleted document's words left the files"
        );
        assert!(holds(&open, "cosmos"));
        assert!(!open.purge_owed());
    }

    /// A catalogue's deleted documents, still in the files until a merge, are no documents of it:
    /// a switch-off whose merge runs is not removed again at the next pass.
    #[test]
    fn deleted_documents_of_a_catalogue_are_not_documents() {
        let open = Arc::new(
            Open::new(
                index(),
                0,
                BTreeMap::new(),
                String::new(),
                Default::default(),
            )
            .unwrap(),
        );
        docs(
            &open,
            &[("catalogue:1", "peak", "wikidata"), ("r", "page", "")],
        );
        open.reader.reload().unwrap();
        assert!(open
            .has_documents(&open.reader.searcher(), "wikidata")
            .unwrap());
        {
            let mut writer = open.writer.lock().unwrap();
            writer.delete_term(Term::from_field_text(open.fields.catalogue, "wikidata"));
            commit(&mut writer, &open.stamp(0)).unwrap();
        }
        open.reader.reload().unwrap();
        let searcher = open.reader.searcher();
        let term = Term::from_field_text(open.fields.catalogue, "wikidata");
        assert!(searcher.doc_freq(&term).unwrap() > 0, "still in the files");
        assert!(!open.has_documents(&searcher, "wikidata").unwrap());
    }

    /// The flag that a merge runs is cleared only when its thread ends, never by the wait itself:
    /// a pass in between owes its merge instead of starting a second one beside it (which a later
    /// pass would then join under the writer lock).
    #[test]
    fn a_merge_stays_running_until_its_thread_ends() {
        let open = Arc::new(
            Open::new(
                index(),
                0,
                BTreeMap::new(),
                String::new(),
                Default::default(),
            )
            .unwrap(),
        );
        docs(&open, &[("a", "aurora", ""), ("c", "cosmos", "")]);
        docs(&open, &[("b", "borealis", ""), ("d", "dawn", "")]);
        open.reader.reload().unwrap();
        open.purging.lock().unwrap().running = true;
        let merged = {
            let mut writer = open.writer.lock().unwrap();
            writer.delete_term(Term::from_field_text(open.fields.resource_id, "a"));
            commit(&mut writer, &open.stamp(0)).unwrap();
            open.reader.reload().unwrap();
            open.start_merge(&mut writer, &["a".into()], false)
                .unwrap()
                .expect("a merge")
        };
        open.wait_merge(merged, vec!["a".into()], false);
        assert_eq!(open.purges.load(Ordering::SeqCst), 1);
        assert!(
            open.purging.lock().unwrap().running,
            "still running until the thread ends"
        );
        {
            let mut writer = open.writer.lock().unwrap();
            open.purge(&mut writer, vec!["b".into()], false).unwrap();
        }
        assert!(
            open.merging.lock().unwrap().is_none(),
            "no second merge started"
        );
        assert_eq!(open.purging.lock().unwrap().resources, ["b"]);
        drop(Running(&open.purging));
        assert!(open.purge_owed());
    }

    /// A merge after a removal while another runs is owed, not lost: the next pass starts it, and
    /// the removed document's words leave the files.
    #[test]
    fn a_removal_while_a_merge_runs_is_merged_at_the_next_pass() {
        let open = Arc::new(
            Open::new(
                index(),
                0,
                BTreeMap::new(),
                String::new(),
                Default::default(),
            )
            .unwrap(),
        );
        let f = open.fields;
        {
            let mut writer = open.writer.lock().unwrap();
            // Two segments; a segment whose every document is deleted leaves at the commit.
            for segment in [
                [("a", "aurora"), ("c", "cosmos")],
                [("b", "borealis"), ("d", "dawn")],
            ] {
                for (id, name) in segment {
                    let mut doc = TantivyDocument::default();
                    doc.add_text(f.resource_id, id);
                    doc.add_text(f.name, name);
                    doc.add_u64(f.has_text, 1);
                    writer.add_document(doc).unwrap();
                }
                commit(&mut writer, &open.stamp(0)).unwrap();
            }
        }
        open.reader.reload().unwrap();
        let holds = |word: &str| {
            let searcher = open.reader.searcher();
            searcher.segment_readers().iter().any(|segment| {
                segment
                    .inverted_index(f.name)
                    .unwrap()
                    .terms()
                    .get(word.as_bytes())
                    .unwrap()
                    .is_some()
            })
        };
        open.purging.lock().unwrap().running = true;
        {
            let mut writer = open.writer.lock().unwrap();
            writer.delete_term(Term::from_field_text(f.resource_id, "a"));
            commit(&mut writer, &open.stamp(0)).unwrap();
            open.reader.reload().unwrap();
            open.purge(&mut writer, vec!["a".into()], false).unwrap();
        }
        assert!(!open.purge_owed(), "one runs");
        assert!(holds("aurora"));
        open.purging.lock().unwrap().running = false;
        assert!(open.purge_owed());
        {
            let mut writer = open.writer.lock().unwrap();
            open.purge(&mut writer, vec![], false).unwrap();
        }
        let waiting = open.merging.lock().unwrap().take().expect("a merge");
        waiting.join().unwrap();
        assert_eq!(open.purges.load(Ordering::SeqCst), 1);
        assert!(!open.purge_owed());
        assert!(!holds("aurora"));
        assert!(holds("cosmos") && holds("borealis"));
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
        let mut seed = crate::catalogue::testgen::seed("tokenizer", 0x9e37_79b9_7f4a_7c15).max(1);
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

    /// Searchable documents named `names`, as the index holds a referenced unit's: IDs, the name
    /// in every name field, one commit.
    fn named(open: &Open, names: &[String]) {
        let f = open.fields;
        let mut writer = open.writer.lock().unwrap();
        for (n, name) in names.iter().enumerate() {
            let id = format!("{n:032x}");
            let mut doc = TantivyDocument::default();
            doc.add_text(f.unit_id, &id);
            doc.add_text(f.resource_id, &id);
            for field in [f.name, f.name_exact, f.name_grams, f.name_pieces] {
                doc.add_text(field, name);
            }
            doc.add_u64(f.referenced, 1);
            doc.add_u64(f.has_text, 1);
            writer.add_document(doc).unwrap();
        }
        commit(&mut writer, &open.stamp(0)).unwrap();
        drop(writer);
        open.reader.reload().unwrap();
    }

    /// Property (fuzz): any text up to the API's 256 bytes (many words, phrases, `-`, `or` chains,
    /// one character, symbols, scripts written without spaces) is searched over a seeded index:
    /// candidates, or `None` (Postgres reads it alone); never a panic, never more than the
    /// candidates asked for from each part. Each text holds at most MAX_ITEMS items, and each of
    /// its words' starts expands to at most PREFIX_TERMS words holding at most PREFIX_POSTINGS
    /// postings, and does at most a ceiling of posting steps. The index has four segments. The
    /// budget is generous (a loaded host is slow): no wall-clock bound is checked.
    #[test]
    fn any_text_of_256_bytes_is_searched_within_its_budget() {
        let mut gen =
            crate::catalogue::testgen::Gen::new("text search fuzz", 0x2f8a_51c3_77e1_0b9d);
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        let syllables = [
            "a", "an", "ar", "ba", "bo", "ca", "de", "el", "ka", "lo", "ma", "ra", "st", "th", "zu",
        ];
        let names: Vec<String> = (0..20_000)
            .map(|_| {
                (0..1 + gen.below(4))
                    .map(|_| {
                        (0..1 + gen.below(4))
                            .map(|_| *gen.pick(&syllables))
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        // In four commits: four segments, so the per-segment scans and the sums across segments
        // are what each search meets.
        for part in names.chunks(names.len() / 4) {
            named(&open, part);
        }
        let mut served = 0;
        let mut most_steps = 0;
        // The ceiling of posting steps per search: with the fixed seed, about twice the most its
        // searches take (59,792, measured; the run prints its own), so a lost bound shows as many
        // times more; with another seed (`PINAKES_TEST_SEED`), every posting of the index once.
        let step_ceiling: u64 = if std::env::var("PINAKES_TEST_SEED").is_err() {
            120_000
        } else {
            let searcher = open.reader.searcher();
            let f = open.fields;
            searcher
                .segment_readers()
                .iter()
                .flat_map(|segment| {
                    [f.name, f.name_exact, f.name_grams, f.name_pieces]
                        .map(|field| segment.inverted_index(field).ok())
                })
                .flatten()
                .map(|inverted| {
                    let mut terms = inverted.terms().stream().expect("terms");
                    let mut postings = 0u64;
                    while terms.advance() {
                        postings += u64::from(terms.value().doc_freq);
                    }
                    postings
                })
                .sum()
        };
        let pieces = [
            "or",
            "OR",
            "-",
            "\"",
            "a",
            "ab",
            "an",
            "ar",
            "bo",
            "ca",
            "the",
            "ka lo",
            "-ma",
            "\"ra st\"",
            "東",
            "京",
            "ภา",
            "€",
            "(",
            "!!!",
            "c++",
            "é",
            "zürich",
            "x",
            "0",
            "\u{301}",
            "😀",
            "a-b",
        ];
        for round in 0..300 {
            let mut text = String::new();
            while text.len() < 256 {
                text.push_str(gen.pick(&pieces));
                if gen.chance(3) {
                    text.push(' ');
                }
            }
            let mut cut = if gen.chance(4) { 1 + gen.below(4) } else { 256 };
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            let q = TextQuery {
                text: text.clone(),
                at: chrono::Utc::now(),
                inside_words: gen.chance(2),
                order: Some(*gen.pick(&[
                    TextOrder::Relevance,
                    TextOrder::Activity,
                    TextOrder::Recent,
                ])),
                ..Default::default()
            };
            let parts = items(&open.index, &text);
            // MAX_ITEMS items, or one chain of `or`s read whole, up to MAX_PIECES words.
            let chain_words = parts
                .iter()
                .filter(|i| matches!(i, Item::Words(..)))
                .count();
            assert!(
                parts.len() <= MAX_ITEMS
                    || (chain_words <= MAX_PIECES
                        && parts
                            .iter()
                            .all(|i| matches!(i, Item::Words(..) | Item::Or))),
                "{round}: {text:?}"
            );
            let searcher = open.reader.searcher();
            for word in text.split_whitespace().take(4) {
                let word = word.to_lowercase();
                let terms = open.prefix_terms(&searcher, open.fields.name, &word);
                let postings: u64 = terms
                    .iter()
                    .map(|t| searcher.doc_freq(t).unwrap_or(0))
                    .sum();
                assert!(terms.len() <= PREFIX_TERMS, "{round}: {word:?}");
                assert!(postings <= PREFIX_POSTINGS, "{round}: {word:?}: {postings}");
            }
            let budget =
                Budget::new(std::time::Instant::now() + std::time::Duration::from_secs(600));
            let before = STEPS.with(|n| n.get());
            let searched = open.search(&q, 500, &budget);
            // The work, counted in posting steps of the budgeted leaves (no clock): far below what
            // reading every posting of the texts' parts would take.
            let steps = STEPS.with(|n| n.get()) - before;
            most_steps = most_steps.max(steps);
            assert!(steps <= step_ceiling, "{round}: {text:?}: {steps} steps");
            match searched {
                Ok(Some(found)) => {
                    served += 1;
                    assert!(found.len() <= 1000, "{round}: {text:?}")
                }
                Ok(None) => assert_eq!(text.trim().chars().count(), 1, "{text:?}"),
                Err(error) => panic!("{round}: {text:?}: {error}"),
            }
        }
        // Near the measured count with the fixed seed (282; the run prints it): the index answers
        // every text but those of one character found inside names, which Postgres reads alone
        // (the `None` arm above accepts no other).
        assert!(served >= 270, "{served} of 300 answered");
        eprintln!(
            "most posting steps of one search: {most_steps} (ceiling {step_ceiling}); {served} of 300 answered"
        );
    }

    /// A part of a query that matches nothing for long (every name holds `the`, `of` and `and`,
    /// and `-the` leaves each out) stops with the budget at its leaves, not at the end of the
    /// segment: a budget spent after a few checks stops the search within a few thousand steps
    /// of 60,000 documents, and the search answers `Busy`.
    #[test]
    fn a_query_that_matches_nothing_stops_at_its_leaves() {
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        let names: Vec<String> = (0..60_000).map(|n| format!("the of and x{n}")).collect();
        named(&open, &names);
        let q = TextQuery {
            text: "the of and -the".into(),
            ..Default::default()
        };
        let budget = Arc::new(Budget {
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            spent: std::sync::atomic::AtomicBool::new(false),
            // A few for the setup's own checks, then the leaves'.
            checks_left: Some(AtomicU64::new(12)),
        });
        let before = STEPS.with(|n| n.get());
        let answer = open.search(&q, 10, &budget);
        let steps = STEPS.with(|n| n.get()) - before;
        assert!(answer.unwrap_err().is::<Busy>());
        // Without the leaves' checks it would be every posting of the four words, 240,000.
        assert!(
            (1_000..50_000).contains(&steps),
            "{steps} steps: the leaves check the budget"
        );
        // With time to spare, the same search answers (nothing matches).
        let budget = Budget::new(std::time::Instant::now() + std::time::Duration::from_secs(60));
        assert_eq!(open.search(&q, 10, &budget).unwrap().unwrap().len(), 0);
    }

    /// A phrase of common words (`"the the of"`) is matched as all its words in the index (its
    /// position lists would be read in one step); one with a rarer word stays a phrase; a phrase
    /// is matched by its first eight words.
    #[test]
    fn phrases_of_common_words_are_matched_as_words() {
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        let mut names: Vec<String> = (0..12_000).map(|n| format!("the of z{n}")).collect();
        names.push("of the rare".into());
        names.push("rare the".into());
        names.push("w w w w w w w w".into());
        named(&open, &names);
        let searcher = open.reader.searcher();
        let field = open.fields.name;
        let words = |w: &[&str]| w.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let found = |query: Box<dyn Query>| {
            searcher
                .search(query.as_ref(), &tantivy::collector::Count)
                .unwrap()
        };
        // Every name holds both: as words, all of them (in either order).
        assert_eq!(
            found(open.words_in(&searcher, field, &words(&["the", "of"]))),
            12_001
        );
        // `rare` is rare: a phrase, in order only.
        assert_eq!(
            found(open.words_in(&searcher, field, &words(&["the", "rare"]))),
            1
        );
        // Twenty words: the first eight, which one name holds in a row.
        let long: Vec<String> = (0..20).map(|_| "w".to_string()).collect();
        assert_eq!(found(open.words_in(&searcher, field, &long)), 1);
    }

    /// A budget that has run out stops the search: `Busy`, not candidates.
    #[test]
    fn a_search_past_its_budget_is_busy() {
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        named(&open, &["aurora borealis".to_owned()]);
        let q = TextQuery {
            text: "aurora".into(),
            ..Default::default()
        };
        let live = Budget::new(std::time::Instant::now() + std::time::Duration::from_secs(5));
        assert_eq!(open.search(&q, 10, &live).unwrap().unwrap().len(), 1);
        let spent = Budget::new(std::time::Instant::now());
        assert!(open.search(&q, 10, &spent).unwrap_err().is::<Busy>());
    }

    /// A word start expands to at most PREFIX_TERMS words per segment, those the most names hold
    /// first; a text holds at most MAX_ITEMS items; a count stops at the number asked for.
    #[test]
    fn expansions_items_and_counts_are_bounded() {
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        let mut names: Vec<String> = (0..PREFIX_TERMS + 500).map(|n| format!("a{n}")).collect();
        names.extend((0..50).map(|_| "apple".to_owned()));
        named(&open, &names);
        let searcher = open.reader.searcher();
        let terms = open.prefix_terms(&searcher, open.fields.name, "a");
        assert_eq!(terms.len(), PREFIX_TERMS);
        assert!(terms.contains(&Term::from_field_text(open.fields.name, "apple")));
        assert_eq!(
            open.prefix_terms(&searcher, open.fields.name, "apple")
                .len(),
            1
        );
        assert_eq!(items(&open.index, &"word ".repeat(100)).len(), MAX_ITEMS);
        let budget = Budget::new(std::time::Instant::now() + std::time::Duration::from_secs(5));
        let all = tantivy::query::AllQuery;
        assert_eq!(count_up_to(&searcher, &all, 10, &budget).unwrap(), 10);
        assert_eq!(
            count_up_to(&searcher, &all, 1_000_000, &budget).unwrap(),
            names.len()
        );
    }

    /// A free turn goes to a client under its share (half the turns, at least one) before one past
    /// it, while both wait; with nobody else waiting, the one past its share takes it. A waiting
    /// search holds nothing.
    #[tokio::test]
    async fn turns_go_to_a_client_under_its_share_first() {
        let turns = Turns::new(2);
        let at = |a: &str| Some(share_key(a.parse().unwrap()));
        let deadline = || tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let own = turns.take(at("2001:db8::1"), deadline()).await.unwrap();
        let other = turns.take(None, deadline()).await.unwrap();
        let past = {
            let turns = turns.clone();
            tokio::spawn(async move { turns.take(at("2001:db8::2"), deadline()).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let under = {
            let turns = turns.clone();
            tokio::spawn(async move { turns.take(at("192.0.2.3"), deadline()).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!past.is_finished() && !under.is_finished());
        drop(other);
        let turn = tokio::time::timeout(std::time::Duration::from_secs(10), under)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!past.is_finished(), "every turn is taken");
        drop(turn);
        let last = tokio::time::timeout(std::time::Duration::from_secs(10), past)
            .await
            .unwrap()
            .unwrap();
        assert!(
            last.is_ok(),
            "alone, past its share, it takes the free turn"
        );
        drop(own);
        let state = turns.state.lock().unwrap();
        assert!(state.waiting.is_empty());
    }

    /// A client's share is counted by its IPv4 address or its IPv6 address's /64; an IPv4 address
    /// written as IPv6 is that address.
    #[test]
    fn a_client_is_its_address_or_its_v6_network() {
        let key = |a: &str| share_key(a.parse().unwrap());
        assert_eq!(key("2001:db8::1"), key("2001:db8::ffff:2"));
        assert_ne!(key("2001:db8::1"), key("2001:db8:0:1::1"));
        assert_eq!(key("::ffff:192.0.2.1"), key("192.0.2.1"));
        assert_ne!(key("192.0.2.1"), key("192.0.2.2"));
    }

    /// A text that is one chain of `or`s, cut at MAX_ITEMS and read whole, finds a unit only its
    /// last word names.
    #[test]
    fn a_long_or_chain_finds_what_only_its_last_word_names() {
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        let names: Vec<String> = (0..20).map(|n| format!("c{n}")).collect();
        named(&open, &names);
        let q = TextQuery {
            text: (0..20)
                .map(|n| format!("c{n}"))
                .collect::<Vec<_>>()
                .join(" or "),
            at: chrono::Utc::now(),
            ..Default::default()
        };
        let budget = Budget::new(std::time::Instant::now() + std::time::Duration::from_secs(60));
        let found = open.search(&q, 500, &budget).unwrap().unwrap();
        assert!(
            found.iter().any(|c| c.unit_id == format!("{:032x}", 19)),
            "{found:?}"
        );
        assert_eq!(found.len(), 20);
    }

    /// A text cut at MAX_ITEMS inside an `or` chain leaves the whole chain out of the index's query
    /// (Postgres checks it on every candidate): `… w32 or w33` would otherwise require `w32`. A text
    /// that is one chain is read whole instead, as main's index reads it, up to MAX_PIECES words.
    #[test]
    fn a_cut_inside_an_or_chain_drops_the_chain() {
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        let head: Vec<String> = (0..MAX_ITEMS - 1).map(|n| format!("w{n}")).collect();
        let words = |text: &str| {
            items(&open.index, text)
                .iter()
                .filter(|i| !matches!(i, Item::Or))
                .count()
        };
        assert_eq!(words(&format!("{} last", head.join(" "))), MAX_ITEMS);
        assert_eq!(words(&format!("{} a or b", head.join(" "))), MAX_ITEMS - 1);
        let shorter = &head[..MAX_ITEMS - 3];
        assert_eq!(
            words(&format!("{} a or b or c", shorter.join(" "))),
            MAX_ITEMS - 3,
            "a or b is cut before c: the chain goes"
        );
        assert_eq!(
            words(&format!("{} a or b", shorter.join(" "))),
            MAX_ITEMS - 1
        );
        // A text that is one chain of `or`s is read whole, as main's index reads it (with no cut),
        // up to MAX_PIECES words: 16 words are 31 items, 20 are cut with `or` the 32nd and read on;
        // a longer chain leaves the query (dropping it leaves no word to find).
        let chain = |n: usize| {
            (0..n)
                .map(|n| format!("c{n}"))
                .collect::<Vec<_>>()
                .join(" or ")
        };
        assert_eq!(words(&chain(16)), 16);
        assert_eq!(words(&chain(20)), 20, "read whole, as main reads it");
        assert_eq!(words(&chain(MAX_PIECES)), MAX_PIECES);
        assert_eq!(words(&chain(MAX_PIECES + 6)), 0);
        // A chain that goes on past the cut through a `-word`: the cut is inside it.
        assert_eq!(
            words(&format!("{} a -n or b", head.join(" "))),
            MAX_ITEMS - 1
        );
        // … or through a piece of no word to find (`""`).
        assert_eq!(
            words(&format!("{} a \"\" or b", head.join(" "))),
            MAX_ITEMS - 1
        );
        // `or` right after a closing quote, past the cut: the quoted phrase it joins goes.
        assert_eq!(
            words(&format!("{} \"p q\"or z", head.join(" "))),
            MAX_ITEMS - 1
        );
        // A word left out between the chain's items is no part of it, and goes with it.
        assert_eq!(
            words(&format!("{} a -n or b", head[..MAX_ITEMS - 3].join(" "))),
            MAX_ITEMS - 3
        );
    }

    /// A word start expands to the words the most names hold while the postings they hold, in all,
    /// stay within PREFIX_POSTINGS (a word set reads every posting of its words in one go); a
    /// phrase whose rarest word more names hold than PHRASE_POSTINGS is matched as all its words,
    /// each read a step at a time, and a phrase leaf checks the budget at every step.
    #[test]
    fn expansions_and_phrases_are_bounded_by_the_postings_they_read() {
        let open = Open::new(
            index(),
            0,
            BTreeMap::new(),
            String::new(),
            Default::default(),
        )
        .unwrap();
        let per = PREFIX_POSTINGS / 4 + 1;
        let mut names: Vec<String> = (0..6)
            .flat_map(|n| (0..per).map(move |_| format!("b{n} york new")))
            .collect();
        names.push("b9".to_owned());
        named(&open, &names);
        let searcher = open.reader.searcher();
        let terms = open.prefix_terms(&searcher, open.fields.name, "b");
        let postings: u64 = terms.iter().map(|t| searcher.doc_freq(t).unwrap()).sum();
        assert!(postings <= PREFIX_POSTINGS, "{postings}");
        assert_eq!(
            terms.len(),
            4,
            "three of the frequent words and the rare one"
        );
        let words = ["new".to_owned(), "york".to_owned()];
        let query = format!("{:?}", open.words_in(&searcher, open.fields.name, &words));
        assert!(!query.contains("PhraseQuery"), "{query}");
        let rare = ["b9".to_owned(), "york".to_owned()];
        let budget = Budget::new(std::time::Instant::now() + std::time::Duration::from_secs(5));
        let _in = InBudget::new(&budget);
        let phrase = open.words_in(&searcher, open.fields.name, &rare);
        assert!(format!("{phrase:?}").contains("PhraseQuery"));
        assert_eq!(
            format!("{phrase:?}").matches("every: 1 }").count(),
            1,
            "{phrase:?}"
        );
    }

    /// A merge owed for a removal is recorded by the commit that deletes the document, so an index
    /// opened again (a restart before the merge ran) merges it; the merge then records that nothing
    /// is owed.
    #[test]
    fn an_owed_merge_outlives_a_restart() {
        let index = index();
        let open = Arc::new(
            Open::new(
                index.clone(),
                0,
                BTreeMap::new(),
                String::new(),
                Default::default(),
            )
            .unwrap(),
        );
        docs(&open, &[("a", "aurora", ""), ("b", "borealis", "")]);
        docs(&open, &[("c", "cosmos", "")]);
        open.reader.reload().unwrap();
        // Another merge runs: this removal's merge is owed, not started.
        open.purging.lock().unwrap().running = true;
        open.replace(&["a".to_owned()], &[], 0).unwrap();
        let payload = |index: &Index| -> Payload {
            serde_json::from_str(index.load_metas().unwrap().payload.as_deref().unwrap()).unwrap()
        };
        let recorded = payload(&index);
        assert_eq!(
            (recorded.owed.clone(), recorded.owed_all),
            (vec!["a".to_owned()], false)
        );
        assert!(holds(&open, "aurora"), "its words are still in the files");
        drop(open);
        // The restart: opened from the files, the merge is owed and runs at the next pass.
        let open = Arc::new(
            Open::new(
                index.clone(),
                0,
                BTreeMap::new(),
                String::new(),
                (recorded.owed, recorded.owed_all),
            )
            .unwrap(),
        );
        assert!(open.purge_owed());
        {
            let mut writer = open.writer.lock().unwrap();
            open.purge(&mut writer, vec![], false).unwrap();
        }
        if let Some(waiting) = open.merging.lock().unwrap().take() {
            waiting.join().unwrap();
        }
        open.reader.reload().unwrap();
        assert!(!holds(&open, "aurora"));
        assert!(holds(&open, "cosmos"));
        let cleared = payload(&index);
        assert!(cleared.owed.is_empty() && !cleared.owed_all);
        assert!(!open.purge_owed());
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
