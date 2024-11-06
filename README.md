Ryzen Power Monitor, based on rust.

[![Rust](https://github.com/ItsLucas/ryzenmon-rust/actions/workflows/rust.yml/badge.svg)](https://github.com/ItsLucas/ryzenmon-rust/actions/workflows/rust.yml)

# Usage:
Put the following into /etc/ryzenmon/config.toml: 
```
[influxdb]
host = "http://localhost:8086"
org = "your_org"
token = "your_token"
bucket = "your_bucket"
```
(Or let the program create one for you)

Use the systemd service file ryzenmon-rust.service, or write one by your own.
