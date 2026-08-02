//! Do the plugin bytes this binary ships still match the receipt it shipped with?
//!
//! The failure this exists for: a plugin gets rebuilt, the receipt (SHA256SUMS)
//! or the embedded bytes lag behind, and the operator debugs chrome that never
//! contained the fix — or worse, a binary quietly mixes plugin generations.
//! `consts.rs` proves the embed/receipt/disk agreement at TEST time; this module
//! is the RUNTIME half, surfaced by `vc-frame doctor` so a drifted install
//! stops "jakoś działa"-ing and says so out loud.
//!
//! [`install_freshness`] answers "is the binary behind the checkout?";
//! this module answers "is the binary internally consistent with itself?".
//!
//! [`install_freshness`]: crate::install_freshness
//!
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// The committed plugin receipt, embedded verbatim at build time. Format is
/// `shasum -a 256` output: `<hex>  <name>` per line.
pub const PLUGIN_RECEIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/plugins/SHA256SUMS"
));

/// Lowercase hex SHA-256 of arbitrary bytes — the one hash spelling every
/// vc-frame surface (receipt, doctor JSON, vibecrafted crosscheck) agrees on.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Parse a `SHA256SUMS`-shaped receipt into `name → hex` pairs. Lines that do
/// not carry both fields are skipped — a receipt is evidence, not a parser
/// battleground; missing entries surface as `unreceipted` at verify time.
pub fn parse_receipt(receipt: &str) -> HashMap<String, String> {
    receipt
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?;
            let name = fields.next()?;
            if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                Some((name.to_owned(), sha.to_ascii_lowercase()))
            } else {
                None
            }
        })
        .collect()
}

/// The runtime verdict on the embedded plugins vs the embedded receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginReceiptCheck {
    /// Debug builds with `plugins_from_target` embed freshly-built plugins
    /// from `target/` — the shipped receipt genuinely does not apply.
    NotApplicable(&'static str),
    Report {
        /// Plugin names whose embedded bytes hash to the receipt entry.
        verified: Vec<String>,
        /// `name: embedded <sha8> ≠ receipt <sha8>` — the shouting case.
        mismatched: Vec<String>,
        /// Embedded plugins the receipt does not know about.
        unreceipted: Vec<String>,
        /// Receipt entries with no embedded counterpart.
        unembedded: Vec<String>,
    },
}

/// Receipt entries built for the test suites and deliberately never
/// embedded in the release binary — their absence is a known exception,
/// not drift, so the doctor must not warn on them.
///
/// The cfg mirrors the only non-test consumer (the receipt-verifying branch
/// of [`verify_embedded_plugins`]): in a `plugins_from_target` debug build
/// that branch is compiled out, and without the mirror this const would trip
/// `-D dead_code`.
#[cfg(any(
    not(feature = "plugins_from_target"),
    not(debug_assertions),
    test
))]
const TEST_ONLY_RECEIPT_ENTRIES: [&str; 1] = ["fixture-plugin-for-tests.wasm"];

/// Hash every embedded plugin and compare against the embedded receipt.
pub fn verify_embedded_plugins() -> PluginReceiptCheck {
    #[cfg(all(feature = "plugins_from_target", debug_assertions))]
    {
        PluginReceiptCheck::NotApplicable(
            "debug build embeds plugins from target/ — the shipped receipt does not apply",
        )
    }
    #[cfg(any(not(feature = "plugins_from_target"), not(debug_assertions)))]
    {
        let mut receipt = parse_receipt(PLUGIN_RECEIPT);
        for test_only in TEST_ONLY_RECEIPT_ENTRIES {
            receipt.remove(test_only);
        }
        verify_map_against_receipt(
            crate::consts::ASSET_MAP.iter().filter_map(|(path, bytes)| {
                let name = path.file_name()?.to_str()?;
                name.ends_with(".wasm")
                    .then(|| (name.to_owned(), bytes.as_slice()))
            }),
            &receipt,
        )
    }
}

