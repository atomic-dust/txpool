use anyhow::{anyhow, bail};
use async_trait::async_trait;
use auto_impl::auto_impl;
use ethereum::Transaction;
use ethereum_types::{Address, H256, U256};
use rlp::{Encodable, RlpStream};
use secp256k1::{
    recovery::{RecoverableSignature, RecoveryId},
    Message, SECP256K1,
};
use sha3::{Digest, Keccak256};
use std::{
    collections::{
        hash_map::Entry::{Occupied, Vacant},
        BTreeMap, HashMap, VecDeque,
    },
    convert::TryFrom,
    sync::Arc,
};
use thiserror::Error;
use tracing::*;

struct RichTransaction {
    inner: Transaction,
    sender: Address,
    hash: H256,
}

impl RichTransaction {
    fn cost(&self) -> U256 {
        self.inner.gas_limit * self.inner.gas_price
    }
}

impl TryFrom<Transaction> for RichTransaction {
    type Error = anyhow::Error;

    fn try_from(tx: Transaction) -> Result<Self, Self::Error> {
        let h = {
            let mut stream = RlpStream::new();
            tx.rlp_append(&mut stream);
            Keccak256::digest(&stream.drain())
        };
        let hash = H256::from_slice(h.as_slice());

        let mut sig = [0_u8; 64];
        sig[..32].copy_from_slice(tx.signature.r().as_bytes());
        sig[32..].copy_from_slice(tx.signature.s().as_bytes());
        let rec = RecoveryId::from_i32(tx.signature.standard_v() as i32).unwrap();

        let public = &SECP256K1
            .recover(
                &Message::from_slice(
                    ethereum::TransactionMessage::from(tx.clone())
                        .hash()
                        .as_bytes(),
                )?,
                &RecoverableSignature::from_compact(&sig, rec)?,
            )?
            .serialize_uncompressed()[1..];

        let sender = Address::from_slice(&Keccak256::digest(&public)[12..]);
        Ok(Self {
            sender,
            hash,
            inner: tx,
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AccountInfo {
    pub balance: U256,
    pub nonce: u64,
}

#[async_trait]
#[auto_impl(&, Box, Arc)]
pub trait AccountInfoProvider: Send + Sync + 'static {
    async fn get_account_info(
        &self,
        block: u64,
        account: Address,
    ) -> anyhow::Result<Option<AccountInfo>>;
}

#[async_trait]
impl AccountInfoProvider for HashMap<u64, HashMap<Address, AccountInfo>> {
    async fn get_account_info(
        &self,
        block: u64,
        account: Address,
    ) -> anyhow::Result<Option<AccountInfo>> {
        if let Some(accounts) = self.get(&block) {
            if let Some(info) = accounts.get(&account) {
                return Ok(Some(*info));
            }
        }

        Ok(None)
    }
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("invalid transaction: {0}")]
    InvalidTransaction(anyhow::Error),
    #[error("nonce gap")]
    NonceGap,
    #[error("stale transaction")]
    StaleTransaction,
    #[error("invalid sender: {0}")]
    InvalidSender(anyhow::Error),
    #[error("fee too low")]
    FeeTooLow,
    #[error("not enough balance to pay for gas")]
    InsufficientBalance,
    #[error("other: {0}")]
    Other(anyhow::Error),
}

struct AccountPool {
    nonce_offset: u64,
    balance: U256,
    txs: VecDeque<Arc<RichTransaction>>,
}

pub struct Pool<DP> {
    block: u64,
    data_provider: DP,
    by_hash: HashMap<H256, Arc<RichTransaction>>,
    by_sender: HashMap<Address, AccountPool>,
}

impl<DP> Pool<DP> {
    pub fn new(block: u64, data_provider: DP) -> Self {
        Self {
            block,
            data_provider,
            by_hash: Default::default(),
            by_sender: Default::default(),
        }
    }
}

impl<DP: AccountInfoProvider> Pool<DP> {
    pub fn get(&self, hash: H256) -> Option<&Transaction> {
        self.by_hash.get(&hash).map(|tx| &tx.inner)
    }

    pub async fn import(&mut self, tx: Transaction) -> Result<bool, ImportError> {
        let mut tx =
            Arc::new(RichTransaction::try_from(tx).map_err(ImportError::InvalidTransaction)?);

        if tx.inner.nonce > U256::from(u64::MAX) {
            return Err(ImportError::InvalidTransaction(anyhow!("nonce too large")));
        }

        match self.by_hash.entry(tx.hash) {
            Occupied(_) => {
                // Tx already there.
                Ok(false)
            }
            Vacant(tx_by_hash_entry) => {
                // This is a new transaction.
                let account_pool = match self.by_sender.entry(tx.sender) {
                    Occupied(occupied) => occupied.into_mut(),
                    Vacant(entry) => {
                        // This is a new sender, let's get its state.
                        let info = self
                            .data_provider
                            .get_account_info(self.block, tx.sender)
                            .await
                            .map_err(ImportError::InvalidSender)?
                            .ok_or_else(|| {
                                ImportError::InvalidSender(anyhow!("sender account does not exist"))
                            })?;

                        entry.insert(AccountPool {
                            nonce_offset: info.nonce,
                            balance: info.balance,
                            txs: Default::default(),
                        })
                    }
                };

                if let Some(offset) = tx
                    .inner
                    .nonce
                    .as_u64()
                    .checked_sub(account_pool.nonce_offset)
                {
                    // This transaction's nonce is account nonce or greater.
                    if offset <= account_pool.txs.len() as u64 {
                        // This transaction is between existing txs in the pool, or right the next one.

                        // Compute balance after executing all txs before it.
                        let mut cumulative_balance = account_pool
                            .txs
                            .iter()
                            .take(offset as usize)
                            .fold(account_pool.balance, |balance, tx| balance - tx.cost());

                        // If this is a replacement transaction, pick between this and old.
                        if let Some(pooled_tx) = account_pool.txs.get_mut(offset as usize) {
                            if pooled_tx.inner.gas_price >= tx.inner.gas_price {
                                return Err(ImportError::FeeTooLow);
                            }

                            if cumulative_balance.checked_sub(tx.cost()).is_none() {
                                return Err(ImportError::InsufficientBalance);
                            }

                            std::mem::swap(&mut tx, pooled_tx);
                        }

                        let mut dropping = VecDeque::new();

                        // Compute the balance after executing remaining transactions. Select for removal those for which we do not have enough balance.
                        for (i, tx) in account_pool.txs.iter().enumerate().skip(offset as usize) {
                            if let Some(balance) = cumulative_balance.checked_sub(tx.cost()) {
                                cumulative_balance = balance;
                            } else {
                                dropping = account_pool.txs.split_off(i);
                                break;
                            }
                        }

                        tx_by_hash_entry.insert(tx);

                        for item in dropping {
                            self.by_hash.remove(&item.hash);
                        }

                        Ok(true)
                    } else {
                        Err(ImportError::NonceGap)
                    }
                } else {
                    // Nonce lower than account, meaning it's stale.
                    Err(ImportError::StaleTransaction)
                }
            }
        }
    }

    pub fn erase(&mut self) {
        self.by_hash.clear();
        self.by_sender.clear();
    }

    fn apply_block_inner(&mut self, block: u64, txs: Vec<Transaction>) -> anyhow::Result<()> {
        if self.block + 1 != block {
            bail!(
                "block gap detected: applying {}, expected {}",
                block,
                self.block + 1
            );
        }

        let mut block_txs_by_sender =
            HashMap::<Address, Option<BTreeMap<u64, RichTransaction>>>::new();

        for tx in txs {
            let tx = RichTransaction::try_from(tx)?;
            if tx.inner.nonce > U256::from(u64::MAX) {
                block_txs_by_sender.insert(tx.sender, None);
            }

            if let Some(m) = block_txs_by_sender
                .entry(tx.sender)
                .or_insert_with(|| Some(Default::default()))
            {
                m.insert(tx.inner.nonce.as_u64(), tx);
            }
        }

        // Now we either cull all confirmed transactions, or drop sender in case of error.
        for (sender, txs) in block_txs_by_sender {
            if let Occupied(mut entry) = self.by_sender.entry(sender) {
                let mut validation_error = false;
                if let Some(txs) = txs {
                    let mut pool = entry.get_mut();

                    for (nonce, tx) in txs {
                        if nonce != pool.nonce_offset {
                            validation_error = true;
                            break;
                        }

                        // Validate that the next tx in pool has the same ID as in block.
                        if let Some(front_tx) = pool.txs.pop_front() {
                            assert!(self.by_hash.remove(&tx.hash).is_some());
                            if front_tx.hash != tx.hash {
                                validation_error = true;
                                break;
                            }

                            pool.nonce_offset += 1;
                        } else {
                            validation_error = true;
                            break;
                        }
                    }
                } else {
                    validation_error = true;
                }

                if validation_error {
                    // We will drop all transactions from this sender now
                    for tx in entry.remove().txs {
                        assert!(self.by_hash.remove(&tx.hash).is_some());
                    }
                }
            }
        }

        Ok(())
    }

    pub fn apply_block(&mut self, block: u64, txs: Vec<Transaction>) {
        if let Err(e) = self.apply_block_inner(block, txs) {
            warn!(
                "Failed to apply block {}: {}. Resetting transaction pool.",
                block, e
            );

            self.erase();
        }
        self.block = block;
    }

    fn revert_block_inner(&mut self, block: u64, txs: Vec<Transaction>) -> anyhow::Result<()> {
        if self.block - 1 != block {
            bail!(
                "block gap detected: reverting {}, expected {}",
                block,
                self.block + 1
            );
        }

        // Nothing fancy for now - just drop all senders.
        for tx in txs {
            let tx = RichTransaction::try_from(tx)?;
            if let Some(pool) = self.by_sender.remove(&tx.sender) {
                for tx in pool.txs {
                    assert!(self.by_hash.remove(&tx.hash).is_some());
                }
            }
        }

        Ok(())
    }

    pub fn revert_block(&mut self, block: u64, txs: Vec<Transaction>) {
        if let Err(e) = self.revert_block_inner(block, txs) {
            warn!(
                "Failed to revert block {}: {}. Resetting transaction pool.",
                block, e
            );

            self.erase();
        }
        self.block = block;
    }
}
