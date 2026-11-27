# Software catalogue fixtures

Real records from each registry's own catalogue file, read on 2026-11-26.

| File | Source | What was kept |
| --- | --- | --- |
| `categories.csv`, `crate_downloads.csv`, `crates.csv`, `keywords.csv`, `crates_categories.csv`, `crates_keywords.csv` | crates.io database dump `https://static.crates.io/db-dump.tar.gz` (dump of 2026-11-26 02:00 UTC), files under `data/` | The rows of six crates (`anyhow`, `rand`, `ripgrep`, `serde`, `tokio`, `url`) and the categories and keywords they name; each README cut to 300 characters |
| `fdroid-entry.json` | `https://f-droid.org/repo/entry.json` (2026-11-23) | Whole file |
| `fdroid-index-v2.json` | `https://f-droid.org/repo/index-v2.json` (2026-11-23) | Three packages (`org.fdroid.fdroid`, `de.danoeh.antennapod`, `org.videolan.vlc`): metadata without images and descriptions, names and summaries in three languages, one version's number |
| `flathub-appstream.xml` | `https://dl.flathub.org/repo/appstream/x86_64/appstream.xml.gz` (2026-11-25) | Three applications, one runtime and one add-on, without descriptions, releases, screenshots and other languages than German |

Licences: the crates.io dump's data is CC0 (its README in the dump says so); F-Droid's index
and Flathub's AppStream data are published for clients to read and redistribute (app metadata
under each app's own metadata licence, mostly CC0 or CC-BY-SA for F-Droid, CC0 metadata for
Flathub per the AppStream specification's `metadata_license`).
