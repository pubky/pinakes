//! GovInfo, the US Government Publishing Office's repository: public and private laws, bills,
//! US Code sections and the Statutes at Large, read from their MODS metadata records. Public
//! domain; no key. GovInfo's Federal Register and CFR copies are other editions of documents the
//! `federal_register` and `ecfr` sources read, and are not read here.
use super::{web_url, Context, Example, Source};
use crate::{model::ResourceMeta, resolver::egress::FetchOptions};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

pub struct Govinfo;

const MODS: &str = "http://www.loc.gov/mods/v3";
const BILL_TYPES: &[&str] = &[
    "hconres", "hjres", "hres", "hr", "sconres", "sjres", "sres", "s",
];

/// What a GovInfo link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A public (or private) law, e.g. `PLAW-117publ58`.
    Law { package: String },
    /// One version of a bill, e.g. `BILLS-117hr3684enr`.
    Bill { package: String },
    /// A section of the US Code: `26 U.S.C. 501`, in whatever edition is current, or one
    /// edition's copy of it (its package and granule).
    Code {
        title: String,
        section: String,
        edition: Option<(String, String)>,
    },
    /// A law as printed in the Statutes at Large: volume and first page.
    Statute { volume: u32, page: u32 },
}

fn digits(value: &str, max: usize) -> Option<u32> {
    super::kit::number(value, max)
}
/// Split `117hr3684enr` into its runs of digits and letters.
fn runs(value: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    let bytes = value.as_bytes();
    for i in 1..=bytes.len() {
        if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[i - 1].is_ascii_digit() {
            out.push(&value[start..i]);
            start = i;
        }
    }
    out
}
fn law(package: &str) -> Option<Target> {
    let rest = package.strip_prefix("PLAW-")?;
    let [congress, kind, number] = runs(rest)[..] else {
        return None;
    };
    (digits(congress, 3).is_some()
        && matches!(kind, "publ" | "pvtl")
        && digits(number, 5).is_some())
    .then(|| Target::Law {
        package: package.to_owned(),
    })
}
fn bill(package: &str) -> Option<Target> {
    let rest = package.strip_prefix("BILLS-")?;
    let [congress, kind, number, version] = runs(rest)[..] else {
        return None;
    };
    (digits(congress, 3).is_some()
        && BILL_TYPES.contains(&kind)
        && digits(number, 5).is_some()
        && (1..=8).contains(&version.len())
        && version.bytes().all(|b| b.is_ascii_lowercase()))
    .then(|| Target::Bill {
        package: package.to_owned(),
    })
}
fn code_section(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}
/// A package and (when it has one) a granule as a target.
fn of_ids(package: &str, granule: Option<&str>) -> Option<Target> {
    if package.starts_with("PLAW-") {
        return law(package);
    }
    if package.starts_with("BILLS-") {
        return bill(package);
    }
    let granule = granule?;
    if let Some(volume) = package.strip_prefix("STATUTE-") {
        let page = granule.strip_prefix(&format!("{package}-Pg"))?;
        return Some(Target::Statute {
            volume: digits(volume, 4)?,
            page: digits(page, 6)?,
        });
    }
    let rest = package.strip_prefix("USCODE-")?;
    let (year, title) = rest.split_once("-title")?;
    digits(year, 4)?;
    let title = title.to_ascii_lowercase();
    let ok_title =
        !title.is_empty() && title.len() <= 4 && title.bytes().all(|b| b.is_ascii_alphanumeric());
    let section = granule.strip_prefix(package)?.rsplit_once("-sec")?.1;
    (ok_title && code_section(section)).then(|| Target::Code {
        title,
        section: section.to_owned(),
        edition: Some((package.to_owned(), granule.to_owned())),
    })
}

