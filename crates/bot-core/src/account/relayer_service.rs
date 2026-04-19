use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::task::spawn_blocking;

use crate::types::{Config, SettlementTxMode};

pub fn should_use_relayer_for_settlement(config: &Config) -> bool {
    matches!(config.settlement_tx_mode, SettlementTxMode::RelayerSafe)
}

#[derive(Debug, Deserialize)]
struct RelayerHelperOutput {
    ok: bool,
    #[serde(rename = "txHash")]
    tx_hash: Option<String>,
    error: Option<String>,
}

fn is_bytes32(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.len() == 66
        && trimmed.starts_with("0x")
        && trimmed.chars().skip(2).all(|c| c.is_ascii_hexdigit())
}

fn push_unique(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if paths.iter().any(|existing| existing == &candidate) {
        return;
    }

    paths.push(candidate);
}

fn candidate_bot_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Ok(from_env) = env::var("BOT_RUST_ROOT") {
        let trimmed = from_env.trim();
        if !trimmed.is_empty() {
            push_unique(&mut roots, PathBuf::from(trimmed));
        }
    }

    if let Ok(cwd) = env::current_dir() {
        push_unique(&mut roots, cwd);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = manifest_dir.parent().and_then(|p| p.parent()) {
        push_unique(&mut roots, root.to_path_buf());
    }

    roots
}

fn resolve_helper_script() -> Result<PathBuf> {
    for root in candidate_bot_roots() {
        let candidate = root.join("scripts").join("relayer_redeem_safe.cjs");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "relayer helper script not found: scripts/relayer_redeem_safe.cjs (set BOT_RUST_ROOT if running outside project root)"
    )
}

fn resolve_local_node_modules(bot_root: &Path) -> Result<PathBuf> {
    let node_modules = bot_root.join("node_modules");
    if !node_modules.exists() {
        bail!(
            "bot-rust local node_modules not found at {}. Run `npm install` in bot-rust.",
            node_modules.display()
        );
    }

    let relayer_pkg = node_modules
        .join("@polymarket")
        .join("builder-relayer-client");
    if !relayer_pkg.exists() {
        bail!(
            "Missing @polymarket/builder-relayer-client in bot-rust/node_modules. Run `npm install` in bot-rust."
        );
    }

    let ethers_pkg = node_modules.join("ethers");
    if !ethers_pkg.exists() {
        bail!("Missing ethers in bot-rust/node_modules. Run `npm install` in bot-rust.");
    }

    Ok(node_modules)
}

fn parse_helper_output(stdout: &str) -> Option<RelayerHelperOutput> {
    for line in stdout.lines().rev() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }

        if let Ok(parsed) = serde_json::from_str::<RelayerHelperOutput>(trimmed) {
            return Some(parsed);
        }
    }

    None
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    value.chars().take(max_chars).collect()
}

fn build_helper_command(
    script_path: &Path,
    config: &Config,
    condition_id: &str,
) -> Result<Command> {
    let bot_root = script_path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("invalid helper script path"))?;

    let mut command = if cfg!(windows) {
        Command::new("node.exe")
    } else {
        Command::new("node")
    };

    command.current_dir(bot_root);
    command.arg(script_path);

    // Force local-only dependency resolution to keep bot-rust runtime standalone.
    let local_node_modules = resolve_local_node_modules(bot_root)?;
    command.env("NODE_PATH", local_node_modules);

    let relayer_api_key = config
        .relayer_api_key
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_owned();

    let relayer_api_key_address = config
        .relayer_api_key_address
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_owned();

    command.env("RELAYER_BASE_URL", config.relayer_base_url.trim());
    command.env("RELAYER_API_KEY", relayer_api_key);
    command.env("RELAYER_API_KEY_ADDRESS", relayer_api_key_address);
    command.env("PRIVATE_KEY", config.private_key.trim());
    command.env("POLYGON_RPC_URL", config.polygon_rpc_url.trim());
    command.env(
        "RELAYER_REQUEST_TIMEOUT_MS",
        config.relayer_request_timeout_ms.to_string(),
    );
    command.env(
        "RELAYER_POLL_INTERVAL_MS",
        config.relayer_poll_interval_ms.to_string(),
    );
    command.env("RELAYER_MAX_POLLS", config.relayer_max_polls.to_string());
    command.env("CTF_CONTRACT", config.ctf_contract.trim());
    command.env("USDC_E", config.usdc_address.trim());
    command.env("CONDITION_ID", condition_id.trim());

    Ok(command)
}

pub async fn relay_redeem_positions(config: &Config, condition_id: &str) -> Result<String> {
    let trimmed = condition_id.trim();
    if !is_bytes32(trimmed) {
        bail!("Invalid conditionId for relayer redeem: {trimmed}");
    }

    if config.private_key.trim().is_empty() {
        bail!("PRIVATE_KEY is required for relayer redeem");
    }

    if config
        .relayer_api_key
        .as_deref()
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        bail!("RELAYER_API_KEY is required for relayer redeem");
    }

    if config
        .relayer_api_key_address
        .as_deref()
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        bail!("RELAYER_API_KEY_ADDRESS is required for relayer redeem");
    }

    let script_path = resolve_helper_script()?;
    let mut command = build_helper_command(&script_path, config, trimmed)?;

    let output: Output = spawn_blocking(move || command.output())
        .await
        .context("failed to join relayer helper task")?
        .context("failed to execute relayer helper script")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let parsed = parse_helper_output(&stdout);

    if !output.status.success() {
        if let Some(payload) = parsed {
            bail!(
                "relayer helper failed: {}",
                payload
                    .error
                    .unwrap_or_else(|| "unknown helper error".to_owned())
            );
        }

        bail!(
            "relayer helper exited with status={} stdout={} stderr={}",
            output
                .status
                .code()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "signal".to_owned()),
            truncate_text(stdout.trim(), 220),
            truncate_text(stderr.trim(), 220)
        );
    }

    let payload = parsed.ok_or_else(|| {
        anyhow!(
            "relayer helper returned unparseable output: stdout={} stderr={}",
            truncate_text(stdout.trim(), 220),
            truncate_text(stderr.trim(), 220)
        )
    })?;

    if !payload.ok {
        bail!(
            "relayer helper reported error: {}",
            payload.error.unwrap_or_else(|| "unknown relayer error".to_owned())
        );
    }

    let tx_hash = payload
        .tx_hash
        .ok_or_else(|| anyhow!("relayer helper missing txHash"))?;

    if !is_bytes32(&tx_hash) {
        bail!("relayer helper returned invalid tx hash: {tx_hash}");
    }

    Ok(tx_hash)
}
