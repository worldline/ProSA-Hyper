use std::{sync::Arc, time::Duration};

use prosa::{
    core::{
        adaptor::Adaptor,
        error::ProcError,
        msg::InternalMsg,
        proc::{Proc, ProcBusParam as _, ProcConfig as _, ProcParam, proc, proc_settings},
    },
    io::stream::TargetSetting,
    tracing::{debug, info, warn},
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch},
    task::{self, JoinError, JoinSet},
};

use crate::{
    HyperProcError,
    client::{
        adaptor::HyperClientAdaptor,
        socket::{HyperClientSocket, SocketControl, SocketMeters},
    },
};

/// Hyper client processor settings
#[proc_settings]
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HyperClientSettings {
    /// Service name
    pub service_name: String,
    /// List of backend services
    pub backends: Vec<TargetSetting>,
    /// Number of socket connections per target, at least one
    #[serde(default = "HyperClientSettings::default_nb_socket")]
    nb_socket: u32,
    /// Timeout for HTTP messages in milliseconds
    #[serde(default = "HyperClientSettings::default_http_timeout")]
    http_timeout: u64,
    /// Delay before reconnecting a socket that just failed, in milliseconds.
    /// It doubles on every consecutive failure, up to `max_reconnect_delay`
    #[serde(default = "HyperClientSettings::default_reconnect_delay")]
    reconnect_delay: u64,
    /// Maximum delay between two reconnection attempts of a socket, in milliseconds
    #[serde(default = "HyperClientSettings::default_max_reconnect_delay")]
    max_reconnect_delay: u64,
}

impl HyperClientSettings {
    fn default_nb_socket() -> u32 {
        1
    }

    fn default_http_timeout() -> u64 {
        5000
    }

    fn default_reconnect_delay() -> u64 {
        500
    }

