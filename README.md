# Smart Distribution Program

Public, program-only source for the native no-Anchor Smart Distribution Solana
program at `45TYikBDuxngJzkxqiMpuzudnA5VaW47renkjE5XFCge`.

This repository intentionally contains only the Rust workspace, locked
dependencies, program source, release contract, and disclosure policy. It does
not contain the MM Tool application, wallet material, runtime configuration, or
private execution evidence.

## Reproducible build

The release contract pins `solana-verify 0.5.1`, Agave `4.0.0`, and the exact
Solana Foundation build-image digest. Install the tagged CLI and keep the lock
file intact:

```text
cargo install solana-verify --version 0.5.1 --locked
solana-verify build --library-name pump_vault_settlement
```

A local stage tree hash is not commit proof. A release is accepted only when
two clean checkouts of the same published commit produce the same raw
`fileSha256`, Solana `executableHash`, and raw `sizeBytes`, and that exact
artifact is qualified on devnet before mainnet. The executable hash follows
`solana-verify 0.5.1` semantics (trailing zero padding removed before SHA-256);
it must not be replaced with the full-file SHA-256.

## Security

Use GitHub Security Advisories for private disclosure. See `SECURITY.md`.
