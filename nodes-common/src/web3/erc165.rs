//! ERC-165 interface detection utilities.
//!
//! Provides helpers for querying whether an on-chain contract implements a
//! given interface according to [EIP-165](https://eips.ethereum.org/EIPS/eip-165).
//!
//! The implementation is inspired by `OpenZeppelin`'s
//! [`ERC165Checker.sol`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol).
//!
//! # Usage
//!
//! For most use cases, call
//! [`HttpRpcProvider::erc165_supports_interface_unchecked`] directly. It queries
//! whether the target contract reports support for the given interface and
//! returns `Ok(())` or `Err(`[`ERC165ConfirmError::Unsupported`]`)`. It does
//! **not** enforce that the contract is ERC-165 compliant — if you only care
//! that the interface is supported, this is the right method to use.
//!
//! Use [`HttpRpcProvider::erc165_supports_interface`] when you also need to
//! enforce strict ERC-165 compliance — i.e., the contract must not claim to
//! support the invalid interface `0xffffffff`. The queries are batched through
//! Multicall3.
//!
//! Use [`HttpRpcProvider::ensure_erc165_conform`] to verify ERC-165 compliance
//! independently of a specific interface query.
//!
//! * [`HttpRpcProvider::erc165_supports_interface_unchecked`] – queries
//!   interface support without enforcing ERC-165 compliance. Preferred for most
//!   callers.
//! * [`HttpRpcProvider::erc165_supports_interface`] – checks interface support
//!   **and** strict ERC-165 compliance in one batched RPC request.
//! * [`HttpRpcProvider::ensure_erc165_conform`] – verifies ERC-165 compliance
//!   only.
//! * [`erc165_interface_selector`] – computes the ERC-165 interface identifier
//!   by XOR-ing the given function selectors.

use alloy::{
    primitives::{Address, FixedBytes},
    providers::{MulticallError, Provider},
    sol,
    transports::{TransportError, TransportErrorKind},
};

use crate::web3::{HttpRpcProvider, erc165::ERC165::ERC165Instance};

sol!(
    #[allow(clippy::exhaustive_structs, reason="comes from sol macro")]
    #[allow(clippy::exhaustive_enums, reason="comes from sol macro")]
    #[sol(rpc)]
    interface ERC165 {
        /// @notice Query if a contract implements an interface
        /// @param interfaceID The interface identifier, as specified in ERC-165
        /// @dev Interface identification is specified in ERC-165. This function
        ///  uses less than 30,000 gas.
        /// @return `true` if the contract implements `interfaceID` and
        ///  `interfaceID` is not 0xffffffff, `false` otherwise
        function supportsInterface(bytes4 interfaceID) external view returns (bool);
    }
);

/// The four-byte selector of `supportsInterface(bytes4)` (`0x01ffc9a7`).
///
/// A contract that implements ERC-165 must return `true` when queried
/// with this selector. Equivalent to `type(IERC165).interfaceId` in
/// Solidity.
pub const ERC_165_SUPPORTS_INTERFACE_SELECTOR: [u8; 4] = [0x01, 0xff, 0xc9, 0xa7];
/// The sentinel interface identifier (`0xffffffff`).
///
/// Per the EIP-165 specification, no compliant contract may claim
/// support for this value. Corresponds to `_INTERFACE_ID_INVALID` in
/// `OpenZeppelin`'s `ERC165Checker`.
pub const INVALID_INTERFACE_SELECTOR: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// Computes an ERC-165 interface identifier from an iterator of function selectors.
///
/// The interface identifier is defined as the XOR of all function selectors
/// that belong to the interface (see [EIP-165](https://eips.ethereum.org/EIPS/eip-165)).
///
/// # Arguments
///
/// * `selectors` – iterator yielding the four-byte selectors of every
///   function in the interface.
#[must_use]
pub fn erc165_interface_selector(selectors: impl IntoIterator<Item = [u8; 4]>) -> FixedBytes<4> {
    FixedBytes::from(selectors.into_iter().fold([0u8; 4], |mut acc, selector| {
        for (a, b) in acc.iter_mut().zip(selector) {
            *a ^= b;
        }
        acc
    }))
}

/// Maps an alloy `supportsInterface` call result into a unit result.
///
/// * `Ok(true)` – the contract confirmed support → `Ok(())`.
/// * `Ok(false)` – the contract denied support → `Err(Unsupported)`.
/// * `ZeroData` error – the address has no deployed code → `Err(NotAContract)`.
/// * `TransportError` – RPC transport failure → propagated as-is.
/// * Any other error – treated as unsupported → `Err(Unsupported)`.
fn unwrap_erc165_call(
    call: Result<bool, alloy::contract::Error>,
) -> Result<(), ERC165ConfirmError> {
    match call {
        Ok(true) => Ok(()),
        Err(alloy::contract::Error::ZeroData(_, _)) => Err(ERC165ConfirmError::NotAContract),
        // There was an RPC transport error
        Err(alloy::contract::Error::TransportError(TransportError::Transport(transport_error))) => {
            Err(ERC165ConfirmError::TransportError(transport_error))
        }
        // every other error means it does not support the interface
        Ok(false) | Err(_) => Err(ERC165ConfirmError::Unsupported),
    }
}