    fn default_max_reconnect_delay() -> u64 {
        30000
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

    /// Number of socket connections the processor keeps open per backend.
    ///
    /// Never zero, a client without socket can't serve anything, so a configuration asking for none
    /// is read as asking for one. Every reader goes through here, which is what keeps the processor
    /// and its sockets from disagreeing on how many sockets should exist
    pub(crate) fn nb_socket(&self) -> u32 {
        self.nb_socket.max(1)
    }

    /// Timeout applied to HTTP messages
    pub(crate) fn http_timeout(&self) -> Duration {
        Duration::from_millis(self.http_timeout)
    }

    /// Maximum delay between two reconnection attempts of a socket, never below the floor
    fn max_reconnect_delay(&self) -> Duration {
        Duration::from_millis(self.max_reconnect_delay).max(MIN_RECONNECT_DELAY)
    }

    /// Delay of the first reconnection attempt of a socket.
    ///
    /// Doubles as how long a connection that served nothing must have lasted to count as a working
    /// one, so a socket never reconnects faster than it would after a refused connection.
    ///
    /// Floored, because a zero would do both at once: no wait between two attempts, and every
    /// attempt counting as a working connection, so the retry count would never grow and a backend
    /// that refuses everything would be dialled at the speed of the machine
    pub(crate) fn base_reconnect_delay(&self) -> Duration {
        Duration::from_millis(self.reconnect_delay)
            .min(self.max_reconnect_delay())
            .max(MIN_RECONNECT_DELAY)
    }

    /// Delay to wait before the `retry`-th consecutive reconnection attempt of a socket.
    ///
    /// `retry` is zero for a socket that never failed, which connects without waiting
    pub(crate) fn reconnect_delay(&self, retry: u32) -> Duration {
        if retry == 0 {
            return Duration::ZERO;
        }

        self.base_reconnect_delay()
            .saturating_mul(1u32.checked_shl(retry - 1).unwrap_or(u32::MAX))
            .min(self.max_reconnect_delay())
    }

    /// Negotiate HTTP/2 first on every SSL backend, so a backend compares equal to the target of a
    /// socket that is already running it.
    ///
    /// [`TargetSetting::set_alpn`] is idempotent and does nothing on a plain backend
    pub(crate) fn normalize_backends(&mut self) {
        for backend in &mut self.backends {
            backend.set_alpn(vec!["h2".into(), "http/1.1".into()]);
        }
    }
}

#[proc_settings]
impl Default for HyperClientSettings {
    fn default() -> HyperClientSettings {
        HyperClientSettings {
            service_name: "hyper".to_string(),
            backends: Vec::new(),
            nb_socket: Self::default_nb_socket(),
            http_timeout: Self::default_http_timeout(),
            reconnect_delay: Self::default_reconnect_delay(),
            max_reconnect_delay: Self::default_max_reconnect_delay(),
        }
    }
}

/// Depth of the bus queue of a socket, as many messages as the processor's own queue
const SOCKET_QUEUE_SIZE: usize = 2048;

/// Shortest a socket ever waits between two connection attempts.
///
/// Short enough that a configured delay is used as written, and that the doubling still reaches
/// `max_reconnect_delay` in a dozen attempts, but not zero
const MIN_RECONNECT_DELAY: Duration = Duration::from_millis(10);

/// One socket the processor wants to keep open to a backend
#[derive(Debug)]
struct SocketHandle {
    /// Bus queue id of the socket, stable across its reconnections
    id: u32,
    /// Backend the socket connects to, and its position in the pool of that backend
    target: TargetSetting,
    index: u32,
    /// Service the socket advertises, so renaming it replaces the whole pool
    service_name: String,
    /// What the processor decides for the socket, out of band from the requests it serves
    control: watch::Sender<SocketControl>,
    /// Task currently running the socket, which is how a slot is found again from a join error
    task: task::Id,
}

impl SocketHandle {
    /// Method to know if two targets open the same connection.
    ///
    /// [`TargetSetting`] compares its `connect_timeout` too, which applies to the next attempt like
    /// the reconnection delays do. Comparing it here would retire every socket of a backend on a
    /// reload that only shortened it, which is the opposite of what it is for
    fn same_connection(target: &TargetSetting, other: &TargetSetting) -> bool {
        target.url == other.url && target.ssl() == other.ssl() && target.proxy == other.proxy
    }

    /// Give the backend of `settings` this slot connects to, if the settings still describe it.
    ///
    /// The one place that decides which sockets exist. The reconnection delays, the message timeout
    /// and the connection timeout aren't part of it: they apply to the next attempt and to the next
    /// request, so they reach the socket through its control channel instead
    fn matching_backend<'s>(&self, settings: &'s HyperClientSettings) -> Option<&'s TargetSetting> {
        if self.index >= settings.nb_socket() || self.service_name != settings.service_name {
            return None;
        }

        settings
            .backends
            .iter()
            .find(|backend| Self::same_connection(backend, &self.target))
    }
}

/// The sockets the processor keeps open to its backends, and the tasks running them
struct SocketPool<M>
where
    M: Sized + Clone + prosa::core::msg::Tvf,
{
    /// Running socket tasks. A task ends when its connection does, and the socket it gives back is
    /// started again for as long as its slot is still in [`Self::handles`]
    tasks: JoinSet<(HyperClientSocket<M>, Option<HyperProcError>)>,
    /// One per socket that should exist, which is the only thing saying a socket must come back
    handles: Vec<SocketHandle>,
    /// Slot ids are never reused, so a queue the bus hasn't removed yet can't be confused with the
    /// one of the socket that takes its place
    next_id: u32,
    /// Instruments shared by every socket
    meters: SocketMeters,
}

