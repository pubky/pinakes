//! Recorded fixtures hold no key a page's publisher sends every visitor
//! (`scripts/scrub-publisher-keys`).
use std::{fs, io::Write, path::Path, process::Command};

fn script() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/scrub-publisher-keys")
}

fn run(args: &[&str], input: Option<&[u8]>) -> std::process::Output {
    let mut child = Command::new("python3")
        .arg("-I")
        .arg(script())
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.unwrap_or_default())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn recorded_fixtures_hold_no_publisher_keys() {
    let out = run(&["--check"], None);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every shape is found in plain and gzip files without its value being printed, `--write`
/// leaves the text around it, `--stdin` scrubs the same way, and an adapter body is renamed to
/// its new SHA-256 with its `responses.json` row.
#[test]
fn every_publisher_key_shape_is_found_and_replaced() {
    use sha2::Digest;
    // Built at run time, so no source file holds a key-shaped value.
    let google = format!("AIza{}", "Q".repeat(35));
    let aws = format!("AKIA{}", "Q7".repeat(8));
    let session = format!("ASIA{}", "7Q".repeat(8));
    let eppo = format!("3mvQ{}", "z9.".repeat(10));
    let fatsoma = format!("fk_ui_cust_{}", "ab12-".repeat(4));
    let values = [
        google.clone(),
        aws.clone(),
        session.clone(),
        "SIGNATUREVALUE1".into(),
        "SESSIONTOKEN2".into(),
        "QUERYKEY3".into(),
        "PAIRID4".into(),
        "CFSIGNATURE5".into(),
        "IGSECRET6".into(),
        "IGENCRYPTED7".into(),
        eppo.clone(),
        fatsoma.clone(),
    ];
    let page = format!(
        concat!(
            "<img src=\"https://maps.googleapis.com/maps/api/staticmap?center=1,2&amp;key={}\">\n",
            "{{\"INNERTUBE_API_KEY\":\"{}\"}}\n",
            "https://b.s3.amazonaws.com/f?X-Amz-Credential={}%2F20261016%2Fus-east-1&X-Amz-Signature={}\n",
            "https://b.s3.amazonaws.com/g?X-Amz-Credential={}%2F20261016&amp;X-Amz-Security-Token={}&amp;x=1\n",
            "https://b.s3.amazonaws.com/h?AWSAccessKeyId={}&Expires=1\n",
            "https://d.cloudfront.net/v.json?Expires=1&amp;Key-Pair-Id={}&amp;Signature={}\n",
            "\"ajaxResponseToken\":{{\"secret\":\"{}\",\"encrypted\":\"{}\"}}\n",
            "<link href=\"https://fscdn.eppo.cloud/api/flag-config/v1/config?sdkVersion&#x3D;3.1.2&amp;apiKey&#x3D;{}&amp;x=1\">\n",
            "%2C%22fatsoma%22%3A%7B%22apiKey%22%3A%22{}%22\n"
        ),
        google,
        google,
        aws,
        values[3],
        session,
        values[4],
        values[5],
        values[6],
        values[7],
        values[8],
        values[9],
        eppo,
        fatsoma
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = root.join(format!(".runtime/publisher-keys-{}", std::process::id()));
    let adapters = temp.join("adapters");
    fs::create_dir_all(&adapters).unwrap();
    fs::write(temp.join("page.html"), &page).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(page.as_bytes()).unwrap();
    let packed = gz.finish().unwrap();
    let digest = format!("{:x}", sha2::Sha256::digest(page.as_bytes()));
    fs::write(adapters.join(format!("{digest}.gz")), &packed).unwrap();
    let row = serde_json::json!([{"uri": "https://example.org/", "path": format!("{digest}.gz"),
        "sha256": digest, "blake3": "0", "bytes": page.len()}]);
    fs::write(adapters.join("responses.json"), row.to_string()).unwrap();

    let named = format!("{:x}", sha2::Sha256::digest(page.as_bytes()));
    fs::write(
        temp.join("README.md"),
        format!("page.html: {} bytes, SHA-256 {named}.\n", page.len()),
    )
    .unwrap();
    let check = run(&["--check", temp.to_str().unwrap()], None);
    assert!(!check.status.success());
    let report = String::from_utf8_lossy(&check.stdout).into_owned()
        + &String::from_utf8_lossy(&check.stderr);
    assert!(report.contains("page.html") && report.contains(&format!("{digest}.gz")));
    for name in [
        "google_api_key",
        "aws_key_id",
        "aws_presigned",
        "aws_query_key",
        "cloudfront_key_pair",
        "cloudfront_signature",
        "instagram_ajax_token",
        "client_api_key_param",
        "fatsoma_key",
    ] {
        assert!(report.contains(name), "{name}: {report}");
    }
    for value in &values {
        assert!(!report.contains(value.as_str()), "{value} printed");
    }

    assert!(run(&["--write", temp.to_str().unwrap()], None)
        .status
        .success());
    assert!(run(&["--check", temp.to_str().unwrap()], None)
        .status
        .success());
    let scrubbed = fs::read_to_string(temp.join("page.html")).unwrap();
    for value in &values {
        assert!(!scrubbed.contains(value.as_str()), "{value} kept");
    }
    for kept in [
        "staticmap?center=1,2&amp;key=REDACTED\"",
        "X-Amz-Credential=REDACTED%2F20261016%2Fus-east-1&X-Amz-Signature=REDACTED\n",
        "X-Amz-Security-Token=REDACTED&amp;x=1",
        "Key-Pair-Id=REDACTED&amp;Signature=REDACTED\n",
        "{\"secret\":\"REDACTED\",\"encrypted\":\"REDACTED\"}",
        "&amp;apiKey&#x3D;REDACTED&amp;x=1",
        "%22apiKey%22%3A%22REDACTED%22",
    ] {
        assert!(scrubbed.contains(kept), "{kept}: {scrubbed}");
    }
    // The README names the scrubbed body's size and digest.
    assert_eq!(
        fs::read_to_string(temp.join("README.md")).unwrap(),
        format!(
            "page.html: {} bytes, SHA-256 {:x}.\n",
            scrubbed.len(),
            sha2::Sha256::digest(scrubbed.as_bytes())
        )
    );
    // A README digest that names no body below it is reported.
    fs::create_dir_all(temp.join("stale")).unwrap();
    fs::write(temp.join("stale/README.md"), format!("SHA-256 {named}\n")).unwrap();
    let stale = run(&["--check", temp.to_str().unwrap()], None);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stdout).contains("stale/README.md: stale_digestx1"));
    fs::remove_dir_all(temp.join("stale")).unwrap();
    let piped = run(&["--stdin"], Some(page.as_bytes()));
    assert!(piped.status.success());
    assert_eq!(String::from_utf8(piped.stdout).unwrap(), scrubbed);

    let new = format!("{:x}", sha2::Sha256::digest(scrubbed.as_bytes()));
    assert!(!adapters.join(format!("{digest}.gz")).exists());
    let mut body = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(
            fs::File::open(adapters.join(format!("{new}.gz"))).unwrap(),
        ),
        &mut body,
    )
    .unwrap();
    assert_eq!(body, scrubbed);
    let rows: serde_json::Value =
        serde_json::from_slice(&fs::read(adapters.join("responses.json")).unwrap()).unwrap();
    assert_eq!(rows[0]["path"], format!("{new}.gz"));
    assert_eq!(rows[0]["sha256"], new);
    assert_eq!(rows[0]["bytes"], scrubbed.len());
    assert!(rows[0].get("blake3").is_none());
    fs::remove_dir_all(temp).unwrap();
}

/// The one recorder in the repository, `adapter_probe`, sends every live result through
/// `record_request` (body, redirect and URLs scrubbed before anything is saved or returned) and
/// finds captures, live and replayed, by the scrubbed key (its own unit test checks both).
#[test]
fn the_adapter_probe_scrubs_every_capture_before_saving_it() {
    let probe =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/adapter_probe.rs"))
            .unwrap();
    let live = probe
        .find("let mut result = PinnedTransport.get_with(url, addresses, options).await;")
        .unwrap();
    assert!(probe[live..]
        .trim_start_matches(|c| c != '\n')
        .trim_start()
        .starts_with("let record = record_request(url, options, &mut result, &self.root)?;"));
    assert!(probe.contains("let key = capture_key(url.as_str(), options.json_body.as_ref())?;"));
    assert!(probe.contains("let key = capture_key(uri, v[\"json_body\"]"));
    assert_eq!(probe.matches("std::fs::write(&file, &f.body)").count(), 1);
    assert!(probe.contains(".arg(\"--stdin\")"));
}