/// Errors returned by the ERC-165 conformance and interface-support checks.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ERC165ConfirmError {
    /// The target address does not contain a deployed contract
    /// (the call returned zero data).
    #[error("The requested address is not a deployed contract")]
    NotAContract,
    /// The contract does not conform to the requested interface.
    #[error("The contract does not support the requested interface")]
    Unsupported,
    /// An RPC transport error occurred while querying the contract.
    #[error(transparent)]
    TransportError(#[from] TransportErrorKind),
}

impl From<MulticallError> for ERC165ConfirmError {
    fn from(error: MulticallError) -> Self {
        match error {
            MulticallError::NoReturnData | MulticallError::DecodeError(_) => {
                ERC165ConfirmError::NotAContract
            }
            MulticallError::TransportError(TransportError::Transport(transport_error)) => {
                ERC165ConfirmError::TransportError(transport_error)
            }
            MulticallError::ValueTx
            | MulticallError::CallFailed(_)
            | MulticallError::TransportError(_) => ERC165ConfirmError::Unsupported,
        }
    }
}

impl HttpRpcProvider {
    /// Checks whether the contract at `address` correctly implements ERC-165.
    ///
    /// The check follows the procedure defined in
    /// [EIP-165](https://eips.ethereum.org/EIPS/eip-165):
    ///
    /// 1. `supportsInterface(0x01ffc9a7)` must return `true`.
    /// 2. `supportsInterface(0xffffffff)` must return `false`.
    ///
    /// Both queries are batched into one RPC request through Multicall3 at its
    /// canonical address.
    ///
    /// Inspired by `OpenZeppelin`'s
    /// [`ERC165Checker.supportsERC165`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol#L24).
    ///
    /// # Errors
    ///
    /// * [`ERC165ConfirmError::NotAContract`] – the address has no deployed code.
    /// * [`ERC165ConfirmError::Unsupported`] – the contract is not ERC-165
    ///   conformant: either it does not respond to `supportsInterface(0x01ffc9a7)`,
    ///   or it incorrectly claims to support the invalid interface `0xffffffff`.
    /// * [`ERC165ConfirmError::TransportError`] – an RPC transport failure.
    pub async fn ensure_erc165_conform(&self, address: Address) -> Result<(), ERC165ConfirmError> {
        let maybe_erc165 = ERC165Instance::new(address, self.inner());
        let supports_erc165_call =
            maybe_erc165.supportsInterface(FixedBytes::from(ERC_165_SUPPORTS_INTERFACE_SELECTOR));
        let supports_invalid_interface_call =
            maybe_erc165.supportsInterface(FixedBytes::from(INVALID_INTERFACE_SELECTOR));
        let (supports_erc165, supports_invalid) = self
            .inner()
            .multicall()
            .add(supports_erc165_call)
            .add(supports_invalid_interface_call)
            .aggregate()
            .await?;

        if supports_erc165 && !supports_invalid {
            Ok(())
        } else {
            Err(ERC165ConfirmError::Unsupported)
        }
    }

    /// Queries whether the contract at `address` supports the interface
    /// identified by the XOR of the given `selectors`, **without** first
    /// verifying ERC-165 conformance.
    ///
    /// Inspired by `OpenZeppelin`'s
    /// [`ERC165Checker.supportsERC165InterfaceUnchecked`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol#L107).
    ///
    /// # Errors
    ///
    /// Returns [`ERC165ConfirmError`] if the contract does not support the
    /// requested interface, on transport failures, or if the target address
    /// is not a deployed contract.
    ///
    /// # Note
    ///
    /// This method does not verify strict ERC-165 compliance. Use
    /// [`HttpRpcProvider::erc165_supports_interface`] if you also want to ensure
    /// the contract does not claim to support the invalid interface `0xffffffff`.
    pub async fn erc165_supports_interface_unchecked(
        &self,
        address: Address,
        selectors: impl IntoIterator<Item = [u8; 4]>,
    ) -> Result<(), ERC165ConfirmError> {
        let erc165 = ERC165Instance::new(address, self.inner());
        let supports_interface = erc165
            .supportsInterface(erc165_interface_selector(selectors))
            .call()
            .await;
        unwrap_erc165_call(supports_interface)
    }

