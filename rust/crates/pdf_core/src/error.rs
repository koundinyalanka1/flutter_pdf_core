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
}