impl<M> SocketPool<M>
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
    /// Create an empty pool, which [`Self::align`] then fills from the settings
    fn new(meters: SocketMeters) -> Self {
        SocketPool {
            tasks: JoinSet::new(),
            handles: Vec::new(),
            next_id: 1,
            meters,
        }
    }

    /// Answer `true` while the pool still runs a socket task
    fn is_running(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Wait for the next socket task to end and hand its socket back
    #[allow(clippy::type_complexity)]
    async fn join_next(
        &mut self,
    ) -> Option<Result<(HyperClientSocket<M>, Option<HyperProcError>), JoinError>> {
        self.tasks.join_next().await
    }

    /// Bring the pool in line with `settings`.
    ///
    /// [`Self::handles`] holds exactly the sockets that should exist, so retiring one is dropping
    /// its handle after telling it to stop, and spawning one is opening its channels. Idempotent,
    /// which is what lets the same call serve the startup and the configuration reload
    fn align<A>(
        &mut self,
        settings: &HyperClientSettings,
        proc: &Arc<ProcParam<M>>,
        adaptor: &Arc<A>,
    ) where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        // Retire the sockets the settings don't describe anymore. They answer what they have in
        // flight and come back through the task set, where the processor finds no handle for them
        self.handles.retain(|handle| {
            if let Some(backend) = handle.matching_backend(settings) {
                handle.control.send_modify(|control| {
                    control.connect_timeout = backend.connect_timeout;
                    control.http_timeout = settings.http_timeout();
                });
                true
            } else {
                debug!(
                    "Retire the Hyper client socket {} on {}",
                    handle.id,
                    handle.target.get_safe_url()
                );
                handle.control.send_modify(|control| control.stopped = true);
                false
            }
        });

        // And spawn the ones that are missing. Matched the same way the sockets were kept, so a
        // backend that only changed its connection timeout doesn't get a second pool
        for target in &settings.backends {
            for index in 0..settings.nb_socket() {
                if !self.handles.iter().any(|handle| {
                    handle.index == index && SocketHandle::same_connection(&handle.target, target)
                }) {
                    self.spawn(target.clone(), index, settings, proc, adaptor);
                }
            }
        }
    }

    /// Open a slot on `target` and start the socket that serves it
    fn spawn<A>(
        &mut self,
        target: TargetSetting,
        index: u32,
        settings: &HyperClientSettings,
        proc: &Arc<ProcParam<M>>,
        adaptor: &Arc<A>,
    ) where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        let (tx_queue, rx_queue) = mpsc::channel(SOCKET_QUEUE_SIZE);
        let (control_tx, control_rx) =
            SocketControl::channel(target.connect_timeout, settings.http_timeout());
        let id = self.next_id;
        self.next_id += 1;

        let task = HyperClientSocket::new(
            id,
            target.clone(),
            settings.service_name.clone(),
            control_rx,
            rx_queue,
            tx_queue,
        )
        .spawn(
            &mut self.tasks,
            proc.clone(),
            adaptor.clone(),
            settings,
            self.meters.clone(),
        );

        self.handles.push(SocketHandle {
            id,
            target,
            index,
            service_name: settings.service_name.clone(),
            control: control_tx,
            task,
        });
    }

    /// Take back a socket task that just ended, and start its slot again if it is still open
    fn take_back<A>(
        &mut self,
        ended: Result<(HyperClientSocket<M>, Option<HyperProcError>), JoinError>,
        settings: &HyperClientSettings,
        proc: &Arc<ProcParam<M>>,
        adaptor: &Arc<A>,
    ) where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        let socket = match ended {
            Ok((socket, None)) => socket,
            Ok((socket, Some(error))) => {
                warn!("A Hyper client socket task ended with error: {error}");
                socket
            }
            // A task that panicked can't hand its socket back, and the queue it held went with it.
            // Taking the processor down over it would drop the queues of every other socket too,
            // so the slot is closed here and opened again below, with a socket of its own
            Err(error) => {
                warn!("A Hyper client socket task panicked, closing its slot: {error}");
                self.handles.retain(|handle| handle.task != error.id());
                // Opened again unless ProSA is on its way out, where `handles` is already empty and
                // realigning would connect the whole pool back to the backends
                if !proc.is_stopping() {
                    self.align(settings, proc, adaptor);
                }
                return;
            }
        };

        // The bus tells the sockets and the processor to stop at the same time, so a socket can end
        // before the processor has read its own `Shutdown`. Restarting it then would open a
        // connection to the backend in the middle of the shutdown, so the stop flag is asked first:
        // ProSA raises it before it sends any of those messages
        let slot = self
            .handles
            .iter()
            .position(|handle| handle.id == socket.id());
        if let Some(slot) = slot
            && !proc.is_stopping()
        {
            debug!("A Hyper client socket has ended, restarting a new one: {socket:?}");
            self.handles[slot].task = socket.spawn(
                &mut self.tasks,
                proc.clone(),
                adaptor.clone(),
                settings,
                self.meters.clone(),
            );
        } else {
            debug!("A Hyper client socket has been retired: {socket:?}");
            socket.retire();
        }
    }

    /// Tell every socket to stop and wait for them to answer what they have in flight.
    ///
    /// The main task only reaches the sockets that are connected. Telling them here reaches the
    /// ones that are only waiting to reconnect too, and waiting rather than dropping the task set
    /// is what keeps a socket from being aborted with a request it never answered. Everything a
    /// retired socket still does is bounded by the message timeout, so this cannot outlast one, and
    /// bounding it again here would only cut a drain short of the budget it was given
    async fn shutdown(&mut self) {
        for handle in &self.handles {
            handle.control.send_modify(|control| control.stopped = true);
        }
        self.handles.clear();

        while let Some(socket) = self.tasks.join_next().await {
            match socket {
                Ok((socket, _)) => socket.retire(),
                Err(error) => warn!("A Hyper client socket task panicked while stopping: {error}"),
            }
        }
    }
}

