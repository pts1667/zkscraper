use std::{
    collections::BTreeMap,
    error::Error,
    fmt::Display,
    path::{Path, PathBuf},
    time::Duration,
};

use indicatif::ProgressBar;
use regex::Regex;
use tokio::{fs::File, io::AsyncWriteExt};
use url::Url;

use crate::http::RateLimitedHttpClient;

pub struct FetchReplaySettings {
    pub site_url: Url,
    pub csv_path: PathBuf,
    pub min_req_wait: u32,
    pub out_path: PathBuf,
}

const MANIFEST_FILENAME: &str = "replay_manifest.csv";

#[derive(Clone)]
struct ManifestEntry {
    replay_id: String,
    battle_id: u64,
    replay_filename: String,
    game_version: String,
}

#[derive(Debug)]
pub enum FetchReplayError {
    NameParseError,
}

impl Display for FetchReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for FetchReplayError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }

    fn description(&self) -> &str {
        "could not parse replay page"
    }

    fn cause(&self) -> Option<&dyn Error> {
        self.source()
    }
}

fn read_manifest(path: &Path) -> Result<BTreeMap<u64, ManifestEntry>, Box<dyn std::error::Error>> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }

    let mut reader = csv::Reader::from_path(path)?;
    let mut entries = BTreeMap::new();
    for record in reader.records() {
        let record = record?;
        let (replay_id, battle_id, replay_filename, game_version) = if record.len() >= 5 {
            let Some(replay_id) = record.get(0) else {
                continue;
            };
            let Some(battle_id) = record.get(1) else {
                continue;
            };
            let Some(replay_filename) = record.get(3) else {
                continue;
            };
            (
                replay_id.to_string(),
                battle_id,
                replay_filename,
                record.get(4).unwrap_or_default().to_string(),
            )
        } else {
            let Some(battle_id) = record.get(0) else {
                continue;
            };
            let Some(replay_filename) = record.get(1) else {
                continue;
            };
            (
                battle_id.to_string(),
                battle_id,
                replay_filename,
                record.get(2).unwrap_or_default().to_string(),
            )
        };

        entries.insert(
            battle_id.parse()?,
            ManifestEntry {
                replay_id,
                battle_id: battle_id.parse()?,
                replay_filename: replay_filename.to_string(),
                game_version,
            },
        );
    }

    Ok(entries)
}

fn write_manifest(
    path: &Path,
    entries: impl IntoIterator<Item = ManifestEntry>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record([
        "replay_id",
        "battle_id",
        "headless_id",
        "replay_filename",
        "game_version",
    ])?;
    for entry in entries {
        writer.write_record([
            entry.replay_id,
            entry.battle_id.to_string(),
            entry.battle_id.to_string(),
            entry.replay_filename,
            entry.game_version,
        ])?;
    }
    writer.flush()?;
    Ok(())
}

pub async fn fetch_replay(
    battle_id: u64,
    client: &RateLimitedHttpClient,
    settings: &FetchReplaySettings,
) -> Result<String, Box<dyn std::error::Error>> {
    let req_url_end = "Battles/Detail/";
    let dl_url_end = "replays/";
    let req_url = settings
        .site_url
        .join(req_url_end)?
        .join(&battle_id.to_string())?;

    let re_name = Regex::new(r"<a href=\'\/replays\/(.+?)[.]sdfz")?;

    let body = client
        .send(client.raw().get(req_url))
        .await?
        .bytes()
        .await?
        .to_vec();

    let body_s = String::from_utf8(body)?;
    let replay_name = if let Some(cap) = re_name.captures(&body_s) {
        let (_full, [replay_name]) = cap.extract();
        Ok(format!("{replay_name}.sdfz"))
    } else {
        Err(FetchReplayError::NameParseError)
    }?;

    let replay_dl_url = settings.site_url.join(dl_url_end)?.join(&replay_name)?;

    let replay_path = settings.out_path.join(replay_name);
    // we don't really handle corrupted downloads etc. Shouldn't be necessary
    if !replay_path.is_file() {
        let dl = client
            .send(client.raw().get(replay_dl_url))
            .await?
            .bytes()
            .await?;

        let mut out_file = File::create(replay_path.clone()).await?;
        out_file.write_all(&dl).await?;
    }

    Ok(replay_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string())
}

pub async fn fetch_replays(
    settings: FetchReplaySettings,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&settings.out_path)?;

    let mut battle_ids_csv = csv::Reader::from_path(settings.csv_path.clone())?;
    let battle_rows: Vec<(u64, String)> = battle_ids_csv
        .records()
        .filter_map(|r| r.ok())
        .filter_map(|r| {
            let bid = r.get(0)?.parse::<u64>().ok()?;
            let version = r.get(1)?.to_string();
            Some((bid, version))
        })
        .collect();

    let manifest_path = settings.out_path.join(MANIFEST_FILENAME);
    let mut manifest_entries = read_manifest(&manifest_path)?;
    let client = RateLimitedHttpClient::new(Duration::from_millis(settings.min_req_wait as u64));
    let pb = ProgressBar::new(battle_rows.len() as u64);
    for (battle_id, game_version) in battle_rows {
        let replay_filename = fetch_replay(battle_id, &client, &settings).await?;
        manifest_entries.insert(
            battle_id,
            ManifestEntry {
                replay_id: battle_id.to_string(),
                battle_id,
                replay_filename,
                game_version,
            },
        );
        pb.inc(1);
    }

    write_manifest(&manifest_path, manifest_entries.into_values())?;
    Ok(())
}
