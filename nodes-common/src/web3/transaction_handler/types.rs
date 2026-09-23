use alloy::contract::{CallBuilder, CallDecoder};
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::Signer;
use eyre::{Context as _, ContextCompat as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One call in the `calls` array of `wallet_prepareCalls`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletCall {
    pub to: Address,
    pub data: Bytes,
    pub value: U256,
}

/// Convert alloy call types into a wallet API call — implemented for
/// `TransactionRequest` and for the `CallBuilder` returned by `#[sol(rpc)]`
/// contract methods, so no JSON needs to be built by hand.
pub trait IntoWalletCall {
    fn into_wallet_call(self) -> eyre::Result<WalletCall>;
}

impl IntoWalletCall for TransactionRequest {
    fn into_wallet_call(self) -> eyre::Result<WalletCall> {
        let to = match self.to {
            Some(TxKind::Call(addr)) => addr,
            _ => eyre::bail!("call has no target address (deployments are not supported)"),
        };
        let value = self.value.unwrap_or(U256::ZERO);
        let data = self.input.into_input().unwrap_or_default();
        Ok(WalletCall { to, data, value })
    }
}

impl<P: Provider, D: CallDecoder> IntoWalletCall for CallBuilder<P, D, Ethereum> {
    fn into_wallet_call(self) -> eyre::Result<WalletCall> {
        self.into_transaction_request().into_wallet_call()
    }
}

/// Build the params for `wallet_prepareCalls`: gas-sponsored (via the Gas
/// Manager `policy_id`) calls from an EIP-7702-delegated EOA.
///
/// `nonce_key` selects an independent EntryPoint 2D-nonce lane so concurrent
/// senders for the same EOA don't collide (Alchemy's `nonceOverride`
/// capability). `None` uses the account's default lane.
pub fn prepare_params(
    from: Address,
    chain_id: &str,
    calls: Vec<WalletCall>,
    policy_id: &str,
    nonce_key: Option<u64>,
) -> Value {
    let mut capabilities = serde_json::json!({ "paymasterService": { "policyId": policy_id } });
    if let Some(key) = nonce_key {
        capabilities["nonceOverride"] = serde_json::json!({ "nonceKey": format!("0x{key:x}") });
    }
    serde_json::json!([{
        "from": from,
        "chainId": chain_id,
        "calls": calls,
        "capabilities": capabilities
    }])
}

/// One prepared item from `wallet_prepareCalls`: either an EIP-7702
/// authorization or a user operation. `data` stays opaque JSON so it can be
/// echoed back verbatim in `wallet_sendPreparedCalls`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
    #[serde(default)]
    pub chain_id: Option<String>,
    #[serde(default)]
    pub signature_request: Option<SignatureRequest>,
}

