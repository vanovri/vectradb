use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use vectra_types::{Result, VectraError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TxnId(pub u64);

pub type Timestamp = u64;

/// Bootstrap transaction ID representing pre-committed / loaded rows.
pub const BOOTSTRAP_TXN: TxnId = TxnId(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxnState {
    Active { start_ts: Timestamp },
    Committed { commit_ts: Timestamp },
    Aborted,
}

/// Snapshot view held by a query or transaction under strict Snapshot Isolation (SI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub txn_id: TxnId,
    pub start_ts: Timestamp,
}

/// Manages monotonic transaction IDs, commit timestamps, and MVCC visibility state.
#[derive(Debug)]
pub struct TransactionManager {
    next_txn_id: AtomicU64,
    next_ts: AtomicU64,
    states: RwLock<HashMap<TxnId, TxnState>>,
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionManager {
    pub fn new() -> Self {
        let mut states = HashMap::new();
        states.insert(BOOTSTRAP_TXN, TxnState::Committed { commit_ts: 0 });
        Self {
            next_txn_id: AtomicU64::new(1),
            next_ts: AtomicU64::new(1),
            states: RwLock::new(states),
        }
    }

    pub fn begin(&self) -> Snapshot {
        let txn_id = TxnId(self.next_txn_id.fetch_add(1, Ordering::AcqRel));
        // Read current latest committed timestamp (next_ts - 1).
        let start_ts = self.next_ts.load(Ordering::Acquire).saturating_sub(1);
        self.states
            .write()
            .insert(txn_id, TxnState::Active { start_ts });
        Snapshot { txn_id, start_ts }
    }

    pub fn commit(&self, txn_id: TxnId) -> Result<Timestamp> {
        let mut states = self.states.write();
        match states.get(&txn_id).copied() {
            Some(TxnState::Active { .. }) => {
                let commit_ts = self.next_ts.fetch_add(1, Ordering::AcqRel);
                states.insert(txn_id, TxnState::Committed { commit_ts });
                Ok(commit_ts)
            }
            Some(TxnState::Committed { commit_ts }) => Ok(commit_ts),
            Some(TxnState::Aborted) => Err(VectraError::TransactionConflict(format!(
                "cannot commit aborted transaction {:?}",
                txn_id
            ))),
            None => Err(VectraError::Internal(format!(
                "unknown transaction {:?}",
                txn_id
            ))),
        }
    }

    pub fn rollback(&self, txn_id: TxnId) -> Result<()> {
        let mut states = self.states.write();
        match states.get(&txn_id).copied() {
            Some(TxnState::Active { .. }) | Some(TxnState::Aborted) => {
                states.insert(txn_id, TxnState::Aborted);
                Ok(())
            }
            Some(TxnState::Committed { .. }) => Err(VectraError::TransactionConflict(format!(
                "cannot rollback already committed transaction {:?}",
                txn_id
            ))),
            None => Err(VectraError::Internal(format!(
                "unknown transaction {:?}",
                txn_id
            ))),
        }
    }

    #[inline]
    pub fn state(&self, txn_id: TxnId) -> TxnState {
        if txn_id == BOOTSTRAP_TXN {
            return TxnState::Committed { commit_ts: 0 };
        }
        self.states
            .read()
            .get(&txn_id)
            .copied()
            .unwrap_or(TxnState::Aborted)
    }

    /// Returns true if a version created by `creator_txn` is visible to `snapshot`.
    #[inline]
    pub fn is_visible(&self, creator_txn: TxnId, snapshot: Snapshot) -> bool {
        if creator_txn == snapshot.txn_id {
            return true;
        }
        match self.state(creator_txn) {
            TxnState::Committed { commit_ts } => commit_ts <= snapshot.start_ts,
            TxnState::Active { .. } | TxnState::Aborted => false,
        }
    }

    /// Checks whether deleting a row that already has a recorded `existing_deleter`
    /// causes a Write-Write conflict under Snapshot Isolation.
    pub fn check_write_conflict(&self, existing_deleter: TxnId, writer: Snapshot) -> Result<bool> {
        if existing_deleter == writer.txn_id {
            // Already deleted by ourselves.
            return Ok(false);
        }
        match self.state(existing_deleter) {
            TxnState::Aborted => Ok(true), // Previous deleter aborted -> we can claim deletion.
            TxnState::Active { .. } => Err(VectraError::TransactionConflict(format!(
                "write-write conflict: row locked by active txn {:?}",
                existing_deleter
            ))),
            TxnState::Committed { commit_ts } => {
                if commit_ts > writer.start_ts {
                    Err(VectraError::TransactionConflict(format!(
                        "write-write conflict: row modified at ts {commit_ts} > snapshot {}",
                        writer.start_ts
                    )))
                } else {
                    // Row was already deleted before our snapshot began.
                    Ok(false)
                }
            }
        }
    }
}
