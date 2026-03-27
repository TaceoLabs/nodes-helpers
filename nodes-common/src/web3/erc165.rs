//! ERC-165 interface detection utilities.
//!
//! Provides helpers for querying whether an on-chain contract implements a
//! given interface according to [EIP-165](https://eips.ethereum.org/EIPS/eip-165).
//!
//! The implementation is inspired by OpenZeppelin's
//! [`ERC165Checker.sol`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol).
//!
//! * [`RpcProvider::is_erc165_conform`] – checks whether a contract correctly
//!   implements the ERC-165 `supportsInterface` function.
//! * [`RpcProvider::erc165_supports_interface`] – checks ERC-165 conformance
//!   **and** support for a specific interface in one call.
//! * [`RpcProvider::erc165_supports_interface_unchecked`] – queries interface
//!   support **without** first verifying ERC-165 conformance.
//! * [`erc165_interface_selector`] – computes the ERC-165 interface identifier
//!   by XOR-ing the given function selectors.

use alloy::{
    primitives::{Address, FixedBytes},
    sol,
    transports::TransportError,
};

use crate::web3::{RpcProvider, erc165::ERC165::ERC165Instance};

sol!(
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
/// OpenZeppelin's `ERC165Checker`.
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

/// Internal helper.
///
/// Maps an alloy contract call result into an [`ERC165ConfirmError`].
///
/// * `Ok(bool)` – passes through.
/// * `ZeroData` error – treated as "address is not a deployed contract".
/// * `TransportError` – propagated as-is.
/// * Any other error – treated as the contract not supporting the interface
///   (returns `Ok(false)`).
fn unwrap_erc165_call(
    call: Result<bool, alloy::contract::Error>,
) -> Result<bool, ERC165ConfirmError> {
    match call {
        Ok(valid) => Ok(valid),
        Err(alloy::contract::Error::ZeroData(_, _)) => Err(ERC165ConfirmError::NotAContract),
        Err(alloy::contract::Error::TransportError(transport_error)) => {
            Err(ERC165ConfirmError::TransportError(transport_error))
        }
        // every other error means it does not support the interface
        Err(_) => Ok(false),
    }
}

/// Errors returned by the ERC-165 conformance and interface-support checks.
#[derive(Debug, thiserror::Error)]
pub enum ERC165ConfirmError {
    /// The target address does not contain a deployed contract
    /// (the call returned zero data).
    #[error("The requested address is not a deployed contract")]
    NotAContract,
    /// The contract claims to support the invalid interface identifier
    /// `0xffffffff`, which violates the EIP-165 specification.
    ///
    /// Importantly it supports the requested interface, so callers might still accept the contract as valid.
    #[error(
        "Supports 0xffffffff interface which is not allowed, but conforms to requested interface"
    )]
    ConfirmsButAlsoToInvalidInterface,
    /// An RPC transport error occurred while querying the contract.
    #[error(transparent)]
    TransportError(#[from] TransportError),
}

impl RpcProvider {
    /// Checks whether the contract at `address` correctly implements ERC-165.
    ///
    /// The check follows the procedure defined in
    /// [EIP-165](https://eips.ethereum.org/EIPS/eip-165):
    ///
    /// 1. `supportsInterface(0x01ffc9a7)` must return `true`.
    /// 2. `supportsInterface(0xffffffff)` must return `false`.
    ///
    /// Both calls are executed **concurrently** via [`tokio::join!`].
    ///
    /// Inspired by OpenZeppelin's
    /// [`ERC165Checker.supportsERC165`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol#L24).
    ///
    /// # Errors
    ///
    /// * [`ERC165ConfirmError::NotAContract`] – the address has no deployed code.
    /// * [`ERC165ConfirmError::ConfirmsButAlsoToInvalidInterface`] – the contract
    ///   claims to support `0xffffffff`, violating the spec.
    /// * [`ERC165ConfirmError::TransportError`] – an RPC transport failure.
    pub async fn is_erc165_conform(&self, address: Address) -> Result<bool, ERC165ConfirmError> {
        let maybe_erc165 = ERC165Instance::new(address, self.http());
        let supports_erc165_call =
            maybe_erc165.supportsInterface(FixedBytes::from(ERC_165_SUPPORTS_INTERFACE_SELECTOR));
        let supports_invalid_interface_call =
            maybe_erc165.supportsInterface(FixedBytes::from(INVALID_INTERFACE_SELECTOR));
        let (supports_erc165, supports_invalid) = tokio::join!(
            supports_erc165_call.call(),
            supports_invalid_interface_call.call()
        );

        let supports_invalid = unwrap_erc165_call(supports_invalid)?;
        let supports_erc165 = unwrap_erc165_call(supports_erc165)?;

        if supports_erc165 && !supports_invalid {
            Ok(true)
        } else if supports_erc165 && supports_invalid {
            Err(ERC165ConfirmError::ConfirmsButAlsoToInvalidInterface)
        } else {
            Ok(false)
        }
    }

