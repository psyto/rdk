# ADR-007 — Token / value capture: none until real revenue

**Status**: Accepted (2026-05-31)
**Scope**: v0–v3 (platform lifecycle)

## Context

Tokenomics choice has eaten many DeFi platforms: pre-PMF tokens distort incentives, drive mercenary liquidity, expose the team to regulatory complexity, and damage brand credibility when they crash. Anchor / Terra / dozens of others all illustrate the failure mode. The opposite extreme (no token ever) gives up a coordination tool that may matter at v3 decentralization.

## Decision

No protocol token at v0 or v1. Earliest token consideration: post-v1, only if real fee revenue exists and tokenization clearly accelerates the product (e.g. sequencer decentralization at v3).

## Rationale

- Token-before-PMF kills brand credibility in DeFi 2025/2026. Pre-PMF tokens train users to expect emissions, distort the actual value of the product, and damage the team's reputation when the token underperforms.
- v0–v1 product-market-fit on lending/options is the real moat — token is a distribution/governance tool, not a product.
- Fee revenue from real usage gives actual data on what to tokenize (sequencer rights, fee discounts, governance, staking).
- "We don't have a token yet" is increasingly a positive signal in institutional finance circles — flags real-product focus.

## Tradeoffs

No token = no token-driven liquidity mining = slower TVL bootstrap. Counter-mitigation: launch tenants (Yogi/Kodiak/Surge as planned v2 structured products) already have liquidity; institutional tenants are not motivated by token incentives.

## Forecloses

Nothing — token can launch when right, in whatever form makes sense based on actual usage data.
