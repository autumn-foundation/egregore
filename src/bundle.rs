use crate::error::{CodegraphError, Result};
use crate::ir::GraphRecord;
use serde::{Deserialize, Serialize};

/// Evidence bundle representing a portable, redaction-safe package of graph records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceBundle {
    // fields will go here
}

/// Exports an evidence bundle for a selected query result, task, memory record, etc.
pub fn export_bundle(
    _records: &[GraphRecord],
    _root_selector: &str,
    _egregore_version: &str,
) -> Result<EvidenceBundle> {
    Err(CodegraphError::InvalidArgument {
        message: "Not implemented".to_owned(),
    })
}
