//! Engine node related functionality.

use std::ops::{Deref, RangeInclusive};
use alloy_rpc_types::engine::ClientVersionV1;
use futures::{future::Either, stream, stream_select, StreamExt};
use reth_beacon_consensus::{
    hooks::{EngineHooks, StaticFileHook},
    BeaconConsensusEngineHandle,
};
use reth_blockchain_tree::BlockchainTreeConfig;
use reth_chainspec::EthChainSpec;
use reth_consensus_debug_client::{DebugConsensusClient, EtherscanBlockProvider};
use reth_engine_service::service::{ChainEvent, EngineService};
use reth_engine_tree::{
    engine::{EngineApiRequest, EngineRequestHandler},
    tree::TreeConfig,
};
use reth_engine_util::EngineMessageStreamExt;
use reth_exex::ExExManagerHandle;
use reth_network::{NetworkSyncUpdater, SyncState};
use reth_network_api::{BlockDownloaderProvider, NetworkEventListenerProvider};
use reth_node_api::{BuiltPayload, FullNodeTypes, NodeAddOns, NodeTypesWithEngine};
use reth_node_core::{
    dirs::{ChainPath, DataDirPath},
    exit::NodeExitFuture,
    primitives::Head,
    rpc::eth::{helpers::AddDevSigners, FullEthApiServer},
    version::{CARGO_PKG_VERSION, CLIENT_CODE, NAME_CLIENT, VERGEN_GIT_SHA},
};
use reth_payload_builder::{EthBuiltPayload,EthPayloadBuilderAttributes,PayloadBuilderHandle,PayloadBuilderService};
use reth_node_events::{cl::ConsensusLayerHealthEvents, node};
use reth_payload_primitives::PayloadBuilder;
use reth_primitives::{BlockWithSenders, EthereumHardforks, Receipt, Receipts, Requests, SealedBlock, SealedBlockWithSenders, SealedHeader, TransactionSigned, TransactionSignedEcRecovered};
use reth_provider::providers::{BlockchainProvider2, ProviderNodeTypes};
use reth_rpc_engine_api::{capabilities::EngineCapabilities, EngineApi};
use reth_tasks::TaskExecutor;
use reth_tokio_util::EventSender;
use reth_tracing::tracing::{debug, error, info};
use std::sync::Arc;
use alloy_consensus::Header;
use alloy_primitives::{BlockHash, BlockNumber, FixedBytes, Sealable, B256};
use alloy_primitives::ruint::aliases::U256;
use alloy_rpc_types::{BlockHashOrNumber, Bundle};
use ethereum_consensus::phase0::state_transition;
use tokio::sync::{mpsc::unbounded_channel, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use reth_auto_seal_consensus::{AutoSealBuilder, AutoSealConsensus};
use reth_consensus::Consensus;
use reth_db_common::init::insert_state;
use reth_evm::{ConfigureEvm, ConfigureEvmEnv};
use reth_evm::execute::{BlockExecutorProvider, ExecutionOutcome, Executor};
use reth_execution_types::{BlockExecutionInput, BlockExecutionOutput};
use reth_node_core::primitives::BlockNumberOrTag;
use reth_node_core::primitives::revm_primitives::{BlockEnv, EnvWithHandlerCfg, TxEnv};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction, TransactionPool};
use crate::{
    common::{Attached, LaunchContextWith, WithConfigs},
    hooks::NodeHooks,
    rpc::{launch_rpc_servers, EthApiBuilderProvider},
    setup::build_networked_pipeline,
    AddOns, ExExLauncher, FullNode, LaunchContext, LaunchNode, NodeAdapter,
    NodeBuilderWithComponents, NodeComponents, NodeComponentsBuilder, NodeHandle, NodeTypesAdapter,
};
use reth_node_core::rpc::compat::engine::payload::try_into_block;
use reth_primitives::revm_primitives::{AccessList, CfgEnv, Env};
use reth_primitives::transaction::FillTxEnv;
use reth_provider::{BlockHashReader, BlockReader, BlockWriter, ChainStateBlockWriter, DBProvider, DatabaseProviderFactory, HeaderProvider, StageCheckpointWriter, StateChangeWriter, StateProviderFactory, StateReader, StateRootProvider, StaticFileProviderFactory};
use reth_revm::db::{BundleState, CacheDB, OriginalValuesKnown};
use reth_revm::{revm, State};
use reth_revm::db::states::{PlainStorageChangeset, StateChangeset};
use reth_revm::primitives::HandlerCfg;
use reth_rpc_eth_types::simulate::build_block;
use reth_rpc_eth_types::StateCacheDb;
use reth_transaction_pool::validate::ValidPoolTransaction;
use reth_trie::{HashedPostState, HashedPostStateSorted};
use reth_trie::updates::TrieUpdates;
use crate::components::ExecutorBuilder;

