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
    use http_body_util::{Full, combinators::BoxBody};
    use hyper::{Request, Response, StatusCode};
    use prosa::core::{
        adaptor::Adaptor,
        error::ProcError,
        main::{MainProc, MainRunnable as _},
        proc::{Proc, ProcConfig as _},
    };
    use prosa_utils::{
        config::ssl::{SslConfig, Store},
        msg::simple_string_tvf::SimpleStringTvf,
    };
    use reqwest::Certificate;
    use std::{
        env,
        fs::{self, File},
        io::Read as _,
        time::Duration,
    };
    use tokio::time;
    use url::Url;

    use crate::{
        HyperResp,
        server::{adaptor::HyperServerAdaptor, proc::HyperServerProc},
        tests::HttpTestSettings,
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
            _proc: &crate::server::proc::HyperServerProc<M>,
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
            _resp: M,
        ) -> hyper::Response<
            http_body_util::combinators::BoxBody<bytes::Bytes, std::convert::Infallible>,
        > {
            unimplemented!()
        }
    }

    async fn run_test(settings: HttpTestSettings, certificate: Option<Certificate>, http2: bool) {
        let url = settings.server.listener.url.clone();

        // Create bus and main processor
        let (bus, main) = MainProc::<SimpleStringTvf>::create(&settings, Some(1));

        // Launch the main task
        let main_task = main.run();

        // Launch an HTTP server processor
        let http_server_proc = HyperServerProc::<SimpleStringTvf>::create(
            1,
            String::from("HTTP_SERVER_PROC"),
            bus.clone(),
            settings.server,
        );
        Proc::<ServerTestAdaptor>::run(http_server_proc);

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
            HttpTestSettings::new(Url::parse("http://localhost:48180").unwrap(), None, None);

        // Run a ProSA to test
        run_test(test_settings, None, false).await;
    }

    #[tokio::test]
    async fn https_client_server() {
        const PROSA_HTTPS_TEST_DIR_NAME: &str = "ProSA_server_HTTPS";
        let prosa_temp_dir = env::temp_dir().join(PROSA_HTTPS_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).unwrap();

        let key_path = prosa_temp_dir.join("prosa_server_https.key");
        let cert_path = prosa_temp_dir.join("prosa_server_https.pem");
        let server_ssl_config = HttpTestSettings::create_server_cert(
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
            Url::parse("https://localhost:48543").unwrap(),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        run_test(test_settings, Some(client_cert), false).await;
    }

    #[tokio::test]
    async fn h2_client_server() {
        const PROSA_H2_TEST_DIR_NAME: &str = "ProSA_server_H2";
        let prosa_temp_dir = env::temp_dir().join(PROSA_H2_TEST_DIR_NAME);

        let _ = fs::remove_dir_all(&prosa_temp_dir);
        fs::create_dir_all(&prosa_temp_dir).unwrap();

        let key_path = prosa_temp_dir.join("prosa_server_h2.key");
        let cert_path = prosa_temp_dir.join("prosa_server_h2.pem");
        let mut server_ssl_config = HttpTestSettings::create_server_cert(
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

        let client_ssl_store = Store::File {
            path: format!("{}/", prosa_temp_dir.as_os_str().to_str().unwrap()),
        };
        let mut client_ssl_config = SslConfig::default();
        client_ssl_config.set_store(client_ssl_store);
        client_ssl_config.set_alpn(vec!["h2".into()]);

        let test_settings = HttpTestSettings::new(
            Url::parse("https://localhost:49543").unwrap(),
            Some(server_ssl_config),
            Some(client_ssl_config),
        );

        // Run a ProSA to test
        run_test(test_settings, Some(client_cert), true).await;
    }
}
