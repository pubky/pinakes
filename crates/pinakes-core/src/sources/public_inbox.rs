//! Mailing-list messages in public-inbox archives (gnusha.org, lore.kernel.org and any other),
//! read from the message's own mbox (`{list}/{message-id}/raw`), keyed on its Message-ID within
//! the archive. The old `lists.linuxfoundation.org` pipermail addresses that gnusha.org maps to
//! a message become aliases of it. No key. An archive that does not answer as public-inbox is
//! remembered, and its links get ordinary web cards.
use super::{web_url, Context, Declined, Example, HostMemo, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use base64::Engine as _;
use std::{collections::BTreeMap, sync::LazyLock};

pub struct PublicInbox;

/// Which hosts answered as public-inbox, and which did not.
static ARCHIVES: LazyLock<HostMemo> = LazyLock::new(HostMemo::default);

/// Archives known without asking; their message addresses merge from the link alone.
const KNOWN: &[&str] = &[
    "gnusha.org",
    "inbox.sourceware.org",
    "lore.kernel.org",
    "public-inbox.org",
];
/// The retired list server whose message addresses gnusha.org maps to its archive.
const PIPERMAIL: &str = "lists.linuxfoundation.org";
/// gnusha.org's map from an old list address to the message (`/url/{old address}`).
const MAPPER: &str = "https://gnusha.org/url/https:/";

/// A path segment, as public-inbox writes a Message-ID into one.
const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

fn decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}
/// A Message-ID without its angle brackets: `local@domain`.
fn message_id(value: &str) -> Option<String> {
    let id = decode(value);
    let (local, domain) = id.rsplit_once('@')?;
    (!local.is_empty()
        && !domain.is_empty()
        && id.len() <= 250
        && !id
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>/".contains(c)))
    .then_some(id)
}
fn path_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// What a list link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A message in an archive: `https://{host}{base}/{message-id}/` (`base` ends with the list).
    Message {
        host: String,
        base: String,
        mid: String,
    },
    /// An old pipermail address, e.g. `/pipermail/bitcoin-dev/2023-May/021637.html`.
    Pipermail { path: String },
}

impl Target {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if url.scheme() != "https" || url.port().is_some() {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        if host == PIPERMAIL {
            return match segments.as_slice() {
                ["pipermail", list, month, file]
                    if path_part(list)
                        && path_part(month)
                        && file.strip_suffix(".html").is_some_and(|n| {
                            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                        }) =>
                {
                    Some(Self::Pipermail {
                        path: url.path().to_owned(),
                    })
                }
                _ => None,
            };
        }
        let at = segments.iter().rposition(|s| message_id(s).is_some())?;
        let (before, after) = (&segments[..at], &segments[at + 1..]);
        if before.is_empty()
            || before.len() > 4
            || !before.iter().all(|s| path_part(s))
            || !matches!(after, [] | ["T"] | ["t"] | ["raw"])
        {
            return None;
        }
        Some(Self::Message {
            host,
            base: format!("/{}", before.join("/")),
            mid: message_id(segments[at])?,
        })
    }
    /// The message's page in its archive.
    pub fn root(&self) -> Option<String> {
        let Self::Message { host, base, mid } = self else {
            return None;
        };
        Some(format!(
            "https://{host}{base}/{}/",
            percent_encoding::utf8_percent_encode(mid, SEGMENT)
        ))
    }
}

/// A message as its mbox headers and first text part say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub subject: Option<String>,
    /// The sender's display name; the address is never kept.
    pub author: Option<String>,
    pub date: Option<String>,
    pub list: Option<String>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub excerpt: Option<String>,
}

/// Headers as (lowercase name → first value), unfolded; and the body after them.
fn headers(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut map = BTreeMap::new();
    let mut current: Option<(String, String)> = None;
    let mut rest = "";
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            rest = &text[offset..];
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = current.take() {
            map.entry(name).or_insert(value);
        }
        if let Some((name, value)) = line.split_once(':') {
            current = Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    if let Some((name, value)) = current {
        map.entry(name).or_insert(value);
    }
    (map, rest)
}

/// A header parameter (`charset`, `boundary`), unquoted.
fn param(value: &str, name: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().trim_matches('"').to_owned())
    })
}

