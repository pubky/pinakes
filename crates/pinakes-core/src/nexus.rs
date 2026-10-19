use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Put,
    Del,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Tag,
    Reply,
    Embed,
    Collection,
    Event,
}
impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Reply => "reply",
            Self::Embed => "embed",
            Self::Collection => "collection",
            Self::Event => "event",
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
/// One line of the Nexus reference feed. Nexus also sends its own `resource_id`; Pinakes keys on
/// `uri` and computes its own ID (`identity`), so that field is not read.
pub struct Change {
    pub op: Op,
    pub uri: String,
    pub kind: Kind,
    pub author: String,
    pub object_path: String,
    pub label: Option<String>,
    pub at: i64,
    pub recurrence_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Changes {
    pub lines: Vec<Change>,
    pub next_cursor: i64,
}
#[derive(Clone, Debug)]
pub struct FeedPage {
    pub page: Changes,
    pub epoch: Option<String>,
    pub length: i64,
}
#[derive(Clone)]
pub struct Nexus {
    base: String,
    client: reqwest::Client,
}
impl Nexus {
    pub fn new(base: &str) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }
    pub async fn changes(&self, cursor: i64) -> Result<FeedPage> {
        let response = self
            .client
            .get(format!("{}/v0/resources/changes", self.base))
            .query(&[("cursor", cursor), ("limit", 1000)])
            .send()
            .await?
            .error_for_status()?;
        let epoch = response
            .headers()
            .get("x-events-epoch")
            .map(|v| v.to_str().map(str::to_owned))
            .transpose()?;
        let length = response
            .headers()
            .get("x-events-length")
            .context("Nexus omitted feed length")?
            .to_str()?
            .parse::<i64>()?;
        if length < 0 {
            bail!("negative Nexus feed length");
        }
        Ok(FeedPage {
            page: bounded_json(response).await?,
            epoch,
            length,
        })
    }
}
pub fn valid_author(id: &str) -> bool {
    id.len() == 52
        && id
            .bytes()
            .all(|c| b"ybndrfg8ejkmcpqxot1uwisza345h769".contains(&c))
}
async fn bounded_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    let mut bytes = vec![];
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            bail!("Nexus response exceeded 8 MB");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
