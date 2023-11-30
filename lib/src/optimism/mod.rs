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

use core::iter::once;

use alloy_sol_types::{sol, SolInterface};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zeth_primitives::{
    address,
    batch::Batch,
    block::Header,
    keccak::keccak,
    transactions::{
        ethereum::{EthereumTxEssence, TransactionKind},
        optimism::{OptimismTxEssence, TxEssenceOptimismDeposited},
        Transaction, TxEssence,
    },
    trie::MptNode,
    uint, Address, BlockHash, BlockNumber, FixedBytes, RlpBytes, B256, U256,
};

#[cfg(not(target_os = "zkvm"))]
use log::info;

use crate::optimism::{
    batches::Batches,
    config::ChainConfig,
    derivation::{BlockInfo, Epoch, State, CHAIN_SPEC},
    epoch::BlockInput,
};

pub mod batcher_transactions;
pub mod batches;
pub mod channels;
pub mod config;
pub mod deposits;
pub mod derivation;
pub mod epoch;
pub mod system_config;

sol! {
    #[derive(Debug)]
    interface OpSystemInfo {
        function setL1BlockValues(
            uint64 number,
            uint64 timestamp,
            uint256 basefee,
            bytes32 hash,
            uint64 sequence_number,
            bytes32 batcher_hash,
            uint256 l1_fee_overhead,
            uint256 l1_fee_scalar
        );
    }
}

pub trait BatcherDb {
    fn get_full_op_block(&mut self, block_no: u64) -> Result<BlockInput<OptimismTxEssence>>;
    fn get_op_block_header(&mut self, block_no: u64) -> Result<Header>;
    fn get_full_eth_block(&mut self, block_no: u64) -> Result<BlockInput<EthereumTxEssence>>;
    fn get_eth_block_header(&mut self, block_no: u64) -> Result<Header>;
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemDb {
    pub full_op_block: HashMap<u64, BlockInput<OptimismTxEssence>>,
    pub op_block_header: HashMap<u64, Header>,
    pub full_eth_block: HashMap<u64, BlockInput<EthereumTxEssence>>,
    pub eth_block_header: HashMap<u64, Header>,
}

impl MemDb {
    pub fn new() -> Self {
        MemDb {
            full_op_block: HashMap::new(),
            op_block_header: HashMap::new(),
            full_eth_block: HashMap::new(),
            eth_block_header: HashMap::new(),
        }
    }
}

impl Default for MemDb {
    fn default() -> Self {
        Self::new()
    }
}

impl BatcherDb for MemDb {
    fn get_full_op_block(&mut self, block_no: u64) -> Result<BlockInput<OptimismTxEssence>> {
        let op_block = self.full_op_block.remove(&block_no).unwrap();
        assert_eq!(block_no, op_block.block_header.number);

        // Validate tx list
        {
            let mut tx_trie = MptNode::default();
            for (tx_no, tx) in op_block.transactions.iter().enumerate() {
                let trie_key = tx_no.to_rlp();
                tx_trie.insert_rlp(&trie_key, tx)?;
            }
            if tx_trie.hash() != op_block.block_header.transactions_root {
                bail!("Invalid op block transaction data!")
            }
        }

        Ok(op_block)
    }

    fn get_op_block_header(&mut self, block_no: u64) -> Result<Header> {
        let op_block = self.op_block_header.remove(&block_no).unwrap();
        assert_eq!(block_no, op_block.number);

        Ok(op_block)
    }

