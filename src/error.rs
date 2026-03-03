use thiserror::Error;

#[derive(Debug, Error)]
pub enum DgxTopError {
    #[error("GPU error: {0}")]
    Gpu(String),

    #[error("NVML error: {0}")]
    Nvml(#[from] nvml_wrapper::error::NvmlError),

    #[error("System collector error: {0}")]
    Collector(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {field} — {message}")]
    Parse { field: String, message: String },

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Terminal error: {0}")]
    Terminal(String),

    #[error("Process error: {0}")]
    Process(String),

    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, DgxTopError>;
