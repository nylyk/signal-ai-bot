mod ai;
mod cli;
mod config;
mod handler;
mod history;
mod images;
mod message;
mod names;
mod recipient;
mod replies;

use directories::ProjectDirs;
use futures::{channel::oneshot, future};
use tracing::error;

use presage::model::identity::OnNewIdentity;
use presage::Manager;
use presage_store_sqlite::SqliteStore;

use crate::cli::{Args, Cmd};
use crate::config::Config;
use crate::handler::run_loop;
use crate::replies::AiReplies;

use clap::Parser;

async fn run(args: Args, store: SqliteStore, db_path: String) -> anyhow::Result<()> {
    match args.cmd {
        Cmd::Link {
            servers,
            device_name,
        } => {
            let (tx, rx) = oneshot::channel();
            let (manager, _) = future::join(
                Manager::link_secondary_device(store, servers, device_name, tx),
                async move {
                    match rx.await {
                        Ok(url) => {
                            println!("scan this qr code with signal on your phone");
                            println!("(settings -> linked devices -> link new device):\n");
                            qr2term::print_qr(url.to_string()).expect("failed to render qr");
                            println!("\nor open this url manually:\n{url}");
                        }
                        Err(e) => error!(%e, "linking cancelled"),
                    }
                },
            )
            .await;

            let manager = manager?;
            let whoami = manager.whoami().await?;
            println!("linked. account: {whoami:?}");
        }
        Cmd::Run => {
            let cfg = Config::from_env()?;
            let replies_path = std::path::Path::new(&db_path).with_file_name("ai_replies.json");
            let replies = AiReplies::load(replies_path);
            let manager = Manager::load_registered(store).await?;
            run_loop(manager, cfg, replies).await?;
        }
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::metadata::LevelFilter::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=error".parse().unwrap());
    tracing_subscriber::fmt::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    let args = Args::parse();

    let db_path = args.sqlite_db_path.clone().unwrap_or_else(|| {
        ProjectDirs::from("org", "whisperfish", "signal-ai-bot")
            .unwrap()
            .config_dir()
            .join("store.db3")
            .display()
            .to_string()
    });
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = SqliteStore::open_with_passphrase(
        &db_path,
        args.passphrase.as_deref(),
        OnNewIdentity::Trust,
    )
    .await?;

    // presage's Manager is !Send, so it has to run on a LocalSet
    let local = tokio::task::LocalSet::new();
    local.run_until(run(args, store, db_path)).await
}
