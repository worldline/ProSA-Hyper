# ProSA Hyper

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
