# ProSA Hyper

[<img alt="github" src="https://img.shields.io/badge/github-46beaa?style=for-the-badge&labelColor=555555&logo=github" height="20">](https://github.com/worldline/ProSA-Hyper)
[<img alt="crates-io" src="https://img.shields.io/badge/crates.io-ffeb78?style=for-the-badge&labelColor=555555&logo=rust" height="20">](https://crates.io/crates/prosa-hyper)
[<img alt="docs-rs" src="https://img.shields.io/badge/docs.rs-41b4d2?style=for-the-badge&labelColor=555555&logo=docs.rs" height="20">](https://docs.rs/prosa-hyper)
[<img alt="build status" src="https://img.shields.io/github/actions/workflow/status/worldline/ProSA-Hyper/ci.yml?branch%3Amain&style=for-the-badge" height="20">](https://github.com/worldline/ProSA-Hyper/actions?query=branch%3Amain)
[<img alt="dependency status" src="https://img.shields.io/deps-rs/repo/github/worldline/ProSA-Hyper?style=for-the-badge" height="20">](https://deps.rs/repo/github/worldline/ProSA-Hyper)

[ProSA](https://github.com/worldline/ProSA) Hyper processor for HTTP client/server build on [Hyper](https://hyper.rs/), a Tokio implementation of HTTP.

## Use

To include it in your project, add the crate to your dependencies:
```sh
cargo add prosa-hyper
```

To build your ProSA application afterward, use [cargo-prosa](https://github.com/worldline/ProSA/blob/main/cargo-prosa/README.md) or build manually.

## Configuration

### Server

The server configuration is straightforward.
You only need to set a [ListenerSetting](https://docs.rs/prosa/latest/prosa/io/listener/struct.ListenerSetting.html) to configure.

```yaml
http_server:
  listener:
    url: https://0.0.0.0:443
    ssl:
      store:
        path: "cert_path/"
      cert: cert.pem
      key: key.pem
      passphrase: MySuperPassphrase
```

If you have some slow services, you can set the `service_timeout` parameter (800 ms by default).

## Examples

### Server

To get help on the processor command-line arguments, run:
```sh
cargo run --example server -- -h
```

If you run it without any parameters, it'll start an HTTPS server using the configuration in _examples/server.yml_:
```sh
cargo run --example server
```

The server provides the following targets:
 - [/](http://localhost:8080/) returns the ProSA name
 - [/test](http://localhost:8080/test) contacts an internal service named SRV_TEST (requires starting the stub processor)
 - [metrics](http://localhost:9090/metrics) exposes Prometheus metrics as configured
