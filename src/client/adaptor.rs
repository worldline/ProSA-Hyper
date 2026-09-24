use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::{Request, Response};
use prosa::core::{error::ProcError, service::ServiceError};
use url::Url;

use crate::client::proc::HyperClientProc;

/// Trait to define the Hyper adaptor structure
///
#[doc = simple_mermaid::mermaid!("diagrams/adaptor.mmd")]
pub trait HyperClientAdaptor<M>
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
    /// Create a new adaptor
    fn new(proc: &HyperClientProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>>
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
    ) -> impl std::future::Future<Output = Result<M, ServiceError>> + Send;
}
