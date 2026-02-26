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
use std::{fs::File, io::Write as _};

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
    /// Method to create private key and certificate for a server
    pub(crate) fn create_server_cert(
        key_path: String,
        cert_path: String,
    ) -> Result<SslConfig, ConfigError> {
        const PASSPHRASE: &str = "prosa_test";

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

        let begin_valid_time =
            Asn1Time::from_unix(std::time::UNIX_EPOCH.elapsed().unwrap().as_secs() as i64 - 360)?;
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
            proc::{Proc, ProcConfig as _},
            service::ServiceError,
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
        env, fs,
        sync::atomic::{AtomicU32, Ordering},
    };
    use tokio::{runtime, time};
    use url::Url;

    use crate::{
        HyperResp,
        client::{adaptor::HyperClientAdaptor, proc::HyperClientProc},
        server::{adaptor::HyperServerAdaptor, proc::HyperServerProc},
        tests::HttpTestSettings,
    };

    const WAIT_TIME: time::Duration = time::Duration::from_secs(1);

    static COUNTER: [AtomicU32; 3] = [
        AtomicU32::new(0), // HTTP
        AtomicU32::new(0), // HTTPS
        AtomicU32::new(0), // HTTP/2
    ];

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
                    if !content.starts_with("Hello") || !user_agent.starts_with("ProSA-Hyper/") {
                        return Err(ServiceError::ProtocolError(
                            "Invalid request content".into(),
                        ))
                        .into();
                    }
                }
                Err(e) => return Err(ServiceError::ProtocolError(e.to_string())).into(),
            }

            let mut srv_req = M::default();
            srv_req.put_string(1, "Hello from the stub!");
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
        ) -> Result<Self, Box<dyn ProcError + Send + Sync>>
        where
            Self: Sized,
        {
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

        async fn process_http_request(&self, req: Request<hyper::body::Incoming>) -> HyperResp<M> {
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

            HyperResp::SrvReq("STUB_HTTP_SRV".into(), srv_req)
        }

        fn process_srv_response(
            &self,
            resp: M,
        ) -> hyper::Response<
            http_body_util::combinators::BoxBody<bytes::Bytes, std::convert::Infallible>,
        > {
            if let Ok(content) = resp.get_string(1) {
                Response::builder()
                    .header(
                        hyper::header::SERVER,
                        <TestAdaptor as HyperServerAdaptor<M>>::SERVER_HEADER,
                    )
                    .status(StatusCode::OK)
                    .body(BoxBody::new(Full::new(Bytes::from_owner(
                        content.into_owned(),
                    ))))
                    .unwrap()
            } else {
                Response::builder()
                    .header(
                        hyper::header::SERVER,
                        <TestAdaptor as HyperServerAdaptor<M>>::SERVER_HEADER,
                    )
                    .status(StatusCode::BAD_REQUEST)
                    .body(BoxBody::new(Full::new(Bytes::from("Bad Request"))))
                    .unwrap()
            }
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
            let test_type = match proc.settings.backends.first().map(|b| b.url.scheme()) {
                Some("http") => 0,
                Some("https") => 1,
                Some("h2") => 2,
                _ => {
                    return Err(Box::new(ConfigError::WrongValue(
                        "HyperClientSettings::scheme".into(),
                        "Unsupported scheme".into(),
                    )));
                }
            };

            Ok(TestAdaptor { test_type })
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
                    .header(
                        hyper::header::USER_AGENT,
                        <TestAdaptor as HyperClientAdaptor<M>>::USER_AGENT_HEADER,
                    )
                    .body(BoxBody::new(Full::new(Bytes::from(body.into_owned()))))
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
            let http_body =
                resp.map_err(|e| ServiceError::ProtocolError(format!("HTTP error: {}", e)))?;
            if let Ok(body) = http_body.into_body().collect().await
                && let Ok(body_str) = String::from_utf8(body.to_bytes().to_vec())
            {
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

    async fn run_test(settings: HttpTestSettings, test_type: u64) {
        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(4));

        // Launch the main task in a separate thread to run the ProSA runtime
        let main_handle = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("main")
                    .build()
                    .unwrap()
                    .block_on(async {
                        main.run().await;
                    })
            })
            .unwrap();

        // Launch stub to respond to the HTTP server
        let http_server_stub = StubProc::<SimpleStringTvf>::create(
            1,
            String::from("HTTP_SERVER_STUB"),
            bus.clone(),
            settings.stub,
        );
        Proc::<TestAdaptor>::run(http_server_stub);

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            2,
            String::from("HTTP_SERVER_PROC"),
            bus.clone(),
            settings.server,
        );
        Proc::<TestAdaptor>::run(http_server_proc);

        // Wait for processor to start
        std::thread::sleep(WAIT_TIME);

        // Launch an HTTP client processor
        let http_client_proc = HyperClientProc::<SimpleStringTvf>::create(
            3,
            String::from("HTTP_CLIENT_PROC"),
            bus.clone(),
            settings.client,
        );
        Proc::<TestAdaptor>::run(http_client_proc);

        // Launch an HTTP injector processor
        let http_inj_proc = InjProc::<SimpleStringTvf>::create(
            4,
            String::from("HTTP_INJ_PROC"),
            bus.clone(),
            settings.inj,
        );
        Proc::<TestAdaptor>::run(http_inj_proc);

        // Wait for processor to finish processing
        std::thread::sleep(WAIT_TIME);

        bus.stop("ProSA HTTP client server unit test end".into())
            .await
            .unwrap();

        assert!(
            COUNTER[test_type as usize].load(Ordering::SeqCst) > 0,
            "No response received for test type {}",
            test_type
        );

        // Wait on main task to end
        let _ = main_handle.join();
    }

    #[tokio::test]
    async fn http_client_server() {
        let test_settings =
            HttpTestSettings::new(Url::parse("http://localhost:48080").unwrap(), None, None);

        // Run a ProSA to test
        run_test(test_settings, 0).await;
    }

    #[tokio::test]
    async fn https_client_server() {
        const PROSA_HTTPS_TEST_DIR_NAME: &str = "ProSA_HTTPS";
        let prosa_temp_dir = env::temp_dir().join(PROSA_HTTPS_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).unwrap();

        let key_path = prosa_temp_dir.join("prosa_https.key");
        let cert_path = prosa_temp_dir.join("prosa_https.pem");
        let server_ssl_config = HttpTestSettings::create_server_cert(
            key_path.as_os_str().to_str().unwrap().into(),
            cert_path.as_os_str().to_str().unwrap().into(),
        )
        .unwrap();

        let client_ssl_store = Store::File {
            path: prosa_temp_dir.as_os_str().to_str().unwrap().into(),
        };
        let mut client_ssl_config = SslConfig::default();
        client_ssl_config.set_store(client_ssl_store);

        let test_settings = HttpTestSettings::new(
            Url::parse("https://localhost:48443").unwrap(),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        run_test(test_settings, 1).await;
    }

    #[tokio::test]
    async fn h2_client_server() {
        const PROSA_H2_TEST_DIR_NAME: &str = "ProSA_H2";
        let prosa_temp_dir = env::temp_dir().join(PROSA_H2_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).unwrap();

        let key_path = prosa_temp_dir.join("prosa_h2.key");
        let cert_path = prosa_temp_dir.join("prosa_h2.pem");
        let mut server_ssl_config = HttpTestSettings::create_server_cert(
            key_path.as_os_str().to_str().unwrap().into(),
            cert_path.as_os_str().to_str().unwrap().into(),
        )
        .unwrap();
        // Need to set the ALPN for server because of inline configuration @see TargetSetting::new
        server_ssl_config.set_alpn(vec!["h2".into()]);

        let client_ssl_store = Store::File {
            path: format!("{}/", prosa_temp_dir.as_os_str().to_str().unwrap()),
        };
        let mut client_ssl_config = SslConfig::default();
        client_ssl_config.set_store(client_ssl_store);
        client_ssl_config.set_alpn(vec!["h2".into()]);

        let test_settings = HttpTestSettings::new(
            Url::parse("h2://localhost:49443").unwrap(),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        run_test(test_settings, 2).await;
    }
}