impl Target {
    pub fn of_uri(uri: &str) -> Option<Self> {
        let url = web_url(uri)?;
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let query = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        match url.host_str()? {
            "www.govinfo.gov" | "govinfo.gov" => match segments.as_slice() {
                ["app", "details", package] => of_ids(package, None),
                ["app", "details", package, granule] => of_ids(package, Some(granule)),
                ["content", "pkg", package, _, file] => {
                    let stem = file.rsplit_once('.')?.0;
                    of_ids(package, (stem != *package).then_some(stem))
                }
                ["metadata", "pkg", package, "mods.xml"] => of_ids(package, None),
                ["metadata", "granule", package, granule, "mods.xml"] => {
                    of_ids(package, Some(granule))
                }
                ["link", "plaw", congress, kind, number] => {
                    let kind = match *kind {
                        "public" => "publ",
                        "private" => "pvtl",
                        _ => return None,
                    };
                    law(&format!(
                        "PLAW-{}{kind}{}",
                        digits(congress, 3)?,
                        digits(number, 5)?
                    ))
                }
                ["link", "bills", congress, kind, number] => bill(&format!(
                    "BILLS-{}{kind}{}{}",
                    digits(congress, 3)?,
                    digits(number, 5)?,
                    query("billversion")?
                )),
                // An edition named by `year` is not current, and its granule is not known here.
                ["link", "uscode", title, section] if query("year").is_none() => {
                    let title = title.to_ascii_lowercase();
                    (title.len() <= 4
                        && title.bytes().all(|b| b.is_ascii_alphanumeric())
                        && code_section(section))
                    .then(|| Self::Code {
                        title,
                        section: (*section).to_owned(),
                        edition: None,
                    })
                }
                ["link", "statute", volume, page] => Some(Self::Statute {
                    volume: digits(volume, 4)?,
                    page: digits(page, 6)?,
                }),
                _ => None,
            },
            "www.gpo.gov" | "gpo.gov" => match segments.as_slice() {
                ["fdsys", "pkg", package, ..] => of_ids(package, None),
                ["fdsys", "granule", package, granule, ..] => of_ids(package, Some(granule)),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn root(&self) -> String {
        match self {
            Self::Law { package } | Self::Bill { package } => {
                format!("https://www.govinfo.gov/app/details/{package}")
            }
            Self::Code {
                edition: Some((package, granule)),
                ..
            } => format!("https://www.govinfo.gov/app/details/{package}/{granule}"),
            Self::Code { title, section, .. } => {
                format!("https://www.govinfo.gov/link/uscode/{title}/{section}")
            }
            Self::Statute { volume, page } => format!(
                "https://www.govinfo.gov/app/details/STATUTE-{volume}/STATUTE-{volume}-Pg{page}"
            ),
        }
    }
    /// The MODS record: the package's, or the granule's through the link service for a US Code
    /// section (whose package record is tens of megabytes).
    pub fn mods_url(&self) -> String {
        match self {
            Self::Law { package } | Self::Bill { package } => {
                format!("https://www.govinfo.gov/metadata/pkg/{package}/mods.xml")
            }
            Self::Code {
                edition: Some((package, granule)),
                ..
            } => format!("https://www.govinfo.gov/metadata/granule/{package}/{granule}/mods.xml"),
            Self::Code { title, section, .. } => format!(
                "https://www.govinfo.gov/link/uscode/{title}/{section}?link-type=mods"
            ),
            Self::Statute { volume, page } => format!(
                "https://www.govinfo.gov/metadata/granule/STATUTE-{volume}/STATUTE-{volume}-Pg{page}/mods.xml"
            ),
        }
    }
}

/// A bill type's citation label: `hr` → `H.R.`.
fn bill_label(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "hr" => "H.R.",
        "s" => "S.",
        "hjres" => "H.J.Res.",
        "sjres" => "S.J.Res.",
        "hconres" => "H.Con.Res.",
        "sconres" => "S.Con.Res.",
        "hres" => "H.Res.",
        "sres" => "S.Res.",
        _ => return None,
    })
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name((MODS, name)))
}
fn text_of(node: Option<roxmltree::Node<'_, '_>>) -> Option<String> {
    node.and_then(|n| n.text())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty())
}
/// The first child element `name` of `node` whose attributes include all of `with`.
fn find<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    name: &str,
    with: &[(&str, &str)],
) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|n| {
        n.has_tag_name((MODS, name)) && with.iter().all(|(k, v)| n.attribute(*k) == Some(v))
    })
}

