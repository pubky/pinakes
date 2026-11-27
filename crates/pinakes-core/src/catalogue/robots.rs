//! A site's `robots.txt` as RFC 9309 reads it, for the Tranco catalogue's homepage reads: the
//! groups of rules for the product token `pinakes` (else for `*`), and whether a path may be
//! read: the longest matching `Allow` or `Disallow` rule decides (an `Allow` wins a tie), `*`
//! matches any run of characters and a final `$` the path's end. No rule matching: allowed.
//!
//! At most [`CAP`] bytes are parsed (RFC 9309 asks for at least 500 KiB); the homepage reads ask
//! the egress for the file within that cap, so a longer file counts as a no there. Lines that
//! are no rule are left out. A group set with a pattern over [`PATTERN_CAP`] bytes or more than
//! [`RULES_CAP`] rules allows nothing (a rule left out could be the one that says no): matching
//! costs at most rules × pattern × path bytes ([`PATH_CAP`]). Nothing here panics on any input.

/// The bytes of a `robots.txt` read.
pub const CAP: usize = 512 * 1024;
/// The most rules kept for an agent (real files hold a few dozen; the largest a few thousand,
/// almost all for one path each).
pub const RULES_CAP: usize = 1000;
/// The longest pattern kept (bytes).
pub const PATTERN_CAP: usize = 512;
/// The longest path a homepage read matches against the rules (a longer one is not read).
pub const PATH_CAP: usize = 2048;
/// The product token Pinakes' User-Agent starts with.
pub const AGENT: &str = "pinakes";

