//! UK legislation from legislation.gov.uk: Acts, statutory instruments, devolved legislation and
//! retained EU law, read from each item's `introduction/data.xml` (a provision's from its own
//! `data.xml`). Open Government Licence v3.0; no key; 1,500 requests per 5 minutes.
use super::{web_url, Budget, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct LegislationGovUk;

pub const ATTRIBUTION: &str =
    "Contains public sector information licensed under the Open Government Licence v3.0.";
/// The added credit for retained EU law.
pub const EU_ATTRIBUTION: &str = "Material derived from the European Institutions © European \
Union, 1998-2019 and re-used under the terms of the Commission Decision 2011/833/EU.";

const LEG: &str = "http://www.legislation.gov.uk/namespaces/legislation";
const UKM: &str = "http://www.legislation.gov.uk/namespaces/metadata";
const DC: &str = "http://purl.org/dc/elements/1.1/";

/// An item of legislation (its type, year and number: `ukpga`, `uksi`, `eur`, …), one of its
/// provisions, or a version of either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub kind: String,
    pub year: u16,
    pub number: u32,
    /// A provision, e.g. `section/1` or `schedule/2/paragraph/3`.
    pub provision: Option<String>,
    /// A version: `enacted`, `made`, … or a point in time (`YYYY-MM-DD`).
    pub version: Option<String>,
}

/// The kinds of provision an address can name, first and nested.
const PROVISIONS: &[&str] = &[
    "section",
    "article",
    "regulation",
    "rule",
    "schedule",
    "part",
    "chapter",
];
const NESTED: &[&str] = &[
    "paragraph",
    "section",
    "article",
    "regulation",
    "rule",
    "part",
    "chapter",
];
const VERSIONS: &[&str] = &["enacted", "made", "adopted", "created", "prospective"];

fn provision_number(value: &str) -> bool {
    !value.is_empty() && value.len() <= 12 && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl Item {
    /// `https://www.legislation.gov.uk[/id]/{type}/{year}/{number}[/{provision}][/{version}]`, with
    /// `/contents`, `/introduction`, a language (`/welsh`) or a data format (`/data.xml`) naming
    /// the same text. Notes, memoranda, impact assessments and other parts are not read, nor
    /// items numbered by regnal year.
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        if !matches!(
            url.host_str()?,
            "www.legislation.gov.uk" | "legislation.gov.uk"
        ) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let rest = match segments.as_slice() {
            ["id", rest @ ..] => rest,
            rest => rest,
        };
        let [kind, year, number, tail @ ..] = rest else {
            return None;
        };
        let kind = kind.to_ascii_lowercase();
        let ok_kind = (2..=8).contains(&kind.len()) && kind.bytes().all(|b| b.is_ascii_lowercase());
        let year: u16 = (year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit()))
            .then(|| year.parse().ok())
            .flatten()?;
        let number: u32 =
            (!number.is_empty() && number.len() <= 6 && number.bytes().all(|b| b.is_ascii_digit()))
                .then(|| number.parse().ok())
                .flatten()
                .filter(|n| *n > 0)?;
        if !ok_kind {
            return None;
        }
        // From the end: a data format, a language, a version; the rest names the part.
        let mut tail: Vec<&str> = tail.to_vec();
        if tail.last().is_some_and(|s| s.starts_with("data.")) {
            tail.pop();
        }
        if tail
            .last()
            .is_some_and(|s| matches!(*s, "welsh" | "english"))
        {
            tail.pop();
        }
        let version = match tail.last() {
            Some(v)
                if VERSIONS.contains(v)
                    || chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok() =>
            {
                let v = (*v).to_owned();
                tail.pop();
                Some(v)
            }
            _ => None,
        };
        let provision = match tail.as_slice() {
            [] | ["contents"] | ["introduction"] => None,
            [first, ..]
                if PROVISIONS.contains(first)
                    && tail.len().is_multiple_of(2)
                    && tail.len() <= 8 =>
            {
                let ok = tail.chunks(2).enumerate().all(|(i, pair)| {
                    (i == 0 || NESTED.contains(&pair[0])) && provision_number(pair[1])
                });
                ok.then(|| tail.join("/"))?.into()
            }
            _ => return None,
        };
        Some(Self {
            kind,
            year,
            number,
            provision,
            version,
        })
    }
    /// The item's own address, without the provision and version.
    pub fn act(&self) -> String {
        format!(
            "https://www.legislation.gov.uk/{}/{}/{}",
            self.kind, self.year, self.number
        )
    }
    pub fn root(&self) -> String {
        let mut root = self.act();
        for part in [&self.provision, &self.version].into_iter().flatten() {
            root.push('/');
            root.push_str(part);
        }
        root
    }
    /// The data to read, in the version asked for: a provision's own (which also tells whether
    /// the act has it, and carries the act's metadata), else the item's introduction.
    pub fn data_url(&self) -> String {
        match &self.provision {
            Some(p) => self.versioned(&format!("{}/{p}", self.act())),
            None => self.introduction_url(),
        }
    }
    /// The item's introduction data, in the version asked for.
    pub fn introduction_url(&self) -> String {
        self.versioned(&format!("{}/introduction", self.act()))
    }
    fn versioned(&self, base: &str) -> String {
        match &self.version {
            Some(v) => format!("{base}/{v}/data.xml"),
            None => format!("{base}/data.xml"),
        }
    }
    /// `section 1`, `schedule 2 paragraph 3`.
    pub fn provision_label(&self) -> Option<String> {
        self.provision.as_ref().map(|p| p.replace('/', " "))
    }
}