impl PreparedItem {
    /// Flatten the `type: "array"` wrapper (first run: authorization + user
    /// op) into a plain list; a single item becomes a one-element list.
    pub fn into_items(self) -> eyre::Result<Vec<PreparedItem>> {
        if self.kind == "array" {
            serde_json::from_value(self.data).context("parsing prepared-calls array data")
        } else {
            Ok(vec![self])
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub raw_payload: Bytes,
}

impl SignatureRequest {
    /// Produce the 65-byte `r||s||v` signature Alchemy expects
    /// (`signature: {type: "secp256k1", data: "0x..."}`).
    ///
    /// `rawPayload` is always the final 32-byte digest, ready for a plain
    /// ECDSA signature. For `personal_sign` the EIP-191 wrapping has already
    /// been applied server-side (`rawPayload = eip191(data.raw)`, where
    /// `data.raw` is the user-op hash) — do NOT wrap again.
    ///
    /// Generic over any alloy [`Signer`] (local key, KMS, hardware, ...).
    pub async fn sign_with<S: Signer>(&self, signer: &S) -> eyre::Result<Bytes> {
        let digest = if self.raw_payload.len() == 32 {
            B256::from_slice(&self.raw_payload)
        } else {
            keccak256(&self.raw_payload)
        };
        let sig = signer.sign_hash(&digest).await?;
        let mut bytes = sig.as_bytes();
        match self.kind.as_str() {
            // User operation: recovery byte as 27/28 (as_bytes default).
            "personal_sign" => {}
            // EIP-7702 authorization ("eip7702Auth" live, "eth_signAuthorization"
            // in docs): recovery byte as yParity (0/1).
            "eth_signAuthorization" | "eip7702Auth" => bytes[64] -= 27,
            other => eyre::bail!("unsupported signature request type: {other}"),
        }
        Ok(bytes.into())
    }
}

/// A prepared item with its `signatureRequest` replaced by our signature —
/// the shape `wallet_sendPreparedCalls` expects back.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<String>,
    pub signature: SignatureData,
}

#[derive(Debug, Serialize)]
pub struct SignatureData {
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Bytes,
}

impl SignedItem {
    pub async fn from_prepared<S: Signer>(item: PreparedItem, signer: &S) -> eyre::Result<Self> {
        let req = item
            .signature_request
            .as_ref()
            .with_context(|| format!("prepared item '{}' has no signatureRequest", item.kind))?;
        let signature = SignatureData {
            kind: "secp256k1".into(),
            data: req.sign_with(signer).await?,
        };
        Ok(SignedItem {
            kind: item.kind,
            data: item.data,
            chain_id: item.chain_id,
            signature,
        })
    }
}

/// Build the single JSON param for `wallet_sendPreparedCalls`, mirroring the
/// shape `wallet_prepareCalls` returned: bare item, or `type: "array"` wrapper.
pub fn to_send_param(items: Vec<SignedItem>) -> eyre::Result<Value> {
    let mut values = items
        .into_iter()
        .map(|i| serde_json::to_value(i).context("serializing signed item"))
        .collect::<eyre::Result<Vec<_>>>()?;
    Ok(if values.len() == 1 {
        values.remove(0)
    } else {
        serde_json::json!({ "type": "array", "data": values })
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendPreparedCallsResponse {
    pub prepared_call_ids: Vec<String>,
}

/// EIP-5792 style status: 1xx pending, 200 confirmed, 4xx/5xx/6xx failed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallsStatus {
    pub status: u16,
    #[serde(default)]
    pub receipts: Option<Vec<CallReceipt>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallReceipt {
    pub transaction_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First-run shape: 7702 authorization + user operation.
    const ARRAY_RESPONSE: &str = r#"{
        "type": "array",
        "data": [
            {
                "type": "authorization",
                "data": { "address": "0x69007702764179f14F51cdce752f4f775d74E139", "nonce": "0x0", "chainId": "0xaa36a7" },
                "signatureRequest": {
                    "type": "eip7702Auth",
                    "rawPayload": "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
                }
            },
            {
                "type": "user-operation-v070",
                "data": { "sender": "0x1234123412341234123412341234123412341234", "nonce": "0x0", "callData": "0x" },
                "chainId": "0xaa36a7",
                "signatureRequest": {
                    "type": "personal_sign",
                    "data": { "raw": "0xabababababababababababababababababababababababababababababababab" },
                    "rawPayload": "0xabababababababababababababababababababababababababababababababab"
                }
            }
        ]
    }"#;

    /// Subsequent-run shape: a single user operation, no array wrapper.
    const SINGLE_RESPONSE: &str = r#"{
        "type": "user-operation-v070",
        "data": { "sender": "0x1234123412341234123412341234123412341234", "callData": "0x" },
        "chainId": "0xaa36a7",
        "signatureRequest": {
            "type": "personal_sign",
            "data": { "raw": "0xabababababababababababababababababababababababababababababababab" },
            "rawPayload": "0xabababababababababababababababababababababababababababababababab"
        }
    }"#;

    #[test]
    fn parses_first_run_array_response() {
        let prepared: PreparedItem = serde_json::from_str(ARRAY_RESPONSE).unwrap();
        let items = prepared.into_items().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "authorization");
        assert_eq!(
            items[0].signature_request.as_ref().unwrap().kind,
            "eip7702Auth"
        );
        assert_eq!(
            items[0]
                .signature_request
                .as_ref()
                .unwrap()
                .raw_payload
                .len(),
            32
        );
        assert_eq!(items[1].kind, "user-operation-v070");
        assert_eq!(items[1].chain_id.as_deref(), Some("0xaa36a7"));
        assert_eq!(
            items[1].signature_request.as_ref().unwrap().kind,
            "personal_sign"
        );
    }

    #[test]
    fn parses_subsequent_run_single_response() {
        let prepared: PreparedItem = serde_json::from_str(SINGLE_RESPONSE).unwrap();
        let items = prepared.into_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "user-operation-v070");
        // data must survive as opaque JSON to be echoed back on send
        assert_eq!(
            items[0].data["sender"],
            "0x1234123412341234123412341234123412341234"
        );
    }

