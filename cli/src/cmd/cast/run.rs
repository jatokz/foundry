use crate::{cmd::Cmd, init_progress, update_progress, utils::try_consume_config_rpc_url};
use cast::trace::{identifier::SignaturesIdentifier, CallTraceDecoder, Traces};
use clap::Parser;
use ethers::{
    abi::Address,
    prelude::{artifacts::ContractBytecodeSome, ArtifactId, Middleware},
    solc::utils::RuntimeOrHandle,
    types::H256,
};
use eyre::WrapErr;
use forge::{
    debug::DebugArena,
    executor::{
        inspector::cheatcodes::util::configure_tx_env, opts::EvmOpts, Backend, DeployResult,
        ExecutorBuilder, RawCallResult,
    },
    trace::{identifier::EtherscanIdentifier, CallTraceDecoderBuilder, TraceKind},
};
use foundry_common::try_get_http_provider;
use foundry_config::{find_project_root_path, Config};
use std::{collections::BTreeMap, str::FromStr};
use tracing::trace;
use ui::{TUIExitReason, Tui, Ui};
use yansi::Paint;

#[derive(Debug, Clone, Parser)]
pub struct RunArgs {
    #[clap(help = "The transaction hash.", value_name = "TXHASH")]
    tx_hash: String,
    #[clap(short, long, env = "ETH_RPC_URL", value_name = "URL")]
    rpc_url: Option<String>,
    #[clap(long, short = 'd', help = "Debugs the transaction.")]
    debug: bool,
    #[clap(long, short = 't', help = "Print out opcode traces.")]
    trace_printer: bool,
    #[clap(
        long,
        short = 'q',
        help = "Executes the transaction only with the state from the previous block. May result in different results than the live execution!"
    )]
    quick: bool,
    #[clap(long, short = 'v', help = "Prints full address")]
    verbose: bool,
    #[clap(
        long,
        help = "Labels address in the trace. 0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045:vitalik.eth",
        value_name = "LABEL"
    )]
    label: Vec<String>,
}

impl Cmd for RunArgs {
    type Output = ();
    /// Executes the transaction by replaying it
    ///
    /// This replays the entire block the transaction was mined in unless `quick` is set to true
    ///
    /// Note: This executes the transaction(s) as is: Cheatcodes are disabled
    fn run(self) -> eyre::Result<Self::Output> {
        RuntimeOrHandle::new().block_on(self.run_tx())
    }
}

