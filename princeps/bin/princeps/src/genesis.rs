//! Public-testnet genesis schema (Stage T1b of `docs/plans/v0-testnet-deploy.md`).
//!
//! Defines [`PrincepsGenesis`] — the umbrella config that pins everything a
//! validator must agree on at boot for the bridge state hash to be
//! deterministic across the network: the validator set, oracle publisher
//! registry, operator registry (ADR-010 Layer 3), seeded lending markets,
//! and the optional seeded demo accounts.
//!
//! This module ONLY defines the schema + load/save round-trip; it does not
//! yet wire into boot. The seed-function refactor (devnet's
//! `seed_v0_lending_markets` / `seed_v0_demo_accounts` learning to load
//! from a `PrincepsGenesis` instead of hardcoded constants) is Stage T1c
//! and lands in a follow-up commit. Keeping that change separate lets the
//! schema land reviewable in isolation.
//!
//! ## Why `String` for `u128` fields
//!
//! JSON numbers are IEEE-754 double-precision and cannot safely represent
//! integers above 2^53 ≈ 9.0×10^15. The IRM rate fields use
//! `Index::RAY / 10_000` ≈ 10^23, well past that ceiling, so every `u128`
//! field on this struct serializes as a decimal string. `serde_json`
//! silently lossy-rounds otherwise — exactly the divergence-across-
//! validators failure mode that motivated this schema in the first place.
//! Same rationale `princeps_node::chain_history` used to switch to
//! adjacently-tagged enums in `406ba5a`.
//!
//! ## Why the validator set is inlined here
//!
//! TD-005 in the testnet plan: the genesis state hash must include the
//! validator set, otherwise nodes with different `--validators` files
//! would agree on bridge state but disagree on consensus identity at
//! genesis. The existing `--validators <path>` flag stays unchanged for
//! T1b; T1c reconciles it (likely by deriving validator-set bytes from
//! the genesis at boot and treating the standalone flag as a legacy
//! override).

use std::path::Path;

use princeps_evm::LiveRethEvmBridge;
use princeps_lending::{AssetId, Bps, IrmParams, Market, MarketId};
use rdk_clearing::Account;
use rdk_clob::AccountId as LendingAccountId;
use serde::{Deserialize, Serialize};

/// Top-level on-disk genesis. One file per chain.
///
/// `chain_id` is the princeps-side identifier (separate from the EVM
/// `chain_id` baked into the alloy `Genesis` chain-spec that
/// `load_chain_spec` consumes — see `main.rs`). Both must be picked
/// together when allocating a new chain; the EVM side governs transaction
/// signing domain separation, the princeps side governs validator-set /
/// bridge-state identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PrincepsGenesis {
    pub chain_id: u32,
    pub validators: Vec<GenesisValidator>,
    pub oracle_publishers: Vec<GenesisPublisher>,
    pub operators: Vec<GenesisOperator>,
    pub markets: Vec<GenesisMarket>,
    /// Optional. Empty on a "clean" testnet; populated on devnet-style
    /// genesis files that want pre-seeded borrower scenarios for the
    /// liquidator-bot / lending-demo flows.
    #[serde(default)]
    pub demo_accounts: Vec<GenesisDemoAccount>,
}

/// Validator entry. Wire-compatible with the standalone `ValidatorSetFile`
/// in `main.rs` so the same hex format can be reused at T1c.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenesisValidator {
    /// Human-readable label for ops / dashboards. Not consensus-relevant.
    pub moniker: String,
    /// Hex-encoded 32-byte Ed25519 public key (no `0x` prefix).
    pub pubkey_hex: String,
    pub voting_power: u64,
    /// libp2p multiaddr where peers can reach this validator. Optional;
    /// matches the standalone `ValidatorEntry::peer_multiaddr` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_multiaddr: Option<String>,
}

