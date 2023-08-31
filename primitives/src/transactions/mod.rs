// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloy_primitives::{TxHash, B160};
use alloy_rlp::Encodable;
use serde::{Deserialize, Serialize};

use crate::{
    keccak::keccak, signature::TxSignature, transactions::ethereum::EthereumTxEssence, U256,
};

pub mod ethereum;

/// Represents a complete Ethereum transaction, encompassing its core essence and the
/// associated signature.
///
/// The `Transaction` struct encapsulates both the core details of the transaction (the
/// essence) and its cryptographic signature. The signature ensures the authenticity and
/// integrity of the transaction, confirming it was issued by the rightful sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    /// The core details of the transaction, which include its type (e.g., legacy,
    /// EIP-2930, EIP-1559) and associated data (e.g., recipient address, value, gas
    /// details).
    pub essence: EthereumTxEssence,
    /// The cryptographic signature associated with the transaction, generated by signing
    /// the transaction essence.
    pub signature: TxSignature,
}

pub trait TxEssence {
    /// Determines the type of the transaction based on its essence.
    ///
    /// Returns a byte representing the transaction type:
    /// - `0x00` for Legacy transactions.
    /// - `0x01` for EIP-2930 transactions.
    /// - `0x02` for EIP-1559 transactions.
    fn tx_type(&self) -> u8;
    /// Retrieves the gas limit set for the transaction.
    ///
    /// The gas limit represents the maximum amount of gas units that the transaction
    /// is allowed to consume. It ensures that transactions don't run indefinitely.
    fn gas_limit(&self) -> U256;
    /// Retrieves the recipient address of the transaction, if available.
    ///
    /// For contract creation transactions, this method returns `None` as there's no
    /// recipient address.
    fn to(&self) -> Option<B160>;
    /// Recovers the Ethereum address of the sender from the transaction's signature.
    ///
    /// This method uses the ECDSA recovery mechanism to derive the sender's public key
    /// and subsequently their Ethereum address. If the recovery is unsuccessful, an
    /// error is returned.
    fn recover_from(&self, signature: &TxSignature) -> anyhow::Result<B160>;
}

/// Provides RLP encoding functionality for the [Transaction] struct.
///
/// This implementation ensures that the entire transaction, including its essence and
/// signature, can be RLP-encoded. The encoding process also considers the EIP-2718
/// transaction type.
impl Encodable for Transaction {
    /// Encodes the [Transaction] struct into the provided `out` buffer.
    ///
    /// The encoding process starts by prepending the EIP-2718 transaction type, if
    /// applicable. It then joins the RLP lists of the transaction essence and the
    /// signature into a single list. This approach optimizes the encoding process by
    /// reusing as much of the generated RLP code as possible.
    #[inline]
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        // prepend the EIP-2718 transaction type
        match self.essence.tx_type() {
            0 => {}
            tx_type => out.put_u8(tx_type),
        }

        // join the essence lists and the signature list into one
        rlp_join_lists(&self.essence, &self.signature, out);
    }

    /// Computes the length of the RLP-encoded [Transaction] struct in bytes.
    ///
    /// The computed length includes the lengths of the encoded transaction essence and
    /// signature. If the transaction type (as per EIP-2718) is not zero, an
    /// additional byte is added to the length.
    #[inline]
    fn length(&self) -> usize {
        let payload_length = self.essence.payload_length() + self.signature.payload_length();
        let mut length = payload_length + alloy_rlp::length_of_length(payload_length);
        if self.essence.tx_type() != 0 {
            length += 1;
        }
        length
    }
}

impl Transaction {
    /// Calculates the Keccak hash of the RLP-encoded transaction.
    ///
    /// This hash uniquely identifies the transaction on the Ethereum network.
    pub fn hash(&self) -> TxHash {
        keccak(alloy_rlp::encode(self)).into()
    }

    /// Recovers the Ethereum address of the sender from the transaction's signature.
    ///
    /// This method uses the ECDSA recovery mechanism to derive the sender's public key
    /// and subsequently their Ethereum address. If the recovery is unsuccessful, an
    /// error is returned.
    pub fn recover_from(&self) -> anyhow::Result<B160> {
        self.essence.recover_from(&self.signature)
    }
}

