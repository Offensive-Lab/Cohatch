//! Reusable download engine. Network operations are asynchronous; offline disk
//! operations are synchronous and can be run on a caller's blocking pool.
pub mod benchmark;
pub mod combine;
pub mod download;
pub mod error;
pub mod http;
pub mod manifest;
pub mod storage;

pub use error::{Error, Result};
