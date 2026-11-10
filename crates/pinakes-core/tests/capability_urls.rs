use std::{fs, path::Path, process::Command};

#[test]
fn committed_data_has_no_capability_urls() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new("python3")
        .arg(root.join("scripts/verify-no-capability-urls"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn capability_guard_rejects_every_provider_without_printing_tokens() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = root.join(format!(".runtime/capability-guard-{}", std::process::id()));
    fs::create_dir_all(temp.join("data")).unwrap();
    for value in [
        "https://calendar.proton.me/share/SECRET",
        "https://outlook.office365.com/owa/calendar/SECRET/calendar.ics",
        "https://outlook.live.com/owa/calendar/SECRET/calendar.ics",
        "https://p29-caldav.icloud.com/published/SECRET",
        "https://calendar.google.com/calendar/ical/test/private-SECRET/basic.ics",
        "https://calendar.example.org/remote.php/dav/public-calendars/SECRET/",
        "https://proton.me/api/calendar/v1/url/SECRET",
    ] {
        fs::write(temp.join("source.rs"), value).unwrap();
        let output = Command::new("python3")
            .arg(root.join("scripts/verify-no-capability-urls"))
            .arg("--root")
            .arg(&temp)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET"));
    }
    fs::write(temp.join("source.rs"), "https://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics").unwrap();
    assert!(Command::new("python3")
        .arg(root.join("scripts/verify-no-capability-urls"))
        .arg("--root")
        .arg(&temp)
        .status()
        .unwrap()
        .success());
    fs::remove_dir_all(temp).unwrap();
}

/// The runtime classifier and the guard agree on every probe of the shared fixture: a provider
/// probe is a capability to both, a query probe to the classifier alone (the guard keeps calendar
/// links out of the repository), a public link to neither. Neither prints a probe's secret.
#[test]
fn the_classifier_and_the_guard_agree_on_every_probe() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let probes: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("crates/pinakes-core/tests/fixtures/capability_urls.json"))
            .unwrap(),
    )
    .unwrap();
    let temp = root.join(format!(".runtime/capability-probes-{}", std::process::id()));
    fs::create_dir_all(&temp).unwrap();
    for (list, classifier, guard) in [
        ("providers", true, true),
        ("query", true, false),
        ("public", false, false),
    ] {
        for probe in probes[list].as_array().unwrap() {
            let probe = probe.as_str().unwrap();
            let shown = probe.replace("SECRET", "…");
            assert_eq!(
                pinakes_core::capability::is_capability(probe),
                classifier,
                "{list}: {shown}"
            );
            fs::write(temp.join("source.rs"), probe).unwrap();
            let output = Command::new("python3")
                .arg(root.join("scripts/verify-no-capability-urls"))
                .arg("--root")
                .arg(&temp)
                .output()
                .unwrap();
            assert_eq!(!output.status.success(), guard, "{list}: {shown}");
            assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET"));
        }
    }
    fs::remove_dir_all(temp).unwrap();
}
