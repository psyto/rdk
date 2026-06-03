//! `PrincepsEvmFactory` + `PrincepsExecutorBuilder` — Reth's `ConfigureEvm` slot,
//! filled with our custom-precompile EVM.
//!
//! Stage 9a (scout commit) — modelled on Reth's `examples/custom-evm/src/main.rs`
//! pattern. The factory's `create_evm` installs `princeps_precompiles(...)` so
//! any EVM execution path (RPC call, payload assembly, validation) sees the
//! CLOB precompile registered at `CLOB_READ_BEST_BID`.
//!
//! ### Stage 17k — revert-aware by default
//!
//! `PrincepsEvmFactory::Evm<DB, I>` is `EthEvm<DB, (I, PrincepsRevertGuard), P>`
//! — every EVM the factory hands out runs the user's inspector AND
//! [`crate::precompiles::PrincepsRevertGuard`], composed via REVM's
//! built-in `Inspector for (L, R)` tuple impl. The guard snapshots
//! the precompile globals (`{accounts, CLOB book, pending_fills}`)
//! on each call-frame entry and restores on revert, so a contract
//! that calls `princeps_deposit` and then `REVERT`s no longer mints
//! collateral.
//!
//! The `inspect` flag on the constructed `EthEvm` is now `true`
//! by default (was `false` for `create_evm` through Stage 17j) so
//! the guard actually runs in production Reth-executor paths.
//! Negligible cost for the v0 dev seed; meaningful semantics for
//! any real EVM transaction that touches an princeps precompile.

use alloy_evm::{
    eth::EthEvmContext,
    precompiles::PrecompilesMap,
    revm::{
        context::{BlockEnv, CfgEnv, Context, TxEnv},
        context_interface::result::{EVMError, HaltReason, ResultAndState},
        handler::{EthPrecompiles, PrecompileProvider},
        inspector::{Inspector, NoOpInspector},
        interpreter::{interpreter::EthInterpreter, InterpreterResult},
        precompile::Precompiles,
        primitives::{hardfork::SpecId, Address, Bytes},
        MainBuilder, MainContext,
    },
    Database, Evm, EvmEnv, EvmFactory,
};
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm_ethereum::{EthEvm, EthEvmConfig};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ExecutorBuilder, BuilderContext};
use std::sync::OnceLock;

use crate::precompiles::{princeps_precompiles, PrincepsRevertGuard};

/// EVM factory that registers princeps's custom precompiles on every EVM
/// instance Reth constructs (for payload assembly, block validation, RPC
/// state queries, etc.).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PrincepsEvmFactory;

impl EvmFactory for PrincepsEvmFactory {
    /// Stage 17k: every EVM the factory hands out is wrapped in
    /// [`PrincepsEvm`], which internally composes the user-facing
    /// inspector with [`PrincepsRevertGuard`] (via REVM's
    /// `Inspector for (L, R)` tuple impl) but presents the
    /// user-facing inspector as its `Inspector` associated type.
    /// The trait pins `Inspector = I`, hence the wrapper rather
    /// than a direct tuple alias.
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>, EthInterpreter>> =
        PrincepsEvm<DB, I, Self::Precompiles>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        let spec = input.cfg_env.spec;
        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector((NoOpInspector::default(), PrincepsRevertGuard::new()))
            .with_precompiles(PrecompilesMap::from_static(precompiles_for(spec)));
        // `inspect = true` (was `false` through 17j) so the guard
        // actually runs in production code paths.
        PrincepsEvm {
            inner: EthEvm::new(evm, true),
        }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>, EthInterpreter>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec = input.cfg_env.spec;
        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector((inspector, PrincepsRevertGuard::new()))
            .with_precompiles(PrecompilesMap::from_static(precompiles_for(spec)));
        PrincepsEvm {
            inner: EthEvm::new(evm, true),
        }
    }
}

/// Wrapper around [`EthEvm`] that internally composes the user's
/// inspector with [`PrincepsRevertGuard`] but presents the user's
/// inspector as the [`Evm::Inspector`] associated type. Needed
/// because `EvmFactory::Evm`'s GAT bound pins `Inspector = I` —
/// a direct `EthEvm<DB, (I, Guard), P>` would set
/// `Inspector = (I, Guard)` and violate that constraint.
///
/// Almost every method delegates 1:1 to the inner [`EthEvm`].
/// [`Self::components`] / [`Self::components_mut`] peel the
/// `.0` off the inspector tuple so the user only sees their own
/// inspector.
pub struct PrincepsEvm<DB: Database, I, P> {
    inner: EthEvm<DB, (I, PrincepsRevertGuard), P>,
}

impl<DB: Database, I, P> core::fmt::Debug for PrincepsEvm<DB, I, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrincepsEvm").finish_non_exhaustive()
    }
}

impl<DB, I, P> Evm for PrincepsEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
    P: PrecompileProvider<EthEvmContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &CfgEnv<SpecId> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(
        &mut self,
        tx: TxEnv,
    ) -> Result<ResultAndState<HaltReason>, Self::Error> {
        self.inner.transact_raw(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<HaltReason>, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (DB, EvmEnv) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&DB, &I, &P) {
        let (db, tuple, pre) = self.inner.components();
        (db, &tuple.0, pre)
    }

    fn components_mut(&mut self) -> (&mut DB, &mut I, &mut P) {
        let (db, tuple, pre) = self.inner.components_mut();
        (db, &mut tuple.0, pre)
    }
}

/// Lazily-initialised per-spec precompile sets. `OnceLock` ensures we build
/// each set once and share the static reference across every `create_evm` call,
/// matching the pattern in Reth's custom-evm example. Shanghai/Paris/London
/// don't add new precompiles, so they fall through to the Berlin set.
fn precompiles_for(spec: SpecId) -> &'static Precompiles {
    static PRAGUE: OnceLock<Precompiles> = OnceLock::new();
    static CANCUN: OnceLock<Precompiles> = OnceLock::new();
    static FALLBACK: OnceLock<Precompiles> = OnceLock::new();

    match spec {
        SpecId::PRAGUE | SpecId::OSAKA => {
            PRAGUE.get_or_init(|| princeps_precompiles(Precompiles::prague()))
        }
        SpecId::CANCUN => CANCUN.get_or_init(|| princeps_precompiles(Precompiles::cancun())),
        // For older hardforks (Berlin/London/Paris/Shanghai), use the Berlin
        // set as the most-recent-additions-cutoff base plus ours.
        _ => FALLBACK.get_or_init(|| {
            let base = EthPrecompiles::new(spec).precompiles;
            princeps_precompiles(base)
        }),
    }
}

/// Executor builder that swaps in `PrincepsEvmFactory` while keeping all other
/// Reth `EthereumNode` components at default.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct PrincepsExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for PrincepsExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = EthEvmConfig<ChainSpec, PrincepsEvmFactory>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(EthEvmConfig::new_with_evm_factory(
            ctx.chain_spec(),
            PrincepsEvmFactory,
        ))
    }
}
