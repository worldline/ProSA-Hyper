//! Module to handle HTTP server

use std::time::{Duration, SystemTime};

use prosa::core::msg::{InternalMsg, Msg};

use tokio::sync::oneshot;

use tracing::{Level, Span, span};

/// Adaptor for Hyper server processor
pub mod adaptor;
/// ProSA Hyper server processor
pub mod proc;
/// Hyper service definition
pub(crate) mod service;

#[cfg(feature = "mcp-server")]
/// ProSA MCP server module
pub mod mcp;

/// Hyper processor
#[derive(Debug)]
pub struct HyperProcMsg<M>
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
    id: u64,
    span: Span,
    service: String,
    data: M,
    begin_time: SystemTime,
    response_queue: oneshot::Sender<InternalMsg<M>>,
}

impl<M> HyperProcMsg<M>
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
    /// Create a new Hyper processor message
    pub fn new(
        service: String,
        data: M,
        response_queue: oneshot::Sender<InternalMsg<M>>,
    ) -> HyperProcMsg<M> {
        let span = span!(Level::INFO, "HyperProcMsg", service = service);
        HyperProcMsg {
            id: 0,
            service,
            span,
            data,
            begin_time: SystemTime::now(),
            response_queue,
        }
    }

    /// Set the ID of the Hyper processor
    pub fn set_id(&mut self, id: u64) {
        self.id = id;
    }
}

