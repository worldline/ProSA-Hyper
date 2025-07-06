//! MCP server adaptor definition
use prosa::core::error::ProcError;
use tokio::sync::mpsc;

use crate::server::{HyperProcMsg, mcp::proc::McpServerProc};

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
pub trait McpServerAdaptor<M>
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
    /// Create a new adaptor
    fn new(
        proc: &McpServerProc<M>,
        prosa_name: &str,
        service_queue: mpsc::Sender<HyperProcMsg<M>>,
    ) -> Result<Self, Box<dyn ProcError + Send + Sync>>
    where
        Self: Sized;
    /*
    // Method to process input HTTP requests. Received by the ProSA through Hyper
    fn process_http_request(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> impl std::future::Future<Output = HyperResp<M>> + Send;

    // Method to process input response to respond with an Hyper HTTP response.
    fn process_srv_response(&self, resp: &M) -> Response<BoxBody<Bytes, Infallible>>;*/
}
