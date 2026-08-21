//! Reth node whose execution engine is an EVMC subject backend (DTVM or evmone)
//! instead of the built-in REVM interpreter.
//!
//! This is the production-mode counterpart to the witness-based replay harness:
//! the engine here reads state from the real on-disk mdbx database through
//! reth's normal provider stack, rather than from a pre-loaded in-memory
//! witness. Everything except the EVM factory is stock reth, so a run of this
//! binary and a run of the stock `reth` binary differ in exactly one dimension.
//!
//! Backend selection reuses the same environment contract as the sealed replay
//! experiments:
//!   * `RETH_SUBJECT_BACKEND`        - `dtvm-eager` | `dtvm-profile-guided` | `evmone-advanced`
//!   * `RETH_SUBJECT_LIBRARY`        - path to the EVMC shared library
//!   * `RETH_SUBJECT_LIBRARY_SHA256` - required; the library is refused unless it matches

#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

// Required for "override_allocator_on_supported_platforms".
#[cfg(all(feature = "jemalloc", unix))]
use reth_cli_util::allocator::tikv_jemalloc_sys as _;

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use clap::Parser;
use reth_chainspec::ChainSpec;
use reth_dtvm_transaction_adapter::{SubjectBackend, SubjectEvmFactory};
use reth_ethereum_cli::{chainspec::EthereumChainSpecParser, Cli};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm_ethereum::EthEvmConfig;
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_node_api::{FullNodeComponents, PayloadAttributesBuilder};
use reth_node_builder::{
    components::{BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, DebugNode, Node, NodeAdapter,
};
use reth_payload_primitives::PayloadTypes;
use reth_node_ethereum::{
    node::{
        EthereumAddOns, EthereumConsensusBuilder, EthereumEngineValidatorBuilder,
        EthereumEthApiBuilder, EthereumNetworkBuilder, EthereumPayloadBuilder, EthereumPoolBuilder,
    },
    EthEngineTypes,
};
use reth_provider::EthStorage;
use tracing::info;

/// Every [`SubjectEvmFactory`] handed to reth, kept so the process can report how
/// many EVMC VMs were actually instantiated.
///
/// The factory's VM cache is *per thread*: `shared_dtvm()` loads a VM the first
/// time each thread asks for one. So this count is the number of threads that
/// executed, and each of those threads paid a full cold JIT compile. A count
/// above 1 means measured time includes duplicated compilation, which changes
/// how any timing on this binary must be read.
static FACTORIES: std::sync::Mutex<Vec<SubjectEvmFactory>> = std::sync::Mutex::new(Vec::new());

fn report_vm_create_counts() {
    let Ok(factories) = FACTORIES.lock() else { return };
    for (index, factory) in factories.iter().enumerate() {
        info!(
            target: "reth::cli",
            factory = index,
            vm_create_count = factory.vm_create_count(),
            "EVMC subject VM instantiations (one per thread that executed)"
        );
    }
}

const BACKEND_ENV: &str = "RETH_SUBJECT_BACKEND";
const LIBRARY_ENV: &str = "RETH_SUBJECT_LIBRARY";
const LIBRARY_SHA256_ENV: &str = "RETH_SUBJECT_LIBRARY_SHA256";

/// Resolved, integrity-checked description of the execution engine to install.
#[derive(Clone, Debug)]
struct SubjectSelection {
    backend: SubjectBackend,
    library: PathBuf,
    sha256: String,
}

impl SubjectSelection {
    /// Reads the selection from the environment and verifies the library hash.
    ///
    /// The hash check is mandatory. A benchmark that silently ran against a
    /// different build of the engine than the one it claims would be worse than
    /// no benchmark at all, so a missing or mismatched hash is a hard error.
    fn from_env() -> eyre::Result<Self> {
        let backend = SubjectBackend::from_env().map_err(|error| {
            eyre::eyre!("{BACKEND_ENV} is not set to a supported backend: {error}")
        })?;

        let library = PathBuf::from(std::env::var(LIBRARY_ENV).map_err(|_| {
            eyre::eyre!("{LIBRARY_ENV} must point at the EVMC shared library to load")
        })?);

        let expected = std::env::var(LIBRARY_SHA256_ENV).map_err(|_| {
            eyre::eyre!(
                "{LIBRARY_SHA256_ENV} must be set; the engine library is pinned by hash so that \
                 measurements can be attributed to an exact build"
            )
        })?;
        let expected = expected.trim().to_ascii_lowercase();

        let actual = sha256_of(&library)?;
        if actual != expected {
            eyre::bail!(
                "{LIBRARY_ENV} hash mismatch for {}: expected {expected}, found {actual}",
                library.display()
            );
        }

        Ok(Self { backend, library, sha256: actual })
    }
}

/// Streams the file so a multi-hundred-megabyte library is not held in memory.
fn sha256_of(path: &Path) -> eyre::Result<String> {
    use std::io::Read;

    let mut file = fs::File::open(path)
        .map_err(|error| eyre::eyre!("cannot open {}: {error}", path.display()))?;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        sha2::Digest::update(&mut hasher, &buf[..read]);
    }
    let digest = sha2::Digest::finalize(hasher);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Builds the [`EthEvmConfig`] carrying the EVMC subject backend.
///
/// Shared by the node-launch path and the offline CLI commands so both install
/// the same engine; a benchmark whose two entry points disagreed about which
/// EVM they were running would be worthless.
fn build_subject_evm_config(
    chain_spec: std::sync::Arc<ChainSpec>,
) -> eyre::Result<EthEvmConfig<ChainSpec, SubjectEvmFactory>> {
    let SubjectSelection { backend, library, sha256 } = SubjectSelection::from_env()?;
    info!(
        target: "reth::cli",
        backend = ?backend,
        library = %library.display(),
        sha256 = %sha256,
        "Installing EVMC subject backend as the node execution engine"
    );

    let factory = SubjectEvmFactory::new_for(library, backend);
    if let Ok(mut factories) = FACTORIES.lock() {
        factories.push(factory.clone());
    }
    Ok(EthEvmConfig::new_with_evm_factory(chain_spec, factory))
}

/// Installs [`SubjectEvmFactory`] in place of reth's default EVM factory.
///
/// This is the entire behavioural delta from the stock node: the same
/// [`EthEvmConfig`] that reth normally builds, parameterised with a different
/// [`EvmFactory`](reth_evm::EvmFactory).
#[derive(Clone, Debug, Default)]
struct SubjectExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for SubjectExecutorBuilder
where
    Types: NodeTypes<Primitives = EthPrimitives, ChainSpec = ChainSpec>,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = EthEvmConfig<ChainSpec, SubjectEvmFactory>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        build_subject_evm_config(ctx.chain_spec())
    }
}

