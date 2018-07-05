// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Eth rpc implementation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ethereum_types::{Address, H160, H256, H64, U256};
use rlp::{self, Rlp};

use client::Client;
use ethcore::client::{BlockChainClient, BlockId, Call, EngineInfo, StateClient, StateInfo,
                      StateOrBlock, TransactionId};
use ethcore::filter::Filter as EthcoreFilter;
use ethcore::header::BlockNumber as EthBlockNumber;
use ethcore::log_entry::LogEntry;
use ethcore::miner;
use transaction::{LocalizedTransaction, SignedTransaction};

use jsonrpc_core::futures::future;
use jsonrpc_core::{BoxFuture, Result};
use jsonrpc_macros::Trailing;

use parity_rpc::v1::helpers::{errors, fake_sign, limit_logs};
use parity_rpc::v1::metadata::Metadata;
use parity_rpc::v1::traits::Eth;
use parity_rpc::v1::types::{block_number_to_id, Block, BlockNumber, BlockTransactions, Bytes,
                            CallRequest, Filter, H160 as RpcH160, H256 as RpcH256, H64 as RpcH64,
                            Index, Log, Receipt, RichBlock, SyncStatus, Transaction,
                            U256 as RpcU256, Work};

// short for "try_boxfuture"
// unwrap a result, returning a BoxFuture<_, Err> on failure.
macro_rules! try_bf {
    ($res:expr) => {
        match $res {
            Ok(val) => val,
            Err(e) => return Box::new(::jsonrpc_core::futures::future::err(e.into())),
        }
    };
}

/// Eth rpc implementation.
pub struct EthClient {
    client: Arc<Client>,
}

#[derive(Debug)]
enum BlockNumberOrId {
    Number(BlockNumber),
    Id(BlockId),
}

impl From<BlockId> for BlockNumberOrId {
    fn from(value: BlockId) -> BlockNumberOrId {
        BlockNumberOrId::Id(value)
    }
}

impl From<BlockNumber> for BlockNumberOrId {
    fn from(value: BlockNumber) -> BlockNumberOrId {
        BlockNumberOrId::Number(value)
    }
}

enum PendingOrBlock {
    Block(BlockId),
    Pending,
}

struct PendingUncleId {
    id: PendingOrBlock,
    position: usize,
}

enum PendingTransactionId {
    Hash(H256),
    Location(PendingOrBlock, usize),
}

impl EthClient where {
    /// Creates new EthClient.
    pub fn new(client: &Arc<Client>) -> Self {
        EthClient {
            client: client.clone(),
        }
    }

    fn rich_block(&self, id: BlockNumberOrId, include_txs: bool) -> Result<Option<RichBlock>> {
        let client = &self.client;

        let block = match id {
            BlockNumberOrId::Number(num) => {
                // for "pending", just use latest block
                let id = match num {
                    BlockNumber::Latest => BlockId::Latest,
                    BlockNumber::Earliest => BlockId::Earliest,
                    BlockNumber::Num(n) => BlockId::Number(n),
                    BlockNumber::Pending => BlockId::Latest,
                };

                client.block(id)
            }

            BlockNumberOrId::Id(id) => client.block(id),
        };

        match block {
            Some(block) => {
                let view = block.header_view();
                Ok(Some(RichBlock {
                    inner: Block {
                        hash: Some(view.hash().into()),
                        size: Some(block.rlp().as_raw().len().into()),
                        parent_hash: view.parent_hash().into(),
                        uncles_hash: view.uncles_hash().into(),
                        author: view.author().into(),
                        miner: view.author().into(),
                        state_root: view.state_root().into(),
                        transactions_root: view.transactions_root().into(),
                        receipts_root: view.receipts_root().into(),
                        number: Some(view.number().into()),
                        gas_used: view.gas_used().into(),
                        gas_limit: view.gas_limit().into(),
                        logs_bloom: Some(view.log_bloom().into()),
                        timestamp: view.timestamp().into(),
                        difficulty: view.difficulty().into(),
                        total_difficulty: None,
                        seal_fields: view.seal().into_iter().map(Into::into).collect(),
                        uncles: block.uncle_hashes().into_iter().map(Into::into).collect(),
                        transactions: match include_txs {
                            true => BlockTransactions::Full(
                                block
                                    .view()
                                    .localized_transactions()
                                    .into_iter()
                                    .map(|t| Transaction::from_localized(t, 0))
                                    .collect(),
                            ),
                            false => BlockTransactions::Hashes(
                                block
                                    .transaction_hashes()
                                    .into_iter()
                                    .map(Into::into)
                                    .collect(),
                            ),
                        },
                        extra_data: Bytes::new(view.extra_data()),
                    },
                    extra_info: BTreeMap::new(),
                }))
            }
            _ => Ok(None),
        }
    }

