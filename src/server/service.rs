//! Hyper service definition

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{Empty, Full};
use hyper::service::Service;
use hyper::{Request, Response};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use prosa::core::msg::{InternalMsg, Msg};
use tokio::sync::{mpsc, oneshot};

use crate::hyper_version_str;

use super::HyperProcMsg;
use super::adaptor::HyperServerAdaptor;

#[derive(Debug, Clone)]
/// Struct to define parameters for a service (HTTP server)
pub(crate) struct HyperService<A, M>
where
    A: 'static + HyperServerAdaptor<M>,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa_utils::msg::tvf::Tvf
        + std::default::Default,
{
    adaptor: Arc<A>,
    proc_queue: mpsc::Sender<HyperProcMsg<M>>,
    h2: bool,
    metric_counter: Counter<u64>,
}

impl<A, M> HyperService<A, M>
where
    A: 'static + HyperServerAdaptor<M> + Clone,
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa_utils::msg::tvf::Tvf
        + std::default::Default,
{
    /// Method to create an Hyper service
    pub(crate) fn new(
        adaptor: Arc<A>,
        proc_queue: mpsc::Sender<HyperProcMsg<M>>,
        h2: bool,
        metric_counter: Counter<u64>,
    ) -> HyperService<A, M> {
        HyperService {
            adaptor,
            proc_queue,
            h2,
            metric_counter,
        }
    }

    async fn process_call(
        adaptor: Arc<A>,
        proc_queue: mpsc::Sender<HyperProcMsg<M>>,
        h2: bool,
        req: Request<hyper::body::Incoming>,
        metric_counter: Counter<u64>,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, hyper::Error> {
        match adaptor.process_http_request(req, h2).await {
            crate::HyperResp::SrvReq(srv_name, req) => {
                let resp =
                    HyperService::<A, M>::wait_intern_resp(adaptor, proc_queue, srv_name, req)
                        .await;
                if let Ok(ref res) = resp {
                    metric_counter.add(
                        1,
                        &[
                            KeyValue::new("code", res.status().as_u16() as i64),
                            KeyValue::new("version", hyper_version_str(res.version())),
                        ],
                    );
                }
                resp
            }
            crate::HyperResp::HttpResp(res) => {
                metric_counter.add(
                    1,
                    &[
                        KeyValue::new("code", res.status().as_u16() as i64),
                        KeyValue::new("version", hyper_version_str(res.version())),
                    ],
                );
                Ok(res)
            }
            crate::HyperResp::HttpErr(err) => {
                metric_counter.add(
                    1,
                    &[
                        KeyValue::new("code", 503),
                        KeyValue::new("version", if h2 { "HTTP/2" } else { "HTTP/1.1" }),
                    ],
                );
                Err(err)
            }
        }
    }

    /// Method to wait for response send by the ProSA HTTP processor
    async fn wait_intern_resp(
        adaptor: Arc<A>,
        proc_queue: mpsc::Sender<HyperProcMsg<M>>,
        service_name: String,
        request: M,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, hyper::Error> {
        let (resp_tx, resp_rx) = oneshot::channel::<InternalMsg<M>>();
        let _ = proc_queue
            .send(HyperProcMsg::new(service_name, request, resp_tx))
            .await;

        match resp_rx.await {
            Ok(msg) => match msg {
                InternalMsg::Response(msg) => Ok(adaptor.process_srv_response(msg.get_data())),
                InternalMsg::Error(err) => match err.get_err() {
                    prosa::core::service::ServiceError::NoError(_) => Ok(Response::builder()
                        .status(202)
                        .header("Server", A::SERVER_HEADER)
                        .body(BoxBody::new(Empty::<Bytes>::new()))
                        .unwrap()),
                    prosa::core::service::ServiceError::UnableToReachService(_) => {
                        Ok(Response::builder()
                            .status(503)
                            .header("Server", A::SERVER_HEADER)
                            .body(BoxBody::new(Full::new(Bytes::from("Can't reach service"))))
                            .unwrap())
                    }
                    prosa::core::service::ServiceError::Timeout(_, _) => Ok(Response::builder()
                        .status(504)
                        .header("Server", A::SERVER_HEADER)
                        .body(BoxBody::new(Empty::<Bytes>::new()))
                        .unwrap()),
                    prosa::core::service::ServiceError::ProtocolError(_) => Ok(Response::builder()
                        .status(502)
                        .header("Server", A::SERVER_HEADER)
                        .body(BoxBody::new(Empty::<Bytes>::new()))
                        .unwrap()),
                },
                _ => Ok(Response::builder()
                    .status(500)
                    .header("Server", A::SERVER_HEADER)
                    .body(BoxBody::new(Full::new(Bytes::from("Server error"))))
                    .unwrap()),
            },
            Err(_) => Ok(Response::builder()
                .status(503)
                .header("Server", A::SERVER_HEADER)
                .body(BoxBody::new(Full::new(Bytes::from(
                    "Can't handle your request for now",
                ))))
                .unwrap()),
        }
    }
}

impl<A, M> Service<Request<hyper::body::Incoming>> for HyperService<A, M>
where
    A: 'static + HyperServerAdaptor<M> + Clone + std::marker::Sync + std::marker::Send,
    M: 'static
        + std::marker::Send
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa_utils::msg::tvf::Tvf
        + std::default::Default
        + std::marker::Sync,
{
    type Response = Response<BoxBody<Bytes, Infallible>>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<hyper::body::Incoming>) -> Self::Future {
        Box::pin(HyperService::<A, M>::process_call(
            self.adaptor.clone(),
            self.proc_queue.clone(),
            self.h2,
            req,
            self.metric_counter.clone(),
        ))
    }
}
