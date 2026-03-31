use clap::{Parser, Subcommand};
use std::error::Error;
use std::path::PathBuf;
use url::Url;
use zkscraper::{fetch, gather, maps, parse, pipeline};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of zero-k.info
    #[arg(short = 'u', long, default_value = "https://zero-k.info")]
    site_url: String,

    /// Minimum time to wait between each set of scrape requests, in milliseconds
    #[arg(short = 't', long, default_value_t = 1_000)]
    min_req_wait: u32,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Gather battle IDs. First task of scraping
    #[command(name = "gather-battle-ids")]
    GatherBIDs {
        /// The initial offset to gather from
        #[arg(long, default_value_t = 0)]
        initial_offset: u32,

        /// Minimum number of battle IDs to gather
        #[arg(long, default_value_t = 100)]
        gather_num: u32,

        /// Optional path to Zero-K portable, used to guess existing map archives from maps/
        #[arg(long)]
        zk_path: Option<PathBuf>,

        /// Output (.csv)
        #[arg(long)]
        out: PathBuf,
    },

    /// Download replays from a list of battle IDs. Second task
    #[command(name = "download-replays")]
    DownloadReplays {
        /// Location of the array of battle IDs (.csv)
        #[arg(long)]
        battle_ids: PathBuf,

        /// Output path for raw replays (.sdfz)
        #[arg(long)]
        sdfz_path: PathBuf,
    },

    /// Download maps needed by a list of battle IDs
    #[command(name = "download-maps")]
    DownloadMaps {
        /// Location of the array of battle IDs (.csv)
        #[arg(long)]
        battle_ids: PathBuf,

        /// Path to Zero-K portable
        #[arg(long)]
        zk_path: PathBuf,
    },

    /// Run gather, map download, replay download, and parsing as one end-to-end pipeline
    #[command(name = "pipeline")]
    Pipeline {
        /// The initial offset to gather from
        #[arg(long, default_value_t = 0)]
        initial_offset: u32,

        /// Minimum number of battle IDs to gather
        #[arg(long, default_value_t = 100)]
        gather_num: u32,

        /// Path to Zero-K portable
        #[arg(long)]
        zk_path: PathBuf,

        /// Output sled DB directory for parsed replays
        #[arg(long)]
        out: PathBuf,

        /// Optional temp directory for intermediate CSV, replay, and working DB files
        #[arg(long)]
        temp: Option<PathBuf>,
    },

    /// Parse replays and produces snapshots. Takes a long time. Final task
    #[command(name = "parse-replays")]
    ParseReplays {
        /// Input directory for raw replays (.sdfz)
        #[arg(long)]
        sdfz_in: PathBuf,

        /// Path to Zero-K portable
        #[arg(long)]
        zk_path: PathBuf,

        /// Output sled DB directory for parsed replays
        #[arg(long)]
        snapshot_path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let site_url = Url::parse(&args.site_url)?;

    match args.command {
        Commands::GatherBIDs {
            initial_offset,
            gather_num,
            zk_path,
            out,
        } => {
            gather::gather_battle_ids(gather::GatherBIDSettings {
                site_url,
                initial_offset,
                gather_num,
                min_req_wait: args.min_req_wait,
                out_path: out,
                zk_path,
                gather_filter: gather::GatherFilterSettings::default(),
            })
            .await
        }
        Commands::DownloadReplays {
            battle_ids,
            sdfz_path,
        } => {
            fetch::fetch_replays(fetch::FetchReplaySettings {
                site_url,
                csv_path: battle_ids,
                min_req_wait: args.min_req_wait,
                out_path: sdfz_path,
            })
            .await
        }
        Commands::DownloadMaps {
            battle_ids,
            zk_path,
        } => {
            maps::download_maps(maps::DownloadMapsSettings {
                site_url,
                csv_path: battle_ids,
                min_req_wait: args.min_req_wait,
                zk_path,
            })
            .await
        }
        Commands::Pipeline {
            initial_offset,
            gather_num,
            zk_path,
            out,
            temp,
        } => {
            pipeline::run_pipeline(pipeline::PipelineSettings {
                site_url,
                min_req_wait: args.min_req_wait,
                initial_offset,
                gather_num,
                zk_path,
                out_path: out,
                temp_root: temp,
            })
            .await
        }
        Commands::ParseReplays {
            sdfz_in,
            zk_path,
            snapshot_path,
        } => {
            parse::parse_replays(parse::ParseReplaySettings {
                sdfz_in,
                zk_path,
                snapshot_path,
            })
            .await
        }
    }
}