/// Hyper client processor
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

        // The sockets the processor keeps open, with the meters they all report to
        let mut pool = SocketPool::new(SocketMeters::new(
            &self.get_proc_param().meter("hyper_client"),
        ));

        // Create client sockets
        if self.settings.backends.is_empty() {
            return Err(Box::new(HyperProcError::Other(
                "No backend configured for the Hyper client processor".to_string(),
            )));
        }

        self.settings.normalize_backends();
        pool.align(&self.settings, &self.proc, &adaptor);

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
                        InternalMsg::Service(table) => self.service = table,
                        InternalMsg::Config(config) => {
                            // Read here and only here, so `Adaptor::reload_config` runs once. What
                            // came out of it reaches the sockets through their control channel,
                            // which gets to one that is busy serving where its bus queue wouldn't
                            if let Some(mut settings) = config.reload_proc::<HyperClientSettings>(self.proc.as_ref(), adaptor.as_ref()) {
                                // Keep the current configuration, a client without backend can't
                                // serve anything
                                if settings.backends.is_empty() {
                                    warn!("Ignoring the configuration reload of {}: no backend configured", self.name());
                                    continue;
                                }

                                info!("Reload the configuration of the Hyper client processor {}", self.name());

                                settings.normalize_backends();
                                self.settings = settings;
                                pool.align(&self.settings, &self.proc, &adaptor);
                            }
                        }
                        InternalMsg::Shutdown => {
                            // Wait for poll shutdown
                            pool.shutdown().await;

                            // Terminated once the sockets are done, the requests they were finishing use it
                            adaptor.terminate();
                            self.proc.remove_proc(None).await?;
                            warn!("The Hyper client processor will shut down");
                            return Ok(());
                        }
                    }
                },
                Some(socket) = pool.join_next(), if pool.is_running() => {
                    // The socket is given back whatever ended it, so a failed connection is retried
                    // instead of shrinking the pool until the next configuration reload
                    pool.take_back(socket, &self.settings, &self.proc, &adaptor);
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(url: &str) -> TargetSetting {
        TargetSetting::new(
            url.parse().expect("Backend URL should be valid"),
            None,
            None,
        )
    }

    #[test]
    fn only_the_connection_decides_which_sockets_exist() {
        let target = backend("http://backend01:8080");

        // A reload that only retimes the connection keeps the socket, where comparing the whole
        // target would retire it
        let mut retimed = target.clone();
        retimed.connect_timeout += 1000;
        assert_ne!(target, retimed);
        assert!(SocketHandle::same_connection(&target, &retimed));

        // Anything that changes where or how the socket connects is a different socket
        assert!(!SocketHandle::same_connection(
            &target,
            &backend("http://backend02:8080")
        ));

        let mut proxied = target.clone();
        proxied.proxy = Some(
            "http://proxy:3128"
                .parse()
                .expect("Proxy URL should be valid"),
        );
        assert!(!SocketHandle::same_connection(&target, &proxied));

        let mut secured = target.clone();
        secured.set_alpn(vec!["h2".into()]);
        secured.set_ssl(Some(Default::default()));
        assert!(!SocketHandle::same_connection(&target, &secured));
    }

    #[test]
    fn reconnect_delay_backoff() {
        let settings = HyperClientSettings::new("hyper".into());

        // A socket that never failed reconnects right away
        assert_eq!(Duration::ZERO, settings.reconnect_delay(0));

        // Then the delay doubles on every consecutive failure, up to the maximum
        assert_eq!(Duration::from_millis(500), settings.reconnect_delay(1));
        assert_eq!(Duration::from_millis(1000), settings.reconnect_delay(2));
        assert_eq!(Duration::from_millis(2000), settings.reconnect_delay(3));
        assert_eq!(Duration::from_secs(30), settings.reconnect_delay(7));

        // A backend that stays down for a long time must not overflow the delay
        assert_eq!(Duration::from_secs(30), settings.reconnect_delay(u32::MAX));
    }

    #[test]
    fn reconnect_delay_below_first_attempt() {
        // A maximum lower than the first delay caps it, instead of reconnecting slower than the
        // maximum on the first attempt
        let settings: HyperClientSettings = config::Config::builder()
            .set_override("reconnect_delay", 5000)
            .expect("Reconnect delay override should be valid")
            .set_override("max_reconnect_delay", 1000)
            .expect("Maximum reconnect delay override should be valid")
            .set_override("service_name", "hyper")
            .expect("Service name override should be valid")
            .set_override("backends", Vec::<String>::new())
            .expect("Backends override should be valid")
            .build()
            .expect("Configuration should be valid")
            .try_deserialize()
            .expect("Hyper client settings should be valid");

        // Asserted on the base delay too: `reconnect_delay` caps against the maximum on its way
        // out, so it would read the same whether the base was clamped or not
        assert_eq!(Duration::from_secs(1), settings.base_reconnect_delay());
        assert_eq!(Duration::from_secs(1), settings.reconnect_delay(1));
        assert_eq!(Duration::from_secs(1), settings.reconnect_delay(9));
    }

    #[test]
    fn reconnect_delay_is_never_zero() {
        // A socket that waited nothing and counted every attempt as a working connection would dial
        // a backend that refuses everything at the speed of the machine
        let settings: HyperClientSettings = config::Config::builder()
            .set_override("reconnect_delay", 0)
            .expect("Reconnect delay override should be valid")
            .set_override("max_reconnect_delay", 0)
            .expect("Maximum reconnect delay override should be valid")
            .set_override("service_name", "hyper")
            .expect("Service name override should be valid")
            .set_override("backends", Vec::<String>::new())
            .expect("Backends override should be valid")
            .build()
            .expect("Configuration should be valid")
            .try_deserialize()
            .expect("Hyper client settings should be valid");

        assert!(!settings.base_reconnect_delay().is_zero());
        assert!(!settings.reconnect_delay(1).is_zero());
        assert!(!settings.reconnect_delay(u32::MAX).is_zero());

        // The first attempt of a socket that never failed still doesn't wait
        assert_eq!(Duration::ZERO, settings.reconnect_delay(0));
    }
}
