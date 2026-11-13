//! Links between biodiversity records that several sources cross-link: a GBIF occurrence and
//! the iNaturalist observation it publishes name each other, whichever of the two sources this
//! build has.
use super::kit::{host_and_segments, number};

/// The sites of the iNaturalist Network (`/v1/sites`), without `www.`, plus naturalista.mx,
/// which redirects to the Mexican site.
pub const INATURALIST_SITES: [&str; 23] = [
    "inaturalist.org",
    "mexico.inaturalist.org",
    "naturalista.mx",
    "inaturalist.nz",
    "inaturalist.ca",
    "colombia.inaturalist.org",
    "biodiversity4all.org",
    "inaturalist.ala.org.au",
    "panama.inaturalist.org",
    "ecuador.inaturalist.org",
    "israel.inaturalist.org",
    "argentinat.org",
    "costarica.inaturalist.org",
    "inaturalist.mma.gob.cl",
    "inaturalist.laji.fi",
    "inaturalist.se",
    "spain.inaturalist.org",
    "greece.inaturalist.org",
    "guatemala.inaturalist.org",
    "uk.inaturalist.org",
    "inaturalist.lu",
    "taiwan.inaturalist.org",
    "naturalista.uy",
];

/// A positive decimal id of at most `max` digits, without leading zeros.
fn id(value: &str, max: usize) -> Option<u64> {
    number(value, max)
}

/// The id of an iNaturalist observation link (any site of the network, or the API).
pub fn inaturalist_observation(uri: &str) -> Option<u64> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (host.as_str(), s.as_slice()) {
        ("api.inaturalist.org", ["v1", "observations", n]) => id(n, 12),
        (h, ["observations", n]) if INATURALIST_SITES.contains(&h) => id(n, 12),
        _ => None,
    }
}
/// An iNaturalist observation's page.
pub fn inaturalist_observation_page(id: u64) -> String {
    format!("https://www.inaturalist.org/observations/{id}")
}

/// A language prefix of gbif.org, e.g. `es`, `fr` or `zh-tw`.
pub fn gbif_language(segment: &str) -> bool {
    let (lang, region) = segment.split_once('-').unwrap_or((segment, "ab"));
    [lang, region]
        .iter()
        .all(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_lowercase()))
}
/// The key of a GBIF occurrence link (gbif.org, any language, its `verbatim` tab; or the API).
pub fn gbif_occurrence(uri: &str) -> Option<u64> {
    let (host, segments) = host_and_segments(uri)?;
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    let s = match (host.as_str(), s.as_slice()) {
        ("gbif.org", [lang, rest @ ..]) if gbif_language(lang) => rest,
        ("gbif.org", s) => s,
        ("api.gbif.org", ["v1", "occurrence", n]) => return id(n, 19),
        _ => return None,
    };
    match s {
        ["occurrence", n] | ["occurrence", n, "verbatim"] => id(n, 19),
        _ => None,
    }
}
/// A GBIF occurrence's page.
pub fn gbif_occurrence_page(key: u64) -> String {
    format!("https://www.gbif.org/occurrence/{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_links() {
        assert_eq!(
            inaturalist_observation("https://www.inaturalist.org/observations/333069440"),
            Some(333069440)
        );
        assert_eq!(
            inaturalist_observation("https://inaturalist.nz/observations/5"),
            Some(5)
        );
        assert_eq!(
            inaturalist_observation("https://www.inaturalist.org/taxa/47219"),
            None
        );
        assert_eq!(
            gbif_occurrence("https://www.gbif.org/fr/occurrence/4011234567/verbatim"),
            Some(4011234567)
        );
        assert_eq!(
            gbif_occurrence("https://api.gbif.org/v1/occurrence/7"),
            Some(7)
        );
        assert_eq!(gbif_occurrence("https://www.gbif.org/species/1"), None);
        assert_eq!(gbif_occurrence("https://www.gbif.org/occurrence/0"), None);
    }
}
