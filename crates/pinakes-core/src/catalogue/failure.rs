//! What a failed catalogue run says about itself, in the log and in `catalogue_state.last_error`.
//!
//! An importer reads bulk files from fixed hosts, and some read pages (Tranco's homepages): the
//! text of an error it meets may hold what an upstream answered. So a failure is shown by its
//! class alone, never by its text, with two exceptions the runner itself vouches for:
//!
//! - a [`Failure`] an importer or the runner raises: a fixed class (`download`, `parse`, `disk`,
//!   `panic`, `lock_lost`, …) and, optionally, where (`at`): the host of a fixed bulk file or a
//!   local path, never anything read from a page;
//! - an error of this process's own database (a `sqlx::Error`), shown by its own text through
//!   `resolver::log_error`, as the daemon's other database errors are.
//!
//! Any other error is shown by `resolver::error_class` (`timeout`, `connect`, `http_5xx`, …), or
//! `unclassified`.

/// A classified failure of a catalogue run. Importers return their failures as this (with
/// `anyhow::Error::new` or `.context(Failure::new(…))`), so the log and the state row say what
/// failed without repeating any text an upstream sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// A fixed class: lower case letters and underscores.
    pub class: &'static str,
    /// Where: the host of a fixed bulk file, or a local path. Never a page's text.
    pub at: Option<String>,
}
impl Failure {
    pub fn new(class: &'static str) -> Self {
        Self { class, at: None }
    }
    pub fn at(mut self, at: impl Into<String>) -> Self {
        self.at = Some(at.into());
        self
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.at {
            Some(at) => write!(f, "{} ({at})", self.class),
            None => f.write_str(self.class),
        }
    }
}
impl std::error::Error for Failure {}

/// How a failed run is shown in the log and stored as `last_error`: a [`Failure`] as it says
/// itself, a database error of this process by its own text, anything else by its class.
pub fn shown(error: &anyhow::Error) -> String {
    // `anyhow::Error::downcast_ref` sees through context layers; `chain` through sources.
    let failure = error
        .downcast_ref::<Failure>()
        .or_else(|| error.chain().find_map(|c| c.downcast_ref::<Failure>()));
    if let Some(failure) = failure {
        return failure.to_string();
    }
    // The database error alone, never the contexts an importer may have put around it.
    let database = error
        .downcast_ref::<sqlx::Error>()
        .or_else(|| error.chain().find_map(|c| c.downcast_ref::<sqlx::Error>()));
    if let Some(database) = database {
        return format!("database: {}", crate::resolver::log_error(database));
    }
    match crate::resolver::error_class(error) {
        // `error_class`'s answer for an error it knows nothing of.
        "parse" => "unclassified".into(),
        class => class.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// An importer's error is shown by its class: an upstream's text never reaches the log or
    /// the state row. A `Failure` says its class and where; a database error its own text.
    #[tokio::test]
    async fn a_failure_is_shown_by_its_class_never_by_an_upstream_s_text() {
        let page = anyhow::anyhow!("<html>secret page text https://x.example/?token=CANARY</html>");
        assert_eq!(shown(&page), "unclassified");
        let wrapped = page.context("reading https://x.example/list?token=CANARY");
        assert!(!shown(&wrapped).contains("CANARY"));
        let classified = anyhow::Error::new(Failure::new("download").at("dumps.wikimedia.org"))
            .context("while reading CANARY");
        assert_eq!(shown(&classified), "download (dumps.wikimedia.org)");
        let around = anyhow::anyhow!("CANARY text").context(Failure::new("parse"));
        assert_eq!(shown(&around), "parse");
        let elapsed = tokio::time::timeout(std::time::Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        let timeout = anyhow::Error::new(elapsed);
        assert_eq!(shown(&timeout), "timeout");
        let database = anyhow::Error::new(sqlx::Error::PoolTimedOut).context("CANARY context");
        let text = shown(&database);
        assert!(
            text.starts_with("database: ") && !text.contains("CANARY"),
            "{text}"
        );
    }
}
