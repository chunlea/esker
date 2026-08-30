//! The SQL node: listen, and speak PostgreSQL.
//!
//! The executor lands in unit 6 of `docs/plans/phase-6a.md`, so every statement is answered with
//! `0A000 feature_not_supported` naming what it was. That is contract C2 working exactly as
//! intended rather than a placeholder: a client connects, gets a prompt, and is told the truth
//! about what this node can do — never a crash, never a syntax error about valid SQL, and never a
//! wrong answer.

use std::sync::Arc;

use esker_sql::pgwire::server::{Auth, Config, Executors, NotYetExecuting, serve};
use esker_sql::pgwire::session::Execute;

/// Hands every session the placeholder executor.
struct Sessions;

impl Executors for Sessions {
    fn for_session(&self) -> Box<dyn Execute + Send> {
        Box::new(NotYetExecuting)
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5432".to_owned());
    let config = Config {
        address,
        auth: Auth::Trust,
        ..Config::default()
    };
    serve(config, Arc::new(Sessions)).await
}