    /// Checks whether the contract at `address` supports the interface
    /// identified by the XOR of the given `selectors`.
    ///
    /// This method performs the **full** ERC-165 verification:
    ///
    /// The requested interface query and both ERC-165 conformance queries are
    /// batched into one RPC request through Multicall3 at its canonical address.
    ///
    /// Inspired by `OpenZeppelin`'s
    /// [`ERC165Checker.supportsInterface`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol#L36).
    ///
    /// # Errors
    ///
    /// Returns [`ERC165ConfirmError`] if the contract does not support the
    /// requested interface, on transport failures, if the target address is
    /// not a contract, or if the contract violates the EIP-165 spec.
    pub async fn erc165_supports_interface(
        &self,
        address: Address,
        selectors: impl IntoIterator<Item = [u8; 4]>,
    ) -> Result<(), ERC165ConfirmError> {
        let maybe_erc165 = ERC165Instance::new(address, self.inner());
        let supports_interface_call =
            maybe_erc165.supportsInterface(erc165_interface_selector(selectors));
        let supports_erc165_call =
            maybe_erc165.supportsInterface(FixedBytes::from(ERC_165_SUPPORTS_INTERFACE_SELECTOR));
        let supports_invalid_interface_call =
            maybe_erc165.supportsInterface(FixedBytes::from(INVALID_INTERFACE_SELECTOR));
        let (supports_interface, supports_erc165, supports_invalid) = self
            .inner()
            .multicall()
            .add(supports_interface_call)
            .add(supports_erc165_call)
            .add(supports_invalid_interface_call)
            .aggregate()
            .await?;

        if supports_interface && supports_erc165 && !supports_invalid {
            Ok(())
        } else {
            Err(ERC165ConfirmError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "web3-asserter")]
    use alloy::{
        primitives::{Bytes, U256, address},
        providers::mock::Asserter,
        sol_types::SolValue,
    };
    use alloy::{sol, sol_types::SolCall};

    use crate::web3::erc165::ERC165;
    #[cfg(feature = "web3-asserter")]
    use crate::web3::{HttpRpcProvider, erc165::ERC165ConfirmError};

    sol! {
        interface Solidity101 {
            function hello() external pure;
            function world(int256) external pure;
        }
    }

    #[test]
    fn test_selector_hashes() {
        assert_eq!(
            super::erc165_interface_selector([ERC165::supportsInterfaceCall::SELECTOR]),
            super::ERC_165_SUPPORTS_INTERFACE_SELECTOR
        );
        assert_eq!(super::erc165_interface_selector([]), [0, 0, 0, 0]);

        let selectors = [
            Solidity101::helloCall::SELECTOR,
            Solidity101::worldCall::SELECTOR,
        ];
        assert_eq!(
            super::erc165_interface_selector(selectors),
            [0xc6, 0xbe, 0x8b, 0x58]
        );
        assert_eq!(
            super::erc165_interface_selector(selectors.into_iter().rev()),
            [0xc6, 0xbe, 0x8b, 0x58],
            "selector order should not matter"
        );
        assert_ne!(
            super::erc165_interface_selector([
                Solidity101::helloCall::SELECTOR,
                Solidity101::worldCall::SELECTOR,
                Solidity101::helloCall::SELECTOR,
            ]),
            [0xc6, 0xbe, 0x8b, 0x58],
            "repeating a selector should change the interface identifier"
        );
    }

    #[cfg(feature = "web3-asserter")]
    fn aggregate_response(values: impl IntoIterator<Item = bool>) -> Bytes {
        let return_data = values
            .into_iter()
            .map(|value| Bytes::from(value.abi_encode()))
            .collect::<Vec<_>>();
        Bytes::from((U256::ZERO, return_data).abi_encode_params())
    }

    #[cfg(feature = "web3-asserter")]
    fn provider_with_response(response: &Bytes) -> (HttpRpcProvider, Asserter) {
        let asserter = Asserter::new();
        asserter.push_success(response);
        let provider = HttpRpcProvider::with_mock_asserter(asserter.clone());
        (provider, asserter)
    }

    #[cfg(feature = "web3-asserter")]
    #[tokio::test]
    async fn ensure_erc165_conform_handles_contract_responses() {
        for (values, should_succeed) in [
            ([true, false], true),
            ([false, false], false),
            ([true, true], false),
        ] {
            let (provider, asserter) = provider_with_response(&aggregate_response(values));
            let result = provider
                .ensure_erc165_conform(address!("0000000000000000000000000000000000000001"))
                .await;

            if should_succeed {
                result.expect("mocked contract should be ERC-165 conformant");
            } else {
                assert!(
                    matches!(result, Err(ERC165ConfirmError::Unsupported)),
                    "non-conformant response should be unsupported"
                );
            }
            assert!(
                asserter.read_q().is_empty(),
                "the check should consume exactly one RPC response"
            );
        }
    }

    #[cfg(feature = "web3-asserter")]
    #[tokio::test]
    async fn ensure_erc165_conform_maps_call_errors() {
        let (provider, asserter) = provider_with_response(&Bytes::new());
        let result = provider
            .ensure_erc165_conform(address!("0000000000000000000000000000000000000001"))
            .await;
        assert!(
            matches!(result, Err(ERC165ConfirmError::NotAContract)),
            "empty return data should identify a non-contract"
        );
        assert!(asserter.read_q().is_empty(), "response should be consumed");

        let provider = HttpRpcProvider::with_mock_asserter(Asserter::new());
        let result = provider
            .ensure_erc165_conform(address!("0000000000000000000000000000000000000001"))
            .await;
        assert!(
            matches!(result, Err(ERC165ConfirmError::TransportError(_))),
            "an empty mock queue should produce a transport error"
        );
    }

    #[cfg(feature = "web3-asserter")]
    #[tokio::test]
    async fn erc165_supports_interface_handles_contract_responses() {
        for (values, should_succeed) in [
            ([true, true, false], true),
            ([false, true, false], false),
            ([true, false, false], false),
            ([true, true, true], false),
        ] {
            let (provider, asserter) = provider_with_response(&aggregate_response(values));
            let result = provider
                .erc165_supports_interface(
                    address!("0000000000000000000000000000000000000001"),
                    [ERC165::supportsInterfaceCall::SELECTOR],
                )
                .await;

            if should_succeed {
                result.expect("mocked contract should support the requested interface");
            } else {
                assert!(
                    matches!(result, Err(ERC165ConfirmError::Unsupported)),
                    "unsupported or non-conformant response should be rejected"
                );
            }
            assert!(
                asserter.read_q().is_empty(),
                "the check should consume exactly one RPC response"
            );
        }
    }

    #[cfg(feature = "web3-asserter")]
    #[tokio::test]
    async fn erc165_supports_interface_maps_call_errors() {
        let (provider, asserter) = provider_with_response(&Bytes::new());
        let result = provider
            .erc165_supports_interface(
                address!("0000000000000000000000000000000000000001"),
                [ERC165::supportsInterfaceCall::SELECTOR],
            )
            .await;
        assert!(
            matches!(result, Err(ERC165ConfirmError::NotAContract)),
            "empty return data should identify a non-contract"
        );
        assert!(asserter.read_q().is_empty(), "response should be consumed");

        let provider = HttpRpcProvider::with_mock_asserter(Asserter::new());
        let result = provider
            .erc165_supports_interface(
                address!("0000000000000000000000000000000000000001"),
                [ERC165::supportsInterfaceCall::SELECTOR],
            )
            .await;
        assert!(
            matches!(result, Err(ERC165ConfirmError::TransportError(_))),
            "an empty mock queue should produce a transport error"
        );
    }

    #[cfg(feature = "web3-asserter")]
    #[tokio::test]
    async fn erc165_supports_interface_unchecked_handles_contract_responses() {
        for (value, should_succeed) in [(true, true), (false, false)] {
            let response = Bytes::from(value.abi_encode());
            let (provider, asserter) = provider_with_response(&response);
            let result = provider
                .erc165_supports_interface_unchecked(
                    address!("0000000000000000000000000000000000000001"),
                    [ERC165::supportsInterfaceCall::SELECTOR],
                )
                .await;

            if should_succeed {
                result.expect("mocked contract should support the requested interface");
            } else {
                assert!(
                    matches!(result, Err(ERC165ConfirmError::Unsupported)),
                    "false response should be unsupported"
                );
            }
            assert!(
                asserter.read_q().is_empty(),
                "the check should consume exactly one RPC response"
            );
        }
    }

    #[cfg(feature = "web3-asserter")]
    #[tokio::test]
    async fn erc165_supports_interface_unchecked_maps_call_errors() {
        let (provider, asserter) = provider_with_response(&Bytes::new());
        let result = provider
            .erc165_supports_interface_unchecked(
                address!("0000000000000000000000000000000000000001"),
                [ERC165::supportsInterfaceCall::SELECTOR],
            )
            .await;
        assert!(
            matches!(result, Err(ERC165ConfirmError::NotAContract)),
            "empty return data should identify a non-contract"
        );
        assert!(asserter.read_q().is_empty(), "response should be consumed");

        let provider = HttpRpcProvider::with_mock_asserter(Asserter::new());
        let result = provider
            .erc165_supports_interface_unchecked(
                address!("0000000000000000000000000000000000000001"),
                [ERC165::supportsInterfaceCall::SELECTOR],
            )
            .await;
        assert!(
            matches!(result, Err(ERC165ConfirmError::TransportError(_))),
            "an empty mock queue should produce a transport error"
        );
    }
}
