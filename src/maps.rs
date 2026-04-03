use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use indicatif::ProgressBar;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use tokio::{fs::File, io::AsyncWriteExt};
use url::Url;

use crate::http::RateLimitedHttpClient;

pub struct DownloadMapsSettings {
    pub site_url: Url,
    pub csv_path: PathBuf,
    pub min_req_wait: u32,
    pub zk_path: PathBuf,
}

fn encode_map_filename(name: &str) -> String {
    utf8_percent_encode(name, NON_ALPHANUMERIC).to_string()
}

fn decode_url_filename(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let parsed = Url::parse(url)?;
    let filename = parsed
        .path_segments()
        .and_then(|segments| segments.last())
        .ok_or("could not extract filename from map download url")?;
    Ok(percent_decode_str(filename).decode_utf8()?.to_string())
}

fn parse_map_download_url(
    map_html: &str,
    site_url: &Url,
) -> Result<Option<Url>, Box<dyn std::error::Error>> {
    let map_download_re = Regex::new(
        r#"(?P<url>(?:https?:)?//[^"' ]+/content/(?:maps|games)/[^"'?#]+\.sd[7z](?:\?[^"'#]*)?|/content/(?:maps|games)/[^"'?#]+\.sd[7z](?:\?[^"'#]*)?)"#,
    )?;
    let Some(download_match) = map_download_re.captures(map_html) else {
        return Ok(None);
    };
    let raw_url = download_match.name("url").unwrap().as_str();

    let download_url = if raw_url.starts_with("//") {
        Url::parse(&format!("https:{raw_url}"))?
    } else if raw_url.starts_with('/') {
        site_url.join(raw_url)?
    } else {
        Url::parse(raw_url)?
    };

    Ok(Some(download_url))
}

fn map_archive_present(maps_dir: &Path, map_file_base: &str) -> Option<PathBuf> {
    let decoded_sd7 = maps_dir.join(format!("{map_file_base}.sd7"));
    if decoded_sd7.is_file() {
        return Some(decoded_sd7);
    }

    let decoded_sdz = maps_dir.join(format!("{map_file_base}.sdz"));
    if decoded_sdz.is_file() {
        return Some(decoded_sdz);
    }

    let encoded_name = encode_map_filename(map_file_base);
    let encoded_sd7 = maps_dir.join(format!("{encoded_name}.sd7"));
    if encoded_sd7.is_file() {
        return Some(encoded_sd7);
    }

    let encoded_sdz = maps_dir.join(format!("{encoded_name}.sdz"));
    if encoded_sdz.is_file() {
        return Some(encoded_sdz);
    }

    None
}

fn map_archive_present_with_extension(
    maps_dir: &Path,
    map_file_base: &str,
    extension: &str,
) -> Option<PathBuf> {
    let decoded = maps_dir.join(format!("{map_file_base}.{extension}"));
    if decoded.is_file() {
        return Some(decoded);
    }

    let encoded_name = encode_map_filename(map_file_base);
    let encoded = maps_dir.join(format!("{encoded_name}.{extension}"));
    if encoded.is_file() {
        return Some(encoded);
    }

    map_archive_present(maps_dir, map_file_base)
}

fn normalize_existing_map_name(
    maps_dir: &Path,
    map_file_base: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(existing_path) = map_archive_present(maps_dir, map_file_base) else {
        return Ok(false);
    };

    let expected_path = match existing_path.extension().and_then(|ext| ext.to_str()) {
        Some("sd7") => maps_dir.join(format!("{map_file_base}.sd7")),
        Some("sdz") => maps_dir.join(format!("{map_file_base}.sdz")),
        _ => return Ok(true),
    };

    if existing_path != expected_path {
        std::fs::rename(existing_path, expected_path)?;
    }
    Ok(true)
}

fn parse_unique_maps(
    csv_path: &Path,
) -> Result<BTreeMap<String, (u64, String)>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_path(csv_path)?;
    let mut maps = BTreeMap::new();
    for record in reader.records().filter_map(|record| record.ok()) {
        let Some(battle_id) = record.get(0).and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let Some(map_file) = record.get(2).map(str::trim).filter(|name| !name.is_empty()) else {
            continue;
        };
        let Some(map_extension) = record.get(3).map(str::trim).filter(|ext| !ext.is_empty()) else {
            continue;
        };
        maps.entry(map_file.to_string())
            .or_insert((battle_id, map_extension.to_string()));
    }
    Ok(maps)
}

