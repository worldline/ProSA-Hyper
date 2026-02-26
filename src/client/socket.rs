use std::{
    convert::Infallible,
    io,
    os::fd::AsRawFd,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::{
    Request,
    client::conn::{http1, http2},
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use opentelemetry::{KeyValue, metrics::Histogram};
use prosa::{
    core::{
        adaptor::Adaptor,
        msg::{InternalMsg, Msg as _, RequestMsg},
        proc::{ProcBusParam as _, ProcParam},
        service::ServiceError,
    },
    io::{SslConfig, stream::TargetSetting, url_is_ssl},
};
use tokio::{
    task::JoinSet,
    time::{self, timeout},
};
use tracing::{debug, info, warn};

use crate::{H2, HyperProcError, client::adaptor::HyperClientAdaptor, hyper_version_str};

/// Hyper client socket
#[derive(Debug, Clone)]
pub struct HyperClientSocket {
    /// Target of the socket
    target: TargetSetting,
    /// Whether the socket is using HTTP/2
    is_http2: bool,
    /// HTTP message timeout duration
    http_timeout: Duration,
}

macro_rules! close_socket {
    ($self:ident, $proc:ident, $socket_id:ident, $msg_queue:ident, $service_name:ident, $return:expr) => {
        $proc.remove_proc_queue($socket_id as u32).await?;
        while let Ok(msg) = $msg_queue.try_recv() {
            if let InternalMsg::Request(req_msg) = msg {
                let _ = req_msg.return_error_to_sender(
                    None,
                    ServiceError::UnableToReachService($service_name.clone()),
                );
            }
        }
        return $return;
    };
}

impl HyperClientSocket {
    pub fn new(mut target: TargetSetting, http_timeout: u64) -> Self {
        // Set default protocol to HTTP2 if target enabled SSL
        if let Some(ssl) = target.ssl.as_mut() {
            info!(
                "Target {} enable SSL, set ALPN to support HTTP/2 and HTTP/1.1",
                target.url
            );
            ssl.set_alpn(vec!["h2".into(), "http/1.1".into()]);
        } else if url_is_ssl(&target.url) {
            let mut ssl = SslConfig::default();
            ssl.set_alpn(vec!["h2".into(), "http/1.1".into()]);
            target.ssl = Some(ssl);
        }

        HyperClientSocket {
            target,
            is_http2: false,
            http_timeout: Duration::from_millis(http_timeout),
        }
    }

    /// Method to spawn a task to handle the Hyper client socket
    pub fn spawn<M, A>(
        mut self,
        join_set: &mut JoinSet<Result<Self, HyperProcError>>,
        proc: Arc<ProcParam<M>>,
        adaptor: Arc<A>,
        service_name: String,
        message_histogram: Histogram<u64>,
    ) where
        M: 'static
            + std::marker::Send
            + std::marker::Sync
            + std::marker::Sized
            + std::clone::Clone
            + std::fmt::Debug
            + prosa::core::msg::Tvf
            + std::default::Default,
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        join_set.spawn(async move {
            let io = TokioIo::new(self.target.connect().await?);
            let socket_id = io.inner().as_raw_fd();
            let target_addr = self.target.to_string();
            if io.inner().selected_alpn_check(|alpn| alpn == H2) {
                self.is_http2 = true;
                match time::timeout(
                    Duration::from_millis(self.target.connect_timeout),
                    http2::handshake(TokioExecutor::new(), io),
                )
                .await
                {
                    Ok(Ok((sender, mut connection))) => {
                        debug!(socket_id = socket_id, addr = target_addr, "Connected to HTTP2 remote");
                        let (tx_queue, mut rx_queue) = tokio::sync::mpsc::channel(2048);
                        proc.add_proc_queue(tx_queue, socket_id as u32).await?;
                        proc.add_service(vec![service_name.clone()], socket_id as u32)
                            .await?;

                        loop {
                            tokio::select! {
                                // Closed the socket
                                Err(_) = &mut connection => {
                                    debug!(socket_id = socket_id, addr = target_addr, "Remote HTTP2 close the socket");
                                    close_socket!(self, proc, socket_id, rx_queue, service_name, Ok(self));
                                }
                                // Receive a message to send from the queue
                                Some(msg) = rx_queue.recv() => {
                                    match msg {
                                        InternalMsg::Request(mut msg) => {
                                            if let Some(data) = msg.take_data() {
                                                let req_instant = Instant::now();
                                                let mut sender = sender.clone();
                                                let adaptor = adaptor.clone();
                                                let target_url = self.target.url.clone();
                                                let service_name = service_name.clone();
                                                let message_histogram = message_histogram.clone();
                                                tokio::spawn(async move {
                                                    match adaptor.process_srv_request(data, &target_url) {
                                                        Ok(http_request) => {
                                                            match timeout(self.http_timeout, sender.send_request(http_request)).await {
                                                                Ok(http_response) => {
                                                                    let (code, version) = http_response.as_ref().map_or((500, "HTTP/2"), |r| (r.status().as_u16() as i64, hyper_version_str(r.version())));
                                                                    message_histogram.record(
                                                                        req_instant.elapsed().as_millis() as u64,
                                                                        &[
                                                                            KeyValue::new("target", target_url.to_string()),
                                                                            KeyValue::new("code", code),
                                                                            KeyValue::new("version", version),
                                                                        ],
                                                                    );

                                                                    match adaptor.process_http_response(http_response).await {
                                                                        Ok(response) => {
                                                                            let _ = msg.return_to_sender(response);
                                                                        },
                                                                        Err(e) => { let _ = msg.return_error_to_sender(None, e); },
                                                                    }
                                                                },
                                                                Err(_) => { let _ = msg.return_error_to_sender(None, ServiceError::Timeout(service_name.clone(), self.http_timeout.as_millis() as u64)); },
                                                            };
                                                        },
                                                        Err(e) => {
                                                            let _ = msg.return_error_to_sender(None, e);
                                                        },
                                                    }
                                                });
                                            } else {
                                                let _ = msg.return_error_to_sender(None, ServiceError::UnableToReachService(service_name.clone()));
                                            }
                                        },
                                        InternalMsg::Response(msg) => panic!(
                                            "The hyper client socket {}/{socket_id} receive a response {:?}",
                                            proc.get_proc_id(),
                                            msg
                                        ),
                                        InternalMsg::Error(err_msg) => panic!(
                                            "The hyper client socket {}/{socket_id} receive an error {:?}",
                                            proc.get_proc_id(),
                                            err_msg
                                        ),
                                        InternalMsg::Command(_) => todo!(),
                                        InternalMsg::Config => todo!(),
                                        InternalMsg::Service(_table) => {/* Will not use service table */},
                                        InternalMsg::Shutdown => {
                                            // Remove the socket queue and wait message to finish
                                            close_socket!(self, proc, socket_id, rx_queue, service_name, Ok(self));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(socket_id = socket_id, addr = target_addr, "HTTP2 handshake error: {}", e);
                        Err(HyperProcError::Hyper(e, target_addr))
                    }
                    Err(_) => {
                        warn!(
                            socket_id = socket_id,
                            addr = target_addr,
                            "HTTP2 handshake timeout after {} ms",
                            self.target.connect_timeout
                        );
                        Err(HyperProcError::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "HTTP2 handshake timeout after {} ms",
                                self.target.connect_timeout
                            ),
                        )))
                    }
                }
            } else {
                match time::timeout(
                    Duration::from_millis(self.target.connect_timeout),
                    http1::handshake(io),
                )
                .await
                {
                    Ok(Ok((mut sender, mut connection))) => {
                        debug!(socket_id = socket_id, addr = target_addr, "Connected to HTTP1 remote");
                        let (tx_queue, mut rx_queue) = tokio::sync::mpsc::channel(2048);
                        proc.add_proc_queue(tx_queue, socket_id as u32).await?;
                        debug!(socket_id = socket_id, addr = target_addr, "HTTP client expose service name: {}", service_name);
                        proc.add_service(vec![service_name.clone()], socket_id as u32)
                            .await?;
                        #[allow(clippy::type_complexity)]
                        let mut msg_to_send: Option<(RequestMsg<M>, Request<BoxBody<Bytes, Infallible>>)> = None;
                        let mut req_instant = Instant::now();

                        loop {
                            if let Some((msg, http_request)) = msg_to_send.take() {
                                let http_log = http_request.uri().to_string();
                                tokio::select! {
                                    // Closed the socket
                                    Err(_) = &mut connection => {
                                        debug!(socket_id = socket_id, addr = target_addr, "Remote HTTP1 close the socket");
                                        close_socket!(self, proc, socket_id, rx_queue, service_name, Ok(self));
                                    }
                                    // Send an HTTP request
                                    response_sent = timeout(self.http_timeout, sender.send_request(http_request)) => {
                                        match response_sent {
                                            Ok(response) => {
                                                let (code, version) = response.as_ref().map_or((500, "HTTP/1"), |r| (r.status().as_u16() as i64, hyper_version_str(r.version())));
                                                message_histogram.record(
                                                    req_instant.elapsed().as_millis() as u64,
                                                    &[
                                                        KeyValue::new("target", target_addr.clone()),
                                                        KeyValue::new("code", code),
                                                        KeyValue::new("version", version),
                                                    ],
                                                );

                                                match adaptor.process_http_response(response).await {
                                                    Ok(r) => {
                                                        let _ = msg.return_to_sender(r);
                                                    },
                                                    Err(e) => {
                                                        let _ = msg.return_error_to_sender(None, e);
                                                    }
                                                }
                                            },
                                            Err(elapsed) => {
                                                info!(socket_id = socket_id, addr = target_addr, "Message timeout after {} ms: {:?} - {}", elapsed, msg, http_log);
                                                let _ = msg.return_error_to_sender(None, ServiceError::Timeout(service_name.clone(), self.http_timeout.as_millis() as u64));
                                                // Need to drop the connection because it's HTTP1
                                                close_socket!(self, proc, socket_id, rx_queue, service_name, Ok(self));
                                            },
                                        };
                                    }
                                }
                            } else {
                                tokio::select! {
                                    // Closed the socket
                                    Err(_) = &mut connection => {
                                        debug!(socket_id = socket_id, addr = target_addr, "Remote close the socket");
                                        close_socket!(self, proc, socket_id, rx_queue, service_name, Ok(self));
                                    }
                                    // Receive a message to send from the queue
                                    Some(msg) = rx_queue.recv() => {
                                        debug!(socket_id = socket_id, addr = target_addr, "HTTP client receive a message to send: {:?}", msg);
                                        match msg {
                                            InternalMsg::Request(mut msg) => {
                                                if let Some(data) = msg.take_data() {
                                                    match adaptor.process_srv_request(data, &self.target.url) {
                                                        Ok(http_request) => {
                                                            msg_to_send = Some((msg, http_request));
                                                            req_instant = Instant::now();
                                                        },
                                                        Err(e) => {
                                                            let _ = msg.return_error_to_sender(None, e);
                                                        },
                                                    }
                                                } else {
                                                    let _ = msg.return_error_to_sender(None, ServiceError::UnableToReachService(service_name.clone()));
                                                }
                                            },
                                            InternalMsg::Response(msg) => panic!(
                                                "The hyper client socket {}/{socket_id} receive a response {:?}",
                                                proc.get_proc_id(),
                                                msg
                                            ),
                                            InternalMsg::Error(err_msg) => panic!(
                                                "The hyper client socket {}/{socket_id} receive an error {:?}",
                                                proc.get_proc_id(),
                                                err_msg
                                            ),
                                            InternalMsg::Command(_) => todo!(),
                                            InternalMsg::Config => todo!(),
                                            InternalMsg::Service(_table) => {/* Will not use service table */},
                                            InternalMsg::Shutdown => {
                                                // Remove the socket queue and wait message to finish
                                                close_socket!(self, proc, socket_id, rx_queue, service_name, Ok(self));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(socket_id = socket_id, addr = target_addr, "HTTP1 handshake error: {}", e);
                        Err(HyperProcError::Hyper(e, target_addr))
                    }
                    Err(_) => {
                        warn!(
                            socket_id = socket_id,
                            addr = target_addr,
                            "HTTP1 handshake timeout after {} ms",
                            self.target.connect_timeout
                        );
                        Err(HyperProcError::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "HTTP1 handshake timeout after {} ms",
                                self.target.connect_timeout
                            ),
                        )))
                    }
                }
            }
        });
    }
}
