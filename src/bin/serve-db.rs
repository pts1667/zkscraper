use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use zkscraper::{
    db::ReplayDb,
    map_assets::MapService,
    server::{serve, DEFAULT_HOST, DEFAULT_PORT},
};

#[derive(Parser, Debug)]
#[command(version, about = "Serve a parsed replay sled DB over HTTP")]
struct Args {
    /// Path to the parsed replay sled DB
    #[arg(long)]
    db: PathBuf,

    /// Optional path to a Zero-K portable install, used to serve map archives from <zk-path>\\maps
    #[arg(long)]
    zk_path: Option<PathBuf>,

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
    let generated = db.build_summary_index().map_err(std::io::Error::other)?;
    let maps = args
        .zk_path
        .as_deref()
        .map(MapService::from_zk_path)
        .transpose()
        .map_err(std::io::Error::other)?;
    let bind_addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(std::io::Error::other)?;

    println!("Summary index generated for {generated} replay(s)");
    println!("Serving replay DB on http://{bind_addr}");
    println!("OpenAPI JSON: http://{bind_addr}/openapi.json");
    println!("Docs: http://{bind_addr}/docs");
    if maps.is_some() {
        println!("Map heightmaps: http://{bind_addr}/maps/<map_name>/heightmap.bmp");
        println!("Map features: http://{bind_addr}/maps/<map_name>/features");
    }

    serve(db, maps, bind_addr).await
}
