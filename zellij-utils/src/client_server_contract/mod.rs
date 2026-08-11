include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/prost_ipc/generated_client_server_api.rs"
));

#[cfg(test)]
mod wire_contract_guard {
    use crate::asset_integrity::sha256_hex;
    use crate::consts::CLIENT_SERVER_CONTRACT_VERSION;

    /// Pinned identity of the generated client↔server wire contract.
    ///
    /// The pair (contract version, sha256 of the generated prost file) must
    /// move TOGETHER: shipping a new message variant without bumping
    /// `CLIENT_SERVER_CONTRACT_VERSION` lets a new client reach an old
    /// server's socket, where the unknown oneof decodes as `None` and, on
    /// pre-2026-08 servers, produces an unbounded "Empty ClientToServerMsg"
    /// WARN+ERROR storm (2026-08-05: `DeclareCaller`, ~2 pairs/s, log
    /// rotation destroyed forensics).
    const PINNED_CONTRACT: (usize, &str) = (
        2,
        "bafef87a5b86ae76f9ba26301ac4540f6d65d3a57bc3686a08980c3b2a47f076",
    );

    fn normalized_source_bytes(source: &[u8]) -> Vec<u8> {
        let mut normalized = Vec::with_capacity(source.len());
        let mut bytes = source.iter().copied().peekable();
        while let Some(byte) = bytes.next() {
            if byte == b'\r' && bytes.peek() == Some(&b'\n') {
                continue;
            }
            normalized.push(byte);
        }
        normalized
    }

    #[test]
    fn wire_contract_changes_require_a_version_bump() {
        let source = normalized_source_bytes(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/prost_ipc/client_server_contract.rs"
        )));
        let current = sha256_hex(&source);
        assert_eq!(
            (CLIENT_SERVER_CONTRACT_VERSION, current.as_str()),
            PINNED_CONTRACT,
            "client↔server wire contract drifted: if you changed the protobuf \
             surface, bump CLIENT_SERVER_CONTRACT_VERSION in consts.rs AND \
             update PINNED_CONTRACT here (version + new sha256) in the same \
             commit — old servers cannot decode new message variants."
        );
    }

    #[test]
    fn wire_contract_hash_is_independent_of_checkout_line_endings() {
        assert_eq!(
            normalized_source_bytes(b"one\r\ntwo\r\nthree\n"),
            b"one\ntwo\nthree\n"
        );
    }
}