    /// Queries whether the contract at `address` supports the interface
    /// identified by the XOR of the given `selectors`, **without** first
    /// verifying ERC-165 conformance.
    ///
    /// Inspired by OpenZeppelin's
    /// [`ERC165Checker.supportsERC165InterfaceUnchecked`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol#L107).
    ///
    /// # Errors
    ///
    /// Returns [`ERC165ConfirmError`] on transport failures or if the target
    /// address is not a deployed contract.
    ///
    /// # Preconditions
    ///
    /// Callers should verify ERC-165 conformance beforehand (see
    /// [`RpcProvider::is_erc165_conform`]) or use
    /// [`RpcProvider::erc165_supports_interface`] which performs that check
    /// automatically.
    pub async fn erc165_supports_interface_unchecked(
        &self,
        address: Address,
        selectors: impl IntoIterator<Item = [u8; 4]>,
    ) -> Result<bool, ERC165ConfirmError> {
        let erc165 = ERC165Instance::new(address, self.http());
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
    /// 1. Verifies the contract is ERC-165 conformant (via
    ///    [`RpcProvider::is_erc165_conform`]).
    /// 2. Queries support for the requested interface (via
    ///    [`RpcProvider::erc165_supports_interface_unchecked`]).
    ///
    /// Both steps run **concurrently** via [`tokio::join!`].
    ///
    /// Inspired by OpenZeppelin's
    /// [`ERC165Checker.supportsInterface`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/5e28952cbdc0eb7d19ee62580ab31b30c2376e48/contracts/utils/introspection/ERC165Checker.sol#L36).
    ///
    /// # Errors
    ///
    /// Returns [`ERC165ConfirmError`] on transport failures, if the target
    /// address is not a contract, or if the contract violates the EIP-165
    /// spec.
    pub async fn erc165_supports_interface(
        &self,
        address: Address,
        selectors: impl IntoIterator<Item = [u8; 4]>,
    ) -> Result<bool, ERC165ConfirmError> {
        let (supports_interface, erc165_conform_check) = tokio::join!(
            self.erc165_supports_interface_unchecked(address, selectors),
            self.is_erc165_conform(address)
        );

        Ok(supports_interface? && erc165_conform_check?)
    }
}

#[cfg(test)]
mod tests {
    use alloy::{sol, sol_types::SolCall};

    use crate::web3::{
        self,
        erc165::{ERC165, ERC165ConfirmError},
        tests::WithWallet,
    };