    fn get_full_eth_block(&mut self, block_no: u64) -> Result<BlockInput<EthereumTxEssence>> {
        let eth_block = self.full_eth_block.remove(&block_no).unwrap();
        assert_eq!(block_no, eth_block.block_header.number);

        // Validate tx list
        {
            let mut tx_trie = MptNode::default();
            for (tx_no, tx) in eth_block.transactions.iter().enumerate() {
                let trie_key = tx_no.to_rlp();
                tx_trie.insert_rlp(&trie_key, tx)?;
            }
            if tx_trie.hash() != eth_block.block_header.transactions_root {
                bail!("Invalid eth block transaction data!")
            }
        }

        // Validate receipts
        if eth_block.receipts.is_some() {
            let mut receipt_trie = MptNode::default();
            for (tx_no, receipt) in eth_block.receipts.as_ref().unwrap().iter().enumerate() {
                let trie_key = tx_no.to_rlp();
                receipt_trie.insert_rlp(&trie_key, receipt)?;
            }
            if receipt_trie.hash() != eth_block.block_header.receipts_root {
                bail!("Invalid eth block receipt data!")
            }
        } else {
            let can_contain_deposits = deposits::can_contain(
                &CHAIN_SPEC.deposit_contract,
                &eth_block.block_header.logs_bloom,
            );
            let can_contain_config = system_config::can_contain(
                &CHAIN_SPEC.system_config_contract,
                &eth_block.block_header.logs_bloom,
            );
            assert!(!can_contain_deposits);
            assert!(!can_contain_config);
        }

        Ok(eth_block)
    }

