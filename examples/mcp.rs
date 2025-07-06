//! MCP server example

pub mod common;

use clap::{ArgAction, Command, arg};
use common::calculator::Calculator;
use config::Config;
use prosa::{
    core::{
        main::{MainProc, MainRunnable as _},
        proc::{Proc, ProcConfig as _},
        settings::settings,
    },
    stub::proc::{StubProc, StubSettings},
};
use prosa_hyper::server::{mcp::proc::McpServerProc, proc::HyperServerSettings};
use prosa_utils::{config::tracing::TelemetryFilter, msg::simple_string_tvf::SimpleStringTvf};
use serde::{Deserialize, Serialize};
use tracing::debug;

#[settings]
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub(crate) struct MainHyperSettings {
    pub(crate) hyper: HyperServerSettings,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("mcp")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(arg!(-v - -verbose).action(ArgAction::SetTrue))
        .arg(
            arg!(-c --config <CONFIG_PATH> "Path of the MCP ProSA server configuration file")
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

    debug!("Start the Stub processor");
    let stub_settings = StubSettings::new(vec!["sum".to_string(), "sub".to_string()]);
    let stub_proc = StubProc::<SimpleStringTvf>::create(1, bus.clone(), stub_settings);
    Proc::<Calculator<SimpleStringTvf>>::run(stub_proc, String::from("calculator_stub"));

    debug!("Start the MCP processor");
    let http_proc =
        McpServerProc::<SimpleStringTvf>::create(2, bus.clone(), prosa_hyper_settings.hyper);
    Proc::<Calculator<SimpleStringTvf>>::run(http_proc, String::from("hyper"));

    // Wait on main task
    main_task.await;
    opentelemetry::global::shutdown_tracer_provider();
    Ok(())
}
