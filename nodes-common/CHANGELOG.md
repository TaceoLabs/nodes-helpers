# Changelog

## [Unreleased]

## [0.5.0](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.4.4...taceo-nodes-common-v0.5.0)

### ⛰️ Features


- *(common)* Add builder step to provide nonce manager - ([4a558f8](https://github.com/TaceoLabs/nodes-helpers/commit/4a558f8ef77e9264b437f567c2df9e567dbe2a3f))
- [**breaking**] Remove aws feature, localstack config helper and aws deps ([#27](https://github.com/TaceoLabs/nodes-helpers/pull/27)) - ([83af85c](https://github.com/TaceoLabs/nodes-helpers/commit/83af85c3f3664960c683a5ee97d7368ef831cd25))
- Impl FromStr for Environment ([#24](https://github.com/TaceoLabs/nodes-helpers/pull/24)) - ([ae46971](https://github.com/TaceoLabs/nodes-helpers/commit/ae46971bd854f4e119d8c3253580ab87fa504afb))

### 🐛 Bug Fixes


- *(nodes-helpers)* Make thiserror not optional ([#26](https://github.com/TaceoLabs/nodes-helpers/pull/26)) - ([646bc99](https://github.com/TaceoLabs/nodes-helpers/commit/646bc9954fbc9b096416ff5bc2dac41dbadc6732))

### 🚜 Refactor


- Refactor!(common): removed ws provider config and use only http - ([059e76a](https://github.com/TaceoLabs/nodes-helpers/commit/059e76ac960d6fb05e052af094f51f5cb2cc011b))


## [0.4.4](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.4.3...taceo-nodes-common-v0.4.4)

### ⛰️ Features


- *(nodes-common)* Add ERC165 checks ([#22](https://github.com/TaceoLabs/nodes-helpers/pull/22)) - ([7cdfde5](https://github.com/TaceoLabs/nodes-helpers/commit/7cdfde51b779a99031407c07a824f915e6355c29))


## [0.4.3](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.4.2...taceo-nodes-common-v0.4.3)

### ⛰️ Features


- Added configurable poll_interval for web3 ([#21](https://github.com/TaceoLabs/nodes-helpers/pull/21)) - ([641296d](https://github.com/TaceoLabs/nodes-helpers/commit/641296dcd0edd69a877b733a3b32e92d855702ad))


## [0.4.2](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.4.1...taceo-nodes-common-v0.4.2)

### 🐛 Bug Fixes


- Now also retry custom errors (and reqwest timeouts) ([#18](https://github.com/TaceoLabs/nodes-helpers/pull/18)) - ([3ad5976](https://github.com/TaceoLabs/nodes-helpers/commit/3ad59762aa9c607aa1e05c448c876cb185dd4ba3))


## [0.4.1](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.4.0...taceo-nodes-common-v0.4.1)

### ⛰️ Features


- Add web3 module to configure RPC providers ([#15](https://github.com/TaceoLabs/nodes-helpers/pull/15)) - ([29c4ba3](https://github.com/TaceoLabs/nodes-helpers/commit/29c4ba38d7e397b125ec5740fbf6ba5eaf28d83f))


## [0.4.0](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.3.1...taceo-nodes-common-v0.4.0)

### 🚜 Refactor


- [**breaking**] Adds a dev environment to distinguish between test-net and local testing ([#13](https://github.com/TaceoLabs/nodes-helpers/pull/13)) - ([d993803](https://github.com/TaceoLabs/nodes-helpers/commit/d993803e9ebfe2dd2d860fba31ee39e7e8135b62))


## [0.3.1](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.3.0...taceo-nodes-common-v0.3.1)

### ⛰️ Features


- Add postgres config + connect to pool ([#11](https://github.com/TaceoLabs/nodes-helpers/pull/11)) - ([bfdb1d4](https://github.com/TaceoLabs/nodes-helpers/commit/bfdb1d455a53461d5c696884f589542e0cec1692))


## [0.3.0](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.2.2...taceo-nodes-common-v0.3.0)

### ⛰️ Features


- Add environment enum - ([149c8e0](https://github.com/TaceoLabs/nodes-helpers/commit/149c8e0dd9d01bf144b9b5fc842993e22ab29c54))

### 🚜 Refactor


- [**breaking**] Move aws behind a feature - ([f078a57](https://github.com/TaceoLabs/nodes-helpers/commit/f078a5736015c59fb20eec0b93643c6b83747197))
- Added prod clippy lints - ([706749a](https://github.com/TaceoLabs/nodes-helpers/commit/706749ae3afd783d5adfe91dae9d0f45f83d4a85))


## [0.2.2](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.2.1...taceo-nodes-common-v0.2.2)

### ⛰️ Features


- Add common axum router for health and version ([#6](https://github.com/TaceoLabs/nodes-helpers/pull/6)) - ([27e69a3](https://github.com/TaceoLabs/nodes-helpers/commit/27e69a35ea058a47345b93d29d2e2444a1630b80))

### 🐛 Bug Fixes


- Cancellation token is triggered on panic in `ctrl_c` handler. ([#7](https://github.com/TaceoLabs/nodes-helpers/pull/7)) - ([a3af227](https://github.com/TaceoLabs/nodes-helpers/commit/a3af227f81650c9bc06930b5ad0672874f5eb826))


## [0.2.1](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.2.0...taceo-nodes-common-v0.2.1)

### ⛰️ Features


- Added StartedServices struct ([#3](https://github.com/TaceoLabs/nodes-helpers/pull/3)) - ([8166da8](https://github.com/TaceoLabs/nodes-helpers/commit/8166da8a3705f5d106270c9901f5d1b556cff937))


## [0.2.0](https://github.com/TaceoLabs/nodes-helpers/compare/taceo-nodes-common-v0.1.0...taceo-nodes-common-v0.2.0)

### ⛰️ Features


- [**breaking**] Returns a token indicating graceful shutdown - ([9ae357c](https://github.com/TaceoLabs/nodes-helpers/commit/9ae357cf098da92c4485b4d3417faa2643b1c4ce))

