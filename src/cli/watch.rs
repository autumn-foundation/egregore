use super::*;

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn watch_cmd(
    data_dir: &Path,
    antigravity_dir: Option<&Path>,
    codex_dir: Option<&Path>,
    claude_dir: Option<&Path>,
    poll_interval: u64,
    embed: bool,
) -> Result<()> {
    let home = crate::watch::get_home_dir();

    let default_antigravity = home.as_ref().map(|h| h.join(".gemini/antigravity/brain"));
    let default_codex = home.as_ref().map(|h| h.join(".codex/sessions"));
    let default_claude = home.as_ref().map(|h| h.join(".claude/projects"));

    // Warn only if paths were explicitly requested but do not exist
    if let Some(p) = antigravity_dir.filter(|p| !p.exists()) {
        eprintln!(
            "[Watcher Warning] Specified Antigravity directory does not exist: {}",
            p.display()
        );
    }
    if let Some(p) = codex_dir.filter(|p| !p.exists()) {
        eprintln!(
            "[Watcher Warning] Specified Codex directory does not exist: {}",
            p.display()
        );
    }
    if let Some(p) = claude_dir.filter(|p| !p.exists()) {
        eprintln!(
            "[Watcher Warning] Specified Claude Code directory does not exist: {}",
            p.display()
        );
    }

    // Filter resolved paths to only watch them if they actually exist
    let resolved_antigravity = antigravity_dir
        .or(default_antigravity.as_deref())
        .filter(|p| p.exists());
    let resolved_codex = codex_dir
        .or(default_codex.as_deref())
        .filter(|p| p.exists());
    let resolved_claude = claude_dir
        .or(default_claude.as_deref())
        .filter(|p| p.exists());

    // Zero-watch validation: bail out if no valid directories remain
    if resolved_antigravity.is_none() && resolved_codex.is_none() && resolved_claude.is_none() {
        anyhow::bail!(
            "No valid agent directories to watch. Ensure at least one directory exists or was explicitly specified."
        );
    }

    crate::watch::watch(
        data_dir,
        resolved_antigravity,
        resolved_codex,
        resolved_claude,
        std::time::Duration::from_secs(poll_interval),
        embed,
        None,
    )?;

    Ok(())
}