    fn transaction(&self, id: PendingTransactionId) -> Result<Option<Transaction>> {
        let client_transaction = |id| match self.client.transaction(id) {
            Some(t) => Ok(Some(Transaction::from_localized(t, 0))),
            None => Ok(None),
        };

        match id {
            PendingTransactionId::Hash(hash) => client_transaction(TransactionId::Hash(hash)),

            PendingTransactionId::Location(PendingOrBlock::Block(block), index) => {
                client_transaction(TransactionId::Location(block, index))
            }

            // we don't have pending blocks
            PendingTransactionId::Location(PendingOrBlock::Pending, index) => {
                return Ok(None);
            }
        }
    }

    fn uncle(&self, id: PendingUncleId) -> Result<Option<RichBlock>> {
        // we don't have uncles
        Ok(None)
    }

    fn get_state(&self, number: BlockNumber) -> StateOrBlock {
        // for "pending", just use latest block
        match number {
            BlockNumber::Num(num) => BlockId::Number(num).into(),
            BlockNumber::Earliest => BlockId::Earliest.into(),
            BlockNumber::Latest => BlockId::Latest.into(),
            BlockNumber::Pending => BlockId::Latest.into(),
        }
    }
}

fn check_known(client: &Client, number: BlockNumber) -> Result<()> {
    use ethcore::block_status::BlockStatus;

    let id = match number {
        BlockNumber::Pending => return Ok(()),

        BlockNumber::Num(n) => BlockId::Number(n),
        BlockNumber::Latest => BlockId::Latest,
        BlockNumber::Earliest => BlockId::Earliest,
    };

    match client.block_status(id) {
        BlockStatus::InChain => Ok(()),
        _ => Err(errors::unknown_block()),
    }
}

impl Eth for EthClient {
    type Metadata = Metadata;

    fn protocol_version(&self) -> Result<String> {
        // TODO: why 63? copied from original contract-evm
        Ok(format!("{}", 63))
    }

    fn syncing(&self) -> Result<SyncStatus> {
        Ok(SyncStatus::None)
    }

    fn author(&self, meta: Metadata) -> Result<RpcH160> {
        Ok(Default::default())
    }

    fn is_mining(&self) -> Result<bool> {
        Ok(true)
    }

    fn hashrate(&self) -> Result<RpcU256> {
        Ok(RpcU256::from(0))
    }

    fn gas_price(&self) -> Result<RpcU256> {
        // TODO: gas model
        Ok(RpcU256::from(0))
    }

    fn accounts(&self, meta: Metadata) -> Result<Vec<RpcH160>> {
        Ok(vec![])
    }

    fn block_number(&self) -> Result<RpcU256> {
        Ok(RpcU256::from(self.client.best_block_number()))
    }

    fn balance(&self, address: RpcH160, num: Trailing<BlockNumber>) -> BoxFuture<RpcU256> {
        let address = address.into();

        let num = num.unwrap_or_default();

        try_bf!(check_known(&*self.client, num.clone()));
        let res = match self.client.balance(&address, self.get_state(num)) {
            Some(balance) => Ok(balance.into()),
            None => Err(errors::state_pruned()),
        };

        Box::new(future::done(res))
    }

    fn storage_at(
        &self,
        address: RpcH160,
        pos: RpcU256,
        num: Trailing<BlockNumber>,
    ) -> BoxFuture<RpcH256> {
        let address: Address = RpcH160::into(address);
        let position: U256 = RpcU256::into(pos);

        let num = num.unwrap_or_default();

        try_bf!(check_known(&*self.client, num.clone()));
        let res = match self.client
            .storage_at(&address, &H256::from(position), self.get_state(num))
        {
            Some(s) => Ok(s.into()),
            None => Err(errors::state_pruned()),
        };

        Box::new(future::done(res))
    }

    fn transaction_count(
        &self,
        address: RpcH160,
        num: Trailing<BlockNumber>,
    ) -> BoxFuture<RpcU256> {
        let address: Address = RpcH160::into(address);

        let res = match num.unwrap_or_default() {
            BlockNumber::Pending => match self.client.nonce(&address, BlockId::Latest) {
                Some(nonce) => Ok(nonce.into()),
                None => Err(errors::database("latest nonce missing")),
            },
            number => {
                try_bf!(check_known(&*self.client, number.clone()));
                match self.client.nonce(&address, block_number_to_id(number)) {
                    Some(nonce) => Ok(nonce.into()),
                    None => Err(errors::state_pruned()),
                }
            }
        };

        Box::new(future::done(res))
    }