fn child<'a, 'i>(
    parent: roxmltree::Node<'a, 'i>,
    ns: &str,
    name: &str,
) -> Option<roxmltree::Node<'a, 'i>> {
    parent.children().find(|n| n.has_tag_name((ns, name)))
}
fn value(node: Option<roxmltree::Node<'_, '_>>, attribute: &str) -> Option<String> {
    node.and_then(|n| n.attribute(attribute))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The card of an item from its data (a provision's or the introduction's; both carry the
/// item's metadata), or `None` when the site has no such item or provision (400 or 404).
pub fn card(
    mut doc: ResourceMeta,
    item: &Item,
    status: u16,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    match status {
        200 => {}
        400 | 404 => return Ok(None),
        s => anyhow::bail!("legislation.gov.uk answered {s}"),
    }
    let text = std::str::from_utf8(body)?;
    let xml = roxmltree::Document::parse(text)?;
    let root = xml.root_element();
    anyhow::ensure!(
        root.has_tag_name((LEG, "Legislation")),
        "not legislation data"
    );
    let id = root.attribute("IdURI").unwrap_or_default();
    let own = format!("/{}/{}/{}", item.kind, item.year, item.number);
    anyhow::ensure!(id.ends_with(&own), "the site answered for another item");
    let metadata = root
        .children()
        .find(|n| n.has_tag_name((UKM, "Metadata")))
        .context("no metadata")?;
    let dc = |name: &str| {
        child(metadata, DC, name)
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let specific = ["PrimaryMetadata", "SecondaryMetadata", "EUMetadata"]
        .iter()
        .find_map(|name| child(metadata, UKM, name))
        .context("no item metadata")?;
    let classification = child(specific, UKM, "DocumentClassification");
    let class = |name: &str| value(classification.and_then(|c| child(c, UKM, name)), "Value");
    let category = class("DocumentCategory");

    doc.set_type("Legislation");
    let title = dc("title").context("no title")?;
    doc.name = Some(match item.provision_label() {
        Some(label) => format!("{title}, {label}"),
        None => title,
    });
    doc.description = dc("description");
    doc.attribution = Some(if category.as_deref() == Some("euretained") {
        format!("{ATTRIBUTION} {EU_ATTRIBUTION}")
    } else {
        ATTRIBUTION.to_owned()
    });
    for (key, value) in [
        ("legislation_type", class("DocumentMainType")),
        ("category", category),
        ("year", Some(item.year.to_string())),
        ("number", Some(item.number.to_string())),
        (
            "enacted",
            value(child(specific, UKM, "EnactmentDate"), "Date"),
        ),
        ("made", value(child(specific, UKM, "Made"), "Date")),
        ("provision", item.provision_label()),
        ("version", item.version.clone()),
        ("page_url", Some(item.root())),
    ] {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

#[async_trait]
impl Source for LegislationGovUk {
    fn name(&self) -> &'static str {
        "legislation_gov_uk"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "legislation_gov_uk",
                uri: "https://www.legislation.gov.uk/ukpga/2018/12/contents",
                card_type: "Legislation",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "www.legislation.gov.uk",
                path: "/ukpga/2018/12/introduction/data.xml",
                query: &[],
                header: None,
                content_type: "application/xml",
                file: "ukpga_2018_12_introduction.xml",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Item::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = Item::of_uri(uri)?.root();
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "legislation_type",
            "category",
            "year",
            "number",
            "enacted",
            "made",
            "provision",
            "version",
            "page_url",
        ]
    }
    fn budgets(&self) -> &'static [Budget] {
        // 1,500 requests per 5 minutes; the site recommends a gentler pace.
        &[("www.legislation.gov.uk", 1.0)]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.legislation.gov.uk/ukpga/2018/12/contents",
                card_type: "Legislation",
            },
            Example {
                uri: "https://www.legislation.gov.uk/uksi/2019/419/made",
                card_type: "Legislation",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let item = Item::of_uri(uri).context("not legislation")?;
        let xml = FetchOptions {
            accept: Some("application/xml".into()),
            extra_types: &["application/xml"],
            ..Default::default()
        };
        let f = match cx.fetch(&item.data_url(), &xml).await {
            // A provision too large to read exists; its card comes from the introduction.
            Err(e)
                if item.provision.is_some()
                    && e.chain()
                        .any(|c| c.is::<crate::resolver::egress::BodyTooLarge>()) =>
            {
                cx.fetch(&item.introduction_url(), &xml).await?
            }
            f => f?,
        };
        Ok(card(cx.card(uri, self), &item, f.status, &f.body)?
            .unwrap_or_else(|| cx.missing(uri, self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("legislation_gov_uk", file)
    }
    fn item(kind: &str, year: u16, number: u32) -> Item {
        Item {
            kind: kind.into(),
            year,
            number,
            provision: None,
            version: None,
        }
    }
    fn base(item: &Item) -> ResourceMeta {
        ResourceMeta::new(&item.root(), "legislation_gov_uk", 3600)
    }

    #[test]
    fn addresses() {
        let root = "https://www.legislation.gov.uk/ukpga/2018/12";
        for uri in [
            "https://www.legislation.gov.uk/ukpga/2018/12/contents",
            "https://www.legislation.gov.uk/ukpga/2018/12/introduction",
            "https://www.legislation.gov.uk/ukpga/2018/12/data.xml",
            "https://www.legislation.gov.uk/id/ukpga/2018/12",
            "https://www.legislation.gov.uk/ukpga/2018/12/contents/welsh",
            "http://legislation.gov.uk/ukpga/2018/12",
        ] {
            assert_eq!(
                LegislationGovUk.alias_root(uri).as_deref(),
                Some(root),
                "{uri}"
            );
        }
        assert_eq!(LegislationGovUk.alias_root(root), None);
        // Provisions and versions are their own resources; their other spellings alias them.
        for (uri, own) in [
            (
                "https://www.legislation.gov.uk/ukpga/2018/12/section/1",
                "https://www.legislation.gov.uk/ukpga/2018/12/section/1",
            ),
            (
                "https://www.legislation.gov.uk/ukpga/2018/12/section/1/data.xml",
                "https://www.legislation.gov.uk/ukpga/2018/12/section/1",
            ),
            (
                "https://www.legislation.gov.uk/ukpga/2018/12/schedule/2/paragraph/3",
                "https://www.legislation.gov.uk/ukpga/2018/12/schedule/2/paragraph/3",
            ),
            (
                "https://www.legislation.gov.uk/ukpga/2018/12/2020-01-31",
                "https://www.legislation.gov.uk/ukpga/2018/12/2020-01-31",
            ),
            (
                "https://www.legislation.gov.uk/ukpga/2018/12/contents/2020-01-31",
                "https://www.legislation.gov.uk/ukpga/2018/12/2020-01-31",
            ),
            (
                "https://www.legislation.gov.uk/ukpga/2018/12/enacted",
                "https://www.legislation.gov.uk/ukpga/2018/12/enacted",
            ),
            (
                "https://www.legislation.gov.uk/ukpga/2018/12/section/1/2020-01-31",
                "https://www.legislation.gov.uk/ukpga/2018/12/section/1/2020-01-31",
            ),
        ] {
            assert_eq!(Item::of_uri(uri).unwrap().root(), own, "{uri}");
        }
        assert_eq!(
            Item::of_uri("https://www.legislation.gov.uk/anaw/2016/1/contents/welsh"),
            Some(item("anaw", 2016, 1))
        );
        for uri in [
            "https://www.legislation.gov.uk/ukpga/Vict/63-64/51",
            "https://www.legislation.gov.uk/ukpga/2018",
            "https://www.legislation.gov.uk/search?title=data",
            "https://www.legislation.gov.uk/ukpga/2018/0",
            "https://www.legislation.gov.uk/ukpga/2018/12/notes",
            "https://www.legislation.gov.uk/ukpga/2018/12/notes/contents",
            "https://www.legislation.gov.uk/uksi/2019/419/memorandum/contents",
            "https://www.legislation.gov.uk/ukpga/2018/12/impacts",
            "https://www.legislation.gov.uk/ukpga/2018/12/body",
            "https://www.legislation.gov.uk/ukpga/2018/12/section",
        ] {
            assert!(!LegislationGovUk.reads(uri), "{uri}");
        }
    }

    #[test]
    fn act_si_and_retained_eu_cards() {
        let act = item("ukpga", 2018, 12);
        let doc = card(
            base(&act),
            &act,
            200,
            &read("ukpga_2018_12_introduction.xml"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "Legislation");
        assert_eq!(doc.name.as_deref(), Some("Data Protection Act 2018"));
        assert!(doc
            .description
            .unwrap()
            .starts_with("An Act to make provision"));
        let a = &doc.attributes;
        assert_eq!(a["legislation_type"], "UnitedKingdomPublicGeneralAct");
        assert_eq!(a["category"], "primary");
        assert_eq!(a["enacted"], "2018-05-23");
        assert!(!a.contains_key("provision") && !a.contains_key("version"));
        assert_eq!(doc.attribution.as_deref(), Some(ATTRIBUTION));

        let si = Item::of_uri("https://www.legislation.gov.uk/uksi/2019/419/made").unwrap();
        assert_eq!(
            si.data_url(),
            "https://www.legislation.gov.uk/uksi/2019/419/introduction/made/data.xml"
        );
        let doc = card(
            base(&si),
            &si,
            200,
            &read("uksi_2019_419_introduction_made.xml"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            doc.attributes["legislation_type"],
            "UnitedKingdomStatutoryInstrument"
        );
        assert_eq!(doc.attributes["made"], "2019-02-28");
        assert_eq!(doc.attributes["version"], "made");

        let eu = item("eur", 2016, 679);
        let doc = card(
            base(&eu),
            &eu,
            200,
            &read("eur_2016_679_introduction.xml.gz"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.attributes["category"], "euretained");
        assert!(doc
            .attribution
            .unwrap()
            .contains("Commission Decision 2011/833/EU"));

        // The act in a version.
        let dated =
            Item::of_uri("https://www.legislation.gov.uk/ukpga/2018/12/2020-01-31").unwrap();
        assert_eq!(
            dated.data_url(),
            "https://www.legislation.gov.uk/ukpga/2018/12/introduction/2020-01-31/data.xml"
        );
        let doc = card(
            base(&dated),
            &dated,
            200,
            &read("ukpga_2018_12_introduction_2020-01-31.xml"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Data Protection Act 2018"));
        assert_eq!(doc.attributes["version"], "2020-01-31");

        // A provision in a version, from its own data: the act's card plus the provision.
        let section =
            Item::of_uri("https://www.legislation.gov.uk/ukpga/2018/12/section/1/2020-01-31")
                .unwrap();
        assert_eq!(
            section.data_url(),
            "https://www.legislation.gov.uk/ukpga/2018/12/section/1/2020-01-31/data.xml"
        );
        assert_eq!(
            section.introduction_url(),
            "https://www.legislation.gov.uk/ukpga/2018/12/introduction/2020-01-31/data.xml"
        );
        let doc = card(
            base(&section),
            &section,
            200,
            &read("ukpga_2018_12_section_1_2020-01-31.xml"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            doc.name.as_deref(),
            Some("Data Protection Act 2018, section 1")
        );
        assert_eq!(doc.attributes["provision"], "section 1");
        assert_eq!(doc.attributes["version"], "2020-01-31");

        // The data must be the item asked for.
        let other = item("ukpga", 2018, 13);
        assert!(card(
            base(&act),
            &other,
            200,
            &read("ukpga_2018_12_introduction.xml")
        )
        .is_err());
        let missing = read("missing_ukpga_2018_9999_introduction.html");
        assert!(card(base(&act), &item("ukpga", 2018, 9999), 400, &missing)
            .unwrap()
            .is_none());
        // A provision the act does not have is missing.
        let absent =
            Item::of_uri("https://www.legislation.gov.uk/ukpga/2018/12/section/999").unwrap();
        let page = read("missing_ukpga_2018_12_section_999.html");
        assert!(card(base(&absent), &absent, 404, &page).unwrap().is_none());
    }
}
