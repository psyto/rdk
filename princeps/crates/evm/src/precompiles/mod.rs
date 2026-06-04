//! Custom REVM precompiles that expose CLOB state to EVM execution.
//!
//! Stage 9b — live CLOB state. The precompile reads from a process-global
//! `Arc<Mutex<Book>>` that the bridge installs at construction. Hardcoded
//! values from 9a are gone; smart contracts now see real best-bid data.
//!
//! ### Why a process-global, not a closure-captured reference
//!
//! REVM's `PrecompileFn = fn(&[u8], u64, u64) -> PrecompileResult` is a
//! **function pointer**, not an `Fn` closure. Function pointers can't capture
//! environment, so the only way to get per-instance state into the precompile
//! is via global storage. The trade-off: only one CLOB can be installed
//! per process. For single-validator princeps deployments that's fine. Future
//! REVM versions may expand the precompile signature; until then, the global
//! is load-bearing infrastructure.
//!
//! Precompile address conventions:
//!   - princeps reserves the range `0x0000...0c1b` upwards (mnemonic: "CLB")
//!   - addresses 1-9 are Ethereum's standard precompiles (ECDSA recover etc.)
//!   - we stay well above those to avoid collisions

use alloy_evm::revm::precompile::{
    Precompile, PrecompileId, PrecompileOutput, PrecompileResult, Precompiles,
};
use alloy_primitives::{address, Address, Bytes};
use rdk_clearing::Account;
use rdk_clob::{AccountId, Book, Fill, Order, OrderId, OrderType, Price, Qty, Side};
use rdk_funding::Notional;
use princeps_lending::{
    borrow as lending_position_borrow, compute_health_factor as lending_compute_health_factor,
    deposit_collateral as lending_position_deposit_collateral,
    repay as lending_position_repay,
    socialize_residual as lending_socialize_residual,
    supply as lending_position_supply,
    withdraw_collateral as lending_position_withdraw_collateral,
    withdraw_supply as lending_position_withdraw_supply,
    Index as LendingIndex, Market, MarketId, Position,
};
use princeps_node::chain_history::{ChainEvent, ChainHistoryStore};
use princeps_node::operator::{
    verify_socialization_declaration, OperatorId, OperatorRegistry, OperatorSignature,
    SocializationDeclaration,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};

mod revert_guard;
pub use revert_guard::PrincepsRevertGuard;

/// Address of the "read best bid" precompile.
///
/// Solidity call shape: `staticcall(gas, 0x...0c1b, calldata=empty, ...) → (price: u256, qty: u256)`
pub const CLOB_READ_BEST_BID: Address = address!("0x0000000000000000000000000000000000000c1b");

/// Address of the "place order" precompile (write path — Stage 9c).
///
/// Solidity call shape (ABI-aligned 128-byte input):
/// `call(gas, 0x...0c1c, calldata=(uint64 account, uint8 side, uint64 price, uint64 qty), ...) → uint256 order_id`
///
/// `side` encoding: 0 = Buy, 1 = Sell. Any other value → call returns 0
/// (rejected, no state change). Order type is hardcoded to Limit at v0.
///
/// Return: 32 bytes; the last 8 are a big-endian u64 `order_id`. A return
/// of 0 means the order was rejected (no CLOB installed, malformed input,
/// or invalid side byte) — distinguishable from "placed" because allocated
/// IDs start at 1.
pub const CLOB_PLACE_ORDER: Address = address!("0x0000000000000000000000000000000000000c1c");

/// Address of the "deposit collateral" precompile (Stage 17c).
///
/// Solidity call shape (ABI-aligned 64-byte input):
/// `call(gas, 0x...0c1d, calldata=(uint64 account, int64 amount), ...) → uint256 new_balance`
///
/// `amount` is signed (encoded as a 32-byte two's-complement big-endian
/// integer); positive credits the account's collateral, negative debits.
/// The returned `new_balance` is the post-deposit collateral as a
/// signed 256-bit integer. A return of 0 means "rejected" — currently
/// only triggered when no account map is installed; in production it
/// would also fire on malformed input or unauthorized accounts.
///
/// Same caveat as `clob_place_order` (and a known v0 limitation): the
/// mutation lands in the bridge's account map regardless of whether
/// the calling EVM transaction reverts. Tying mutations to the
/// transaction's success is a future hardening item.
pub const PRINCEPS_DEPOSIT: Address = address!("0x0000000000000000000000000000000000000c1d");

/// Address of the "withdraw collateral" precompile (Stage 17e).
///
/// Solidity call shape (ABI-aligned 64-byte input):
/// `call(gas, 0x...0c1e, calldata=(uint64 account, uint64 amount), ...) → uint256 new_balance_or_zero`
///
/// Returns the new collateral balance as a 32-byte sign-extended
/// int. Returns `1` packed in the rightmost byte (i.e., a 32-byte
/// value equal to `1`) is technically achievable for a real balance
/// of 1, so callers should distinguish success from rejection by
/// comparing against the pre-call balance rather than reading the
/// return alone.
///
/// **The zero-return is overloaded with success:** if the post-
/// withdraw balance happens to be exactly 0 (the caller drained
/// the account), the return is also 0. Future hardening should
/// use a richer return shape — for v0 the simplicity wins.
///
/// Rejections (no account map installed, account doesn't exist,
/// insufficient balance, input shorter than 64 bytes) return all
/// zeros. Same caveat as the other write precompiles: the
/// withdrawal lands regardless of whether the calling EVM
/// transaction reverts.
pub const PRINCEPS_WITHDRAW: Address = address!("0x0000000000000000000000000000000000000c1e");

/// The minimum gas charge for invoking a CLOB precompile. Tuned later.
const CLOB_BASE_GAS_COST: u64 = 500;

/// Base gas for the deposit precompile (Stage 17c). Same magnitude as
/// the CLOB precompiles; tuned later.
const DEPOSIT_BASE_GAS_COST: u64 = 500;

/// Base gas for the withdraw precompile (Stage 17e). Same magnitude.
const WITHDRAW_BASE_GAS_COST: u64 = 500;

// === Stage 21: Lending precompile addresses ===
//
// Continue the `0x0c1*` range. Lending precompiles do more work than
// CLOB precompiles (per-position state mutation + health check +
// market totals update), so gas costs are higher than the 500-baseline
// of CLOB precompiles. Final tuning happens when v0 testnet runs at
// realistic load.

/// `princeps_lending_deposit_collateral` precompile address (Stage 21a).
///
/// Solidity call shape (96-byte input):
/// `call(gas, 0x...0c1f, calldata=(uint64 account, uint32 market_id, uint128 amount), ...) → uint256 new_collateral`
pub const PRINCEPS_LENDING_DEPOSIT_COLLATERAL: Address =
    address!("0x0000000000000000000000000000000000000c1f");

/// `princeps_lending_borrow` precompile address (Stage 21b).
///
/// Solidity call shape (160-byte input):
/// `call(gas, 0x...0c20, calldata=(uint64 account, uint32 market_id, uint128 amount, uint128 collateral_price, uint128 debt_price), ...) → uint256 success(1)/failure(0)`
pub const PRINCEPS_LENDING_BORROW: Address =
    address!("0x0000000000000000000000000000000000000c20");

/// `princeps_lending_repay` precompile address (Stage 21c).
///
/// Solidity call shape (96-byte input):
/// `call(gas, 0x...0c21, calldata=(uint64 account, uint32 market_id, uint128 amount), ...) → uint256 actual_repaid`
pub const PRINCEPS_LENDING_REPAY: Address =
    address!("0x0000000000000000000000000000000000000c21");

/// `princeps_lending_withdraw_collateral` precompile address (Stage 21d).
///
/// Solidity call shape (160-byte input):
/// `call(gas, 0x...0c22, calldata=(uint64 account, uint32 market_id, uint128 amount, uint128 collateral_price, uint128 debt_price), ...) → uint256 success(1)/failure(0)`
pub const PRINCEPS_LENDING_WITHDRAW_COLLATERAL: Address =
    address!("0x0000000000000000000000000000000000000c22");

/// `princeps_lending_liquidate` precompile address (Stage 22b).
///
/// Solidity call shape (192-byte input):
/// `call(gas, 0x...0c24, calldata=(uint64 liquidator, uint64 target, uint32 market_id, uint128 repay_amount, uint128 collateral_price, uint128 debt_price), ...) → uint256 actual_repay`
///
/// Returns the nominal debt actually repaid (low 16 bytes of u256), capped
/// at the target's outstanding debt and zero-if-rejected. Liquidator
/// pays `actual_repay` of underlying and receives
/// `actual_repay × debt_price × (1 + bonus) / collateral_price` of
/// collateral asset. Token transfers between liquidator and pool are the
/// EVM caller's responsibility — this precompile only mutates lending state.
pub const PRINCEPS_LENDING_LIQUIDATE: Address =
    address!("0x0000000000000000000000000000000000000c24");

/// `princeps_lending_health` precompile address (Stage 21e). Read-only;
/// can be invoked via `staticcall`.
///
/// Solidity call shape (128-byte input):
/// `staticcall(gas, 0x...0c23, calldata=(uint64 account, uint32 market_id, uint128 collateral_price, uint128 debt_price), ...) → uint256 health_factor_ray`
///
/// Returns `0` for no-position-or-no-market (which is indistinguishable
/// from a real HF of 0 — fully underwater. Callers needing to
/// disambiguate should check position existence separately).
/// Returns `u256::MAX` for "no debt = infinite health".
pub const PRINCEPS_LENDING_HEALTH: Address =
    address!("0x0000000000000000000000000000000000000c23");

/// `princeps_lending_supply` precompile address (v1 multi-asset / scaled_supply
/// work — follow-up to `b1b5981`).
///
/// Solidity call shape (96-byte input):
/// `call(gas, 0x...0c25, calldata=(uint64 account, uint32 market_id, uint128 amount), ...) → uint256 new_nominal_supply`
///
/// Returns the post-supply nominal balance (low 16 bytes of u256), zero on
/// error (market doesn't exist, bridge not installed, scaled-supply math
/// fails). The supplier-side mirror of
/// [`PRINCEPS_LENDING_DEPOSIT_COLLATERAL`]. Caller transfers the underlying
/// to the pool separately — this precompile only mutates lending state.
pub const PRINCEPS_LENDING_SUPPLY: Address =
    address!("0x0000000000000000000000000000000000000c25");

/// `princeps_lending_socialize` precompile address — ADR-010 Layer 3
/// EVM entrypoint. Companion to the `princeps socialize` CLI subcommand
/// (`bd5b21b`); they share the orchestration shape but this is the
/// production path that smart contracts can invoke.
///
/// Solidity call shape (192-byte input):
/// `call(gas, 0x...0c27, calldata=(uint64 operator, uint32 market_id, uint128 unfilled, uint64 block_height, bytes64 signature), ...) → uint256 absorbed`
///
/// Returns the nominal amount actually socialized (low 16 bytes of u256),
/// capped at the market's `total_supplied`. Zero on any rejection
/// (unknown / invalid signature, market_id mismatch with installed
/// state, market doesn't exist, no operator registry or chain history
/// installed, malformed input). Mutates `market.supply_index` +
/// `total_supplied`; appends a [`ChainEvent::Socialization`] to the
/// installed [`ChainHistoryStore`] at `block_height`.
pub const PRINCEPS_LENDING_SOCIALIZE: Address =
    address!("0x0000000000000000000000000000000000000c27");

/// `princeps_lending_withdraw_supply` precompile address (v1 multi-asset /
/// scaled_supply work — follow-up to `b1b5981`).
///
/// Solidity call shape (96-byte input):
/// `call(gas, 0x...0c26, calldata=(uint64 account, uint32 market_id, uint128 amount), ...) → uint256 nominal_withdrawn`
///
/// Returns the nominal amount actually withdrawn (low 16 bytes of u256),
/// capped at the supplier's current nominal_supply and zero-if-rejected.
/// Rejected when the post-withdraw `total_supplied` would fall below
/// `total_borrowed` (would push utilization above 100% and oversubscribe
/// borrowers). Mirrors the relationship between
/// [`PRINCEPS_LENDING_REPAY`] (borrower-side wind-down) and the
/// `withdraw_collateral` precompile, except the rejection ground is
/// market-utilization rather than position-health.
pub const PRINCEPS_LENDING_WITHDRAW_SUPPLY: Address =
    address!("0x0000000000000000000000000000000000000c26");