fn charset(bytes: &[u8], charset: &str) -> String {
    match charset.to_ascii_lowercase().as_str() {
        "iso-8859-1" | "latin1" | "windows-1252" | "us-ascii" => {
            bytes.iter().map(|b| char::from(*b)).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn quoted_printable(text: &str, header: bool) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'=' if bytes.get(i + 1) == Some(&b'\n') => i += 2,
            b'=' if bytes.get(i + 1) == Some(&b'\r') && bytes.get(i + 2) == Some(&b'\n') => i += 3,
            b'=' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'=');
                        i += 1;
                    }
                }
            }
            b'_' if header => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// RFC 2047 encoded words (`=?UTF-8?Q?…?=`, `=?utf-8?B?…?=`) as text; whitespace between two
/// encoded words is dropped.
pub fn header_text(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut after_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, word) = rest.split_at(start);
        let decoded = (|| {
            let inner = &word[2..];
            let (cs, inner) = inner.split_once('?')?;
            let (enc, inner) = inner.split_once('?')?;
            let end = inner.find("?=")?;
            let payload = &inner[..end];
            let bytes = match enc {
                "B" | "b" => base64::engine::general_purpose::STANDARD
                    .decode(payload.trim())
                    .ok()?,
                "Q" | "q" => quoted_printable(payload, true),
                _ => return None,
            };
            let consumed = 2 + cs.len() + 1 + enc.len() + 1 + end + 2;
            Some((
                charset(&bytes, cs.split('*').next().unwrap_or(cs)),
                consumed,
            ))
        })();
        match decoded {
            Some((text, consumed)) => {
                if !(after_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = &word[consumed..];
                after_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = &word[2..];
                after_word = false;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first `text/plain` part of a body, decoded.
fn plain_part(headers: &BTreeMap<String, String>, body: &str, depth: u8) -> Option<String> {
    let kind = headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| "text/plain".into());
    let lower = kind.to_ascii_lowercase();
    if lower.starts_with("multipart/") {
        let boundary = param(&kind, "boundary")?;
        if depth > 3 {
            return None;
        }
        let marker = format!("--{boundary}");
        return body.split(marker.as_str()).skip(1).find_map(|part| {
            if part.starts_with("--") {
                return None;
            }
            let part = part.trim_start_matches(['\r', '\n']);
            let (h, b) = self::headers(part);
            plain_part(&h, b, depth + 1)
        });
    }
    if !lower.starts_with("text/plain") {
        return None;
    }
    let encoding = headers
        .get("content-transfer-encoding")
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let bytes = match encoding.as_str() {
        "quoted-printable" => quoted_printable(body, false),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(body.split_whitespace().collect::<String>())
            .ok()?,
        _ => body.as_bytes().to_vec(),
    };
    Some(charset(
        &bytes,
        &param(&kind, "charset").unwrap_or_default(),
    ))
}

/// The opening words of a message's own text: quoted lines, attributions ("On … wrote:"), the
/// signature and list footers left out.
fn excerpt(text: &str) -> Option<String> {
    let mut words = String::new();
    // In-body pseudo-headers (`From: Name <address>` when the sender is not the author) come
    // first; trailers (`Signed-off-by: Name <address>`) anywhere.
    let mut head = true;
    for line in text.lines() {
        let line = line.trim_end();
        if line == "-- " || line == "--" || line.starts_with("You received this message because") {
            break;
        }
        let trimmed = line.trim();
        if head && trimmed.is_empty() {
            continue;
        }
        let field = trimmed
            .split_once(':')
            .map(|(name, _)| name.to_ascii_lowercase())
            .filter(|name| {
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')
            });
        if head
            && field
                .as_deref()
                .is_some_and(|f| matches!(f, "from" | "date" | "subject" | "to" | "cc"))
        {
            continue;
        }
        head = false;
        if field.as_deref().is_some_and(|f| {
            f.ends_with("-by") || matches!(f, "cc" | "to" | "link" | "closes" | "fixes")
        }) && trimmed.contains(['@', '<'])
        {
            continue;
        }
        if trimmed.is_empty()
            || trimmed.starts_with('>')
            || trimmed.ends_with("wrote:")
            || trimmed.ends_with("writes:")
        {
            continue;
        }
        if !words.is_empty() {
            words.push(' ');
        }
        words.push_str(trimmed);
        if words.len() > 400 {
            break;
        }
    }
    let words = super::without_addresses(&words);
    let mut cut: String = words.chars().take(200).collect();
    if cut.len() < words.len() {
        cut.push('…');
    }
    (!cut.is_empty()).then_some(cut)
}

fn bracketed(value: &str) -> Option<String> {
    let inner = value.split('<').nth(1)?.split('>').next()?.trim();
    (!inner.is_empty()).then(|| inner.to_owned())
}

/// Read a message from its mbox (`/raw`).
pub fn parse(raw: &[u8]) -> Result<Message> {
    let text = String::from_utf8_lossy(raw);
    let text = match text.strip_prefix("From ") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, r)| r),
        None => &text,
    };
    let (headers, body) = self::headers(text);
    anyhow::ensure!(headers.contains_key("message-id"), "no message headers");
    let author = headers.get("from").and_then(|from| {
        let from = header_text(from);
        let name = from.split('<').next()?.trim().trim_matches('"').trim();
        (!name.is_empty() && !name.contains('@')).then(|| name.to_owned())
    });
    let date = headers
        .get("date")
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d.trim()).ok())
        .map(|d| d.to_rfc3339());
    Ok(Message {
        subject: headers
            .get("subject")
            .map(|s| header_text(s))
            .filter(|s| !s.is_empty()),
        author,
        date,
        list: headers.get("list-id").and_then(|l| bracketed(l)),
        message_id: headers.get("message-id").and_then(|m| bracketed(m)),
        in_reply_to: headers.get("in-reply-to").and_then(|m| bracketed(m)),
        excerpt: plain_part(&headers, body, 0).and_then(|t| excerpt(&t)),
    })
}

