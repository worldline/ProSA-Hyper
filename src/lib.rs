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
    msg::{ErrorMsg, InternalMsg},
};
use thiserror::Error;

const H2: &[u8] = b"h2";

/// Product version header value used for `Server` or `User-Agent` header in HTTP requests and responses
#[cfg(target_family = "unix")]
pub const PRODUCT_VERSION_HEADER: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (Unix)"
);
#[cfg(target_family = "windows")]
pub const PRODUCT_VERSION_HEADER: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (Windows)"
);
#[cfg(all(not(target_family = "unix"), not(target_family = "windows")))]
pub const PRODUCT_VERSION_HEADER: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

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

/// Error related to an HTTP message or Hyper error
#[derive(Debug, Error)]
pub enum HttpError {
    /// Hyper Error
    #[error("Hyper error: `{0}`")]
    Hyper(#[from] hyper::Error),
    /// HTTP Error
    #[error("HTTP error: `{0}`")]
    Http(#[from] http::Error),
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

/// Type alias for the closure that processes an internal service response or error.
///
/// Receives `Ok(M)` on success or `Err(ErrorMsg<M>)` on service error.
/// Returns an HTTP response or an HTTP error.
pub type SrvRespHandler<A, M> = Box<
    dyn FnOnce(
            &A,
            Result<M, ErrorMsg<M>>,
        ) -> Result<Response<BoxBody<Bytes, Infallible>>, HttpError>
        + Send,
>;

/// Enum to define all type of response to request it can be made
pub enum HyperResp<A, M>
where
    A: server::adaptor::HyperServerAdaptor<M>,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    /// Make an internal service request and process the response with the provided handler
    SrvReq(String, M, SrvRespHandler<A, M>),
    /// Make a direct HTTP response
    HttpResp(Response<BoxBody<Bytes, Infallible>>),
    /// Response with an HTTP error
    HttpErr(HttpError),
}

impl<A, M> std::fmt::Debug for HyperResp<A, M>
where
    A: server::adaptor::HyperServerAdaptor<M>,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HyperResp::SrvReq(name, msg, _) => f
                .debug_tuple("SrvReq")
                .field(name)
                .field(msg)
                .field(&"<handler>")
                .finish(),
            HyperResp::HttpResp(resp) => f.debug_tuple("HttpResp").field(resp).finish(),
            HyperResp::HttpErr(err) => f.debug_tuple("HttpErr").field(err).finish(),
        }
    }
}

impl<A, M> From<HttpError> for HyperResp<A, M>
where
    A: server::adaptor::HyperServerAdaptor<M>,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    fn from(err: HttpError) -> Self {
        Self::HttpErr(err)
    }
}

impl<A, M> From<hyper::Error> for HyperResp<A, M>
where
    A: server::adaptor::HyperServerAdaptor<M>,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    fn from(err: hyper::Error) -> Self {
        Self::HttpErr(HttpError::Hyper(err))
    }
}

impl<A, M> From<http::Error> for HyperResp<A, M>
where
    A: server::adaptor::HyperServerAdaptor<M>,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    fn from(err: http::Error) -> Self {
        Self::HttpErr(HttpError::Http(err))
    }
}

impl<A, M> From<Result<Response<BoxBody<Bytes, Infallible>>, http::Error>> for HyperResp<A, M>
where
    A: server::adaptor::HyperServerAdaptor<M>,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    fn from(res: Result<Response<BoxBody<Bytes, Infallible>>, http::Error>) -> Self {
        match res {
            Ok(response) => Self::HttpResp(response),
            Err(err) => Self::HttpErr(HttpError::Http(err)),
        }
    }
}

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

#[cfg(any(feature = "server", feature = "client"))]
#[cfg(test)]
pub mod tests;