/// The card of a target from its MODS record, or `None` when GovInfo has no such record.
pub fn card(
    mut doc: ResourceMeta,
    target: &Target,
    status: u16,
    final_path: &str,
    body: &[u8],
) -> Result<Option<ResourceMeta>> {
    match status {
        200 if final_path == "/error" => return Ok(None),
        200 => {}
        400 | 404 => return Ok(None),
        s => anyhow::bail!("GovInfo answered {s}"),
    }
    let xml = roxmltree::Document::parse(std::str::from_utf8(body)?)?;
    let mods = xml.root_element();
    anyhow::ensure!(mods.has_tag_name((MODS, "mods")), "not a MODS record");
    // A record may hold several extension blocks: look in each, in order.
    let extensions: Vec<_> = mods
        .children()
        .filter(|n| n.has_tag_name((MODS, "extension")))
        .collect();
    anyhow::ensure!(!extensions.is_empty(), "no extension");
    let in_extensions =
        |name: &str, with: &[(&str, &str)]| extensions.iter().find_map(|e| find(*e, name, with));
    let access = text_of(in_extensions("accessId", &[])).unwrap_or_default();
    let title = text_of(child(mods, "titleInfo").and_then(|t| child(t, "title")));
    let citation = text_of(find(mods, "identifier", &[("type", "preferred citation")]));
    let host = find(mods, "relatedItem", &[("type", "host")]);
    let ext = |name: &str| text_of(in_extensions(name, &[]));
    doc.set_type("Legislation");
    let mut facts: Vec<(&str, Option<String>)> = vec![];
    match target {
        Target::Law { package } | Target::Bill { package } => {
            anyhow::ensure!(access == *package, "GovInfo answered for another package");
            let short = match target {
                Target::Bill { .. } => text_of(in_extensions("shortTitle", &[("type", "measure")])),
                _ => ext("shortTitle"),
            };
            doc.name = short.clone().or_else(|| title.clone());
            doc.description = short.and(title);
            let date = text_of(child(mods, "originInfo").and_then(|o| child(o, "dateIssued")));
            if let Target::Law { .. } = target {
                let bill = in_extensions("bill", &[("priority", "primary")]).and_then(|b| {
                    let kind = b.attribute("type")?.to_ascii_lowercase();
                    Some(format!("{} {}", bill_label(&kind)?, b.attribute("number")?))
                });
                facts.extend([
                    ("public_law", citation),
                    ("congress", ext("congress")),
                    ("enacted", date),
                    ("bill", bill),
                ]);
            } else {
                facts.extend([
                    ("bill", citation),
                    ("congress", ext("congress")),
                    ("bill_version", ext("billVersion")),
                    ("date_issued", date),
                    ("origin_chamber", ext("originChamber")),
                    ("current_chamber", ext("currentChamber")),
                ]);
            }
        }
        Target::Code {
            title: t,
            section,
            edition,
        } => {
            anyhow::ensure!(
                access.starts_with("USCODE-")
                    && access.ends_with(&format!("-sec{section}"))
                    && edition
                        .as_ref()
                        .is_none_or(|(_, granule)| access == *granule),
                "GovInfo answered for another section"
            );
            doc.name = title;
            let code =
                host.and_then(|h| text_of(child(h, "titleInfo").and_then(|t| child(t, "title"))));
            let cite = citation.unwrap_or_else(|| format!("{t} U.S.C. {section}"));
            doc.description = Some(match &code {
                Some(code) => format!("{cite} · {code}"),
                None => cite.clone(),
            });
            let year = access
                .strip_prefix("USCODE-")
                .and_then(|r| r.split('-').next())
                .map(str::to_owned);
            facts.extend([
                ("citation", Some(cite)),
                ("code_title", code),
                ("edition", year.filter(|_| edition.is_some())),
                (
                    "date_issued",
                    host.and_then(|h| {
                        text_of(child(h, "originInfo").and_then(|o| child(o, "dateIssued")))
                    }),
                ),
            ]);
        }
        Target::Statute { volume, page } => {
            anyhow::ensure!(
                access == format!("STATUTE-{volume}-Pg{page}"),
                "GovInfo answered for another page"
            );
            let short = text_of(in_extensions("shortTitle", &[("context", "CONTENT")]));
            doc.name = short.clone().or_else(|| title.clone());
            doc.description = short.and(title);
            let law = ext("congress")
                .zip(ext("number"))
                .map(|(c, n)| format!("Public Law {c}-{n}"));
            facts.extend([
                ("citation", citation),
                ("public_law", law),
                ("enacted", ext("granuleDate")),
            ]);
        }
    }
    anyhow::ensure!(doc.name.is_some(), "no title");
    facts.push((
        "page_url",
        Some(match target {
            Target::Code { edition: None, .. } => format!("{}?link-type=details", target.root()),
            _ => target.root(),
        }),
    ));
    for (key, value) in facts {
        if let Some(value) = value {
            doc.attributes.insert(key.into(), value);
        }
    }
    Ok(Some(doc))
}

