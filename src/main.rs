//! Binary entry point for Aletheia Codegraph.

fn main() {
    if let Err(error) = aletheia_codegraph::cli::run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
