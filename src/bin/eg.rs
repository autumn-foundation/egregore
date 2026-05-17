//! Short binary alias for Egregore.

fn main() {
    if let Err(error) = aletheia_egregore::cli::run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