    fn block_transaction_count_by_hash(&self, hash: RpcH256) -> BoxFuture<Option<RpcU256>> {
        Box::new(future::ok(
            self.client
                .block(BlockId::Hash(hash.into()))
                .map(|block| block.transactions_count().into()),
        ))
    }

    fn block_transaction_count_by_number(&self, num: BlockNumber) -> BoxFuture<Option<RpcU256>> {
        let block_number = self.client.best_block_number();

        Box::new(future::ok(
            self.client
                .block(block_number_to_id(num))
                .map(|block| block.transactions_count().into()),
        ))
    }

    fn block_uncles_count_by_hash(&self, hash: RpcH256) -> BoxFuture<Option<RpcU256>> {
        // we don't have uncles
        Box::new(future::ok(Some(RpcU256::from(0))))
    }

    fn block_uncles_count_by_number(&self, num: BlockNumber) -> BoxFuture<Option<RpcU256>> {
        // we don't have uncles
        Box::new(future::ok(Some(RpcU256::from(0))))
    }

    fn code_at(&self, address: RpcH160, num: Trailing<BlockNumber>) -> BoxFuture<Bytes> {
        let address: Address = RpcH160::into(address);

        let num = num.unwrap_or_default();
        try_bf!(check_known(&*self.client, num.clone()));

        let res = match self.client.code(&address, self.get_state(num)) {
            Some(code) => Ok(code.map_or_else(Bytes::default, Bytes::new)),
            None => Err(errors::state_pruned()),
        };

        Box::new(future::done(res))
    }

    fn block_by_hash(&self, hash: RpcH256, include_txs: bool) -> BoxFuture<Option<RichBlock>> {
        Box::new(future::done(self.rich_block(
            BlockId::Hash(hash.into()).into(),
            include_txs,
        )))
    }

    fn block_by_number(&self, num: BlockNumber, include_txs: bool) -> BoxFuture<Option<RichBlock>> {
        Box::new(future::done(self.rich_block(num.into(), include_txs)))
    }

    fn transaction_by_hash(&self, hash: RpcH256) -> BoxFuture<Option<Transaction>> {
        let hash: H256 = hash.into();
        let block_number = self.client.best_block_number();
        let tx = try_bf!(self.transaction(PendingTransactionId::Hash(hash)));

        Box::new(future::ok(tx))
    }

    fn transaction_by_block_hash_and_index(
        &self,
        hash: RpcH256,
        index: Index,
    ) -> BoxFuture<Option<Transaction>> {
        let id = PendingTransactionId::Location(
            PendingOrBlock::Block(BlockId::Hash(hash.into())),
            index.value(),
        );
        Box::new(future::done(self.transaction(id)))
    }

    fn transaction_by_block_number_and_index(
        &self,
        num: BlockNumber,
        index: Index,
    ) -> BoxFuture<Option<Transaction>> {
        let block_id = match num {
            BlockNumber::Latest => PendingOrBlock::Block(BlockId::Latest),
            BlockNumber::Earliest => PendingOrBlock::Block(BlockId::Earliest),
            BlockNumber::Num(num) => PendingOrBlock::Block(BlockId::Number(num)),
            BlockNumber::Pending => PendingOrBlock::Pending,
        };

        let transaction_id = PendingTransactionId::Location(block_id, index.value());
        Box::new(future::done(self.transaction(transaction_id)))
    }

    fn transaction_receipt(&self, hash: RpcH256) -> BoxFuture<Option<Receipt>> {
        let best_block = self.client.best_block_number();
        let hash: H256 = hash.into();

        let receipt = self.client.transaction_receipt(TransactionId::Hash(hash));
        Box::new(future::ok(receipt.map(Into::into)))
    }

    fn uncle_by_block_hash_and_index(
        &self,
        hash: RpcH256,
        index: Index,
    ) -> BoxFuture<Option<RichBlock>> {
        // we dont' have uncles
        Box::new(future::ok(None))
    }

    fn uncle_by_block_number_and_index(
        &self,
        num: BlockNumber,
        index: Index,
    ) -> BoxFuture<Option<RichBlock>> {
        // we dont' have uncles
        Box::new(future::ok(None))
    }

    fn compilers(&self) -> Result<Vec<String>> {
        Err(errors::deprecated(
            "Compilation functionality is deprecated.".to_string(),
        ))
    }

