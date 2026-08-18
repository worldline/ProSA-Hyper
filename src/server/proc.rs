use std::{env, sync::Arc, time::Duration};

use hyper::server::conn::{http1, http2};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::graceful::GracefulShutdown,
};
use prosa::{
    core::{
        adaptor::Adaptor,
        error::ProcError,
        msg::{InternalMsg, Msg, RequestMsg},
        proc::{Proc, ProcBusParam, ProcConfig as _, proc, proc_settings},
        service::ServiceError,
    },
    event::pending::PendingMsgs,
    io::listener::ListenerSetting,
    otel::KeyValue,
    tracing::{debug, info, warn},
};

use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, task::JoinHandle};
use url::Url;

use crate::{
    H2,
    server::{adaptor::HyperServerAdaptor, service::HyperService},
};

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
        let mut url =
            Url::parse("http://0.0.0.0:8080").expect("Default Hyper server URL should be valid");
        if let Ok(Ok(port)) = env::var("PORT").map(|p| p.parse::<u16>()) {
            url.set_port(Some(port))
                .expect("Default Hyper server URL should be base");
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
        + prosa::core::msg::Tvf
        + std::default::Default,
    A: 'static + Adaptor + HyperServerAdaptor<M> + std::marker::Send + std::marker::Sync,
{
    /// Main loop of the processor
    async fn internal_run(&mut self) -> Result<(), Box<dyn ProcError + Send + Sync>> {
        // Force default protocol to HTTP2 for SSL
        self.settings
            .listener
            .set_alpn(vec!["h2".into(), "http/1.1".into()]);

        let bound_listener = self.settings.listener.bind().await?;
        let local_addr = bound_listener.local_addr()?;
        let mut listener = Some(Arc::new(bound_listener));
        info!("Listening on {local_addr}");

        // Initiate an adaptor for the hyper server processor.
        // The very same instance is shared with every `HyperService`, so a configuration reload
        // through `Adaptor::reload_config` is seen by the requests being served
        let adaptor = Arc::new(A::new(self, local_addr)?);

        // Add proc main queue (id: 0)
        self.proc.add_proc().await?;

        // Declare an internal queue for HTTP requests
        let (http_tx, mut http_rx) = mpsc::channel::<RequestMsg<M>>(2048);

        // Declare a list for pending HTTP request
        let mut pending_req = PendingMsgs::<RequestMsg<M>, M>::default();

        // Meter to log HTTP reponses
        let meter = self.get_proc_param().meter("hyper_server");
        let observable_http_counter = meter
            .u64_counter("prosa_hyper_srv_count")
            .with_description("Hyper HTTP server counter")
            .build();
        let observable_http_socket = meter
            .i64_up_down_counter("prosa_hyper_srv_socket")
            .with_description("Hyper HTTP server socket counter")
            .build();

        // `Some` while the processor serves. Taken when it is asked to stop, to signal every open
        // connection to answer what it has in flight and then close
        let mut graceful = Some(GracefulShutdown::new());

        // `None` until the processor is asked to stop, then holds the task that drains the connections
        let mut draining: Option<JoinHandle<()>> = None;

        loop {
            // Clone the listener so the configuration reload can swap it while an accept is pending
            let accept_listener = listener.clone();
            tokio::select! {
                Some(msg) = self.internal_rx_queue.recv() => {
                    match msg {
                        InternalMsg::Request(msg) => panic!(
                            "The hyper processor {} receive a request {:?}",
                            self.get_proc_id(),
                            msg
                        ),
                        InternalMsg::Response(mut msg) => {
                            if let Some(hyper_msg) = pending_req.pull_msg(msg.get_id())
                                && let Some(data) = msg.take_data()
                            {
                                let _ = hyper_msg.return_to_sender(data);
                            }
                        }
                        InternalMsg::Error(mut err_msg) => {
                            if let Some(hyper_err_msg) = pending_req.pull_msg(err_msg.get_id()) {
                                let _ = hyper_err_msg.return_error_to_sender(err_msg.take_data(), err_msg.into_err());
                            }
                        }
                        InternalMsg::Service(table) => self.service = table,
                        InternalMsg::Config(config) => {
                            // A reload landing while the processor drains would bind a listener it will never accept from
                            if let Some(mut settings) = graceful.is_some()
                                .then(|| config.reload_proc::<HyperServerSettings>(self.proc.as_ref(), adaptor.as_ref()))
                                .flatten()
                            {
                                // Normalize the ALPN as done at startup before comparing
                                settings.listener.set_alpn(vec!["h2".into(), "http/1.1".into()]);

                                if settings.listener != self.settings.listener {
                                    match settings.listener.bind().await {
                                        Ok(new_listener) => {
                                            let local_addr = new_listener.local_addr()?;
                                            listener = Some(Arc::new(new_listener));
                                            info!("Reload the Hyper server processor configuration, listening on {local_addr}");
                                        }
                                        // An address the processor can't bind is no reason to lose the
                                        // one it serves on, so keep the listener and the settings that describe it
                                        Err(e) => {
                                            warn!("Can't listen on {}, keep the previous address: {e}", settings.listener.get_safe_url());
                                            settings.listener = self.settings.listener.clone();
                                        }
                                    }
                                }

                                // The service timeout is picked up by the next request
                                self.settings = settings;
                            }
                        }
                        InternalMsg::Shutdown => {
                            // Release the port right away. A listener left bound but never accepted from
                            // would have the kernel complete handshakes into the accept queue, so a new
                            // client would wait out the whole drain only to be reset
                            listener = None;

                            if let Some(graceful) = graceful.take() {
                                warn!("The Hyper server processor stops accepting and drains its connections");
                                draining = Some(tokio::task::spawn(graceful.shutdown()));
                            }
                        }
                    }
                },
                Some(mut http_msg) = http_rx.recv() => {
                    if let Some(service) = self.service.get_proc_service(http_msg.get_service())
                        && let Some(http_msg_data) = http_msg.take_data()
                    {
                        let request = RequestMsg::new(http_msg.get_service().clone(), http_msg_data, self.proc.get_service_queue().clone());
                        let request_id = request.get_id();

                        // A processor that stopped since the service table was received only concerns
                        // this request. Answering it keeps the ones already in flight alive
                        if let Err(e) = service.proc_queue.send(InternalMsg::Request(request)).await {
                            warn!(parent: http_msg.get_span(), code = "503", "hyper::server::Msg");
                            debug!("Can't reach the service {}: {e}", http_msg.get_service());
                            let service_name = http_msg.get_service().clone();
                            let _ = http_msg.return_error_to_sender(None, ServiceError::UnableToReachService(service_name));
                        } else {
                            pending_req.push_with_id(request_id, http_msg, self.settings.service_timeout);
                        }
                    } else {
                        warn!(
                            parent: http_msg.get_span(),
                            code = "503",
                            "hyper::server::Msg",
                        );
                        let data = http_msg.take_data();
                        let service_name = http_msg.get_service().clone();
                        let _ = http_msg.return_error_to_sender(data, ServiceError::UnableToReachService(service_name));
                    }
                },
                Some(accept_result) = async {
                    match &accept_listener {
                        Some(listener) => Some(listener.accept_raw().await),
                        None => None,
                    }
                }, if accept_listener.is_some() => {
                    let (stream, addr) = accept_result?;

                    // The watcher is taken before the connection is spawned, so a shutdown asked
                    // in between is not missed
                    let (Some(listener), Some(watcher)) = (accept_listener.clone(), graceful.as_ref().map(GracefulShutdown::watcher)) else {
                        continue;
                    };

                    let service_adaptor = adaptor.clone();
                    let http_tx = http_tx.clone();
                    let http_counter = observable_http_counter.clone();
                    let http_socket = observable_http_socket.clone();
                    tokio::task::spawn(async move {
                        let handshake = listener.handshake(stream).await;

                        // Only the handshake needs the listener. Release it right away so a listener
                        // retired by a configuration reload doesn't stay bound until the last
                        // connection it accepted is closed
                        drop(listener);

                        match handshake {
                            Ok(stream) => {
                                let is_http2 = stream.selected_alpn_check(|alpn| alpn == H2);

                                http_socket.add(1, &[KeyValue::new("version", if is_http2 { "HTTP/2" } else { "HTTP/1.1" })]);

                                let io = TokioIo::new(stream);
                                let service = HyperService::new(service_adaptor, http_tx, http_counter);
                                if is_http2 {
                                    if let Err(err) = watcher.watch(
                                        http2::Builder::new(TokioExecutor::new()).serve_connection(
                                            io,
                                            service,
                                        )
                                    ).await
                                    {
                                        warn!("Failed to serve http/2 connection[{addr}]: {err:?}");
                                    }
                                } else if let Err(err) = watcher.watch(
                                    http1::Builder::new().serve_connection(
                                        io,
                                        service,
                                    )
                                ).await
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
                // Every connection has been answered and closed, nothing is left to serve
                Some(_) = async {
                    match draining.as_mut() {
                        Some(drain) => Some(drain.await),
                        None => None,
                    }
                }, if draining.is_some() => break,
                Some(mut msg) = pending_req.pull(), if !pending_req.is_empty() => {
                    warn!(parent: msg.get_span(), "Timeout message {:?}", msg);
                    let data = msg.take_data();
                    let service_name = msg.get_service().clone();
                    let _ = msg.return_error_to_sender(
                        data,
                        ServiceError::Timeout(
                            service_name,
                            self.settings.service_timeout.as_millis() as u64,
                        ),
                    );
                },
            }
        }

        adaptor.terminate();
        self.proc.remove_proc(None).await?;
        warn!("The Hyper server processor is shut down");

        Ok(())
    }
}
