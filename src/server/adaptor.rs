//! Hyper server adaptor definition
use std::convert::Infallible;

use bytes::Bytes;
use http::response;
use http_body_util::{Empty, Full, combinators::BoxBody};
use hyper::{Request, Response, StatusCode};
use prosa::core::{adaptor::Adaptor, error::ProcError, msg::ErrorMsg, proc::ProcBusParam as _};

use crate::{HttpError, HyperResp};

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
    /// Server header value send by the server
    #[cfg(target_family = "unix")]
    const SERVER_HEADER: &'static str = concat!(
        env!("CARGO_PKG_NAME"),
        "/",
        env!("CARGO_PKG_VERSION"),
        " (Unix)"
    );
    #[cfg(target_family = "windows")]
    const SERVER_HEADER: &'static str = concat!(
        env!("CARGO_PKG_NAME"),
        "/",
        env!("CARGO_PKG_VERSION"),
        " (Windows)"
    );
    #[cfg(all(not(target_family = "unix"), not(target_family = "windows")))]
    const SERVER_HEADER: &'static str =
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

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
            .header(hyper::header::SERVER, Self::SERVER_HEADER)
    }

    /// Method to process input HTTP requests. Received by the ProSA through Hyper
    fn process_http_request(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> impl std::future::Future<Output = HyperResp<M>> + Send;

    /// Method to process input response to respond with an Hyper HTTP response.
    fn process_srv_response(
        &self,
        resp: M,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, HttpError>;

    /// Method to process input service error to respond with an Hyper HTTP response.
    fn process_srv_error(
        &self,
        err: ErrorMsg<M>,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, HttpError> {
        match err.get_err() {
            prosa::core::service::ServiceError::NoError(_) => self
                .response_builder(StatusCode::ACCEPTED)
                .body(BoxBody::new(Empty::<Bytes>::new()))
                .map_err(|e| e.into()),
            prosa::core::service::ServiceError::UnableToReachService(_) => self
                .response_builder(StatusCode::SERVICE_UNAVAILABLE)
                .body(BoxBody::new(Full::new(Bytes::from("Can't reach service"))))
                .map_err(|e| e.into()),
            prosa::core::service::ServiceError::Timeout(_, _) => self
                .response_builder(StatusCode::GATEWAY_TIMEOUT)
                .body(BoxBody::new(Empty::<Bytes>::new()))
                .map_err(|e| e.into()),
            prosa::core::service::ServiceError::ProtocolError(_) => self
                .response_builder(StatusCode::BAD_GATEWAY)
                .body(BoxBody::new(Empty::<Bytes>::new()))
                .map_err(|e| e.into()),
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

    async fn process_http_request(&self, _req: Request<hyper::body::Incoming>) -> HyperResp<M> {
        Response::builder()
            .status(200)
            .header(
                "Server",
                <HelloHyperServerAdaptor as HyperServerAdaptor<M>>::SERVER_HEADER,
            )
            .body(BoxBody::new(Full::new(Bytes::from(self.hello_msg.clone()))))
            .into()
    }

    fn process_srv_response(
        &self,
        _resp: M,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, HttpError> {
        panic!("No message should be send to an external service")
    }
}
