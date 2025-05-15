//! ProSA Hyper processor to handle HTTP client and server

#![warn(missing_docs)]

use std::{convert::Infallible, io};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::{Response, Version};
use prosa::core::error::ProcError;
use thiserror::Error;

const H2: &[u8] = b"h2";

/// Global Hyper processor error
#[derive(Debug, Error)]
pub enum HyperProcError {
    /// IO Error
    #[error("Hyper IO error: {0}")]
    Io(#[from] io::Error),
    /// Hyper Error
    #[error("Hyper error: {0}")]
    Hyper(#[from] hyper::Error),
    /// Other Error
    #[error("Other error: {0}")]
    Other(String),
}

impl ProcError for HyperProcError {
    fn recoverable(&self) -> bool {
        match self {
            HyperProcError::Io(error) => error.recoverable(),
            HyperProcError::Hyper(_) => true,
            HyperProcError::Other(_) => true,
        }
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

pub mod server;
