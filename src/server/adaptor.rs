//! Hyper server adaptor definition
use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::{Full, combinators::BoxBody};
use hyper::{Request, Response};
use prosa::core::{adaptor::Adaptor, error::ProcError};

use crate::HyperResp;

use super::proc::HyperServerProc;

#[cfg_attr(doc, aquamarine::aquamarine)]
/// Trait to define the Hyper adaptor structure
///
/// ```mermaid
/// graph LR
///     IN[Input HTTP server]
///     ProSA[ProSA Hyper Procesor]
///
///     IN-- HTTP request (process_server_request) -->ProSA
///     ProSA-- HTTP response (process_server_response) -->IN
/// ```
pub trait HyperServerAdaptor<M>
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
    /// Server header value send by the server
    #[cfg(target_family = "unix")]
    const SERVER_HEADER: &'static str =
        concat!("ProSA-Hyper/", env!("CARGO_PKG_VERSION"), " (Unix)");
    #[cfg(target_family = "windows")]
    const SERVER_HEADER: &'static str =
        concat!("ProSA-Hyper/", env!("CARGO_PKG_VERSION"), " (Windows)");
    #[cfg(all(not(target_family = "unix"), not(target_family = "windows")))]
    const SERVER_HEADER: &'static str = concat!("ProSA-Hyper/", env!("CARGO_PKG_VERSION"));

    /// Create a new adaptor
    fn new(
        proc: &HyperServerProc<M>,
        prosa_name: &str,
    ) -> Result<Self, Box<dyn ProcError + Send + Sync>>
    where
        Self: Sized;

    /// Method to process input HTTP requests. Received by the ProSA through Hyper
    fn process_http_request(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> impl std::future::Future<Output = HyperResp<M>> + Send;

    /// Method to process input response to respond with an Hyper HTTP response.
    fn process_srv_response(&self, resp: &M) -> Response<BoxBody<Bytes, Infallible>>;
}

/// Hello adaptor for the Hyper server processor. Use to respond to a request with a simple hello message
#[derive(Debug, Adaptor, Clone)]
pub struct HelloHyperServerAdaptor {
    hello_msg: String,
}

impl<M> HyperServerAdaptor<M> for HelloHyperServerAdaptor
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
    fn new(
        _proc: &HyperServerProc<M>,
        prosa_name: &str,
    ) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
        Ok(HelloHyperServerAdaptor {
            hello_msg: format!("Hello from {prosa_name}"),
        })
    }

    async fn process_http_request(&self, _req: Request<hyper::body::Incoming>) -> HyperResp<M> {
        HyperResp::<M>::HttpResp(
            Response::builder()
                .status(200)
                .header(
                    "Server",
                    <HelloHyperServerAdaptor as HyperServerAdaptor<M>>::SERVER_HEADER,
                )
                .body(BoxBody::new(Full::new(Bytes::from(self.hello_msg.clone()))))
                .unwrap(),
        )
    }

    fn process_srv_response(&self, _resp: &M) -> Response<BoxBody<Bytes, Infallible>> {
        panic!("No message should be send to an external service")
    }
}
