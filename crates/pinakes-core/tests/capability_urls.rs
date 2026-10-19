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
