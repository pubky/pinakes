//! How two cards' names are compared when nothing exact joins them: a recording's title and an
//! artist's name in the form the possible-duplicates signal and the duplicates reports read.
//! Never a key: a normalised name equal to another is a candidate, and only beside facts that
//! agree (the artist, a length within seconds).
//!
//! The same-recording pass stores each recording's keys (`same_recording_unit`), so a report
//! can group by them.
//!
//! A title is lower-cased, and its tags (in brackets, or after ` - ` as Spotify writes them)
//! are read one by one: a tag that keeps the recording's identity goes ("Remastered 2011",
//! "2011 Remaster", "Explicit", "Album Version", "Deluxe", "Bonus Track", "From the X
//! Soundtrack", a `feat.`/`with` credit); a tag that names another recording stays ("Live",
//! "Remix", "Radio Edit", "Acoustic", "Instrumental", "Demo", "Extended", "Mono", "Stereo",
//! "Part 2", "Taylor's Version"). A credit after the title (`Song feat. X`) goes. Then `&` is
//! `and`, punctuation is space, and spaces collapse. Diacritics stay: services spell one title
//! alike. An artist credit keeps its first artist (before `feat.`, `&`, a comma, `with`, `x`,
//! `vs`), lower-cased, punctuation as space.

/// Words that name another recording: a tag holding one stays in the title.
const CHANGING: &[&str] = &[
    "live",
    "remix",
    "remixed",
    "remixes",
    "mix",
    "edit",
    "edits",
    "edited",
    "acoustic",
    "unplugged",
    "instrumental",
    "instrumentals",
    "acapella",
    "demo",
    "demos",
    "extended",
    "mono",
    "stereo",
    "part",
    "pt",
    "vol",
    "volume",
    "cover",
    "karaoke",
    "dub",
    "rework",
    "reworked",
    "reprise",
    "session",
    "sessions",
    "take",
    "alternate",
    "alternative",
    "orchestral",
    "radio",
    "single",
    "version",
    "rehearsal",
    "outtake",
    "bootleg",
    "medley",
    "slowed",
    "sped",
    "nightcore",
];
/// With `version`, the words of a tag that still names the same recording ("Album Version").
const PLAIN_VERSION: &[&str] = &["album", "original", "explicit", "clean", "version", "the"];
/// A tag starting with one of these is a credit or a soundtrack attribution: it goes.
const CREDIT_START: &[&str] = &["feat", "ft", "featuring", "with", "from"];
/// After one of these words, the rest of a title or an artist credit is a credit.
const CREDIT_WORDS: &[&str] = &["feat", "ft", "featuring"];

/// The words of a piece of text: lower-case runs of letters and digits, `&` as `and`.
fn words(text: &str) -> Vec<String> {
    text.replace('&', " and ")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}
/// Whether a tag names another recording (it stays), or keeps the identity (it goes).
fn tag_stays(tag: &str) -> bool {
    let words = words(tag);
    let Some(first) = words.first() else {
        return false;
    };
    if CREDIT_START.contains(&first.as_str()) {
        return false;
    }
    words.iter().any(|w| match w.as_str() {
        "mix" => !(words.len() == 2 && words.contains(&"original".to_owned())),
        "version" => !words.iter().all(|w| PLAIN_VERSION.contains(&w.as_str())),
        w => CHANGING.contains(&w),
    })
}
/// Lower-case `text` up to its first credit (`feat.`, `ft.`, `featuring` as a word after the
/// first).
fn before_credit(text: &str) -> &str {
    let mut start = None;
    for (at, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (start, c.is_alphanumeric()) {
            (None, true) => start = Some(at),
            (Some(from), false) => {
                if from > 0 && CREDIT_WORDS.contains(&&text[from..at]) {
                    return &text[..from];
                }
                start = None;
            }
            _ => {}
        }
    }
    text
}
/// A bracketed tag: the text inside the innermost `(…)` or `[…]` from `open`, when the group
/// closes before another opens (as the SQL function's pattern reads it).
fn group_end(chars: &[char], open: usize) -> Option<usize> {
    chars[open + 1..]
        .iter()
        .position(|c| matches!(c, '(' | ')' | '[' | ']'))
        .map(|n| open + 1 + n)
        .filter(|&end| matches!(chars[end], ')' | ']'))
}
/// The bracketed tags of `text`, and the text with them blanked.
fn bracketed(text: &str) -> (Vec<String>, String) {
    let chars: Vec<char> = text.chars().collect();
    let mut tags = vec![];
    let mut rest = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '(' | '[') {
            if let Some(end) = group_end(&chars, i) {
                tags.push(chars[i + 1..end].iter().collect());
                rest.push(' ');
                i = end + 1;
                continue;
            }
        }
        rest.push(chars[i]);
        i += 1;
    }
    (tags, rest)
}
/// The pieces of `text` around ` - ` (a hyphen, an en or an em dash between spaces).
fn dashed(text: &str) -> Vec<String> {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut pieces = vec![];
    let mut rest = collapsed.as_str();
    loop {
        let cut = [" - ", " – ", " — "]
            .iter()
            .filter_map(|d| rest.find(d).map(|at| (at, d.len())))
            .min();
        match cut {
            Some((at, len)) => {
                pieces.push(rest[..at].to_owned());
                rest = &rest[at + len..];
            }
            None => {
                pieces.push(rest.to_owned());
                return pieces;
            }
        }
    }
}
/// Words joined by one space.
fn plain(text: &str) -> String {
    words(text).join(" ")
}

