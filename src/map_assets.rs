use std::{
    env, fmt, fs,
    io::{Cursor, Read},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use fastapi::ToSchema;
use image::{imageops::FilterType, DynamicImage, GrayImage, ImageFormat, Luma};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use sevenz_rust::{Archive, Password, SevenZReader};
use zip::ZipArchive;

#[derive(Debug)]
pub struct MapAssetError(String);

impl MapAssetError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for MapAssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MapAssetError {}

#[derive(Debug, Clone)]
pub struct MapService {
    maps_dir: PathBuf,
    heightmap_cache: Arc<Mutex<LruCache<String, Arc<Vec<u8>>>>>,
    features_cache: Arc<Mutex<LruCache<String, Arc<MapFeaturesResponse>>>>,
}

const MAP_HEIGHTMAP_CACHE_SIZE: usize = 64;
const MAP_FEATURES_CACHE_SIZE: usize = 64;

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct MetalSpot {
    pub x: f32,
    pub z: f32,
    pub metal: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct MapFeature {
    pub name: String,
    pub x: f32,
    pub z: f32,
    pub y: Option<f32>,
    pub rot: Option<f32>,
    pub scale: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct MapFeaturesResponse {
    pub map_name: String,
    pub metal_spots: Vec<MetalSpot>,
    pub features: Vec<MapFeature>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct MapListResponse {
    pub items: Vec<String>,
}

#[derive(Debug, Clone)]
enum ArchiveKind {
    Zip(PathBuf),
    SevenZip(PathBuf),
}

impl MapService {
    pub fn from_zk_path(zk_path: impl AsRef<Path>) -> Result<Self, MapAssetError> {
        let maps_dir = zk_path.as_ref().join("maps");
        if !maps_dir.is_dir() {
            return Err(MapAssetError::new(format!(
                "maps directory does not exist: {}",
                maps_dir.display()
            )));
        }

        Ok(Self {
            maps_dir,
            heightmap_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(MAP_HEIGHTMAP_CACHE_SIZE).expect("cache size must be non-zero"),
            ))),
            features_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(MAP_FEATURES_CACHE_SIZE).expect("cache size must be non-zero"),
            ))),
        })
    }

    pub fn heightmap_bmp(&self, map_name: &str) -> Result<Vec<u8>, MapAssetError> {
        let cache_key = normalize_map_key(map_name);
        if let Some(cached) = self
            .heightmap_cache
            .lock()
            .map_err(|_| MapAssetError::new("heightmap cache mutex poisoned"))?
            .get(&cache_key)
            .cloned()
        {
            return Ok((*cached).clone());
        }

        let archive = self.resolve_archive(map_name)?;
        let smf_path = self.find_map_file_path(&archive, map_name, ".smf")?;
        let smf_bytes = self.read_archive_file(&archive, &smf_path)?;
        let (width, height, samples) = parse_smf_heightmap(&smf_bytes)?;
        let image = build_heightmap_image(width, height, &samples)?;

        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut out, ImageFormat::Bmp)
            .map_err(|err| MapAssetError::new(err.to_string()))?;
        let bytes = out.into_inner();
        self.heightmap_cache
            .lock()
            .map_err(|_| MapAssetError::new("heightmap cache mutex poisoned"))?
            .put(cache_key, Arc::new(bytes.clone()));
        Ok(bytes)
    }

    pub fn map_features(&self, map_name: &str) -> Result<MapFeaturesResponse, MapAssetError> {
        let cache_key = normalize_map_key(map_name);
        if let Some(cached) = self
            .features_cache
            .lock()
            .map_err(|_| MapAssetError::new("map features cache mutex poisoned"))?
            .get(&cache_key)
            .cloned()
        {
            return Ok((*cached).clone());
        }

        let archive = self.resolve_archive(map_name)?;
        let metal_spots = self
            .read_optional_archive_file(&archive, "mapconfig/map_metal_layout.lua")?
            .map(|bytes| parse_metal_spots(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_default();
        let features = self
            .read_optional_archive_file(&archive, "mapconfig/featureplacer/set.lua")?
            .map(|bytes| parse_feature_placements(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_default();

        let response = MapFeaturesResponse {
            map_name: map_name.to_string(),
            metal_spots,
            features,
        };
        self.features_cache
            .lock()
            .map_err(|_| MapAssetError::new("map features cache mutex poisoned"))?
            .put(cache_key, Arc::new(response.clone()));
        Ok(response)
    }

    pub fn list_maps(&self) -> Result<MapListResponse, MapAssetError> {
        let mut map_names = Vec::new();

        for entry in
            fs::read_dir(&self.maps_dir).map_err(|err| MapAssetError::new(err.to_string()))?
        {
            let entry = entry.map_err(|err| MapAssetError::new(err.to_string()))?;
            let path = entry.path();
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if !extension.eq_ignore_ascii_case("sd7") && !extension.eq_ignore_ascii_case("sdz") {
                continue;
            }

            let archive = archive_kind(path)?;
            let entries = self.list_archive_entries_display(&archive)?;
            let mut added_from_archive = false;
            for map_name in entries
                .into_iter()
                .filter_map(|name| archive_map_stem_display(&name))
            {
                map_names.push(display_map_name(&map_name));
                added_from_archive = true;
            }

            if !added_from_archive {
                if let Some(stem) = archive_path_stem(&archive) {
                    map_names.push(display_map_name(&stem));
                }
            }
        }

        map_names.sort_unstable_by_key(|name| normalize_map_key(name));
        map_names.dedup_by(|left, right| normalize_map_key(left) == normalize_map_key(right));
        Ok(MapListResponse { items: map_names })
    }

    fn resolve_archive(&self, map_name: &str) -> Result<ArchiveKind, MapAssetError> {
        let requested = normalize_map_key(map_name);
        let mut exact_matches = Vec::new();
        let mut prefix_matches = Vec::new();
        let mut fallbacks = Vec::new();

        for entry in
            fs::read_dir(&self.maps_dir).map_err(|err| MapAssetError::new(err.to_string()))?
        {
            let entry = entry.map_err(|err| MapAssetError::new(err.to_string()))?;
            let path = entry.path();
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let folded_stem = normalize_map_key(stem);
            if folded_stem == requested {
                exact_matches.push(path.clone());
            } else if folded_stem.starts_with(&requested) || requested.starts_with(&folded_stem) {
                prefix_matches.push(path.clone());
            }
            if extension.eq_ignore_ascii_case("sd7") || extension.eq_ignore_ascii_case("sdz") {
                fallbacks.push(path);
            }
        }

        if exact_matches.len() == 1 {
            return archive_kind(exact_matches.remove(0));
        }
        if prefix_matches.len() == 1 {
            return archive_kind(prefix_matches.remove(0));
        }

        for path in fallbacks {
            let archive = archive_kind(path)?;
            if self
                .list_archive_entries(&archive)?
                .into_iter()
                .filter_map(|name| archive_map_stem(&name))
                .any(|candidate| normalize_map_key(&candidate) == requested)
            {
                return Ok(archive);
            }
        }

        Err(MapAssetError::new(format!(
            "map archive not found for '{map_name}'"
        )))
    }

    fn list_archive_entries(&self, archive: &ArchiveKind) -> Result<Vec<String>, MapAssetError> {
        match archive {
            ArchiveKind::Zip(path) => {
                let file =
                    fs::File::open(path).map_err(|err| MapAssetError::new(err.to_string()))?;
                let mut archive =
                    ZipArchive::new(file).map_err(|err| MapAssetError::new(err.to_string()))?;
                let mut names = Vec::with_capacity(archive.len());
                for index in 0..archive.len() {
                    let entry = archive
                        .by_index(index)
                        .map_err(|err| MapAssetError::new(err.to_string()))?;
                    names.push(normalize_archive_path(entry.name()));
                }
                Ok(names)
            }
            ArchiveKind::SevenZip(path) => match Archive::open(path) {
                Ok(archive) => Ok(archive
                    .files
                    .iter()
                    .map(|entry| normalize_archive_path(entry.name()))
                    .collect()),
                Err(_) => list_archive_entries_with_7z(path),
            },
        }
    }

    fn list_archive_entries_display(
        &self,
        archive: &ArchiveKind,
    ) -> Result<Vec<String>, MapAssetError> {
        match archive {
            ArchiveKind::Zip(path) => {
                let file =
                    fs::File::open(path).map_err(|err| MapAssetError::new(err.to_string()))?;
                let mut archive =
                    ZipArchive::new(file).map_err(|err| MapAssetError::new(err.to_string()))?;
                let mut names = Vec::with_capacity(archive.len());
                for index in 0..archive.len() {
                    let entry = archive
                        .by_index(index)
                        .map_err(|err| MapAssetError::new(err.to_string()))?;
                    names.push(entry.name().replace('\\', "/"));
                }
                Ok(names)
            }
            ArchiveKind::SevenZip(path) => match Archive::open(path) {
                Ok(archive) => Ok(archive
                    .files
                    .iter()
                    .map(|entry| entry.name().replace('\\', "/"))
                    .collect()),
                Err(_) => list_archive_entries_with_7z(path),
            },
        }
    }

    fn find_map_file_path(
        &self,
        archive: &ArchiveKind,
        map_name: &str,
        extension: &str,
    ) -> Result<String, MapAssetError> {
        let requested = normalize_map_key(map_name);
        let entries = self.list_archive_entries(archive)?;
        if let Some(path) = entries.iter().find(|path| {
            archive_map_stem(path)
                .map(|stem| normalize_map_key(&stem) == requested && path.ends_with(extension))
                .unwrap_or(false)
        }) {
            return Ok(path.clone());
        }

        let candidates = entries
            .iter()
            .filter(|path| path.ends_with(extension))
            .filter(|path| {
                archive_map_stem(path)
                    .map(|stem| {
                        let folded = normalize_map_key(&stem);
                        folded.starts_with(&requested) || requested.starts_with(&folded)
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<_>>();
        if candidates.len() == 1 {
            return Ok(candidates[0].clone());
        }

        let all_map_files = entries
            .into_iter()
            .filter(|path| path.ends_with(extension))
            .collect::<Vec<_>>();
        if all_map_files.len() == 1 {
            return Ok(all_map_files[0].clone());
        }

        Err(MapAssetError::new(format!(
            "could not find {extension} for map '{map_name}' in archive"
        )))
    }

    fn read_optional_archive_file(
        &self,
        archive: &ArchiveKind,
        relative_path: &str,
    ) -> Result<Option<Vec<u8>>, MapAssetError> {
        let requested = normalize_archive_path(relative_path);
        let Some(existing_path) = self
            .list_archive_entries(archive)?
            .into_iter()
            .find(|path| normalize_archive_path(path) == requested)
        else {
            return Ok(None);
        };

        self.read_archive_file(archive, &existing_path).map(Some)
    }

    fn read_archive_file(
        &self,
        archive: &ArchiveKind,
        relative_path: &str,
    ) -> Result<Vec<u8>, MapAssetError> {
        let requested = normalize_archive_path(relative_path);
        match archive {
            ArchiveKind::Zip(path) => {
                let file =
                    fs::File::open(path).map_err(|err| MapAssetError::new(err.to_string()))?;
                let mut archive =
                    ZipArchive::new(file).map_err(|err| MapAssetError::new(err.to_string()))?;
                for index in 0..archive.len() {
                    let mut entry = archive
                        .by_index(index)
                        .map_err(|err| MapAssetError::new(err.to_string()))?;
                    if normalize_archive_path(entry.name()) == requested {
                        let mut bytes = Vec::new();
                        entry
                            .read_to_end(&mut bytes)
                            .map_err(|err| MapAssetError::new(err.to_string()))?;
                        return Ok(bytes);
                    }
                }
            }
            ArchiveKind::SevenZip(path) => match read_sevenz_file(path, &requested) {
                Ok(bytes) => return Ok(bytes),
                Err(primary_err) => {
                    if let Ok(bytes) = read_sevenz_file_with_7z(path, &requested) {
                        return Ok(bytes);
                    }
                    return Err(primary_err);
                }
            },
        }

        Err(MapAssetError::new(format!(
            "archive file not found: {relative_path}"
        )))
    }
}

fn archive_kind(path: PathBuf) -> Result<ArchiveKind, MapAssetError> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("sdz") => Ok(ArchiveKind::Zip(path)),
        Some(ext) if ext.eq_ignore_ascii_case("sd7") => Ok(ArchiveKind::SevenZip(path)),
        _ => Err(MapAssetError::new(format!(
            "unsupported map archive type: {}",
            path.display()
        ))),
    }
}

fn archive_path_stem(archive: &ArchiveKind) -> Option<String> {
    let path = match archive {
        ArchiveKind::Zip(path) | ArchiveKind::SevenZip(path) => path,
    };
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.to_string())
}

fn normalize_archive_path(path: &str) -> String {
    path.replace('\\', "/").to_ascii_lowercase()
}

fn archive_map_stem(path: &str) -> Option<String> {
    let normalized = normalize_archive_path(path);
    let relative = normalized.strip_prefix("maps/")?;
    let stem = relative.strip_suffix(".smf")?;
    Some(stem.to_string())
}

fn archive_map_stem_display(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let prefix_len = normalized
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("maps/"))
        .map(str::len)?;
    let relative = &normalized[prefix_len..];
    if relative.len() < 4 || !relative[relative.len() - 4..].eq_ignore_ascii_case(".smf") {
        return None;
    }
    Some(relative[..relative.len() - 4].to_string())
}

fn normalize_map_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn display_map_name(value: &str) -> String {
    value.replace('_', " ")
}

fn parse_smf_heightmap(bytes: &[u8]) -> Result<(u32, u32, Vec<u16>), MapAssetError> {
    const HEADER_SIZE: usize = 76;
    if bytes.len() < HEADER_SIZE {
        return Err(MapAssetError::new("smf file too small"));
    }
    if &bytes[..15] != b"spring map file" {
        return Err(MapAssetError::new("invalid smf header magic"));
    }

    let map_x = read_u32_le(bytes, 24)?;
    let map_y = read_u32_le(bytes, 28)?;
    let heightmap_ptr = read_u32_le(bytes, 52)? as usize;
    let width = map_x
        .checked_add(1)
        .ok_or_else(|| MapAssetError::new("smf width overflow"))?;
    let height = map_y
        .checked_add(1)
        .ok_or_else(|| MapAssetError::new("smf height overflow"))?;
    let sample_count = width as usize * height as usize;
    let byte_len = sample_count
        .checked_mul(2)
        .ok_or_else(|| MapAssetError::new("smf heightmap byte length overflow"))?;
    let end = heightmap_ptr
        .checked_add(byte_len)
        .ok_or_else(|| MapAssetError::new("smf heightmap end overflow"))?;
    if end > bytes.len() {
        return Err(MapAssetError::new("smf heightmap exceeds file length"));
    }

    let mut samples = Vec::with_capacity(sample_count);
    for chunk in bytes[heightmap_ptr..end].chunks_exact(2) {
        samples.push(u16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Ok((width, height, samples))
}

fn build_heightmap_image(
    width: u32,
    height: u32,
    samples: &[u16],
) -> Result<GrayImage, MapAssetError> {
    if samples.is_empty() {
        return Err(MapAssetError::new("heightmap has no samples"));
    }

    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    let range = max.saturating_sub(min).max(1);
    let mut image = GrayImage::new(width, height);

    for (index, sample) in samples.iter().enumerate() {
        let x = (index as u32) % width;
        let y = (index as u32) / width;
        let value = (((sample.saturating_sub(min)) as f32 / range as f32) * 255.0).round() as u8;
        image.put_pixel(x, y, Luma([value]));
    }

    Ok(image::imageops::resize(
        &image,
        512,
        512,
        FilterType::Triangle,
    ))
}

fn parse_metal_spots(contents: &str) -> Vec<MetalSpot> {
    extract_lua_table_blocks(contents)
        .into_iter()
        .filter_map(|block| {
            let x = extract_number(&block, "x")?;
            let z = extract_number(&block, "z")?;
            Some(MetalSpot {
                x,
                z,
                metal: extract_number(&block, "metal"),
            })
        })
        .collect()
}

fn parse_feature_placements(contents: &str) -> Vec<MapFeature> {
    extract_lua_table_blocks(contents)
        .into_iter()
        .filter_map(|block| {
            let name = extract_string(&block, &["name", "feature", "defname"])?;
            let x = extract_number(&block, "x")?;
            let z = extract_number(&block, "z")?;
            Some(MapFeature {
                name,
                x,
                z,
                y: extract_number(&block, "y"),
                rot: extract_number(&block, "rot"),
                scale: extract_number(&block, "scale"),
            })
        })
        .collect()
}

fn extract_lua_table_blocks(contents: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut depth = 0_u32;
    let mut start = None;
    for (index, ch) in contents.char_indices() {
        match ch {
            '{' => {
                depth += 1;
                if depth == 2 {
                    start = Some(index);
                }
            }
            '}' => {
                if depth == 2 {
                    if let Some(block_start) = start.take() {
                        blocks.push(contents[block_start..=index].to_string());
                    }
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    blocks
}

fn extract_number(block: &str, key: &str) -> Option<f32> {
    let needle = format!("{key}=");
    let compact: String = block.chars().filter(|ch| !ch.is_whitespace()).collect();
    let start = compact.find(&needle)? + needle.len();
    let rest = &compact[start..];
    let end = rest
        .find(|ch: char| !(ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+')))
        .unwrap_or(rest.len());
    rest[..end].parse::<f32>().ok()
}

fn extract_string(block: &str, keys: &[&str]) -> Option<String> {
    let compact: String = block.chars().filter(|ch| !ch.is_whitespace()).collect();
    for key in keys {
        let needle = format!("{key}=");
        let start = compact.find(&needle)? + needle.len();
        let rest = &compact[start..];
        if let Some(stripped) = rest.strip_prefix('"') {
            let end = stripped.find('"')?;
            return Some(stripped[..end].to_string());
        }
        if let Some(stripped) = rest.strip_prefix('\'') {
            let end = stripped.find('\'')?;
            return Some(stripped[..end].to_string());
        }
        let end = rest
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .unwrap_or(rest.len());
        return Some(rest[..end].to_string());
    }
    None
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, MapAssetError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| MapAssetError::new("smf header truncated"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_sevenz_file(path: &Path, requested: &str) -> Result<Vec<u8>, MapAssetError> {
    let mut source = fs::File::open(path).map_err(|err| MapAssetError::new(err.to_string()))?;
    let reader_len = source
        .metadata()
        .map_err(|err| MapAssetError::new(err.to_string()))?
        .len();
    let mut reader = SevenZReader::new(&mut source, reader_len, Password::empty())
        .map_err(|err| MapAssetError::new(err.to_string()))?;
    let mut found: Option<Vec<u8>> = None;
    reader
        .for_each_entries(|entry, contents| {
            if normalize_archive_path(entry.name()) == requested {
                let mut bytes = Vec::new();
                contents
                    .read_to_end(&mut bytes)
                    .map_err(sevenz_rust::Error::io)?;
                found = Some(bytes);
                return Ok(false);
            }
            Ok(true)
        })
        .map_err(|err| MapAssetError::new(err.to_string()))?;
    found.ok_or_else(|| MapAssetError::new(format!("archive file not found: {requested}")))
}

fn list_archive_entries_with_7z(path: &Path) -> Result<Vec<String>, MapAssetError> {
    let output = Command::new(resolve_7z_path())
        .arg("l")
        .arg("-slt")
        .arg(path)
        .output()
        .map_err(|err| MapAssetError::new(err.to_string()))?;
    if !output.status.success() {
        return Err(MapAssetError::new(format!(
            "7z list failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("Path = "))
        .map(|entry| entry.replace('\\', "/"))
        .filter(|entry| !entry.is_empty() && !entry.ends_with('/'))
        .collect())
}

fn read_sevenz_file_with_7z(path: &Path, requested: &str) -> Result<Vec<u8>, MapAssetError> {
    let output = Command::new(resolve_7z_path())
        .arg("x")
        .arg(path)
        .arg(requested)
        .arg("-so")
        .output()
        .map_err(|err| MapAssetError::new(err.to_string()))?;
    if !output.status.success() {
        return Err(MapAssetError::new(format!(
            "7z extract failed for {}:{}: {}",
            path.display(),
            requested,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn resolve_7z_path() -> &'static str {
    Box::leak(
        env::var("ZKSCRAPER_7Z_PATH")
            .unwrap_or_else(|_| default_7z_path().to_string())
            .into_boxed_str(),
    )
}

fn default_7z_path() -> &'static str {
    if cfg!(target_os = "windows") {
        r"C:\Program Files\7-Zip\7z.exe"
    } else if cfg!(target_os = "macos") {
        "7zz"
    } else {
        "7z"
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, path::Path};

    use super::{MapService, MetalSpot};

    fn sample_smf() -> Vec<u8> {
        let width = 64_u32;
        let height = 64_u32;
        let map_x = width / 64;
        let map_y = height / 64;
        let sample_width = width + 1;
        let sample_height = height + 1;
        let sample_count = (sample_width * sample_height) as usize;
        let heightmap_offset = 76_u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"spring map file");
        bytes.push(0);
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&map_x.to_le_bytes());
        bytes.extend_from_slice(&map_y.to_le_bytes());
        bytes.extend_from_slice(&8_u32.to_le_bytes());
        bytes.extend_from_slice(&8_u32.to_le_bytes());
        bytes.extend_from_slice(&32_u32.to_le_bytes());
        bytes.extend_from_slice(&0.0_f32.to_le_bytes());
        bytes.extend_from_slice(&255.0_f32.to_le_bytes());
        bytes.extend_from_slice(&heightmap_offset.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        for index in 0..sample_count {
            let value = (index % 1024) as u16;
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn write_test_archive(root: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let maps_dir = root.join("maps");
        fs::create_dir_all(&maps_dir)?;
        let archive_path = maps_dir.join("TestMap.sdz");
        let file = fs::File::create(archive_path)?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("maps/TestMap.smf", options)?;
        writer.write_all(&sample_smf())?;
        writer.start_file("mapconfig/map_metal_layout.lua", options)?;
        writer.write_all(b"return { { x = 100, z = 200, metal = 2.5 }, { x = 300, z = 400 } }")?;
        writer.start_file("mapconfig/featureplacer/set.lua", options)?;
        writer.write_all(
            b"return { { name = 'treetype1', x = 64, z = 96, rot = 1.5 }, { name = 'rock', x = 128, z = 160 } }",
        )?;
        writer.finish()?;
        Ok(())
    }

    #[test]
    fn reads_heightmap_and_features_from_archive(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = tempfile::tempdir()?;
        write_test_archive(temp_dir.path())?;
        let service = MapService::from_zk_path(temp_dir.path())?;

        let bmp = service.heightmap_bmp("TestMap")?;
        assert!(bmp.starts_with(b"BM"));

        let features = service.map_features("TestMap")?;
        assert_eq!(features.map_name, "TestMap");
        assert_eq!(features.metal_spots.len(), 2);
        assert_eq!(
            features.metal_spots[0],
            MetalSpot {
                x: 100.0,
                z: 200.0,
                metal: Some(2.5),
            }
        );
        assert_eq!(features.features.len(), 2);
        assert_eq!(features.features[0].name, "treetype1");
        Ok(())
    }

    #[test]
    fn matches_display_name_to_underscored_map_file(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = tempfile::tempdir()?;
        let maps_dir = temp_dir.path().join("maps");
        fs::create_dir_all(&maps_dir)?;
        let archive_path = maps_dir.join("duke_nukem_4.2.sdz");
        let file = fs::File::create(&archive_path)?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("maps/duke_nukem_4.2.smf", options)?;
        writer.write_all(&sample_smf())?;
        writer.finish()?;

        let service = MapService::from_zk_path(temp_dir.path())?;
        let bmp = service.heightmap_bmp("duke nukem 4.2")?;
        assert!(bmp.starts_with(b"BM"));
        Ok(())
    }

    #[test]
    fn lists_available_maps_from_archives() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let temp_dir = tempfile::tempdir()?;
        write_test_archive(temp_dir.path())?;

        let maps_dir = temp_dir.path().join("maps");
        let archive_path = maps_dir.join("duke_nukem_4.2.sdz");
        let file = fs::File::create(&archive_path)?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("maps/duke_nukem_4.2.smf", options)?;
        writer.write_all(&sample_smf())?;
        writer.finish()?;

        let service = MapService::from_zk_path(temp_dir.path())?;
        let maps = service.list_maps()?;
        assert!(maps.items.iter().any(|name| name == "TestMap"));
        assert!(maps.items.iter().any(|name| name == "duke nukem 4.2"));
        Ok(())
    }
}