    fn get_eth_block_header(&mut self, block_no: u64) -> Result<Header> {
        let eth_block = self.eth_block_header.remove(&block_no).unwrap();
        assert_eq!(block_no, eth_block.number);

        Ok(eth_block)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeriveInput<D> {
    pub db: D,
    pub op_head_block_no: u64,
    pub op_derive_block_count: u64,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveOutput {
    pub eth_tail: (BlockNumber, BlockHash),
    pub op_head: (BlockNumber, BlockHash),
    pub derived_op_blocks: Vec<(BlockNumber, BlockHash)>,
}

pub struct DeriveMachine<D> {
    pub derive_input: DeriveInput<D>,
    op_head_block_hash: BlockHash,
    op_block_no: u64,
    op_block_seq_no: u64,
    op_batches: Batches,
    eth_block_no: u64,
}

impl<D: BatcherDb> DeriveMachine<D> {
    pub fn new(mut derive_input: DeriveInput<D>) -> Result<Self> {
        let op_block_no = derive_input.op_head_block_no;

        // read system config from op_head (seq_no/epoch_no..etc)
        let op_head = derive_input.db.get_full_op_block(op_block_no)?;
        let op_head_block_hash = op_head.block_header.hash();

        #[cfg(not(target_os = "zkvm"))]
        info!(
            "Fetched Op head (block no {}) {}",
            derive_input.op_head_block_no, op_head_block_hash
        );

        let set_l1_block_values = {
            let system_tx_data = op_head
                .transactions
                .first()
                .unwrap()
                .essence
                .data()
                .to_vec();
            let call = OpSystemInfo::OpSystemInfoCalls::abi_decode(&system_tx_data, true)
                .expect("Could not decode call data");
            match call {
                OpSystemInfo::OpSystemInfoCalls::setL1BlockValues(x) => x,
            }
        };

        let op_block_seq_no = set_l1_block_values.sequence_number;

        let eth_block_no = set_l1_block_values.number;
        let eth_head = derive_input.db.get_eth_block_header(eth_block_no)?;
        let eth_head_hash = eth_head.hash();
        if eth_head_hash != set_l1_block_values.hash.as_slice() {
            bail!("Ethereum head block hash mismatch.")
        }
        #[cfg(not(target_os = "zkvm"))]
        info!(
            "Fetched Eth head (block no {}) {}",
            eth_block_no, set_l1_block_values.hash
        );

        let op_batches = {
            let mut op_chain_config = ChainConfig::optimism();
            op_chain_config.system_config.batch_sender =
                Address::from_slice(&set_l1_block_values.batcher_hash.as_slice()[12..]);
            op_chain_config.system_config.l1_fee_overhead = set_l1_block_values.l1_fee_overhead;
            op_chain_config.system_config.l1_fee_scalar = set_l1_block_values.l1_fee_scalar;

            Batches::new(
                op_chain_config,
                State::new(
                    eth_block_no,
                    eth_head_hash,
                    BlockInfo {
                        hash: op_head_block_hash,
                        timestamp: op_head.block_header.timestamp.try_into().unwrap(),
                    },
                    Epoch {
                        number: eth_block_no,
                        hash: eth_head_hash,
                        timestamp: eth_head.timestamp.try_into().unwrap(),
                        base_fee_per_gas: eth_head.base_fee_per_gas,
                        deposits: Vec::new(),
                    },
                ),
            )
        };

        Ok(DeriveMachine {
            derive_input,
            op_head_block_hash,
            op_block_no,
            op_block_seq_no,
            op_batches,
            eth_block_no,
        })
    }

    pub fn derive(&mut self) -> Result<DeriveOutput> {
        let target_block_no =
            self.derive_input.op_head_block_no + self.derive_input.op_derive_block_count;

        let mut derived_op_blocks = Vec::new();

        while self.op_block_no < target_block_no {
            #[cfg(not(target_os = "zkvm"))]
            info!(
                "op_block_no = {}, eth_block_no = {}",
                self.op_block_no, self.eth_block_no
            );

            // Process next Eth block
            self.process_next_eth_block()?;

            // Process batches
            while let Some(op_batch) = self.op_batches.next() {
                // Process the batch
                self.op_block_no += 1;

                #[cfg(not(target_os = "zkvm"))]
                info!(
                    "Read batch for Op block {}: timestamp={}, epoch={}, tx count={}, parent hash={:?}",
                    self.op_block_no,
                    op_batch.essence.timestamp,
                    op_batch.essence.epoch_num,
                    op_batch.essence.transactions.len(),
                    op_batch.essence.parent_hash,
                );

                // Update sequence number (and fetch deposits if start of new epoch)
                let deposits =
                    if op_batch.essence.epoch_num == self.op_batches.state.epoch.number + 1 {
                        self.op_block_seq_no = 0;
                        self.op_batches.state.do_next_epoch()?;

                        self.op_batches
                            .state
                            .epoch
                            .deposits
                            .iter()
                            .map(|tx| tx.to_rlp())
                            .collect()
                    } else {
                        self.op_block_seq_no += 1;

                        Vec::new()
                    };

                // Obtain new Op head
                let new_op_head = {
                    let new_op_head = self
                        .derive_input
                        .db
                        .get_op_block_header(self.op_block_no)
                        .context("block not found")?;

                    // Verify new op head has the expected parent
                    assert_eq!(
                        new_op_head.parent_hash,
                        self.op_batches.state.safe_head.hash
                    );

                    // Verify that the new op head transactions are consistent with the batch transactions
                    {
                        let system_tx = self.derive_system_transaction(&op_batch);

                        let derived_transactions: Vec<_> = once(system_tx.to_rlp())
                            .chain(deposits)
                            .chain(op_batch.essence.transactions.iter().map(|tx| tx.to_vec()))
                            .collect();

                        let mut tx_trie = MptNode::default();
                        for (tx_no, tx) in derived_transactions.into_iter().enumerate() {
                            let trie_key = tx_no.to_rlp();
                            tx_trie.insert(&trie_key, tx)?;
                        }
                        if tx_trie.hash() != new_op_head.transactions_root {
                            bail!("Invalid op block transaction data! Transaction trie root does not match")
                        }
                    }

                    new_op_head
                };

                let new_op_head_hash = new_op_head.hash();

                #[cfg(not(target_os = "zkvm"))]
                info!(
                    "Derived Op block {} w/ hash {}",
                    new_op_head.number, new_op_head_hash
                );

                self.op_batches.state.safe_head = BlockInfo {
                    hash: new_op_head_hash,
                    timestamp: new_op_head.timestamp.try_into().unwrap(),
                };

                derived_op_blocks.push((new_op_head.number, new_op_head_hash));

                if self.op_block_no == target_block_no {
                    break;
                }
            }
        }

        Ok(DeriveOutput {
            eth_tail: (
                self.op_batches.state.current_l1_block_number,
                self.op_batches.state.current_l1_block_hash,
            ),
            op_head: (self.derive_input.op_head_block_no, self.op_head_block_hash),
            derived_op_blocks,
        })
    }

    fn process_next_eth_block(&mut self) -> Result<()> {
        let eth_block = self
            .derive_input
            .db
            .get_full_eth_block(self.eth_block_no)
            .context("block not found")?;
        let eth_block_hash = eth_block.block_header.hash();

        // Ensure block has correct parent
        if self.op_batches.state.current_l1_block_number < self.eth_block_no {
            assert_eq!(
                eth_block.block_header.parent_hash,
                self.op_batches.state.current_l1_block_hash,
            );
        }

        // Update the system config
        if eth_block.receipts.is_some() {
            #[cfg(not(target_os = "zkvm"))]
            info!("Process config");

            self.op_batches
                .config
                .system_config
                .update(&self.op_batches.config.system_config_contract, &eth_block)
                .context("failed to update system config")?;
        }

        // Enqueue epoch
        self.op_batches.state.push_epoch(Epoch {
            number: self.eth_block_no,
            hash: eth_block_hash,
            timestamp: eth_block.block_header.timestamp.try_into().unwrap(),
            base_fee_per_gas: eth_block.block_header.base_fee_per_gas,
            deposits: deposits::extract_transactions(&self.op_batches.config, &eth_block)?,
        })?;

        // Process batcher transactions
        self.op_batches
            .process(&eth_block)
            .context("failed to create batcher transactions")?;

        self.op_batches.state.current_l1_block_number = self.eth_block_no;
        self.op_batches.state.current_l1_block_hash = eth_block_hash;
        self.eth_block_no += 1;

        Ok(())
    }

    fn derive_system_transaction(&mut self, op_batch: &Batch) -> Transaction<OptimismTxEssence> {
        let batcher_hash = {
            let all_zero: FixedBytes<12> = FixedBytes([0_u8; 12]);
            all_zero.concat_const::<20, 32>(self.op_batches.config.system_config.batch_sender.0)
        };

        let set_l1_block_values =
            OpSystemInfo::OpSystemInfoCalls::setL1BlockValues(OpSystemInfo::setL1BlockValuesCall {
                number: self.op_batches.state.epoch.number,
                timestamp: self.op_batches.state.epoch.timestamp,
                basefee: self.op_batches.state.epoch.base_fee_per_gas,
                hash: self.op_batches.state.epoch.hash,
                sequence_number: self.op_block_seq_no,
                batcher_hash,
                l1_fee_overhead: self.op_batches.config.system_config.l1_fee_overhead,
                l1_fee_scalar: self.op_batches.config.system_config.l1_fee_scalar,
            });

        let source_hash: B256 = {
            let source_hash_sequencing = keccak(
                &[
                    op_batch.essence.epoch_hash.to_vec(),
                    U256::from(self.op_block_seq_no).to_be_bytes_vec(),
                ]
                .concat(),
            );
            keccak(
                &[
                    [0u8; 31].as_slice(),
                    [1u8].as_slice(),
                    source_hash_sequencing.as_slice(),
                ]
                .concat(),
            )
            .into()
        };

        Transaction {
            essence: OptimismTxEssence::OptimismDeposited(TxEssenceOptimismDeposited {
                source_hash,
                from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
                to: TransactionKind::Call(address!("4200000000000000000000000000000000000015")),
                mint: Default::default(),
                value: Default::default(),
                gas_limit: uint!(1_000_000_U256),
                is_system_tx: false,
                data: set_l1_block_values.abi_encode().into(),
            }),
            signature: Default::default(),
        }
    }
}