    use alloy::primitives::Signature;
    use alloy::signers::local::PrivateKeySigner;

    #[tokio::test]
    async fn personal_sign_signs_prehashed_payload() {
        let signer = PrivateKeySigner::random();
        let payload: Bytes = vec![0xab; 32].into();
        let req = SignatureRequest {
            kind: "personal_sign".into(),
            raw_payload: payload.clone(),
        };

        let sig_bytes = req.sign_with(&signer).await.unwrap();
        assert_eq!(sig_bytes.len(), 65);
        assert!(
            sig_bytes[64] == 27 || sig_bytes[64] == 28,
            "v must be 27/28"
        );

        // rawPayload is already the final digest — plain ECDSA, no re-wrapping.
        let sig = Signature::try_from(sig_bytes.as_ref()).unwrap();
        let prehash = alloy::primitives::B256::from_slice(&payload);
        assert_eq!(
            sig.recover_address_from_prehash(&prehash).unwrap(),
            signer.address()
        );
    }

    /// Real values captured from a live wallet_prepareCalls response prove
    /// that rawPayload = eip191(data.raw): the server pre-wraps the user-op
    /// hash, so the client must NOT apply EIP-191 again.
    #[test]
    fn raw_payload_is_eip191_wrapped_userop_hash() {
        use alloy::primitives::{b256, eip191_hash_message};
        let userop_hash =
            b256!("0x81da820c96e34f10a77d5ab317c09ed2640bd105c59d2044cafef42b93d11767");
        let raw_payload =
            b256!("0xfc481ea332f89882be1312975f0f3b25a80fcbc97a3e1dc438be00b1d207bdfc");
        assert_eq!(eip191_hash_message(userop_hash), raw_payload);
    }

    #[tokio::test]
    async fn authorization_sign_recovers_to_signer() {
        let signer = PrivateKeySigner::random();
        let digest: Bytes = vec![0xcd; 32].into();
        let req = SignatureRequest {
            kind: "eip7702Auth".into(),
            raw_payload: digest.clone(),
        };

        let sig_bytes = req.sign_with(&signer).await.unwrap();
        assert_eq!(sig_bytes.len(), 65);

        let sig = Signature::try_from(sig_bytes.as_ref()).unwrap();
        // Plain ECDSA over the digest — no EIP-191 prefix.
        let prehash = alloy::primitives::B256::from_slice(&digest);
        assert_eq!(
            sig.recover_address_from_prehash(&prehash).unwrap(),
            signer.address()
        );
    }

    #[tokio::test]
    async fn unknown_signature_type_errors() {
        let signer = PrivateKeySigner::random();
        let req = SignatureRequest {
            kind: "eth_signTypedData_v4".into(),
            raw_payload: vec![0u8; 32].into(),
        };
        assert!(req.sign_with(&signer).await.is_err());
    }

    async fn sign_all(prepared: PreparedItem, signer: &PrivateKeySigner) -> Vec<SignedItem> {
        let mut items = Vec::new();
        for item in prepared.into_items().unwrap() {
            items.push(SignedItem::from_prepared(item, signer).await.unwrap());
        }
        items
    }

    #[tokio::test]
    async fn send_param_shapes() {
        let signer = PrivateKeySigner::random();
        let prepared: PreparedItem = serde_json::from_str(ARRAY_RESPONSE).unwrap();
        let items = sign_all(prepared, &signer).await;

        // Two items -> re-wrapped as an array-type param.
        let param = to_send_param(items).unwrap();
        assert_eq!(param["type"], "array");
        let entries = param["data"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["type"], "authorization");
        assert_eq!(entries[0]["signature"]["type"], "secp256k1");
        // signature.data is a 65-byte hex string; signatureRequest is gone.
        assert_eq!(
            entries[0]["signature"]["data"].as_str().unwrap().len(),
            2 + 65 * 2
        );
        assert!(entries[0].get("signatureRequest").is_none());
        assert_eq!(entries[1]["chainId"], "0xaa36a7");

        // One item -> sent bare, no array wrapper.
        let prepared: PreparedItem = serde_json::from_str(SINGLE_RESPONSE).unwrap();
        let items = sign_all(prepared, &signer).await;
        let param = to_send_param(items).unwrap();
        assert_eq!(param["type"], "user-operation-v070");
        assert_eq!(param["signature"]["type"], "secp256k1");
    }