/// Base gas for lending precompiles (Stage 21). Higher than CLOB because
/// of per-position state mutation + market totals update + (for borrow/
/// withdraw/health) compute_health_factor evaluation. v0 setting; tuned
/// by testnet load profiling.
const LENDING_BASE_GAS_COST: u64 = 2_000;

/// Monotonic order-ID counter for orders placed via the EVM. Starts at 1
/// so the sentinel value 0 (returned on rejection) is distinguishable from
/// a successfully placed order.
///
/// **Single-validator caveat:** This is a process-global counter. For
/// multi-validator deployments, order IDs must come from consensus —
/// each validator's precompile must allocate the same ID for the same
/// EVM-side call, which means the counter has to be either deterministic
/// from input or read from a shared block-scoped state. Out of scope at v0.
static NEXT_ORDER_ID: AtomicU64 = AtomicU64::new(1);

/// Process-global handle to the CLOB the precompile reads from.
///
/// `None` until [`install_clob`] is called (typically by `LiveRethEvmBridge::new`).
/// While `None`, `read_best_bid` returns zero-encoded output rather than
/// erroring — this keeps existing tests deterministic and matches what an
/// uninitialised perp market would return on mainnet.
static CLOB_STATE: RwLock<Option<Arc<Mutex<Book>>>> = RwLock::new(None);

/// Install the CLOB instance the precompile should read from. The bridge
/// shares its `Arc<Mutex<Book>>` with the global so every EVM-side
/// `staticcall` to `CLOB_READ_BEST_BID` sees the same book the application
/// writes to via `submit_order`.
///
/// Calling this replaces any previously-installed CLOB. Production deployments
/// should call it exactly once at bridge construction.
pub fn install_clob(clob: Arc<Mutex<Book>>) {
    *CLOB_STATE.write().expect("CLOB_STATE rwlock poisoned") = Some(clob);
}

/// Clear the installed CLOB. Used by tests that need a clean slate; rare in
/// production. Idempotent — uninstalling when nothing is installed is a no-op.
pub fn uninstall_clob() {
    *CLOB_STATE.write().expect("CLOB_STATE rwlock poisoned") = None;
}

/// Process-global handle to the buffer where the precompile pushes fills.
///
/// Same lifecycle rules as `CLOB_STATE`: installed by `LiveRethEvmBridge::new`,
/// none until set. When set, `place_order` extends this buffer with any fills
/// produced by the matched order, so production-shape EVM-placed orders flow
/// into the next `build_payload`'s drained fills exactly like bridge-side
/// `submit_order` does.
static FILL_SINK: RwLock<Option<Arc<Mutex<Vec<Fill>>>>> = RwLock::new(None);

/// Install the `pending_fills` buffer the precompile should write to.
/// Companion to `install_clob`. Calling this replaces any previously-installed
/// sink.
pub fn install_fill_sink(sink: Arc<Mutex<Vec<Fill>>>) {
    *FILL_SINK.write().expect("FILL_SINK rwlock poisoned") = Some(sink);
}

/// Clear the installed fill sink. Test-only typical use; idempotent.
pub fn uninstall_fill_sink() {
    *FILL_SINK.write().expect("FILL_SINK rwlock poisoned") = None;
}

/// Process-global handle to the bridge's per-account state map (Stage
/// 17c). When installed, the deposit precompile mutates this same map
/// that `LiveRethEvmBridge::deposit` / `submit_order` write to, so an
/// EVM-side deposit and a Rust-side bridge deposit are equivalent
/// state changes.
static ACCOUNTS_STATE: RwLock<Option<Arc<Mutex<HashMap<AccountId, Account>>>>> =
    RwLock::new(None);

/// Install the account map the deposit precompile should mutate.
/// Companion to `install_clob` / `install_fill_sink`; same lifecycle.
pub fn install_accounts(accounts: Arc<Mutex<HashMap<AccountId, Account>>>) {
    *ACCOUNTS_STATE.write().expect("ACCOUNTS_STATE rwlock poisoned") = Some(accounts);
}

/// Clear the installed account map. Test-only typical use; idempotent.
pub fn uninstall_accounts() {
    *ACCOUNTS_STATE.write().expect("ACCOUNTS_STATE rwlock poisoned") = None;
}

/// Process-global handle to the bridge's lending markets map (Stage 21).
/// When installed, all 5 lending precompiles mutate / read this same map
/// that `LiveRethEvmBridge`'s `lending_*` methods touch. Same
/// shared-Arc lifecycle as `ACCOUNTS_STATE`.
static MARKETS_STATE: RwLock<Option<Arc<Mutex<BTreeMap<MarketId, Market>>>>> = RwLock::new(None);

/// Install the lending markets map the precompiles should mutate.
/// Companion to `install_accounts`; called by `LiveRethEvmBridge::new`.
pub fn install_lending_markets(markets: Arc<Mutex<BTreeMap<MarketId, Market>>>) {
    *MARKETS_STATE.write().expect("MARKETS_STATE rwlock poisoned") = Some(markets);
}

/// Clear the installed markets map. Test-only typical use; idempotent.
pub fn uninstall_lending_markets() {
    *MARKETS_STATE.write().expect("MARKETS_STATE rwlock poisoned") = None;
}

/// Process-global handle to the bridge's lending positions map (Stage 21).
/// Same shared-Arc lifecycle as `MARKETS_STATE`.
static POSITIONS_STATE: RwLock<
    Option<Arc<Mutex<BTreeMap<(AccountId, MarketId), Position>>>>,
> = RwLock::new(None);

/// Install the lending positions map the precompiles should mutate.
pub fn install_lending_positions(
    positions: Arc<Mutex<BTreeMap<(AccountId, MarketId), Position>>>,
) {
    *POSITIONS_STATE.write().expect("POSITIONS_STATE rwlock poisoned") = Some(positions);
}

/// Clear the installed positions map. Test-only typical use; idempotent.
pub fn uninstall_lending_positions() {
    *POSITIONS_STATE.write().expect("POSITIONS_STATE rwlock poisoned") = None;
}

/// Process-global handle to the ADR-010 Layer 3 operator-key registry.
/// Read-only after install — the registry is configured at boot from
/// deployment config. Wrapped in `Arc` for cheap clone-into-handler;
/// no outer mutex because no in-place mutation happens past install.
static OPERATOR_REGISTRY: RwLock<Option<Arc<OperatorRegistry>>> = RwLock::new(None);

/// Install the operator-key registry the socialize precompile
/// consults during verify. Replaces any prior registry; idempotent
/// for the same value.
pub fn install_operator_registry(registry: Arc<OperatorRegistry>) {
    *OPERATOR_REGISTRY
        .write()
        .expect("OPERATOR_REGISTRY rwlock poisoned") = Some(registry);
}

/// Clear the installed registry. Test-only typical use; idempotent.
pub fn uninstall_operator_registry() {
    *OPERATOR_REGISTRY
        .write()
        .expect("OPERATOR_REGISTRY rwlock poisoned") = None;
}

/// Process-global handle to the chain-history event log. The socialize
/// precompile appends a [`ChainEvent::Socialization`] to it after a
/// successful haircut. Other future event producers (forthcoming) can
/// share the same store. Internally Mutex-guarded, so the outer
/// wrapper is just an install/uninstall lifecycle handle.
static CHAIN_HISTORY: RwLock<Option<Arc<ChainHistoryStore>>> = RwLock::new(None);

/// Install the chain-history store. Same shared-Arc lifecycle as
/// `OPERATOR_REGISTRY`.
pub fn install_chain_history(store: Arc<ChainHistoryStore>) {
    *CHAIN_HISTORY
        .write()
        .expect("CHAIN_HISTORY rwlock poisoned") = Some(store);
}

/// Clear the installed chain-history store. Test-only typical use;
/// idempotent.
pub fn uninstall_chain_history() {
    *CHAIN_HISTORY
        .write()
        .expect("CHAIN_HISTORY rwlock poisoned") = None;
}

/// Read the currently-installed CLOB's best bid. Returns `None` if no CLOB
/// is installed or if the book has no bids. Public so tests can verify
/// install/uninstall without going through the precompile dispatch.
#[must_use]
pub fn current_best_bid() -> Option<(rdk_clob::Price, rdk_clob::Qty)> {
    let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
    let clob = state.as_ref()?;
    let book = clob.lock().expect("clob mutex poisoned");
    book.best_bid_with_qty()
}

/// Stage 17j — read the currently-installed CLOB's midpoint as a
/// [`rdk_funding::MarkPrice`]. Returns `None` when no CLOB is
/// installed or either side of the book is empty; that's the signal
/// the withdraw precompile uses to fall back to the avg-entry IM
/// rule. Mirror of [`crate::live_node::LiveRethEvmBridge::current_mark`]
/// so the EVM-side check and the bridge's Rust-side check stay in
/// lockstep.
#[must_use]
pub fn current_mark() -> Option<rdk_funding::MarkPrice> {
    let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
    let clob = state.as_ref()?;
    let book = clob.lock().expect("clob mutex poisoned");
    let bid = book.best_bid()?;
    let ask = book.best_ask()?;
    Some(rdk_funding::MarkPrice((bid.0 + ask.0) / 2))
}

/// Stage 17i: in-memory snapshot of every mutating bridge global —
/// `{accounts, book, pending_fills}`. Used by [`revert_guard`] to
/// roll back precompile mutations when the calling EVM frame
/// reverts.
///
/// All three fields are `Option` so a snapshot is meaningful even
/// before any of the globals are installed (e.g., the read-only
/// CLOB precompile under tests that haven't wired in the account
/// map).
#[derive(Debug, Default)]
pub(crate) struct BridgeStateSnapshot {
    accounts: Option<HashMap<AccountId, Account>>,
    book: Option<Book>,
    fills: Option<Vec<Fill>>,
    /// Lending markets at snapshot time (lending revert-guard extension).
    /// All six lending precompiles (0x0c1f–0x0c24) mutate this state, so
    /// it must be journaled alongside accounts/book/fills to keep the
    /// EVM's view consistent on revert.
    markets: Option<BTreeMap<MarketId, Market>>,
    /// Lending positions at snapshot time (lending revert-guard extension).
    positions: Option<BTreeMap<(AccountId, MarketId), Position>>,
}

/// Clone the contents of every currently-installed mutating global.
/// Cheap for v0 (5-account dev state); a production rewrite would
/// move to per-mutation journal entries instead of whole-state
/// clones, mirroring REVM's storage journal.
#[must_use]
pub(crate) fn snapshot_bridge_state() -> BridgeStateSnapshot {
    let accounts = {
        let state = ACCOUNTS_STATE
            .read()
            .expect("ACCOUNTS_STATE rwlock poisoned");
        state
            .as_ref()
            .map(|a| a.lock().expect("accounts mutex poisoned").clone())
    };
    let book = {
        let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
        state
            .as_ref()
            .map(|c| c.lock().expect("clob mutex poisoned").clone())
    };
    let fills = {
        let state = FILL_SINK.read().expect("FILL_SINK rwlock poisoned");
        state
            .as_ref()
            .map(|f| f.lock().expect("fill_sink mutex poisoned").clone())
    };
    let markets = {
        let state = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
        state
            .as_ref()
            .map(|m| m.lock().expect("markets mutex poisoned").clone())
    };
    let positions = {
        let state = POSITIONS_STATE
            .read()
            .expect("POSITIONS_STATE rwlock poisoned");
        state
            .as_ref()
            .map(|p| p.lock().expect("positions mutex poisoned").clone())
    };
    BridgeStateSnapshot {
        accounts,
        book,
        fills,
        markets,
        positions,
    }
}

