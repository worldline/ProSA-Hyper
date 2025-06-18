use std::{env, sync::Arc, time::Duration};

use hyper::server::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use opentelemetry::KeyValue;
use prosa::{
    core::{
        adaptor::Adaptor,
        error::ProcError,
        msg::{ErrorMsg, InternalMsg, Msg, RequestMsg},
        proc::{Proc, ProcBusParam, ProcConfig as _, proc, proc_settings},
        service::ServiceError,
    },
    event::pending::PendingMsgs,
    io::{listener::ListenerSetting, stream::Stream, url_is_ssl},
};

use prosa_utils::config::ssl::SslConfig;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{Level, debug, info, span, warn};
use url::Url;

use crate::{H2, server::service::HyperService};

use super::{HyperProcMsg, adaptor::HyperServerAdaptor};

/// Hyper server processor settings
#[proc_settings]
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HyperServerSettings {
    /// Listener settings
    #[serde(default = "HyperServerSettings::default_listener")]
    pub listener: ListenerSetting,
    /// Timeout for internal service requests
    #[serde(default = "HyperServerSettings::default_service_timeout")]
    pub service_timeout: Duration,
}

impl HyperServerSettings {
    fn default_listener() -> ListenerSetting {
        let mut url = Url::parse("http://0.0.0.0:8080").unwrap();
        if let Ok(Ok(port)) = env::var("PORT").map(|p| p.parse::<u16>()) {
            url.set_port(Some(port)).unwrap();
        }

        ListenerSetting::new(url, None)
    }

    fn default_service_timeout() -> Duration {
        Duration::from_millis(800)
    }

    /// Create a new Hyper Server settings
    pub fn new(listener: ListenerSetting, service_timeout: Duration) -> HyperServerSettings {
        HyperServerSettings {
            listener,
            service_timeout,
            ..Default::default()
        }
    }
}

#[proc_settings]
impl Default for HyperServerSettings {
    fn default() -> HyperServerSettings {
        HyperServerSettings {
            listener: Self::default_listener(),
            service_timeout: Self::default_service_timeout(),
        }
    }
}

/// Hyper server processor
#[proc(settings = HyperServerSettings)]
pub struct HyperServerProc {}

