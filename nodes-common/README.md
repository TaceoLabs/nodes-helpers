# taceo-nodes-common

[![Crates.io](https://img.shields.io/crates/v/taceo-nodes-common.svg)](https://crates.io/crates/taceo-nodes-common)
[![docs.rs](https://docs.rs/taceo-nodes-common/badge.svg)](https://docs.rs/taceo-nodes-common)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/TaceoLabs/nodes-helpers/blob/main/LICENSE-MIT)

Collection of common functions used by nodes in our MPC networks.

## Features

No features are enabled by default; enable the ones you need.

| Feature | Default | Description |
|---------|---------|-------------|
| `web3` | | HTTP Ethereum RPC provider via Alloy, ERC-165 interface detection |
| `web3-asserter` | | Alloy mocked-provider constructor for `HttpRpcProvider` (tests) |
| `api` | | Health and version endpoints via Axum |
| `postgres` | | PostgreSQL connection pool via SQLx |
| `serde` | | Serde support |
| `middleware` | | Axum/Tower middleware layers |
| `unkey` | | Bearer-token verification via the Unkey API |
| `test-utils` | | Integration-test helpers: Postgres testcontainers, test schemas, Axum test server |

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE) or [MIT license](../LICENSE-MIT) at your option.
