#![allow(dead_code)]

use prosa::{
    core::{
        adaptor::Adaptor,
        error::ProcError,
        msg::{InternalMsg, Msg},
    },
    stub::{adaptor::StubAdaptor, proc::StubProc},
};
use prosa_hyper::server::{
    HyperProcMsg,
    mcp::{adaptor::McpServerAdaptor, proc::McpServerProc},
};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::Parameters, wrapper::Json},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SumRequest {
    #[schemars(description = "the left hand side number")]
    pub a: i32,
    pub b: i32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SubRequest {
    #[schemars(description = "the left hand side number")]
    pub a: i32,
    #[schemars(description = "the right hand side number")]
    pub b: i32,
}

#[derive(Debug, Clone)]
pub struct Calculator<M>
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
    service_queue: mpsc::Sender<HyperProcMsg<M>>,
    tool_router: ToolRouter<Self>,
}

// FIXME replace by macro when it'll be fix
impl<M> Adaptor for Calculator<M>
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
    fn terminate(&mut self) {
        // Do nothing
    }
}

#[tool_router]
impl<M> Calculator<M>
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
    pub async fn send_msg(
        &self,
        service_name: String,
        request: M,
    ) -> Result<InternalMsg<M>, rmcp::Error> {
        let (resp_tx, resp_rx) = oneshot::channel::<InternalMsg<M>>();
        let _ = self
            .service_queue
            .send(HyperProcMsg::new(service_name, request, resp_tx))
            .await;

        resp_rx
            .await
            .map_err(|e| rmcp::Error::resource_not_found(e.to_string(), None))
    }

    #[tool(description = "Calculate the sum of two numbers")]
    async fn sum(
        &self,
        Parameters(SumRequest { a, b }): Parameters<SumRequest>,
    ) -> Result<String, rmcp::Error> {
        let mut request = M::default();
        request.put_signed(1, a as i64);
        request.put_signed(2, b as i64);

        match self.send_msg("sum".to_string(), request).await? {
            InternalMsg::Response(msg) => Ok(msg
                .get_data()
                .get_signed(3)
                .map_err(|e| rmcp::Error::invalid_request(e.to_string(), None))?
                .to_string()),
            InternalMsg::Error(err) => Err(rmcp::Error::internal_error(format!("{err:?}"), None)),
            _ => Err(rmcp::Error::internal_error("Wrong message returned", None)),
        }
    }

    #[tool(description = "Calculate the difference of two numbers")]
    async fn sub(
        &self,
        Parameters(SubRequest { a, b }): Parameters<SubRequest>,
    ) -> Result<Json<i32>, rmcp::Error> {
        let mut request = M::default();
        request.put_signed(1, a as i64);
        request.put_signed(2, b as i64);

        match self.send_msg("sub".to_string(), request).await? {
            InternalMsg::Response(msg) => Ok(Json(
                msg.get_data()
                    .get_signed(3)
                    .map_err(|e| rmcp::Error::invalid_request(e.to_string(), None))?
                    as i32,
            )),
            InternalMsg::Error(err) => Err(rmcp::Error::internal_error(format!("{err:?}"), None)),
            _ => Err(rmcp::Error::internal_error("Wrong message returned", None)),
        }
    }
}

#[tool_handler]
impl<M> ServerHandler for Calculator<M>
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
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("A simple calculator".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

impl<M> McpServerAdaptor<M> for Calculator<M>
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
        _proc: &McpServerProc<M>,
        _prosa_name: &str,
        service_queue: mpsc::Sender<HyperProcMsg<M>>,
    ) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
        Ok(Self {
            service_queue,
            tool_router: Self::tool_router(),
        })
    }
}

impl<M> StubAdaptor<M> for Calculator<M>
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
    fn new(_proc: &StubProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
        // Just create a dummy queue just to create a new `Calculator` instance. It'll not use
        let (mcp_tx, mut _mcp_rx) = mpsc::channel::<HyperProcMsg<M>>(1);
        Ok(Self {
            service_queue: mcp_tx,
            tool_router: Self::tool_router(),
        })
    }

    fn process_request(&mut self, service_name: &str, request: &M) -> M {
        debug!("Receive request[{service_name:}]: {request:?}");
        let mut response = request.clone();
        match service_name {
            "sum" => {
                if let (Ok(a), Ok(b)) = (request.get_signed(1), request.get_signed(2)) {
                    response.put_signed(3, a + b);
                }
            }
            "sub" => {
                if let (Ok(a), Ok(b)) = (request.get_signed(1), request.get_signed(2)) {
                    response.put_signed(3, a - b);
                }
            }
            _ => {}
        }

        response
    }
}