/// Oracle publisher entry. Maps to `rdk_oracle::{FeedId, PublisherKey}`
/// at boot. Mirrors how `princeps_node::operator` documents publisher
/// re-registration: "publishers are re-registered by the binary at
/// boot" — this is the source-of-truth list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenesisPublisher {
    /// Matches `rdk_oracle::FeedId(pub u32)`.
    pub feed_id: u32,
    /// Diagnostics-only label (e.g. "foundation-eth-feed-a").
    pub label: String,
    /// Hex-encoded 33-byte SEC1-compressed secp256k1 public key
    /// (no `0x` prefix; 66 hex chars).
    pub pubkey_hex: String,
}

/// Operator entry for ADR-010 Layer 3 socialization declarations. Maps
/// to `princeps_node::operator::{OperatorId, OperatorKey}` at boot.
/// `princeps_node::operator` notes: "operator keys are NOT included in
/// `CoordinatorSnapshot`. The binary re-registers operators at boot
/// from the deployment config." This is that deployment config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenesisOperator {
    /// Matches `princeps_node::operator::OperatorId(pub u32)`.
    pub operator_id: u32,
    pub label: String,
    /// Hex-encoded 33-byte SEC1-compressed secp256k1 public key.
    pub pubkey_hex: String,
}

/// Lending market entry. Mirrors the parameters
/// `seed_v0_lending_markets` passes into `Market::new` today; T1c will
/// switch that function to read from this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenesisMarket {
    /// Matches `princeps_lending::MarketId(pub u32)`.
    pub market_id: u32,
    /// Diagnostics-only label (e.g. "USDC-ETH").
    pub label: String,
    /// Matches `princeps_lending::AssetId(pub u32)`. The asset that
    /// borrowers receive.
    pub underlying_asset_id: u32,
    pub collateral_asset_id: u32,
    pub irm: GenesisIrmParams,
    pub liquidation_threshold_bps: u16,
    pub liquidation_bonus_bps: u16,
    pub reserve_factor_bps: u16,
    /// Initial `total_supplied` to seed the bridge-implicit pool.
    /// Decimal string to fit u128. Matches the `market.total_supplied =
    /// 1_000_000` line in `seed_v0_lending_markets` today.
    pub initial_total_supplied: String,
}

/// IRM parameter sub-struct. All rate fields are per-block RAY-scaled
/// (see `princeps_lending::IrmParams` for the math). Decimal strings
/// for the u128 fields per the module-level comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenesisIrmParams {
    pub base_rate_per_block: String,
    pub slope_below_kink_per_block: String,
    pub slope_above_kink_per_block: String,
    pub kink_bps: u16,
}

/// Pre-seeded borrower scenario. Optional — empty on production
/// testnet genesis; populated for devnet / lending-demo flows. T1c
/// will wire this through to the bridge as the current
/// `seed_v0_demo_accounts` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenesisDemoAccount {
    pub account_id: u64,
    pub market_id: u32,
    /// Decimal string (u128).
    pub collateral: String,
    /// Decimal string (u128).
    pub borrow: String,
    /// Seed price for collateral asset at borrow time. Decimal string
    /// (u128). Matches the `(coll_price, underlying_price)` pair
    /// `lending_borrow` takes today (currently `(1, 1)` in the seed).
    pub seed_price_collateral: String,
    pub seed_price_underlying: String,
}

impl PrincepsGenesis {
    /// Load a genesis from a path. Errors surface as `eyre::Report` so
    /// `main.rs` can propagate them uniformly with the other config
    /// loaders (`load_chain_spec`, `load_validator_set`).
    pub(crate) fn load(path: &Path) -> eyre::Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| eyre::eyre!("failed to read genesis at {}: {e}", path.display()))?;
        let g: PrincepsGenesis = serde_json::from_slice(&bytes)
            .map_err(|e| eyre::eyre!("malformed genesis at {}: {e}", path.display()))?;
        Ok(g)
    }
}

