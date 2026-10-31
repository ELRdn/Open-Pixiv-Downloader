use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    UserCancel,
    RequestBudget,
    HttpStatus { status: u16 },
    UnexpectedContent,
    WorkerError,
    DiskError,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Request budget reached; active transfers remain saved")]
    Budget,
    #[error("{0}")]
    Retryable(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Stop(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("HTTP {status}; resource unavailable")]
    Missing { status: u16 },
    #[error("Filesystem operation failed ({0})")]
    Io(#[from] std::io::Error),
    #[error("Archive database operation failed ({0})")]
    Sql(#[from] rusqlite::Error),
    #[error("Invalid JSON data ({0})")]
    Json(#[from] serde_json::Error),
    #[error("Invalid ZIP data ({0})")]
    Zip(#[from] zip::result::ZipError),
}
pub type Result<T> = std::result::Result<T, Error>;
pub fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
