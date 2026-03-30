use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about = "Inspect parsed replay records from the sled DB")]
struct Args {
    /// Path to the parsed replay sled DB
    #[arg(long)]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List stored replay keys
    List,

    /// Show a compact summary for one replay
    Show {
        /// Battle ID key
        #[arg(long)]
        battle_id: String,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let db = sled::open(args.db)?;

    match args.command {
        Command::List => {
            for entry in db.iter() {
                let (key, _) = entry?;
                println!("{}", String::from_utf8_lossy(&key));
            }
        }
        Command::Show { battle_id } => {
            let value = db
                .get(battle_id.as_bytes())?
                .ok_or_else(|| format!("battle_id {} not found", battle_id))?;
            let decompressed = zstd::decode_all(value.as_ref())?;
            let parsed: Value = serde_json::from_slice(&decompressed)?;

            let global_snapshots = parsed["global_snapshots"]
                .as_array()
                .map_or(0, |items| items.len());
            let allyteam_snapshot_streams = parsed["allyteam_snapshots"]
                .as_object()
                .map_or(0, |items| items.len());
            let allyteam_snapshot_frames: usize = parsed["allyteam_snapshots"]
                .as_object()
                .map(|items| {
                    items.values()
                        .map(|value| value.as_array().map_or(0, |frames| frames.len()))
                        .sum()
                })
                .unwrap_or(0);
            let commands = parsed["command_history"]
                .as_array()
                .map_or(0, |items| items.len());
            let events = parsed["events"].as_array().map_or(0, |items| items.len());
            let first_snapshot_units = parsed["global_snapshots"]
                .as_array()
                .and_then(|items| items.first())
                .and_then(|snapshot| snapshot["units"].as_array())
                .map_or(0, |items| items.len());
            let first_snapshot_frame = parsed["global_snapshots"]
                .as_array()
                .and_then(|items| items.first())
                .map(|snapshot| snapshot["frame"].to_string())
                .unwrap_or_else(|| "<none>".to_string());
            let last_snapshot_frame = parsed["global_snapshots"]
                .as_array()
                .and_then(|items| items.last())
                .map(|snapshot| snapshot["frame"].to_string())
                .unwrap_or_else(|| "<none>".to_string());
            let last_event = parsed["events"]
                .as_array()
                .and_then(|items| items.last())
                .and_then(|event| event["event_type"].as_str())
                .unwrap_or("<none>");
            let event_preview = parsed["events"]
                .as_array()
                .map(|items| {
                    items.iter()
                        .take(6)
                        .filter_map(|event| event["event_type"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();

            println!("battle_id: {}", parsed["battle_id"]);
            println!("replay_filename: {}", parsed["replay_filename"]);
            println!("game_version: {}", parsed["game_version"]);
            println!("engine_version: {}", parsed["engine_version"]);
            println!("map_name: {}", parsed["map_name"]);
            println!("game_name: {}", parsed["game_name"]);
            println!(
                "players: {}",
                parsed["players"].as_array().map_or(0, |items| items.len())
            );
            println!(
                "teams: {}",
                parsed["teams"].as_array().map_or(0, |items| items.len())
            );
            println!("map_size: {}", parsed["map_size"]);
            println!("global_snapshots: {}", global_snapshots);
            println!("allyteam_snapshot_streams: {}", allyteam_snapshot_streams);
            println!("allyteam_snapshot_frames: {}", allyteam_snapshot_frames);
            println!("commands: {}", commands);
            println!("events: {}", events);
            println!("first_snapshot_frame: {}", first_snapshot_frame);
            println!("last_snapshot_frame: {}", last_snapshot_frame);
            println!("first_snapshot_units: {}", first_snapshot_units);
            println!("last_event_type: {}", last_event);
            println!("event_preview: {}", event_preview);

            if let Some(player_names) = parsed["players"].as_array() {
                let names = player_names
                    .iter()
                    .filter_map(|player| player["name"].as_str())
                    .collect::<Vec<_>>();
                println!("player_names: {}", names.join(", "));
            }
        }
    }

    Ok(())
}