/// A message's card.
pub fn card(mut doc: ResourceMeta, target: &Target, message: Message) -> Result<ResourceMeta> {
    let Target::Message { mid, .. } = target else {
        anyhow::bail!("not a message");
    };
    anyhow::ensure!(
        message.message_id.as_deref() == Some(mid.as_str()),
        "the archive answered for another message"
    );
    let root = target.root().context("a message has a root")?;
    doc.set_type("EmailMessage");
    doc.name = message.subject;
    doc.description = message.excerpt;
    for (key, value) in [
        ("author", message.author),
        ("published", message.date),
        ("list", message.list),
        ("message_id", Some(mid.clone())),
        ("in_reply_to", message.in_reply_to),
        ("page_url", Some(root.clone())),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    doc.source_root = Some(root);
    Ok(doc)
}

#[async_trait]
impl Source for PublicInbox {
    fn name(&self) -> &'static str {
        "public_inbox"
    }
    fn reads(&self, uri: &str) -> bool {
        match Target::of_uri(uri) {
            Some(Target::Message { host, .. }) => ARCHIVES.get(&host) != Some(false),
            Some(Target::Pipermail { .. }) => true,
            None => false,
        }
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        // `/T/`, `/t/` and `/raw` name the message, in the archives known without asking.
        let target = Target::of_uri(uri)?;
        let Target::Message { host, .. } = &target else {
            return None;
        };
        if !KNOWN.contains(&host.as_str()) {
            return None;
        }
        let root = target.root()?;
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "author",
            "published",
            "list",
            "message_id",
            "in_reply_to",
            "page_url",
        ]
    }
    fn social(&self, _doc: &ResourceMeta) -> bool {
        true
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://gnusha.org/pi/bitcoindev/atCSAVprBE-tAnN3@erisian.com.au/",
                card_type: "EmailMessage",
            },
            Example {
                uri: "https://lore.kernel.org/git/pull.2458.git.git.1792437798463.gitgitgadget@gmail.com/",
                card_type: "EmailMessage",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let mut target = Target::of_uri(uri).context("not a list message")?;
        if let Target::Pipermail { path } = &target {
            // gnusha.org redirects an old address to the message it archived, if any.
            let f = cx
                .fetch(
                    &format!("{MAPPER}{PIPERMAIL}{path}"),
                    &FetchOptions {
                        accept: Some("text/html".into()),
                        ..Default::default()
                    },
                )
                .await?;
            target = match Target::of_uri(f.url.as_str()) {
                Some(t @ Target::Message { .. }) if f.status == 200 => t,
                _ => return Err(Declined.into()),
            };
        }
        let Target::Message { host, .. } = &target else {
            unreachable!("pipermail addresses became messages above")
        };
        let host = host.clone();
        if ARCHIVES.get(&host) == Some(false) {
            return Err(Declined.into());
        }
        let known = KNOWN.contains(&host.as_str()) || ARCHIVES.get(&host) == Some(true);
        let raw = format!("{}raw", target.root().context("a message has a root")?);
        let f = match cx
            .fetch(
                &raw,
                &FetchOptions {
                    accept: Some("application/mbox, text/plain;q=0.5".into()),
                    extra_types: &["application/mbox", "text/plain"],
                    ..Default::default()
                },
            )
            .await
        {
            // A message too large to read (attachments): its page gets an ordinary web card.
            Err(e)
                if e.chain()
                    .any(|c| c.is::<crate::resolver::egress::BodyTooLarge>()) =>
            {
                return Err(Declined.into())
            }
            f => f?,
        };
        // lore.kernel.org serves the mbox as `text/plain`; it still starts with a `From ` line.
        let mbox = f.content_type.contains("mbox")
            || (f.content_type.starts_with("text/plain") && f.body.starts_with(b"From "));
        match f.status {
            200 if mbox => {}
            404 if known => {
                let mut doc = cx.missing(uri, self);
                doc.site = host;
                return Ok(doc);
            }
            _ => {
                if !known {
                    ARCHIVES.set(&host, false);
                }
                return Err(Declined.into());
            }
        }
        ARCHIVES.set(&host, true);
        let mut doc = card(cx.card(uri, self), &target, parse(&f.body)?)?;
        doc.site = host;
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::Status, sources::fixture};

    const TOWNS: &str = "https://gnusha.org/pi/bitcoindev/atCSAVprBE-tAnN3@erisian.com.au/";
    fn read(file: &str) -> Vec<u8> {
        fixture::read("public_inbox", file)
    }
    fn base(uri: &str) -> ResourceMeta {
        ResourceMeta::new(uri, "public_inbox", 3600)
    }

    #[test]
    fn addresses() {
        let message = Target::of_uri(TOWNS).unwrap();
        assert_eq!(
            message,
            Target::Message {
                host: "gnusha.org".into(),
                base: "/pi/bitcoindev".into(),
                mid: "atCSAVprBE-tAnN3@erisian.com.au".into()
            }
        );
        assert_eq!(message.root().as_deref(), Some(TOWNS));
        for spelling in ["T/", "t/", "raw", "T/#u"] {
            let uri = format!("{TOWNS}{spelling}");
            assert_eq!(
                PublicInbox.alias_root(&uri).as_deref(),
                Some(TOWNS),
                "{uri}"
            );
        }
        assert_eq!(
            PublicInbox
                .alias_root("https://gnusha.org/pi/bitcoindev/atCSAVprBE-tAnN3@erisian.com.au"),
            Some(TOWNS.into())
        );
        assert_eq!(PublicInbox.alias_root(TOWNS), None);
        // `$` and `=` stay as public-inbox writes them; `/` in a Message-ID is encoded.
        let odd = Target::of_uri(
            "https://gnusha.org/pi/bitcoindev/02c201dce227$e808e050$b81aa0f0$@voskuil.org/",
        )
        .unwrap();
        assert_eq!(
            odd.root().as_deref(),
            Some("https://gnusha.org/pi/bitcoindev/02c201dce227$e808e050$b81aa0f0$@voskuil.org/")
        );
        // Another archive is read too, but its spellings merge only after it answers.
        assert!(PublicInbox.reads("https://lists.example.org/devel/abc@example.org/"));
        assert_eq!(
            PublicInbox.alias_root("https://lists.example.org/devel/abc@example.org/T/"),
            None
        );
        assert_eq!(
            Target::of_uri(
                "https://lists.linuxfoundation.org/pipermail/bitcoin-dev/2023-May/021637.html"
            ),
            Some(Target::Pipermail {
                path: "/pipermail/bitcoin-dev/2023-May/021637.html".into()
            })
        );
        for uri in [
            "https://medium.com/@someone/a-post-123",
            "https://mastodon.social/@Gargron/1",
            "https://gnusha.org/abc@example.org/",
            "https://gnusha.org/pi/bitcoindev/abc@example.org/T/extra",
            "https://gnusha.org/pi/bitcoindev/@example.org/",
            "https://lists.linuxfoundation.org/pipermail/bitcoin-dev/",
            "http://gnusha.org/pi/bitcoindev/atCSAVprBE-tAnN3@erisian.com.au/",
        ] {
            assert!(!PublicInbox.reads(uri), "{uri}");
        }
    }

    #[test]
    fn headers_and_bodies() {
        assert_eq!(
            header_text("=?UTF-8?Q?Re=3A_caf=C3=A9?= =?utf-8?B?IGF1IGxhaXQ=?= ok"),
            "Re: café au lait ok"
        );
        assert_eq!(
            header_text("=?iso-8859-1?q?J=FCrgen?= <j@x>"),
            "Jürgen <j@x>"
        );
        assert_eq!(header_text("plain =?bogus"), "plain =?bogus");
        assert_eq!(quoted_printable("a=3Db=\nc=ZZ", false), b"a=bc=ZZ");
        let qp = "Message-ID: <q@x>\nContent-Type: text/plain; charset=utf-8\n\
                  Content-Transfer-Encoding: quoted-printable\n\nOn Monday someone wrote:\n> quoted\n\
                  Caf=C3=A9 au lait is=\n good.\n-- \nsig";
        assert_eq!(
            parse(qp.as_bytes()).unwrap().excerpt.as_deref(),
            Some("Café au lait is good.")
        );
        let b64 =
            "Message-ID: <b@x>\nContent-Type: multipart/alternative; boundary=\"zz\"\n\n--zz\n\
                   Content-Type: text/html\n\n<p>html</p>\n--zz\nContent-Type: text/plain\n\
                   Content-Transfer-Encoding: base64\n\nSGVsbG8gdGhlcmU=\n--zz--\n";
        assert_eq!(
            parse(b64.as_bytes()).unwrap().excerpt.as_deref(),
            Some("Hello there")
        );
        assert!(parse(b"<html>not a message</html>").is_err());
    }

    #[test]
    fn messages() {
        let m = parse(&read("raw-towns.mbox")).unwrap();
        assert_eq!(
            m.subject.as_deref(),
            Some("Re: [bitcoindev] [BIP Draft] P2P UTXO Set Sharing")
        );
        assert_eq!(m.author.as_deref(), Some("Anthony Towns"));
        assert_eq!(m.date.as_deref(), Some("2026-10-15T18:42:41+10:00"));
        assert_eq!(m.list.as_deref(), Some("bitcoindev.googlegroups.com"));
        assert!(m.in_reply_to.unwrap().ends_with("@protonmail.com"));
        let excerpt = m.excerpt.unwrap();
        assert!(
            !excerpt.contains('>') && excerpt.chars().count() <= 201,
            "{excerpt}"
        );

        let target = Target::of_uri(TOWNS).unwrap();
        let doc = card(
            base(TOWNS),
            &target,
            parse(&read("raw-towns.mbox")).unwrap(),
        )
        .unwrap();
        assert_eq!(doc.type_name, "EmailMessage");
        assert_eq!(
            doc.attributes["message_id"],
            "atCSAVprBE-tAnN3@erisian.com.au"
        );
        // No address reaches the card.
        assert!(!serde_json::to_string(&doc.attributes)
            .unwrap()
            .contains("aj@"));
        let other = Target::of_uri("https://gnusha.org/pi/bitcoindev/other@x.org/").unwrap();
        assert!(card(base(TOWNS), &other, parse(&read("raw-towns.mbox")).unwrap()).is_err());

        // No address reaches the description either: in-body headers, trailers and any address
        // left in the text are dropped.
        for file in [
            "raw-towns.mbox",
            "raw-gitgitgadget.mbox",
            "raw-toaster.mbox",
        ] {
            let excerpt = parse(&read(file)).unwrap().excerpt.unwrap();
            assert_eq!(
                crate::sources::without_addresses(&excerpt),
                excerpt,
                "{file}"
            );
            assert!(!excerpt.contains("From:"), "{file}: {excerpt}");
        }
        let gadget = parse(&read("raw-gitgitgadget.mbox"))
            .unwrap()
            .excerpt
            .unwrap();
        assert!(!gadget.contains('@'), "{gadget}");
        let patch = "Message-ID: <p@x>\nContent-Type: text/plain\n\nFrom: Ann Author <ann@example.org>\n\
                     Date: Mon, 1 Jan 2026\nSubject: [PATCH] fix\n\nFix the thing; ask bob@example.org.\n\n\
                     Signed-off-by: Ann Author <ann@example.org>\nCc: Bob <bob@example.org>\n\
                     Reported-by: carol@example.net\nOn Monday, Dan <dan@example.com>\nwrote:\n\
                     Eve <eve@example.com> writes:\n";
        let excerpt = parse(patch.as_bytes()).unwrap().excerpt.unwrap();
        assert_eq!(excerpt, "Fix the thing; ask On Monday, Dan");
        let only_trailers = "Message-ID: <q@x>\n\nSigned-off-by: Ann <ann@example.org>\n";
        assert_eq!(parse(only_trailers.as_bytes()).unwrap().excerpt, None);

        // A folded subject, a "via" sender and a patch's own From line in the body.
        let m = parse(&read("raw-gitgitgadget.mbox")).unwrap();
        assert_eq!(
            m.author.as_deref(),
            Some("Harald Nordgren via GitGitGadget")
        );
        assert!(m
            .subject
            .unwrap()
            .starts_with("[PATCH] status: compare against both upstream and push branch by "));
        assert_eq!(m.list.as_deref(), Some("git.vger.kernel.org"));
        // A signed message with nested parts.
        let m = parse(&read("raw-toaster.mbox")).unwrap();
        assert_eq!(m.author.as_deref(), Some("angus"));
        assert!(m.excerpt.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn pipermail_addresses_become_their_message() {
        let mid = "Ny5lrlyxNcdDUKpy8_vYd8RRl5Q6brWkPDkaxRzjCgM7JF8HXRIZnfs1ubvmIA_OhwYwmdthSXyGsJ8CP8qQ0x0i82qbzlehNzuVtefSeHc=@toaster.cc";
        let message = format!("https://gnusha.org/pi/bitcoindev/{mid}/");
        let (egress, _) = fixture::egress(vec![
            (
                "https://gnusha.org/url/https:/lists.linuxfoundation.org/pipermail/bitcoin-dev/2023-May/021637.html",
                302,
                "text/html",
                message.clone().into_bytes(),
            ),
            (&message, 200, "text/html", b"<html>the message page</html>".to_vec()),
            (&format!("{message}raw"), 200, "application/mbox; charset=UTF-8", read("raw-toaster.mbox")),
            (
                "https://lore.kernel.org/git/pull.2458.git.git.1792437798463.gitgitgadget@gmail.com/raw",
                200,
                "text/plain; charset=UTF-8",
                read("raw-gitgitgadget.mbox"),
            ),
            (
                "https://gnusha.org/pi/bitcoindev/no-such-message@example.org/raw",
                404,
                "text/plain",
                read("raw-missing.html"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let old = "https://lists.linuxfoundation.org/pipermail/bitcoin-dev/2023-May/021637.html";
        let doc = PublicInbox.resolve(&cx, old).await.unwrap();
        assert_eq!(doc.source_root.as_deref(), Some(message.as_str()));
        assert_eq!(
            doc.attributes["list"],
            "bitcoin-dev.lists.linuxfoundation.org"
        );
        assert_eq!(doc.site, "gnusha.org");
        let doc = PublicInbox
            .resolve(
                &cx,
                "https://lore.kernel.org/git/pull.2458.git.git.1792437798463.gitgitgadget@gmail.com/T/",
            )
            .await
            .unwrap();
        // lore.kernel.org's mbox comes as text/plain.
        assert_eq!(doc.attributes["list"], "git.vger.kernel.org");
        let doc = PublicInbox
            .resolve(
                &cx,
                "https://gnusha.org/pi/bitcoindev/no-such-message@example.org/",
            )
            .await
            .unwrap();
        assert_eq!(doc.status, Status::Missing);
        // Another host that does not answer as public-inbox: an ordinary web card from now on.
        let other = "https://lists.example.org/devel/abc@example.org/";
        assert!(PublicInbox
            .resolve(&cx, other)
            .await
            .unwrap_err()
            .is::<Declined>());
        assert!(!PublicInbox.reads(other));
    }
}
