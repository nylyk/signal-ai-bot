use clap::{Parser, Subcommand};
use presage::libsignal_service::configuration::SignalServers;

#[derive(Parser)]
#[clap(about = "signal client that runs an ai chatbot in your chats via @ai")]
pub struct Args {
    #[clap(long = "sqlite-db-path")]
    pub sqlite_db_path: Option<String>,
    #[clap(
        long = "passphrase",
        short = 'p',
        help = "passphrase to encrypt local storage"
    )]
    pub passphrase: Option<String>,
    #[clap(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    #[clap(about = "link this client to your phone by scanning a qr code")]
    Link {
        #[clap(long, short = 's', default_value = "production")]
        servers: SignalServers,
        #[clap(long, short = 'n', default_value = "signal-ai-bot")]
        device_name: String,
    },
    #[clap(about = "receive messages and answer @ai prompts")]
    Run,
}
