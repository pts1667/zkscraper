use indicatif::ProgressBar;
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use tokio::time::Duration;
use url::Url;

use crate::http::RateLimitedHttpClient;
use crate::maps::resolve_map_archive_name_from_battle;

pub struct GatherFilterSettings {
    pub title: Option<String>,
    pub map: Option<String>,
    pub players_from: Option<u32>,
    pub players_to: Option<u32>,
    pub age: Option<u32>,
    pub min_length: Option<u32>,
    pub max_length: Option<u32>,
    pub mission: Option<bool>,
    pub bots: Option<bool>,
    pub rank: Option<u32>,
    pub victory: Option<bool>,
    pub matchmaker: Option<bool>,
    pub rating: Option<u32>,
}

impl Default for GatherFilterSettings {
    fn default() -> Self {
        GatherFilterSettings {
            title: None,
            map: None,
            players_from: None,
            players_to: None,
            age: None,
            min_length: None,
            max_length: None,
            mission: None,
            bots: None,
            rank: None,
            victory: None,
            matchmaker: None,
            rating: None,
        }
    }
}

pub struct GatherBIDSettings {
    pub site_url: Url,
    pub initial_offset: u32,
    pub gather_num: u32,
    pub min_req_wait: u32,
    pub out_path: PathBuf,
    pub zk_path: Option<PathBuf>,
    pub gather_filter: GatherFilterSettings,
    pub explicit_battle_ids: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatheredBattleRow {
    pub battle_id: u64,
    pub version: String,
    pub map_file: String,
    pub map_extension: String,
}

impl GatheredBattleRow {
    fn csv_record(&self) -> [String; 4] {
        [
            self.battle_id.to_string(),
            self.version.clone(),
            self.map_file.clone(),
            self.map_extension.clone(),
        ]
    }
}

#[derive(Clone)]
struct LocalMapArchive {
    stem: String,
    extension: String,
    folded_stem: String,
}

fn fold_map_name(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn load_local_map_archives(
    zk_path: Option<&Path>,
) -> Result<Vec<LocalMapArchive>, Box<dyn std::error::Error>> {
    let Some(zk_path) = zk_path else {
        return Ok(Vec::new());
    };

    let maps_dir = zk_path.join("maps");
    if !maps_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut archives = Vec::new();
    for entry in std::fs::read_dir(maps_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if extension != "sd7" && extension != "sdz" {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        archives.push(LocalMapArchive {
            stem: stem.to_string(),
            extension: extension.to_string(),
            folded_stem: fold_map_name(stem),
        });
    }

    Ok(archives)
}

fn guess_local_map_archive(
    local_maps: &[LocalMapArchive],
    map_key: &str,
) -> Option<(String, String)> {
    let folded_key = fold_map_name(map_key);
    if folded_key.is_empty() {
        return None;
    }

    let exact_matches: Vec<_> = local_maps
        .iter()
        .filter(|archive| archive.folded_stem == folded_key)
        .collect();
    if exact_matches.len() == 1 {
        let archive = exact_matches[0];
        return Some((archive.stem.clone(), archive.extension.clone()));
    }

    let prefix_matches: Vec<_> = local_maps
        .iter()
        .filter(|archive| {
            archive.folded_stem.starts_with(&folded_key)
                || folded_key.starts_with(&archive.folded_stem)
        })
        .collect();
    if prefix_matches.len() == 1 {
        let archive = prefix_matches[0];
        return Some((archive.stem.clone(), archive.extension.clone()));
    }

    None
}

pub fn write_gathered_battle_csv(
    out_path: &Path,
    rows: &[GatheredBattleRow],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = csv::Writer::from_path(out_path)?;
    writer.write_record(["Battle ID", "Version", "Map File", "Map Extension"])?;
    for row in rows {
        writer.write_record(row.csv_record())?;
    }
    writer.flush()?;
    Ok(())
}

pub async fn gather_battle_ids(
    settings: GatherBIDSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let (rows, failures) = if settings.explicit_battle_ids.is_empty() {
        (
            gather_battle_rows_from_listing(&settings).await?,
            Vec::<String>::new(),
        )
    } else {
        gather_battle_rows_for_ids(
            settings.site_url.clone(),
            settings.min_req_wait,
            settings.zk_path.as_deref(),
            &settings.explicit_battle_ids,
        )
        .await?
    };

    write_gathered_battle_csv(&settings.out_path, &rows)?;

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to resolve {} explicit battle(s): {}",
            failures.len(),
            failures.join(", ")
        )
        .into())
    }
}

