# ADR-004 — Base unit / collateral: USD-stable

**Status**: Accepted (2026-05-31)
**Scope**: v0–v3 (platform lifecycle)

## Context

The base denomination determines collateral haircut math, liquidation thresholds, fee accrual semantics, and the chain's natural "unit of account." Every major perp/lending L1 has converged on USD-stable for the same reasons (HL, dYdX, Aave, Compound).

## Decision

USD-stable denominated. Default base: USDC (Circle) at v0; extend to USDT and a Tempo-issued stablecoin at v1+.

## Rationale

- Lending markets work best with stable base — collateral haircuts simpler, liquidation math cleaner.
- Global tradfi (the v3 institutional target audience) expects USD denomination.
- DeFi-native users already operate USD-stable-first; ETH-denominated lending is a Yearn/Maker-style play with different positioning.
- All HL/perp-style platforms converged on USD-base — strong external validation.

## Tradeoffs

USD-stable means Circle/Tether dependency, plus stablecoin depeg risk lives at the protocol level. Mitigation: multi-stablecoin collateral pool from v1, each with its own haircut; Tempo-issued stablecoin at v1+ provides a third uncorrelated peg.

## Forecloses

ETH-denominated lending markets (would be a different product, different positioning).
