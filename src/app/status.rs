//! Status-bar message with severity. The UI renders `Error` variants in
//! red; everything else is rendered as plain info text.

pub enum Status {
    Info(String),
    Error(String),
}

impl Status {
    pub fn info(s: impl Into<String>) -> Self {
        Status::Info(s.into())
    }
    pub fn error(s: impl Into<String>) -> Self {
        Status::Error(s.into())
    }
    pub fn text(&self) -> &str {
        match self {
            Status::Info(s) | Status::Error(s) => s,
        }
    }
    pub fn is_error(&self) -> bool {
        matches!(self, Status::Error(_))
    }
}