    fn logs(&self, filter: Filter) -> BoxFuture<Vec<Log>> {
        let filter: EthcoreFilter = filter.into();
        let mut logs = self.client
            .logs(filter.clone())
            .into_iter()
            .map(From::from)
            .collect::<Vec<Log>>();

        let logs = limit_logs(logs, filter.limit);

        Box::new(future::ok(logs))
    }

    fn work(&self, no_new_work_timeout: Trailing<u64>) -> Result<Work> {
        Err(errors::unimplemented(None))
    }

    fn submit_work(&self, nonce: RpcH64, pow_hash: RpcH256, mix_hash: RpcH256) -> Result<bool> {
        Err(errors::unimplemented(None))
    }

    fn submit_hashrate(&self, rate: RpcU256, id: RpcH256) -> Result<bool> {
        Err(errors::unimplemented(None))
    }

    fn send_raw_transaction(&self, raw: Bytes) -> Result<RpcH256> {
        /*
        Rlp::new(&raw.into_vec()).as_val()
			.map_err(errors::rlp)
			.and_then(|tx| SignedTransaction::new(tx).map_err(errors::transaction))
			.and_then(|signed_transaction| {
				FullDispatcher::dispatch_transaction(
					&*self.client,
					&*self.miner,
					signed_transaction.into(),
				)
			})
			.map(Into::into)
        */
        self.client
            .send_raw_transaction(raw.into())
            .map(Into::into)
            .map_err(errors::call)
    }

    fn submit_transaction(&self, raw: Bytes) -> Result<RpcH256> {
        self.send_raw_transaction(raw)
    }

    fn call(
        &self,
        meta: Self::Metadata,
        request: CallRequest,
        num: Trailing<BlockNumber>,
    ) -> BoxFuture<Bytes> {
        let request = CallRequest::into(request);
        let signed = try_bf!(fake_sign::sign_call(request, meta.is_dapp()));

        let num = num.unwrap_or_default();

        let (mut state, header) = {
            // for "pending", just use latest block
            let id = match num {
                BlockNumber::Num(num) => BlockId::Number(num),
                BlockNumber::Earliest => BlockId::Earliest,
                BlockNumber::Latest => BlockId::Latest,
                BlockNumber::Pending => BlockId::Latest,
            };

            let state = try_bf!(self.client.state_at(id).ok_or(errors::state_pruned()));
            let header = try_bf!(
                self.client
                    .block_header(id)
                    .ok_or(errors::state_pruned())
                    .and_then(|h| h.decode().map_err(errors::decode))
            );

            (state, header)
        };

        let result = self.client
            .call(&signed, Default::default(), &mut state, &header);

        Box::new(future::done(
            result
                .map_err(errors::call)
                .and_then(|executed| match executed.exception {
                    Some(ref exception) => Err(errors::vm(exception, &executed.output)),
                    None => Ok(executed),
                })
                .map(|b| b.output.into()),
        ))
    }

    fn estimate_gas(
        &self,
        meta: Self::Metadata,
        request: CallRequest,
        num: Trailing<BlockNumber>,
    ) -> BoxFuture<RpcU256> {
        let request = CallRequest::into(request);
        let signed = try_bf!(fake_sign::sign_call(request, meta.is_dapp()));
        let num = num.unwrap_or_default();

        let (state, header) = {
            // for "pending", just use latest block
            let id = match num {
                BlockNumber::Num(num) => BlockId::Number(num),
                BlockNumber::Earliest => BlockId::Earliest,
                BlockNumber::Latest => BlockId::Latest,
                BlockNumber::Pending => BlockId::Latest,
            };

            let state = try_bf!(self.client.state_at(id).ok_or(errors::state_pruned()));
            let header = try_bf!(
                self.client
                    .block_header(id)
                    .ok_or(errors::state_pruned())
                    .and_then(|h| h.decode().map_err(errors::decode))
            );

            (state, header)
        };

        Box::new(future::done(
            self.client
                .estimate_gas(&signed, &state, &header)
                .map(Into::into)
                .map_err(errors::call),
        ))
    }

    fn compile_lll(&self, _: String) -> Result<Bytes> {
        Err(errors::deprecated(
            "Compilation of LLL via RPC is deprecated".to_string(),
        ))
    }

    fn compile_serpent(&self, _: String) -> Result<Bytes> {
        Err(errors::deprecated(
            "Compilation of Serpent via RPC is deprecated".to_string(),
        ))
    }

    fn compile_solidity(&self, _: String) -> Result<Bytes> {
        Err(errors::deprecated(
            "Compilation of Solidity via RPC is deprecated".to_string(),
        ))
    }
}
