//! oLLM binary — thin entry point. All logic lives in `ollm` (lib.rs).
//! T0: this file is the thin main the testability refactor promised.

use ollm::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env();
    ollm::serve(cfg).await
}