/// A recording's title as two cards are compared (the module's rules).
pub fn recording_title(title: &str) -> String {
    let lower = title.to_lowercase();
    let (tags, rest) = bracketed(&lower);
    let mut pieces = dashed(&rest);
    let base = pieces.remove(0);
    let mut out = plain(before_credit(&base));
    for tag in tags.iter().chain(&pieces) {
        let tag = before_credit(tag);
        if tag_stays(tag) {
            let tag = plain(tag);
            if !tag.is_empty() {
                out.push(' ');
                out.push_str(&tag);
            }
        }
    }
    out.trim().to_owned()
}

/// The first credited artist of a credit ("Queen", "A feat. B", "A & B", "A, B"), as two cards
/// are compared.
pub fn artist_name(credit: &str) -> String {
    let lower = credit.to_lowercase();
    let first = before_credit(&lower);
    let mut first = first;
    for separator in [" & ", ", ", " with ", " x ", " vs ", " vs. ", " / "] {
        if let Some(at) = first.find(separator) {
            first = &first[..at];
        }
    }
    plain(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tags that keep a recording's identity go; tags that name another recording stay; credits
    /// and soundtrack attributions go wherever they are.
    #[test]
    fn titles_lose_their_identity_keeping_tags_and_keep_the_others() {
        for (title, expected) in [
            ("Bohemian Rhapsody - Remastered 2011", "bohemian rhapsody"),
            ("Bohemian Rhapsody (2011 Remaster)", "bohemian rhapsody"),
            ("Bohemian Rhapsody", "bohemian rhapsody"),
            ("Bohemian Rhapsody (Live Aid)", "bohemian rhapsody live aid"),
            (
                "Bohemian Rhapsody - Live at Wembley '86",
                "bohemian rhapsody live at wembley 86",
            ),
            (
                "Lose Yourself - From \"8 Mile\" Soundtrack",
                "lose yourself",
            ),
            (
                "Lose Yourself (Soundtrack Version)",
                "lose yourself soundtrack version",
            ),
            (
                "Never Gonna Give You Up (feat. Someone)",
                "never gonna give you up",
            ),
            (
                "Never Gonna Give You Up feat. Someone",
                "never gonna give you up",
            ),
            (
                "Never Gonna Give You Up ft. Someone - Remastered",
                "never gonna give you up",
            ),
            ("Take Five (Mono)", "take five mono"),
            ("Take Five [Explicit]", "take five"),
            ("Song (Album Version)", "song"),
            ("Song (Taylor's Version)", "song taylor s version"),
            ("Song (Original Mix)", "song"),
            ("Song (Club Mix)", "song club mix"),
            ("Song - Radio Edit", "song radio edit"),
            ("Song - Single Version", "song single version"),
            ("Song (Deluxe Edition) [Bonus Track]", "song"),
            ("Song (with Someone)", "song"),
            ("Song (Acoustic) (feat. Someone)", "song acoustic"),
            ("Song, Pt. 2", "song pt 2"),
            ("Rock & Roll", "rock and roll"),
            ("Don't Stop Me Now – 2011 Remaster", "don t stop me now"),
            ("Ænima (Instrumental)", "ænima instrumental"),
            ("Song (feat. Someone (Remix))", "song remix"),
            ("Song (unclosed", "song unclosed"),
            ("", ""),
        ] {
            assert_eq!(recording_title(title), expected, "{title:?}");
        }
    }

    #[test]
    fn an_artist_credit_keeps_its_first_artist() {
        for (credit, expected) in [
            ("Queen", "queen"),
            ("Queen & David Bowie", "queen"),
            ("Rick Astley feat. Someone", "rick astley"),
            ("A, B and C", "a"),
            ("Simon & Garfunkel", "simon"),
            ("Jay-Z x Kanye West", "jay z"),
            ("Beyoncé", "beyoncé"),
            ("", ""),
        ] {
            assert_eq!(artist_name(credit), expected, "{credit:?}");
        }
    }
}
