mod ai;
mod config;
mod convo_cache;
mod handler;
mod history;
mod images;
mod message;
mod names;
mod recipient;

use futures::{channel::oneshot, future};
use tracing::{error, info, warn};

use presage::libsignal_service::configuration::SignalServers;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::store::StateStore;
use presage::Manager;
use presage_store_sqlite::SqliteStore;

use crate::config::Config;
use crate::handler::{run_loop, Outcome};

async fn open_store(db_path: &str) -> anyhow::Result<SqliteStore> {
    // left unencrypted: a passphrase passed via env would sit right next to the
    // data it protects, so securing the volume is left to the host
    Ok(SqliteStore::open_with_passphrase(db_path, None, OnNewIdentity::Trust).await?)
}

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

async fn run(db_path: String, cfg: Config, device_name: String) -> anyhow::Result<()> {
    loop {
        let store = open_store(&db_path).await?;
        let manager = if store.is_registered().await {
            Manager::load_registered(store).await?
        } else {
            link(store, device_name.clone()).await?
        };

        match run_loop(manager, &cfg).await? {
            Outcome::Done => return Ok(()),
            Outcome::Relink => {
                warn!("device was unlinked; clearing registration to re-link");
                let mut store = open_store(&db_path).await?;
                store.clear_registration().await?;
            }
        }
    }
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

    let db_path = "/data/store.db3".to_string();
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let device_name = std::env::var("DEVICE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "signal-ai-bot".to_string());

    // presage's Manager is !Send, so it has to run on a LocalSet
    let local = tokio::task::LocalSet::new();
    local.run_until(run(db_path, cfg, device_name)).await
}