impl GenesisMarket {
    /// Convert into the runtime `princeps_lending::Market` shape that
    /// the bridge owns. Mirrors the parameters
    /// `seed_v0_lending_markets` passes into `Market::new` today, with
    /// the u128 string fields parsed back to numbers.
    ///
    /// `at_block` is the starting block for `last_accrual_block` —
    /// callers typically pass `0` on a fresh chain, matching the
    /// existing seed code.
    pub(crate) fn build(&self, at_block: u64) -> eyre::Result<Market> {
        let parse_u128 = |s: &str, field: &str| -> eyre::Result<u128> {
            s.parse()
                .map_err(|e| eyre::eyre!("market {} {field}: {e}", self.market_id))
        };
        let mut m = Market::new(
            MarketId(self.market_id),
            AssetId(self.underlying_asset_id),
            AssetId(self.collateral_asset_id),
            IrmParams {
                base_rate_per_block: parse_u128(&self.irm.base_rate_per_block, "base_rate_per_block")?,
                slope_below_kink_per_block: parse_u128(
                    &self.irm.slope_below_kink_per_block,
                    "slope_below_kink_per_block",
                )?,
                slope_above_kink_per_block: parse_u128(
                    &self.irm.slope_above_kink_per_block,
                    "slope_above_kink_per_block",
                )?,
                kink_bps: Bps(self.irm.kink_bps),
            },
            Bps(self.liquidation_threshold_bps),
            Bps(self.liquidation_bonus_bps),
            Bps(self.reserve_factor_bps),
            at_block,
        );
        m.total_supplied = parse_u128(&self.initial_total_supplied, "initial_total_supplied")?;
        Ok(m)
    }
}