    // compiled with:
    // solc Selector.sol --via-ir --optimize --bin
    sol!(
        // SPDX-License-Identifier: MIT
        pragma solidity ^0.8.28;

        interface Solidity101 {
            function hello() external pure;
            function world(int256) external pure;
        }

        #[sol(rpc, bytecode="60808060405234601357607a908160188239f35b5f80fdfe60808060405260043610156011575f80fd5b5f3560e01c63bb71eb3b146023575f80fd5b346040575f3660031901126040576318d7d16b60e31b8152602090f35b5f80fdfea264697066735822122050bdf014f6d049e0b709e30cbe71191a291cf62033b9d636415ed4c0d491262464736f6c634300081e0033")]
        contract Selector {
            function calculateSelector() public pure returns (bytes4) {
                Solidity101 i;
                return i.hello.selector ^ i.world.selector;
            }
        }

        #[sol(rpc, bytecode="6080806040523460135760ab908160188239f35b5f80fdfe60808060405260043610156011575f80fd5b5f3560e01c6301ffc9a7146023575f80fd5b3460715760203660031901126071576004359063ffffffff60e01b82168092036071576020916301ffc9a760e01b81149081156061575b5015158152f35b6318d7d16b60e31b1490505f605a565b5f80fdfea26469706673582212205f87878e063679dad406dce588e07a8a58164c7fcb0fe10a9c5700f56330addf64736f6c634300081e0033")]
        contract ConfirmsERC165 {
            function supportsInterface(bytes4 interfaceID) external pure returns (bool) {
                return interfaceID == type(ERC165).interfaceId || interfaceID == type(Solidity101).interfaceId;
            }
        }

        #[sol(rpc, bytecode="6080806040523460135760ac908160188239f35b5f80fdfe60808060405260043610156011575f80fd5b5f3560e01c6301ffc9a7146023575f80fd5b3460725760203660031901126072576004359063ffffffff60e01b82168092036072576020916301ffc9a760e01b81149081156061575b5015158152f35b6001600160e01b03191490505f605a565b5f80fdfea26469706673582212209465485de6d71f94f5b12921ac7989fab7ba63b2c0fdb38cb176559558902f7764736f6c634300081e0033")]
        contract ConfirmsInvalidInterface {
            function supportsInterface(bytes4 interfaceID) external pure returns (bool) {
                return interfaceID == type(ERC165).interfaceId || interfaceID == 0xffffffff;
            }
        }
    );