/// A node type identical to [`EthereumNode`] except that its executor component
/// is [`SubjectExecutorBuilder`].
///
/// This exists because the CLI resolves the EVM type from the *node type*
/// (`EvmFor<N>`), not from the launch closure. Offline commands such as
/// `re-execute` therefore run whatever engine the node type declares — with
/// `EthereumNode` they silently run stock REVM even inside this binary, which
/// would make an engine comparison measure nothing at all.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
struct SubjectNode;

impl NodeTypes for SubjectNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl<N> Node<N> for SubjectNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        SubjectExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns =
        EthereumAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(EthereumPoolBuilder::default())
            .executor(SubjectExecutorBuilder)
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
    }
}

/// Mirrors [`EthereumNode`]'s [`DebugNode`] impl so the subject node keeps the
/// stock binary's debug launch capabilities.
impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for SubjectNode {
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes> {
        LocalPayloadAttributesBuilder::new(std::sync::Arc::new(chain_spec.clone()))
    }
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    // Resolve and verify the engine before anything runs, so a bad
    // configuration fails immediately instead of part-way through a sync.
    if let Err(err) = SubjectSelection::from_env() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }

    // The components closure supplies the EVM used by the offline CLI commands
    // (`re-execute`, `stage`, ...). It mirrors the stock binary's closure with
    // the subject factory substituted for reth's default.
    let components = |spec: std::sync::Arc<ChainSpec>| {
        let evm_config = build_subject_evm_config(spec.clone())
            .expect("EVMC subject backend was validated at startup");
        (evm_config, std::sync::Arc::new(EthBeaconConsensus::new(spec)))
    };

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run_with_components::<SubjectNode>(
        components,
        async move |builder, _| {
            info!(target: "reth::cli", "Launching node with EVMC subject backend");
            let handle =
                builder.node(SubjectNode::default()).launch_with_debug_capabilities().await?;

            handle.wait_for_node_exit().await
        },
    ) {
        report_vm_create_counts();
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }

    report_vm_create_counts();
}
