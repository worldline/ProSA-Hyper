use std::borrow::Cow;
use std::convert::Infallible;
use std::env;

use bytes::Bytes;
use clap::{ArgAction, Command, arg};
use config::Config;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, Response};
use prosa::core::adaptor::Adaptor;
use prosa::core::error::ProcError;
use prosa::core::main::MainRunnable as _;
use prosa::core::proc::{Proc, ProcConfig};
use prosa::core::settings::settings;
use prosa::stub::adaptor::StubParotAdaptor;
use prosa::stub::proc::StubSettings;
use prosa::{core::main::MainProc, stub::proc::StubProc};
use prosa_hyper::HyperResp;
use prosa_hyper::server::adaptor::HyperServerAdaptor;
use prosa_hyper::server::proc::{HyperServerProc, HyperServerSettings};
use prosa_utils::config::tracing::TelemetryFilter;
use prosa_utils::msg::simple_string_tvf::SimpleStringTvf;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Demo Hyper processor adaptor
#[derive(Debug, Adaptor, Clone)]
pub struct HyperDemoAdaptor {
    prosa_name: String,
}

impl<M> HyperServerAdaptor<M> for HyperDemoAdaptor
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
        _proc: &HyperServerProc<M>,
        prosa_name: &str,
    ) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
        Ok(HyperDemoAdaptor {
            prosa_name: prosa_name.into(),
        })
    }

    async fn process_http_request(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> crate::HyperResp<M> {
        match req.uri().path() {
            "/" => HyperResp::HttpResp(
                Response::builder()
                    .body(BoxBody::new(Full::new(Bytes::from(format!(
                        "{} - Home of {}",
                        if req.version() == hyper::Version::HTTP_2 {
                            "H2"
                        } else {
                            "HTTP/1.1"
                        },
                        self.prosa_name,
                    )))))
                    .unwrap(),
            ),
            "/test" => {
                let mut tvf_req = M::default();
                tvf_req.put_string(1, req.method().to_string());
                tvf_req.put_string(2, "/test");

                if let Ok(_body) = req.collect().await.map(|b| b.aggregate()) {
                    // should work
                }

                HyperResp::SrvReq(String::from("SRV_TEST"), tvf_req)
            }
            _ => HyperResp::HttpResp(
                Response::builder()
                    .status(404)
                    .body(BoxBody::new(Full::new(Bytes::from("Not Found"))))
                    .unwrap(),
            ),
        }
    }

    fn process_srv_response(&self, resp: &M) -> Response<BoxBody<Bytes, Infallible>> {
        let body = resp
            .get_string(10)
            .unwrap_or(Cow::Owned(String::from("empty body")));
        Response::builder()
            .body(BoxBody::new(Full::new(Bytes::from(format!(
                "Body: {body}\nTvfResp: {resp:?}"
            )))))
            .unwrap()
    }
}

#[settings]
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub(crate) struct MainHyperSettings {
    pub(crate) hyper: HyperServerSettings,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(all(debug_assertions, feature = "subsecond"))]
    dioxus_devtools::connect_subsecond();

    let matches = Command::new("hyper")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(arg!(-v - -verbose).action(ArgAction::SetTrue))
        .arg(arg!(-s --stub "Start a Stub processor").action(ArgAction::SetTrue))
        .arg(
            arg!(-c --config <CONFIG_PATH> "Path of the Hyper ProSA server configuration file")
                .default_value("examples/server.yml"),
        )
        .get_matches();

    // load the configuration
    let config = Config::builder()
        .add_source(config::File::with_name(
            matches.get_one::<String>("config").unwrap().as_str(),
        ))
        .add_source(
            config::Environment::with_prefix("PROSA")
                .try_parsing(true)
                .separator("_")
                .list_separator(" "),
        )
        .build()
        .unwrap();

    let prosa_hyper_settings = config.try_deserialize::<MainHyperSettings>()?;

    println!("ProSA HYPER settings: {prosa_hyper_settings:?}");

    let filter = TelemetryFilter::default();
    prosa_hyper_settings.observability.tracing_init(&filter)?;

    // Create bus and main processor
    let (bus, main) = MainProc::<SimpleStringTvf>::create(&prosa_hyper_settings);

    // Launch the main task
    debug!("Launch the main task");
    let main_task = main.run();

    debug!("Start the Hyper processor");
    let http_proc =
        HyperServerProc::<SimpleStringTvf>::create(1, bus.clone(), prosa_hyper_settings.hyper);
    Proc::<HyperDemoAdaptor>::run(http_proc, String::from("hyper"));

    if matches.contains_id("stub") && matches.get_flag("stub") {
        debug!("Start a Stub processor");
        let stub_settings = StubSettings::new(vec![String::from("SRV_TEST")]);
        let stub_proc = StubProc::<SimpleStringTvf>::create(2, bus.clone(), stub_settings);
        Proc::<StubParotAdaptor>::run(stub_proc, String::from("STUB_PROC"));
    }

    // Wait on main task
    main_task.await;
    opentelemetry::global::shutdown_tracer_provider();
    Ok(())
}
