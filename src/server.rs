//! Module to handle HTTP server

/// Adaptor for Hyper server processor
pub mod adaptor;
/// ProSA Hyper server processor
pub mod proc;
/// Hyper service definition
pub(crate) mod service;

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http_body_util::{Empty, Full, combinators::BoxBody};
    use hyper::{Request, StatusCode};
    use hyper_util::rt::TokioIo;
    use prosa::core::{
        adaptor::Adaptor,
        error::ProcError,
        main::{MainProc, MainRunnable as _},
        proc::{Proc, ProcBusParam as _, ProcConfig as _},
        settings::ProsaConfig,
    };
    use prosa_utils::{
        config::ssl::{SslConfig, Store},
        msg::simple_string_tvf::SimpleStringTvf,
    };
    use reqwest::Certificate;
    use std::{
        env,
        fs::{self, File},
        io::{self, Read as _},
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::time;
    use url::Url;

    use crate::{
        HyperResp,
        server::{adaptor::HyperServerAdaptor, proc::HyperServerProc},
        tests::{HttpTestSettings, TEST_TIMEOUT, bound_url, set_bound_port, wait_for},
    };

    const WAIT_TIME: time::Duration = time::Duration::from_secs(5);

    #[derive(Adaptor, Clone)]
    struct ServerTestAdaptor {
        // Nothing
    }

    impl<M> HyperServerAdaptor<M> for ServerTestAdaptor
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

            Ok(ServerTestAdaptor {})
        }

        async fn process_http_request(
            &self,
            req: Request<hyper::body::Incoming>,
        ) -> HyperResp<Self, M> {
            let resp_msg = if req.version() == hyper::Version::HTTP_2 {
                "Hello, H2 world"
            } else {
                "Hello, world"
            };
            <ServerTestAdaptor as HyperServerAdaptor<M>>::response_builder(self, StatusCode::OK)
                .body(BoxBody::new(Full::new(Bytes::from(resp_msg))))
                .into()
        }
    }

    /// Time [`SlowServerTestAdaptor`] takes to answer its first request, long enough to still be
    /// serving when ProSA stops. Every further request takes a multiple of it, so the requests in
    /// flight don't all end at the same moment
    const SLOW_RESPONSE_TIME: time::Duration = time::Duration::from_millis(300);

    /// Number of requests [`SlowServerTestAdaptor`] started answering, so a test knows how many are
    /// in flight, and so each one can be given a different response time
    static SLOW_REQUESTS_STARTED: AtomicUsize = AtomicUsize::new(0);

    /// Set when the Hyper server processor reaches the end of its loop, which it only does once
    /// every connection has been drained
    static SLOW_PROC_TERMINATED: AtomicBool = AtomicBool::new(false);

    /// Adaptor that takes its time to answer, so a request is still being served when ProSA stops
    #[derive(Clone)]
    struct SlowServerTestAdaptor {
        // Nothing
    }

    impl Adaptor for SlowServerTestAdaptor {
        fn terminate(&self) {
            SLOW_PROC_TERMINATED.store(true, Ordering::Relaxed);
        }
    }

    impl<M> HyperServerAdaptor<M> for SlowServerTestAdaptor
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
            set_bound_port(proc.name(), addr.port());

            Ok(SlowServerTestAdaptor {})
        }

        async fn process_http_request(
            &self,
            _req: Request<hyper::body::Incoming>,
        ) -> HyperResp<Self, M> {
            let request_index = SLOW_REQUESTS_STARTED.fetch_add(1, Ordering::Relaxed);
            time::sleep(SLOW_RESPONSE_TIME * (request_index as u32 + 1)).await;

            <SlowServerTestAdaptor as HyperServerAdaptor<M>>::response_builder(self, StatusCode::OK)
                .body(BoxBody::new(Full::new(Bytes::from("Hello, slow world"))))
                .into()
        }
    }

    async fn run_test(
        settings: HttpTestSettings,
        certificate: Option<Certificate>,
        http2: bool,
        proc_name: &str,
    ) -> io::Result<()> {
        let url = settings.server.listener.url.clone();

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(1));

        // Launch the main task
        let main_task = main.run();

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            1,
            String::from(proc_name),
            bus.clone(),
            settings.server,
        );
        Proc::<ServerTestAdaptor>::run(http_server_proc)?;

        // The listener is on the port 0, the processor publishes where it bound
        let url = bound_url(proc_name, &url).await;

        // Send request to the server with reqwest
        let mut client_builder = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(WAIT_TIME.as_secs()))
            .use_rustls_tls();
        if let Some(cert) = certificate {
            client_builder = client_builder.tls_certs_only(vec![cert]);
        }
        if http2 {
            client_builder = client_builder.http2_prior_knowledge();
        }
        let client = client_builder
            .build()
            .expect("reqwest client should be valid");
        for _i in 0..20 {
            let resp = client
                .get(url.clone())
                .send()
                .await
                .expect("Failed to send request");
            assert_eq!(resp.status(), StatusCode::OK);
            assert!(resp.headers().get(hyper::header::SERVER).is_some_and(|h| {
                h.to_str().is_ok_and(|s| {
                    s.starts_with(concat!(
                        env!("CARGO_PKG_NAME"),
                        "/",
                        env!("CARGO_PKG_VERSION")
                    ))
                })
            }));
        }

        bus.stop("ProSA HTTP client server unit test end".into())
            .await
            .map_err(io::Error::other)?;

        // Wait on main task to end
        main_task.await;
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
        assert!(
            run_test(test_settings, None, false, "SRV_HTTP_PROC")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn https_client_server() {
        const PROSA_HTTPS_TEST_DIR_NAME: &str = "ProSA_server_HTTPS";
        let prosa_temp_dir = env::temp_dir().join(PROSA_HTTPS_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir)
            .expect("Can't create ProSA temporary directory for HTTPS");

        let key_path = prosa_temp_dir.join("prosa_server_https.key");
        let cert_path = prosa_temp_dir.join("prosa_server_https.pem");
        let cert_path_str = cert_path
            .as_os_str()
            .to_str()
            .expect("Cert path should be a valid String");
        let server_ssl_config = HttpTestSettings::create_server_cert(
            key_path
                .as_os_str()
                .to_str()
                .expect("Key path should be a valid String")
                .into(),
            cert_path_str.into(),
        )
        .expect("Server certificate should be created");

        let mut buf = Vec::new();
        File::open(cert_path_str)
            .expect("Cert file should exist")
            .read_to_end(&mut buf)
            .expect("Cert file should be read");
        let client_cert =
            reqwest::Certificate::from_pem(&buf).expect("Certificate should be valid for reqwest");

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

        let test_settings = HttpTestSettings::new(
            Url::parse("https://localhost:0").expect("HTTPS client/server URL should be valid"),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        assert!(
            run_test(test_settings, Some(client_cert), false, "SRV_HTTPS_PROC")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn h2_client_server() {
        const PROSA_H2_TEST_DIR_NAME: &str = "ProSA_server_H2";
        let prosa_temp_dir = env::temp_dir().join(PROSA_H2_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).expect("Can't create ProSA temporary directory for H2");

        let key_path = prosa_temp_dir.join("prosa_server_h2.key");
        let cert_path = prosa_temp_dir.join("prosa_server_h2.pem");
        let cert_path_str = cert_path
            .as_os_str()
            .to_str()
            .expect("Cert path should be a valid String");
        let server_ssl_config = HttpTestSettings::create_server_cert(
            key_path
                .as_os_str()
                .to_str()
                .expect("Key path should be a valid String")
                .into(),
            cert_path_str.into(),
        )
        .expect("Server certificate should be created");

        let mut buf = Vec::new();
        File::open(cert_path_str)
            .expect("Cert file should exist")
            .read_to_end(&mut buf)
            .expect("Cert file should be read");
        let client_cert =
            reqwest::Certificate::from_pem(&buf).expect("Certificate should be valid for reqwest");

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
            Url::parse("https://localhost:0").expect("HTTP2 client/server URL should be valid"),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        assert!(
            run_test(test_settings, Some(client_cert), true, "SRV_H2_PROC")
                .await
                .is_ok()
        );
    }

    /// Send a GET request through a UNIX socket, and return the status the server answered.
    ///
    /// A listener that binds without ever serving accepts connections all the same, so only a
    /// complete round trip tells that the processor rebound
    async fn unix_get(socket_path: &Path) -> Option<StatusCode> {
        let stream = tokio::net::UnixStream::connect(socket_path).await.ok()?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .ok()?;
        tokio::spawn(connection);

        let request = Request::builder()
            .uri("/")
            .header(hyper::header::HOST, "localhost")
            .body(Empty::<Bytes>::new())
            .ok()?;

        sender
            .send_request(request)
            .await
            .ok()
            .map(|resp| resp.status())
    }

    #[tokio::test]
    async fn server_config_reload() {
        const PROC_NAME: &str = "SRV_RELOAD_PROC";
        const PROSA_RELOAD_TEST_DIR_NAME: &str = "ProSA_server_reload";

        let settings = HttpTestSettings::new(
            Url::parse("http://127.0.0.1:0").expect("Initial server URL should be valid"),
            None,
            None,
        );
        let initial_url = settings.server.listener.url.clone();

        // A reload has to say where to go, and a port named before anything binds it is a port
        // another test can be given in the meantime. A UNIX socket is an address the test owns
        // outright, so the rebind lands where it is told
        let prosa_temp_dir = env::temp_dir().join(PROSA_RELOAD_TEST_DIR_NAME);
        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir)
            .expect("Can't create ProSA temporary directory for the configuration reload");
        let socket_path = prosa_temp_dir.join("prosa_server_reload.sock");
        let reloaded_url = Url::parse(&format!(
            "unix://{}",
            socket_path
                .to_str()
                .expect("Socket path should be a valid String")
        ))
        .expect("Reloaded server URL should be valid");

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(1));

        // The main task must run to broadcast the configuration to the processors
        let main_task = tokio::spawn(main.run());

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            1,
            String::from(PROC_NAME),
            bus.clone(),
            settings.server,
        );
        Proc::<ServerTestAdaptor>::run(http_server_proc)
            .expect("Hyper server processor should run");

        // The listener is on the port 0, the processor publishes where it bound
        let initial_url = bound_url(PROC_NAME, &initial_url).await;
        let initial_addr = format!(
            "127.0.0.1:{}",
            initial_url.port().expect("Initial URL should have a port")
        );

        // The connection is kept alive on purpose: it outlives the rebind, and must not keep the
        // retired listener bound
        let client = reqwest::ClientBuilder::new()
            .timeout(WAIT_TIME)
            .build()
            .expect("reqwest client should be valid");
        let resp = client
            .get(initial_url)
            .send()
            .await
            .expect("Failed to send request to the initial URL");
        assert_eq!(resp.status(), StatusCode::OK);

        // Move the listener to another address
        let config = ProsaConfig::from_config(
            config::Config::builder()
                .set_override(format!("{PROC_NAME}.listener.url"), reloaded_url.as_str())
                .expect("Reloaded listener URL should be a valid config override")
                .build()
                .expect("Reloaded configuration should be valid"),
        )
        .expect("Reloaded ProSA configuration should be valid");
        bus.update_config(Arc::new(config))
            .await
            .expect("ProSA configuration should be updated");

        assert!(
            wait_for(TEST_TIMEOUT, async || unix_get(&socket_path).await
                == Some(StatusCode::OK))
            .await,
            "The Hyper server processor should serve on {reloaded_url}"
        );

        assert!(
            wait_for(TEST_TIMEOUT, async || tokio::net::TcpListener::bind(
                initial_addr.as_str()
            )
            .await
            .is_ok())
            .await,
            "The initial listener should have been freed by the reload"
        );

        bus.stop("ProSA HTTP server configuration reload unit test end".into())
            .await
            .expect("ProSA should stop");

        // Wait on main task to end
        let _ = main_task.await;
    }

    /// Open a TLS connection and give back the certificate the server presented.
    ///
    /// Done with openssl rather than `reqwest`, which verifies the certificate and then keeps it to
    /// itself. Blocking, so it runs off the test runtime
    async fn served_certificate(addr: String) -> Option<Vec<u8>> {
        tokio::task::spawn_blocking(move || {
            let mut connector = openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls())
                .expect("The test TLS connector should build");
            // Which certificate is served is the whole point, whether it is trusted isn't
            connector.set_verify(openssl::ssl::SslVerifyMode::NONE);

            let stream = std::net::TcpStream::connect(&addr).ok()?;
            let stream = connector.build().connect("localhost", stream).ok()?;
            let certificate = stream.ssl().peer_certificate()?;
            certificate.to_pem().ok()
        })
        .await
        .ok()
        .flatten()
    }

    #[tokio::test]
    async fn server_certificate_reload() {
        const PROC_NAME: &str = "SRV_CERT_RELOAD_PROC";
        const PROSA_CERT_RELOAD_TEST_DIR_NAME: &str = "ProSA_server_cert_reload";

        let prosa_temp_dir = env::temp_dir().join(PROSA_CERT_RELOAD_TEST_DIR_NAME);
        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir)
            .expect("Can't create ProSA temporary directory for the certificate reload");

        let key_path = prosa_temp_dir.join("prosa_server_cert_reload.key");
        let cert_path = prosa_temp_dir.join("prosa_server_cert_reload.pem");
        let key_path = key_path
            .to_str()
            .expect("Key path should be a valid String")
            .to_string();
        let cert_path = cert_path
            .to_str()
            .expect("Cert path should be a valid String")
            .to_string();

        let server_ssl_config =
            HttpTestSettings::create_server_cert(key_path.clone(), cert_path.clone())
                .expect("Server certificate should be created");

        let settings = HttpTestSettings::new(
            Url::parse("https://localhost:0").expect("Certificate reload URL should be valid"),
            Some(server_ssl_config),
            None,
        );
        let initial_url = settings.server.listener.url.clone();

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(1));

        // The main task must run to broadcast the configuration to the processors
        let main_task = tokio::spawn(main.run());

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            1,
            String::from(PROC_NAME),
            bus.clone(),
            settings.server,
        );
        Proc::<ServerTestAdaptor>::run(http_server_proc)
            .expect("Hyper server processor should run");

        // The listener is on the port 0, the processor publishes where it bound
        let bound = bound_url(PROC_NAME, &initial_url).await;
        let addr = format!(
            "localhost:{}",
            bound.port().expect("Bound URL should have a port")
        );

        let first_certificate = served_certificate(addr.clone())
            .await
            .expect("The server should present a certificate");

        // Renew it under the very same paths, which is what a certificate manager does. The
        // configuration is left describing exactly what it described before
        HttpTestSettings::create_server_cert(key_path.clone(), cert_path.clone())
            .expect("Server certificate should be renewed");

        // Reload with settings equal to the running ones, down to the URL: the port is still the 0
        // the processor was given, so a rebind would land on another port and the address below
        // would stop answering altogether
        let reloaded = format!(
            "{PROC_NAME}:\n  listener:\n    url: {}\n    ssl:\n      cert: {cert_path}\n      key: {key_path}\n      passphrase: {}\n",
            initial_url.as_str(),
            HttpTestSettings::PASSPHRASE,
        );
        let config = ProsaConfig::from_config(
            config::Config::builder()
                .add_source(config::File::from_str(&reloaded, config::FileFormat::Yaml))
                .build()
                .expect("Reloaded configuration should be valid"),
        )
        .expect("Reloaded ProSA configuration should be valid");
        bus.update_config(Arc::new(config))
            .await
            .expect("ProSA configuration should be updated");

        // The new certificate is served, on the socket that was never rebound
        assert!(
            wait_for(TEST_TIMEOUT, async || {
                served_certificate(addr.clone())
                    .await
                    .is_some_and(|certificate| certificate != first_certificate)
            })
            .await,
            "The Hyper server processor should serve the renewed certificate on {addr}"
        );

        bus.stop("ProSA HTTP server certificate reload unit test end".into())
            .await
            .expect("ProSA should stop");

        // Wait on main task to end
        let _ = main_task.await;
    }

    #[tokio::test]
    async fn server_graceful_shutdown() {
        const PROC_NAME: &str = "SRV_SHUTDOWN_PROC";

        let settings = HttpTestSettings::new(
            Url::parse("http://127.0.0.1:0").expect("Graceful shutdown server URL should be valid"),
            None,
            None,
        );
        let url = settings.server.listener.url.clone();

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(1));
        let main_task = tokio::spawn(main.run());

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            1,
            String::from(PROC_NAME),
            bus.clone(),
            settings.server,
        );
        Proc::<SlowServerTestAdaptor>::run(http_server_proc)
            .expect("Hyper server processor should run");

        // The listener is on the port 0, the processor publishes where it bound
        let url = bound_url(PROC_NAME, &url).await;
        let addr = format!(
            "127.0.0.1:{}",
            url.port().expect("Bound URL should have a port")
        );

        // Fire a request on each of several connections and leave them all in flight. They are
        // answered at different moments, so draining until the first one is done is not enough
        const IN_FLIGHT_CONNECTIONS: usize = 3;
        let mut in_flight = Vec::with_capacity(IN_FLIGHT_CONNECTIONS);
        for _ in 0..IN_FLIGHT_CONNECTIONS {
            let stream = tokio::net::TcpStream::connect(&addr)
                .await
                .expect("The Hyper server processor should accept a connection");
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .expect("The HTTP/1.1 handshake should succeed");
            tokio::spawn(connection);
            let request = Request::builder()
                .uri("/")
                .header(hyper::header::HOST, "localhost")
                .body(Empty::<Bytes>::new())
                .expect("The request should be valid");
            in_flight.push(tokio::spawn(
                async move { sender.send_request(request).await },
            ));
        }

        assert!(
            wait_for(TEST_TIMEOUT, async || SLOW_REQUESTS_STARTED
                .load(Ordering::Relaxed)
                >= IN_FLIGHT_CONNECTIONS)
            .await,
            "The Hyper server processor should be serving every request"
        );

        bus.stop("ProSA HTTP server graceful shutdown unit test end".into())
            .await
            .expect("ProSA should stop");

        // The listener is released as the drain starts, so a new client is told right away instead
        // of waiting in the accept queue of a server that will never take it
        assert!(
            wait_for(TEST_TIMEOUT, async || tokio::net::TcpStream::connect(&addr)
                .await
                .is_err())
            .await,
            "A new connection should be refused once the processor stops accepting"
        );

        // Every connection is answered, not just the one that finished first
        for request in in_flight {
            let resp = request
                .await
                .expect("The in flight request should not be dropped")
                .expect("The Hyper server processor should answer the request it was serving");
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // The drain completing is what ends the loop, so the processor only terminates once it has
        // nothing left to serve
        assert!(
            wait_for(TEST_TIMEOUT, async || SLOW_PROC_TERMINATED
                .load(Ordering::Relaxed))
            .await,
            "The Hyper server processor should terminate once its connections are drained"
        );

        // Wait on main task to end
        let _ = main_task.await;
    }
}
