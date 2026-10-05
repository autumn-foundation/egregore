# Encrypted local store mode (issue #54)

Egregore stores are plaintext by default. Encrypted-at-rest mode is an explicit
operator opt-in at store creation, built on AletheiaDB's native encryption
subsystem (AES-256-GCM with AES-NI / ChaCha20-Poly1305, per-component data keys
derived via HKDF-SHA256). Egregore does not implement any crypto itself — it
only decides when encryption is on and where the key comes from.

## Shortest workflow

```bash
# 1. Generate a key file (0600, refuses to overwrite).
eg keygen --out /secure/eg-store.key

# 2. Create the encrypted store and ingest in one step.
eg ingest graph.jsonl --adapter embedded --data-dir .egregore-enc \
  --encrypted --key-file /secure/eg-store.key
```

That's it. Every later open (`ingest`, `inspect`, `query`, `export`, the
daemon) resolves the mode from the store itself — no per-command flags.

## Passphrase-wrapped keys

For a key file protected by a passphrase (AEKF, Argon2id):

```bash
export EG_STORE_PASSPHRASE='correct horse battery staple'
eg keygen --out /secure/eg-store.aekf --passphrase-env EG_STORE_PASSPHRASE
eg ingest graph.jsonl --adapter embedded --data-dir .egregore-enc \
  --encrypted --key-file /secure/eg-store.aekf --passphrase-env EG_STORE_PASSPHRASE
```

The passphrase is read from the environment, never from the command line, and
Egregore zeroizes its copy immediately after use.

## Rules

- **Creation-time opt-in only.** `--encrypted` requires a fresh `--data-dir`
  and `--key-file`. Pointing `--encrypted` at an existing plaintext store is
  refused (`storage_mode_mismatch`) — there is no live migration. Ingest into
  a fresh data dir instead.
- **The store remembers.** After creation, `{data_dir}/egregore-store.json`
  pins the non-secret key-source descriptor (paths and env-var names only —
  never key material), cross-checked against AletheiaDB's durable
  `encryption.state` authority on every open. Any disagreement fails closed.
- **Fail closed.** A missing/unreadable key file or unset passphrase env var
  refuses the open before any I/O (`encrypted_store_key_unavailable`). A wrong
  key fails the open (`encrypted_store_key_error`); nothing is modified.
- **No per-command flags afterwards.** `eg inspect`, `eg query`, `eg export`,
  and the daemon resolve the key from the pinned descriptor automatically.
- **Redaction is orthogonal.** The redaction gate runs on records above the
  storage layer and is unchanged: a store being encrypted never excuses an
  unredacted secret, and encryption never permits storing raw secrets.
- **Key files are not backed up by Egregore.** If the key file is lost, the
  store cannot be opened. Keep the key file (or the passphrase) somewhere the
  backup procedure documents — see "Backup and restore" below.

## Daemon

The daemon resolves the key from the pinned marker at startup; there are no
new daemon flags. `egregored.json` and `GET /v1/status` report the non-secret
`storage_mode` (`plaintext`/`encrypted`) and `key_source` (`file` /
`passphrase_file`) fields.

## Backup and restore (AC7)

The encrypted store is an ordinary directory: back up the whole data dir.
Restore is the reverse copy — the marker and the engine authority travel with
the data. Two non-obvious points:

1. **The key file is outside the data dir and must be backed up separately.**
   A restored data dir without its key file opens to
   `encrypted_store_key_unavailable`. Document where the key lives.
2. **Restore the marker and `encryption.state` together.** Restoring the data
   dir without one of them trips `storage_mode_mismatch` (fail-closed), which
   is the safety net working as designed — it means the restore is incomplete.

```bash
# Backup: copy the data dir and the key file (separately, securely).
cp -a .egregore-enc /backups/egregore-enc-2026-09-30
cp -a /secure/eg-store.key /backups/keys/

# Restore: copy both back, then open normally.
cp -a /backups/egregore-enc-2026-09-30 .egregore-enc
# (key file back in place)
eg inspect .egregore-enc --summary   # opens encrypted, no extra flags
```

## Machine codes

| Code | Meaning |
|---|---|
| `storage_mode_mismatch` | Requested mode disagrees with durable state (e.g. `--encrypted` on an existing plaintext store; marker/authority disagreement). |
| `encrypted_store_key_unavailable` | Key file missing/unreadable or passphrase env var unset. Checked before any I/O. |
| `encrypted_store_key_error` | Key material loaded but the store failed to open under it (wrong key). Nothing modified. |

All three are emitted as `{"ok": false, "error": {"code": ..., "message": ...}}`
envelopes on the CLI. The messages name the diagnosis and remedy; they never
contain key material.
