use std::process::ExitCode;

use sourcelens::config::Config;
use sourcelens::http;
use sourcelens::index::jobs;
use sourcelens::state::AppState;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg = Config::from_env()?;
    let state = AppState::new(cfg)?;

    let owner_exists = state
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM users WHERE role = 'owner')",
                [],
                |row| row.get::<_, bool>(0),
            )
        })
        .await?;
    if !owner_exists {
        tracing::info!(
            "Setup token: {} — open {}/setup",
            state.setup_token,
            state.cfg.public_url
        );
    }

    if let Err(err) = jobs::recover_and_reconcile(&state).await {
        tracing::error!("startup reconcile failed: {err}");
    }
    jobs::spawn_workers(state.clone());

    let listener = tokio::net::TcpListener::bind(&state.cfg.bind).await?;
    tracing::info!("listening on {} ({})", state.cfg.bind, state.cfg.public_url);
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("cannot listen for Ctrl-C: {err}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::error!("cannot listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
