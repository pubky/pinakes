//! Questions and their answers (`Question`): from a Q&A page's own JSON-LD (`QAPage` and its
//! `Question`), a Q&A site's API (`sources::stack_exchange`) or a forum topic marked solved.
use crate::{model::ResourceMeta, resolver::web::origin_url};
use serde_json::Value;
use url::Url;

pub const TYPE: &str = "Question";

/// The attributes a question publishes in `facts`, beside the common `author`,
/// `published` and `tags`.
pub const FACTS: &[&str] = &[
    // Its answers, and the one its asker accepted (a link to it).
    "answer_count",
    "accepted_answer",
    // Its votes, as its site counts them.
    "score",
    // Why it was closed, and the question it repeats (links, joined with "; ").
    "closed_reason",
    "duplicate_of",
];

pub fn is_question(doc: &ResourceMeta) -> bool {
    doc.type_path.iter().any(|t| t == TYPE)
}

/// What a question says, whichever route read it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Question {
    pub title: Option<String>,
    /// Plain text.
    pub description: Option<String>,
    pub answer_count: Option<u64>,
    pub accepted_answer: Option<String>,
    pub score: Option<i64>,
    pub author: Option<String>,
    pub published: Option<String>,
    pub closed_reason: Option<String>,
    pub duplicate_of: Vec<String>,
}
impl Question {
    /// Make `doc` this question's card, keeping what it already holds.
    pub fn apply(self, doc: &mut ResourceMeta) {
        if !is_question(doc) {
            doc.set_type(TYPE);
        }
        if doc.name.is_none() {
            doc.name = self.title;
        }
        if doc.description.is_none() {
            doc.description = self.description;
        }
        let duplicate_of = (!self.duplicate_of.is_empty()).then(|| self.duplicate_of.join("; "));
        for (key, value) in [
            ("answer_count", self.answer_count.map(|n| n.to_string())),
            ("accepted_answer", self.accepted_answer),
            ("score", self.score.map(|n| n.to_string())),
            ("author", self.author),
            ("published", self.published),
            ("closed_reason", self.closed_reason),
            ("duplicate_of", duplicate_of),
        ] {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                doc.attributes.entry(key.into()).or_insert(value);
            }
        }
    }
}

/// The question a JSON-LD `Question` node describes on the page at `base`.
pub fn from_jsonld(v: &Value, base: &Url) -> Question {
    let accepted = first(&v["acceptedAnswer"]);
    Question {
        title: text(&v["name"]).or_else(|| text(&v["headline"])),
        description: text(&v["text"])
            .map(|t| {
                if t.contains('<') {
                    crate::kinds::jobs::excerpt(&t).unwrap_or_default()
                } else {
                    excerpt(&t)
                }
            })
            .filter(|t| !t.is_empty()),
        answer_count: number(&v["answerCount"]).and_then(|n| u64::try_from(n).ok()),
        accepted_answer: text(&accepted["url"])
            .or_else(|| text(&accepted["@id"]))
            .and_then(|u| origin_url(base, &u)),
        score: number(&v["upvoteCount"]),
        author: text(&first(&v["author"])["name"]).or_else(|| text(first(&v["author"]))),
        published: text(&v["dateCreated"]),
        ..Default::default()
    }
}

/// Plain text cut to 200 characters (a question quotes its asker).
pub fn excerpt(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = text.chars().take(200).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    cut
}

fn first(v: &Value) -> &Value {
    v.as_array().and_then(|a| a.first()).unwrap_or(v)
}
fn text(v: &Value) -> Option<String> {
    crate::event::page::string(v)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn number(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_question_from_its_json_ld() {
        let base: Url = "https://qa.example/q/1".parse().unwrap();
        let q = from_jsonld(
            &json!({"@type": "Question", "name": "Why is the sky blue?",
                "text": "<p>I always   wondered.</p>", "answerCount": 3, "upvoteCount": "12",
                "dateCreated": "2026-01-02T03:04:05Z", "author": {"@type": "Person", "name": "Ada"},
                "acceptedAnswer": {"@type": "Answer", "url": "/q/1#a2", "text": "Rayleigh."}}),
            &base,
        );
        assert_eq!(q.title.as_deref(), Some("Why is the sky blue?"));
        assert_eq!(q.description.as_deref(), Some("I always wondered."));
        assert_eq!(q.answer_count, Some(3));
        assert_eq!(q.score, Some(12));
        assert_eq!(
            q.accepted_answer.as_deref(),
            Some("https://qa.example/q/1#a2")
        );
        assert_eq!(q.author.as_deref(), Some("Ada"));
        let mut doc = ResourceMeta::new(base.as_str(), "jsonld", 60);
        q.apply(&mut doc);
        assert_eq!(doc.type_name, "Question");
        assert_eq!(doc.attributes["answer_count"], "3");
        assert_eq!(doc.attributes["published"], "2026-01-02T03:04:05Z");
        assert_eq!(excerpt(&"word ".repeat(100)).chars().count(), 201);
    }
}