/// Overwrite the contents of every installed mutating global with
/// the values captured by [`snapshot_bridge_state`]. Preserves the
/// `Arc` identity (so consumers holding a clone of the Arc see the
/// restored state); only the data behind the lock is replaced.
///
/// `None` fields are skipped — if a global wasn't installed at
/// snapshot time, this leaves whatever's currently installed in
/// place.
pub(crate) fn restore_bridge_state(snap: BridgeStateSnapshot) {
    if let Some(snap_accounts) = snap.accounts {
        let state = ACCOUNTS_STATE
            .read()
            .expect("ACCOUNTS_STATE rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("accounts mutex poisoned") = snap_accounts;
        }
    }
    if let Some(snap_book) = snap.book {
        let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("clob mutex poisoned") = snap_book;
        }
    }
    if let Some(snap_fills) = snap.fills {
        let state = FILL_SINK.read().expect("FILL_SINK rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("fill_sink mutex poisoned") = snap_fills;
        }
    }
    if let Some(snap_markets) = snap.markets {
        let state = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("markets mutex poisoned") = snap_markets;
        }
    }
    if let Some(snap_positions) = snap.positions {
        let state = POSITIONS_STATE
            .read()
            .expect("POSITIONS_STATE rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("positions mutex poisoned") = snap_positions;
        }
    }
}

/// Reads the best bid (highest-priced buy order's price + total qty at that
/// level) from the currently-installed CLOB and returns it as two
/// big-endian u256s (64 bytes total).
///
/// Encoding:
///   bytes  0..32  big-endian u256 price (0 if no bid or no CLOB installed)
///   bytes 32..64  big-endian u256 qty   (0 if no bid or no CLOB installed)
///
/// `PrecompileFn` signature is `fn(&[u8], u64, u64) -> PrecompileResult`;
/// the third arg is a `reservoir` value (extra gas budget) that we ignore
/// at v0. The Result wrapper is required by the signature even though we
/// never error — gas accounting is the EVM's responsibility.
#[allow(clippy::unnecessary_wraps)]
fn read_best_bid(_input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 64];

    if let Some((price, qty)) = current_best_bid() {
        // Big-endian u256: rightmost bytes carry the value.
        out[24..32].copy_from_slice(&price.0.to_be_bytes());
        out[56..64].copy_from_slice(&qty.0.to_be_bytes());
    }
    // If no CLOB is installed or there are no bids, `out` stays all zeros —
    // matches what an uninitialised perp market would return on mainnet.

    Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0))
}

/// Place a limit order on the installed CLOB. The write counterpart to
/// `read_best_bid` — completes the EVM ↔ CLOB bidirectional surface.
///
/// Calldata layout (ABI-aligned, 128 bytes):
/// ```text
///   [  0.. 32]  account_id  (u64 in last 8 bytes)
///   [ 32.. 64]  side        (u8 in last byte: 0 = Buy, 1 = Sell)
///   [ 64.. 96]  price       (u64 in last 8 bytes)
///   [ 96..128]  qty         (u64 in last 8 bytes)
/// ```
///
/// Returns 32 bytes: the allocated `order_id` in the last 8 bytes, or zero
/// on rejection (no CLOB installed, malformed input, invalid side byte).
/// Allocated IDs start at 1, so zero is unambiguously "rejected".
///
/// Stage 9c+ (this commit): any fills produced by the submit are pushed into
/// the `FILL_SINK` global if installed. This is what makes EVM-placed orders
/// flow into the bridge's `pending_fills` and out via `build_payload`,
/// matching the bridge-side `submit_order` semantics. If no sink is
/// installed the fills are still produced (visible via subsequent
/// `read_best_bid`) but won't reach a payload.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn place_order(input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 32];

    // Need exactly 128 bytes of input (4 × ABI-padded fields).
    if input.len() < 128 {
        return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0));
    }

    let account_id = u64_from_be_chunk(&input[0..32]);
    let side_byte = input[63];
    let price_value = u64_from_be_chunk(&input[64..96]);
    let qty_value = u64_from_be_chunk(&input[96..128]);

    let side = match side_byte {
        0 => Side::Buy,
        1 => Side::Sell,
        _ => return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0)),
    };

    // Reject orders with zero quantity outright — the book accepts them
    // technically, but a zero-qty order is always a bug from the caller.
    if qty_value == 0 {
        return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0));
    }

    let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
    let Some(clob) = state.as_ref() else {
        // No CLOB installed → 0 sentinel.
        return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0));
    };

    let order_id_val = NEXT_ORDER_ID.fetch_add(1, Ordering::Relaxed);

    let mut book = clob.lock().expect("clob mutex poisoned");
    let submit_result = book.submit(Order {
        id: OrderId(order_id_val),
        account: AccountId(account_id),
        side,
        qty: Qty(qty_value),
        order_type: OrderType::Limit {
            price: Price(price_value),
        },
    });
    drop(book);

    // Stage 9c+: route any fills produced by this order through the bridge's
    // pending_fills buffer so they reach the next `build_payload`. Drops
    // silently if no sink is installed (consistent with no-CLOB → return 0).
    if !submit_result.fills.is_empty() {
        let sink_state = FILL_SINK.read().expect("FILL_SINK rwlock poisoned");
        if let Some(sink) = sink_state.as_ref() {
            sink.lock()
                .expect("fill_sink mutex poisoned")
                .extend(submit_result.fills.iter().copied());
        }
    }

    out[24..32].copy_from_slice(&order_id_val.to_be_bytes());
    Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0))
}

/// Read a big-endian u64 from the last 8 bytes of a 32-byte ABI chunk.
fn u64_from_be_chunk(chunk: &[u8]) -> u64 {
    debug_assert!(chunk.len() == 32);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&chunk[24..32]);
    u64::from_be_bytes(buf)
}

/// Read a big-endian signed i64 from a 32-byte ABI chunk. Solidity's
/// `int256` encoding is sign-extended to 32 bytes; we take the
/// upper 24 bytes as the sign-extension and the last 8 bytes as the
/// magnitude. Values outside `i64` range saturate.
fn i64_from_be_chunk(chunk: &[u8]) -> i64 {
    debug_assert!(chunk.len() == 32);
    // Sign-extension check: bytes 0..24 must all match the sign bit
    // of byte 24 for the value to fit in i64. If they don't, we
    // saturate to i64::MIN or i64::MAX.
    let sign_byte = chunk[24];
    let sign_ext = if sign_byte & 0x80 != 0 { 0xff } else { 0x00 };
    if chunk[..24].iter().any(|&b| b != sign_ext) {
        return if sign_byte & 0x80 != 0 { i64::MIN } else { i64::MAX };
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&chunk[24..32]);
    i64::from_be_bytes(buf)
}

/// Deposit collateral on behalf of an account (Stage 17c).
///
/// Calldata (64 bytes):
///   bytes  0..32  account_id (last 8 bytes are the u64; upper bytes ignored)
///   bytes 32..64  amount     (full 32-byte sign-extended int256)
///
/// Returns 32 bytes: the post-deposit collateral as a big-endian
/// `int256` (sign-extended). Returns all zeros when rejected (no
/// account map installed, or input shorter than 64 bytes).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn deposit(input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 32];

    if input.len() < 64 {
        return Ok(PrecompileOutput::new(
            DEPOSIT_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    }

    let account_id = u64_from_be_chunk(&input[0..32]);
    let amount = i64_from_be_chunk(&input[32..64]);

    let state = ACCOUNTS_STATE
        .read()
        .expect("ACCOUNTS_STATE rwlock poisoned");
    let Some(accounts) = state.as_ref() else {
        return Ok(PrecompileOutput::new(
            DEPOSIT_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };

    let mut map = accounts.lock().expect("accounts mutex poisoned");
    let acct = map
        .entry(AccountId(account_id))
        .or_insert_with(|| Account::flat(AccountId(account_id)));
    acct.collateral = Notional(acct.collateral.0.saturating_add(amount));
    let new_balance = acct.collateral.0;
    drop(map);
    drop(state);

    // Encode i64 → 32-byte sign-extended big-endian.
    let sign_ext: u8 = if new_balance < 0 { 0xff } else { 0x00 };
    for b in &mut out[..24] {
        *b = sign_ext;
    }
    out[24..32].copy_from_slice(&new_balance.to_be_bytes());
    Ok(PrecompileOutput::new(
        DEPOSIT_BASE_GAS_COST,
        Bytes::from(out),
        0,
    ))
}

/// Withdraw collateral from an account (Stage 17e). Companion to
/// [`deposit`].
///
/// Calldata (64 bytes):
///   bytes  0..32  account_id (last 8 bytes are the u64)
///   bytes 32..64  amount     (last 8 bytes are the u64; upper bytes ignored)
///
/// Returns 32 bytes: the post-withdraw collateral as a big-endian
/// sign-extended int256 on success. Returns all zeros when
/// rejected (no map installed, account doesn't exist, insufficient
/// balance, input shorter than 64 bytes). Note: a successful
/// withdraw that drains to exactly 0 also returns 0 — callers
/// distinguishing success from rejection should read the
/// pre-call balance separately.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn withdraw(input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 32];

    if input.len() < 64 {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    }

    let account_id = u64_from_be_chunk(&input[0..32]);
    let amount = u64_from_be_chunk(&input[32..64]);

    let state = ACCOUNTS_STATE
        .read()
        .expect("ACCOUNTS_STATE rwlock poisoned");
    let Some(accounts) = state.as_ref() else {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };

    let mut map = accounts.lock().expect("accounts mutex poisoned");
    let Some(acct) = map.get_mut(&AccountId(account_id)) else {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };
    let Ok(amount_i64) = i64::try_from(amount) else {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };
    // Stage 17j: mark-aware free collateral. With a two-sided CLOB
    // book, use the midpoint as mark and the production-shape
    // `(equity − IM_req_at_mark)` rule. Without a book midpoint, the
    // helper falls back to the Stage 17g avg-entry rule. For a flat
    // position both reduce to a raw-collateral check.
    let free = crate::live_node::withdraw_free_collateral(acct, current_mark());
    if i128::from(amount_i64) > i128::from(free) {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    }
    acct.collateral = Notional(acct.collateral.0 - amount_i64);
    let new_balance = acct.collateral.0;
    drop(map);
    drop(state);

    let sign_ext: u8 = if new_balance < 0 { 0xff } else { 0x00 };
    for b in &mut out[..24] {
        *b = sign_ext;
    }
    out[24..32].copy_from_slice(&new_balance.to_be_bytes());
    Ok(PrecompileOutput::new(
        WITHDRAW_BASE_GAS_COST,
        Bytes::from(out),
        0,
    ))
}

/// Read a big-endian u32 from the last 4 bytes of a 32-byte ABI chunk.
fn u32_from_be_chunk(chunk: &[u8]) -> u32 {
    debug_assert!(chunk.len() == 32);
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&chunk[28..32]);
    u32::from_be_bytes(buf)
}

/// Read a big-endian u128 from the last 16 bytes of a 32-byte ABI chunk.
fn u128_from_be_chunk(chunk: &[u8]) -> u128 {
    debug_assert!(chunk.len() == 32);
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&chunk[16..32]);
    u128::from_be_bytes(buf)
}

/// Encode a u128 as a 32-byte big-endian ABI word (zero-padded upper 16 bytes).
fn u128_to_abi_word(v: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[16..32].copy_from_slice(&v.to_be_bytes());
    out
}

/// Encode a u256 = u128::MAX << 128 | u128::MAX as 32-byte big-endian — for
/// the "infinite health" signal from `lending_health`.
fn u256_max_word() -> [u8; 32] {
    [0xff; 32]
}

/// Encode a u256 ABI word from a u128 (zero upper).
fn u128_in_low_word(v: u128) -> Vec<u8> {
    u128_to_abi_word(v).to_vec()
}

