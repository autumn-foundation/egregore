//! Generates the checked-in JSON Schema files for the frozen MCP tool I/O
//! contract (issue #194).
//!
//! Writes `docs/schema/mcp/<tool>.schema.json` for every shipped tool plus
//! `docs/schema/mcp/error.schema.json` for the shared error envelope. The
//! `mcp_contract` Rust module is the single source of truth; these files are
//! its published twin for agent clients that cannot call into Rust.
//!
//! Regenerate with `cargo run --example gen_mcp_schemas` whenever the
//! contract module changes. The
//! `published_schema_files_match_registered_schemas` conformance test fails
//! if a checked-in file drifts from its registered schema.

use std::path::Path;

use aletheia_egregore::mcp_contract::{MCP_CONTRACT_TOOLS, error_schema, response_schema};

fn write_pretty(path: &Path, schema: &serde_json::Value) {
    let mut out = serde_json::to_string_pretty(schema).expect("schema must serialize");
    out.push('\n');
    std::fs::write(path, out).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/schema/mcp");
    std::fs::create_dir_all(&dir).expect("create docs/schema/mcp");
    for tool in MCP_CONTRACT_TOOLS {
        let schema = response_schema(tool).expect("shipped tool must have a schema");
        write_pretty(&dir.join(format!("{tool}.schema.json")), &schema);
    }
    write_pretty(&dir.join("error.schema.json"), &error_schema());
    println!(
        "wrote {} schema files to {}",
        MCP_CONTRACT_TOOLS.len() + 1,
        dir.display()
    );
}
