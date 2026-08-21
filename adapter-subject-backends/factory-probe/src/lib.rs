//! Compile-only probe for Reth's current `EvmFactory` integration seam.
//!
//! This crate does not call DTVM. [`DtvmEvm`] deliberately delegates execution to
//! alloy-evm's standard [`EthEvm`] so the probe can isolate API compatibility from
//! backend correctness.

use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    eth::{EthEvm, EthEvmContext, EthEvmFactory},
    precompiles::PrecompilesMap,
};
use revm::{
    Inspector,
    context::{BlockEnv, CfgEnv, DBErrorMarker, TxEnv},
    context_interface::result::{EVMError, HaltReason, ResultAndState},
    inspector::NoOpInspector,
    primitives::{Address, Bytes, hardfork::SpecId},
};

/// Machine-readable warning for provenance consumers.
pub const COMPILE_ONLY_NOT_DTVM_EXECUTION: &str =
    "compile-only interface probe; execution delegates to EthEvm; not DTVM correctness evidence";

/// Compile-only factory with the associated types required by the pinned Reth checkout.
///
/// This name reserves the intended adapter shape. It does not construct a DTVM-backed VM.
#[derive(Clone, Copy, Debug, Default)]
pub struct DtvmEvmFactory;

/// Compile-only wrapper that preserves Reth's expected EVM surface.
///
/// All execution methods delegate to [`EthEvm`]. Replacing that delegation with DTVM
/// requires a separately verified EVMC host, journal, nested-call, and precompile bridge.
pub struct DtvmEvm<DB: Database, I> {
    inner: EthEvm<DB, I, PrecompilesMap>,
}

impl<DB: Database, I> DtvmEvm<DB, I> {
    fn compile_only(inner: EthEvm<DB, I, PrecompilesMap>) -> Self {
        Self { inner }
    }
}

impl<DB, I> Evm for DtvmEvm<DB, I>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &Self::BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.transact_raw(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        self.inner.components()
    }

    fn components_mut(
        &mut self,
    ) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        self.inner.components_mut()
    }
}

impl EvmFactory for DtvmEvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = DtvmEvm<DB, I>;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        evm_env: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        DtvmEvm::compile_only(EthEvmFactory::default().create_evm(db, evm_env))
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        evm_env: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        DtvmEvm::compile_only(
            EthEvmFactory::default().create_evm_with_inspector(db, evm_env, inspector),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::MAINNET;
    use reth_evm::ConfigureEvm;
    use reth_evm_ethereum::EthEvmConfig;
    use revm::database::EmptyDB;

    #[derive(Clone, Copy, Debug, Default)]
    struct ShapeOnlyInspector;

    impl Inspector<EthEvmContext<EmptyDB>> for ShapeOnlyInspector {}

    fn assert_configure_evm<T: ConfigureEvm>(_config: &T) {}

    #[test]
    fn pinned_reth_accepts_compile_only_dtvm_factory_shape() {
        let config =
            EthEvmConfig::new_with_evm_factory(MAINNET.clone(), DtvmEvmFactory);

        assert_configure_evm(&config);
        let _ = config.block_executor_factory();
        assert!(COMPILE_ONLY_NOT_DTVM_EXECUTION.starts_with("compile-only"));
    }

    #[test]
    fn factory_gat_accepts_noop_and_generic_inspector_shapes() {
        let factory = DtvmEvmFactory;
        let evm_env = EvmEnv::default();

        let _evm = factory.create_evm(EmptyDB::default(), evm_env.clone());
        let _inspected = factory.create_evm_with_inspector(
            EmptyDB::default(),
            evm_env,
            ShapeOnlyInspector,
        );
    }
}
