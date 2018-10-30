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

// Based on parity/rpc/src/v1/impls/eth_pubsub.rs [v1.12.0]

//! Eth PUB-SUB rpc implementation.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use jsonrpc_core::futures::Future;
use jsonrpc_core::Result;
use jsonrpc_macros::pubsub::{Sink, Subscriber};
use jsonrpc_macros::Trailing;
use jsonrpc_pubsub::SubscriptionId;

use parity_rpc::v1::helpers::{errors, Subscribers};
use parity_rpc::v1::metadata::Metadata;
use parity_rpc::v1::traits::EthPubSub;
use parity_rpc::v1::types::{pubsub, H256, H64, Log, RichHeader};

use ethcore::encoded;
use ethcore::filter::Filter as EthFilter;
use ethcore::ids::BlockId;
use parity_reactor::Remote;
use parking_lot::RwLock;

use client::{ChainNotify, Client};

type PubSubClient = Sink<pubsub::Result>;

/// Eth PubSub implementation.
pub struct EthPubSubClient {
    handler: Arc<ChainNotificationHandler>,
    heads_subscribers: Arc<RwLock<Subscribers<PubSubClient>>>,
    logs_subscribers: Arc<RwLock<Subscribers<(PubSubClient, EthFilter)>>>,
}

impl EthPubSubClient {
    /// Creates new `EthPubSubClient`.
    pub fn new(client: Arc<Client>, remote: Remote) -> Self {
        let heads_subscribers = Arc::new(RwLock::new(Subscribers::default()));
        let logs_subscribers = Arc::new(RwLock::new(Subscribers::default()));

        EthPubSubClient {
            handler: Arc::new(ChainNotificationHandler {
                client,
                remote,
                heads_subscribers: heads_subscribers.clone(),
                logs_subscribers: logs_subscribers.clone(),
            }),
            heads_subscribers,
            logs_subscribers,
        }
    }

    /// Returns a chain notification handler.
    pub fn handler(&self) -> Weak<ChainNotificationHandler> {
        Arc::downgrade(&self.handler)
    }
}

/// PubSub Notification handler.
pub struct ChainNotificationHandler {
    client: Arc<Client>,
    remote: Remote,
    heads_subscribers: Arc<RwLock<Subscribers<PubSubClient>>>,
    logs_subscribers: Arc<RwLock<Subscribers<(PubSubClient, EthFilter)>>>,
}

impl ChainNotificationHandler {
    fn notify(remote: &Remote, subscriber: &PubSubClient, result: pubsub::Result) {
        remote.spawn(
            subscriber
                .notify(Ok(result))
                .map(|_| ())
                .map_err(|e| warn!(target: "rpc", "Unable to send notification: {}", e)),
        );
    }
}

impl ChainNotify for ChainNotificationHandler {
    fn has_heads_subscribers(&self) -> bool {
        !self.heads_subscribers.read().is_empty()
    }

    fn notify_heads(&self, headers: &[encoded::Header]) {
        for subscriber in self.heads_subscribers.read().values() {
            for &ref header in headers {
                // geth will fail to decode the response unless it has a number of
                // fields even if they aren't relevant.
                //
                // See:
                //  * https://github.com/ethereum/go-ethereum/issues/3230
                //  * https://github.com/paritytech/parity-ethereum/issues/8841
                let mut extra_info: BTreeMap<String, String> = BTreeMap::new();
                extra_info.insert("mixHash".to_string(), format!("0x{:?}", H256::default()));
                extra_info.insert("nonce".to_string(), format!("0x{:?}", H64::default()));

                Self::notify(
                    &self.remote,
                    subscriber,
                    pubsub::Result::Header(RichHeader {
                        inner: header.into(),
                        extra_info,
                    }),
                );
            }
        }
    }

    fn notify_logs(&self, from_block: BlockId, to_block: BlockId) {
        for &(ref subscriber, ref filter) in self.logs_subscribers.read().values() {
            let mut filter = filter.clone();

            // if filter.from_block == "Latest", replace with from_block
            if filter.from_block == BlockId::Latest {
                filter.from_block = from_block;
            }
            // if filter.to_block == "Latest", replace with to_block
            if filter.to_block == BlockId::Latest {
                filter.to_block = to_block;
            }

            // limit query to range (from_block, to_block)
            filter.from_block = self.client.max_block_number(filter.from_block, from_block);
            filter.to_block = self.client.min_block_number(filter.to_block, to_block);

            let remote = self.remote.clone();
            let subscriber = subscriber.clone();
            self.remote.spawn({
                let logs = self.client
                    .logs(filter)
                    .into_iter()
                    .map(From::from)
                    .collect::<Vec<Log>>();
                for log in logs {
                    Self::notify(&remote, &subscriber, pubsub::Result::Log(log))
                }
                Ok(())
            });
        }
    }
}

impl EthPubSub for EthPubSubClient {
    type Metadata = Metadata;

    fn subscribe(
        &self,
        _meta: Metadata,
        subscriber: Subscriber<pubsub::Result>,
        kind: pubsub::Kind,
        params: Trailing<pubsub::Params>,
    ) {
        measure_counter_inc!("subscribe");
        info!(
            "eth_subscribe(subscriber: {:?}, kind: {:?})",
            subscriber, kind
        );
        let error = match (kind, params.into()) {
            (pubsub::Kind::NewHeads, None) => {
                self.heads_subscribers.write().push(subscriber);
                return;
            }
            (pubsub::Kind::NewHeads, _) => {
                errors::invalid_params("newHeads", "Expected no parameters.")
            }
            (pubsub::Kind::Logs, Some(pubsub::Params::Logs(filter))) => {
                self.logs_subscribers
                    .write()
                    .push(subscriber, filter.into());
                return;
            }
            (pubsub::Kind::Logs, _) => errors::invalid_params("logs", "Expected a filter object."),
            (pubsub::Kind::NewPendingTransactions, None) => {
                // this is a no-op: we're not mining, so we have no pending transactions
                return;
            }
            (pubsub::Kind::NewPendingTransactions, _) => {
                errors::invalid_params("newPendingTransactions", "Expected no parameters.")
            }
            _ => errors::unimplemented(None),
        };

        let _ = subscriber.reject(error);
    }

    fn unsubscribe(&self, id: SubscriptionId) -> Result<bool> {
        measure_counter_inc!("unsubscribe");
        info!("unsubscribe(id: {:?})", id);
        let res = self.heads_subscribers.write().remove(&id).is_some();
        let res2 = self.logs_subscribers.write().remove(&id).is_some();

        Ok(res || res2)
    }
}