/// Apply a genesis to a fresh bridge: register every market and replay
/// every demo account's deposit + borrow. Equivalent to calling
/// `seed_v0_lending_markets` + `seed_v0_demo_accounts` when the genesis
/// is `devnet-genesis.json`; arbitrary configs work the same way.
///
/// Markets are built first (so u128-parse errors surface before any
/// bridge mutation); only then are accounts replayed against the
/// installed markets.
///
/// Caller must guarantee the bridge is on a fresh chain — replaying
/// against a bridge that already has positions for the same accounts
/// will fail at the second `lending_borrow`, exactly as the hardcoded
/// seed path does.
pub(crate) fn apply_to_bridge<P>(
    genesis: &PrincepsGenesis,
    bridge: &LiveRethEvmBridge<P>,
) -> eyre::Result<()> {
    let built_markets: Vec<Market> = genesis
        .markets
        .iter()
        .map(|gm| gm.build(0))
        .collect::<eyre::Result<_>>()?;

    bridge.with_markets_mut(|m| {
        for market in built_markets {
            m.insert(market.id, market);
        }
    });

    for da in &genesis.demo_accounts {
        let acct = LendingAccountId(da.account_id);
        let market_id = MarketId(da.market_id);
        let parse_u128 = |s: &str, field: &str| -> eyre::Result<u128> {
            s.parse()
                .map_err(|e| eyre::eyre!("demo_account {} {field}: {e}", da.account_id))
        };
        let collateral = parse_u128(&da.collateral, "collateral")?;
        let borrow = parse_u128(&da.borrow, "borrow")?;
        let coll_price = parse_u128(&da.seed_price_collateral, "seed_price_collateral")?;
        let und_price = parse_u128(&da.seed_price_underlying, "seed_price_underlying")?;

        bridge
            .lending_deposit_collateral(acct, market_id, collateral)
            .map_err(|e| eyre::eyre!("demo_account {} deposit: {e:?}", da.account_id))?;
        bridge
            .lending_borrow(acct, market_id, borrow, coll_price, und_price)
            .map_err(|e| eyre::eyre!("demo_account {} borrow: {e:?}", da.account_id))?;
        bridge.with_accounts_mut(|map| {
            map.insert(acct, Account::flat(acct));
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid genesis — exercises every required field at least once.
    fn sample() -> PrincepsGenesis {
        PrincepsGenesis {
            chain_id: 424242,
            validators: vec![GenesisValidator {
                moniker: "alice".into(),
                pubkey_hex: "00".repeat(32),
                voting_power: 1,
                peer_multiaddr: Some("/ip4/127.0.0.1/tcp/27656".into()),
            }],
            oracle_publishers: vec![GenesisPublisher {
                feed_id: 0,
                label: "eth-usd-a".into(),
                pubkey_hex: "02".to_string() + &"00".repeat(32),
            }],
            operators: vec![GenesisOperator {
                operator_id: 0,
                label: "foundation-op-0".into(),
                pubkey_hex: "03".to_string() + &"00".repeat(32),
            }],
            markets: vec![GenesisMarket {
                market_id: 0,
                label: "USDC-ETH".into(),
                underlying_asset_id: 1,
                collateral_asset_id: 0,
                irm: GenesisIrmParams {
                    base_rate_per_block: "0".into(),
                    // RAY / 10_000 = 10^23 — beyond JSON-number safe range.
                    slope_below_kink_per_block: "100000000000000000000000".into(),
                    // RAY / 1_000 = 10^24.
                    slope_above_kink_per_block: "1000000000000000000000000".into(),
                    kink_bps: 8_000,
                },
                liquidation_threshold_bps: 9_500,
                liquidation_bonus_bps: 500,
                reserve_factor_bps: 1_000,
                initial_total_supplied: "1000000".into(),
            }],
            demo_accounts: vec![],
        }
    }

    #[test]
    fn json_round_trip() {
        let g = sample();
        let s = serde_json::to_string(&g).expect("serialize");
        let back: PrincepsGenesis = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(g, back);
    }

    #[test]
    fn round_trip_via_disk() {
        let g = sample();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("genesis.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&g).expect("ser")).expect("write");
        let back = PrincepsGenesis::load(&path).expect("load");
        assert_eq!(g, back);
    }

    /// Pin the RAY-scale slope strings parse as the expected u128 values.
    /// Catches accidental edits to the sample numbers that would silently
    /// change market params at load time.
    #[test]
    fn ray_scale_slopes_decode_correctly() {
        let g = sample();
        let s_below: u128 = g.markets[0].irm.slope_below_kink_per_block.parse().unwrap();
        let s_above: u128 = g.markets[0].irm.slope_above_kink_per_block.parse().unwrap();
        // RAY = 10^27; RAY / 10_000 = 10^23; RAY / 1_000 = 10^24.
        assert_eq!(s_below, 100_000_000_000_000_000_000_000_u128);
        assert_eq!(s_above, 1_000_000_000_000_000_000_000_000_u128);
    }

    /// Load the committed `princeps/genesis/testnet-genesis.json` and
    /// confirm it parses. This is the file an operator would actually
    /// point a validator at; if it stops parsing, the schema and the
    /// sample have drifted apart.
    #[test]
    fn committed_testnet_genesis_loads() {
        let g = PrincepsGenesis::load(&committed_genesis_path("testnet-genesis.json"))
            .expect("testnet genesis must parse");
        assert!(g.chain_id > 0);
        assert!(!g.validators.is_empty(), "testnet needs ≥1 validator");
        assert!(!g.markets.is_empty(), "testnet needs ≥1 lending market");
    }

    /// Sibling check for `devnet-genesis.json` — same schema, populated
    /// `demo_accounts`. If this fails the devnet boot path is broken
    /// even though the testnet one still works.
    #[test]
    fn committed_devnet_genesis_loads() {
        let g = PrincepsGenesis::load(&committed_genesis_path("devnet-genesis.json"))
            .expect("devnet genesis must parse");
        assert!(!g.markets.is_empty());
        assert_eq!(g.demo_accounts.len(), 5, "devnet seeds 5 demo accounts");
    }

    /// The keystone determinism guarantee for T1c-a: building the
    /// `princeps_lending::Market` from `devnet-genesis.json` produces
    /// a struct byte-equal to what the hardcoded `seed_v0_lending_markets`
    /// constructs. If anyone edits one path without updating the
    /// other, this fires.
    #[test]
    fn devnet_genesis_market_matches_seed_v0() {
        use princeps_lending::{AssetId, Bps, Index, IrmParams, Market, MarketId};
        let g = PrincepsGenesis::load(&committed_genesis_path("devnet-genesis.json"))
            .expect("load devnet genesis");
        let built = g.markets[0].build(0).expect("build");

        // Mirrors `seed_v0_lending_markets` exactly. If that function
        // ever changes, this expected-value block must change with it.
        let mut expected = Market::new(
            MarketId(0),
            AssetId(1),
            AssetId(0),
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: Index::RAY / 10_000,
                slope_above_kink_per_block: Index::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500),
            Bps(500),
            Bps(1_000),
            0,
        );
        expected.total_supplied = 1_000_000;

        assert_eq!(built, expected, "genesis-built market must equal seed_v0 market");
    }

    /// Helper — resolves a committed genesis filename to its absolute
    /// path. CARGO_MANIFEST_DIR is `bin/princeps`; the genesis dir
    /// lives at the princeps/ root (TD-005 — top-level for operator
    /// visibility).
    fn committed_genesis_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../genesis")
            .join(name)
    }

    /// T1d — the genesis-hash determinism guarantee. Two independent
    /// loads of the same genesis file, applied to two independent
    /// fresh bridges, must produce byte-identical (markets, positions,
    /// accounts) state. This is the single-process surrogate for the
    /// cross-validator convergence property a real multi-validator
    /// network needs at genesis: if either side of the load → apply
    /// pipeline becomes non-deterministic (HashMap iteration leaking
    /// into a Vec field, system time creeping into a struct,
    /// `Position::empty` picking up randomness, etc.), two validators
    /// running the same binary against the same genesis file would
    /// diverge at block 0. This test refuses that.
    #[test]
    fn genesis_apply_is_deterministic_across_loads() {
        let path = committed_genesis_path("devnet-genesis.json");

        let g1 = PrincepsGenesis::load(&path).expect("load 1");
        let bridge1 = LiveRethEvmBridge::new((), crate::dev_chain_spec());
        apply_to_bridge(&g1, &bridge1).expect("apply 1");

        let g2 = PrincepsGenesis::load(&path).expect("load 2");
        let bridge2 = LiveRethEvmBridge::new((), crate::dev_chain_spec());
        apply_to_bridge(&g2, &bridge2).expect("apply 2");

        // Structural equality on each sub-state — sharper error
        // messages when one of the three diverges.
        assert_eq!(
            bridge1.markets_snapshot(),
            bridge2.markets_snapshot(),
            "markets diverged across independent loads"
        );
        assert_eq!(
            bridge1.positions_snapshot(),
            bridge2.positions_snapshot(),
            "positions diverged across independent loads"
        );
        assert_eq!(
            bridge1.snapshot().accounts,
            bridge2.snapshot().accounts,
            "accounts diverged across independent loads"
        );

        // Belt-and-suspenders: serialize the combined state and
        // compare raw bytes. Catches any divergence that the
        // structural assertions above might miss (e.g., a future
        // field added to Market/Position/Account that derives
        // PartialEq trivially but serializes non-deterministically).
        let combined1 = serde_json::to_vec(&(
            bridge1.markets_snapshot(),
            bridge1.positions_snapshot(),
            bridge1.snapshot().accounts,
        ))
        .expect("ser 1");
        let combined2 = serde_json::to_vec(&(
            bridge2.markets_snapshot(),
            bridge2.positions_snapshot(),
            bridge2.snapshot().accounts,
        ))
        .expect("ser 2");
        assert_eq!(combined1, combined2, "combined serialized state diverged");
    }
}