    #[test]
    fn call_builder_converts_to_wallet_call() {
        use alloy::primitives::{U256, address};
        use alloy::providers::ProviderBuilder;
        use alloy::sol;

        sol! {
            #[sol(rpc)]
            contract TestFaucet {
                function mint(address token, address to, uint256 amount) external returns (uint256);
            }
        }

        // Provider is never contacted — the builder only encodes locally.
        let provider = ProviderBuilder::new().connect_http("http://localhost:1".parse().unwrap());
        let target = address!("0xC959483DBa39aa9E78757139af0e9a2EDEb3f42D");
        let token = address!("0xFF34B3d4Aee8ddCd6F9AFFFB6Fe49bD371b8a357");
        let recipient = address!("0x4DBF34B7DD883aF46483e63CaF9bD485B2427ff3");

        let faucet = TestFaucet::new(target, &provider);
        let call = faucet
            .mint(token, recipient, U256::from(5))
            .into_wallet_call()
            .unwrap();

        assert_eq!(call.to, target);
        // mint(address,address,uint256) selector, as seen in live callData.
        assert_eq!(&call.data[..4], [0xc6, 0xc3, 0xbb, 0xe6]);
        assert_eq!(call.value, U256::ZERO);

        // Serializes to the wallet API call shape.
        let json = serde_json::to_value(&call).unwrap();
        assert!(
            json["to"]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("0xC959483DBa39aa9E78757139af0e9a2EDEb3f42D")
        );
        assert!(json["data"].as_str().unwrap().starts_with("0xc6c3bbe6"));
        assert_eq!(json["value"], "0x0");
    }

    #[test]
    fn deployment_without_target_is_rejected() {
        use alloy::rpc::types::TransactionRequest;
        let tx = TransactionRequest::default(); // no `to`
        assert!(tx.into_wallet_call().is_err());
    }

    #[test]
    fn prepare_params_builds_expected_json() {
        use alloy::primitives::{U256, address};
        let from = address!("0x4DBF34B7DD883aF46483e63CaF9bD485B2427ff3");
        let call = WalletCall {
            to: address!("0xC959483DBa39aa9E78757139af0e9a2EDEb3f42D"),
            data: vec![0x01, 0x02].into(),
            value: U256::ZERO,
        };
        let params = prepare_params(from, "0xaa36a7", vec![call.clone()], "policy-uuid", None);
        assert_eq!(params[0]["chainId"], "0xaa36a7");
        assert_eq!(
            params[0]["capabilities"]["paymasterService"]["policyId"],
            "policy-uuid"
        );
        assert_eq!(params[0]["calls"][0]["data"], "0x0102");
        assert!(
            params[0]["from"]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("0x4DBF34B7DD883aF46483e63CaF9bD485B2427ff3")
        );
        // No nonce key -> no nonceOverride capability at all.
        assert!(params[0]["capabilities"].get("nonceOverride").is_none());

        // With a nonce key: hex-encoded nonceOverride capability.
        let params = prepare_params(from, "0xaa36a7", vec![call], "policy-uuid", Some(7));
        assert_eq!(
            params[0]["capabilities"]["nonceOverride"]["nonceKey"],
            "0x7"
        );
        assert_eq!(
            params[0]["capabilities"]["paymasterService"]["policyId"],
            "policy-uuid"
        );
    }

    #[test]
    fn parses_send_response_and_status() {
        let sent: SendPreparedCallsResponse =
            serde_json::from_str(r#"{ "preparedCallIds": ["0xdeadbeef"] }"#).unwrap();
        assert_eq!(sent.prepared_call_ids, vec!["0xdeadbeef"]);

        let status: CallsStatus = serde_json::from_str(
            r#"{ "status": 200, "receipts": [ { "status": "0x1", "transactionHash": "0x8ec2" } ] }"#,
        )
        .unwrap();
        assert_eq!(status.status, 200);
        assert_eq!(status.receipts.unwrap()[0].transaction_hash, "0x8ec2");

        let pending: CallsStatus = serde_json::from_str(r#"{ "status": 100 }"#).unwrap();
        assert_eq!(pending.status, 100);
        assert!(pending.receipts.is_none());
    }
}
