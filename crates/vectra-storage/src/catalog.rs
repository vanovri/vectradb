use crate::mvcc::{Snapshot, TransactionManager, TxnId};
use crate::table::{Table, TableStorageSummary};
use crate::vdb_format::{
    deserialize_vdb_bytes, serialize_vdb_bytes, PersistedCatalog, PersistedTable,
};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::Arc;
use vectra_types::{Result, Schema, VectraError};

/// Thread-safe MVCC database catalog managing tables, transactions, and `.vdb` snapshots.
#[derive(Debug)]
pub struct Catalog {
    tables: RwLock<BTreeMap<String, Arc<Table>>>,
    txns: Arc<TransactionManager>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog {
    pub fn new() -> Self {
        Self {
            tables: RwLock::new(BTreeMap::new()),
            txns: Arc::new(TransactionManager::new()),
        }
    }

    pub fn transaction_manager(&self) -> &Arc<TransactionManager> {
        &self.txns
    }

    pub fn begin_txn(&self) -> Snapshot {
        self.txns.begin()
    }

    pub fn commit_txn(&self, txn_id: TxnId) -> Result<u64> {
        self.txns.commit(txn_id)
    }

    pub fn rollback_txn(&self, txn_id: TxnId) -> Result<()> {
        self.txns.rollback(txn_id)
    }

    pub fn create_table(
        &self,
        name: &str,
        schema: Schema,
        if_not_exists: bool,
    ) -> Result<Arc<Table>> {
        let key = name.to_ascii_lowercase();
        let mut guard = self.tables.write();
        if let Some(existing) = guard.get(&key) {
            if if_not_exists {
                return Ok(Arc::clone(existing));
            }
            return Err(VectraError::BindError(format!(
                "table '{name}' already exists"
            )));
        }
        let table = Arc::new(Table::new(key.clone(), schema));
        guard.insert(key, Arc::clone(&table));
        Ok(table)
    }

    pub fn drop_table(&self, name: &str, if_exists: bool) -> Result<()> {
        let key = name.to_ascii_lowercase();
        let mut guard = self.tables.write();
        if guard.remove(&key).is_none() && !if_exists {
            return Err(VectraError::BindError(format!(
                "table '{name}' does not exist"
            )));
        }
        Ok(())
    }

    pub fn get_table(&self, name: &str) -> Result<Arc<Table>> {
        let key = name.to_ascii_lowercase();
        self.tables
            .read()
            .get(&key)
            .cloned()
            .ok_or_else(|| VectraError::BindError(format!("table '{name}' does not exist")))
    }

    pub fn list_tables(&self) -> Vec<String> {
        self.tables.read().keys().cloned().collect()
    }

    pub fn inspect_table(&self, name: &str, snap: Snapshot) -> Result<TableStorageSummary> {
        let tbl = self.get_table(name)?;
        Ok(tbl.inspect_storage(&self.txns, snap))
    }

    pub fn export_persisted(&self, snap: Snapshot) -> PersistedCatalog {
        let guard = self.tables.read();
        let tables = guard
            .values()
            .map(|t| PersistedTable {
                name: t.name.clone(),
                schema: t.schema.clone(),
                row_groups: t.to_persisted(&self.txns, snap),
            })
            .collect();
        PersistedCatalog { tables }
    }

    pub fn from_persisted(persisted: PersistedCatalog) -> Self {
        let catalog = Self::new();
        let mut guard = catalog.tables.write();
        for pt in persisted.tables {
            let key = pt.name.to_ascii_lowercase();
            let table = Arc::new(Table::from_persisted(key.clone(), pt.schema, pt.row_groups));
            guard.insert(key, table);
        }
        drop(guard);
        catalog
    }

    pub fn save_to_bytes(&self, snap: Snapshot) -> Result<Vec<u8>> {
        serialize_vdb_bytes(&self.export_persisted(snap), 1)
    }

    pub fn load_from_bytes(bytes: &[u8]) -> Result<Self> {
        let (persisted, _) = deserialize_vdb_bytes(bytes)?;
        Ok(Self::from_persisted(persisted))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_file(&self, path: &std::path::Path, snap: Snapshot) -> Result<u64> {
        crate::vdb_format::write_vdb_file(path, &self.export_persisted(snap))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_from_file(path: &std::path::Path) -> Result<Self> {
        let (persisted, _) = crate::vdb_format::read_vdb_file(path)?;
        Ok(Self::from_persisted(persisted))
    }
}
