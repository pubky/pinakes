pub(crate) fn header_alternate(header: Option<&str>, base: &url::Url) -> Option<(String, String)> {
    header?.split(',').find_map(|part| {
        if !part.contains("rel=\"alternate\"") && !part.contains("rel=alternate") {
            return None;
        }
        let mime = if part.contains("application/activity+json") {
            "application/activity+json"
        } else if part.contains("application/ld+json") {
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        } else {
            return None;
        };
        Some((
            base.join(part.split('<').nth(1)?.split('>').next()?)
                .ok()?
                .to_string(),
            mime.into(),
        ))
    })
}