impl RunArgs {
    async fn run_tx(self) -> eyre::Result<()> {
        let figment = Config::figment_with_root(find_project_root_path().unwrap());
        let mut evm_opts = figment.extract::<EvmOpts>()?;
        let config = Config::from_provider(figment).sanitized();

        let rpc_url = try_consume_config_rpc_url(self.rpc_url)?;
        let provider = try_get_http_provider(&rpc_url)?;

        let tx_hash = H256::from_str(&self.tx_hash).wrap_err("invalid tx hash")?;
        let tx = provider
            .get_transaction(tx_hash)
            .await?
            .ok_or_else(|| eyre::eyre!("tx not found: {:?}", tx_hash))?;

        let tx_block_number = tx
            .block_number
            .ok_or_else(|| eyre::eyre!("tx may still be pending: {:?}", tx_hash))?
            .as_u64();
        evm_opts.fork_url = Some(rpc_url);
        // we need to set the fork block to the previous block, because that's the state at
        // which we access the data in order to execute the transaction(s)
        evm_opts.fork_block_number = Some(tx_block_number - 1);

        // Set up the execution environment
        let env = evm_opts.evm_env().await;
        let db = Backend::spawn(evm_opts.get_fork(&config, env.clone()));

        // configures a bare version of the evm executor: no cheatcode inspector is enabled,
        // tracing will be enabled only for the targeted transaction
        let builder = ExecutorBuilder::default()
            .with_config(env)
            .with_spec(crate::utils::evm_spec(&config.evm_version));

        let mut executor = builder.build(db);

        let mut env = executor.env().clone();
        env.block.number = tx_block_number.into();

        let block = provider.get_block_with_txs(tx_block_number).await?;
        if let Some(ref block) = block {
            env.block.timestamp = block.timestamp;
            env.block.coinbase = block.author.unwrap_or_default();
            env.block.difficulty = block.difficulty;
            env.block.prevrandao = block.mix_hash;
            env.block.basefee = block.base_fee_per_gas.unwrap_or_default();
            env.block.gas_limit = block.gas_limit;
        }

        // Set the state to the moment right before the transaction
        if !self.quick {
            println!("Executing previous transactions from the block.");

            if let Some(block) = block {
                let pb = init_progress!(block.transactions, "tx");
                pb.set_position(0);

                for (index, tx) in block.transactions.into_iter().enumerate() {
                    if tx.hash().eq(&tx_hash) {
                        break
                    }

                    configure_tx_env(&mut env, &tx);

                    if let Some(to) = tx.to {
                        trace!(tx=?tx.hash,?to, "executing previous call transaction");
                        executor.commit_tx_with_env(env.clone()).wrap_err_with(|| {
                            format!("Failed to execute transaction: {:?}", tx.hash())
                        })?;
                    } else {
                        trace!(tx=?tx.hash, "executing previous create transaction");
                        executor.deploy_with_env(env.clone(), None).wrap_err_with(|| {
                            format!("Failed to deploy transaction: {:?}", tx.hash())
                        })?;
                    }

                    update_progress!(pb, index);
                }
            }
        }

        // Execute our transaction
        let mut result = {
            executor
                .set_tracing(true)
                .set_debugger(self.debug)
                .set_trace_printer(self.trace_printer);

            configure_tx_env(&mut env, &tx);

            if let Some(to) = tx.to {
                trace!(tx=?tx.hash,to=?to, "executing call transaction");
                let RawCallResult {
                    reverted,
                    gas_used: gas,
                    traces,
                    debug: run_debug,
                    exit_reason: _,
                    ..
                } = executor.commit_tx_with_env(env).unwrap();

                RunResult {
                    success: !reverted,
                    traces: vec![(TraceKind::Execution, traces.unwrap_or_default())],
                    debug: run_debug.unwrap_or_default(),
                    gas_used: gas,
                }
            } else {
                trace!(tx=?tx.hash, "executing create transaction");
                let DeployResult { gas_used, traces, debug: run_debug, .. }: DeployResult =
                    executor.deploy_with_env(env, None).unwrap();

                RunResult {
                    success: true,
                    traces: vec![(TraceKind::Execution, traces.unwrap_or_default())],
                    debug: run_debug.unwrap_or_default(),
                    gas_used,
                }
            }
        };

        let mut etherscan_identifier =
            EtherscanIdentifier::new(&config, evm_opts.get_remote_chain_id())?;

        let labeled_addresses: BTreeMap<Address, String> = self
            .label
            .iter()
            .filter_map(|label_str| {
                let mut iter = label_str.split(':');

                if let Some(addr) = iter.next() {
                    if let (Ok(address), Some(label)) = (Address::from_str(addr), iter.next()) {
                        return Some((address, label.to_string()))
                    }
                }
                None
            })
            .collect();

        let mut decoder = CallTraceDecoderBuilder::new().with_labels(labeled_addresses).build();

        decoder.add_signature_identifier(SignaturesIdentifier::new(
            Config::foundry_cache_dir(),
            config.offline,
        )?);

        for (_, trace) in &mut result.traces {
            decoder.identify(trace, &mut etherscan_identifier);
        }

        if self.debug {
            let (sources, bytecode) = etherscan_identifier.get_compiled_contracts().await?;
            run_debugger(result, decoder, bytecode, sources)?;
        } else {
            print_traces(&mut result, decoder, self.verbose).await?;
        }
        Ok(())
    }
}

fn run_debugger(
    result: RunResult,
    decoder: CallTraceDecoder,
    known_contracts: BTreeMap<ArtifactId, ContractBytecodeSome>,
    sources: BTreeMap<ArtifactId, String>,
) -> eyre::Result<()> {
    let calls: Vec<DebugArena> = vec![result.debug];
    let flattened = calls.last().expect("we should have collected debug info").flatten(0);
    let tui = Tui::new(
        flattened,
        0,
        decoder.contracts,
        known_contracts.into_iter().map(|(id, artifact)| (id.name, artifact)).collect(),
        sources
            .into_iter()
            .map(|(id, source)| {
                let mut sources = BTreeMap::new();
                sources.insert(0, source);
                (id.name, sources)
            })
            .collect(),
    )?;
    match tui.start().expect("Failed to start tui") {
        TUIExitReason::CharExit => Ok(()),
    }
}

async fn print_traces(
    result: &mut RunResult,
    decoder: CallTraceDecoder,
    verbose: bool,
) -> eyre::Result<()> {
    if result.traces.is_empty() {
        eyre::bail!("Unexpected error: No traces. Please report this as a bug: https://github.com/foundry-rs/foundry/issues/new?assignees=&labels=T-bug&template=BUG-FORM.yml");
    }

    println!("Traces:");
    for (_, trace) in &mut result.traces {
        decoder.decode(trace).await;
        if !verbose {
            println!("{trace}");
        } else {
            println!("{trace:#}");
        }
    }
    println!();

    if result.success {
        println!("{}", Paint::green("Transaction successfully executed."));
    } else {
        println!("{}", Paint::red("Transaction failed."));
    }

    println!("Gas used: {}", result.gas_used);
    Ok(())
}

struct RunResult {
    pub success: bool,
    pub traces: Traces,
    pub debug: DebugArena,
    pub gas_used: u64,
}
