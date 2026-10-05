//! `eg keygen` — generate a key file for encrypted local store mode (issue #54).

use std::path::Path;

use anyhow::Result;

/// Generate a key file for `eg ingest --encrypted --key-file`.
///
/// Without `passphrase_env`: a raw 32-byte key file (0600, refuses to
/// overwrite). With `passphrase_env`: a passphrase-wrapped AEKF file
/// (Argon2id); the passphrase is read from the named env var and zeroized
/// after use. Prints the non-secret descriptor only — never key material.
pub(crate) fn keygen(out: &Path, passphrase_env: Option<&str>) -> Result<()> {
    let descriptor = crate::encrypted_store::generate_key_file(out, passphrase_env)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    println!("wrote {} to {}", descriptor.kind_str(), out.display());
    println!("key source: {}", descriptor.describe());
    println!(
        "Use it with: eg ingest <graph.jsonl> --adapter embedded --data-dir <dir> \
         --encrypted --key-file {}",
        out.display()
    );
    Ok(())
}