/// The rules that apply to [`AGENT`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Robots {
    /// `(allow, pattern)` in file order.
    rules: Vec<(bool, String)>,
}
impl Robots {
    /// The rules of `text` for `agent` (its groups that name it, else those for `*`).
    pub fn parse(text: &[u8], agent: &str) -> Self {
        let text = String::from_utf8_lossy(&text[..text.len().min(CAP)]);
        let mut own: Vec<(bool, String)> = Vec::new();
        let mut any: Vec<(bool, String)> = Vec::new();
        let mut named_own = false;
        // The current group's agents; a rule line ends the run of agent lines.
        let mut agents: Vec<String> = Vec::new();
        let mut in_rules = false;
        // A rule left out for the caps, for us or for everyone: those rules are no longer the
        // file's, so they allow nothing (a rule past the caps may be the `Disallow` that counts).
        let (mut own_capped, mut any_capped) = (false, false);
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if in_rules {
                        agents.clear();
                        in_rules = false;
                    }
                    // The product token: letters, `-` and `_` (RFC 9309 2.2.1), compared without case.
                    let token: String = value
                        .chars()
                        .take_while(|c| {
                            c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '*'
                        })
                        .collect::<String>()
                        .to_ascii_lowercase();
                    if token == agent {
                        named_own = true;
                    }
                    agents.push(token);
                }
                "allow" | "disallow" => {
                    in_rules = true;
                    // An empty `Disallow` allows all: no rule.
                    if value.is_empty() {
                        continue;
                    }
                    let ours = agents.iter().any(|a| a == agent);
                    let everyone = agents.iter().any(|a| a == "*");
                    if value.len() > PATTERN_CAP {
                        own_capped |= ours;
                        any_capped |= everyone;
                        continue;
                    }
                    let rule = (key == "allow", value.to_owned());
                    if ours {
                        if own.len() < RULES_CAP {
                            own.push(rule.clone());
                        } else {
                            own_capped = true;
                        }
                    }
                    if everyone {
                        if any.len() < RULES_CAP {
                            any.push(rule);
                        } else {
                            any_capped = true;
                        }
                    }
                }
                _ => {}
            }
        }
        let (rules, capped) = if named_own {
            (own, own_capped)
        } else {
            (any, any_capped)
        };
        if capped {
            return Self::disallow_all();
        }
        Self { rules }
    }
    /// What the rules hold in memory, in bytes (their patterns and each rule's own).
    pub fn weight(&self) -> usize {
        // Each rule: its pattern, and its own memory (the pair in the list, the allocation).
        self.rules
            .iter()
            .map(|(_, pattern)| pattern.len() + 48)
            .sum()
    }
    /// A file that allows everything (a missing `robots.txt`).
    pub fn allow_all() -> Self {
        Self::default()
    }
    /// A file that allows nothing (a server error: RFC 9309 2.3.1.4).
    pub fn disallow_all() -> Self {
        Self {
            rules: vec![(false, "/".into())],
        }
    }
    /// Whether `path` (with its query, starting with `/`) may be read.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if matches(pattern, path) {
                let len = pattern.len();
                best = match best {
                    Some((l, a)) if l > len || (l == len && a) => Some((l, a)),
                    _ => Some((len, *allow)),
                };
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}
/// Whether `pattern` matches the start of `path` (`*` any run, a final `$` the end). Linear in
/// the lengths' product at worst, both capped.
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let p = pattern.as_bytes();
    let s = path.as_bytes();
    // Positions in `s` the pattern so far can end at; a set of booleans.
    let mut at = vec![false; s.len() + 1];
    let mut next = vec![false; s.len() + 1];
    at[0] = true;
    for &c in p {
        if c == b'*' {
            let mut reach = false;
            for slot in at.iter_mut() {
                reach |= *slot;
                *slot = reach;
            }
        } else {
            next.fill(false);
            for i in 0..s.len() {
                if at[i] && s[i] == c {
                    next[i + 1] = true;
                }
            }
            std::mem::swap(&mut at, &mut next);
        }
        if !at.iter().any(|b| *b) {
            return false;
        }
    }
    if anchored {
        at[s.len()]
    } else {
        at.iter().any(|b| *b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn rules_and_patterns_are_capped() {
        let mut text = String::from("User-agent: *\n");
        for n in 0..RULES_CAP + 10 {
            text.push_str(&format!("Disallow: /p{n}$\n"));
        }
        // Past the cap the rules are not the file's: nothing is allowed.
        let robots = Robots::parse(text.as_bytes(), AGENT);
        assert_eq!(robots, Robots::disallow_all());
        assert!(!robots.allows(&format!("/p{}", RULES_CAP + 5)));
        assert!(!robots.allows("/"));
        let long = format!("User-agent: *\nDisallow: /{}\n", "a".repeat(PATTERN_CAP));
        assert_eq!(
            Robots::parse(long.as_bytes(), AGENT),
            Robots::disallow_all()
        );
        // Within the caps the rules stand.
        let mut within = String::from("User-agent: *\n");
        for n in 0..RULES_CAP {
            within.push_str(&format!("Disallow: /p{n}$\n"));
        }
        let robots = Robots::parse(within.as_bytes(), AGENT);
        assert_eq!(robots.rules.len(), RULES_CAP);
        assert!(!robots.allows("/p0") && robots.allows("/"));
        // Another group's capped rules do not touch ours.
        let theirs = format!(
            "User-agent: other\nDisallow: /{}\nUser-agent: *\nDisallow: /x\n",
            "a".repeat(PATTERN_CAP)
        );
        assert!(Robots::parse(theirs.as_bytes(), AGENT).allows("/"));
    }

    #[test]
    fn rules_for_our_agent_or_everyone() {
        let text = b"User-agent: *\nDisallow: /private\n\nUser-agent: Pinakes/1.0\nUser-agent: other\nDisallow: /\nAllow: /$\n";
        let ours = Robots::parse(text, AGENT);
        assert!(ours.allows("/"));
        assert!(!ours.allows("/page"));
        let theirs = Robots::parse(text, "nobody");
        assert!(theirs.allows("/") && !theirs.allows("/private/x"));
        // A site that shuts everyone out.
        assert!(!Robots::parse(b"User-agent: *\nDisallow: /", AGENT).allows("/"));
        // An empty disallow allows; comments and junk lines are passed over.
        assert!(Robots::parse(b"User-agent: * # all\nDisallow:\nnonsense\n", AGENT).allows("/"));
        // Longest match wins; allow wins a tie.
        let r = Robots::parse(
            b"User-agent: *\nDisallow: /a\nAllow: /a/b\nDisallow: /x\nAllow: /x\n",
            AGENT,
        );
        assert!(!r.allows("/a/c") && r.allows("/a/b/c") && r.allows("/x"));
        // Wildcards and the end anchor.
        let r = Robots::parse(b"User-agent: *\nDisallow: /*.pdf$\nDisallow: /*?\n", AGENT);
        assert!(
            !r.allows("/a/b.pdf") && r.allows("/a/b.pdf/x") && !r.allows("/?q=1") && r.allows("/")
        );
        assert!(!Robots::disallow_all().allows("/") && Robots::allow_all().allows("/"));
        // A group's agents end at its first rule.
        let r = Robots::parse(
            b"User-agent: pinakes\nAllow: /\nUser-agent: *\nDisallow: /\n",
            AGENT,
        );
        assert!(r.allows("/"));
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]
        /// Fuzz: any text parses, and any path is decided, without a panic.
        #[test]
        fn any_text_and_path(text in "(?s)[ -~\n\r\t*$#:]{0,400}", path in "/[ -~*$]{0,60}") {
            let r = Robots::parse(text.as_bytes(), AGENT);
            let _ = r.allows(&path);
        }
        /// Property: a pattern without wildcards matches exactly the paths it starts.
        #[test]
        fn a_plain_pattern_is_a_prefix(pattern in "/[a-z/]{0,10}", path in "/[a-z/]{0,12}") {
            prop_assert_eq!(matches(&pattern, &path), path.starts_with(&pattern));
        }
    }
    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]
        /// Property: a long file is read within its cap and quickly.
        #[test]
        fn long_files_are_capped(lines in 1usize..20_000) {
            let text = "User-agent: *\nDisallow: /*a*b*c*d*e\n".repeat(lines);
            let r = Robots::parse(text.as_bytes(), AGENT);
            prop_assert!(r.rules.len() <= CAP / 20);
            let _ = r.allows(&format!("/{}", "a".repeat(200)));
        }
    }
}
