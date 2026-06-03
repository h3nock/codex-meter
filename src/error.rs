use std::{fmt, io};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    Argument(String),
    Alias(String),
    CodexHomeMissing(std::path::PathBuf),
    Io {
        context: String,
        source: io::Error,
    },
    Json {
        context: String,
        source: serde_json::Error,
    },
    Terminal(String),
}

impl AppError {
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub fn json(context: impl Into<String>, source: serde_json::Error) -> Self {
        Self::Json {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument(message) => write!(formatter, "{message}"),
            Self::Alias(message) => write!(formatter, "{message}"),
            Self::CodexHomeMissing(path) => {
                write!(formatter, "Codex home does not exist: {}", path.display())
            }
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::Json { context, source } => write!(formatter, "{context}: {source}"),
            Self::Terminal(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}