/// The card a link names by itself, for a record too large to read: its citation as the name.
pub fn minimal_card(mut doc: ResourceMeta, target: &Target) -> ResourceMeta {
    let id = |package: &str, prefix: &str| -> Vec<String> {
        package
            .strip_prefix(prefix)
            .map(|rest| runs(rest).into_iter().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let citation = match target {
        Target::Law { package } => match id(package, "PLAW-").as_slice() {
            [c, kind, n] if kind == "pvtl" => format!("Private Law {c}-{n}"),
            [c, _, n] => format!("Public Law {c}-{n}"),
            _ => package.clone(),
        },
        Target::Bill { package } => match id(package, "BILLS-").as_slice() {
            [c, kind, n, version] => format!(
                "{} {n} ({version}), {c}th Congress",
                bill_label(kind).unwrap_or(kind)
            ),
            _ => package.clone(),
        },
        Target::Code { title, section, .. } => format!("{title} U.S.C. {section}"),
        Target::Statute { volume, page } => format!("{volume} Stat. {page}"),
    };
    doc.set_type("Legislation");
    doc.name = Some(citation.clone());
    doc.attributes.insert("citation".into(), citation);
    doc.attributes.insert("page_url".into(), target.root());
    doc
}

#[async_trait]
impl Source for Govinfo {
    fn name(&self) -> &'static str {
        "govinfo"
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "govinfo",
                uri: "https://www.govinfo.gov/app/details/PLAW-117publ58",
                card_type: "Legislation",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "www.govinfo.gov",
                path: "/metadata/pkg/PLAW-117publ58/mods.xml",
                query: &[],
                header: None,
                content_type: "application/xml",
                file: "mods_pkg_PLAW-117publ58.xml.gz",
            }],
        })
    }
    fn reads(&self, uri: &str) -> bool {
        Target::of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let root = Target::of_uri(uri)?.root();
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &[
            "public_law",
            "congress",
            "enacted",
            "bill",
            "bill_version",
            "date_issued",
            "origin_chamber",
            "current_chamber",
            "citation",
            "code_title",
            "edition",
            "page_url",
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.govinfo.gov/app/details/PLAW-117publ58",
                card_type: "Legislation",
            },
            Example {
                uri: "https://www.govinfo.gov/link/uscode/26/501",
                card_type: "Legislation",
            },
        ]
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let target = Target::of_uri(uri).context("not a GovInfo link")?;
        let fetched = cx
            .fetch(
                &target.mods_url(),
                &FetchOptions {
                    accept: Some("application/xml".into()),
                    extra_types: &["application/xml", "text/xml"],
                    // A cap of its own: a larger record is refused (the minimal card below),
                    // where an XML body without one is cut and no longer parses.
                    body_cap: Some(crate::resolver::egress::BODY_CAP),
                    ..Default::default()
                },
            )
            .await;
        let f = match fetched {
            // A record over the 1 MB cap: the card the link itself names, never a retry loop.
            Err(e)
                if e.chain()
                    .any(|c| c.is::<crate::resolver::egress::BodyTooLarge>()) =>
            {
                return Ok(minimal_card(cx.card(uri, self), &target));
            }
            other => other?,
        };
        Ok(
            card(cx.card(uri, self), &target, f.status, f.url.path(), &f.body)?
                .unwrap_or_else(|| cx.missing(uri, self)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("govinfo", file)
    }
    fn base(target: &Target) -> ResourceMeta {
        ResourceMeta::new(&target.root(), "govinfo", 3600)
    }
    fn law() -> Target {
        Target::Law {
            package: "PLAW-117publ58".into(),
        }
    }

    /// A record over the page cap gives the card its link names: the read sets a cap of its
    /// own, so the body is refused rather than cut into XML that no longer parses.
    #[tokio::test(start_paused = true)]
    async fn a_record_over_the_cap_gets_the_minimal_card() {
        let mut cut = read("mods_pkg_PLAW-117publ58.xml.gz");
        cut.truncate(cut.len() / 2);
        let (egress, _) = crate::sources::fixture::egress(vec![(
            "https://www.govinfo.gov/metadata/pkg/PLAW-117publ58/mods.xml",
            200,
            "!large-xml",
            cut,
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = Govinfo
            .resolve(&cx, "https://www.govinfo.gov/app/details/PLAW-117publ58")
            .await
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Public Law 117-58"));
        assert_eq!(doc.type_name, "Legislation");
    }

    #[test]
    fn addresses() {
        let root = "https://www.govinfo.gov/app/details/PLAW-117publ58";
        for uri in [
            "https://www.govinfo.gov/content/pkg/PLAW-117publ58/pdf/PLAW-117publ58.pdf",
            "https://www.govinfo.gov/content/pkg/PLAW-117publ58/html/PLAW-117publ58.htm",
            "https://www.govinfo.gov/metadata/pkg/PLAW-117publ58/mods.xml",
            "https://www.govinfo.gov/link/plaw/117/public/58?link-type=pdf",
            "https://www.gpo.gov/fdsys/pkg/PLAW-117publ58/pdf/PLAW-117publ58.pdf",
        ] {
            assert_eq!(Govinfo.alias_root(uri).as_deref(), Some(root), "{uri}");
        }
        assert_eq!(Govinfo.alias_root(root), None);
        let code = "https://www.govinfo.gov/link/uscode/26/501";
        assert_eq!(
            Govinfo
                .alias_root("https://www.govinfo.gov/link/uscode/26/501?link-type=details")
                .as_deref(),
            Some(code)
        );
        // Each annual edition's copy of a section is its own resource.
        let edition = "https://www.govinfo.gov/app/details/USCODE-2022-title26/USCODE-2022-title26-subtitleA-chap1-subchapF-partI-sec501";
        for uri in [
            edition,
            "https://www.govinfo.gov/content/pkg/USCODE-2022-title26/html/USCODE-2022-title26-subtitleA-chap1-subchapF-partI-sec501.htm",
            "https://www.govinfo.gov/metadata/granule/USCODE-2022-title26/USCODE-2022-title26-subtitleA-chap1-subchapF-partI-sec501/mods.xml",
        ] {
            assert_eq!(Target::of_uri(uri).unwrap().root(), edition, "{uri}");
        }
        assert!(!Govinfo.reads("https://www.govinfo.gov/link/uscode/26/501?year=2022"));
        assert_eq!(
            Target::of_uri("https://www.govinfo.gov/link/bills/117/hr/3684?billversion=enr"),
            Some(Target::Bill {
                package: "BILLS-117hr3684enr".into()
            })
        );
        assert_eq!(
            Target::of_uri("https://www.govinfo.gov/link/statute/135/429"),
            Some(Target::Statute {
                volume: 135,
                page: 429
            })
        );
        for uri in [
            "https://www.govinfo.gov/link/bills/117/hr/3684",
            "https://www.govinfo.gov/app/details/USCODE-2024-title26",
            "https://www.govinfo.gov/app/details/FR-2026-09-09/2026-18291",
            "https://www.govinfo.gov/app/details/CFR-2025-title14-vol1/CFR-2025-title14-vol1-part39",
            "https://www.govinfo.gov/app/details/PLAW-117xyz58",
            "https://www.govinfo.gov/app/collection/plaw",
        ] {
            assert!(!Govinfo.reads(uri), "{uri}");
        }
    }

    #[test]
    fn law_bill_code_and_statute_cards() {
        let doc = card(
            base(&law()),
            &law(),
            200,
            "/metadata/pkg/PLAW-117publ58/mods.xml",
            &read("mods_pkg_PLAW-117publ58.xml.gz"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "Legislation");
        assert_eq!(
            doc.name.as_deref(),
            Some("Infrastructure Investment and Jobs Act")
        );
        assert!(doc
            .description
            .as_deref()
            .unwrap()
            .starts_with("An act to authorize funds"));
        let a = &doc.attributes;
        assert_eq!(a["public_law"], "Public Law 117-58");
        assert_eq!(a["enacted"], "2021-11-15");
        assert_eq!(a["bill"], "H.R. 3684");

        let bill = Target::Bill {
            package: "BILLS-117hr3684enr".into(),
        };
        let doc = card(
            base(&bill),
            &bill,
            200,
            "/x",
            &read("mods_pkg_BILLS-117hr3684enr.xml.gz"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            doc.name.as_deref(),
            Some("Infrastructure Investment and Jobs Act")
        );
        assert_eq!(doc.attributes["bill"], "H.R. 3684");
        assert_eq!(doc.attributes["bill_version"], "enr");
        assert_eq!(doc.attributes["current_chamber"], "JOINT");

        let code = Target::Code {
            title: "26".into(),
            section: "501".into(),
            edition: None,
        };
        let doc = card(
            base(&code),
            &code,
            200,
            "/x",
            &read("mods_granule_USCODE-2024-title26-sec501.xml.gz"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            doc.name.as_deref(),
            Some("Exemption from tax on corporations, certain trusts, etc.")
        );
        assert_eq!(
            doc.description.as_deref(),
            Some("26 U.S.C. 501 · INTERNAL REVENUE CODE")
        );
        assert_eq!(doc.attributes["date_issued"], "2024-12-31");
        assert!(!doc.attributes.contains_key("edition"));
        let granule = "USCODE-2024-title26-subtitleA-chap1-subchapF-partI-sec501";
        let copy = Target::of_uri(&format!(
            "https://www.govinfo.gov/app/details/USCODE-2024-title26/{granule}"
        ))
        .unwrap();
        let doc = card(
            base(&copy),
            &copy,
            200,
            "/x",
            &read("mods_granule_USCODE-2024-title26-sec501.xml.gz"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.attributes["edition"], "2024");
        let other = Target::of_uri(&format!(
            "https://www.govinfo.gov/app/details/USCODE-2022-title26/{}",
            granule.replace("2024", "2022")
        ))
        .unwrap();
        let wrong = card(
            base(&other),
            &other,
            200,
            "/x",
            &read("mods_granule_USCODE-2024-title26-sec501.xml.gz"),
        );
        assert!(wrong.is_err(), "another edition's record");

        // A record over the size cap: the card the link names.
        let big = minimal_card(base(&law()), &law());
        assert_eq!(big.name.as_deref(), Some("Public Law 117-58"));
        let bill = Target::Bill {
            package: "BILLS-117hr3684enr".into(),
        };
        assert_eq!(
            minimal_card(base(&bill), &bill).name.as_deref(),
            Some("H.R. 3684 (enr), 117th Congress")
        );

        let statute = Target::Statute {
            volume: 135,
            page: 429,
        };
        let doc = card(
            base(&statute),
            &statute,
            200,
            "/x",
            &read("mods_granule_STATUTE-135-Pg429.xml.gz"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(doc.attributes["citation"], "135 Stat. 429");
        assert_eq!(doc.attributes["public_law"], "Public Law 117-58");

        // The record must be the one asked for; a missing package lands on /error.
        let other = Target::Law {
            package: "PLAW-117publ59".into(),
        };
        assert!(card(
            base(&other),
            &other,
            200,
            "/x",
            &read("mods_pkg_PLAW-117publ58.xml.gz")
        )
        .is_err());
        assert!(card(base(&law()), &law(), 200, "/error", b"<html/>")
            .unwrap()
            .is_none());
        assert!(
            card(base(&law()), &law(), 400, "/link/plaw/117/public/9999", b"")
                .unwrap()
                .is_none()
        );
        assert!(card(base(&law()), &law(), 503, "/x", b"").is_err());
    }
}
