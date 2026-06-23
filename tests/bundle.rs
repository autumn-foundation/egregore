#![allow(missing_docs)]

use aletheia_egregore::{
    bundle::{export_bundle, EvidenceBundle},
    GraphRecord,
};

#[test]
fn test_basic_bundle_module_exists() {
    let records: Vec<GraphRecord> = vec![];
    let result = export_bundle(&records, "id:test", "0.1.0");
    assert!(result.is_err());
}
