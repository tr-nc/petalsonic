//! Error types for PetalSonic

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PetalSonicError {
    #[error("Audio device error: {0}")]
    AudioDevice(String),

    #[error("Audio format error: {0}")]
    AudioFormat(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Audio loading error: {0}")]
    AudioLoading(String),

    #[error("Ring buffer error: {0}")]
    RingBuffer(String),

    #[error("Engine error: {0}")]
    Engine(String),

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("Spatialization error: {0}")]
    Spatialization(String),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, PetalSonicError>;
