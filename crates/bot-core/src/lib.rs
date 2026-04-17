pub mod account;
pub mod config;
pub mod data;
pub mod execution;
pub mod notifications;
pub mod orchestrator;
pub mod settlement;
pub mod storage;
pub mod types;
pub mod utils;

use std::path::Path;

use anyhow::Result;

use crate::config::load_config;

pub async fn run(root_dir: &Path, argv: &[String]) -> Result<()> {
    let config = load_config(argv, root_dir)?;
    orchestrator::run_orchestrator(config).await
}