pub async fn gather_battle_rows_for_ids(
    site_url: Url,
    min_req_wait: u32,
    zk_path: Option<&Path>,
    battle_ids: &[u64],
) -> Result<(Vec<GatheredBattleRow>, Vec<String>), Box<dyn std::error::Error>> {
    let client = RateLimitedHttpClient::new(Duration::from_millis(min_req_wait as u64));
    let local_maps = load_local_map_archives(zk_path)?;
    let version_re = Regex::new(r#"\(Zero-K v(?<version>[^\)]+)\)"#)?;
    let map_thumbnail_re =
        Regex::new(r#"<img src='/Resources/(?<map_file>[^/'"]+?)\.(?:thumbnail|minimap)\.jpg'"#)?;
    let battle_url_base = site_url.join("Battles/Detail/")?;

    let mut rows = Vec::new();
    let mut failures = Vec::new();
    let mut seen = HashSet::new();
    let mut map_archives: HashMap<String, (String, String)> = HashMap::new();
    let pb = ProgressBar::new(battle_ids.len() as u64);

    for battle_id in battle_ids.iter().copied() {
        if !seen.insert(battle_id) {
            pb.inc(1);
            continue;
        }

        let result: Result<GatheredBattleRow, Box<dyn std::error::Error>> = async {
            let battle_html = client
                .send(
                    client
                        .raw()
                        .get(battle_url_base.join(&battle_id.to_string())?),
                )
                .await?
                .text()
                .await?;

            let version = version_re
                .captures(&battle_html)
                .and_then(|cap| cap.name("version"))
                .map(|matched| matched.as_str().trim().to_string())
                .ok_or_else(|| format!("battle {battle_id} is missing a Zero-K version"))?;
            let map_key = map_thumbnail_re
                .captures(&battle_html)
                .and_then(|cap| cap.name("map_file"))
                .map(|matched| matched.as_str().trim().to_string())
                .ok_or_else(|| format!("battle {battle_id} is missing a map thumbnail"))?;

            let (map_file, map_extension) = if let Some(archive) = map_archives.get(&map_key) {
                archive.clone()
            } else if let Some(archive) = guess_local_map_archive(&local_maps, &map_key) {
                map_archives.insert(map_key.clone(), archive.clone());
                archive
            } else {
                let archive = resolve_map_archive_name_from_battle(battle_id, &client, &site_url)
                    .await?
                    .ok_or_else(|| {
                        format!("could not resolve map archive while gathering battle {battle_id}")
                    })?;
                map_archives.insert(map_key.clone(), archive.clone());
                archive
            };

            Ok(GatheredBattleRow {
                battle_id,
                version,
                map_file,
                map_extension,
            })
        }
        .await;

        match result {
            Ok(row) => rows.push(row),
            Err(err) => failures.push(format!("{battle_id} ({err})")),
        }

        pb.inc(1);
    }

    Ok((rows, failures))
}

async fn gather_battle_rows_from_listing(
    settings: &GatherBIDSettings,
) -> Result<Vec<GatheredBattleRow>, Box<dyn std::error::Error>> {
    let req_url = settings.site_url.join("Battles")?;
    let g_filter = &settings.gather_filter;

    let map_bool_opt = |opt, default: u32| match opt {
        None => default.to_string(),
        Some(false) => "0".to_string(),
        Some(true) => "1".to_string(),
    };

    let mut req_form: HashMap<&'static str, String> = HashMap::new();
    req_form.insert("Title", g_filter.title.clone().unwrap_or_default());
    req_form.insert("Map", g_filter.map.clone().unwrap_or_default());
    req_form.insert(
        "PlayersFrom",
        g_filter.players_from.unwrap_or(1).to_string(),
    );
    req_form.insert("PlayersTo", g_filter.players_to.unwrap_or(2).to_string());
    req_form.insert("Age", g_filter.age.unwrap_or(0).to_string());
    req_form.insert("MinLength", g_filter.min_length.unwrap_or(60).to_string());
    req_form.insert("MaxLength", g_filter.max_length.unwrap_or(3600).to_string());
    req_form.insert("Mission", map_bool_opt(g_filter.mission, 0));
    req_form.insert("Bots", map_bool_opt(g_filter.bots, 0));
    req_form.insert("Rank", g_filter.rank.unwrap_or(8).to_string());
    req_form.insert("Victory", map_bool_opt(g_filter.victory, 1));
    req_form.insert("Matchmaker", map_bool_opt(g_filter.matchmaker, 0));
    req_form.insert("Rating", g_filter.rating.unwrap_or(0).to_string());

    let battle_card_block_re = Regex::new(
        r#"(?s)<a href='/Battles/Detail/(?<bid>[0-9]+)'>\s*<div class='mission fleft'.*?</a>"#,
    )?;
    let zero_k_version_re = Regex::new(r#"\(Zero-K v(?<version>[^\)]+)\)"#)?;
    let map_thumbnail_re =
        Regex::new(r#"<img src='/Resources/(?<map_file>[^/'"]+?)\.(?:thumbnail|minimap)\.jpg'"#)?;

    let http_client =
        RateLimitedHttpClient::new(Duration::from_millis(settings.min_req_wait as u64));
    let local_maps = load_local_map_archives(settings.zk_path.as_deref())?;
    let mut rows = Vec::new();
    let mut battle_ids: HashSet<u64> = HashSet::new();
    let mut map_archives: HashMap<String, (String, String)> = HashMap::new();
    let mut offset = settings.initial_offset;
    battle_ids.reserve(settings.gather_num as usize + 40);

    let bar = ProgressBar::new(settings.gather_num as u64);
    while battle_ids.len() < settings.gather_num as usize {
        req_form.insert("offset", offset.to_string());

        let body = http_client
            .send(http_client.raw().post(req_url.clone()).form(&req_form))
            .await?
            .bytes()
            .await?
            .to_vec();

        let body_s = String::from_utf8(body)?;

        let new_battle_ids = battle_card_block_re
            .captures_iter(&body_s)
            .filter_map(|cap| {
                let bid = cap.name("bid")?.as_str().to_string();
                let card_html = cap.get(0)?.as_str();
                let version = zero_k_version_re
                    .captures(card_html)?
                    .name("version")?
                    .as_str()
                    .trim()
                    .to_string();
                let map_file = map_thumbnail_re
                    .captures(card_html)?
                    .name("map_file")?
                    .as_str()
                    .trim()
                    .to_string();
                Some((version, bid, map_file))
            });

        let mut found_any = false;
        for (version, battle_id, map_key) in new_battle_ids {
            found_any = true;
            let battle_id_p = battle_id.parse()?;
            if !battle_ids.contains(&battle_id_p) {
                if battle_ids.len() >= settings.gather_num as usize {
                    break;
                }

                let (map_file, map_extension) = if let Some(archive) = map_archives.get(&map_key) {
                    archive.clone()
                } else if let Some(archive) = guess_local_map_archive(&local_maps, &map_key) {
                    map_archives.insert(map_key.clone(), archive.clone());
                    archive
                } else {
                    let archive = resolve_map_archive_name_from_battle(
                        battle_id_p,
                        &http_client,
                        &settings.site_url,
                    )
                    .await?
                    .ok_or_else(|| {
                        format!(
                            "could not resolve map archive while gathering battle {}",
                            battle_id
                        )
                    })?;
                    map_archives.insert(map_key.clone(), archive.clone());
                    archive
                };

                rows.push(GatheredBattleRow {
                    battle_id: battle_id_p,
                    version,
                    map_file,
                    map_extension,
                });
                battle_ids.insert(battle_id_p);
                bar.inc(1);
            }
        }

        if !found_any {
            return Err(format!(
                "gather-battle-ids found no parseable battle cards at offset {}",
                offset
            )
            .into());
        }

        offset += 40;
    }

    Ok(rows)
}