// ===== Stage 21 lending precompile handlers =====
//
// All 5 follow the same pattern:
// 1. Validate input length; short-input → return zero output.
// 2. Decode ABI chunks (account_id, market_id, amount, optional prices).
// 3. Read installed markets + positions globals; missing → return zero.
// 4. Lock both maps in markets-then-positions order (matches bridge convention).
// 5. Do the mutation (deposit/borrow/repay/withdraw) or read (health).
// 6. Encode result into 32-byte output.
//
// Errors return all-zero output. Callers distinguish success/failure
// by comparing pre-call and post-call state (same convention as the
// existing deposit/withdraw precompiles).

/// `princeps_lending_deposit_collateral` precompile handler (Stage 21a).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_deposit(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 96 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let account_id = u64_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let amount = u128_from_be_chunk(&input[64..96]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let markets_guard = markets.lock().expect("markets mutex poisoned");
    if !markets_guard.contains_key(&MarketId(market_id)) {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    drop(markets_guard);

    let mut positions_guard = positions.lock().expect("positions mutex poisoned");
    let key = (AccountId(account_id), MarketId(market_id));
    let position = positions_guard
        .entry(key)
        .or_insert_with(|| Position::empty(MarketId(market_id)));
    if lending_position_deposit_collateral(position, amount).is_err() {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let new_collateral = position.collateral_amount;
    drop(positions_guard);

    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(new_collateral)),
        0,
    ))
}

/// `princeps_lending_borrow` precompile handler (Stage 21b).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_borrow(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 160 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let account_id = u64_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let amount = u128_from_be_chunk(&input[64..96]);
    let collateral_price = u128_from_be_chunk(&input[96..128]);
    let debt_price = u128_from_be_chunk(&input[128..160]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let mut markets_guard = markets.lock().expect("markets mutex poisoned");
    let Some(market) = markets_guard.get_mut(&MarketId(market_id)) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let Some(new_borrowed) = market.total_borrowed.checked_add(amount) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    if new_borrowed > market.total_supplied {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }

    let mut positions_guard = positions.lock().expect("positions mutex poisoned");
    let key = (AccountId(account_id), MarketId(market_id));
    let existing = positions_guard
        .get(&key)
        .cloned()
        .unwrap_or_else(|| Position::empty(MarketId(market_id)));

    let mut hypothetical = existing;
    if lending_position_borrow(&mut hypothetical, amount, market.borrow_index).is_err() {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let hf = lending_compute_health_factor(&hypothetical, market, collateral_price, debt_price);
    if hf < LendingIndex::RAY {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    positions_guard.insert(key, hypothetical);
    market.total_borrowed = new_borrowed;

    // success = 1
    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(1)),
        0,
    ))
}

/// `princeps_lending_repay` precompile handler (Stage 21c).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_repay(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 96 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let account_id = u64_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let amount = u128_from_be_chunk(&input[64..96]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let mut markets_guard = markets.lock().expect("markets mutex poisoned");
    let Some(market) = markets_guard.get_mut(&MarketId(market_id)) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let mut positions_guard = positions.lock().expect("positions mutex poisoned");
    let key = (AccountId(account_id), MarketId(market_id));
    let Some(position) = positions_guard.get_mut(&key) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let Ok(actual_repaid) = lending_position_repay(position, amount, market.borrow_index) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    market.total_borrowed = market.total_borrowed.saturating_sub(actual_repaid);

    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(actual_repaid)),
        0,
    ))
}

/// `princeps_lending_withdraw_collateral` precompile handler (Stage 21d).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_withdraw(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 160 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let account_id = u64_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let amount = u128_from_be_chunk(&input[64..96]);
    let collateral_price = u128_from_be_chunk(&input[96..128]);
    let debt_price = u128_from_be_chunk(&input[128..160]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let markets_guard = markets.lock().expect("markets mutex poisoned");
    let Some(market) = markets_guard.get(&MarketId(market_id)).cloned() else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    drop(markets_guard);

    let mut positions_guard = positions.lock().expect("positions mutex poisoned");
    let key = (AccountId(account_id), MarketId(market_id));
    let Some(existing) = positions_guard.get(&key).cloned() else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let mut hypothetical = existing;
    if lending_position_withdraw_collateral(&mut hypothetical, amount).is_err() {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let hf = lending_compute_health_factor(&hypothetical, &market, collateral_price, debt_price);
    if hf < LendingIndex::RAY {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    positions_guard.insert(key, hypothetical);

    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(1)),
        0,
    ))
}

/// `princeps_lending_supply` precompile handler (supplier-side mirror of
/// [`lending_deposit`]).
///
/// Adds `amount` of nominal supply to the caller's `scaled_supply` position
/// in the named market. The underlying token transfer (caller → pool) is
/// the EVM caller's responsibility — this precompile only mutates lending
/// state. The bridge-owned implicit pool continues to provide initial
/// liquidity in parallel (`b1b5981`'s additive model).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_supply(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 96 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let account_id = u64_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let amount = u128_from_be_chunk(&input[64..96]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let mut markets_guard = markets.lock().expect("markets mutex poisoned");
    let Some(market) = markets_guard.get_mut(&MarketId(market_id)) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let Some(new_supplied) = market.total_supplied.checked_add(amount) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let supply_index = market.supply_index;

    let mut positions_guard = positions.lock().expect("positions mutex poisoned");
    let key = (AccountId(account_id), MarketId(market_id));
    let position = positions_guard
        .entry(key)
        .or_insert_with(|| Position::empty(MarketId(market_id)));
    if lending_position_supply(position, amount, supply_index).is_err() {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let new_nominal = position.nominal_supply(supply_index);
    market.total_supplied = new_supplied;

    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(new_nominal)),
        0,
    ))
}

/// `princeps_lending_withdraw_supply` precompile handler (supplier-side
/// mirror of [`lending_repay`]).
///
/// Withdraws up to `amount` of nominal supply from the caller's
/// `scaled_supply` position. The actual withdrawal is capped at the
/// caller's current `nominal_supply(supply_index)`. The withdrawal is
/// rejected entirely (returns zero) when the post-withdraw
/// `total_supplied` would fall below `total_borrowed` — that would push
/// utilization above 100% and oversubscribe borrowers, which is the
/// supplier-side analog to a position falling below the health-factor
/// threshold on the borrower side.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_withdraw_supply(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 96 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let account_id = u64_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let amount = u128_from_be_chunk(&input[64..96]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let mut markets_guard = markets.lock().expect("markets mutex poisoned");
    let Some(market) = markets_guard.get_mut(&MarketId(market_id)) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let supply_index = market.supply_index;
    let total_borrowed = market.total_borrowed;
    let total_supplied = market.total_supplied;

    let mut positions_guard = positions.lock().expect("positions mutex poisoned");
    let key = (AccountId(account_id), MarketId(market_id));
    let Some(position) = positions_guard.get_mut(&key) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    // Hypothetically withdraw — withdraw_supply caps at the supplier's
    // current nominal balance internally, so we read back the actual
    // amount it consumed.
    let mut hypothetical = position.clone();
    let Ok(actual_withdrawn) =
        lending_position_withdraw_supply(&mut hypothetical, amount, supply_index)
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    // Utilization safety gate: refuse withdrawals that would push
    // total_supplied below total_borrowed. With the additive
    // bridge-implicit-pool model from b1b5981, this protects borrowers
    // from a supplier exodus that strands the pool.
    let new_total_supplied = total_supplied.saturating_sub(actual_withdrawn);
    if new_total_supplied < total_borrowed {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }

    *position = hypothetical;
    market.total_supplied = new_total_supplied;

    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(actual_withdrawn)),
        0,
    ))
}

/// `princeps_lending_socialize` precompile handler — ADR-010 Layer 3
/// EVM entrypoint.
///
/// Wires the three primitives that landed earlier:
///   1. `verify_socialization_declaration` against the installed
///      [`OperatorRegistry`].
///   2. `socialize_residual` on the named market in [`MARKETS_STATE`].
///   3. `append_event(block_height, ChainEvent::Socialization{..})`
///      on the installed [`ChainHistoryStore`].
///
/// Input layout (192 bytes, big-endian throughout):
///   [  0.. 32]  operator      (u32 in low 4 bytes of chunk)
///   [ 32.. 64]  market_id     (u32)
///   [ 64.. 96]  unfilled      (u128 in low 16)
///   [ 96..128]  block_height  (u64 in low 8)
///   [128..192]  signature     (64 raw bytes, r || s)
///
/// Returns 32 bytes — `absorbed` (u128 in low word) on success, zero
/// on any rejection. Matches the existing precompile convention.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_socialize(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_result = || {
        Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(vec![0u8; 32]),
            0,
        ))
    };

    if input.len() < 192 {
        return zero_result();
    }

    // Parse the declaration fields.
    let operator = u32_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let unfilled = u128_from_be_chunk(&input[64..96]);
    let block_height = u64_from_be_chunk(&input[96..128]);
    let mut sig_bytes = [0u8; 64];
    sig_bytes.copy_from_slice(&input[128..192]);
    let declaration = SocializationDeclaration {
        operator: OperatorId(operator),
        market_id,
        unfilled,
        block_height,
        signature: OperatorSignature(sig_bytes),
    };

    // Pull all four globals up-front. Drop the read handles before
    // taking any write locks downstream.
    let registry = {
        let handle = OPERATOR_REGISTRY
            .read()
            .expect("OPERATOR_REGISTRY rwlock poisoned");
        handle.as_ref().map(Arc::clone)
    };
    let Some(registry) = registry else {
        return zero_result();
    };

    let chain_history = {
        let handle = CHAIN_HISTORY
            .read()
            .expect("CHAIN_HISTORY rwlock poisoned");
        handle.as_ref().map(Arc::clone)
    };
    let Some(chain_history) = chain_history else {
        return zero_result();
    };

    let markets = {
        let handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
        handle.as_ref().map(Arc::clone)
    };
    let Some(markets) = markets else {
        return zero_result();
    };

    // Verify operator-sig admission. Reject = zero.
    if verify_socialization_declaration(&declaration, &registry).is_err() {
        return zero_result();
    }

    // Apply the haircut.
    let absorbed = {
        let mut markets_guard = markets.lock().expect("markets mutex poisoned");
        let Some(market) = markets_guard.get_mut(&MarketId(market_id)) else {
            // Market doesn't exist on this chain.
            drop(markets_guard);
            return zero_result();
        };
        if market.id.0 != market_id {
            // Defensive — shouldn't happen since the key is the same id.
            drop(markets_guard);
            return zero_result();
        }
        let Ok(report) = lending_socialize_residual(market, unfilled) else {
            // ZeroUnfilled or NoSupply.
            drop(markets_guard);
            return zero_result();
        };
        report.absorbed
    };

    // Append the audit event. Use `absorbed` (the actual post-cap
    // value), not the requested `unfilled` — the on-chain audit
    // trail records what happened, not what was asked for.
    chain_history.append_event(
        block_height,
        ChainEvent::Socialization {
            market_id,
            unfilled: absorbed,
            declared_by: format!("operator-{operator}"),
        },
    );

    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(absorbed)),
        0,
    ))
}