#[proc]
impl<M, A> Proc<A> for HyperServerProc
where
    M: 'static
        + std::marker::Send
        + std::marker::Sync
        + std::marker::Sized
        + std::clone::Clone
        + std::fmt::Debug
        + prosa_utils::msg::tvf::Tvf
        + std::default::Default,
    A: 'static + Adaptor + HyperServerAdaptor<M> + Clone + std::marker::Send + std::marker::Sync,
{
    /// Main loop of the processor
    async fn internal_run(&mut self, name: String) -> Result<(), Box<dyn ProcError + Send + Sync>> {
        // Initiate an adaptor for the stub processor
        let mut adaptor = A::new(self, &name)?;

        // Add proc main queue (id: 0)
        self.proc.add_proc().await?;

        // Declare an internal queue for HTTP requests
        let (http_tx, mut http_rx) = mpsc::channel::<HyperProcMsg<M>>(2048);

        // Declare a list for pending HTTP request
        let mut pending_req = PendingMsgs::<HyperProcMsg<M>, M>::default();
        let mut message_ref_request = 0;

        // Set default protocol to HTTP2
        if url_is_ssl(&self.settings.listener.url) {
            if let Some(ssl) = self.settings.listener.ssl.as_mut() {
                ssl.set_alpn(vec!["h2".into(), "http/1.1".into()]);
            } else {
                let mut ssl = SslConfig::default();
                ssl.set_alpn(vec!["h2".into(), "http/1.1".into()]);
                self.settings.listener.ssl = Some(ssl);
            }
        }

        // Meter to log HTTP reponses
        let meter = self.get_proc_param().meter("hyper_server");
        let observable_http_counter = meter
            .u64_counter("prosa_hyper_srv_count")
            .with_description("Hyper HTTP counter")
            .init();
        let observable_http_socket = meter
            .i64_up_down_counter("prosa_hyper_srv_socket")
            .with_description("Hyper HTTP socket counter")
            .init();

        let listener = Arc::new(self.settings.listener.bind().await?);
        info!("Listening on {:?}", listener.local_addr());
        loop {
            tokio::select! {
                Some(msg) = self.internal_rx_queue.recv() => {
                    match msg {
                        InternalMsg::Request(msg) => panic!(
                            "The hyper processor {} receive a request {:?}",
                            self.get_proc_id(),
                            msg
                        ),
                        InternalMsg::Response(msg) => {
                            if let Some(hyper_msg) = pending_req.pull_msg(msg.get_id()) {
                                let _ = hyper_msg.response_queue.send(InternalMsg::Response(msg));
                            }
                        }
                        InternalMsg::Error(err_msg) => {
                            if let Some(hyper_err_msg) = pending_req.pull_msg(err_msg.get_id()) {
                                let _ = hyper_err_msg
                                    .response_queue
                                    .send(InternalMsg::Error(err_msg));
                            }
                        }
                        InternalMsg::Command(_) => todo!(),
                        InternalMsg::Config => todo!(),
                        InternalMsg::Service(table) => self.service = table,
                        InternalMsg::Shutdown => {
                            adaptor.terminate();
                            self.proc.remove_proc(None).await?;
                            warn!("The Hyper server processor will shut down");
                            return Ok(());
                        }
                    }
                },
                Some(http_msg) = http_rx.recv() => {
                    let service_name = http_msg.get_service().clone();
                    if let Some(service) = self.service.get_proc_service(&service_name, message_ref_request) {
                        debug!("The service is find: {service:?}, send to the internal service");
                        service.proc_queue.send(InternalMsg::Request(RequestMsg::new(message_ref_request, service_name, http_msg.get_data().clone(), self.proc.get_service_queue().clone()))).await.unwrap();
                        pending_req.push_with_id(message_ref_request, http_msg, self.settings.service_timeout);

                        message_ref_request += 1;
                    } else {
                        let origin_data = http_msg.get_data().clone();
                        let _ = http_msg.response_queue.send(InternalMsg::Error(ErrorMsg::new(0, service_name.clone(), span!(Level::WARN, "hyper::server::Msg", code = "503"), origin_data, ServiceError::UnableToReachService(service_name))));
                    }
                },
                accept_result = listener.accept_raw() => {
                    let (stream, addr) = accept_result?;

                    let listener = listener.clone();
                    let adaptor = adaptor.clone();
                    let http_tx = http_tx.clone();
                    let http_counter = observable_http_counter.clone();
                    let http_socket = observable_http_socket.clone();
                    tokio::task::spawn(async move {
                        match listener.handshake(stream).await {
                            Ok(stream) => {
                                let is_http2 = if let Stream::Ssl(ssl) = &stream {
                                    if let Some(alpn) = ssl.ssl().selected_alpn_protocol() {
                                        alpn == H2
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                };

                                http_socket.add(1, &[KeyValue::new("version", if is_http2 { "HTTP/2" } else { "HTTP/1.1" })]);

                                let io = TokioIo::new(stream);
                                if is_http2 {
                                    if let Err(err) = http2::Builder::new(TokioExecutor::new())
                                        .serve_connection(
                                            io,
                                            HyperService::new(adaptor, http_tx, true, http_counter),
                                        )
                                        .await
                                    {
                                        warn!("Failed to serve http/2 connection[{addr}]: {err:?}");
                                    }
                                } else if let Err(err) = http1::Builder::new()
                                    .serve_connection(
                                        io,
                                        HyperService::new(adaptor, http_tx, false, http_counter),
                                    )
                                    .await
                                {
                                    warn!("Failed to serve http/1 connection[{addr}]: {err:?}");
                                }

                                http_socket.add(-1, &[KeyValue::new("version", if is_http2 { "HTTP/2" } else { "HTTP/1.1" })]);
                            }
                            Err(e) => warn!("Failed to handshake with client[{addr}]: {e:?}"),
                        }

                        debug!("Connection closed {addr}");
                    });
                },
                Some(msg) = pending_req.pull(), if !pending_req.is_empty() => {
                    warn!(parent: msg.get_span(), "Timeout message {:?}", msg);

                    let service_name = msg.get_service().clone();
                    let span_msg = msg.get_span().clone();
                    let origin_data = msg.get_data().clone();
                    let _ = msg.response_queue.send(InternalMsg::Error(ErrorMsg::new(0, service_name.clone(), span_msg, origin_data, ServiceError::Timeout(service_name, self.settings.service_timeout.as_millis() as u64))));
                },
            }
        }
    }
}