    #[test]
    fn test_constant_selector_hashes() {
        assert_eq!(
            super::erc165_interface_selector([ERC165::supportsInterfaceCall::SELECTOR]),
            super::ERC_165_SUPPORTS_INTERFACE_SELECTOR
        );
        assert_eq!(super::erc165_interface_selector([]), [0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn test_selector_hash_contract() {
        let (_anvil, rpc_provider) = web3::tests::fixture(WithWallet::Yes).await;
        let selector = Selector::deploy(rpc_provider.http())
            .await
            .expect("Should be able to deploy with RPC provider");
        let should_selector = selector
            .calculateSelector()
            .call()
            .await
            .expect("Should be able to calculate selector on deployed instance ");
        assert_eq!(
            should_selector,
            super::erc165_interface_selector([
                Solidity101::helloCall::SELECTOR,
                Solidity101::worldCall::SELECTOR
            ]),
            "Did not match expected selector"
        );
        assert_eq!(
            should_selector,
            super::erc165_interface_selector([
                Solidity101::worldCall::SELECTOR,
                Solidity101::helloCall::SELECTOR
            ]),
            "Should not matter in which order we compute the interface selector"
        );
        assert_ne!(
            should_selector,
            super::erc165_interface_selector([
                Solidity101::worldCall::SELECTOR,
                Solidity101::helloCall::SELECTOR,
                Solidity101::helloCall::SELECTOR
            ]),
            "Should no longer match"
        );
    }

    #[tokio::test]
    async fn test_not_deployed_contract() {
        let (_anvil, rpc_provider) = web3::tests::fixture(WithWallet::No).await;

        let zero_address =
            alloy::primitives::address!("0x0000000000000000000000000000000000000000");
        let (support_interface, is_erc165_conform, support_interface_unchecked) = tokio::join!(
            rpc_provider
                .erc165_supports_interface(zero_address, [ERC165::supportsInterfaceCall::SELECTOR]),
            rpc_provider.is_erc165_conform(zero_address),
            rpc_provider.erc165_supports_interface_unchecked(
                zero_address,
                [ERC165::supportsInterfaceCall::SELECTOR],
            )
        );
        assert!(
            matches!(support_interface, Err(ERC165ConfirmError::NotAContract)),
            "Should fail with NotAContractError"
        );
        assert!(
            matches!(is_erc165_conform, Err(ERC165ConfirmError::NotAContract)),
            "Should fail with NotAContractError"
        );
        assert!(
            matches!(
                support_interface_unchecked,
                Err(ERC165ConfirmError::NotAContract)
            ),
            "Should fail with NotAContractError"
        );
    }

    #[tokio::test]
    async fn test_erc165_confirm() {
        let (_anvil, rpc_provider) = web3::tests::fixture(WithWallet::Yes).await;

        let confirms_erc165_address = *ConfirmsERC165::deploy(rpc_provider.http())
            .await
            .expect("Should be able to deploy with RPC provider")
            .address();
        let (
            support_interface_erc165,
            support_interface_sol101,
            is_erc165_conform,
            support_interface_erc165_unchecked,
            support_interface_sol101_unchecked,
        ) = tokio::join!(
            rpc_provider.erc165_supports_interface(
                confirms_erc165_address,
                [ERC165::supportsInterfaceCall::SELECTOR]
            ),
            rpc_provider.erc165_supports_interface(
                confirms_erc165_address,
                [
                    Solidity101::worldCall::SELECTOR,
                    Solidity101::helloCall::SELECTOR
                ]
            ),
            rpc_provider.is_erc165_conform(confirms_erc165_address),
            rpc_provider.erc165_supports_interface_unchecked(
                confirms_erc165_address,
                [ERC165::supportsInterfaceCall::SELECTOR],
            ),
            rpc_provider.erc165_supports_interface_unchecked(
                confirms_erc165_address,
                [
                    Solidity101::worldCall::SELECTOR,
                    Solidity101::helloCall::SELECTOR
                ],
            )
        );
        assert!(support_interface_erc165.expect("Should be conform"));
        assert!(support_interface_sol101.expect("Should be conform"));
        assert!(is_erc165_conform.expect("Should be conform"));
        assert!(support_interface_erc165_unchecked.expect("Should be conform"));
        assert!(support_interface_sol101_unchecked.expect("Should be conform"));
    }

    #[tokio::test]
    async fn test_erc165_confirm_invalid_interface() {
        let (_anvil, rpc_provider) = web3::tests::fixture(WithWallet::Yes).await;

        let confirms_erc165_address = *ConfirmsInvalidInterface::deploy(rpc_provider.http())
            .await
            .expect("Should be able to deploy with RPC provider")
            .address();
        let (support_interface_erc165, is_erc165_conform, support_interface_erc165_unchecked) = tokio::join!(
            rpc_provider.erc165_supports_interface(
                confirms_erc165_address,
                [ERC165::supportsInterfaceCall::SELECTOR]
            ),
            rpc_provider.is_erc165_conform(confirms_erc165_address),
            rpc_provider.erc165_supports_interface_unchecked(
                confirms_erc165_address,
                [ERC165::supportsInterfaceCall::SELECTOR],
            ),
        );
        assert!(
            matches!(
                support_interface_erc165,
                Err(ERC165ConfirmError::ConfirmsButAlsoToInvalidInterface)
            ),
            "Should fail with ConfirmsButAlsoToInvalidInterface"
        );
        assert!(
            matches!(
                is_erc165_conform,
                Err(ERC165ConfirmError::ConfirmsButAlsoToInvalidInterface)
            ),
            "Should fail with ConfirmsButAlsoToInvalidInterface"
        );
        assert!(
            support_interface_erc165_unchecked.expect("Should work on unchecked call"),
            "Unchecked ERC165 should succeed if confirms to interface"
        );
    }

    #[tokio::test]
    async fn test_erc165_confirm_but_does_not_support_interface() {
        let (_anvil, rpc_provider) = web3::tests::fixture(WithWallet::Yes).await;

        let confirms_erc165_address = *ConfirmsERC165::deploy(rpc_provider.http())
            .await
            .expect("Should be able to deploy with RPC provider")
            .address();
        let (support_interface_sol101, support_interface_sol101_unchecked) = tokio::join!(
            rpc_provider.erc165_supports_interface(
                confirms_erc165_address,
                [
                    Solidity101::worldCall::SELECTOR,
                    Solidity101::helloCall::SELECTOR,
                    Solidity101::helloCall::SELECTOR
                ]
            ),
            rpc_provider.erc165_supports_interface_unchecked(
                confirms_erc165_address,
                [
                    Solidity101::worldCall::SELECTOR,
                    Solidity101::helloCall::SELECTOR,
                    Solidity101::helloCall::SELECTOR
                ],
            )
        );
        assert!(
            !support_interface_sol101.expect("Should be conform"),
            "Should return false is it does not support interface"
        );
        assert!(
            !support_interface_sol101_unchecked.expect("Should be conform"),
            "Should return false is it does not support interface"
        );
    }
}
