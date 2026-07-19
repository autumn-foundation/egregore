use super::*;

impl PrintText for WhoResult<'_> {
    fn as_text(&self) -> String {
        let author = match (self.author_name, self.author_email) {
            (Some(name), Some(email)) => format!("{name} <{email}>"),
            (Some(name), None) => name.to_owned(),
            (None, Some(email)) => format!("<{email}>"),
            (None, None) => "unknown".to_owned(),
        };
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let freshness = self
            .freshness
            .map_or(String::new(), |code| format!(" (freshness: {code})"));
        format!(
            "{} last changed by {} in commit {} @ {} ({}){}\ncorpus: {}",
            self.symbol_name,
            author,
            self.commit_sha,
            self.valid_time,
            path,
            freshness,
            self.corpus_mode
        )
    }
}
