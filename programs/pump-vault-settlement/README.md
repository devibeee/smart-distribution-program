# Pump vault settlement

Native Solana program at `45TYikBDuxngJzkxqiMpuzudnA5VaW47renkjE5XFCge`. Existing settlement and stealth handlers remain available; tag23 adds native-SOL two-vault distribution, and tag24 adds quote-aware two-vault adapters for Pump curve, Pump AMM, LaunchLab, CPMM and CLMM.

Each two-vault use binds source, recipient, base/quote mints and a fresh nonce. Source and recipient sign; the program measures transfers/swaps in raw token units, applies minimum proceeds and buy caps, preserves the recipient's existing balances, refunds residual funds and closes temporary accounts atomically. Recipient base-token and nonempty source quote-refund accounts are durable outputs. Closed PDAs leave no permanent nonce tombstone; the server-side registry/journal enforces operational non-reuse.

Pump curve ordinary USDC currently fails complete temporary cleanup on the captured official deployment and is rejected with error 2401 and atomic rollback. Unknown or unqualified variants must not be advertised as supported. See the repository [release procedure](../../README.md).

## Build and test

From the repository root, `cargo test --manifest-path programs/pump-vault-settlement/Cargo.toml` runs host checks. `cargo-build-sbf --manifest-path programs/pump-vault-settlement/Cargo.toml` builds a local candidate; record the exact builder and artifact hash. The production verifiable-build contract under the root `release/` directory is a separate qualification requirement.

The application repository also contains SBF runtime and official-program snapshot tests. Those fixtures and operational receipts are outside this program-only source repository. Local tests do not prove network finality.

Deployment requires the correct cluster genesis, existing program/authority binding, funded fee payer, reviewed artifact, rollback bytecode and finalized post-upgrade byte comparison. The user must authorize deployment separately from development. Do not use deterministic public test keys on a network or infer authority from historical runbooks.
