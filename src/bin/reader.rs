use clap::{Parser, Subcommand};
use std::path::PathBuf;
use zkscraper::db::{ReplayDb, ReplaySummary};

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
    let db = ReplayDb::open(args.db)?;

    match args.command {
        Command::List => {
            for battle_id in db.battle_ids() {
                println!("{battle_id}");
            }
        }
        Command::Show { battle_id } => {
            let battle_id_num = battle_id.parse::<u64>()?;
            let parsed = db
                .get_replay(battle_id_num)?
                .ok_or_else(|| format!("battle_id {} not found", battle_id))?;
            let summary = ReplaySummary::from(&parsed);
            let first_snapshot_units = parsed
                .global_snapshots
                .first()
                .map(|snapshot| snapshot.units.len())
                .unwrap_or(0);
            let last_event = parsed
                .events
                .last()
                .map(|event| event.event_type.as_str())
                .unwrap_or("<none>");
            let event_preview = parsed
                .events
                .iter()
                .take(6)
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let player_names = parsed
                .players
                .iter()
                .filter_map(|player| player.name.as_deref())
                .collect::<Vec<_>>()
                .join(", ");

            println!("battle_id: {}", parsed.battle_id);
            println!("replay_filename: {}", parsed.replay_filename);
            println!("game_version: {}", parsed.game_version);
            println!("engine_version: {}", parsed.engine_version);
            println!("map_name: {:?}", parsed.map_name);
            println!("game_name: {:?}", parsed.game_name);
            println!("players: {}", summary.players_count);
            println!("teams: {}", summary.teams_count);
            println!("map_size: {:?}", parsed.map_size);
            println!("global_snapshots: {}", summary.global_snapshots);
            println!(
                "allyteam_snapshot_streams: {}",
                summary.allyteam_snapshot_streams
            );
            println!(
                "allyteam_snapshot_frames: {}",
                summary.allyteam_snapshot_frames
            );
            println!("commands: {}", summary.commands);
            println!("events: {}", summary.events);
            println!(
                "first_snapshot_frame: {}",
                summary
                    .first_snapshot_frame
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            );
            println!(
                "last_snapshot_frame: {}",
                summary
                    .last_snapshot_frame
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            );
            println!("first_snapshot_units: {}", first_snapshot_units);
            println!("last_event_type: {}", last_event);
            println!("event_preview: {}", event_preview);
            println!("player_names: {}", player_names);
        }
    }

    Ok(())
}
