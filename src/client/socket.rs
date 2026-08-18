//! Hyper client sockets
//!
//! A socket decides nothing. The processor tells it which backend to connect to and when to stop,
//! it serves what its bus queue brings until the connection ends, then hands itself back.
//!
//! Two channels reach a socket, and they carry different things. The bus queue carries the
//! requests, and it is as deep as the traffic the socket is behind on, so anything put there is
//! only read once the socket caught up. What the processor decides — the message timeout, and
//! whether the socket should stop — goes through a [`watch`] channel instead, which is always
//! current, reaches a socket that is busy serving, and never blocks the processor's loop.

use std::{
    convert::Infallible,
    io,
    ops::ControlFlow,
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::{
    Request, Response,
    body::Incoming,
    client::conn::{http1, http2},
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use prosa::{
    core::{
        adaptor::Adaptor,
        msg::{InternalMsg, Msg as _, RequestMsg},
        proc::{ProcBusParam as _, ProcParam},
        service::ServiceError,
    },
    io::stream::{Stream, TargetSetting},
    otel::{
        KeyValue,
        metrics::{Histogram, UpDownCounter},
    },
    tracing::{debug, info, warn},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::{self, timeout},
};

use crate::{
    H2, HyperProcError,
    client::{adaptor::HyperClientAdaptor, proc::HyperClientSettings},
    hyper_version_str,
};

/// Type alias for HTTP request pair to reduce type complexity
type HttpRequestPair<M> = (RequestMsg<M>, Request<BoxBody<Bytes, Infallible>>);

/// Instruments reported by the Hyper client sockets
#[derive(Debug, Clone)]
pub(super) struct SocketMeters {
    /// Duration of the HTTP messages
    message_histogram: Histogram<u64>,
    /// Number of connected sockets
    socket_counter: UpDownCounter<i64>,
}

impl SocketMeters {
    /// Create the instruments of the Hyper client sockets from the processor meter
    pub(super) fn new(meter: &prosa::otel::metrics::Meter) -> Self {
        SocketMeters {
            message_histogram: meter
                .u64_histogram("prosa_hyper_cli_duration")
                .with_description("Hyper HTTP client request duration histogram")
                .build(),
            socket_counter: meter
                .i64_up_down_counter("prosa_hyper_cli_socket")
                .with_description("Hyper HTTP client connected socket counter")
                .build(),
        }
    }
}

/// What the processor decides for one of its sockets, out of band from the requests it serves.
///
/// Everything that applies to the next attempt or the next message, so changing it doesn't retire a
/// healthy socket. What defines the connection itself isn't here: a socket that has to reach
/// somewhere else is a different socket
#[derive(Debug)]
pub(super) struct SocketControl {
    /// Timeout applied to the next connection attempt, in milliseconds
    pub(super) connect_timeout: u64,
    /// Timeout applied to the next HTTP message
    pub(super) http_timeout: Duration,
    /// Set once the processor retires the socket
    pub(super) stopped: bool,
}

impl SocketControl {
    /// Open the control channel of a socket the processor is about to spawn
    pub(super) fn channel(
        connect_timeout: u64,
        http_timeout: Duration,
    ) -> (watch::Sender<Self>, watch::Receiver<Self>) {
        watch::channel(SocketControl {
            connect_timeout,
            http_timeout,
            stopped: false,
        })
    }
}

/// Report the duration of a message under the backend it was sent to.
///
/// The target is formatted rather than taken from the URL, so a backend configured with credentials
/// doesn't put them in a metric attribute, and both protocols report under the same value
fn record_message(
    message_histogram: &Histogram<u64>,
    target_addr: &str,
    response: &Result<Response<Incoming>, hyper::Error>,
    started: Instant,
    default_version: &'static str,
) {
    let (code, version) = response.as_ref().map_or((500, default_version), |r| {
        (r.status().as_u16() as i64, hyper_version_str(r.version()))
    });

    message_histogram.record(
        started.elapsed().as_millis() as u64,
        &[
            KeyValue::new("target", target_addr.to_string()),
            KeyValue::new("code", code),
            KeyValue::new("version", version),
        ],
    );
}

/// Hyper client socket
///
/// A socket connects to its backend, serves what its bus queue brings until the connection ends or
/// it is asked to stop, then hands itself back. Which sockets must exist is the processor's call,
/// and the way it retires one is [`SocketControl::stopped`].
#[derive(Debug)]
pub(crate) struct HyperClientSocket<M>
where
    M: Sized + Clone + prosa::core::msg::Tvf,
{
    /// Bus queue id of the socket, stable across its reconnections
    id: u32,
    /// Target of the socket
    target: TargetSetting,
    /// Service the socket advertises on its bus queue
    service_name: String,
    /// Number of consecutive failed connection attempts, delaying the next reconnection
    retry: u32,
    /// What the processor decides for the socket, always current
    control: watch::Receiver<SocketControl>,
    /// Queue the bus brings the requests to.
    ///
    /// Kept across reconnections, so a request that arrives while the socket is down is served by
    /// its next connection instead of landing in a receiver nobody holds anymore
    rx_queue: mpsc::Receiver<InternalMsg<M>>,
    /// Sending end of [`Self::rx_queue`], declared to the bus while the socket is connected
    tx_queue: mpsc::Sender<InternalMsg<M>>,
}

impl<M> HyperClientSocket<M>
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
    /// Create a socket on `target`, which must have been normalized by
    /// [`HyperClientSettings::normalize_backends`] so it compares equal to the configured backend
    pub(super) fn new(
        id: u32,
        target: TargetSetting,
        service_name: String,
        control: watch::Receiver<SocketControl>,
        rx_queue: mpsc::Receiver<InternalMsg<M>>,
        tx_queue: mpsc::Sender<InternalMsg<M>>,
    ) -> Self {
        HyperClientSocket {
            id,
            target,
            service_name,
            retry: 0,
            control,
            rx_queue,
            tx_queue,
        }
    }

    /// Bus queue id of the socket, which is the slot it occupies in the pool of the processor
    pub(super) fn id(&self) -> u32 {
        self.id
    }

    /// Timeout the processor currently applies to an HTTP message
    fn http_timeout(&self) -> Duration {
        self.control.borrow().http_timeout
    }

    /// Answer `true` once the processor retired the socket.
    ///
    /// Reads without marking the value seen, so it never consumes the wake-up [`Self::stopped`] is
    /// waiting for
    fn is_stopped(&self) -> bool {
        self.control.borrow().stopped
    }

    /// Resolve once the processor retires the socket, and never otherwise.
    ///
    /// A dropped sender counts as retired: the processor only ever lets a handle go after it set
    /// [`SocketControl::stopped`], and a closed channel makes `changed` return instantly forever,
    /// which as a `select!` arm would spin the socket rather than stop it
    async fn stopped(control: &mut watch::Receiver<SocketControl>) {
        while control.changed().await.is_ok() {
            if control.borrow().stopped {
                return;
            }
        }
    }

    /// Answer whatever still reaches a socket that won't come back, until nobody can reach it.
    ///
    /// The bus is told to remove the queue, but a processor that hasn't received the new service
    /// table yet still holds it and still sends to it. Closing it there turns a request that should
    /// have come back `UnableToReachService` into a send error for its sender, and a processor that
    /// treats that as fatal restarts on it. So the queue outlives the socket, answering rather than
    /// closing, and only goes away once the last sender did.
    pub(super) fn retire(self) {
        let HyperClientSocket {
            service_name,
            mut rx_queue,
            tx_queue,
            ..
        } = self;

        // The socket holds a sending end of its own queue, which would keep it open forever
        drop(tx_queue);

        tokio::spawn(async move {
            while let Some(msg) = rx_queue.recv().await {
                if let InternalMsg::Request(req_msg) = msg {
                    let _ = req_msg.return_error_to_sender(
                        None,
                        ServiceError::UnableToReachService(service_name.clone()),
                    );
                }
            }
        });
    }

    /// Declare the socket queue and the service it serves to the bus
    async fn declare_socket_queue(&self, proc: &ProcParam<M>) -> Result<(), HyperProcError> {
        proc.add_proc_queue(self.tx_queue.clone(), self.id).await?;
        proc.add_service(vec![self.service_name.clone()], self.id)
            .await?;
        Ok(())
    }

    /// Take the socket queue back off the bus, which also takes the service it advertised.
    ///
    /// Reported rather than propagated: a bus that can't be told is no reason to abandon the
    /// requests the socket still has to answer, and it is the ordinary case while ProSA stops,
    /// where the main task is already gone
    async fn withdraw_socket_queue(&self, proc: &ProcParam<M>) {
        if let Err(e) = proc.remove_proc_queue(self.id).await {
            debug!(
                socket_id = self.id,
                addr = %self.target,
                "Can't remove the socket queue from the bus: {e}"
            );
        }
    }

    /// Helper to process a service request into an HTTP request
    fn process_request<A>(
        &self,
        adaptor: &Arc<A>,
        mut msg: RequestMsg<M>,
    ) -> Option<HttpRequestPair<M>>
    where
        A: 'static + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        if let Some(data) = msg.take_data() {
            match adaptor.process_srv_request(data, &self.target.url) {
                Ok(http_request) => Some((msg, http_request)),
                Err(e) => {
                    let _ = msg.return_error_to_sender(None, e);
                    None
                }
            }
        } else {
            let _ = msg.return_error_to_sender(
                None,
                ServiceError::UnableToReachService(self.service_name.clone()),
            );
            None
        }
    }

    /// Turn a handshake attempt into the connected pair it yields, reporting what went wrong
    fn handshake_result<T>(
        &self,
        result: Result<Result<T, hyper::Error>, time::error::Elapsed>,
        protocol: &str,
    ) -> Result<T, HyperProcError> {
        match result {
            Ok(Ok(connected)) => Ok(connected),
            Ok(Err(e)) => {
                warn!(
                    socket_id = self.id,
                    addr = %self.target,
                    "{protocol} handshake error: {e}"
                );
                Err(HyperProcError::Hyper(e, self.target.to_string()))
            }
            Err(_) => {
                let connect_timeout = self.target.connect_timeout;
                warn!(
                    socket_id = self.id,
                    addr = %self.target,
                    "{protocol} handshake timeout after {connect_timeout} ms"
                );
                Err(HyperProcError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{protocol} handshake timeout after {connect_timeout} ms"),
                )))
            }
        }
    }

    /// Send one HTTP/1.1 request and answer its sender, telling whether the socket can carry on and
    /// whether the backend answered at all.
    ///
    /// The message timeout bounds the whole exchange, body included, because a response that is
    /// only half read leaves the connection where the next one can't be correlated. That is also
    /// why a timeout ends the socket instead of only failing the request
    async fn exchange<A>(
        &self,
        sender: &mut http1::SendRequest<BoxBody<Bytes, Infallible>>,
        msg: RequestMsg<M>,
        request: Request<BoxBody<Bytes, Infallible>>,
        adaptor: &Arc<A>,
        message_histogram: &Histogram<u64>,
    ) -> ControlFlow<(), bool>
    where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        let http_timeout = self.http_timeout();
        let target_addr = self.target.to_string();
        let http_log = request.uri().to_string();
        let started = Instant::now();

        let exchanged = timeout(http_timeout, async {
            // Hyper takes the request the moment it is handed over and refuses it until the
            // connection reported the previous one done, so readiness is asked for, not assumed
            let response = match sender.ready().await {
                Ok(()) => sender.send_request(request).await,
                Err(e) => Err(e),
            };

            // Whether the backend answered, which is not the same as the adaptor accepting what it
            // answered: a rejected payload still came over a connection that works
            let answered = response.is_ok();

            record_message(
                message_histogram,
                &target_addr,
                &response,
                started,
                "HTTP/1.1",
            );

            (answered, adaptor.process_http_response(response).await)
        })
        .await;

        match exchanged {
            Ok((answered, Ok(response))) => {
                let _ = msg.return_to_sender(response);
                ControlFlow::Continue(answered)
            }
            Ok((answered, Err(e))) => {
                let _ = msg.return_error_to_sender(None, e);
                ControlFlow::Continue(answered)
            }
            Err(_) => {
                info!(
                    socket_id = self.id,
                    addr = target_addr,
                    "Message timeout after {} ms: {:?} - {}",
                    http_timeout.as_millis(),
                    msg,
                    http_log
                );
                let _ = msg.return_error_to_sender(
                    None,
                    ServiceError::Timeout(
                        self.service_name.clone(),
                        http_timeout.as_millis() as u64,
                    ),
                );
                ControlFlow::Break(())
            }
        }
    }

    /// Serve the Hyper client socket with HTTP/1.1 until its connection ends, answering whether it
    /// managed to serve anything on it
    async fn serve_http1<A>(
        &mut self,
        io: TokioIo<Stream>,
        proc: &ProcParam<M>,
        adaptor: &Arc<A>,
        message_histogram: &Histogram<u64>,
    ) -> Result<bool, HyperProcError>
    where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        let fd = io.inner().as_raw_fd();
        let connect_timeout = Duration::from_millis(self.target.connect_timeout);

        // Raced against the shutdown like the connect that produced the stream: it has a budget of
        // its own, and a socket that has not finished shaking hands holds nothing anyone waits on
        let handshake = tokio::select! {
            handshake = time::timeout(connect_timeout, http1::handshake(io)) => handshake,
            _ = Self::stopped(&mut self.control) => return Ok(true),
        };
        let (mut sender, connection) = self.handshake_result(handshake, "HTTP1")?;

        // The connection runs in its own task, which is what lets the loop below await a response
        // body: Hyper only hands over the bytes it has read, and it only reads while the connection
        // is polled. Driving it from the loop would stall any body that doesn't arrive with the head
        let mut connection = tokio::task::spawn(connection);

        debug!(
            socket_id = self.id,
            fd = fd,
            addr = %self.target,
            "Connected to HTTP1 remote, expose the service {}",
            self.service_name
        );
        self.declare_socket_queue(proc).await?;

        // Whether the backend answered anything on this connection, which is what tells one that is
        // merely slow from one that accepts and closes without ever serving
        let mut served = false;

        loop {
            tokio::select! {
                // Closed the socket
                closed = &mut connection => {
                    debug!(socket_id = self.id, addr = %self.target, "Remote HTTP1 close the socket: {closed:?}");
                    break;
                }
                // The processor retired the socket
                _ = Self::stopped(&mut self.control) => break,
                // Receive a message to send from the queue. A retired socket stops taking them, so
                // the exchange it is in the middle of is the last one it serves: the arms above
                // are only reached between two of them, never during one
                Some(msg) = self.rx_queue.recv(), if !self.is_stopped() => {
                    debug!(socket_id = self.id, addr = %self.target, "HTTP client receive a message to send: {msg:?}");
                    match msg {
                        InternalMsg::Request(req_msg) => {
                            let Some((msg, request)) = self.process_request(adaptor, req_msg) else {
                                continue;
                            };

                            // HTTP/1.1 correlates by order, so the exchange is awaited here and
                            // nothing else goes out until it is answered
                            match self.exchange(&mut sender, msg, request, adaptor, message_histogram).await {
                                ControlFlow::Continue(answered) => served |= answered,
                                ControlFlow::Break(()) => break,
                            }
                        },
                        InternalMsg::Response(msg) => panic!(
                            "The HTTP1 hyper client socket {}/{} receive a response {:?}",
                            proc.get_proc_id(),
                            self.id,
                            msg
                        ),
                        InternalMsg::Error(err_msg) => panic!(
                            "The HTTP1 hyper client socket {}/{} receive an error {:?}",
                            proc.get_proc_id(),
                            self.id,
                            err_msg
                        ),
                        // The processor is the one that reads the service table and the
                        // configuration, and tells its sockets what came out of it
                        InternalMsg::Service(_) | InternalMsg::Config(_) => {},
                        // The main task shutting ProSA down
                        InternalMsg::Shutdown => break,
                    }
                }
            }
        }

        // What is left in the queue is served by the next connection of the socket, or answered by
        // the processor if it doesn't restart it
        self.withdraw_socket_queue(proc).await;

        Ok(served)
    }

    /// Let the requests already sent on an HTTP/2 socket finish before the socket goes away.
    ///
    /// They run in their own task and hold a sender of the connection, which keeps its task alive
    /// for as long as they need it, so waiting is all there is to do. Bounded by the message
    /// timeout, past which they are detached rather than aborted, because an aborted task never
    /// answers the sender of the request it carries
    async fn drain_requests(&self, requests: &mut JoinSet<()>) {
        if requests.is_empty() {
            return;
        }

        debug!(
            socket_id = self.id,
            addr = %self.target,
            "Wait for {} in flight request(s) before closing the socket",
            requests.len()
        );

        let drained = timeout(self.http_timeout(), async {
            while let Some(request) = requests.join_next().await {
                if let Err(e) = request {
                    warn!(
                        socket_id = self.id,
                        addr = %self.target,
                        "An HTTP2 request task panicked, its sender won't be answered: {e}"
                    );
                }
            }
        })
        .await;

        if drained.is_err() {
            warn!(
                socket_id = self.id,
                addr = %self.target,
                "Close the socket with {} request(s) still in flight",
                requests.len()
            );

            requests.detach_all();
        }
    }

    /// Serve the Hyper client socket with HTTP/2 until its connection ends, answering whether it
    /// managed to serve anything on it
    async fn serve_h2<A>(
        &mut self,
        io: TokioIo<Stream>,
        proc: &ProcParam<M>,
        adaptor: &Arc<A>,
        message_histogram: &Histogram<u64>,
    ) -> Result<bool, HyperProcError>
    where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        let fd = io.inner().as_raw_fd();
        let connect_timeout = Duration::from_millis(self.target.connect_timeout);

        // Raced against the shutdown like the connect that produced the stream: it has a budget of
        // its own, and a socket that has not finished shaking hands holds nothing anyone waits on
        let handshake = tokio::select! {
            handshake = time::timeout(connect_timeout, http2::handshake(TokioExecutor::new(), io)) => handshake,
            _ = Self::stopped(&mut self.control) => return Ok(true),
        };
        let (sender, connection) = self.handshake_result(handshake, "HTTP2")?;

        // The connection runs in its own task, so it keeps serving the requests below, which read
        // their response body long after the loop moved on, and outlives this method for the ones
        // that are still draining
        let mut connection = tokio::task::spawn(connection);

        debug!(
            socket_id = self.id,
            fd = fd,
            addr = %self.target,
            "Connected to HTTP2 remote, expose the service {}",
            self.service_name
        );
        self.declare_socket_queue(proc).await?;

        // Requests are multiplexed on the connection, each one running in its own task. Tracked so
        // they can be drained when the socket closes
        let mut requests = JoinSet::new();

        // Whether the backend answered anything on this connection, which is what tells one that is
        // merely slow from one that accepts and closes without ever serving. Shared with the
        // request tasks, so an answer counts as long as its task was joined: one detached by
        // `drain_requests` stores it after this has been read, and is lost. That only happens after
        // a whole message timeout of waiting, by which point the connection lasted long enough to
        // count on its own
        let served = Arc::new(AtomicBool::new(false));

        loop {
            tokio::select! {
                // Closed the socket
                closed = &mut connection => {
                    debug!(socket_id = self.id, addr = %self.target, "Remote HTTP2 close the socket: {closed:?}");
                    break;
                }
                // The processor retired the socket
                _ = Self::stopped(&mut self.control) => break,
                // Reap the requests that are done, so the set doesn't grow with the socket. A task
                // that panicked took the request it held with it, which nothing can answer anymore,
                // so the least it can do is not be silent about it
                Some(request) = requests.join_next(), if !requests.is_empty() => {
                    if let Err(e) = request {
                        warn!(socket_id = self.id, addr = %self.target, "An HTTP2 request task panicked, its sender won't be answered: {e}");
                    }
                },
                // Receive a message to send from the queue. A retired socket stops taking them and
                // drains the ones it multiplexed rather than starting another
                Some(msg) = self.rx_queue.recv(), if !self.is_stopped() => {
                    match msg {
                        InternalMsg::Request(mut req_msg) => {
                            let Some(data) = req_msg.take_data() else {
                                let _ = req_msg.return_error_to_sender(None, ServiceError::UnableToReachService(self.service_name.clone()));
                                continue;
                            };

                            let started = Instant::now();
                            let mut sender = sender.clone();
                            let adaptor = adaptor.clone();
                            let target_url = self.target.url.clone();
                            let target_addr = self.target.to_string();
                            let message_histogram = message_histogram.clone();
                            let http_timeout = self.http_timeout();
                            let service_name = self.service_name.clone();
                            let served = served.clone();

                            requests.spawn(async move {
                                let request = match adaptor.process_srv_request(data, &target_url) {
                                    Ok(request) => request,
                                    Err(e) => {
                                        let _ = req_msg.return_error_to_sender(None, e);
                                        return;
                                    }
                                };

                                let answered = timeout(http_timeout, async {
                                    let response = sender.send_request(request).await;
                                    if response.is_ok() {
                                        served.store(true, Ordering::Relaxed);
                                    }
                                    record_message(&message_histogram, &target_addr, &response, started, "HTTP/2");
                                    adaptor.process_http_response(response).await
                                })
                                .await;

                                match answered {
                                    Ok(Ok(response)) => { let _ = req_msg.return_to_sender(response); },
                                    Ok(Err(e)) => { let _ = req_msg.return_error_to_sender(None, e); },
                                    Err(_) => { let _ = req_msg.return_error_to_sender(None, ServiceError::Timeout(service_name, http_timeout.as_millis() as u64)); },
                                }
                            });
                        },
                        InternalMsg::Response(msg) => panic!(
                            "The H2 hyper client socket {}/{} receive a response {:?}",
                            proc.get_proc_id(),
                            self.id,
                            msg
                        ),
                        InternalMsg::Error(err_msg) => panic!(
                            "The H2 hyper client socket {}/{} receive an error {:?}",
                            proc.get_proc_id(),
                            self.id,
                            err_msg
                        ),
                        // The processor is the one that reads the service table and the
                        // configuration, and tells its sockets what came out of it
                        InternalMsg::Service(_) | InternalMsg::Config(_) => {},
                        // The main task shutting ProSA down
                        InternalMsg::Shutdown => break,
                    }
                }
            }
        }

        // Stop taking new requests before draining the ones already sent, which must happen even if
        // the bus couldn't be told: dropping the set here would abort them unanswered
        self.withdraw_socket_queue(proc).await;
        self.drain_requests(&mut requests).await;

        Ok(served.load(Ordering::Relaxed))
    }

    /// Connect the socket and serve it until it closes, answering whether the connection worked.
    ///
    /// A connection that served something did its job, and so did one that merely stayed open long
    /// enough: an idle keep-alive close is healthy, and a socket that backed off from those would
    /// leave a quiet pool disconnected. What is left is a backend that accepts and closes right
    /// away, which is indistinguishable from one that refuses and has to be backed off the same way
    async fn connect<A>(
        &mut self,
        proc: &ProcParam<M>,
        adaptor: &Arc<A>,
        meters: &SocketMeters,
        healthy_connection: Duration,
    ) -> Result<bool, HyperProcError>
    where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        // Picked up here rather than at spawn, so a reload that only changes it applies to the next
        // attempt instead of retiring a socket that is connecting perfectly well
        self.target.connect_timeout = self.control.borrow().connect_timeout;

        // Connecting is the one thing a socket does that its bus queue can't interrupt, and it can
        // take two `connect_timeout` with the handshake below, so the shutdown is raced against it
        let stream = tokio::select! {
            stream = self.target.connect() => stream?,
            _ = Self::stopped(&mut self.control) => return Ok(true),
        };

        let io = TokioIo::new(stream);
        let is_http2 = io.inner().selected_alpn_check(|alpn| alpn == H2);

        // Counted here rather than in the processor, which only knows the sockets it wants to
        // exist. This reports the ones that are really connected, so a backend that is down or
        // flapping shows up as its counter dropping
        let socket_attributes = [
            KeyValue::new("target", self.target.to_string()),
            KeyValue::new("version", if is_http2 { "HTTP/2" } else { "HTTP/1.1" }),
        ];
        meters.socket_counter.add(1, &socket_attributes);

        let connected_at = Instant::now();
        let result = if is_http2 {
            self.serve_h2(io, proc, adaptor, &meters.message_histogram)
                .await
        } else {
            self.serve_http1(io, proc, adaptor, &meters.message_histogram)
                .await
        };

        meters.socket_counter.add(-1, &socket_attributes);
        result.map(|served| served || connected_at.elapsed() >= healthy_connection)
    }

    /// Wait `reconnect_delay` out before connecting again, serving the queue in the meantime.
    ///
    /// The socket has no bus queue while it isn't connected, so nothing should reach it here, but a
    /// processor holding an older service table still can. Answering rather than letting a request
    /// sit in the queue is what keeps its sender from waiting on a backend that is down.
    ///
    /// Break when the socket was asked to stop, which the processor does here too: a socket that is
    /// only waiting to reconnect is unknown to the main task
    async fn wait_reconnect(&mut self, reconnect_delay: Duration) -> ControlFlow<()> {
        if reconnect_delay.is_zero() {
            return ControlFlow::Continue(());
        }

        debug!(
            socket_id = self.id,
            addr = %self.target,
            "Reconnect the socket in {reconnect_delay:?}"
        );

        let sleep = time::sleep(reconnect_delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return ControlFlow::Continue(()),
                _ = Self::stopped(&mut self.control) => return ControlFlow::Break(()),
                Some(msg) = self.rx_queue.recv() => {
                    match msg {
                        InternalMsg::Request(req_msg) => {
                            let _ = req_msg.return_error_to_sender(
                                None,
                                ServiceError::UnableToReachService(self.service_name.clone()),
                            );
                        }
                        InternalMsg::Shutdown => return ControlFlow::Break(()),
                        _ => {}
                    }
                }
            }
        }
    }

    /// Method to spawn a task to handle the Hyper client socket, answering the task it runs in.
    ///
    /// The task always gives the socket back to the processor, even when it never managed to
    /// connect, so a backend that is down doesn't silently shrink the pool. The processor is the
    /// only one that decides whether to restart it, and the task id is how it finds the slot again
    /// if the task panicked instead of returning
    pub(super) fn spawn<A>(
        mut self,
        join_set: &mut JoinSet<(Self, Option<HyperProcError>)>,
        proc: Arc<ProcParam<M>>,
        adaptor: Arc<A>,
        settings: &HyperClientSettings,
        meters: SocketMeters,
    ) -> tokio::task::Id
    where
        A: 'static + Adaptor + HyperClientAdaptor<M> + std::marker::Send + std::marker::Sync,
    {
        let reconnect_delay = settings.reconnect_delay(self.retry);
        let healthy_connection = settings.base_reconnect_delay();

        join_set
            .spawn(async move {
                if self.is_stopped() || self.wait_reconnect(reconnect_delay).await.is_break() {
                    return (self, None);
                }

                let connected = self
                    .connect(&proc, &adaptor, &meters, healthy_connection)
                    .await;

                // Back off from a backend the socket couldn't get a working connection out of, whether
                // it refused one or gave one it closed straight away. Both look the same to a caller,
                // and reconnecting either without waiting is a loop at the speed of the machine
                self.retry = if matches!(connected, Ok(true)) {
                    0
                } else {
                    self.retry.saturating_add(1)
                };

                (self, connected.err())
            })
            .id()
    }
}
