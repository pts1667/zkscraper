use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use url::Url;
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

    /// URL of zero-k.info used for live replay append
    #[arg(short = 'u', long, default_value = "https://zero-k.info")]
    site_url: String,

    /// Minimum time to wait between scrape requests for live replay append, in milliseconds
    #[arg(short = 't', long, default_value_t = 1_000)]
    min_req_wait: u32,

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
    let maps = args
        .zk_path
        .as_deref()
        .map(MapService::from_zk_path)
        .transpose()
        .map_err(std::io::Error::other)?;
    let site_url = Url::parse(&args.site_url).map_err(std::io::Error::other)?;
    let bind_addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(std::io::Error::other)?;

    println!("Serving replay DB on http://{bind_addr}");
    println!("OpenAPI JSON: http://{bind_addr}/openapi.json");
    println!("Docs: http://{bind_addr}/docs");
    if maps.is_some() {
        println!("Map heightmaps: http://{bind_addr}/maps/<map_name>/heightmap.bmp");
        println!("Map features: http://{bind_addr}/maps/<map_name>/features");
    }
    if args.zk_path.is_some() {
        println!("Live append: POST http://{bind_addr}/replays/append");
    }

    serve(
        db,
        maps,
        site_url,
        args.min_req_wait,
        args.zk_path,
        bind_addr,
    )
    .await
}
