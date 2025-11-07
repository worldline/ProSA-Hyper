use std::{
    io,
    os::fd::AsRawFd,
    sync::Arc,
    time::{Duration, Instant},
};

use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use opentelemetry::{KeyValue, metrics::Histogram};
use prosa::{
    core::{
        adaptor::Adaptor,
        msg::{InternalMsg, Msg as _},
        proc::{ProcBusParam as _, ProcParam},
        service::ServiceError,
    },
    io::{stream::TargetSetting, url_is_ssl},
};
use prosa_utils::config::ssl::SslConfig;
use tokio::{task::JoinSet, time};
use tracing::{debug, warn};

use crate::{HyperProcError, client::adaptor::HyperClientAdaptor, hyper_version_str};

/// Hyper client socket
#[derive(Debug, Clone)]
pub struct HyperClientSocket {
    /// Target of the socket
    target: TargetSetting,
    /// Whether the socket is using HTTP/2
    is_http2: bool,
}

impl HyperClientSocket {
    pub fn new(mut target: TargetSetting) -> Self {
        // Set default protocol to HTTP2 if target enabled SSL
        if let Some(ssl) = target.ssl.as_mut() {
            ssl.set_alpn(vec!["h2".into(), "http/1.1".into()]);
        } else if url_is_ssl(&target.url) {
            let mut ssl = SslConfig::default();
            ssl.set_alpn(vec!["h2".into(), "http/1.1".into()]);
            target.ssl = Some(ssl);
        }

        HyperClientSocket {
            target,
            is_http2: false,
        }
    }

    /// Method to spawn a task to handle the Hyper client socket
    pub fn spawn<M, A>(
        mut self,
        join_set: &mut JoinSet<Result<Self, HyperProcError>>,
        proc: ProcParam<M>,
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
            + prosa_utils::msg::tvf::Tvf
            + std::default::Default,
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        join_set.spawn(async move {
            let io = TokioIo::new(self.target.connect().await?);
            let socket_id = io.inner().as_raw_fd();
            let target_addr = self.target.to_string();
            if io.inner().selected_alpn_check(|alpn| alpn == b"h2") {
                self.is_http2 = true;
                match time::timeout(
                    Duration::from_millis(self.target.connect_timeout),
                    http2::handshake(TokioExecutor::new(), io),
                )
                .await
                {
                    Ok(Ok((sender, mut connection))) => {
                        debug!(addr = target_addr, "Connected to HTTP2 remote");
                        let (tx_queue, mut rx_queue) = tokio::sync::mpsc::channel(2048);
                        proc.add_proc_queue(tx_queue, socket_id as u32).await?;
                        proc.add_service(vec![service_name.clone()], socket_id as u32)
                            .await?;

                        loop {
                            tokio::select! {
                                // Closed the socket
                                Err(_) = &mut connection => {
                                    debug!(addr = target_addr, "Remote close the socket");
                                    proc.remove_proc_queue(socket_id as u32).await?;
                                    return Ok(self);
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
                                                let message_histogram = message_histogram.clone();
                                                tokio::spawn(async move {
                                                    match adaptor.process_srv_request(data, &target_url) {
                                                        Ok(http_request) => {
                                                            let http_response = sender.send_request(http_request).await;

                                                            let (code, version) = http_response.as_ref().map_or((500, "HTTP/2"), |r| (r.status().as_u16() as i64, hyper_version_str(r.version())));
                                                            message_histogram.record(
                                                                req_instant.elapsed().as_millis() as u64,
                                                                &[
                                                                    KeyValue::new("code", code),
                                                                    KeyValue::new("version", version),
                                                                ],
                                                            );

                                                            match adaptor.process_http_response(http_response) {
                                                                Ok(response) => {
                                                                    msg.return_to_sender(response).await
                                                                },
                                                                Err(e) => msg.return_error_to_sender(None, e).await,
                                                            }
                                                        },
                                                        Err(e) => msg.return_error_to_sender(None, e).await,
                                                    }
                                                });
                                            } else {
                                                msg.return_error_to_sender(None, ServiceError::UnableToReachService(service_name.clone())).await?;
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
                                            proc.remove_proc_queue(socket_id as u32).await?;
                                            return Ok(self);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("HTTP2 handshake error: {}", e);
                        Err(HyperProcError::Hyper(e))
                    }
                    Err(_) => {
                        warn!(
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
                        debug!(addr = target_addr, "Connected to HTTP1 remote");
                        let (tx_queue, mut rx_queue) = tokio::sync::mpsc::channel(2048);
                        proc.add_proc_queue(tx_queue, socket_id as u32).await?;
                        proc.add_service(vec![service_name.clone()], socket_id as u32)
                            .await?;
                        let mut msg_to_send = None;
                        let mut request_to_send = None;
                        let mut req_instant = Instant::now();

                        loop {
                            tokio::select! {
                                // Closed the socket
                                Err(_) = &mut connection => {
                                    debug!(addr = target_addr, "Remote close the socket");
                                    proc.remove_proc_queue(socket_id as u32).await?;
                                    return Ok(self);
                                }
                                // Receive a message to send from the queue
                                Some(msg) = rx_queue.recv(), if request_to_send.is_none() => {
                                    match msg {
                                        InternalMsg::Request(mut msg) => {
                                            if let Some(data) = msg.take_data() {
                                                match adaptor.process_srv_request(data, &self.target.url) {
                                                    Ok(http_request) => {
                                                        msg_to_send = Some(msg);
                                                        request_to_send = Some(http_request);
                                                        req_instant = Instant::now();
                                                    },
                                                    Err(e) => {
                                                        msg.return_error_to_sender(None, e).await?;
                                                    },
                                                }
                                            } else {
                                                msg.return_error_to_sender(None, ServiceError::UnableToReachService(service_name.clone())).await?;
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
                                            proc.remove_proc_queue(socket_id as u32).await?;
                                            return Ok(self);
                                        }
                                    }
                                }
                                // Send an HTTP request
                                response = sender.send_request(request_to_send.take().unwrap()), if request_to_send.is_some() => {
                                    let (code, version) = response.as_ref().map_or((500, "HTTP/1"), |r| (r.status().as_u16() as i64, hyper_version_str(r.version())));
                                    message_histogram.record(
                                        req_instant.elapsed().as_millis() as u64,
                                        &[
                                            KeyValue::new("code", code),
                                            KeyValue::new("version", version),
                                        ],
                                    );

                                    let msg = msg_to_send.take().unwrap();
                                    match adaptor.process_http_response(response) {
                                        Ok(r) => {
                                            msg.return_to_sender(r).await?;
                                        },
                                        Err(e) => {
                                            msg.return_error_to_sender(None, e).await?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("HTTP1 handshake error: {}", e);
                        Err(HyperProcError::Hyper(e))
                    }
                    Err(_) => {
                        warn!(
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
