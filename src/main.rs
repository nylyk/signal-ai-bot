mod ai;
mod config;
mod handler;
mod history;
mod images;
mod message;
mod names;
mod recipient;
mod replies;

use futures::{channel::oneshot, future};
use tracing::{error, info};

use presage::libsignal_service::configuration::SignalServers;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::store::StateStore;
use presage::Manager;
use presage_store_sqlite::SqliteStore;

use crate::config::Config;
use crate::handler::run_loop;
use crate::replies::AiReplies;

// no account is linked yet: print a qr code and wait for the phone to scan it,
// then return the freshly-linked manager so the bot can start normally.
async fn link(
    store: SqliteStore,
    device_name: String,
) -> anyhow::Result<Manager<SqliteStore, Registered>> {
    let (tx, rx) = oneshot::channel();
    let (manager, _) = future::join(
        Manager::link_secondary_device(store, SignalServers::Production, device_name, tx),
        async move {
            match rx.await {
                Ok(url) => {
                    println!("no account linked yet — scan this qr code with signal on your phone");
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
    info!("linked. account: {whoami:?}");
    Ok(manager)
}

async fn run(
    store: SqliteStore,
    db_path: String,
    cfg: Config,
    device_name: String,
) -> anyhow::Result<()> {
    // link on first run (shows the qr code), otherwise just load and go
    let manager = if store.is_registered().await {
        Manager::load_registered(store).await?
    } else {
        link(store, device_name).await?
    };

    let replies_path = std::path::Path::new(&db_path).with_file_name("ai_replies.json");
    let replies = AiReplies::load(replies_path);
    run_loop(manager, cfg, replies).await
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

    let cfg = Config::from_env()?;

    // linking state lives on the mounted /data volume
    let db_path = "/data/store.db3".to_string();
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let device_name = std::env::var("DEVICE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "signal-ai-bot".to_string());

    // store is left unencrypted: a passphrase passed via env would sit right
    // next to the data it protects, so it's left to the host/volume to secure.
    let store = SqliteStore::open_with_passphrase(&db_path, None, OnNewIdentity::Trust).await?;

    // presage's Manager is !Send, so it has to run on a LocalSet
    let local = tokio::task::LocalSet::new();
    local
        .run_until(run(store, db_path, cfg, device_name))
        .await
}
