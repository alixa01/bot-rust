use std::path::PathBuf;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn project_root() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("BOT_RUST_ROOT") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    Ok(std::env::current_dir()?)
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .without_time()
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let root_dir = project_root()?;
    let dotenv_path = root_dir.join(".env");

    if dotenv_path.exists() {
        dotenvy::from_path(&dotenv_path)?;
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    bot_core::run(&root_dir, &args).await
}