/// `princeps_lending_liquidate` precompile handler (Stage 22b).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_liquidate(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 192 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let _liquidator_id = u64_from_be_chunk(&input[0..32]);
    let target_id = u64_from_be_chunk(&input[32..64]);
    let market_id = u32_from_be_chunk(&input[64..96]);
    let repay_amount = u128_from_be_chunk(&input[96..128]);
    let collateral_price = u128_from_be_chunk(&input[128..160]);
    let debt_price = u128_from_be_chunk(&input[160..192]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let mut markets_guard = markets.lock().expect("markets mutex poisoned");
    let Some(market) = markets_guard.get_mut(&MarketId(market_id)) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let market_snapshot = market.clone();

    let mut positions_guard = positions.lock().expect("positions mutex poisoned");
    let target_key = (AccountId(target_id), MarketId(market_id));
    let Some(target_position) = positions_guard.get(&target_key).cloned() else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    // Health check
    let hf = lending_compute_health_factor(
        &target_position,
        &market_snapshot,
        collateral_price,
        debt_price,
    );
    if hf >= LendingIndex::RAY {
        // Healthy → reject
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }

    let nominal_debt = target_position.nominal_debt(market_snapshot.borrow_index);
    let actual_repay = repay_amount.min(nominal_debt);
    if actual_repay == 0 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }

    // Seize collateral with bonus
    let bonus_bps = u128::from(market_snapshot.liquidation_bonus.0);
    let quote_value = actual_repay.saturating_mul(debt_price);
    let bonus_value = quote_value.saturating_mul(bonus_bps) / 10_000;
    let total_quote = quote_value.saturating_add(bonus_value);
    let seized_collateral = if collateral_price == 0 {
        0
    } else {
        total_quote / collateral_price
    };
    let actual_seized = seized_collateral.min(target_position.collateral_amount);

    // Apply
    let mut target_after = target_position;
    if lending_position_repay(&mut target_after, actual_repay, market_snapshot.borrow_index)
        .is_err()
    {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    target_after.collateral_amount = target_after.collateral_amount.saturating_sub(actual_seized);

    positions_guard.insert(target_key, target_after);
    market.total_borrowed = market.total_borrowed.saturating_sub(actual_repay);

    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(u128_in_low_word(actual_repay)),
        0,
    ))
}

/// `princeps_lending_health` precompile handler (Stage 21e). Read-only.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn lending_health(
    input: &[u8],
    _gas_limit: u64,
    _reservoir: u64,
) -> PrecompileResult {
    let zero_out = vec![0u8; 32];
    if input.len() < 128 {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    }
    let account_id = u64_from_be_chunk(&input[0..32]);
    let market_id = u32_from_be_chunk(&input[32..64]);
    let collateral_price = u128_from_be_chunk(&input[64..96]);
    let debt_price = u128_from_be_chunk(&input[96..128]);

    let markets_handle = MARKETS_STATE.read().expect("MARKETS_STATE rwlock poisoned");
    let positions_handle = POSITIONS_STATE
        .read()
        .expect("POSITIONS_STATE rwlock poisoned");
    let (Some(markets), Some(positions)) = (markets_handle.as_ref(), positions_handle.as_ref())
    else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };

    let markets_guard = markets.lock().expect("markets mutex poisoned");
    let Some(market) = markets_guard.get(&MarketId(market_id)) else {
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(zero_out),
            0,
        ));
    };
    let positions_guard = positions.lock().expect("positions mutex poisoned");
    let key = (AccountId(account_id), MarketId(market_id));
    let Some(position) = positions_guard.get(&key) else {
        // No position → encode as "infinite health" (matches no-debt semantic)
        return Ok(PrecompileOutput::new(
            LENDING_BASE_GAS_COST,
            Bytes::from(u256_max_word().to_vec()),
            0,
        ));
    };
    let hf = lending_compute_health_factor(position, market, collateral_price, debt_price);

    // Encode HF into low 16 bytes of 32-byte word. RAY fits in u128.
    // For HF = u128::MAX (infinite), use u256::MAX for unambiguous signal.
    let out = if hf == u128::MAX {
        u256_max_word().to_vec()
    } else {
        u128_in_low_word(hf)
    };
    Ok(PrecompileOutput::new(
        LENDING_BASE_GAS_COST,
        Bytes::from(out),
        0,
    ))
}

