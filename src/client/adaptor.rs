use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::{Request, Response};
use prosa::core::{error::ProcError, service::ServiceError};
use url::Url;

use crate::client::proc::HyperClientProc;

#[cfg_attr(doc, aquamarine::aquamarine)]
/// Trait to define the Hyper adaptor structure
///
/// ```mermaid
/// graph LR
///     OUT1[Output HTTP server]
///     ProSA[ProSA Hyper Procesor]
///
///     ProSA-- HTTP request (process_client_request) -->OUT
///     OUT-- HTTP response (process_client_response) -->ProSA
/// ```
pub trait HyperClientAdaptor<M>
where
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa_utils::msg::tvf::Tvf
        + std::default::Default,
{
    /// User-Agent header value sent by the client
    #[cfg(target_family = "unix")]
    const USER_AGENT_HEADER: &'static str =
        concat!("ProSA-Hyper/", env!("CARGO_PKG_VERSION"), " (Unix)");
    #[cfg(target_family = "windows")]
    const USER_AGENT_HEADER: &'static str =
        concat!("ProSA-Hyper/", env!("CARGO_PKG_VERSION"), " (Windows)");
    #[cfg(all(not(target_family = "unix"), not(target_family = "windows")))]
    const USER_AGENT_HEADER: &'static str = concat!("ProSA-Hyper/", env!("CARGO_PKG_VERSION"));

    /// Create a new adaptor
    fn new(
        proc: &HyperClientProc<M>,
        prosa_name: &str,
    ) -> Result<Self, Box<dyn ProcError + Send + Sync>>
    where
        Self: Sized;

    /// Method to process input HTTP requests. Received by the ProSA through Hyper
    fn process_srv_request(
        &self,
        request: M,
        socket_url: &Url,
    ) -> Result<Request<BoxBody<Bytes, Infallible>>, ServiceError>;

    /// Method to process input HTTP response to respond with an TVF response.
    fn process_http_response(
        &self,
        resp: Result<Response<hyper::body::Incoming>, hyper::Error>,
    ) -> Result<M, ServiceError>;
}
