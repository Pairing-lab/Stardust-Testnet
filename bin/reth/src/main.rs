#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use std::sync::Arc;
use clap::{Args, Parser};
use reth::cli::Cli;
use reth_chainspec::{ChainSpec, DEV, HOLESKY, STARDUST};
use reth_db::test_utils::{create_test_rw_db, TempDatabase};
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_builder::{engine_tree_config::{
    TreeConfig, DEFAULT_MEMORY_BLOCK_BUFFER_TARGET, DEFAULT_PERSISTENCE_THRESHOLD,
}, EngineNodeLauncher, NodeBuilder, NodeConfig, NodeTypesWithDBAdapter};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_provider::providers::BlockchainProvider2;
use reth_tasks::TaskManager;
use reth_tracing::tracing::warn;
use tracing::info;
use reth_db::{tables, DatabaseEnv};
use reth_db_api::cursor::DbCursorRO;
use reth_db_api::Database;
use reth_db_api::transaction::DbTx;
use std::fs;


/// Parameters for configuring the engine
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "Engine")]
pub struct EngineArgs {
    /// Enable the experimental engine features on reth binary
    ///
    /// DEPRECATED: experimental engine is default now, use --engine.legacy to enable the legacy
    /// functionality
    #[arg(long = "engine.experimental", default_value = "false")]
    pub experimental: bool,

    /// Enable the legacy engine on reth binary
    #[arg(long = "engine.legacy", default_value = "false")]
    pub legacy: bool,

    /// Configure persistence threshold for engine experimental.
    #[arg(long = "engine.persistence-threshold", requires = "experimental", default_value_t = DEFAULT_PERSISTENCE_THRESHOLD)]
    pub persistence_threshold: u64,

    /// Configure the target number of blocks to keep in memory.
    #[arg(long = "engine.memory-block-buffer-target", requires = "experimental", default_value_t = DEFAULT_MEMORY_BLOCK_BUFFER_TARGET)]
    pub memory_block_buffer_target: u64,
}

impl Default for EngineArgs {
    fn default() -> Self {
        Self {
            experimental: false,
            legacy: false,
            persistence_threshold: DEFAULT_PERSISTENCE_THRESHOLD,
            memory_block_buffer_target: DEFAULT_MEMORY_BLOCK_BUFFER_TARGET,
        }
    }
}

fn initialize_clean_db(config: &NodeConfig<ChainSpec>) -> () {
    let data_dir = config.datadir();
    let data= data_dir.data_dir();
    if data.exists() {
        fs::remove_dir_all(&data).expect("Failed to remove data directory");
    }

}




#[tokio::main]
async fn main() -> eyre::Result<()>{

    let config = NodeConfig::new(STARDUST.clone());

    let config2 = config.clone();
    let builder = NodeBuilder::new(config);

    let flag = reth_db::init_db(config2.datadir().data_dir(), builder.config().db.database_args()).unwrap();

    println!("셍성!");

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }


    reth_tracing::init_test_tracing();
    let tasks = TaskManager::current();
    let exec = tasks.executor();

    let engine_tree_config = TreeConfig::default()
        .with_persistence_threshold(DEFAULT_PERSISTENCE_THRESHOLD)
        .with_memory_block_buffer_target(DEFAULT_MEMORY_BLOCK_BUFFER_TARGET);



    println!("{:?}",reth_db::is_database_empty(config2.datadir().data_dir()));
    println!("{:?}", config2.datadir().data_dir());
;

    let db2 = Arc::new(flag);

   println!("{:?}",builder.config().datadir());




   let handle = builder
       .with_database(db2.clone())
       .with_launch_context(exec)
       .with_types_and_provider::<EthereumNode,BlockchainProvider2<_>>()
       .with_components(EthereumNode::components())
       .with_add_ons(EthereumAddOns::default())
       .launch_with_fn(|builder| {
           let launcher = EngineNodeLauncher::new(
               builder.task_executor().clone(),
               builder.config().datadir(),
               engine_tree_config,
           );
           builder.launch_with(launcher)
       }
       ).await?;


    let tx = db2.clone().tx().expect("Failed to start transaction");

    println!("\n=== Headers ===");
    let mut headers_cursor = tx.cursor_read::<tables::Headers>().expect("Failed to create headers cursor");
    while let Some((key, value)) = headers_cursor.next().expect("Failed to read next header") {
        println!("Block Number: {}, Header: {:?}", key, value);
    }

    println!("\n=== Transactions ===");
    let mut tx_cursor = tx.cursor_read::<tables::Transactions>().expect("Failed to create tx cursor");
    while let Some((key, value)) = tx_cursor.next().expect("Failed to read next transaction") {
        println!("Transaction ID: {}, Transaction: {:?}", key, value);
    }

    println!("\n=== Accounts ===");
    let mut accounts_cursor = tx.cursor_read::<tables::PlainAccountState>().expect("Failed to create accounts cursor");
    while let Some((key, value)) = accounts_cursor.next().expect("Failed to read next account") {
        println!("Account: {:?}, State: {:?}", key, value);
    }

    println!("\n=== Storage ===");
    let mut storage_cursor = tx.cursor_read::<tables::PlainStorageState>().expect("Failed to create storage cursor");
    while let Some((key, value)) = storage_cursor.next().expect("Failed to read next storage entry") {
        println!("Storage Key: {:?}, Value: {:?}", key, value);
    }

    println!("\n=== Blocks ===");
    let mut blocks_cursor = tx.cursor_read::<tables::CanonicalHeaders>().expect("Failed to create blocks cursor");
    while let Some((key, value)) = blocks_cursor.next().expect("Failed to read next block") {
        println!("Block Number: {}, Hash: {:?}", key, value);
    }

    println!("\n=== Receipts ===");
    let mut receipts_cursor = tx.cursor_read::<tables::Receipts>().expect("Failed to create receipts cursor");
    while let Some((key, value)) = receipts_cursor.next().expect("Failed to read next receipt") {
        println!("Transaction Number: {}, Receipt: {:?}", key, value);
    }





    match tokio::signal::ctrl_c().await {
        Ok(()) => {
            println!("Ctrl+C 감지됨, 종료 시작...");
            println!("{:?}", handle);
            // 필요한 정리 작업을 여기서 수행
            Ok(())
        }
        Err(err) => {
            eprintln!("Ctrl+C 시그널을 처리하는 중 에러 발생: {}", err);
            Ok(())
        }
    }



}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_parse_engine_args() {
        let default_args = EngineArgs::default();
        let args = CommandParser::<EngineArgs>::parse_from(["reth"]).args;
        assert_eq!(args, default_args);
    }
}