/// Build a `Precompiles` set that extends Reth's standard precompiles with
/// princeps's CLOB-reading + CLOB-writing additions. The base set is parameterized
/// over the hardfork's spec id so we inherit Ethereum's evolution (e.g., the
/// BLS-12-381 precompiles activated in Prague).
#[must_use]
pub fn princeps_precompiles(base: &Precompiles) -> Precompiles {
    let mut precompiles = base.clone();
    precompiles.extend([
        Precompile::new(
            PrecompileId::custom("clob_read_best_bid"),
            CLOB_READ_BEST_BID,
            read_best_bid,
        ),
        Precompile::new(
            PrecompileId::custom("clob_place_order"),
            CLOB_PLACE_ORDER,
            place_order,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_deposit"),
            PRINCEPS_DEPOSIT,
            deposit,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_withdraw"),
            PRINCEPS_WITHDRAW,
            withdraw,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_deposit_collateral"),
            PRINCEPS_LENDING_DEPOSIT_COLLATERAL,
            lending_deposit,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_borrow"),
            PRINCEPS_LENDING_BORROW,
            lending_borrow,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_repay"),
            PRINCEPS_LENDING_REPAY,
            lending_repay,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_withdraw_collateral"),
            PRINCEPS_LENDING_WITHDRAW_COLLATERAL,
            lending_withdraw,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_liquidate"),
            PRINCEPS_LENDING_LIQUIDATE,
            lending_liquidate,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_health"),
            PRINCEPS_LENDING_HEALTH,
            lending_health,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_supply"),
            PRINCEPS_LENDING_SUPPLY,
            lending_supply,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_withdraw_supply"),
            PRINCEPS_LENDING_WITHDRAW_SUPPLY,
            lending_withdraw_supply,
        ),
        Precompile::new(
            PrecompileId::custom("princeps_lending_socialize"),
            PRINCEPS_LENDING_SOCIALIZE,
            lending_socialize,
        ),
    ]);
    precompiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use rdk_clob::{AccountId, Order, OrderId, OrderType, Price, Qty, Side};

    /// Tests in this module touch process-global `CLOB_STATE`. This mutex
    /// serializes them so parallel test execution can't observe a torn state.
    static TEST_SERIALIZER: Mutex<()> = Mutex::new(());

    /// With no CLOB installed, the precompile returns 64 zero bytes —
    /// matching what an uninitialised perp market would report on mainnet.
    #[test]
    fn read_best_bid_returns_zero_when_no_clob_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_clob();

        let result = read_best_bid(&[], 100_000, 0).expect("precompile must not error");
        assert_eq!(result.bytes.len(), 64);
        let price = U256::from_be_slice(&result.bytes[0..32]);
        let qty = U256::from_be_slice(&result.bytes[32..64]);
        assert_eq!(price, U256::ZERO);
        assert_eq!(qty, U256::ZERO);
        assert_eq!(result.gas_used, CLOB_BASE_GAS_COST);
    }

    /// **Stage 9b end-to-end**: install a CLOB with a known bid, call the
    /// precompile, observe the live data flow through to the EVM-visible
    /// response. This is the moment custom EVM execution reads real
    /// orderbook state.
    #[test]
    fn read_best_bid_returns_live_state_when_clob_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        let book = Arc::new(Mutex::new(Book::new()));
        // Rest a buy @ 250 with qty 7
        book.lock().unwrap().submit(Order {
            id: OrderId(1),
            account: AccountId(42),
            side: Side::Buy,
            qty: Qty(7),
            order_type: OrderType::Limit { price: Price(250) },
        });
        // Rest another buy @ 240 (lower; shouldn't be picked as best bid)
        book.lock().unwrap().submit(Order {
            id: OrderId(2),
            account: AccountId(43),
            side: Side::Buy,
            qty: Qty(99),
            order_type: OrderType::Limit { price: Price(240) },
        });

        install_clob(book);

        let result = read_best_bid(&[], 100_000, 0).expect("precompile must not error");
        let price = U256::from_be_slice(&result.bytes[0..32]);
        let qty = U256::from_be_slice(&result.bytes[32..64]);
        assert_eq!(price, U256::from(250u64), "best bid is the 250 order, not 240");
        assert_eq!(qty, U256::from(7u64), "qty at the best level is 7");

        uninstall_clob();
    }

    /// Registry test: `princeps_precompiles()` extends a base precompile set
    /// with our CLOB precompile at the well-known address. This is what the
    /// Stage 9a `EvmFactory` plugs into every EVM instance Reth constructs.
    #[test]
    fn princeps_precompiles_registers_clob_address() {
        let base = Precompiles::cancun();
        let extended = princeps_precompiles(base);

        // The CLOB address must be in the extended set.
        assert!(
            extended.contains(&CLOB_READ_BEST_BID),
            "princeps_precompiles must register the CLOB_READ_BEST_BID address"
        );

        // The base Ethereum precompiles (e.g. ECDSA recover at 0x...01) must
        // still be present — we EXTEND, not replace.
        let ecrecover: Address = alloy_primitives::address!("0x0000000000000000000000000000000000000001");
        assert!(
            extended.contains(&ecrecover),
            "extended set must retain base Ethereum precompiles"
        );
    }

    /// Invoke the registered precompile end-to-end through the registry
    /// (rather than calling `read_best_bid` directly). This proves the
    /// registration is wired such that an EVM dispatch to the address hits
    /// our function — the same path Reth's EVM uses on `staticcall` to
    /// `CLOB_READ_BEST_BID`.
    #[test]
    fn registered_precompile_is_invokable_via_registry() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_clob();

        let extended = princeps_precompiles(Precompiles::cancun());
        let precompile = extended
            .get(&CLOB_READ_BEST_BID)
            .expect("CLOB precompile must be registered");

        // Precompile::execute is the public dispatch method — same as what
        // the EVM calls internally when a contract STATICCALLs the address.
        let result = precompile
            .execute(&[], 100_000, 0)
            .expect("call must not error");
        assert_eq!(result.bytes.len(), 64);
        // No CLOB → zero output, matching read_best_bid_returns_zero_when_no_clob_installed.
        let price = U256::from_be_slice(&result.bytes[0..32]);
        assert_eq!(price, U256::ZERO);
    }

    /// Helper: build a 128-byte ABI-aligned `place_order` calldata buffer.
    fn place_order_calldata(account: u64, side: u8, price: u64, qty: u64) -> Vec<u8> {
        let mut buf = vec![0u8; 128];
        buf[24..32].copy_from_slice(&account.to_be_bytes());
        buf[63] = side;
        buf[88..96].copy_from_slice(&price.to_be_bytes());
        buf[120..128].copy_from_slice(&qty.to_be_bytes());
        buf
    }

    /// With no CLOB installed, `place_order` rejects (returns sentinel 0).
    #[test]
    fn place_order_returns_zero_when_no_clob_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_clob();

        let calldata = place_order_calldata(42, 0, 100, 5);
        let result = place_order(&calldata, 100_000, 0).expect("precompile must not error");
        let order_id = U256::from_be_slice(&result.bytes[0..32]);
        assert_eq!(order_id, U256::ZERO);
    }

    /// `place_order` with bad input (too short, invalid side byte, zero qty)
    /// rejects without mutating state.
    #[test]
    fn place_order_rejects_malformed_input() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let book = Arc::new(Mutex::new(Book::new()));
        install_clob(book.clone());

        // Too short.
        let r = place_order(&[0u8; 64], 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[0..32]), U256::ZERO);
        assert_eq!(book.lock().unwrap().depth_bid(), 0, "no order on book after short input");

        // Invalid side byte.
        let bad_side = place_order_calldata(42, 7, 100, 5);
        let r = place_order(&bad_side, 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[0..32]), U256::ZERO);
        assert_eq!(book.lock().unwrap().depth_bid(), 0, "no order on book after bad side");

        // Zero qty.
        let zero_qty = place_order_calldata(42, 0, 100, 0);
        let r = place_order(&zero_qty, 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[0..32]), U256::ZERO);
        assert_eq!(book.lock().unwrap().depth_bid(), 0, "no order on book after zero qty");

        uninstall_clob();
    }

    /// **Stage 9c end-to-end (write side)**: place a Buy via the precompile,
    /// then read the best bid via the read precompile. The two-precompile
    /// round-trip is the moment the EVM ↔ CLOB surface becomes bidirectional.
    #[test]
    fn place_order_then_read_best_bid_round_trips() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let book = Arc::new(Mutex::new(Book::new()));
        install_clob(book);

        // EVM call: place Buy @ 175 with qty 12, account 0xABCD.
        let calldata = place_order_calldata(0xABCD, 0, 175, 12);
        let result = place_order(&calldata, 100_000, 0).expect("precompile must not error");
        let returned_id = U256::from_be_slice(&result.bytes[0..32]);
        assert!(
            returned_id > U256::ZERO,
            "place_order must return a non-zero order id on success"
        );

        // Now read the best bid via the read precompile. Should see our order.
        let read_result = read_best_bid(&[], 100_000, 0).expect("precompile must not error");
        let price = U256::from_be_slice(&read_result.bytes[0..32]);
        let qty = U256::from_be_slice(&read_result.bytes[32..64]);
        assert_eq!(price, U256::from(175u64), "best bid is the placed order's price");
        assert_eq!(qty, U256::from(12u64), "qty at best level matches placed qty");

        uninstall_clob();
    }

    /// **Stage 9c+**: when a `FILL_SINK` is installed alongside the CLOB,
    /// fills produced by a `place_order` call flow into the sink. This is the
    /// hook the bridge relies on to surface EVM-placed fills in the next
    /// `build_payload`. With no sink installed, fills are still produced but
    /// Stage 17c: build 64-byte deposit calldata `(uint64 account,
    /// int64 amount)` ABI-aligned to 32-byte chunks.
    fn deposit_calldata(account: u64, amount: i64) -> Vec<u8> {
        let mut input = vec![0u8; 64];
        input[24..32].copy_from_slice(&account.to_be_bytes());
        // Sign-extend amount into bytes 32..64.
        let sign_byte = if amount < 0 { 0xff } else { 0x00 };
        for b in &mut input[32..56] {
            *b = sign_byte;
        }
        input[56..64].copy_from_slice(&amount.to_be_bytes());
        input
    }

    /// Stage 17c: without an installed account map, deposit returns
    /// the zero sentinel — same shape as `place_order` / `read_best_bid`.
    #[test]
    fn deposit_returns_zero_when_no_accounts_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_accounts();

        let calldata = deposit_calldata(42, 500);
        let r = deposit(&calldata, 100_000, 0).expect("precompile must not error");
        assert_eq!(r.bytes.len(), 32);
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        assert_eq!(r.gas_used, DEPOSIT_BASE_GAS_COST);
    }

    /// Stage 17c: a first-time deposit creates the flat account
    /// and credits collateral. Returns the new balance encoded as
    /// a 32-byte sign-extended int.
    #[test]
    fn deposit_creates_account_and_credits_balance() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let calldata = deposit_calldata(42, 750);
        let r = deposit(&calldata, 100_000, 0).unwrap();
        // 32-byte big-endian decoding of a positive i64 = 750.
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 750);

        let map = accounts.lock().unwrap();
        let acct = map.get(&AccountId(42)).expect("account created on deposit");
        assert_eq!(acct.collateral, Notional(750));

        uninstall_accounts();
    }

    /// Stage 17e: build 64-byte withdraw calldata `(uint64 account,
    /// uint64 amount)` ABI-aligned to 32-byte chunks.
    fn withdraw_calldata(account: u64, amount: u64) -> Vec<u8> {
        let mut input = vec![0u8; 64];
        input[24..32].copy_from_slice(&account.to_be_bytes());
        input[56..64].copy_from_slice(&amount.to_be_bytes());
        input
    }

    /// Stage 17e: with no map installed, withdraw returns zero
    /// like its companion precompiles.
    #[test]
    fn withdraw_returns_zero_when_no_accounts_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_accounts();

        let r = withdraw(&withdraw_calldata(42, 100), 100_000, 0).unwrap();
        assert_eq!(r.bytes.len(), 32);
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        assert_eq!(r.gas_used, WITHDRAW_BASE_GAS_COST);
    }

    /// Stage 17e: withdraw rejects an unknown account.
    #[test]
    fn withdraw_rejects_unknown_account() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let r = withdraw(&withdraw_calldata(42, 100), 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        assert!(accounts.lock().unwrap().is_empty(), "no account materialized");

        uninstall_accounts();
    }

    /// Stage 17e: withdraw rejects when balance is insufficient.
    #[test]
    fn withdraw_rejects_insufficient_balance() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let _ = deposit(&deposit_calldata(7, 100), 100_000, 0).unwrap();
        // Try to take more than is there.
        let r = withdraw(&withdraw_calldata(7, 250), 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        // Balance untouched.
        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(7)).unwrap().collateral,
            Notional(100)
        );

        uninstall_accounts();
    }

    /// Stage 17e: happy path — deposit, then withdraw, balance
    /// reflected in the return AND in the map.
    #[test]
    fn withdraw_debits_balance_on_success() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let _ = deposit(&deposit_calldata(7, 1000), 100_000, 0).unwrap();
        let r = withdraw(&withdraw_calldata(7, 300), 100_000, 0).unwrap();

        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 700);

        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(7)).unwrap().collateral,
            Notional(700)
        );

        uninstall_accounts();
    }

    /// Stage 17j: when a CLOB with a two-sided book is installed,
    /// the precompile uses mark-aware free collateral — a long at
    /// a gain can withdraw against unrealized profits, mirroring
    /// the bridge's Rust-side rule. Sanity-check that the EVM-side
    /// withdraw stays byte-identical with `bridge.withdraw`.
    #[test]
    fn withdraw_precompile_uses_mark_aware_free_collateral_at_gain() {
        use rdk_funding::{MarkPrice, PositionSize};

        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Defensive: prior tests may have left a CLOB installed
        // whose midpoint isn't what this test wants.
        uninstall_clob();
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        // Install a CLOB with bid 119 / ask 121 → midpoint 120.
        let book = Arc::new(Mutex::new(Book::new()));
        book.lock().unwrap().submit(Order {
            id: OrderId(101),
            account: AccountId(99),
            side: Side::Buy,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(119) },
        });
        book.lock().unwrap().submit(Order {
            id: OrderId(102),
            account: AccountId(98),
            side: Side::Sell,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(121) },
        });
        install_clob(Arc::clone(&book));
        assert_eq!(current_mark(), Some(MarkPrice(120)));

        // Long 10 @ 100, collateral 500. At mark 120: uPnL=+200,
        // equity=700, IM=120, free=580.
        accounts.lock().unwrap().insert(
            AccountId(42),
            Account {
                account: AccountId(42),
                position_size: PositionSize(10),
                avg_entry: MarkPrice(100),
                collateral: Notional(500),
            },
        );

        // One above free → reject; at free → succeeds with balance
        // = 500 - 580 = -80 (deficit absorbed by the gain).
        let r = withdraw(&withdraw_calldata(42, 581), 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "above free → reject");
        let r = withdraw(&withdraw_calldata(42, 580), 100_000, 0).unwrap();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), -80);
        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(42)).unwrap().collateral,
            Notional(-80),
        );

        uninstall_accounts();
        uninstall_clob();
    }

    /// Stage 17g: with an open position installed, the precompile
    /// rejects a withdraw that would breach the initial-margin
    /// requirement — same rule the bridge enforces. A boundary
    /// withdraw (post-balance == IM_req) is allowed; one more quote
    /// is not.
    ///
    /// Stage 17j note: this test installs no CLOB, so the
    /// `current_mark()` fallback puts us on the Stage 17g
    /// avg-entry rule — exactly what this test expects.
    #[test]
    fn withdraw_precompile_respects_initial_margin() {
        use rdk_clearing::DEFAULT_INITIAL_MARGIN_BPS;
        use rdk_funding::{MarkPrice, PositionSize};

        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Stage 17j: defensive clear of any leftover CLOB so the
        // fallback (no mark → avg-entry rule) actually fires.
        uninstall_clob();
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        // Open-position account: size=10, avg_entry=100, collateral=500.
        // IM_req at default 1000 bps = 10*100*1000/10000 = 100, so
        // free collateral = 400.
        accounts.lock().unwrap().insert(
            AccountId(42),
            Account {
                account: AccountId(42),
                position_size: PositionSize(10),
                avg_entry: MarkPrice(100),
                collateral: Notional(500),
            },
        );
        // Sanity-check the IM math against the helper itself so this
        // test is robust to a future DEFAULT bps tweak.
        let acct_snapshot = *accounts.lock().unwrap().get(&AccountId(42)).unwrap();
        let im_req = rdk_clearing::initial_margin_requirement(
            &acct_snapshot,
            DEFAULT_INITIAL_MARGIN_BPS,
        );
        assert_eq!(im_req, 100, "IM_req sanity check");

        // 1 wei past the free-collateral line → reject (sentinel zero).
        let r = withdraw(&withdraw_calldata(42, 401), 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "above-IM withdraw rejects");
        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(42)).unwrap().collateral,
            Notional(500),
            "balance untouched on reject",
        );

        // Exactly to the IM line → succeeds. Post balance = IM_req = 100.
        let r = withdraw(&withdraw_calldata(42, 400), 100_000, 0).unwrap();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 100);

        // One more → reject.
        let r = withdraw(&withdraw_calldata(42, 1), 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "below-IM withdraw rejects");

        uninstall_accounts();
    }

    /// Stage 17c: a negative amount debits the balance.
    #[test]
    fn deposit_accepts_negative_amount_as_debit() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let _ = deposit(&deposit_calldata(7, 1000), 100_000, 0).unwrap();
        let r = deposit(&deposit_calldata(7, -250), 100_000, 0).unwrap();

        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 750);

        uninstall_accounts();
    }

    /// silently dropped — verified by the round-trip test above (which never
    /// installs a sink yet still observes book state changes).
    #[test]
    fn place_order_routes_fills_to_installed_sink() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let book = Arc::new(Mutex::new(Book::new()));
        let sink: Arc<Mutex<Vec<Fill>>> = Arc::new(Mutex::new(Vec::new()));
        install_clob(book);
        install_fill_sink(Arc::clone(&sink));

        // Maker: Buy @ 100, qty 10. Rests, no fill.
        let maker = place_order_calldata(1, 0, 100, 10);
        let r = place_order(&maker, 100_000, 0).unwrap();
        assert!(U256::from_be_slice(&r.bytes[0..32]) > U256::ZERO);
        assert!(sink.lock().unwrap().is_empty(), "no fills after resting maker");

        // Taker: Sell @ 100, qty 10. Crosses the maker → exactly one fill.
        let taker = place_order_calldata(2, 1, 100, 10);
        let r = place_order(&taker, 100_000, 0).unwrap();
        assert!(U256::from_be_slice(&r.bytes[0..32]) > U256::ZERO);

        let fills = sink.lock().unwrap().clone();
        assert_eq!(fills.len(), 1, "exactly one fill from the crossing taker");
        assert_eq!(fills[0].price, Price(100));
        assert_eq!(fills[0].qty, Qty(10));

        uninstall_fill_sink();
        uninstall_clob();
    }

    // ===== Stage 21: lending precompile e2e tests =====

    fn make_test_lending_market() -> (
        Arc<Mutex<BTreeMap<MarketId, Market>>>,
        Arc<Mutex<BTreeMap<(AccountId, MarketId), Position>>>,
    ) {
        use princeps_lending::{AssetId, Bps, IrmParams};
        let markets = Arc::new(Mutex::new(BTreeMap::<MarketId, Market>::new()));
        let positions =
            Arc::new(Mutex::new(BTreeMap::<(AccountId, MarketId), Position>::new()));
        let mut market = Market::new(
            MarketId(0),
            AssetId(1),
            AssetId(0),
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: LendingIndex::RAY / 10_000,
                slope_above_kink_per_block: LendingIndex::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500),
            Bps(500),
            Bps(1_000),
            0,
        );
        market.total_supplied = 1_000_000;
        markets.lock().unwrap().insert(MarketId(0), market);
        (markets, positions)
    }

    fn encode_3_chunk_input(account_id: u64, market_id: u32, amount: u128) -> Vec<u8> {
        let mut input = vec![0u8; 96];
        input[24..32].copy_from_slice(&account_id.to_be_bytes());
        input[60..64].copy_from_slice(&market_id.to_be_bytes());
        input[80..96].copy_from_slice(&amount.to_be_bytes());
        input
    }

    fn encode_5_chunk_input(
        account_id: u64,
        market_id: u32,
        amount: u128,
        coll_price: u128,
        debt_price: u128,
    ) -> Vec<u8> {
        let mut input = vec![0u8; 160];
        input[24..32].copy_from_slice(&account_id.to_be_bytes());
        input[60..64].copy_from_slice(&market_id.to_be_bytes());
        input[80..96].copy_from_slice(&amount.to_be_bytes());
        input[112..128].copy_from_slice(&coll_price.to_be_bytes());
        input[144..160].copy_from_slice(&debt_price.to_be_bytes());
        input
    }

    fn decode_u128_from_low_word(bytes: &[u8]) -> u128 {
        assert_eq!(bytes.len(), 32);
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[16..32]);
        u128::from_be_bytes(buf)
    }

    #[test]
    fn lending_deposit_precompile_e2e_mutates_position() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        let input = encode_3_chunk_input(42, 0, 500);
        let result = lending_deposit(&input, 100_000, 0).unwrap();
        let new_collateral = decode_u128_from_low_word(&result.bytes);
        assert_eq!(new_collateral, 500);

        let positions_guard = positions.lock().unwrap();
        let position = positions_guard
            .get(&(AccountId(42), MarketId(0)))
            .expect("position created");
        assert_eq!(position.collateral_amount, 500);
        drop(positions_guard);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_deposit_short_input_returns_zero() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        let short = vec![0u8; 32]; // Need 96 bytes
        let result = lending_deposit(&short, 100_000, 0).unwrap();
        let v = decode_u128_from_low_word(&result.bytes);
        assert_eq!(v, 0);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_borrow_then_health_then_repay_round_trip() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // 1. Deposit 1000 USDC
        let deposit_in = encode_3_chunk_input(1, 0, 1_000);
        lending_deposit(&deposit_in, 100_000, 0).unwrap();

        // 2. Borrow 500 ETH @ prices (1, 1) — HF should be 1.9 (healthy)
        let borrow_in = encode_5_chunk_input(1, 0, 500, 1, 1);
        let borrow_out = lending_borrow(&borrow_in, 100_000, 0).unwrap();
        assert_eq!(
            decode_u128_from_low_word(&borrow_out.bytes),
            1,
            "borrow should succeed"
        );

        // 3. Check health: expect ~1.9 RAY
        let mut health_in = vec![0u8; 128];
        health_in[24..32].copy_from_slice(&1u64.to_be_bytes());
        health_in[60..64].copy_from_slice(&0u32.to_be_bytes());
        health_in[80..96].copy_from_slice(&1u128.to_be_bytes());
        health_in[112..128].copy_from_slice(&1u128.to_be_bytes());
        let health_out = lending_health(&health_in, 100_000, 0).unwrap();
        let hf = decode_u128_from_low_word(&health_out.bytes);
        assert!(
            hf > LendingIndex::RAY,
            "HF should be > 1.0 (got {hf}, RAY = {})",
            LendingIndex::RAY
        );

        // 4. Repay 200 ETH
        let repay_in = encode_3_chunk_input(1, 0, 200);
        let repay_out = lending_repay(&repay_in, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&repay_out.bytes), 200);

        // 5. Verify market state
        let markets_guard = markets.lock().unwrap();
        let market = markets_guard.get(&MarketId(0)).unwrap();
        assert_eq!(market.total_borrowed, 300);
        drop(markets_guard);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_liquidate_precompile_e2e() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // Target=1 deposits 100, borrows 90 (healthy at price 1).
        lending_deposit(&encode_3_chunk_input(1, 0, 100), 100_000, 0).unwrap();
        lending_borrow(&encode_5_chunk_input(1, 0, 90, 1, 1), 100_000, 0).unwrap();

        // Now liquidator=2 calls with debt_price=2 (target unhealthy).
        // Encode 6-chunk input: liquidator, target, market, repay, coll_price, debt_price
        let mut input = vec![0u8; 192];
        input[24..32].copy_from_slice(&2u64.to_be_bytes()); // liquidator
        input[56..64].copy_from_slice(&1u64.to_be_bytes()); // target
        input[92..96].copy_from_slice(&0u32.to_be_bytes()); // market
        input[112..128].copy_from_slice(&50u128.to_be_bytes()); // repay
        input[144..160].copy_from_slice(&1u128.to_be_bytes()); // coll_price
        input[176..192].copy_from_slice(&2u128.to_be_bytes()); // debt_price

        let out = lending_liquidate(&input, 100_000, 0).unwrap();
        let actual_repay = decode_u128_from_low_word(&out.bytes);
        assert_eq!(actual_repay, 50);

        let positions_guard = positions.lock().unwrap();
        let pos = positions_guard
            .get(&(AccountId(1), MarketId(0)))
            .expect("position exists");
        assert_eq!(pos.scaled_debt, 40);
        drop(positions_guard);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_borrow_rejects_post_unhealthy() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // Deposit only 100, then try to borrow 1000 — would be massively underwater
        lending_deposit(&encode_3_chunk_input(1, 0, 100), 100_000, 0).unwrap();
        let borrow_out =
            lending_borrow(&encode_5_chunk_input(1, 0, 1_000, 1, 1), 100_000, 0).unwrap();
        // Failure → 0
        assert_eq!(decode_u128_from_low_word(&borrow_out.bytes), 0);

        // State should be unchanged
        let markets_guard = markets.lock().unwrap();
        assert_eq!(markets_guard.get(&MarketId(0)).unwrap().total_borrowed, 0);
        drop(markets_guard);
        let positions_guard = positions.lock().unwrap();
        let position = positions_guard.get(&(AccountId(1), MarketId(0))).unwrap();
        assert_eq!(position.scaled_debt, 0);
        drop(positions_guard);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    // ===== Lending revert-guard tests =====
    //
    // Verifies that snapshot_bridge_state + restore_bridge_state cover
    // lending markets + positions, not just the accounts/book/fills that
    // Stage 17i shipped originally. Closes the known v0 caveat that
    // lending-precompile mutations leaked across EVM reverts.

    #[test]
    fn snapshot_with_no_lending_installed_returns_none_fields() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let snap = snapshot_bridge_state();
        assert!(snap.markets.is_none());
        assert!(snap.positions.is_none());

        // Restore with all-None fields is a no-op; must not panic.
        restore_bridge_state(snap);
    }

    #[test]
    fn snapshot_then_restore_round_trips_lending_state() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // Pre-mutation state: market with 1M supplied, no positions, no debt.
        assert_eq!(positions.lock().unwrap().len(), 0);
        assert_eq!(
            markets.lock().unwrap().get(&MarketId(0)).unwrap().total_borrowed,
            0
        );

        // Take savepoint.
        let snap = snapshot_bridge_state();

        // Mutate via the precompile path (deposit + borrow).
        lending_deposit(&encode_3_chunk_input(1, 0, 1_000), 100_000, 0).unwrap();
        lending_borrow(
            &encode_5_chunk_input(1, 0, 500, 1, 1),
            100_000,
            0,
        )
        .unwrap();

        // Verify the mutations landed.
        assert_eq!(positions.lock().unwrap().len(), 1);
        assert_eq!(
            markets.lock().unwrap().get(&MarketId(0)).unwrap().total_borrowed,
            500
        );

        // Restore — simulates EVM-frame revert.
        restore_bridge_state(snap);

        // State must be exactly back to pre-mutation.
        assert_eq!(
            positions.lock().unwrap().len(),
            0,
            "positions should be restored to empty"
        );
        assert_eq!(
            markets.lock().unwrap().get(&MarketId(0)).unwrap().total_borrowed,
            0,
            "market.total_borrowed should be restored to 0"
        );

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn nested_savepoint_pattern_keeps_outer_state_safe() {
        // Mirrors the real REVM pattern: outer frame does work that should
        // persist; inner frame does work that reverts. After inner-restore,
        // only the inner mutation is undone.
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // Outer-frame work: deposit collateral. This should survive the
        // inner-frame revert.
        lending_deposit(&encode_3_chunk_input(1, 0, 1_000), 100_000, 0).unwrap();

        // Inner frame opens: savepoint, then borrow (the inner-frame work
        // that will be reverted).
        let inner_savepoint = snapshot_bridge_state();
        lending_borrow(
            &encode_5_chunk_input(1, 0, 500, 1, 1),
            100_000,
            0,
        )
        .unwrap();
        assert_eq!(
            markets.lock().unwrap().get(&MarketId(0)).unwrap().total_borrowed,
            500
        );

        // Inner frame REVERTs — restore.
        restore_bridge_state(inner_savepoint);

        // Borrow is undone; outer-frame deposit must remain.
        assert_eq!(
            markets.lock().unwrap().get(&MarketId(0)).unwrap().total_borrowed,
            0,
            "inner borrow should be undone"
        );
        let position = positions
            .lock()
            .unwrap()
            .get(&(AccountId(1), MarketId(0)))
            .cloned();
        assert_eq!(
            position.map(|p| (p.collateral_amount, p.scaled_debt)),
            Some((1_000, 0)),
            "outer deposit should persist; debt should be cleared"
        );

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    // ─── supplier-side precompiles (b1b5981 follow-up) ─────────────

    #[test]
    fn lending_supply_precompile_e2e_mutates_position() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // Snapshot pre-call total_supplied for the conservation check.
        let prior_supplied = markets
            .lock()
            .unwrap()
            .get(&MarketId(0))
            .unwrap()
            .total_supplied;

        let input = encode_3_chunk_input(7, 0, 500);
        let result = lending_supply(&input, 100_000, 0).unwrap();
        let new_nominal = decode_u128_from_low_word(&result.bytes);
        // At inception supply_index == RAY, so 500 supplied → 500 nominal.
        assert_eq!(new_nominal, 500);

        let positions_guard = positions.lock().unwrap();
        let position = positions_guard
            .get(&(AccountId(7), MarketId(0)))
            .expect("position created");
        assert!(position.scaled_supply > 0);
        // Conservation: market.total_supplied grew by exactly amount.
        let new_supplied = markets
            .lock()
            .unwrap()
            .get(&MarketId(0))
            .unwrap()
            .total_supplied;
        assert_eq!(new_supplied - prior_supplied, 500);
        drop(positions_guard);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_supply_short_input_returns_zero() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        let short = vec![0u8; 32]; // need 96
        let result = lending_supply(&short, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 0);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_supply_unknown_market_returns_zero() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // market_id = 999 doesn't exist
        let input = encode_3_chunk_input(7, 999, 500);
        let result = lending_supply(&input, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 0);

        // No position created.
        assert!(positions.lock().unwrap().get(&(AccountId(7), MarketId(0))).is_none());

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_supply_then_withdraw_supply_round_trip() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // Supply 1_000 then withdraw 400 — both should succeed; final
        // nominal_supply should be 600.
        let supply_in = encode_3_chunk_input(11, 0, 1_000);
        let supply_out = lending_supply(&supply_in, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&supply_out.bytes), 1_000);

        let withdraw_in = encode_3_chunk_input(11, 0, 400);
        let withdraw_out = lending_withdraw_supply(&withdraw_in, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&withdraw_out.bytes), 400);

        let positions_guard = positions.lock().unwrap();
        let position = positions_guard
            .get(&(AccountId(11), MarketId(0)))
            .expect("position still present");
        // supply_index is still ONE (no accrual mid-test); so nominal == scaled.
        assert_eq!(position.scaled_supply, 600);
        drop(positions_guard);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_withdraw_supply_caps_at_current_nominal() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        let supply_in = encode_3_chunk_input(11, 0, 200);
        lending_supply(&supply_in, 100_000, 0).unwrap();

        // Ask to withdraw 10_000; should cap at 200.
        let withdraw_in = encode_3_chunk_input(11, 0, 10_000);
        let withdraw_out = lending_withdraw_supply(&withdraw_in, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&withdraw_out.bytes), 200);

        let positions_guard = positions.lock().unwrap();
        let position = positions_guard.get(&(AccountId(11), MarketId(0))).unwrap();
        assert_eq!(position.scaled_supply, 0);
        drop(positions_guard);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_withdraw_supply_rejected_when_would_oversubscribe_borrowers() {
        // The supplier safety gate: withdraw is refused if it would
        // push total_supplied below total_borrowed.
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        // Stack the deck so the borrower side is sized close to the
        // supply side. Account 1 deposits 1_000 collateral and borrows
        // a substantial amount; account 2 supplies a smaller amount;
        // then account 2 tries to withdraw all of it.
        let deposit_in = encode_3_chunk_input(1, 0, 1_000);
        lending_deposit(&deposit_in, 100_000, 0).unwrap();
        let borrow_in = encode_5_chunk_input(1, 0, 700, 1, 1);
        let borrow_out = lending_borrow(&borrow_in, 100_000, 0).unwrap();
        assert_eq!(
            decode_u128_from_low_word(&borrow_out.bytes),
            1,
            "borrow should succeed",
        );
        let supply_in = encode_3_chunk_input(2, 0, 200);
        lending_supply(&supply_in, 100_000, 0).unwrap();

        // total_supplied is now (initial + 200), total_borrowed is 700.
        // Capture pre-state to assert nothing changes.
        let pre_total_supplied = markets
            .lock()
            .unwrap()
            .get(&MarketId(0))
            .unwrap()
            .total_supplied;
        let pre_scaled = positions
            .lock()
            .unwrap()
            .get(&(AccountId(2), MarketId(0)))
            .unwrap()
            .scaled_supply;

        // Attempt to withdraw enough that total_supplied drops below
        // total_borrowed. With initial = 1_000_000 in the test market,
        // this is hard to trigger from one supplier — so we test the
        // happy refusal by manually shrinking total_supplied first to
        // make the gate active.
        {
            let mut m = markets.lock().unwrap();
            let market = m.get_mut(&MarketId(0)).unwrap();
            // Push total_supplied so that withdrawing 200 would drop
            // it below the 700 total_borrowed.
            market.total_supplied = 750;
        }

        let withdraw_in = encode_3_chunk_input(2, 0, 200);
        let withdraw_out = lending_withdraw_supply(&withdraw_in, 100_000, 0).unwrap();
        assert_eq!(
            decode_u128_from_low_word(&withdraw_out.bytes),
            0,
            "withdraw should be rejected",
        );
        // Position scaled_supply unchanged.
        let post_scaled = positions
            .lock()
            .unwrap()
            .get(&(AccountId(2), MarketId(0)))
            .unwrap()
            .scaled_supply;
        assert_eq!(pre_scaled, post_scaled);
        // total_supplied still at the doctored value (no decrement).
        let post_total_supplied = markets
            .lock()
            .unwrap()
            .get(&MarketId(0))
            .unwrap()
            .total_supplied;
        assert_eq!(post_total_supplied, 750);
        let _ = pre_total_supplied;

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    #[test]
    fn lending_withdraw_supply_with_no_position_returns_zero() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();

        let (markets, positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        install_lending_positions(Arc::clone(&positions));

        let input = encode_3_chunk_input(99, 0, 500);
        let result = lending_withdraw_supply(&input, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 0);

        uninstall_lending_markets();
        uninstall_lending_positions();
    }

    // ─── socialize precompile (ADR-010 Layer 3 EVM entrypoint) ─────

    use princeps_node::operator::demo_signing::{
        demo_operator_key, demo_signing_key, sign_declaration as demo_sign_declaration,
    };

    fn encode_socialize_input(
        operator: u32,
        market_id: u32,
        unfilled: u128,
        block_height: u64,
        sig: [u8; 64],
    ) -> Vec<u8> {
        let mut buf = vec![0u8; 192];
        buf[28..32].copy_from_slice(&operator.to_be_bytes());
        buf[60..64].copy_from_slice(&market_id.to_be_bytes());
        buf[80..96].copy_from_slice(&unfilled.to_be_bytes());
        buf[120..128].copy_from_slice(&block_height.to_be_bytes());
        buf[128..192].copy_from_slice(&sig);
        buf
    }

    fn install_socialize_globals(
        operator: u32,
        seed: u8,
    ) -> (Arc<ChainHistoryStore>, Arc<OperatorRegistry>) {
        let mut registry = OperatorRegistry::new();
        registry.register(OperatorId(operator), demo_operator_key(seed));
        let registry_arc = Arc::new(registry);
        install_operator_registry(Arc::clone(&registry_arc));
        let store_arc = Arc::new(ChainHistoryStore::empty());
        install_chain_history(Arc::clone(&store_arc));
        (store_arc, registry_arc)
    }

    fn signed_input(
        operator: u32,
        market_id: u32,
        unfilled: u128,
        block_height: u64,
        seed: u8,
    ) -> Vec<u8> {
        let sk = demo_signing_key(seed);
        let decl = demo_sign_declaration(
            OperatorId(operator),
            market_id,
            unfilled,
            block_height,
            &sk,
        );
        encode_socialize_input(
            operator,
            market_id,
            unfilled,
            block_height,
            decl.signature.0,
        )
    }

    #[test]
    fn lending_socialize_e2e_mutates_market_and_appends_event() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();

        let (markets, _positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));

        // Capture pre-state for the haircut conservation check.
        let prior_supplied = markets
            .lock()
            .unwrap()
            .get(&MarketId(0))
            .unwrap()
            .total_supplied;
        let prior_index = markets
            .lock()
            .unwrap()
            .get(&MarketId(0))
            .unwrap()
            .supply_index;

        let (store, _registry) = install_socialize_globals(1, 1);

        let input = signed_input(1, 0, 100, 42, 1);
        let result = lending_socialize(&input, 100_000, 0).unwrap();
        let absorbed = decode_u128_from_low_word(&result.bytes);
        assert_eq!(absorbed, 100);

        // Market state mutated.
        let m = markets.lock().unwrap();
        let market = m.get(&MarketId(0)).unwrap();
        assert_eq!(market.total_supplied, prior_supplied - 100);
        assert!(market.supply_index.0 < prior_index.0);
        drop(m);

        // Chain history populated at the declared block height.
        let events = store.peek_at_height(42).expect("event recorded");
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChainEvent::Socialization {
                market_id,
                unfilled,
                declared_by,
            } => {
                assert_eq!(*market_id, 0);
                assert_eq!(*unfilled, 100);
                assert_eq!(declared_by, "operator-1");
            }
        }

        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();
    }

    #[test]
    fn lending_socialize_invalid_signature_returns_zero_no_mutation() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();

        let (markets, _positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        let prior = markets
            .lock()
            .unwrap()
            .get(&MarketId(0))
            .unwrap()
            .total_supplied;

        // Registry has seed=1 for operator 1; signer uses seed=2 (wrong key).
        let (store, _registry) = install_socialize_globals(1, 1);
        let input = signed_input(1, 0, 100, 42, 2);
        let result = lending_socialize(&input, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 0);

        assert_eq!(
            markets
                .lock()
                .unwrap()
                .get(&MarketId(0))
                .unwrap()
                .total_supplied,
            prior,
        );
        assert!(store.peek_at_height(42).is_none());

        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();
    }

    #[test]
    fn lending_socialize_no_registry_returns_zero() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();

        let (markets, _positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        // Chain history installed but NOT registry.
        install_chain_history(Arc::new(ChainHistoryStore::empty()));

        let input = signed_input(1, 0, 100, 42, 1);
        let result = lending_socialize(&input, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 0);

        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_chain_history();
    }

    #[test]
    fn lending_socialize_unknown_market_returns_zero() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();

        let (markets, _positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        let (store, _registry) = install_socialize_globals(1, 1);

        // Declaration cites market 99, registry has the right key for it.
        let input = signed_input(1, 99, 100, 42, 1);
        let result = lending_socialize(&input, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 0);
        assert!(store.peek_at_height(42).is_none());

        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();
    }

    #[test]
    fn lending_socialize_short_input_returns_zero() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let short = vec![0u8; 64]; // need 192
        let result = lending_socialize(&short, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 0);
    }

    #[test]
    fn lending_socialize_caps_at_total_supplied() {
        let _g = TEST_SERIALIZER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();

        let (markets, _positions) = make_test_lending_market();
        install_lending_markets(Arc::clone(&markets));
        // Shrink the market to a small total_supplied so we can verify
        // the cap-at-total behavior easily.
        {
            let mut m = markets.lock().unwrap();
            let market = m.get_mut(&MarketId(0)).unwrap();
            market.total_supplied = 200;
            market.total_borrowed = 0;
        }
        let (store, _registry) = install_socialize_globals(1, 1);

        // Request 10_000 against a market with only 200 — absorbed
        // caps at 200; chain-history event records 200, not 10_000.
        let input = signed_input(1, 0, 10_000, 42, 1);
        let result = lending_socialize(&input, 100_000, 0).unwrap();
        assert_eq!(decode_u128_from_low_word(&result.bytes), 200);
        let events = store.peek_at_height(42).expect("event recorded");
        match &events[0] {
            ChainEvent::Socialization { unfilled, .. } => assert_eq!(*unfilled, 200),
        }

        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();
    }

    // ─── E-3 fuzz harness — lending precompile boundary ────────────
    //
    // Threat-model row E-3 ("Lending precompile fuzz exposes overflow /
    // panic that halts the EVM") flagged the precompile boundary as
    // lacking a systematic fuzzer. These proptest properties cover the
    // boundary's two invariants:
    //
    //   1. Never panic on any input bytes (including malformed,
    //      truncated, or pathologically long).
    //   2. Always return a 32-byte output (the EVM ABI shape).
    //
    // Properties run without any process-global state installed —
    // that exercises the "no installed bridge" branches that real
    // smart-contract callers can hit (e.g., calling the precompile
    // before boot finishes). With state installed the deeper mutation
    // logic is covered by the existing e2e tests; here we lock down
    // the boundary itself.
    //
    // 512 cases each — generous enough to surface a parse-overflow
    // class bug, fast enough to stay in the unit-test loop. Cargo-fuzz /
    // libFuzzer would be the next step for sustained adversarial
    // fuzzing (audit-prep follow-up).

    use proptest::prelude::*;

    /// All process-global state is uninstalled — the property here
    /// is "the precompile never panics on arbitrary input AND
    /// returns a 32-byte zero word when no state is installed."
    fn drop_all_lending_state() {
        uninstall_lending_markets();
        uninstall_lending_positions();
        uninstall_operator_registry();
        uninstall_chain_history();
    }

    fn assert_safe_32byte_zero(result: PrecompileResult) {
        let output = result.expect("precompile must not error");
        assert_eq!(output.bytes.len(), 32, "precompile must return 32-byte word");
        assert!(
            output.bytes.iter().all(|b| *b == 0),
            "no state → zero word expected; got non-zero output",
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 512,
            ..ProptestConfig::default()
        })]

        #[test]
        fn fuzz_lending_deposit_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_deposit(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_borrow_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_borrow(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_repay_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_repay(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_withdraw_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_withdraw(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_supply_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_supply(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_withdraw_supply_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_withdraw_supply(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_liquidate_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_liquidate(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_health_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_health(&input, 100_000, 0));
        }

        #[test]
        fn fuzz_lending_socialize_boundary(input in prop::collection::vec(any::<u8>(), 0..512)) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            assert_safe_32byte_zero(lending_socialize(&input, 100_000, 0));
        }

        /// Boundary length grid: every precompile gets hit with inputs
        /// at and around its expected size. A single property is
        /// expensive (9× the work), so cases is reduced. Catches an
        /// off-by-one in the length check (input.len() < N vs ≤).
        #[test]
        #[ignore = "boundary-grid: lots of inputs per case; run with --ignored when wanted"]
        fn fuzz_boundary_lengths_no_panic(
            len in 0usize..256,
            seed in any::<u8>(),
        ) {
            let _g = TEST_SERIALIZER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop_all_lending_state();
            let input = vec![seed; len];
            assert_safe_32byte_zero(lending_deposit(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_borrow(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_repay(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_withdraw(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_supply(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_withdraw_supply(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_liquidate(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_health(&input, 100_000, 0));
            assert_safe_32byte_zero(lending_socialize(&input, 100_000, 0));
        }
    }
}