impl<M> Msg<M> for HyperProcMsg<M>
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
    fn get_id(&self) -> u64 {
        self.id
    }

    fn get_service(&self) -> &String {
        &self.service
    }

    fn get_span(&self) -> &Span {
        &self.span
    }

    fn get_span_mut(&mut self) -> &mut Span {
        &mut self.span
    }

    fn enter_span(&self) -> span::Entered<'_> {
        self.span.enter()
    }

    fn get_data(&self) -> &M {
        &self.data
    }

    fn get_data_mut(&mut self) -> &mut M {
        &mut self.data
    }

    fn elapsed(&self) -> Duration {
        self.begin_time.elapsed().unwrap_or(Duration::new(0, 0))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http_body_util::{Full, combinators::BoxBody};
    use hyper::{Request, Response, StatusCode};
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
    use prosa::{
        core::{
            adaptor::Adaptor,
            error::ProcError,
            main::{MainProc, MainRunnable as _},
            proc::{Proc, ProcConfig as _},
            settings::settings,
        },
        io::listener::ListenerSetting,
    };
    use prosa_utils::{
        config::{
            ConfigError, os_country,
            ssl::{SslConfig, Store},
        },
        msg::simple_string_tvf::SimpleStringTvf,
    };
    use reqwest::Certificate;
    use serde::Serialize;
    use std::{
        env,
        fs::{self, File},
        io::{Read as _, Write as _},
        time::Duration,
    };
    use tokio::time;
    use url::Url;

    use crate::{
        HyperResp,
        server::{
            adaptor::HyperServerAdaptor,
            proc::{HyperServerProc, HyperServerSettings},
        },
    };

    const WAIT_TIME: time::Duration = time::Duration::from_secs(5);

    /// HTTP settings
    #[settings]
    #[derive(Default, Debug, Serialize)]
    struct HttpTestSettings {
        server: HyperServerSettings,
    }

    impl HttpTestSettings {
        fn new(url: Url, server_ssl: Option<SslConfig>) -> Self {
            let server = HyperServerSettings::new(
                ListenerSetting::new(url.clone(), server_ssl),
                Duration::from_secs(1),
            );
            HttpTestSettings {
                server,
                ..Default::default()
            }
        }
    }

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
            _proc: &crate::server::proc::HyperServerProc<M>,
            _prosa_name: &str,
        ) -> Result<Self, Box<dyn ProcError + Send + Sync>>
        where
            Self: Sized,
        {
            Ok(ServerTestAdaptor {})
        }

        async fn process_http_request(&self, req: Request<hyper::body::Incoming>) -> HyperResp<M> {
            let resp_msg = if req.version() == hyper::Version::HTTP_2 {
                "Hello, H2 world"
            } else {
                "Hello, world"
            };
            let response = Response::builder()
                .header(
                    hyper::header::SERVER,
                    <ServerTestAdaptor as HyperServerAdaptor<M>>::SERVER_HEADER,
                )
                .status(StatusCode::OK)
                .body(BoxBody::new(Full::new(Bytes::from(resp_msg))))
                .unwrap();

            HyperResp::HttpResp(response)
        }

        fn process_srv_response(
            &self,
            _resp: &M,
        ) -> hyper::Response<
            http_body_util::combinators::BoxBody<bytes::Bytes, std::convert::Infallible>,
        > {
            unimplemented!()
        }
    }

    async fn run_test(settings: HttpTestSettings, certificate: Option<Certificate>, http2: bool) {
        let url = settings.server.listener.url.clone();

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings);

        // Launch the main task
        let main_task = main.run();

        // Launch an HTTP server processor
        let http_server_proc =
            HyperServerProc::<SimpleStringTvf>::create(1, bus.clone(), settings.server);
        Proc::<ServerTestAdaptor>::run(http_server_proc, String::from("HTTP_SERVER_PROC"));

        // Wait for processor to start
        std::thread::sleep(Duration::from_secs(1));

        // Send request to the server with reqwest
        let mut client_builder = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(WAIT_TIME.as_secs()))
            .use_rustls_tls();
        if let Some(cert) = certificate {
            client_builder = client_builder.add_root_certificate(cert);
        }
        if http2 {
            client_builder = client_builder.http2_prior_knowledge();
        }
        let client = client_builder.build().unwrap();
        for _i in 0..20 {
            let resp = client
                .get(url.clone())
                .send()
                .await
                .expect("Failed to send request");
            assert_eq!(resp.status(), StatusCode::OK);
            let server_header = resp.headers().get(hyper::header::SERVER).unwrap();
            assert!(server_header.to_str().unwrap().starts_with("ProSA-Hyper/"));
        }

        bus.stop("ProSA HTTP client server unit test end".into())
            .await
            .unwrap();

        // Wait on main task to end
        main_task.await;
    }

    #[tokio::test]
    async fn http_client_server() {
        let test_settings =
            HttpTestSettings::new(Url::parse("http://localhost:48080").unwrap(), None);

        // Run a ProSA to test
        run_test(test_settings, None, false).await;
    }

    /// Method to create private key and certificate for a server
    fn create_server_cert(key_path: String, cert_path: String) -> Result<SslConfig, ConfigError> {
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

    #[tokio::test]
    async fn https_client_server() {
        const PROSA_HTTPS_TEST_DIR_NAME: &str = "ProSA_HTTPS";
        let prosa_temp_dir = env::temp_dir().join(PROSA_HTTPS_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).unwrap();

        let key_path = prosa_temp_dir.join("prosa_https.key");
        let cert_path = prosa_temp_dir.join("prosa_https.pem");
        let server_ssl_config = create_server_cert(
            key_path.as_os_str().to_str().unwrap().into(),
            cert_path.as_os_str().to_str().unwrap().into(),
        )
        .unwrap();

        let mut buf = Vec::new();
        File::open(cert_path.as_os_str().to_str().unwrap())
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        let client_cert = reqwest::Certificate::from_pem(&buf).unwrap();

        let client_ssl_store = Store::File {
            path: format!("{}/", prosa_temp_dir.as_os_str().to_str().unwrap()),
        };
        let mut client_ssl_config = SslConfig::default();
        client_ssl_config.set_store(client_ssl_store);

        let test_settings = HttpTestSettings::new(
            Url::parse("https://localhost:48443").unwrap(),
            Some(server_ssl_config),
        );

        // Run a ProSA to test
        run_test(test_settings, Some(client_cert), false).await;
    }

    #[tokio::test]
    async fn h2_client_server() {
        const PROSA_H2_TEST_DIR_NAME: &str = "ProSA_H2";
        let prosa_temp_dir = env::temp_dir().join(PROSA_H2_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).unwrap();

        let key_path = prosa_temp_dir.join("prosa_h2.key");
        let cert_path = prosa_temp_dir.join("prosa_h2.pem");
        let mut server_ssl_config = create_server_cert(
            key_path.as_os_str().to_str().unwrap().into(),
            cert_path.as_os_str().to_str().unwrap().into(),
        )
        .unwrap();
        // Need to set the ALPN for server because of inline configuration @see TargetSetting::new
        server_ssl_config.set_alpn(vec!["h2".into()]);

        let mut buf = Vec::new();
        File::open(cert_path.as_os_str().to_str().unwrap())
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        let client_cert = reqwest::Certificate::from_pem(&buf).unwrap();

        let test_settings = HttpTestSettings::new(
            Url::parse("https://localhost:49443").unwrap(),
            Some(server_ssl_config),
        );

        // Run a ProSA to test
        run_test(test_settings, Some(client_cert), true).await;
    }
}