/// The pure core of [`verify_embedded_plugins`], testable without the real
/// asset map: compare named byte blobs against a parsed receipt.
pub fn verify_map_against_receipt<'a>(
    embedded: impl Iterator<Item = (String, &'a [u8])>,
    receipt: &HashMap<String, String>,
) -> PluginReceiptCheck {
    let mut verified = Vec::new();
    let mut mismatched = Vec::new();
    let mut unreceipted = Vec::new();
    let mut seen = Vec::new();

    for (name, bytes) in embedded {
        seen.push(name.clone());
        let embedded_sha = sha256_hex(bytes);
        match receipt.get(&name) {
            Some(receipt_sha) if *receipt_sha == embedded_sha => verified.push(name),
            Some(receipt_sha) => mismatched.push(format!(
                "{name}: embedded {} ≠ receipt {}",
                &embedded_sha[..8],
                &receipt_sha[..8]
            )),
            None => unreceipted.push(name),
        }
    }

    let mut unembedded: Vec<String> = receipt
        .keys()
        .filter(|name| !seen.iter().any(|s| s == *name))
        .cloned()
        .collect();

    verified.sort();
    mismatched.sort();
    unreceipted.sort();
    unembedded.sort();

    PluginReceiptCheck::Report {
        verified,
        mismatched,
        unreceipted,
        unembedded,
    }
}

impl PluginReceiptCheck {
    /// True only when the binary provably disagrees with its own receipt.
    pub fn is_compromised(&self) -> bool {
        match self {
            PluginReceiptCheck::NotApplicable(_) => false,
            PluginReceiptCheck::Report {
                mismatched,
                unreceipted,
                unembedded,
                ..
            } => !mismatched.is_empty() || !unreceipted.is_empty() || !unembedded.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_of(entries: &[(&str, &[u8])]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), sha256_hex(bytes)))
            .collect()
    }

    #[test]
    fn matching_bytes_verify_against_the_receipt() {
        let receipt = receipt_of(&[("a.wasm", b"alpha"), ("b.wasm", b"beta")]);
        let embedded = vec![
            ("a.wasm".to_owned(), b"alpha" as &[u8]),
            ("b.wasm".to_owned(), b"beta" as &[u8]),
        ];
        let check = verify_map_against_receipt(embedded.into_iter(), &receipt);
        assert!(!check.is_compromised());
        match check {
            PluginReceiptCheck::Report { verified, .. } => {
                assert_eq!(verified, vec!["a.wasm", "b.wasm"])
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_drifted_plugin_is_named_with_both_hashes() {
        let receipt = receipt_of(&[("a.wasm", b"alpha")]);
        let embedded = vec![("a.wasm".to_owned(), b"not-alpha" as &[u8])];
        let check = verify_map_against_receipt(embedded.into_iter(), &receipt);
        assert!(check.is_compromised());
        match check {
            PluginReceiptCheck::Report { mismatched, .. } => {
                assert_eq!(mismatched.len(), 1);
                assert!(mismatched[0].starts_with("a.wasm: embedded "));
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn receipt_and_embed_set_differences_both_surface() {
        let receipt = receipt_of(&[("only-in-receipt.wasm", b"x")]);
        let embedded = vec![("only-embedded.wasm".to_owned(), b"y" as &[u8])];
        match verify_map_against_receipt(embedded.into_iter(), &receipt) {
            PluginReceiptCheck::Report {
                unreceipted,
                unembedded,
                ..
            } => {
                assert_eq!(unreceipted, vec!["only-embedded.wasm"]);
                assert_eq!(unembedded, vec!["only-in-receipt.wasm"]);
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn the_shipped_receipt_parses_and_names_every_bundled_plugin() {
        let receipt = parse_receipt(PLUGIN_RECEIPT);
        assert!(
            receipt.len() >= 10,
            "the shipped SHA256SUMS should carry the full plugin fleet, got {}",
            receipt.len()
        );
        assert!(receipt.contains_key("compact-bar.wasm"));
        assert!(receipt.contains_key("status-bar.wasm"));
        assert!(receipt.contains_key("session-manager.wasm"));
    }

    #[test]
    fn garbage_receipt_lines_are_skipped_not_fatal() {
        let receipt = parse_receipt("not-a-sha  x.wasm\n\ndeadbeef short.wasm\n");
        assert!(receipt.is_empty());
    }

    #[test]
    fn test_only_receipt_entries_cover_the_shipped_fixture() {
        // The shipped receipt carries the e2e fixture plugin; the release
        // binary deliberately never embeds it. The exception list must name
        // it, or every clean build warns "in the receipt but not embedded".
        let receipt = parse_receipt(PLUGIN_RECEIPT);
        for test_only in TEST_ONLY_RECEIPT_ENTRIES {
            assert!(
                receipt.contains_key(test_only),
                "{test_only} vanished from SHA256SUMS — drop it from TEST_ONLY_RECEIPT_ENTRIES"
            );
        }
    }
}