/// Joins two RLP-encoded lists into a single RLP-encoded list.
///
/// This function takes two RLP-encoded lists, decodes their headers to ensure they are
/// valid lists, and then combines their payloads into a single RLP-encoded list. The
/// resulting list is written to the provided `out` buffer.
///
/// # Arguments
///
/// * `a` - The first RLP-encoded list to be joined.
/// * `b` - The second RLP-encoded list to be joined.
/// * `out` - The buffer where the resulting RLP-encoded list will be written.
///
/// # Panics
///
/// This function will panic if either `a` or `b` are not valid RLP-encoded lists.
fn rlp_join_lists(a: impl Encodable, b: impl Encodable, out: &mut dyn alloy_rlp::BufMut) {
    let a_buf = alloy_rlp::encode(a);
    let header = alloy_rlp::Header::decode(&mut &a_buf[..]).unwrap();
    if !header.list {
        panic!("`a` not a list");
    }
    let a_head_length = header.length();
    let a_payload_length = a_buf.len() - a_head_length;

    let b_buf = alloy_rlp::encode(b);
    let header = alloy_rlp::Header::decode(&mut &b_buf[..]).unwrap();
    if !header.list {
        panic!("`b` not a list");
    }
    let b_head_length = header.length();
    let b_payload_length = b_buf.len() - b_head_length;

    alloy_rlp::Header {
        list: true,
        payload_length: a_payload_length + b_payload_length,
    }
    .encode(out);
    out.put_slice(&a_buf[a_head_length..]); // skip the header
    out.put_slice(&b_buf[b_head_length..]); // skip the header
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn legacy() {
        // Tx: 0x5c504ed432cb51138bcf09aa5e8a410dd4a1e204ef84bfed1be16dfba1b22060
        let tx = json!({
                "Legacy": {
                    "nonce": 0,
                    "gas_price": "0x2d79883d2000",
                    "gas_limit": "0x5208",
                    "to": { "Call": "0x5df9b87991262f6ba471f09758cde1c0fc1de734" },
                    "value": "0x7a69",
                    "data": "0x"
                  }
        });
        let essence: EthereumTxEssence = serde_json::from_value(tx).unwrap();

        let signature: TxSignature = serde_json::from_value(json!({
            "v": 28,
            "r": "0x88ff6cf0fefd94db46111149ae4bfc179e9b94721fffd821d38d16464b3f71d0",
            "s": "0x45e0aff800961cfce805daef7016b9b675c137a6a41a548f7b60a3484c06a33a"
        }))
        .unwrap();
        let transaction = Transaction { essence, signature };

        // verify that bincode serialization works
        let _: Transaction =
            bincode::deserialize(&bincode::serialize(&transaction).unwrap()).unwrap();

        assert_eq!(
            "0x5c504ed432cb51138bcf09aa5e8a410dd4a1e204ef84bfed1be16dfba1b22060",
            transaction.hash().to_string()
        );
        let recovered = transaction.recover_from().unwrap();
        assert_eq!(
            "0xa1e4380a3b1f749673e270229993ee55f35663b4",
            recovered.to_string()
        );
    }

    #[test]
    fn eip155() {
        // Tx: 0x4540eb9c46b1654c26353ac3c65e56451f711926982ce1b02f15c50e7459caf7
        let tx = json!({
                "Legacy": {
                    "nonce": 537760,
                    "gas_price": "0x03c49bfa04",
                    "gas_limit": "0x019a28",
                    "to": { "Call": "0xf0ee707731d1be239f9f482e1b2ea5384c0c426f" },
                    "value": "0x06df842eaa9fb800",
                    "data": "0x",
                    "chain_id": 1
                  }
        });
        let essence: EthereumTxEssence = serde_json::from_value(tx).unwrap();

        let signature: TxSignature = serde_json::from_value(json!({
            "v": 38,
            "r": "0xcadd790a37b78e5613c8cf44dc3002e3d7f06a5325d045963c708efe3f9fdf7a",
            "s": "0x1f63adb9a2d5e020c6aa0ff64695e25d7d9a780ed8471abe716d2dc0bf7d4259"
        }))
        .unwrap();
        let transaction = Transaction { essence, signature };

        // verify that bincode serialization works
        let _: Transaction =
            bincode::deserialize(&bincode::serialize(&transaction).unwrap()).unwrap();

        assert_eq!(
            "0x4540eb9c46b1654c26353ac3c65e56451f711926982ce1b02f15c50e7459caf7",
            transaction.hash().to_string()
        );
        let recovered = transaction.recover_from().unwrap();
        assert_eq!(
            "0x974caa59e49682cda0ad2bbe82983419a2ecc400",
            recovered.to_string()
        );
    }

    #[test]
    fn eip2930() {
        // Tx: 0xbe4ef1a2244e99b1ef518aec10763b61360be22e3b649dcdf804103719b1faef
        let tx = json!({
          "Eip2930": {
            "chain_id": 1,
            "nonce": 93847,
            "gas_price": "0xf46a5a9d8",
            "gas_limit": "0x21670",
            "to": { "Call": "0xc11ce44147c9f6149fbe54adb0588523c38718d7" },
            "value": "0x10d1471",
            "data": "0x050000000002b8809aef26206090eafd7d5688615d48197d1c5ce09be6c30a33be4c861dee44d13f6dd33c2e8c5cad7e2725f88a8f0000000002d67ca5eb0e5fb6",
            "access_list": [
              {
                "address": "0xd6e64961ba13ba42858ad8a74ed9a9b051a4957d",
                "storage_keys": [
                  "0x0000000000000000000000000000000000000000000000000000000000000008",
                  "0x0b4b38935f88a7bddbe6be76893de2a04640a55799d6160729a82349aff1ffae",
                  "0xc59ee2ee2ba599569b2b1f06989dadbec5ee157c8facfe64f36a3e33c2b9d1bf"
                ]
              },
              {
                "address": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "storage_keys": [
                  "0x7635825e4f8dfeb20367f8742c8aac958a66caa001d982b3a864dcc84167be80",
                  "0x42555691810bdf8f236c31de88d2cc9407a8ff86cd230ba3b7029254168df92a",
                  "0x29ece5a5f4f3e7751868475502ab752b5f5fa09010960779bf7204deb72f5dde"
                ]
              },
              {
                "address": "0x4c861dee44d13f6dd33c2e8c5cad7e2725f88a8f",
                "storage_keys": [
                  "0x000000000000000000000000000000000000000000000000000000000000000c",
                  "0x0000000000000000000000000000000000000000000000000000000000000008",
                  "0x0000000000000000000000000000000000000000000000000000000000000006",
                  "0x0000000000000000000000000000000000000000000000000000000000000007"
                ]
              },
              {
                "address": "0x90eafd7d5688615d48197d1c5ce09be6c30a33be",
                "storage_keys": [
                  "0x0000000000000000000000000000000000000000000000000000000000000001",
                  "0x9c04773acff4c5c42718bd0120c72761f458e43068a3961eb935577d1ed4effb",
                  "0x0000000000000000000000000000000000000000000000000000000000000008",
                  "0x0000000000000000000000000000000000000000000000000000000000000000",
                  "0x0000000000000000000000000000000000000000000000000000000000000004"
                ]
              }
            ]
          }
        });
        let essence: EthereumTxEssence = serde_json::from_value(tx).unwrap();

        let signature: TxSignature = serde_json::from_value(json!({
            "v": 1,
            "r": "0xf86aa2dfde99b0d6a41741e96cfcdee0c6271febd63be4056911db19ae347e66",
            "s": "0x601deefbc4835cb15aa1af84af6436fc692dea3428d53e7ff3d34a314cefe7fc"
        }))
        .unwrap();
        let transaction = Transaction { essence, signature };

        // verify that bincode serialization works
        let _: Transaction =
            bincode::deserialize(&bincode::serialize(&transaction).unwrap()).unwrap();

        assert_eq!(
            "0xbe4ef1a2244e99b1ef518aec10763b61360be22e3b649dcdf804103719b1faef",
            transaction.hash().to_string()
        );
        let recovered = transaction.recover_from().unwrap();
        assert_eq!(
            "0x79b7a69d90c82e014bf0315e164208119b510fa0",
            recovered.to_string()
        );
    }

    #[test]
    fn eip1559() {
        // Tx: 0x2bcdc03343ca9c050f8dfd3c87f32db718c762ae889f56762d8d8bdb7c5d69ff
        let tx = json!({
                "Eip1559": {
                  "chain_id": 1,
                  "nonce": 32,
                  "max_priority_fee_per_gas": "0x3b9aca00",
                  "max_fee_per_gas": "0x89d5f3200",
                  "gas_limit": "0x5b04",
                  "to": { "Call": "0xa9d1e08c7793af67e9d92fe308d5697fb81d3e43" },
                  "value": "0x1dd1f234f68cde2",
                  "data": "0x",
                  "access_list": []
                }
        });
        let essence: EthereumTxEssence = serde_json::from_value(tx).unwrap();

        let signature: TxSignature = serde_json::from_value(json!({
            "v": 0,
            "r": "0x2bdf47562da5f2a09f09cce70aed35ec9ac62f5377512b6a04cc427e0fda1f4d",
            "s": "0x28f9311b515a5f17aa3ad5ea8bafaecfb0958801f01ca11fd593097b5087121b"
        }))
        .unwrap();
        let transaction = Transaction { essence, signature };

        // verify that bincode serialization works
        let _: Transaction =
            bincode::deserialize(&bincode::serialize(&transaction).unwrap()).unwrap();

        assert_eq!(
            "0x2bcdc03343ca9c050f8dfd3c87f32db718c762ae889f56762d8d8bdb7c5d69ff",
            transaction.hash().to_string()
        );
        let recovered = transaction.recover_from().unwrap();
        assert_eq!(
            "0x4b9f4114d50e7907bff87728a060ce8d53bf4cf7",
            recovered.to_string()
        );
    }

    #[test]
    fn rlp() {
        // Tx: 0x275631a3549307b2e8c93b18dfcc0fe8aedf0276bb650c28eaa0a8a011d18867
        let tx = json!({
                "Eip1559": {
                  "chain_id": 1,
                  "nonce": 267,
                  "max_priority_fee_per_gas": "0x05f5e100",
                  "max_fee_per_gas": "0x0cb2bf61c2",
                  "gas_limit": "0x0278be",
                  "to": { "Call": "0x00005ea00ac477b1030ce78506496e8c2de24bf5" },
                  "value": "0x01351609ff758000",
                  "data": "0x161ac21f0000000000000000000000007de6f03b8b50b835f706e51a40b3224465802ddc0000000000000000000000000000a26b00c1f0df003000390027140000faa71900000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003360c6ebe",
                  "access_list": []
                }
        });
        let essence: EthereumTxEssence = serde_json::from_value(tx).unwrap();

        let encoded = alloy_rlp::encode(&essence);
        assert_eq!(encoded.len(), essence.length());
        assert_eq!(
            essence.payload_length() + alloy_rlp::length_of_length(essence.payload_length()),
            encoded.len()
        );

        let signature: TxSignature = serde_json::from_value(json!({
            "v": 0,
            "r": "0x5fc1441d3469a16715c862240794ef76656c284930e08820b79fd703a98b380a",
            "s": "0x37488b0ceef613dc68116ed44b8e63769dbcf039222e25acc1cb9e85e777ade2"
        }))
        .unwrap();

        let encoded = alloy_rlp::encode(&signature);
        assert_eq!(encoded.len(), signature.length());
        assert_eq!(
            signature.payload_length() + alloy_rlp::length_of_length(signature.payload_length()),
            encoded.len()
        );

        let transaction = Transaction { essence, signature };

        let encoded = alloy_rlp::encode(&transaction);
        assert_eq!(encoded.len(), transaction.length());

        assert_eq!(
            "0x275631a3549307b2e8c93b18dfcc0fe8aedf0276bb650c28eaa0a8a011d18867",
            transaction.hash().to_string()
        );
    }
}
