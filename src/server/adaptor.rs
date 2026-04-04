//! Hyper server adaptor definition
use std::convert::Infallible;

use bytes::Bytes;
use http::response;
use http_body_util::{Empty, Full, combinators::BoxBody};
use hyper::{Request, Response, StatusCode};
use prosa::core::{adaptor::Adaptor, error::ProcError, msg::ErrorMsg, proc::ProcBusParam as _};

use crate::{HttpError, HyperResp, PRODUCT_VERSION_HEADER};

use super::proc::HyperServerProc;

#[cfg_attr(doc, aquamarine::aquamarine)]
/// Trait to define the Hyper server adaptor structure
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
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    /// Create a new adaptor
    fn new(proc: &HyperServerProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>>
    where
        Self: Sized;

    /// Initiate a response builder with specific status code and headers from adaptor.
    /// The response with its header generated will be used by the processor in case of error
    fn response_builder<T>(&self, status_code: T) -> response::Builder
    where
        T: TryInto<StatusCode>,
        <T as TryInto<StatusCode>>::Error: Into<http::Error>,
    {
        Response::builder()
            .status(status_code)
            .header(hyper::header::SERVER, PRODUCT_VERSION_HEADER)
    }

    /// Method to process input HTTP requests. Received by the ProSA through Hyper
    fn process_http_request(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> impl std::future::Future<Output = HyperResp<Self, M>> + Send
    where
        Self: Sized + Send + Sync + 'static,
        M: 'static
            + Send
            + Sync
            + Sized
            + Clone
            + std::fmt::Debug
            + prosa::core::msg::Tvf
            + Default;
}

/// Convert a service error message into a default HTTP response.
///
/// This provides a default mapping from [`prosa::core::service::ServiceError`] variants to HTTP status codes:
/// - `NoError` -> `202 Accepted`
/// - `UnableToReachService` -> `503 Service Unavailable`
/// - `Timeout` -> `504 Gateway Timeout`
/// - `ProtocolError` -> `502 Bad Gateway`
///
/// The `response_builder` parameter allows customizing the response headers (e.g., adding a `Server` header).
pub fn default_srv_error_response<M, F>(
    err: &ErrorMsg<M>,
    response_builder: F,
) -> Result<Response<BoxBody<Bytes, Infallible>>, HttpError>
where
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa::core::msg::Tvf
        + std::default::Default,
    F: Fn(StatusCode) -> response::Builder,
{
    match err.get_err() {
        prosa::core::service::ServiceError::NoError(_) => response_builder(StatusCode::ACCEPTED)
            .body(BoxBody::new(Empty::<Bytes>::new()))
            .map_err(|e| e.into()),
        prosa::core::service::ServiceError::UnableToReachService(_) => {
            response_builder(StatusCode::SERVICE_UNAVAILABLE)
                .body(BoxBody::new(Full::new(Bytes::from("Can't reach service"))))
                .map_err(|e| e.into())
        }
        prosa::core::service::ServiceError::Timeout(_, _) => {
            response_builder(StatusCode::GATEWAY_TIMEOUT)
                .body(BoxBody::new(Empty::<Bytes>::new()))
                .map_err(|e| e.into())
        }
        prosa::core::service::ServiceError::ProtocolError(_) => {
            response_builder(StatusCode::BAD_GATEWAY)
                .body(BoxBody::new(Empty::<Bytes>::new()))
                .map_err(|e| e.into())
        }
    }
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
        + prosa::core::msg::Tvf
        + std::default::Default,
{
    fn new(proc: &HyperServerProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
        Ok(HelloHyperServerAdaptor {
            hello_msg: format!("Hello from {}", proc.name()),
        })
    }

    async fn process_http_request(
        &self,
        _req: Request<hyper::body::Incoming>,
    ) -> HyperResp<Self, M> {
        Response::builder()
            .status(200)
            .header("Server", PRODUCT_VERSION_HEADER)
            .body(BoxBody::new(Full::new(Bytes::from(self.hello_msg.clone()))))
            .into()
    }
}
