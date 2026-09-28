# Changes

## 2026-09-28 — two-vault upgrade candidate

- Tag23 and tag24 provide atomic two-vault routes with fresh per-use identities, measured fee-aware transfers, quote-denominated swaps, refunds and complete temporary cleanup.
- Pump curve validation rejects malformed known cashback/admin Boolean fields while preserving the official SDK's prefix decoding and opaque extension semantics, including observed174-byte curve accounts.
- Runtime regressions cover missing source signatures, custody-account aliases, expiry, delegated source tokens and foreign close authorities; they assert rejection before CPI and exact rollback.
- Official-program test outputs can use separate fixture roots to preserve historical evidence when qualifying a new candidate.
- The release deployment budget accommodates the larger executable: at the observed rent rate, the 359,456-byte candidate needs 859,332,800 additional mainnet rent lamports and a temporary buffer of 1,826,874,680 lamports. Caps are 900,000,000 net and 2,750,000,000 temporary total funding, with no market trades included. Re-read rent, authority and balances immediately before any deployment; the user must separately authorize it.

Ordinary Pump curve USDC remains rejected because the upstream volume-owned quote ATA cannot be closed by the tested claim/parent-close sequence. Deployment status is recorded in the dated upgrade plan, not inferred from this changelog.
