// Seed fixture repository containing representative secrets in Rust source code

/// Planted OpenAI API key (static string-literal API key)
pub const OPENAI_API_KEY: &str = "sk-live-5555566666777778888899999000001111122222";

/// Planted Private Key (inlined PEM block)
pub const PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC3Y2F\n-----END PRIVATE KEY-----";

/// Planted High-Entropy Token (GitHub PAT)
pub const GITHUB_PAT: &str = "ghp_1234567890123456789012345678901234567890";
