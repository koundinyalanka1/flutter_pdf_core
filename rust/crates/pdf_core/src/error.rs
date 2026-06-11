use thiserror::Error;

#[derive(Debug, Error)]
pub enum PdfError {
    #[error("input is not a PDF file")]
    MissingHeader,
    #[error("unsupported PDF feature: {0}")]
    Unsupported(&'static str),
    #[error("parse error at byte {offset}: {message}")]
    Parse { offset: usize, message: String },
    #[error("xref error at byte {offset}: {message}")]
    Xref { offset: usize, message: String },
    #[error("write error: {0}")]
    Write(String),
    #[error("filter error: {0}")]
    Filter(String),
    #[error("unsupported stream filter: {0}")]
    UnsupportedFilter(String),
    #[error("document is encrypted; a password is required")]
    Encrypted,
    #[error("incorrect password")]
    WrongPassword,
    #[error("encryption error: {0}")]
    Crypt(String),
    #[error("invalid document structure: {0}")]
    Structure(String),
    #[error("page index {0} is out of bounds")]
    PageIndex(usize),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, PdfError>;

impl PdfError {
    pub(crate) fn parse(offset: usize, message: impl Into<String>) -> Self {
        Self::Parse {
            offset,
            message: message.into(),
        }
    }

    pub(crate) fn xref(offset: usize, message: impl Into<String>) -> Self {
        Self::Xref {
            offset,
            message: message.into(),
        }
    }

    pub(crate) fn write(message: impl Into<String>) -> Self {
        Self::Write(message.into())
    }

    pub(crate) fn structure(message: impl Into<String>) -> Self {
        Self::Structure(message.into())
    }

    pub(crate) fn crypt(message: impl Into<String>) -> Self {
        Self::Crypt(message.into())
    }
}
