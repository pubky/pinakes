#[derive(serde::Serialize, serde::Deserialize)]
pub struct OccurrenceRef {
    pub uid: String,
    pub recurrence_id: Option<String>,
}
