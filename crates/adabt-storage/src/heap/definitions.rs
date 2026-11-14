//! Logged declarations that outlive a restart without being data: index
//! definitions and each collection's clustering field.

use adabt_core::error::Result;

use super::migration::is_migration_name;
use super::HeapStore;
use crate::wal::WalOp;

impl HeapStore {
    /// Index definitions recorded in the log, for rebuilding after a restart.
    ///
    /// Definitions only. The contents are derived and reconstructed by a scan,
    /// which is the rebuildability invariant doing its job: losing an index
    /// costs a scan, never a record.
    pub fn index_definitions(&self) -> &[(String, String, String)] {
        &self.index_defs
    }

    pub fn record_index(&mut self, collection: &str, field: &str, kind: &str) -> Result<()> {
        let def = (collection.to_string(), field.to_string(), kind.to_string());
        if store_has(&self.index_defs, &def) {
            return Ok(());
        }
        self.log(WalOp::CreateIndex {
            collection: collection.to_string(),
            field: field.to_string(),
            kind: kind.to_string(),
        })?;
        self.index_defs.push(def);
        Ok(())
    }

    pub fn forget_index(&mut self, collection: &str, field: &str, kind: &str) -> Result<()> {
        let before = self.index_defs.len();
        self.index_defs
            .retain(|(c, f, k)| !(c == collection && f == field && k == kind));
        if self.index_defs.len() != before {
            self.log(WalOp::DropIndex {
                collection: collection.to_string(),
                field: field.to_string(),
                kind: kind.to_string(),
            })?;
        }
        Ok(())
    }

    /// Declare (or clear) the collection's clustering field. Logged, so the
    /// declaration survives restarts; the page ranges themselves re-derive.
    /// Fails only if the collection does not exist.
    pub fn set_cluster_field(&mut self, collection: &str, field: Option<&str>) -> Result<()> {
        self.coll(collection)?;
        self.log(WalOp::SetClusterField {
            collection: collection.to_string(),
            field: field.map(String::from),
        })?;
        let c = self.collections.get_mut(collection).expect("checked above");
        c.cluster_name = field.map(String::from);
        Ok(())
    }

    /// The declared clustering fields across all user collections — what an
    /// engine reads once at open to restore its routing map.
    pub fn declared_cluster_fields(&self) -> Vec<(String, String)> {
        self.collections
            .iter()
            .filter(|(name, _)| !is_migration_name(name))
            .filter_map(|(name, c)| c.cluster_name.as_ref().map(|f| (name.clone(), f.clone())))
            .collect()
    }
}

/// Whether a definition is already recorded.
pub(super) fn store_has(defs: &[(String, String, String)], def: &(String, String, String)) -> bool {
    defs.iter().any(|d| d == def)
}
