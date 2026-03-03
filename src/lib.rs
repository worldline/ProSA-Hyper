#![doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/COPYRIGHT"))]
//!
//! [![github]](https://github.com/worldline/ProSA-Hyper)&ensp;[![crates-io]](https://crates.io/crates/prosa-hyper)&ensp;[![docs-rs]](crate)
//!
//! [github]: https://img.shields.io/badge/github-46beaa?style=for-the-badge&labelColor=555555&logo=github
//! [crates-io]: https://img.shields.io/badge/crates.io-ffeb78?style=for-the-badge&labelColor=555555&logo=rust
//! [docs-rs]: https://img.shields.io/badge/docs.rs-41b4d2?style=for-the-badge&labelColor=555555&logo=docs.rs
//!
//! ProSA Hyper processor to handle HTTP client and server

#![warn(missing_docs)]

use std::{convert::Infallible, io};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::{Response, Version};
use prosa::core::{
    error::{BusError, ProcError},
    msg::InternalMsg,
};
use thiserror::Error;

const H2: &[u8] = b"h2";

/// Global Hyper processor error
#[derive(Debug, Error)]
pub enum HyperProcError {
    /// IO Error
    #[error("Hyper IO error: {0}")]
    Io(#[from] io::Error),
    /// Hyper Error
    #[error("Hyper error[{1}]: `{0}`")]
    Hyper(hyper::Error, String),
    /// Internal bus error
    #[error("Internal bus error: {0}")]
    InternalBus(#[from] BusError),
    /// Other Error
    #[error("Other error: {0}")]
    Other(String),
}

impl ProcError for HyperProcError {
    fn recoverable(&self) -> bool {
        match self {
            HyperProcError::Io(error) => error.recoverable(),
            HyperProcError::Hyper(..) => true,
            HyperProcError::InternalBus(b) => b.recoverable(),
            HyperProcError::Other(_) => true,
        }
    }
}

impl<M> From<tokio::sync::mpsc::error::SendError<InternalMsg<M>>> for HyperProcError
where
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    fn from(err: tokio::sync::mpsc::error::SendError<InternalMsg<M>>) -> Self {
        HyperProcError::InternalBus(BusError::InternalQueue(format!(
            "Failed to send message: {}",
            err
        )))
    }
}

/// Method to get a string version of the Hyper Version object
fn hyper_version_str(version: Version) -> &'static str {
    match version {
        hyper::Version::HTTP_11 => "HTTP/1.1",
        hyper::Version::HTTP_2 => "HTTP/2",
        hyper::Version::HTTP_3 => "HTTP/3",
        _ => "Unknown",
    }
}

/// Enum to define all type of response to request it can be made
#[derive(Debug)]
pub enum HyperResp<M> {
    /// Make an internal service request to have a response
    SrvReq(String, M),
    /// Make a direct HTTP response
    HttpResp(Response<BoxBody<Bytes, Infallible>>),
    /// Response with an HTTP error
    HttpErr(hyper::Error),
}

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

#[cfg(any(feature = "server", feature = "client"))]
#[cfg(test)]
pub mod tests;
