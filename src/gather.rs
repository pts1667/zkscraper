use indicatif::ProgressBar;
use percent_encoding::percent_decode_str;
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use tokio::time::{self, Duration};
use url::Url;

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
            archive.folded_stem.starts_with(&folded_key) || folded_key.starts_with(&archive.folded_stem)
        })
        .collect();
    if prefix_matches.len() == 1 {
        let archive = prefix_matches[0];
        return Some((archive.stem.clone(), archive.extension.clone()));
    }

    None
}

pub async fn gather_battle_ids(
    settings: GatherBIDSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let req_url_end = "Battles";
    let req_url = settings.site_url.join(req_url_end)?;
    let g_filter = settings.gather_filter;

    let map_bool_opt = |opt, default: u32| match opt {
        None => default.to_string(),
        Some(false) => "0".to_string(),
        Some(true) => "1".to_string(),
    };

    let mut req_form: HashMap<&'static str, String> = HashMap::new();
    req_form.insert("Title", g_filter.title.unwrap_or("".to_string()));
    req_form.insert("Map", g_filter.map.unwrap_or("".to_string()));
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
    let map_detail_re = Regex::new(r#"<a href="(?P<path>/Maps/Detail/\d+)""#)?;
    let map_download_re =
        Regex::new(r#"(https?://zero-k\.info/content/(?:maps|games)/(?P<filename>[^"']+\.(?:sd7|sdz))|//zero-k\.info/content/(?:maps|games)/(?P<filename_scheme_relative>[^"']+\.(?:sd7|sdz)))"#)?;

    let http_client = reqwest::Client::new();
    let local_maps = load_local_map_archives(settings.zk_path.as_deref())?;
    let mut wtr = csv::Writer::from_path(settings.out_path)?;
    let mut timer = time::interval(Duration::from_millis(settings.min_req_wait as u64));
    timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    wtr.write_record(["Battle ID", "Version", "Map File", "Map Extension"])?;

    let mut battle_ids: HashSet<u64> = HashSet::new();
    let mut map_archives: HashMap<String, (String, String)> = HashMap::new();
    let mut offset = settings.initial_offset;
    battle_ids.reserve(settings.gather_num as usize + 40);

    let bar = ProgressBar::new(settings.gather_num as u64);
    while battle_ids.len() < settings.gather_num as usize {
        req_form.insert("offset", offset.to_string());

        let body = http_client
            .post(req_url.clone())
            .form(&req_form)
            .send()
            .await?
            .bytes()
            .await?
            .to_vec();

        let body_s = String::from_utf8(body)?;

        let new_battle_ids = battle_card_block_re.captures_iter(&body_s).filter_map(|cap| {
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
                    let battle_url = settings
                        .site_url
                        .join("Battles/Detail/")?
                        .join(&battle_id)?;
                    let battle_html = http_client.get(battle_url).send().await?.text().await?;

                    let map_detail_path = map_detail_re
                        .captures(&battle_html)
                        .and_then(|cap| cap.name("path").map(|m| m.as_str().to_string()))
                        .ok_or_else(|| {
                            format!(
                                "could not resolve map detail page while gathering battle {}",
                                battle_id
                            )
                        })?;

                    timer.tick().await;

                    let map_detail_url = settings.site_url.join(&map_detail_path)?;
                    let map_html = http_client.get(map_detail_url).send().await?.text().await?;

                    let captures = map_download_re.captures(&map_html).ok_or_else(|| {
                        format!("could not resolve map archive while gathering battle {}", battle_id)
                    })?;
                    let archive_filename = captures
                        .name("filename")
                        .or_else(|| captures.name("filename_scheme_relative"))
                        .map(|m| m.as_str().to_string())
                        .ok_or_else(|| {
                            format!("could not extract map archive while gathering battle {}", battle_id)
                        })?;
                    let decoded_filename = percent_decode_str(&archive_filename)
                        .decode_utf8()?
                        .to_string();
                    let (archive_base, archive_extension) = decoded_filename
                        .rsplit_once('.')
                        .ok_or_else(|| {
                            format!(
                                "map archive missing extension while gathering battle {}",
                                battle_id
                            )
                        })?;
                    let archive = (archive_base.to_string(), archive_extension.to_string());
                    map_archives.insert(map_key.clone(), archive.clone());
                    archive
                };

                wtr.write_record(&[battle_id.to_string(), version, map_file, map_extension])?;
                battle_ids.insert(battle_id_p);
                bar.inc(1);
            }
        }

        if !found_any {
            return Err(format!("gather-battle-ids found no parseable battle cards at offset {}", offset).into());
        }

        offset += 40;
        timer.tick().await;
    }

    wtr.flush()?;
    Ok(())
}