pub async fn resolve_map_download_url_from_battle(
    battle_id: u64,
    client: &RateLimitedHttpClient,
    site_url: &Url,
) -> Result<Option<Url>, Box<dyn std::error::Error>> {
    let battle_url = site_url
        .join("Battles/Detail/")?
        .join(&battle_id.to_string())?;
    let battle_html = client
        .send(client.raw().get(battle_url))
        .await?
        .text()
        .await?;

    let map_detail_re = Regex::new(r#"<a href=['"](?P<path>/Maps/Detail/\d+)['"]"#)?;
    let Some(map_detail_match) = map_detail_re.captures(&battle_html) else {
        return Ok(None);
    };
    let map_detail_url = site_url.join(map_detail_match.name("path").unwrap().as_str())?;

    let map_html = client
        .send(client.raw().get(map_detail_url))
        .await?
        .text()
        .await?;
    parse_map_download_url(&map_html, site_url)
}

pub async fn resolve_map_archive_name_from_battle(
    battle_id: u64,
    client: &RateLimitedHttpClient,
    site_url: &Url,
) -> Result<Option<(String, String)>, Box<dyn std::error::Error>> {
    let Some(download_url) =
        resolve_map_download_url_from_battle(battle_id, client, site_url).await?
    else {
        return Ok(None);
    };

    let decoded_filename = decode_url_filename(download_url.as_str())?;
    let Some((archive_base, archive_extension)) = decoded_filename.rsplit_once('.') else {
        return Ok(None);
    };

    Ok(Some((
        archive_base.to_string(),
        archive_extension.to_string(),
    )))
}

async fn download_map(
    map_file_base: &str,
    map_extension: &str,
    battle_id: u64,
    client: &RateLimitedHttpClient,
    site_url: &Url,
    maps_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_name = format!("{map_file_base}.{map_extension}");
    let encoded_file_name = encode_map_filename(&file_name);
    let download_url = site_url.join(&format!("content/maps/{encoded_file_name}"))?;
    let response = client.send(client.raw().get(download_url)).await?;
    if response.status().is_success() {
        let size_label = response
            .content_length()
            .map(|bytes| format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0)))
            .unwrap_or_else(|| "unknown size".to_string());
        println!(
            "Downloading map: {}.{} ({})",
            map_file_base, map_extension, size_label
        );

        let resolved_url = response.url().to_string();
        let bytes = response.bytes().await?;
        let out_path = maps_dir.join(decode_url_filename(&resolved_url)?);
        let mut file = File::create(out_path).await?;
        file.write_all(&bytes).await?;
        return Ok(());
    }

    if let Some(download_url) =
        resolve_map_download_url_from_battle(battle_id, client, site_url).await?
    {
        let response = client.send(client.raw().get(download_url)).await?;
        if response.status().is_success() {
            let size_label = response
                .content_length()
                .map(|bytes| format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0)))
                .unwrap_or_else(|| "unknown size".to_string());
            println!("Downloading map: {} ({})", map_file_base, size_label);

            let resolved_url = response.url().to_string();
            let bytes = response.bytes().await?;
            let out_path = maps_dir.join(decode_url_filename(&resolved_url)?);
            let mut file = File::create(out_path).await?;
            file.write_all(&bytes).await?;
            return Ok(());
        }
    }

    Err(format!("could not download map archive for {}", map_file_base).into())
}

pub async fn download_maps(
    settings: DownloadMapsSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let maps_dir = settings.zk_path.join("maps");
    std::fs::create_dir_all(&maps_dir)?;

    let maps = parse_unique_maps(&settings.csv_path)?;

    let client = RateLimitedHttpClient::new(Duration::from_millis(settings.min_req_wait as u64));

    let pb = ProgressBar::new(maps.len() as u64);
    let mut failures = Vec::new();
    for (map_file_base, (battle_id, map_extension)) in maps {
        if let Some(existing_path) =
            map_archive_present_with_extension(&maps_dir, &map_file_base, &map_extension)
        {
            let expected_path = maps_dir.join(format!("{map_file_base}.{map_extension}"));
            if existing_path != expected_path {
                std::fs::rename(existing_path, expected_path)?;
            }
            pb.inc(1);
            continue;
        }

        if normalize_existing_map_name(&maps_dir, &map_file_base)? {
            pb.inc(1);
            continue;
        }

        if let Err(err) = download_map(
            &map_file_base,
            &map_extension,
            battle_id,
            &client,
            &settings.site_url,
            &maps_dir,
        )
        .await
        {
            eprintln!("Map download failed for {}: {}", map_file_base, err);
            failures.push(map_file_base);
        }
        pb.inc(1);
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to download {} map(s): {}",
            failures.len(),
            failures.join(", ")
        )
        .into())
    }
}