/// The engine node launcher.
#[derive(Debug)]
pub struct EngineNodeLauncher {
    /// The task executor for the node.
    pub ctx: LaunchContext,

    /// Temporary configuration for engine tree.
    /// After engine is stabilized, this should be configured through node builder.
    pub engine_tree_config: TreeConfig,
}

impl EngineNodeLauncher {
    /// Create a new instance of the ethereum node launcher.
    pub const fn new(
        task_executor: TaskExecutor,
        data_dir: ChainPath<DataDirPath>,
        engine_tree_config: TreeConfig,
    ) -> Self {
        Self { ctx: LaunchContext::new(task_executor, data_dir), engine_tree_config }
    }
}

impl<Types, T, CB, AO> LaunchNode<NodeBuilderWithComponents<T, CB, AO>> for EngineNodeLauncher
where
    Types: ProviderNodeTypes + NodeTypesWithEngine,
    T: FullNodeTypes<Types = Types, Provider = BlockchainProvider2<Types>>,
    CB: NodeComponentsBuilder<T>,
    AO: NodeAddOns<
        NodeAdapter<T, CB::Components>,
        EthApi: EthApiBuilderProvider<NodeAdapter<T, CB::Components>>
                    + FullEthApiServer
                    + AddDevSigners,
    >,
{
    type Node = NodeHandle<NodeAdapter<T, CB::Components>, AO>;

    async fn launch_node(
        self,
        target: NodeBuilderWithComponents<T, CB, AO>,
    ) -> eyre::Result<Self::Node> {
        let Self { ctx, engine_tree_config } = self;
        let NodeBuilderWithComponents {
            adapter: NodeTypesAdapter { database },
            components_builder,
            add_ons: AddOns { hooks, rpc, exexs: installed_exex, .. },
            config,
        } = target;
        let NodeHooks { on_component_initialized, on_node_started, .. } = hooks;
        
        println!("engine node launcher 가동중입니다!!!!");

        // TODO: move tree_config and canon_state_notification_sender
        // initialization to with_blockchain_db once the engine revamp is done
        // https://github.com/paradigmxyz/reth/issues/8742
        let tree_config = BlockchainTreeConfig::default();

        // NOTE: This is a temporary workaround to provide the canon state notification sender to the components builder because there's a cyclic dependency between the blockchain provider and the tree component. This will be removed once the Blockchain provider no longer depends on an instance of the tree: <https://github.com/paradigmxyz/reth/issues/7154>
        let (canon_state_notification_sender, _receiver) =
            tokio::sync::broadcast::channel(tree_config.max_reorg_depth() as usize * 2);

        // setup the launch context
        let ctx = ctx
            .with_configured_globals()
            // load the toml config
            .with_loaded_toml_config(config)?
            // add resolved peers
            .with_resolved_peers().await?
            // attach the database
            .attach(database.clone())
            // ensure certain settings take effect
            .with_adjusted_configs()
            // Create the provider factory
            .with_provider_factory().await?
            .inspect(|_| {
                info!(target: "reth::cli", "Database opened");
            })
            .with_prometheus_server().await?
            .inspect(|this| {
                debug!(target: "reth::cli", chain=%this.chain_id(), genesis=?this.genesis_hash(), "Initializing genesis");
            })
            .with_genesis()?
            .inspect(|this: &LaunchContextWith<Attached<WithConfigs<Types::ChainSpec>, _>>| {
                info!(target: "reth::cli", "\n{}", this.chain_spec().display_hardforks());
            })
            .with_metrics_task()
            // passing FullNodeTypes as type parameter here so that we can build
            // later the components.
            .with_blockchain_db::<T, _>(move |provider_factory| {
                Ok(BlockchainProvider2::new(provider_factory)?)
            }, tree_config, canon_state_notification_sender)?
            .with_components(components_builder, on_component_initialized).await?;



        // spawn exexs
        let exex_manager_handle = ExExLauncher::new(
            ctx.head(),
            ctx.node_adapter().clone(),
            installed_exex,
            ctx.configs().clone(),
        )
        .launch()
        .await?;

        // create pipeline
        let network_client = ctx.components().network().fetch_client().await?;
        let (consensus_engine_tx, consensus_engine_rx) = unbounded_channel();

        let node_config = ctx.node_config();
        let consensus_engine_stream = UnboundedReceiverStream::from(consensus_engine_rx)
            .maybe_skip_fcu(node_config.debug.skip_fcu)
            .maybe_skip_new_payload(node_config.debug.skip_new_payload)
            .maybe_reorg(
                ctx.blockchain_db().clone(),
                ctx.components().evm_config().clone(),
                reth_payload_validator::ExecutionPayloadValidator::new(ctx.chain_spec()),
                node_config.debug.reorg_frequency,
                node_config.debug.reorg_depth,
            )
            // Store messages _after_ skipping so that `replay-engine` command
            // would replay only the messages that were observed by the engine
            // during this run.
            .maybe_store_messages(node_config.debug.engine_api_store.clone());

        let max_block = ctx.max_block(network_client.clone() ).await?;

        
        
        let mut hooks = EngineHooks::new();

        let static_file_producer = ctx.static_file_producer();
        let static_file_producer_events = static_file_producer.lock().events();
        hooks.add(StaticFileHook::new(
            static_file_producer.clone(),
            Box::new(ctx.task_executor().clone()),
        ));
        info!(target: "reth::cli", "StaticFileProducer initialized");

        println!("{:?}", ctx.consensus());
        
        

        // Configure the pipeline
        let pipeline_exex_handle =
            exex_manager_handle.clone().unwrap_or_else(ExExManagerHandle::empty);
        let mut pipeline = build_networked_pipeline(
            &ctx.toml_config().stages,
            network_client.clone(),
            ctx.consensus(),
            ctx.provider_factory().clone(),
            ctx.task_executor(),
            ctx.sync_metrics_tx(),
            ctx.prune_config(),
            Some(10000),
            static_file_producer,
            ctx.components().block_executor().clone(),
            pipeline_exex_handle,
        )?;
        
        

        // The new engine writes directly to static files. This ensures that they're up to the tip.
        pipeline.move_to_static_files()?;

        let pipeline_events = pipeline.events();

        let mut pruner_builder = ctx.pruner_builder();
        if let Some(exex_manager_handle) = &exex_manager_handle {
            pruner_builder =
                pruner_builder.finished_exex_height(exex_manager_handle.finished_height());
        }
        let pruner = pruner_builder.build_with_provider_factory(ctx.provider_factory().clone());

        let pruner_events = pruner.events();
        info!(target: "reth::cli", prune_config=?ctx.prune_config().unwrap_or_default(), "Pruner initialized");
        
        
        // Configure the consensus engine
        let mut eth_service = EngineService::new(
            ctx.consensus(),
            ctx.components().block_executor().clone(),
            ctx.chain_spec(),
            network_client.clone(),
            Box::pin(consensus_engine_stream),
            pipeline,
            Box::new(ctx.task_executor().clone()),
            ctx.provider_factory().clone(),
            ctx.blockchain_db().clone(),
            pruner,
            ctx.components().payload_builder().clone(),
            engine_tree_config,
            ctx.invalid_block_hook()?,
            ctx.sync_metrics_tx(),
        );
        

        let event_sender = EventSender::default();

        let beacon_engine_handle =
            BeaconConsensusEngineHandle::new(consensus_engine_tx, event_sender.clone());

        info!(target: "reth::cli", "Consensus engine initialized");

        let events = stream_select!(
            ctx.components().network().event_listener().map(Into::into),
            beacon_engine_handle.event_listener().map(Into::into),
            pipeline_events.map(Into::into),
            if ctx.node_config().debug.tip.is_none() && !ctx.is_dev() {
                Either::Left(
                    ConsensusLayerHealthEvents::new(Box::new(ctx.blockchain_db().clone()))
                        .map(Into::into),
                )
            } else {
                Either::Right(stream::empty())
            },
            pruner_events.map(Into::into),
            static_file_producer_events.map(Into::into),
        );
        ctx.task_executor().spawn_critical(
            "events task",
            node::handle_events(
                Some(Box::new(ctx.components().network().clone())),
                Some(ctx.head().number),
                events,
            ),
        );

        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: NAME_CLIENT.to_string(),
            version: CARGO_PKG_VERSION.to_string(),
            commit: VERGEN_GIT_SHA.to_string(),
        };


        let engine_api = EngineApi::new(
            ctx.blockchain_db().clone(),
            ctx.chain_spec(),
            beacon_engine_handle,
            ctx.components().payload_builder().clone().into(),
            ctx.components().pool().clone(),
            Box::new(ctx.task_executor().clone()),
            client,
            EngineCapabilities::default(),
            ctx.components().engine_validator().clone(),
        );
        
       
        info!(target: "reth::cli", "Engine API handler initialized");

        // extract the jwt secret from the args if possible
        let jwt_secret = ctx.auth_jwt_secret()?;
        
        let xx = ctx.node_config();
        

        // Start RPC servers
        let (rpc_server_handles, rpc_registry) = launch_rpc_servers(
            ctx.node_adapter().clone(),
            engine_api,
            ctx.node_config(),
            jwt_secret,
            rpc,
        )
        .await?;
        
        println!("rpc 주소는 다음과 같습니다 {:?}", rpc_server_handles.rpc.http_local_addr().unwrap());

        // TODO: migrate to devmode with https://github.com/paradigmxyz/reth/issues/10104
        if let Some(maybe_custom_etherscan_url) = ctx.node_config().debug.etherscan.clone() {
            info!(target: "reth::cli", "Using etherscan as consensus client");

            let chain = ctx.node_config().chain.chain();
            let etherscan_url = maybe_custom_etherscan_url.map(Ok).unwrap_or_else(|| {
                // If URL isn't provided, use default Etherscan URL for the chain if it is known
                chain
                    .etherscan_urls()
                    .map(|urls| urls.0.to_string())
                    .ok_or_else(|| eyre::eyre!("failed to get etherscan url for chain: {chain}"))
            })?;

            let block_provider = EtherscanBlockProvider::new(
                etherscan_url,
                chain.etherscan_api_key().ok_or_else(|| {
                    eyre::eyre!(
                        "etherscan api key not found for rpc consensus client for chain: {chain}"
                    )
                })?,
            );
            let rpc_consensus_client = DebugConsensusClient::new(
                rpc_server_handles.auth.clone(),
                Arc::new(block_provider),
            );
            ctx.task_executor().spawn_critical("etherscan consensus client", async move {
                rpc_consensus_client.run::<<Types as NodeTypesWithEngine>::Engine>().await
            });
        }

        // Run consensus engine to completion
        let initial_target = ctx.initial_backfill_target()?;
        let network_handle = ctx.components().network().clone();
        let mut built_payloads = ctx
            .components()
            .payload_builder()
            .subscribe()
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to payload builder events: {:?}", e))?
            .into_built_payload_stream()
            .fuse();


        let chainspec = ctx.chain_spec();
        let (exit, rx) = oneshot::channel();
        info!(target: "reth::cli", "Starting consensus engine");
        ctx.task_executor().spawn_critical("consensus engine", async move {
            if let Some(initial_target) = initial_target {
                debug!(target: "reth::cli", %initial_target,  "start backfill sync");
                eth_service.orchestrator_mut().start_backfill_sync(initial_target);
            }

            let mut res = Ok(());

            // advance the chain and await payloads built locally to add into the engine api tree handler to prevent re-execution if that block is received as payload from the CL
            loop {
                tokio::select! {
                    payload = built_payloads.select_next_some() => {
                        if let Some(executed_block) = payload.executed_block() {
                            debug!(target: "reth::cli", block=?executed_block.block().num_hash(),  "inserting built payload");
                            eth_service.orchestrator_mut().handler_mut().handler_mut().on_event(EngineApiRequest::InsertExecutedBlock(executed_block).into());
                        }
                    }
                    event =  eth_service.next() => {
                        let Some(event) = event else { break };
                        debug!(target: "reth::cli", "Event: {event}");
                        match event {
                            ChainEvent::BackfillSyncFinished => {
                                network_handle.update_sync_state(SyncState::Idle);
                            }
                            ChainEvent::BackfillSyncStarted => {
                                network_handle.update_sync_state(SyncState::Syncing);
                            }
                            ChainEvent::FatalError => {
                                error!(target: "reth::cli", "Fatal error in consensus engine");
                                res = Err(eyre::eyre!("Fatal error in consensus engine"));
                                break
                            }
                            ChainEvent::Handler(ev) => {
                                if let Some(head) = ev.canonical_header() {
                                    let head_block = Head {
                                        number: head.number,
                                        hash: head.hash(),
                                        difficulty: head.difficulty,
                                        timestamp: head.timestamp,
                                        total_difficulty: chainspec
                                            .final_paris_total_difficulty(head.number)
                                            .unwrap_or_default(),
                                    };
                                    network_handle.update_status(head_block);
                                }
                                event_sender.notify(ev);
                            }
                        }
                    }
                }
            }

            let _ = exit.send(res);
        });

        let full_node = FullNode {
            evm_config: ctx.components().evm_config().clone(),
            block_executor: ctx.components().block_executor().clone(),
            pool: ctx.components().pool().clone(),
            network: ctx.components().network().clone(),
            provider: ctx.node_adapter().provider.clone(),
            payload_builder: ctx.components().payload_builder().clone(),
            task_executor: ctx.task_executor().clone(),
            rpc_server_handles,
            rpc_registry,
            config: ctx.node_config().clone(),
            data_dir: ctx.data_dir().clone(),
        };
        // Notify on node started
        on_node_started.on_event(full_node.clone())?;




        let mut count = 0;
        let mut txs= full_node.pool.all_transactions();


        let mut queue = txs.queued_recovered();
        let mut pend = txs.pending_recovered();

        println!("all txss are .. {:?}", txs);
 ;

        let mut block_body = reth_primitives::BlockBody{
            transactions: Vec::new(),
            ommers: Vec::new(),
            withdrawals: None,
            requests: None,
        };

        let mut add_v = Vec::new();


        let db_provider = full_node.provider.database_provider_rw()?;
        let mut sc_set = db_provider.take_state(RangeInclusive::new(0,0)).unwrap();
        let mut sc_set = sc_set.bundle.into_plain_state(OriginalValuesKnown::No);

        while let Some(tx) =  queue.next(){

            let tx: TransactionSignedEcRecovered = tx.into();
            let mut tx_env = TxEnv::default();
            tx.fill_tx_env(&mut tx_env, tx.signer());

            tx_env.nonce = Some(0);


            let mut cfg_ = CfgEnv::default();
            cfg_.chain_id = 1337;

            let env = Env{
                cfg: cfg_.clone(),
                block: BlockEnv::default(),
                tx: tx_env.clone()
            };

            println!("{:?}", tx_env);


            
            let state_provider = full_node.provider.state_by_block_number_or_tag(BlockNumberOrTag::Number(0)).unwrap();
            
            
            let mut db = reth_revm::database::StateProviderDatabase::new(state_provider);
            let env_with_box = Box::new(env);
            let cfgg = EnvWithHandlerCfg::new(env_with_box,HandlerCfg::default());

            let mut evm = full_node.evm_config.evm_with_env(db, cfgg);
            let result = evm.transact();

            println!("\n\n\n #######account{:?}\n\n\n", sc_set.accounts);
            println!("\n\n\n #######storage{:?}\n\n\n", sc_set.storage);

            let mut sc_set = sc_set.clone();


            match  result{
                Ok(x) =>{

                    let tx: TransactionSigned = tx.into();
                    add_v.push(tx.recover_signer().unwrap());
                    block_body.transactions.push(tx);
                    println!("state is... {:?}",x.state);
                    for (address, account) in x.state.iter() {
                        if let Some(index) = sc_set.accounts.iter().position(|addr| {
                            let (ad , ac) = addr;
                            ad == address
                        }) {
                            sc_set.accounts[index].1 = Some(account.info.clone());
                        }
                        else{
                            sc_set.accounts.push((address.clone(),Some(account.info.clone())));
                        }

                    }

                    println!("\n\n\n #######account{:?}\n\n\n", sc_set.accounts);
                    println!("\n\n\n #######storage{:?}\n\n\n", sc_set.storage);

                    let r = db_provider.write_state_changes(sc_set);
                    match r {
                        Ok(r) => { println!(" 쓰기 성공 ");},
                        Err(e) => {println!("실패");}
                    }
                }
                Err(e) =>{

                }
            }

        }

        while let Some(tx) =  pend.next(){

            let tx: TransactionSignedEcRecovered = tx.into();
            let mut tx_env = TxEnv::default();
            tx.fill_tx_env(&mut tx_env, tx.signer());

            tx_env.nonce = Some(0);


            let mut cfg_ = CfgEnv::default();
            cfg_.chain_id = 1337;

            let env = Env{
                cfg: cfg_.clone(),
                block: BlockEnv::default(),
                tx: tx_env.clone()
            };

            println!("{:?}", tx_env);



            let state_provider = full_node.provider.state_by_block_number_or_tag(BlockNumberOrTag::Number(0)).unwrap();

            let mut db = reth_revm::database::StateProviderDatabase::new(state_provider);
            let env_with_box = Box::new(env);
            let cfgg = EnvWithHandlerCfg::new(env_with_box,HandlerCfg::default());

            let mut evm = full_node.evm_config.evm_with_env(db, cfgg);
            let result = evm.transact();

            println!("\n\n\n #######account{:?}\n\n\n", sc_set.accounts);
            println!("\n\n\n #######storage{:?}\n\n\n", sc_set.storage);

            let mut sc_set = sc_set.clone();


            match  result{
                Ok(x) =>{

                    let tx: TransactionSigned = tx.into();
                    add_v.push(tx.recover_signer().unwrap());
                    block_body.transactions.push(tx);
                    println!("state is... {:?}",x.state);
                    for (address, account) in x.state.iter() {
                        if let Some(index) = sc_set.accounts.iter().position(|addr| {
                            let (ad , ac) = addr;
                            ad == address
                        }) {
                            sc_set.accounts[index].1 = Some(account.info.clone());
                        }
                        else{
                            sc_set.accounts.push((address.clone(),Some(account.info.clone())));
                        }

                    }

                    println!("\n\n\n #######account{:?}\n\n\n", sc_set.accounts);
                    println!("\n\n\n #######storage{:?}\n\n\n", sc_set.storage);

                    let r = db_provider.write_state_changes(sc_set);
                    match r {
                        Ok(r) => { println!(" 쓰기 성공 ");},
                        Err(e) => {println!("실패");}
                    }
                }
                Err(e) =>{

                }
            }

        }

       let b_hash = full_node.provider.convert_block_hash(BlockHashOrNumber::Number(0)).unwrap().unwrap();

        if add_v.len() > 0 {
            println!("okay");
            let header = Header {
                parent_hash: b_hash,
                ommers_hash: FixedBytes::<32>::ZERO, // 비어있으므로 EMPTY_LIST_HASH가 됨
                beneficiary: add_v[0], // 첫 번째 트랜잭션 서명자를 beneficiary로 설정
                state_root: B256::ZERO, // 나중에 업데이트됨
                transactions_root: block_body.calculate_tx_root(),
                receipts_root: B256::ZERO, // 나중에 업데이트됨
                logs_bloom: Default::default(),
                difficulty: U256::ZERO,
                number: 1, // genesis 다음 블록
                gas_limit: 30_000_000u64.into(), // 적절한 가스 리밋 설정
                gas_used: 0, // 나중에 업데이트됨
                timestamp: 0, // 현재 시간으로 설정하거나 적절한 값 사용
                extra_data: vec![].into(),
                mix_hash: B256::ZERO,
                nonce: FixedBytes::<8>::ZERO,
                base_fee_per_gas: None,
                withdrawals_root: None,
                blob_gas_used: None,
                excess_blob_gas: None,
                parent_beacon_block_root: Some(B256::ZERO),
                requests_root: block_body.calculate_requests_root()
            };
            let seal = header.clone().seal_slow().seal();
            let seal = SealedHeader::new(header.clone(), seal);

            let block = reth_primitives::Block {
                header: header.clone(),
                body: block_body.clone()
            };

            let blockwithsenders = BlockWithSenders::new(block, add_v.clone());

            match blockwithsenders {
                Some(k) => {
                    
                    let state_provider = full_node.provider.state_by_block_number_or_tag(BlockNumberOrTag::Number(0)).unwrap();

                    let mut db = reth_revm::database::StateProviderDatabase::new(state_provider);

                    let input = BlockExecutionInput::new(&k, U256::ZERO);

                    let outcome = full_node.block_executor.executor(db).execute(input);

                    match outcome {
                        Ok(outcome) => {
                            println!("진입!!!");
                            let outcome: BlockExecutionOutput<Receipt> = outcome.into();
                            let outcome = ExecutionOutcome {
                                bundle: outcome.state,
                                receipts: Receipts::from(outcome.receipts),
                                first_block: 1,
                                requests: Vec::<Requests>::new()
                            };
                            println!("{:?}",outcome);
                            let h = HashedPostStateSorted::default();
                            let t = TrieUpdates::default();
                            let mut v = Vec::<SealedBlockWithSenders>::new();
                            let seal_block = SealedBlock::new(seal, block_body);
                            let seal_block_v = SealedBlockWithSenders::new(seal_block, add_v).unwrap();
                            v.push(seal_block_v);
                            let result = db_provider.append_blocks_with_state(v, outcome, h, t).unwrap();
                            
                            let consensus =  ctx.consensus();
                            AutoSealBuilder::
                            
                        }
                        Err(e) => {
                            println!("{:?}", e);
                        }
                    }
                }
                None =>{
                    println!("썩 꺼지라");
                }

            }


        }

       


            
        let handle = NodeHandle {
            node_exit_future: NodeExitFuture::new(
                async { rx.await? },
                full_node.config.debug.terminate,
            ),
            node: full_node,
        };

        Ok(handle)
    }

    
}


