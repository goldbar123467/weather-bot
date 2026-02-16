use crate::core::types::Config;
use crate::storage;
use std::fs;
use std::io::Write;
use std::process;

pub struct Lockfile {
    path: String,
}

impl Lockfile {
    pub fn acquire(path: &str) -> anyhow::Result<Self> {
        if let Ok(contents) = fs::read_to_string(path) {
            let pid: u32 = contents.trim().parse().unwrap_or(0);
            if pid > 0 && std::path::Path::new(&format!("/proc/{}", pid)).exists() {
                anyhow::bail!("Another instance running (PID {})", pid);
            }
            tracing::warn!("Removing stale lockfile (PID {} dead)", pid);
        }

        let mut f = fs::File::create(path)?;
        write!(f, "{}", process::id())?;
        Ok(Self {
            path: path.to_string(),
        })
    }
}

impl Drop for Lockfile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn validate_startup(config: &Config) -> anyhow::Result<()> {
    if config.kalshi_private_key_pem.is_empty() {
        anyhow::bail!("KALSHI_PRIVATE_KEY_PATH is empty or file not found");
    }
    if !config.kalshi_private_key_pem.contains("BEGIN") {
        anyhow::bail!("PEM file doesn't look like a private key");
    }

    if config.cities.is_empty() {
        anyhow::bail!("No cities configured — check CITIES env var");
    }

    if config.kalshi_key_id.is_empty() {
        anyhow::bail!("KALSHI_API_KEY_ID not set");
    }

    if !std::path::Path::new("brain/ledger.md").exists() {
        anyhow::bail!("brain/ledger.md not found");
    }
    storage::read_ledger()?;

    if !std::path::Path::new("brain/prompt.md").exists() {
        anyhow::bail!("brain/prompt.md not found");
    }

    if !config.paper_trade && !config.confirm_live {
        anyhow::bail!(
            "PAPER_TRADE=false but CONFIRM_LIVE is not true. \
             Set CONFIRM_LIVE=true to acknowledge real money trading."
        );
    }

    if !config.paper_trade {
        tracing::warn!("LIVE TRADING ENABLED — real money at risk");
    }

    Ok(())
}
