use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use zkscraper::{
    db::ReplayDb,
    server::{serve, DEFAULT_HOST, DEFAULT_PORT},
};

#[derive(Parser, Debug)]
#[command(version, about = "Serve a parsed replay sled DB over HTTP")]
struct Args {
    /// Path to the parsed replay sled DB
    #[arg(long)]
    db: PathBuf,

    /// Host or IP address to bind to
    #[arg(long, default_value = DEFAULT_HOST)]
    host: String,

    /// TCP port to bind to
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let db = ReplayDb::open(&args.db).map_err(std::io::Error::other)?;
    let bind_addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(std::io::Error::other)?;

    println!("Serving replay DB on http://{bind_addr}");
    println!("OpenAPI JSON: http://{bind_addr}/openapi.json");
    println!("Docs: http://{bind_addr}/docs");

    serve(db, bind_addr).await
}
