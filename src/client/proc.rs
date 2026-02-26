use std::sync::Arc;

use opentelemetry::KeyValue;
use prosa::{
    core::{
        adaptor::Adaptor,
        error::ProcError,
        msg::InternalMsg,
        proc::{Proc, ProcBusParam as _, ProcConfig as _, proc, proc_settings},
    },
    io::stream::TargetSetting,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::{
    HyperProcError,
    client::{adaptor::HyperClientAdaptor, socket::HyperClientSocket},
};

/// Hyper client processor settings
#[proc_settings]
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HyperClientSettings {
    /// Service name
    pub service_name: String,
    /// List of backend services
    pub backends: Vec<TargetSetting>,
    /// Minimum number of socket connections per target
    #[serde(default = "HyperClientSettings::default_min_socket")]
    min_socket: u32,
    /// Maximum number of socket connections per target
    #[serde(default = "HyperClientSettings::default_max_socket")]
    max_socket: u32,
    /// Timeout for HTTP messages in milliseconds
    #[serde(default = "HyperClientSettings::default_http_timeout")]
    http_timeout: u64,
}

impl HyperClientSettings {
    fn default_min_socket() -> u32 {
        1
    }

    fn default_max_socket() -> u32 {
        20
    }

    fn default_http_timeout() -> u64 {
        5000
    }

    /// Create a new Hyper client settings listenning to a service
    pub fn new(service_name: String) -> Self {
        HyperClientSettings {
            service_name,
            ..Default::default()
        }
    }

    /// Add a new Hyper client backend
    pub fn add_backend(&mut self, target: TargetSetting) {
        self.backends.push(target);
    }
}

#[proc_settings]
impl Default for HyperClientSettings {
    fn default() -> HyperClientSettings {
        HyperClientSettings {
            service_name: "hyper".to_string(),
            backends: Vec::new(),
            min_socket: Self::default_min_socket(),
            max_socket: Self::default_max_socket(),
            http_timeout: Self::default_http_timeout(),
        }
    }
}

/// Hyper server processor
#[proc(settings = HyperClientSettings)]
pub struct HyperClientProc {}

#[proc]
impl<M, A> Proc<A> for HyperClientProc
where
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
    /// Main loop of the processor
    async fn internal_run(&mut self) -> Result<(), Box<dyn ProcError + Send + Sync>> {
        // Initiate an adaptor for the Hyper client processor
        let adaptor = Arc::new(A::new(self)?);

        // Add proc main queue (id: 0)
        self.proc.add_proc().await?;

        // List of client sockets tasks
        let mut client_sockets = JoinSet::new();

        // Meter to log HTTP requests
        let meter = self.get_proc_param().meter("hyper_client");
        let observable_http_histogram = meter
            .u64_histogram("prosa_hyper_cli_duration")
            .with_description("Hyper HTTP client request duration histogram")
            .build();
        let observable_http_socket = meter
            .u64_gauge("prosa_hyper_cli_socket")
            .with_description("Hyper HTTP client socket counter")
            .build();

        // Create client sockets
        if !self.settings.backends.is_empty() {
            for backend in &self.settings.backends {
                for _ in 0..self.settings.min_socket {
                    let client_socket =
                        HyperClientSocket::new(backend.clone(), self.settings.http_timeout);
                    client_socket.spawn(
                        &mut client_sockets,
                        self.proc.clone(),
                        adaptor.clone(),
                        self.settings.service_name.clone(),
                        observable_http_histogram.clone(),
                    );
                }
            }
        } else {
            return Err(Box::new(HyperProcError::Other(
                "No backend configured for the Hyper client processor".to_string(),
            )));
        }

        // Update socket number after all creations
        observable_http_socket.record(
            client_sockets.len() as u64,
            &[
                KeyValue::new("proc", self.name().to_string()),
                KeyValue::new("service", self.settings.service_name.clone()),
            ],
        );

        loop {
            tokio::select! {
                Some(msg) = self.internal_rx_queue.recv() => {
                    match msg {
                        InternalMsg::Request(msg) => panic!(
                            "The hyper client processor[0] {} receive a request {:?}",
                            self.get_proc_id(),
                            msg
                        ),
                        InternalMsg::Response(msg) => panic!(
                            "The hyper client processor[0] {} receive a response {:?}",
                            self.get_proc_id(),
                            msg
                        ),
                        InternalMsg::Error(err_msg) => panic!(
                            "The hyper client processor[0] {} receive an error {:?}",
                            self.get_proc_id(),
                            err_msg
                        ),
                        InternalMsg::Command(_) => todo!(),
                        InternalMsg::Config => todo!(),
                        InternalMsg::Service(table) => self.service = table,
                        InternalMsg::Shutdown => {
                            adaptor.terminate();
                            self.proc.remove_proc(None).await?;
                            warn!("The Hyper client processor will shut down");
                            return Ok(());
                        }
                    }
                },
                Some(socket) = client_sockets.join_next(), if !client_sockets.is_empty() => {
                    match socket.map_err(|e| HyperProcError::Other(format!("Hyper client socket task join error: {}", e)))? {
                        Ok(s) => {
                            info!("A Hyper client socket has ended, restarting a new one: {s:?}");
                            s.spawn(
                                &mut client_sockets,
                                self.proc.clone(),
                                adaptor.clone(),
                                self.settings.service_name.clone(),
                                observable_http_histogram.clone()
                            );
                        },
                        Err(e) =>  {
                            warn!("A Hyper client socket task ended with error: {}", e);
                        }
                    }

                    observable_http_socket.record(
                        client_sockets.len() as u64,
                        &[
                            KeyValue::new("proc", self.name().to_string()),
                            KeyValue::new("service", self.settings.service_name.clone()),
                        ],
                    );
                },
            }
        }
    }
}
