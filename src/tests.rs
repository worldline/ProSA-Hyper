//! Tests module to handle HTTP client and server tests

use openssl::{
    asn1::{Asn1Integer, Asn1Time},
    bn::{BigNum, MsbOption},
    ec::{Asn1Flag, EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::PKey,
    symm::Cipher,
    x509::{X509, X509NameBuilder, extension::SubjectAlternativeName},
};
use prosa::{core::settings::settings, inj::proc::InjSettings, stub::proc::StubSettings};
use prosa_utils::config::{ConfigError, os_country, ssl::SslConfig};
use serde::Serialize;
use std::{fs::File, io::Write as _, num::TryFromIntError};

use crate::{client::proc::HyperClientSettings, server::proc::HyperServerSettings};

/// HTTP settings for tests
#[settings]
#[derive(Default, Debug, Serialize)]
pub(crate) struct HttpTestSettings {
    pub(crate) stub: StubSettings,
    pub(crate) inj: InjSettings,
    #[cfg(feature = "server")]
    pub(crate) server: HyperServerSettings,
    #[cfg(feature = "client")]
    pub(crate) client: HyperClientSettings,
}

impl HttpTestSettings {
    /// Passphrase protecting the private keys [`HttpTestSettings::create_server_cert`] writes.
    ///
    /// A test restating the SSL configuration in a reload has to say it again
    pub(crate) const PASSPHRASE: &str = "prosa_test";

    /// Method to create private key and certificate for a server
    pub(crate) fn create_server_cert(
        key_path: String,
        cert_path: String,
    ) -> Result<SslConfig, ConfigError> {
        const PASSPHRASE: &str = HttpTestSettings::PASSPHRASE;

        let mut group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        group.set_asn1_flag(Asn1Flag::NAMED_CURVE);
        let pkey = PKey::from_ec_key(EcKey::generate(&group)?)?;
        let mut pkey_file =
            File::create(key_path.clone()).map_err(|e| ConfigError::IoFile(key_path.clone(), e))?;
        pkey_file
            .write_all(&pkey.private_key_to_pem_pkcs8_passphrase(
                Cipher::aes_256_cbc(),
                PASSPHRASE.as_bytes(),
            )?)
            .map_err(|e| ConfigError::IoFile(key_path.clone(), e))?;

        let mut cert = X509::builder()?;
        cert.set_version(2)?;
        cert.set_pubkey(&pkey)?;

        let mut serial_bn = BigNum::new()?;
        serial_bn.pseudo_rand(64, MsbOption::MAYBE_ZERO, true)?;
        let serial_number = Asn1Integer::from_bn(&serial_bn)?;
        cert.set_serial_number(&serial_number)?;

        let begin_valid_time = Asn1Time::from_unix(
            (std::time::UNIX_EPOCH
                .elapsed()
                .expect("UNIX epoch should be valid")
                .as_secs()
                - 360)
                .try_into()
                .map_err(|e: TryFromIntError| {
                    ConfigError::WrongValue("Asn1Time::from_UNIX_EPOCH".to_string(), e.to_string())
                })?,
        )?;
        cert.set_not_before(&begin_valid_time)?;
        let end_valid_time = Asn1Time::days_from_now(3)?; // 3 days from now
        cert.set_not_after(&end_valid_time)?;

        let mut x509_name = X509NameBuilder::new()?;
        if let Some(cn) = os_country() {
            x509_name.append_entry_by_text("C", cn.as_str())?;
        }
        x509_name.append_entry_by_text("CN", "ProSA-hyper")?;
        let x509_name = x509_name.build();
        cert.set_subject_name(&x509_name)?;
        cert.set_issuer_name(&x509_name)?;

        let mut subject_alternative_name = SubjectAlternativeName::new();
        let x509_extension = subject_alternative_name
            .dns("localhost")
            .build(&cert.x509v3_context(None, None))?;
        cert.append_extension2(&x509_extension)?;

        cert.sign(&pkey, MessageDigest::sha256())?;

        let mut cert_file = File::create(cert_path.clone())
            .map_err(|e| ConfigError::IoFile(cert_path.clone(), e))?;
        cert_file
            .write_all(&cert.build().to_pem()?)
            .map_err(|e| ConfigError::IoFile(cert_path.clone(), e))?;

        Ok(SslConfig::new_cert_key(
            cert_path,
            key_path,
            Some(PASSPHRASE.into()),
        ))
    }

    /// Create a new HttpTestSettings with the given URL and optional SSL configuration for the server.
    /// The client will be configured to connect to the same URL.
    pub(crate) fn new(
        url: url::Url,
        server_ssl: Option<SslConfig>,
        client_ssl: Option<SslConfig>,
    ) -> Self {
        HttpTestSettings {
            stub: StubSettings::new(vec!["STUB_HTTP_SRV".to_string()]),
            inj: InjSettings::new("HTTP_CLIENT_SRV".to_string()),
            #[cfg(feature = "server")]
            server: HyperServerSettings::new(
                prosa::io::listener::ListenerSetting::new(url.clone(), server_ssl),
                std::time::Duration::from_secs(1),
            ),
            #[cfg(feature = "client")]
            client: {
                let mut client = HyperClientSettings::new("HTTP_CLIENT_SRV".to_string());
                client.add_backend(prosa::io::stream::TargetSetting::new(
                    url.clone(),
                    client_ssl,
                    None,
                ));
                client
            },
            ..Default::default()
        }
    }
}

/// Ports the Hyper server processors of the tests bound, by processor name.
///
/// The tests share a process, so a name must belong to a single processor of a single test:
/// [`bound_url`] reads the first port published under a name, and a processor publishes once
static BOUND_PORTS: std::sync::Mutex<Vec<(String, u16)>> = std::sync::Mutex::new(Vec::new());

/// Publish where a Hyper server processor bound, called by the test adaptors.
///
/// The tests listen on the port 0 so that the operating system picks a port nothing else holds,
/// which means the port is only known once bound. The processor hands its address to the adaptor,
/// and the adaptor leaves it here for the test to pick up
pub(crate) fn set_bound_port(proc_name: &str, port: u16) {
    BOUND_PORTS
        .lock()
        .expect("Bound test ports should be writable")
        .push((proc_name.to_string(), port));
}

/// Wait for the Hyper server processor `proc_name` to bind, and return `url` pointing at it.
///
/// The processor publishes its address before serving anything, so this also stands for waiting on
/// the listener
pub(crate) async fn bound_url(proc_name: &str, url: &url::Url) -> url::Url {
    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
    loop {
        let bound_port = BOUND_PORTS
            .lock()
            .expect("Bound test ports should be readable")
            .iter()
            .find(|(name, _)| name == proc_name)
            .map(|(_, port)| *port);

        if let Some(port) = bound_port {
            let mut url = url.clone();
            url.set_port(Some(port))
                .expect("Test URL should accept a port");
            return url;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "The Hyper server processor {proc_name} should have bound"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Interval between two polls of [`wait_for`]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Time a test waits for something it expects to happen.
///
/// Generous on purpose: it is only ever reached by a test that is going to fail anyway
pub(crate) const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Poll `condition` until it holds, returning `false` if it still doesn't after `timeout`.
///
/// Tests wait on the state they expect instead of sleeping a fixed budget, so a loaded machine
/// makes them slower rather than making them fail
pub(crate) async fn wait_for<F>(timeout: std::time::Duration, mut condition: F) -> bool
where
    F: AsyncFnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return true;
        } else if tokio::time::Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Wait for `counter` to stop moving for a whole `quiet` window, and return the value it settled on.
///
/// Traffic doesn't stop the instant a socket is retired, it still answers what it had in flight
pub(crate) async fn wait_for_quiescence(
    counter: &std::sync::atomic::AtomicU32,
    quiet: std::time::Duration,
    timeout: std::time::Duration,
) -> u32 {
    use std::sync::atomic::Ordering;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let count = counter.load(Ordering::SeqCst);
        tokio::time::sleep(quiet).await;
        if count == counter.load(Ordering::SeqCst) || tokio::time::Instant::now() >= deadline {
            return counter.load(Ordering::SeqCst);
        }
    }
}

#[allow(clippy::module_inception)]
#[cfg(all(feature = "server", feature = "client"))]
mod tests {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full, combinators::BoxBody};
    use hyper::{Method, Request, Response, StatusCode};
    use prosa::{
        core::{
            adaptor::{Adaptor, MaybeAsync},
            error::ProcError,
            main::{MainProc, MainRunnable as _},
            msg::Tvf,
            proc::{Proc, ProcBusParam as _, ProcConfig as _},
            service::ServiceError,
            settings::ProsaConfig,
        },
        inj::{adaptor::InjAdaptor, proc::InjProc},
        stub::{adaptor::StubAdaptor, proc::StubProc},
    };
    use prosa_utils::{
        config::{
            ConfigError,
            ssl::{SslConfig, Store},
        },
        msg::simple_string_tvf::SimpleStringTvf,
    };
    use std::{
        env, fs, io,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
    };
    use tokio::{runtime, time};
    use url::Url;

    use crate::{
        HyperResp, PRODUCT_VERSION_HEADER,
        client::{adaptor::HyperClientAdaptor, proc::HyperClientProc},
        server::{
            adaptor::{HyperServerAdaptor, default_srv_error_response},
            proc::HyperServerProc,
        },
        tests::{
            HttpTestSettings, TEST_TIMEOUT, bound_url, set_bound_port, wait_for,
            wait_for_quiescence,
        },
    };

    const WAIT_TIME: time::Duration = time::Duration::from_secs(1);

    /// Prefix of the user agent the Hyper client sets on every request
    const CLIENT_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/");

    /// What the stub answers, and therefore what a complete round trip brings back
    const STUB_RESPONSE: &str = "Hello from the stub!";

    /// Server processor of each test.
    ///
    /// The tests share a process, so they name their processors apart to read back the port of
    /// their own listener and not of somebody else's
    const SERVER_PROCS: [&str; 9] = [
        "HTTP_SERVER_PROC",
        "HTTPS_SERVER_PROC",
        "H2_SERVER_PROC",
        "RELOAD_SERVER_PROC",
        "RECONNECT_SERVER_PROC",
        // The next three tests answer from a raw socket, they run no server processor
        "SPLIT_BODY_SERVER_PROC",
        "GRACEFUL_SERVER_PROC",
        "BACKOFF_SERVER_PROC",
        "PANIC_SERVER_PROC",
    ];

    /// Client processor of each test, at the index of the counter that test uses
    const CLIENT_PROCS: [&str; 9] = [
        "HTTP_CLIENT_PROC",
        "HTTPS_CLIENT_PROC",
        "H2_CLIENT_PROC",
        "RELOAD_CLIENT_PROC",
        "RECONNECT_CLIENT_PROC",
        "SPLIT_BODY_CLIENT_PROC",
        "GRACEFUL_CLIENT_PROC",
        "BACKOFF_CLIENT_PROC",
        "PANIC_CLIENT_PROC",
    ];

    static COUNTER: [AtomicU32; 9] = [
        AtomicU32::new(0), // HTTP
        AtomicU32::new(0), // HTTPS
        AtomicU32::new(0), // HTTP/2
        AtomicU32::new(0), // HTTP, configuration reload
        AtomicU32::new(0), // HTTP, socket reconnection
        AtomicU32::new(0), // HTTP, response body split across two writes
        AtomicU32::new(0), // HTTP, graceful shutdown of the client socket
        AtomicU32::new(0), // HTTP, backoff on a backend that closes right away
        AtomicU32::new(0), // HTTP, socket task panicking in the adaptor
    ];

    /// Test whose client socket is stopped in the middle of a transaction
    const GRACEFUL_TEST_TYPE: usize = 6;

    /// Requests the socket of [`GRACEFUL_TEST_TYPE`] handed to Hyper, and the ones Hyper answered.
    ///
    /// A transaction the socket started must come back, ProSA stopping in the middle of it included
    static GRACEFUL_STARTED: AtomicU32 = AtomicU32::new(0);
    static GRACEFUL_ANSWERED: AtomicU32 = AtomicU32::new(0);

    /// Test whose client adaptor panics once, in the middle of a socket task
    const PANIC_TEST_TYPE: usize = 8;

    /// Set to make the client adaptor of [`PANIC_TEST_TYPE`] panic on the next response it reads
    static PANIC_ARMED: AtomicBool = AtomicBool::new(false);

    /// Number of adaptors the client processor of [`PANIC_TEST_TYPE`] built.
    ///
    /// One per run of its main loop, so it counts the times the processor was restarted
    static PANIC_CLIENT_ADAPTORS: AtomicU32 = AtomicU32::new(0);

    #[derive(Adaptor, Default, Clone, Copy)]
    struct TestAdaptor {
        test_type: u64,
    }

    impl<M> StubAdaptor<M> for TestAdaptor
    where
        M: 'static
            + std::marker::Send
            + std::marker::Sync
            + std::marker::Sized
            + std::clone::Clone
            + std::fmt::Debug
            + Tvf
            + std::default::Default,
    {
        fn new(_proc: &StubProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
            Ok(Self { test_type: 0 })
        }

        fn process_request(
            &self,
            _service_name: &str,
            request: M,
        ) -> MaybeAsync<Result<M, ServiceError>> {
            match request
                .get_string(1)
                .and_then(|b| request.get_string(2).map(|ua| (b, ua)))
            {
                Ok((content, user_agent)) => {
                    if !content.starts_with("Hello") || !user_agent.starts_with(CLIENT_USER_AGENT) {
                        return Err(ServiceError::ProtocolError(format!(
                            "Invalid request content: {content:?} from {user_agent:?}"
                        )))
                        .into();
                    }
                }
                Err(e) => return Err(ServiceError::ProtocolError(e.to_string())).into(),
            }

            let mut srv_req = M::default();
            srv_req.put_string(1, STUB_RESPONSE);
            Ok(srv_req).into()
        }
    }

    impl<M> HyperServerAdaptor<M> for TestAdaptor
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
        fn new(
            proc: &crate::server::proc::HyperServerProc<M>,
            addr: prosa::io::SocketAddr,
        ) -> Result<Self, Box<dyn ProcError + Send + Sync>>
        where
            Self: Sized,
        {
            // The listener is configured on the port 0, this is where the test learns where it landed
            set_bound_port(proc.name(), addr.port());

            let test_type = match proc.settings.listener.url.scheme() {
                "http" => 0,
                "https" => 1,
                "h2" => 2,
                _ => {
                    return Err(Box::new(ServiceError::ProtocolError(
                        "Unsupported scheme".into(),
                    )));
                }
            };

            Ok(TestAdaptor { test_type })
        }

        async fn process_http_request(
            &self,
            req: Request<hyper::body::Incoming>,
        ) -> HyperResp<Self, M> {
            let mut srv_req = M::default();

            if let Some(user_agent) = req
                .headers()
                .get(hyper::header::USER_AGENT)
                .and_then(|h| h.to_str().ok())
            {
                srv_req.put_string(2, user_agent);
            }

            if let Ok(body) = req.into_body().collect().await
                && let Ok(body_str) = String::from_utf8(body.to_bytes().to_vec())
            {
                srv_req.put_string(1, body_str);
            }

            HyperResp::SrvReq(
                "STUB_HTTP_SRV".into(),
                srv_req,
                Box::new(move |adaptor, result| match result {
                    Ok(resp) => {
                        if let Ok(content) = resp.get_string(1) {
                            <TestAdaptor as HyperServerAdaptor<M>>::response_builder(
                                adaptor,
                                StatusCode::OK,
                            )
                            .body(BoxBody::new(Full::new(Bytes::from_owner(
                                content.into_owned(),
                            ))))
                            .map_err(|e| e.into())
                        } else {
                            <TestAdaptor as HyperServerAdaptor<M>>::response_builder(
                                adaptor,
                                StatusCode::BAD_REQUEST,
                            )
                            .body(BoxBody::new(Full::new(Bytes::from("Bad Request"))))
                            .map_err(|e| e.into())
                        }
                    }
                    Err(err) => default_srv_error_response(&err, |s| {
                        <TestAdaptor as HyperServerAdaptor<M>>::response_builder(adaptor, s)
                    }),
                }),
            )
        }
    }

    impl<M> InjAdaptor<M> for TestAdaptor
    where
        M: 'static
            + std::marker::Send
            + std::marker::Sync
            + std::marker::Sized
            + std::clone::Clone
            + std::fmt::Debug
            + Tvf
            + std::default::Default,
    {
        fn new(_proc: &InjProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
            Ok(Self { test_type: 0 })
        }

        fn build_transaction(&mut self) -> M {
            let mut msg = M::default();
            msg.put_string(1, "Hello, ProSA Hyper! This is an injected message");
            msg
        }

        fn process_response(
            &mut self,
            response: M,
            _service_name: &str,
        ) -> Result<(), Box<dyn ProcError + Send + Sync>> {
            // The counters must only count a complete round trip. The client adaptor turns any HTTP
            // response into a message, so without this an error status reads as a success
            let content = response
                .get_string(1)
                .map_err(|e| ServiceError::ProtocolError(e.to_string()))?;
            if !content.starts_with(STUB_RESPONSE) {
                return Err(Box::new(ServiceError::ProtocolError(format!(
                    "Unexpected response content: {content:?}"
                ))));
            }

            match response
                .get_unsigned(10)
                .map_err(|e| ServiceError::ProtocolError(e.to_string()))?
            {
                0 => {
                    // HTTP
                    COUNTER[0].fetch_add(1, Ordering::SeqCst);
                }
                1 => {
                    // HTTPS
                    COUNTER[1].fetch_add(1, Ordering::SeqCst);
                }
                2 => {
                    // HTTP/2
                    COUNTER[2].fetch_add(1, Ordering::SeqCst);
                }
                3 => {
                    // HTTP, configuration reload
                    COUNTER[3].fetch_add(1, Ordering::SeqCst);
                }
                4 => {
                    // HTTP, socket reconnection
                    COUNTER[4].fetch_add(1, Ordering::SeqCst);
                }
                5 => {
                    // HTTP, response body split across two writes
                    COUNTER[5].fetch_add(1, Ordering::SeqCst);
                }
                6 => {
                    // HTTP, graceful shutdown of the client socket
                    COUNTER[6].fetch_add(1, Ordering::SeqCst);
                }
                7 => {
                    // HTTP, backoff on a backend that closes right away
                    COUNTER[7].fetch_add(1, Ordering::SeqCst);
                }
                8 => {
                    // HTTP, socket task panicking in the adaptor
                    COUNTER[8].fetch_add(1, Ordering::SeqCst);
                }
                _ => {
                    return Err(Box::new(ServiceError::ProtocolError(
                        "Invalid response type".into(),
                    )));
                }
            }

            Ok(())
        }
    }

    impl<M> HyperClientAdaptor<M> for TestAdaptor
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
        fn new(proc: &HyperClientProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>>
        where
            Self: Sized,
        {
            // The backend ports come from the operating system, so the test a client belongs to is
            // read from its name
            let Some(test_type) = CLIENT_PROCS.iter().position(|name| *name == proc.name()) else {
                return Err(Box::new(ConfigError::WrongValue(
                    "HyperClientProc::name".into(),
                    proc.name().into(),
                )));
            };

            if test_type == PANIC_TEST_TYPE {
                PANIC_CLIENT_ADAPTORS.fetch_add(1, Ordering::SeqCst);
            }

            Ok(TestAdaptor {
                test_type: test_type as u64,
            })
        }

        fn process_srv_request(
            &self,
            request: M,
            socket_url: &Url,
        ) -> Result<
            Request<BoxBody<Bytes, std::convert::Infallible>>,
            prosa::core::service::ServiceError,
        > {
            match request.get_string(1) {
                Ok(body) => Request::builder()
                    .method(Method::POST)
                    .uri(socket_url.as_str())
                    .header(hyper::header::USER_AGENT, PRODUCT_VERSION_HEADER)
                    .body(BoxBody::new(Full::new(Bytes::from(body.into_owned()))))
                    .inspect(|_| {
                        // The socket sends the request right after this, so the transaction is in
                        // flight from here until the response comes back
                        if self.test_type == GRACEFUL_TEST_TYPE as u64 {
                            GRACEFUL_STARTED.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .map_err(|e| {
                        ServiceError::ProtocolError(format!("Failed to build request: {}", e))
                    }),
                Err(e) => Err(prosa::core::service::ServiceError::ProtocolError(
                    e.to_string(),
                )),
            }
        }

        async fn process_http_response(
            &self,
            resp: Result<Response<hyper::body::Incoming>, hyper::Error>,
        ) -> Result<M, prosa::core::service::ServiceError> {
            // Adaptors are user code and this one is asked to fail the way user code does. It runs
            // inside the socket task, so the panic takes that task down with the request it holds
            if self.test_type == PANIC_TEST_TYPE as u64 && PANIC_ARMED.swap(false, Ordering::SeqCst)
            {
                panic!("The client adaptor of the panic test was armed to panic");
            }

            let http_body =
                resp.map_err(|e| ServiceError::ProtocolError(format!("HTTP error: {}", e)))?;
            if let Ok(body) = http_body.into_body().collect().await
                && let Ok(body_str) = String::from_utf8(body.to_bytes().to_vec())
            {
                if self.test_type == GRACEFUL_TEST_TYPE as u64 {
                    GRACEFUL_ANSWERED.fetch_add(1, Ordering::SeqCst);
                }

                let mut srv_req = M::default();
                srv_req.put_string(1, body_str);
                srv_req.put_unsigned(10, self.test_type);
                Ok(srv_req)
            } else {
                Err(ServiceError::ProtocolError(
                    "Failed to read response body".into(),
                ))
            }
        }
    }

    async fn run_test(mut settings: HttpTestSettings, test_type: usize) -> Result<(), io::Error> {
        let server_url = settings.server.listener.url.clone();

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));

        // Launch the main task in a separate thread to run the ProSA runtime
        let main_handle =
            std::thread::Builder::new()
                .name("main".to_string())
                .spawn(move || {
                    runtime::Builder::new_multi_thread()
                        .worker_threads(1)
                        .enable_all()
                        .thread_name("main")
                        .build()
                        .expect("Runtime should be valid")
                        .block_on(async {
                            main.run().await;
                        })
                })?;

        // Launch stub to respond to the HTTP server
        let http_server_stub = StubProc::<SimpleStringTvf>::create(
            1,
            String::from("HTTP_SERVER_STUB"),
            bus.clone(),
            settings.stub,
        );
        Proc::<TestAdaptor>::run(http_server_stub)?;

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            2,
            String::from(SERVER_PROCS[test_type]),
            bus.clone(),
            settings.server,
        );
        Proc::<TestAdaptor>::run(http_server_proc)?;

        // The listener is on the port 0, so the client can only be pointed at the server once the
        // processor has published where it bound
        let server_url = bound_url(SERVER_PROCS[test_type], &server_url).await;
        for backend in &mut settings.client.backends {
            backend.url = server_url.clone();
        }

        // Launch an HTTP client processor
        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            3,
            String::from(CLIENT_PROCS[test_type]),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc)?;

        // Launch an HTTP injector processor
        let http_inj_proc = InjProc::<SimpleStringTvf>::create(
            4,
            String::from("HTTP_INJ_PROC"),
            bus.clone(),
            settings.inj,
        );
        Proc::<TestAdaptor>::run(http_inj_proc)?;

        // Wait for a full loop: injector, client, HTTP, server and stub
        let responded = wait_for(TEST_TIMEOUT, async || {
            COUNTER[test_type].load(Ordering::SeqCst) > 0
        })
        .await;

        bus.stop("ProSA HTTP client server unit test end".into())
            .await
            .map_err(io::Error::other)?;

        assert!(
            responded,
            "No response received for test type {}",
            test_type
        );

        // Wait on main task to end
        let _ = main_handle.join();
        Ok(())
    }

    #[tokio::test]
    async fn http_client_server() {
        let test_settings = HttpTestSettings::new(
            Url::parse("http://localhost:0").expect("HTTP client/server URL should be valid"),
            None,
            None,
        );

        // Run a ProSA to test
        assert!(run_test(test_settings, 0).await.is_ok());
    }

    #[tokio::test]
    async fn https_client_server() {
        const PROSA_HTTPS_TEST_DIR_NAME: &str = "ProSA_HTTPS";
        let prosa_temp_dir = env::temp_dir().join(PROSA_HTTPS_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir)
            .expect("Can't create ProSA temporary directory for HTTPS");

        let key_path = prosa_temp_dir.join("prosa_https.key");
        let cert_path = prosa_temp_dir.join("prosa_https.pem");
        let server_ssl_config = HttpTestSettings::create_server_cert(
            key_path
                .as_os_str()
                .to_str()
                .expect("Key path should be a valid String")
                .into(),
            cert_path
                .as_os_str()
                .to_str()
                .expect("Cert path should be a valid String")
                .into(),
        )
        .expect("Server certificate should be created");

        let client_ssl_store = Store::File {
            path: prosa_temp_dir
                .as_os_str()
                .to_str()
                .expect("ProSA temp dir should be a valid String")
                .into(),
        };
        let mut client_ssl_config = SslConfig::default();
        client_ssl_config.set_store(client_ssl_store);

        let test_settings = HttpTestSettings::new(
            Url::parse("https://localhost:0").expect("HTTPS client/server URL should be valid"),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        assert!(run_test(test_settings, 1).await.is_ok());
    }

    #[tokio::test]
    async fn h2_client_server() {
        const PROSA_H2_TEST_DIR_NAME: &str = "ProSA_H2";
        let prosa_temp_dir = env::temp_dir().join(PROSA_H2_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).expect("Can't create ProSA temporary directory for H2");

        let key_path = prosa_temp_dir.join("prosa_h2.key");
        let cert_path = prosa_temp_dir.join("prosa_h2.pem");
        let server_ssl_config = HttpTestSettings::create_server_cert(
            key_path
                .as_os_str()
                .to_str()
                .expect("Key path should be a valid String")
                .into(),
            cert_path
                .as_os_str()
                .to_str()
                .expect("Cert path should be a valid String")
                .into(),
        )
        .expect("Server certificate should be created");

        let client_ssl_store = Store::File {
            path: format!(
                "{}/",
                prosa_temp_dir
                    .as_os_str()
                    .to_str()
                    .expect("ProSA temp dir should be a valid String")
            ),
        };
        let mut client_ssl_config = SslConfig::default();
        client_ssl_config.set_store(client_ssl_store);
        client_ssl_config.set_alpn(vec!["h2".into()]);

        let test_settings = HttpTestSettings::new(
            Url::parse("h2://localhost:0").expect("HTTP2 client/server URL should be valid"),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        assert!(run_test(test_settings, 2).await.is_ok());
    }

    /// Build a ProSA configuration that points the Hyper client processor at the given backend
    fn client_backend_config(proc_name: &str, backend_url: &Url) -> ProsaConfig {
        ProsaConfig::from_config(
            config::Config::builder()
                .add_source(config::File::from_str(
                    &format!(
                        "[{proc_name}]\nservice_name = \"HTTP_CLIENT_SRV\"\nbackends = [{{ url = \"{backend_url}\" }}]\n"
                    ),
                    config::FileFormat::Toml,
                ))
                .build()
                .expect("Hyper client configuration should be valid"),
        )
        .expect("Reloaded ProSA configuration should be valid")
    }

    #[tokio::test]
    async fn client_config_reload() {
        const TEST_TYPE: usize = 3;
        // The port 1 is privileged, so nothing can be listening there and the client can't reach
        // any backend once reloaded on it
        let dead_url = Url::parse("http://localhost:1").expect("Dead backend URL should be valid");

        let mut settings = HttpTestSettings::new(
            Url::parse("http://localhost:0").expect("Backend URL should be valid"),
            None,
            None,
        );
        let server_url = settings.server.listener.url.clone();

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));

        // The main task must run to broadcast the configuration to the processors
        let main_handle = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("main")
                    .build()
                    .expect("Runtime should be valid")
                    .block_on(async {
                        main.run().await;
                    })
            })
            .expect("Main thread should be spawned");

        // Launch stub to respond to the HTTP server
        let http_server_stub = StubProc::<SimpleStringTvf>::create(
            1,
            String::from("HTTP_SERVER_STUB"),
            bus.clone(),
            settings.stub,
        );
        Proc::<TestAdaptor>::run(http_server_stub).expect("Stub processor should run");

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            2,
            String::from(SERVER_PROCS[TEST_TYPE]),
            bus.clone(),
            settings.server,
        );
        Proc::<TestAdaptor>::run(http_server_proc).expect("Hyper server processor should run");

        // The listener is on the port 0, so the backend is only known once the processor bound
        let backend_url = bound_url(SERVER_PROCS[TEST_TYPE], &server_url).await;
        for backend in &mut settings.client.backends {
            backend.url = backend_url.clone();
        }

        // Launch an HTTP client processor
        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            3,
            String::from(CLIENT_PROCS[TEST_TYPE]),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc).expect("Hyper client processor should run");

        // Launch an HTTP injector processor
        let http_inj_proc = InjProc::<SimpleStringTvf>::create(
            4,
            String::from("HTTP_INJ_PROC"),
            bus.clone(),
            settings.inj,
        );
        Proc::<TestAdaptor>::run(http_inj_proc).expect("Inj processor should run");

        assert!(
            wait_for(TEST_TIMEOUT, async || COUNTER[TEST_TYPE]
                .load(Ordering::SeqCst)
                > 0)
            .await,
            "No response received before the configuration reload"
        );

        // Reload the client on a backend that can't be reached, retiring the current sockets
        bus.update_config(Arc::new(client_backend_config(
            CLIENT_PROCS[TEST_TYPE],
            &dead_url,
        )))
        .await
        .expect("ProSA configuration should be updated");

        // The retired sockets still answer what they had in flight, so wait for the traffic to stop
        // before taking the reference count
        let retired_count = wait_for_quiescence(&COUNTER[TEST_TYPE], WAIT_TIME, TEST_TIMEOUT).await;
        time::sleep(WAIT_TIME).await;
        assert_eq!(
            retired_count,
            COUNTER[TEST_TYPE].load(Ordering::SeqCst),
            "The sockets of the previous backend should have been retired"
        );

        // Reload the client back on the reachable backend
        bus.update_config(Arc::new(client_backend_config(
            CLIENT_PROCS[TEST_TYPE],
            &backend_url,
        )))
        .await
        .expect("ProSA configuration should be updated");

        assert!(
            wait_for(TEST_TIMEOUT, async || COUNTER[TEST_TYPE]
                .load(Ordering::SeqCst)
                > retired_count)
            .await,
            "No response received after the configuration reload"
        );

        bus.stop("ProSA HTTP client configuration reload unit test end".into())
            .await
            .expect("ProSA should stop");

        // Wait on main task to end
        let _ = main_handle.join();
    }

    /// A client socket that can't connect must keep retrying instead of leaving the pool empty
    #[tokio::test]
    async fn client_socket_reconnect() {
        const TEST_TYPE: usize = 4;

        // The client has to start while the backend is down, so this is the one address that can't
        // come from the processor. Binding and closing right away is only a way to have the
        // operating system name a port, the server processor takes it further down
        let backend_url = {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("A port should be free");
            let addr = listener
                .local_addr()
                .expect("A bound listener should have an address");
            Url::parse(&format!("http://{addr}")).expect("Backend URL should be valid")
        };
        let settings = HttpTestSettings::new(backend_url, None, None);

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));
        let main_handle = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("main")
                    .build()
                    .expect("Runtime should be valid")
                    .block_on(async {
                        main.run().await;
                    })
            })
            .expect("Main thread should be spawned");

        // Launch stub to respond to the HTTP server
        let http_server_stub = StubProc::<SimpleStringTvf>::create(
            1,
            String::from("HTTP_SERVER_STUB"),
            bus.clone(),
            settings.stub,
        );
        Proc::<TestAdaptor>::run(http_server_stub).expect("Stub processor should run");

        // Launch the client and the injector while nothing listens on the backend port
        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            2,
            String::from(CLIENT_PROCS[TEST_TYPE]),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc).expect("Hyper client processor should run");

        let http_inj_proc = InjProc::<SimpleStringTvf>::create(
            3,
            String::from("HTTP_INJ_PROC"),
            bus.clone(),
            settings.inj,
        );
        Proc::<TestAdaptor>::run(http_inj_proc).expect("Inj processor should run");

        // The client sockets fail to connect and back off, none of them can serve anything
        time::sleep(WAIT_TIME).await;
        assert_eq!(
            0,
            COUNTER[TEST_TYPE].load(Ordering::SeqCst),
            "No response can be received while the backend is down"
        );

        // Bring the backend up, the sockets must find it on their next attempt
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            4,
            String::from(SERVER_PROCS[TEST_TYPE]),
            bus.clone(),
            settings.server,
        );
        Proc::<TestAdaptor>::run(http_server_proc).expect("Hyper server processor should run");

        assert!(
            wait_for(TEST_TIMEOUT, async || COUNTER[TEST_TYPE]
                .load(Ordering::SeqCst)
                > 0)
            .await,
            "The client sockets should have reconnected once the backend came up"
        );

        bus.stop("ProSA HTTP client socket reconnection unit test end".into())
            .await
            .expect("ProSA should stop");

        // Wait on main task to end
        let _ = main_handle.join();
    }

    /// Answer requests with a body that cannot reach the client with the head.
    ///
    /// A real HTTP server splits a response whenever it is bigger than one write, which a stub
    /// answering a short string never does. Hyper only hands over the bytes it has read, so the
    /// socket has to keep driving its connection while the adaptor collects the body
    async fn split_body_origin(listener: tokio::net::TcpListener) {
        use tokio::io::AsyncWriteExt as _;

        raw_origin(listener, async |sock| {
            // The head and the first half go out, then a pause long enough that the client cannot
            // have them in the same read, then the rest
            let (first, second) = STUB_RESPONSE.split_at(STUB_RESPONSE.len() / 2);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{first}",
                STUB_RESPONSE.len()
            );
            if sock.write_all(head.as_bytes()).await.is_err() {
                return false;
            }
            time::sleep(time::Duration::from_millis(50)).await;
            sock.write_all(second.as_bytes()).await.is_ok()
        })
        .await;
    }

    /// Answer requests slowly enough that ProSA can be stopped while one is in flight
    async fn slow_origin(listener: tokio::net::TcpListener) {
        use tokio::io::AsyncWriteExt as _;

        raw_origin(listener, async |sock| {
            time::sleep(SLOW_RESPONSE_TIME).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{STUB_RESPONSE}",
                STUB_RESPONSE.len()
            );
            sock.write_all(response.as_bytes()).await.is_ok()
        })
        .await;
    }

    /// Serve `answer` on every request the client sends, until it goes away.
    ///
    /// A raw socket rather than a Hyper server, because these tests are about what the client does
    /// with a response Hyper would never produce on its own. One connection at a time is enough,
    /// they all configure a single socket
    async fn raw_origin<F>(listener: tokio::net::TcpListener, answer: F)
    where
        F: AsyncFn(&mut tokio::net::TcpStream) -> bool,
    {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];

            // Keep the connection alive across requests, which is also what makes the client send
            // a second request on a connection it already used
            while read_request(&mut sock, &mut chunk, &mut buf).await && answer(&mut sock).await {}
        }
    }

    /// Consume one request from `sock`, answering `false` once the peer is gone
    async fn read_request(
        sock: &mut tokio::net::TcpStream,
        chunk: &mut [u8],
        buf: &mut Vec<u8>,
    ) -> bool {
        // The head first, then as many body bytes as it announces
        let head_end = loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            } else if !read_more(sock, chunk, buf).await {
                return false;
            }
        };
        let body_len = String::from_utf8_lossy(&buf[..head_end])
            .to_lowercase()
            .split("content-length:")
            .nth(1)
            .and_then(|value| value.split("\r\n").next())
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < head_end + body_len {
            if !read_more(sock, chunk, buf).await {
                return false;
            }
        }
        buf.drain(..head_end + body_len);

        true
    }

    /// Read the next bytes of a request into `buf`, answering `false` once the peer is gone
    async fn read_more(
        sock: &mut tokio::net::TcpStream,
        chunk: &mut [u8],
        buf: &mut Vec<u8>,
    ) -> bool {
        use tokio::io::AsyncReadExt as _;

        match sock.read(chunk).await {
            Ok(0) | Err(_) => false,
            Ok(read) => {
                buf.extend_from_slice(&chunk[..read]);
                true
            }
        }
    }

    /// A response body that doesn't arrive with the head must still be read
    #[tokio::test]
    async fn client_split_body_response() {
        const TEST_TYPE: usize = 5;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("A port should be free");
        let backend_url = Url::parse(&format!(
            "http://{}",
            listener
                .local_addr()
                .expect("A bound listener should have an address")
        ))
        .expect("Backend URL should be valid");
        tokio::spawn(split_body_origin(listener));

        let settings = HttpTestSettings::new(backend_url, None, None);

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));
        let main_handle = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("main")
                    .build()
                    .expect("Runtime should be valid")
                    .block_on(async {
                        main.run().await;
                    })
            })
            .expect("Main thread should be spawned");

        // Launch an HTTP client processor on the raw backend, and the injector that drives it
        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            1,
            String::from(CLIENT_PROCS[TEST_TYPE]),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc).expect("Hyper client processor should run");

        let http_inj_proc = InjProc::<SimpleStringTvf>::create(
            2,
            String::from("HTTP_INJ_PROC"),
            bus.clone(),
            settings.inj,
        );
        Proc::<TestAdaptor>::run(http_inj_proc).expect("Inj processor should run");

        // More than one, so a second request also goes out on a connection already used once
        assert!(
            wait_for(TEST_TIMEOUT, async || COUNTER[TEST_TYPE]
                .load(Ordering::SeqCst)
                > 1)
            .await,
            "The client should have read the split response bodies"
        );

        bus.stop("ProSA HTTP client split body unit test end".into())
            .await
            .expect("ProSA should stop");

        // Wait on main task to end
        let _ = main_handle.join();
    }

    /// Long enough that ProSA can be stopped while the origin still owes a response
    const SLOW_RESPONSE_TIME: time::Duration = time::Duration::from_millis(300);

    /// A transaction the socket started must be answered, even if ProSA stops in the middle of it
    #[tokio::test]
    async fn client_graceful_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("A port should be free");
        let backend_url = Url::parse(&format!(
            "http://{}",
            listener
                .local_addr()
                .expect("A bound listener should have an address")
        ))
        .expect("Backend URL should be valid");
        tokio::spawn(slow_origin(listener));

        let settings = HttpTestSettings::new(backend_url, None, None);

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));
        let main_handle = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("main")
                    .build()
                    .expect("Runtime should be valid")
                    .block_on(async {
                        main.run().await;
                    })
            })
            .expect("Main thread should be spawned");

        // Launch an HTTP client processor on the slow backend, and the injector that drives it
        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            1,
            String::from(CLIENT_PROCS[GRACEFUL_TEST_TYPE]),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc).expect("Hyper client processor should run");

        let http_inj_proc = InjProc::<SimpleStringTvf>::create(
            2,
            String::from("HTTP_INJ_PROC"),
            bus.clone(),
            settings.inj,
        );
        Proc::<TestAdaptor>::run(http_inj_proc).expect("Inj processor should run");

        // Let a full loop go through first, so what is asserted below is a socket that was working
        assert!(
            wait_for(TEST_TIMEOUT, async || COUNTER[GRACEFUL_TEST_TYPE]
                .load(Ordering::SeqCst)
                > 0)
            .await,
            "No response received before the shutdown"
        );

        // Stop ProSA while the origin owes a response, which is what the socket has to see through.
        // The injector saturates the socket, so it is in flight for all but an instant of the time
        assert!(
            wait_for(TEST_TIMEOUT, async || {
                GRACEFUL_STARTED.load(Ordering::SeqCst) > GRACEFUL_ANSWERED.load(Ordering::SeqCst)
            })
            .await,
            "A transaction should have been in flight"
        );

        bus.stop("ProSA HTTP client graceful shutdown unit test end".into())
            .await
            .expect("ProSA should stop");

        // Every transaction the socket handed to Hyper came back, the one it was in the middle of
        // included. A retired socket starts no other, so a dropped one leaves the counts apart for
        // good. Whether the answer still finds its requester is up to ProSA, which tells the
        // injector to stop at the very same time
        assert!(
            wait_for(TEST_TIMEOUT, async || {
                GRACEFUL_STARTED.load(Ordering::SeqCst) == GRACEFUL_ANSWERED.load(Ordering::SeqCst)
            })
            .await,
            "The socket dropped a transaction it had started: {} started, {} answered",
            GRACEFUL_STARTED.load(Ordering::SeqCst),
            GRACEFUL_ANSWERED.load(Ordering::SeqCst)
        );

        // Wait on main task to end
        let _ = main_handle.join();
    }

    /// A backend that accepts and closes right away must be backed off from, like one that refuses.
    ///
    /// Nothing distinguishes the two for a caller, and a socket that treats the first as a healthy
    /// connection that simply ended reconnects with no delay at all, at the speed of the machine
    #[tokio::test]
    async fn client_dead_backend_backoff() {
        const TEST_TYPE: usize = 7;
        /// Long enough for the backoff to have doubled a few times if it engages at all
        const OBSERVE: time::Duration = time::Duration::from_secs(2);
        /// Delays of 0, then 500, 1000 and 2000 ms by default, so attempts land at 0, 0.5, 1.5 and
        /// 3.5 s and only three of them fall inside `OBSERVE`. One of slack for a loaded machine,
        /// and low enough that a delay that stopped doubling would show up here
        const MAX_ATTEMPTS: u32 = 4;

        static ACCEPTED: AtomicU32 = AtomicU32::new(0);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("A port should be free");
        let backend_url = Url::parse(&format!(
            "http://{}",
            listener
                .local_addr()
                .expect("A bound listener should have an address")
        ))
        .expect("Backend URL should be valid");

        // Accept the connection and drop it, which is what an overloaded backend or a load balancer
        // with no healthy upstream does. The TCP connect succeeds, so the socket has to notice on
        // its own that it got nothing out of it
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                ACCEPTED.fetch_add(1, Ordering::SeqCst);
                drop(sock);
            }
        });

        let settings = HttpTestSettings::new(backend_url, None, None);

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));
        let main_handle = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("main")
                    .build()
                    .expect("Runtime should be valid")
                    .block_on(async {
                        main.run().await;
                    })
            })
            .expect("Main thread should be spawned");

        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            1,
            String::from(CLIENT_PROCS[TEST_TYPE]),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc).expect("Hyper client processor should run");

        time::sleep(OBSERVE).await;
        let accepted = ACCEPTED.load(Ordering::SeqCst);

        bus.stop("ProSA HTTP client backoff unit test end".into())
            .await
            .expect("ProSA should stop");

        assert!(
            accepted > 0,
            "The client socket should have tried to connect"
        );
        assert!(
            accepted <= MAX_ATTEMPTS,
            "The client socket reconnected {accepted} times in {OBSERVE:?}, it is not backing off"
        );

        // Wait on main task to end
        let _ = main_handle.join();
    }

    /// A socket task that panics must not take the other sockets down with it.
    ///
    /// The adaptor runs inside the socket task, so any panic in user code ends that task. The
    /// processor used to propagate the join error, which dropped the whole task set and aborted
    /// every other socket with the requests its queue was holding
    #[tokio::test]
    async fn client_socket_panic_is_contained() {
        let mut settings = HttpTestSettings::new(
            Url::parse("http://localhost:0").expect("Panic test URL should be valid"),
            None,
            None,
        );
        let server_url = settings.server.listener.url.clone();

        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));
        let main_handle = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("main")
                    .build()
                    .expect("Runtime should be valid")
                    .block_on(async {
                        main.run().await;
                    })
            })
            .expect("Main thread should be spawned");

        let http_server_stub = StubProc::<SimpleStringTvf>::create(
            1,
            String::from("HTTP_SERVER_STUB"),
            bus.clone(),
            settings.stub,
        );
        Proc::<TestAdaptor>::run(http_server_stub).expect("Stub processor should run");

        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            2,
            String::from(SERVER_PROCS[PANIC_TEST_TYPE]),
            bus.clone(),
            settings.server,
        );
        Proc::<TestAdaptor>::run(http_server_proc).expect("Hyper server processor should run");

        let server_url = bound_url(SERVER_PROCS[PANIC_TEST_TYPE], &server_url).await;
        for backend in &mut settings.client.backends {
            backend.url = server_url.clone();
        }

        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            3,
            String::from(CLIENT_PROCS[PANIC_TEST_TYPE]),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc).expect("Hyper client processor should run");

        // The request the socket was holding dies with it, and the injector has no timeout of its
        // own, so with a single transaction in flight it would wait on that one forever and the
        // test would measure the injector rather than the pool
        settings.inj.max_concurrents_send = 4;
        let http_inj_proc = InjProc::<SimpleStringTvf>::create(
            4,
            String::from("HTTP_INJ_PROC"),
            bus.clone(),
            settings.inj,
        );
        Proc::<TestAdaptor>::run(http_inj_proc).expect("Inj processor should run");

        // Let the loop settle before breaking it, so the panic lands on a socket that was serving
        assert!(
            wait_for(TEST_TIMEOUT, async || COUNTER[PANIC_TEST_TYPE]
                .load(Ordering::SeqCst)
                > 0)
            .await,
            "The panic test client should have completed a round trip"
        );

        let before_panic = COUNTER[PANIC_TEST_TYPE].load(Ordering::SeqCst);
        PANIC_ARMED.store(true, Ordering::SeqCst);

        // The socket that panicked is gone, and with it the request it was holding. What must come
        // back is the pool: the processor reopens the slot and the next requests are served again
        assert!(
            wait_for(TEST_TIMEOUT, async || COUNTER[PANIC_TEST_TYPE]
                .load(Ordering::SeqCst)
                > before_panic + 2)
            .await,
            "The client should keep serving after one of its socket tasks panicked"
        );

        // A restarted processor would serve again too, so this is what tells the two apart. The
        // adaptor is built once per run of the main loop, and a restart is what abandons the queues
        // of every other socket
        assert_eq!(
            1,
            PANIC_CLIENT_ADAPTORS.load(Ordering::SeqCst),
            "A panicking socket task should not restart the Hyper client processor"
        );

        bus.stop("ProSA HTTP client socket panic unit test end".into())
            .await
            .expect("ProSA should stop");

        let _ = main_handle.join();
    }
}
