use super::*;

pub(crate) fn import_local_tasks_cmd(
    tasks_path: &Path,
    out: &Path,
    repo_root: Option<&Path>,
    transaction_time: Option<&str>,
) -> Result<()> {
    let repo_root = match repo_root {
        Some(r) => r.to_path_buf(),
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let opts = local_project::ImportOptions {
        transaction_time: transaction_time.map(str::to_owned),
        ..local_project::ImportOptions::default()
    };
    let result = local_project::import_local_tasks(tasks_path, &repo_root, &opts)
        .with_context(|| format!("failed to import local tasks from {}", tasks_path.display()))?;
    let jsonl = result
        .graph
        .to_jsonl()
        .context("failed to serialize project-graph JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records ({} diagnostics) from {}",
        result.graph.records().len(),
        result.diagnostic_count,
        tasks_path.display()
    );
    Ok(())
}

/// Handles `eg import github <owner>/<repo>`.
///
/// On failure, prints a scrubbed machine-readable `{"code":"github_..."}` line
/// to stderr and exits non-zero. The handoff JSONL and state file are written
/// only on success, so an auth/rate-limit failure leaves no partial state
/// (`docs/schema/import-github.md` §5).
#[allow(clippy::too_many_arguments)]
pub(crate) fn import_github_cmd(
    repo: &str,
    out: &Path,
    state_file: Option<&Path>,
    code_graph: Option<&Path>,
    token_file: Option<&Path>,
    api_base: Option<&str>,
    transaction_time: Option<&str>,
    no_backoff: bool,
) -> Result<()> {
    use crate::github::{
        client::scrub_line,
        error::GithubError,
        import::{ImportOptions, run_import},
        state::State,
    };

    // Validate the repo argument shape before any network work.
    if repo.split('/').filter(|s| !s.is_empty()).count() != 2 || repo.matches('/').count() != 1 {
        let e = GithubError::InvalidRepoArg {
            arg: repo.to_owned(),
        };
        eprintln!("{}", scrub_line(&format!(r#"{{"code":"{}"}}"#, e.code())));
        eprintln!("{}", scrub_line(&e.to_string()));
        process::exit(1);
    }

    let api_base = api_base.map_or_else(
        || crate::github::client::DEFAULT_API_BASE.to_owned(),
        str::to_owned,
    );

    // Default state-file path: alongside the handoff output.
    let default_state = out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".github-import-state.json");
    let state_path = state_file.map_or(default_state, Path::to_path_buf);

    let prior_state = State::load_or_fresh(&state_path, repo, &api_base);

    let opts = ImportOptions {
        source_repo: repo,
        api_base,
        token_file,
        code_graph,
        transaction_time: transaction_time.map(str::to_owned),
        no_backoff,
    };

    match run_import(&opts, prior_state) {
        Ok(outcome) => {
            fs::write(out, &outcome.jsonl)
                .with_context(|| format!("failed to write handoff JSONL to {}", out.display()))?;
            outcome
                .state
                .save(&state_path)
                .with_context(|| format!("failed to write state file {}", state_path.display()))?;
            let q = outcome
                .summary
                .quota_remaining
                .map_or_else(|| "unknown".to_owned(), |v| v.to_string());
            // Per-run summary (scrubbed) to stderr per §4.
            eprintln!(
                "{}",
                scrub_line(&format!(
                    "egregore-github-import: requests={} quota_remaining={} elapsed={:.2}s",
                    outcome.summary.requests, q, outcome.summary.elapsed_secs
                ))
            );
            println!(
                "imported {} records from {} into {}",
                outcome.summary.emitted_records,
                repo,
                out.display()
            );
            Ok(())
        }
        Err(e) => {
            // Machine-readable, token-scrubbed diagnostic; no partial state write.
            eprintln!("{}", scrub_line(&format!(r#"{{"code":"{}"}}"#, e.code())));
            eprintln!("{}", scrub_line(&e.to_string()));
            process::exit(1);
        }
    }
}

pub(crate) fn import_codex_cmd(
    codex_path: &Path,
    out: &Path,
    redaction_report: Option<&Path>,
) -> Result<()> {
    ensure_report_path_distinct(out, redaction_report)?;
    let opts = crate::codex::ImportOptions::default();
    let graph = crate::codex::import_codex(codex_path, &opts)
        .with_context(|| format!("failed to import Codex JSONL from {}", codex_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    // Now that --out exists, aliases invisible to the pre-write guard (e.g.
    // case-insensitive name folding) are observable; recheck before the
    // report write. Refusal leaves the records JSONL intact on disk.
    ensure_report_still_distinct_after_write(out, redaction_report)?;
    let status = format!(
        "imported {} records from {}",
        graph.records().len(),
        codex_path.display()
    );
    emit_import_status_and_report(
        &status,
        graph.records(),
        opts.policy_version,
        redaction_report,
    )
}

pub(crate) fn import_claude_code_cmd(transcript_path: &Path, out: &Path) -> Result<()> {
    let opts = crate::claude_code::ImportOptions::default();
    let graph =
        crate::claude_code::import_claude_code(transcript_path, &opts).with_context(|| {
            format!(
                "failed to import Claude Code transcript from {}",
                transcript_path.display()
            )
        })?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        transcript_path.display()
    );
    Ok(())
}

pub(crate) fn import_antigravity_cmd(antigravity_path: &Path, out: &Path) -> Result<()> {
    let opts = crate::antigravity::ImportOptions::default();
    let graph =
        crate::antigravity::import_antigravity(antigravity_path, &opts).with_context(|| {
            format!(
                "failed to import Antigravity transcript from {}",
                antigravity_path.display()
            )
        })?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        antigravity_path.display()
    );
    Ok(())
}

/// Rejects a `--redaction-report` path that would overwrite the `--out` JSONL.
///
/// The report is written after the records, so a matching path would silently
/// replace the graph JSONL with the report while the command still exits 0.
/// Paths are compared after resolving their components in filesystem order —
/// symlinks followed as encountered, `.`/`..` applied where the OS applies
/// them, never collapsed lexically up front — so aliases such as
/// `tmp/../records.jsonl` vs `records.jsonl`, `..` after a symlinked
/// directory (`link/../records.jsonl` with `link -> target/child`), symlinked
/// parent directories, and a pre-existing dangling symlink pointing at the
/// other output all conflict even though the output files themselves do not
/// exist yet; `-` (stdout) never conflicts. A path whose symlink chain cannot be
/// resolved within [`SYMLINK_RESOLUTION_LIMIT`] hops (a cycle or an absurdly
/// deep chain) is treated as conflicting — the guard refuses rather than
/// guessing the paths are distinct. When both resolved targets already exist,
/// on-disk file identity (device + inode on Unix, the file-index equivalent
/// on Windows, via [`same_file`]) is compared as well, so two pre-existing
/// hard links to one inode conflict even though their path strings differ.
///
/// One aliasing class is invisible to this pre-write pass by construction:
/// on a case-insensitive filesystem (Windows NTFS, default APFS) two
/// spellings differing only by case name one file, but while *neither*
/// destination exists the resolved paths compare unequal and no metadata
/// exists to probe for identity. Case folding is not second-guessed
/// lexically here — on a case-sensitive filesystem those spellings are
/// genuinely distinct files and must pass. Instead,
/// [`ensure_report_still_distinct_after_write`] reruns the check after the
/// records write, when the alias (if any) has become observable on the
/// actual filesystem; behavior stays deterministic per filesystem.
///
/// # Errors
///
/// Returns an error naming both flags when the paths resolve to the same file
/// or when a symlink chain on either path cannot be resolved.
pub(crate) fn ensure_report_path_distinct(
    out: &Path,
    redaction_report: Option<&Path>,
) -> Result<()> {
    let Some(report_path) = redaction_report else {
        return Ok(());
    };
    if report_path == Path::new("-") {
        return Ok(());
    }
    if report_and_out_paths_conflict(out, report_path) {
        anyhow::bail!(
            "--redaction-report path {} matches --out; the report would overwrite the \
             records JSONL — choose distinct paths",
            report_path.display()
        );
    }
    Ok(())
}

/// Reruns the `--out`/`--redaction-report` collision check after the records
/// JSONL has been written, immediately before the report write.
///
/// The pre-write [`ensure_report_path_distinct`] pass cannot see aliases that
/// only exist at the filesystem level while neither destination exists —
/// canonically, case-folded spellings (`Records.JSONL` vs `records.jsonl`)
/// on a case-insensitive filesystem such as Windows NTFS or default APFS. At
/// this point `--out` exists, so resolving the report path probes real
/// metadata: if the two names alias one file, the identity comparison
/// ([`existing_files_share_identity`]) now detects it and the report write is
/// refused. This also covers any other OS-level aliasing the pre-write probe
/// cannot observe. On a case-sensitive filesystem the same spellings remain
/// distinct files and pass — deterministic per filesystem, never a lexical
/// case-folding guess.
///
/// Refusal here is late but lossless: the records JSONL is already on disk,
/// untouched and valid; only the report is withheld and the command exits
/// nonzero. `-` (stdout) never conflicts.
///
/// # Errors
///
/// Returns an error naming both flags when the report path resolves to the
/// just-written records file or a symlink chain cannot be resolved.
pub(crate) fn ensure_report_still_distinct_after_write(
    out: &Path,
    redaction_report: Option<&Path>,
) -> Result<()> {
    let Some(report_path) = redaction_report else {
        return Ok(());
    };
    if report_path == Path::new("-") {
        return Ok(());
    }
    if report_and_out_paths_conflict(out, report_path) {
        anyhow::bail!(
            "--redaction-report path {} resolves to the just-written --out records JSONL \
             (a filesystem-level alias, e.g. case-insensitive name folding); the records \
             file was written and remains valid, but the report was not written — choose \
             distinct paths",
            report_path.display()
        );
    }
    Ok(())
}

/// Returns `true` when `out` and `report_path` cannot be shown to name
/// distinct files: their filesystem-order resolutions compare equal, both
/// resolve but the existing files share on-disk identity, or either path
/// fails to resolve (unknowable target — the safe side). Shared by the
/// pre-write guard and the post-records-write recheck.
pub(crate) fn report_and_out_paths_conflict(out: &Path, report_path: &Path) -> bool {
    match (
        resolve_output_path_for_collision(out),
        resolve_output_path_for_collision(report_path),
    ) {
        (Some(resolved_out), Some(resolved_report)) => {
            resolved_out == resolved_report
                || existing_files_share_identity(&resolved_out, &resolved_report)
        }
        // An unresolvable symlink chain means the write target is unknowable;
        // refuse deterministically instead of risking a clobber.
        _ => true,
    }
}

/// Returns `true` when both paths name *existing* files that share on-disk
/// identity — the same device + inode on Unix, the same volume serial +
/// file index on Windows (via [`same_file::is_same_file`]) — catching
/// pre-existing hard-link aliases whose resolved path strings differ.
///
/// Identity is only comparable for files that exist: when either target is
/// missing ([`fs::metadata`] fails), this returns `false` and the caller's
/// resolved-path comparison alone decides. If the identity probe itself fails
/// on two files that were just observed to exist, the write target is
/// unknowable and this refuses deterministically (`true`, the safe side)
/// rather than risking a clobber.
pub(crate) fn existing_files_share_identity(a: &Path, b: &Path) -> bool {
    if fs::metadata(a).is_err() || fs::metadata(b).is_err() {
        return false;
    }
    same_file::is_same_file(a, b).unwrap_or(true)
}

/// Upper bound on symlink hops followed while resolving an output path for
/// the collision check, mirroring the kernel's `ELOOP` limit of 40. Hitting
/// the bound (a symlink cycle, or a chain deeper than any legitimate layout)
/// yields `None`, which [`ensure_report_path_distinct`] treats as a conflict.
pub(crate) const SYMLINK_RESOLUTION_LIMIT: u32 = 40;

/// Resolves an output path for the `--out`/`--redaction-report` collision
/// check without requiring the target file to exist.
///
/// Components are resolved in *filesystem order* — the order the OS applies
/// when the write finally happens — never by collapsing `.`/`..` lexically up
/// front. Starting from the canonicalized cwd (relative paths) or the
/// root/prefix (absolute paths), each raw component is applied left to right:
/// `.` is skipped; `..` pops the last resolved component (safe because the
/// resolved prefix is already fully symlink-free; at the root it stays at the
/// root); a normal component is appended and, when [`fs::symlink_metadata`]
/// reports a symlink, its [`fs::read_link`] target is resolved through this
/// same walk (relative targets against the link's parent). With
/// `link -> target/child`, `link/../records.jsonl` therefore resolves to
/// `target/records.jsonl` — where the OS actually writes — not the lexical
/// `./records.jsonl`. Components that do not exist yet never test as symlinks
/// and are appended as-is, so the guard works before either output exists;
/// dangling symlinks still resolve to their eventual targets.
///
/// Returns `None` when a symlink chain exceeds [`SYMLINK_RESOLUTION_LIMIT`]
/// hops, a discovered link cannot be read, or the cwd cannot be
/// canonicalized; callers must treat `None` as "possibly the same file" (the
/// safe side).
pub(crate) fn resolve_output_path_for_collision(path: &Path) -> Option<PathBuf> {
    let mut resolved = if path.is_absolute() {
        // The walk's prefix/root components establish the base themselves.
        PathBuf::new()
    } else {
        std::env::current_dir().ok()?.canonicalize().ok()?
    };
    let mut hops: u32 = 0;
    resolve_components_in_filesystem_order(&mut resolved, path, &mut hops)?;
    Some(resolved)
}

/// Applies `path`'s raw components onto `resolved` in filesystem order,
/// following symlinks as they are encountered (recursing for link targets,
/// bounded by [`SYMLINK_RESOLUTION_LIMIT`] total hops via `hops`).
///
/// `resolved` must be fully symlink-free on entry — either empty (an absolute
/// `path` supplies its own prefix/root) or a canonicalized directory — so
/// popping a component for `..` is exactly what the OS would do.
///
/// Returns `None` on an unresolvable chain (hop limit or unreadable link);
/// the caller treats that as a possible collision.
pub(crate) fn resolve_components_in_filesystem_order(
    resolved: &mut PathBuf,
    path: &Path,
    hops: &mut u32,
) -> Option<()> {
    use std::path::Component;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                *resolved = PathBuf::from(prefix.as_os_str());
            }
            // Pushing a rooted component drops everything after any prefix,
            // matching the OS restart-at-root behavior for absolute targets.
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // `resolved` is symlink-free, so popping the last component
                // is the filesystem-order meaning of `..`; at the root there
                // is nothing to pop and `..` stays at the root.
                if matches!(
                    resolved.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    resolved.pop();
                }
            }
            Component::Normal(name) => {
                resolved.push(name);
                // Nonexistent components never test as symlinks and stay
                // appended as-is — outputs need not exist yet.
                let is_symlink = fs::symlink_metadata(&resolved)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false);
                if !is_symlink {
                    continue;
                }
                if *hops >= SYMLINK_RESOLUTION_LIMIT {
                    return None;
                }
                *hops += 1;
                let target = fs::read_link(&resolved).ok()?;
                // Resolve the target through this same walk: relative targets
                // continue from the link's parent; absolute targets reset at
                // their root/prefix via the components above.
                resolved.pop();
                resolve_components_in_filesystem_order(resolved, &target, hops)?;
            }
        }
    }
    Some(())
}

/// Emits the import status line and, when requested, the issue #266 redaction
/// report.
///
/// The report is a single deterministic JSON line built from the emitted
/// records' stored markers — record IDs, field paths, class names, hash
/// prefixes, and counts only, never raw payloads. `-` writes the report to
/// stdout (the status line moves to stderr so stdout is exactly the report);
/// any other path writes a file and keeps the status line on stdout.
pub(crate) fn emit_import_status_and_report(
    status: &str,
    records: &[crate::ir::GraphRecord],
    policy_version: Option<&str>,
    redaction_report: Option<&Path>,
) -> Result<()> {
    let Some(report_path) = redaction_report else {
        println!("{status}");
        return Ok(());
    };
    let report = crate::redaction_report::build_redaction_report(records, policy_version);
    let json = serde_json::to_string(&report).context("failed to serialize redaction report")?;
    if report_path == Path::new("-") {
        println!("{json}");
        eprintln!("{status}");
    } else {
        fs::write(report_path, format!("{json}\n")).with_context(|| {
            format!(
                "failed to write redaction report to {}",
                report_path.display()
            )
        })?;
        println!("{status}");
        println!("redaction report written to {}", report_path.display());
    }
    Ok(())
}

pub(crate) fn import_traj_cmd(
    traj_path: &Path,
    out: &Path,
    redaction_report: Option<&Path>,
) -> Result<()> {
    ensure_report_path_distinct(out, redaction_report)?;
    let opts = ImportOptions::default();
    let graph = traj::import_traj(traj_path, &opts)
        .with_context(|| format!("failed to import .traj from {}", traj_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    // Now that --out exists, aliases invisible to the pre-write guard (e.g.
    // case-insensitive name folding) are observable; recheck before the
    // report write. Refusal leaves the records JSONL intact on disk.
    ensure_report_still_distinct_after_write(out, redaction_report)?;
    let status = format!(
        "imported {} records from {}",
        graph.records().len(),
        traj_path.display()
    );
    emit_import_status_and_report(
        &status,
        graph.records(),
        opts.policy_version,
        redaction_report,
    )
}

// -----------------------------------------------------------------------------------------------------------
// Issue #266: post-records-write recheck of the --out/--redaction-report
// collision guard.
//
// On case-insensitive filesystems (Windows NTFS, default APFS) two spellings
// that differ only by case alias one file, but while NEITHER destination
// exists the pre-write guard cannot see that: the resolved paths compare
// unequal and both identity probes miss. The alias becomes observable the
// moment the records write creates `--out` — so the recheck runs then,
// before the report write. A hard link created between the two checks stands
// in for that aliasing here, reproducible on every filesystem (the true
// casing scenario is exercised by the `#[cfg(any(windows, target_os =
// "macos"))]` integration tests in tests/integration/redaction_report.rs).
// -----------------------------------------------------------------------------------------------------------
#[cfg(test)]
mod report_collision_recheck {
    use super::*;

    #[test]
    fn recheck_refuses_alias_observable_only_after_records_write() {
        let temp = tempfile::tempdir().expect("temp dir");
        let out = temp.path().join("records.jsonl");
        let report = temp.path().join("report.json");

        // Pre-write guard passes: neither destination exists yet and the
        // spellings resolve to distinct paths.
        ensure_report_path_distinct(&out, Some(&report))
            .expect("pre-write guard must pass while both destinations are missing");

        // The records write creates --out, and the report spelling turns out
        // to alias it at the OS level (as differing case does on a
        // case-insensitive filesystem).
        fs::write(&out, "records line\n").expect("records write");
        fs::hard_link(&out, &report).expect("alias report path to out");

        let err = ensure_report_still_distinct_after_write(&out, Some(&report))
            .expect_err("recheck must refuse once the alias is observable");
        let msg = err.to_string();
        assert!(
            msg.contains("--redaction-report"),
            "refusal must name the flag: {msg}"
        );
        assert!(
            msg.contains("report was not written"),
            "refusal must state the report was withheld: {msg}"
        );
        assert_eq!(
            fs::read_to_string(&out).expect("records file must survive"),
            "records line\n",
            "the just-written records JSONL must remain untouched"
        );
    }

    #[test]
    fn recheck_passes_for_distinct_report_path() {
        let temp = tempfile::tempdir().expect("temp dir");
        let out = temp.path().join("records.jsonl");
        let report = temp.path().join("report.json");
        fs::write(&out, "records line\n").expect("records write");

        ensure_report_still_distinct_after_write(&out, Some(&report))
            .expect("distinct missing report path must pass");

        fs::write(&report, "stale report\n").expect("pre-existing report");
        ensure_report_still_distinct_after_write(&out, Some(&report))
            .expect("distinct existing report file must pass");
    }

    #[test]
    fn recheck_exempts_stdout_report() {
        let temp = tempfile::tempdir().expect("temp dir");
        let out = temp.path().join("records.jsonl");
        fs::write(&out, "records line\n").expect("records write");
        ensure_report_still_distinct_after_write(&out, Some(Path::new("-")))
            .expect("stdout report never conflicts");
    }

    #[test]
    fn recheck_is_noop_without_report_flag() {
        let temp = tempfile::tempdir().expect("temp dir");
        let out = temp.path().join("records.jsonl");
        fs::write(&out, "records line\n").expect("records write");
        ensure_report_still_distinct_after_write(&out, None)
            .expect("no report flag, nothing to recheck");
    }
}
