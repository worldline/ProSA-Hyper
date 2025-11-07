use std::convert::Infallible;
use std::env;

use bytes::Bytes;
use clap::{ArgAction, Command, arg};
use config::Config;
use http_body_util::Empty;
use http_body_util::combinators::BoxBody;
use hyper::{Request, Response};
use prosa::core::adaptor::Adaptor;
use prosa::core::error::ProcError;
use prosa::core::main::MainProc;
use prosa::core::main::MainRunnable as _;
use prosa::core::proc::{Proc, ProcBusParam, ProcConfig};
use prosa::core::service::ServiceError;
use prosa::core::settings::settings;
use prosa::inj::adaptor::InjAdaptor;
use prosa::inj::proc::{InjProc, InjSettings};
use prosa_hyper::client::adaptor::HyperClientAdaptor;
use prosa_hyper::client::proc::{HyperClientProc, HyperClientSettings};
use prosa_utils::config::tracing::TelemetryFilter;
use prosa_utils::msg::simple_string_tvf::SimpleStringTvf;
use serde::{Deserialize, Serialize};
use tracing::debug;
use url::Url;

/// Demo Hyper processor adaptor
#[derive(Debug, Adaptor, Clone)]
pub struct HyperDemoAdaptor {
    prosa_name: String,
}

impl<M> HyperClientAdaptor<M> for HyperDemoAdaptor
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
        _proc: &prosa_hyper::client::proc::HyperClientProc<M>,
        prosa_name: &str,
    ) -> Result<Self, Box<dyn ProcError + Send + Sync>>
    where
        Self: Sized,
    {
        Ok(Self {
            prosa_name: prosa_name.to_string(),
        })
    }

    fn process_srv_request(
        &self,
        request: M,
        socket_url: &Url,
    ) -> Result<Request<BoxBody<Bytes, Infallible>>, prosa::core::service::ServiceError> {
        let path = request
            .get_string(1)
            .map_err(|e| ServiceError::ProtocolError(e.to_string()))?;
        let mut uri = socket_url.clone();
        uri.set_path(path.as_str());

        Request::builder()
            .uri(uri.as_str())
            .header(
                "User-Agent",
                <HyperDemoAdaptor as HyperClientAdaptor<M>>::USER_AGENT_HEADER,
            )
            .body(BoxBody::new(Empty::<Bytes>::new()))
            .map_err(|e| ServiceError::ProtocolError(format!("Failed to build request: {}", e)))
    }

    fn process_http_response(
        &self,
        resp: Result<Response<hyper::body::Incoming>, hyper::Error>,
    ) -> Result<M, prosa::core::service::ServiceError> {
        match resp {
            Ok(response) => {
                debug!(
                    name = self.prosa_name,
                    "Received response with status: {}",
                    response.status()
                );
                let mut tvf_resp = M::default();
                tvf_resp.put_string(1, response.status().to_string());
                Ok(tvf_resp)
            }
            Err(e) => {
                debug!(name = self.prosa_name, "Error receiving response: {}", e);
                Err(ServiceError::ProtocolError(format!("HTTP error: {}", e)))
            }
        }
    }
}

impl<M> InjAdaptor<M> for HyperDemoAdaptor
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
    fn new(proc: &InjProc<M>) -> Result<Self, Box<dyn ProcError + Send + Sync>> {
        Ok(Self {
            prosa_name: proc.name().to_string(),
        })
    }

    fn build_transaction(&mut self) -> M {
        let mut msg = M::default();
        msg.put_string(1, "/");
        msg
    }
}

#[settings]
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub(crate) struct MainHyperSettings {
    pub(crate) hyper_client: HyperClientSettings,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("hyper")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(arg!(-v - -verbose).action(ArgAction::SetTrue))
        .arg(arg!(-i --inj "Start a Inj processor").action(ArgAction::SetTrue))
        .arg(
            arg!(-c --config <CONFIG_PATH> "Path of the Hyper ProSA server configuration file")
                .default_value("examples/config.yml"),
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
    let service_name = prosa_hyper_settings.hyper_client.service_name.clone();

    println!("ProSA HYPER settings: {prosa_hyper_settings:?}");

    let filter = TelemetryFilter::default();
    prosa_hyper_settings.observability.tracing_init(&filter)?;

    // Create bus and main processor
    let (bus, main) = MainProc::<SimpleStringTvf>::create(&prosa_hyper_settings);

    // Launch the main task
    debug!("Launch the main task");
    let main_task = main.run();

    debug!("Start the Hyper processor");
    let http_proc = HyperClientProc::<SimpleStringTvf>::create(
        1,
        bus.clone(),
        prosa_hyper_settings.hyper_client,
    );
    Proc::<HyperDemoAdaptor>::run(http_proc, String::from("hyper_client"));

    if matches.contains_id("inj") && matches.get_flag("inj") {
        debug!("Start a Inj processor");
        let inj_settings = InjSettings::new(service_name);
        let inj_proc = InjProc::<SimpleStringTvf>::create(2, bus.clone(), inj_settings);
        Proc::<HyperDemoAdaptor>::run(inj_proc, String::from("INJ_PROC"));
    }

    // Wait on main task
    main_task.await;
    Ok(())
}
