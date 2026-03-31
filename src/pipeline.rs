use std::{
    error::Error,
    fs,
    io,
    path::{Path, PathBuf},
};

use url::Url;

use crate::{fetch, gather, maps, parse};

pub struct PipelineSettings {
    pub site_url: Url,
    pub min_req_wait: u32,
    pub initial_offset: u32,
    pub gather_num: u32,
    pub zk_path: PathBuf,
    pub out_path: PathBuf,
    pub temp_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum PipelineStage {
    GatherBattleIds,
    DownloadMaps,
    DownloadReplays,
    ParseReplays,
}

impl PipelineStage {
    fn label(self) -> &'static str {
        match self {
            PipelineStage::GatherBattleIds => "gather battle IDs",
            PipelineStage::DownloadMaps => "download maps",
            PipelineStage::DownloadReplays => "download replays",
            PipelineStage::ParseReplays => "parse replays",
        }
    }
}

#[derive(Debug, Clone)]
struct PipelinePaths {
    temp_root: Option<PathBuf>,
    battle_csv_path: PathBuf,
    replay_dir_path: PathBuf,
    working_db_path: PathBuf,
    fail_db_path: PathBuf,
    replace_backup_path: PathBuf,
}

pub async fn run_pipeline(settings: PipelineSettings) -> Result<(), Box<dyn Error>> {
    let paths = derive_pipeline_paths(&settings)?;
    prepare_run_layout(&paths)?;
    seed_working_db(&settings.out_path, &paths.working_db_path)?;

    if let Err(err) = gather::gather_battle_ids(gather::GatherBIDSettings {
        site_url: settings.site_url.clone(),
        initial_offset: settings.initial_offset,
        gather_num: settings.gather_num,
        min_req_wait: settings.min_req_wait,
        out_path: paths.battle_csv_path.clone(),
        zk_path: Some(settings.zk_path.clone()),
        gather_filter: gather::GatherFilterSettings::default(),
    })
    .await
    {
        return pipeline_failure(
            err,
            PipelineStage::GatherBattleIds,
            &settings,
            &paths,
            &paths.battle_csv_path,
        );
    }

    if let Err(err) = maps::download_maps(maps::DownloadMapsSettings {
        site_url: settings.site_url.clone(),
        csv_path: paths.battle_csv_path.clone(),
        min_req_wait: settings.min_req_wait,
        zk_path: settings.zk_path.clone(),
    })
    .await
    {
        return pipeline_failure(
            err,
            PipelineStage::DownloadMaps,
            &settings,
            &paths,
            &paths.battle_csv_path,
        );
    }

    if let Err(err) = fetch::fetch_replays(fetch::FetchReplaySettings {
        site_url: settings.site_url.clone(),
        csv_path: paths.battle_csv_path.clone(),
        min_req_wait: settings.min_req_wait,
        out_path: paths.replay_dir_path.clone(),
    })
    .await
    {
        return pipeline_failure(
            err,
            PipelineStage::DownloadReplays,
            &settings,
            &paths,
            &paths.battle_csv_path,
        );
    }

    if let Err(err) = parse::parse_replays(parse::ParseReplaySettings {
        sdfz_in: paths.replay_dir_path.clone(),
        zk_path: settings.zk_path.clone(),
        snapshot_path: paths.working_db_path.clone(),
    })
    .await
    {
        return pipeline_failure(
            err,
            PipelineStage::ParseReplays,
            &settings,
            &paths,
            &paths.fail_db_path,
        );
    }

    promote_working_db(
        &paths.working_db_path,
        &settings.out_path,
        &paths.replace_backup_path,
    )?;
    cleanup_success_artifacts(&paths)?;
    Ok(())
}

fn derive_pipeline_paths(settings: &PipelineSettings) -> Result<PipelinePaths, Box<dyn Error>> {
    let out_name = settings
        .out_path
        .file_name()
        .ok_or_else(|| format!("output path '{}' must include a final directory name", settings.out_path.display()))?
        .to_string_lossy()
        .to_string();

    let parent = settings
        .out_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let temp_root = settings.temp_root.clone();
    let (battle_csv_path, replay_dir_path, working_db_path) = if let Some(temp_root) = &temp_root {
        (
            temp_root.join("battles.csv"),
            temp_root.join("replays"),
            temp_root.join("working-db"),
        )
    } else {
        (
            parent.join(format!("{out_name}.battles.csv")),
            parent.join(format!("{out_name}.replays")),
            parent.join(format!("{out_name}_tmp")),
        )
    };

    Ok(PipelinePaths {
        temp_root,
        battle_csv_path,
        replay_dir_path,
        working_db_path,
        fail_db_path: parent.join(format!("{out_name}_fail")),
        replace_backup_path: parent.join(format!("{out_name}_replace_backup")),
    })
}

fn prepare_run_layout(paths: &PipelinePaths) -> Result<(), Box<dyn Error>> {
    if let Some(temp_root) = &paths.temp_root {
        if temp_root.exists() {
            fs::remove_dir_all(temp_root)?;
        }
        fs::create_dir_all(temp_root)?;
    } else {
        remove_if_exists(&paths.battle_csv_path)?;
        remove_if_exists(&paths.replay_dir_path)?;
        remove_if_exists(&paths.working_db_path)?;
    }

    remove_if_exists(&paths.fail_db_path)?;
    remove_if_exists(&paths.replace_backup_path)?;
    Ok(())
}

fn seed_working_db(out_path: &Path, working_db_path: &Path) -> Result<(), Box<dyn Error>> {
    if !out_path.exists() {
        return Ok(());
    }

    remove_if_exists(working_db_path)?;
    copy_dir_recursive(out_path, working_db_path)?;
    Ok(())
}

fn promote_working_db(
    working_db_path: &Path,
    out_path: &Path,
    replace_backup_path: &Path,
) -> Result<(), Box<dyn Error>> {
    remove_if_exists(replace_backup_path)?;

    if out_path.exists() {
        fs::rename(out_path, replace_backup_path)?;
    }

    let promote_result = rename_or_copy_dir(working_db_path, out_path);
    if let Err(err) = promote_result {
        if out_path.exists() {
            remove_if_exists(out_path)?;
        }
        if replace_backup_path.exists() {
            fs::rename(replace_backup_path, out_path)?;
        }
        return Err(err);
    }

    remove_if_exists(replace_backup_path)?;
    Ok(())
}

fn cleanup_success_artifacts(paths: &PipelinePaths) -> Result<(), Box<dyn Error>> {
    if let Some(temp_root) = &paths.temp_root {
        remove_if_exists(temp_root)?;
    } else {
        remove_if_exists(&paths.battle_csv_path)?;
        remove_if_exists(&paths.replay_dir_path)?;
        remove_if_exists(&paths.working_db_path)?;
    }
    remove_if_exists(&paths.fail_db_path)?;
    remove_if_exists(&paths.replace_backup_path)?;
    Ok(())
}

fn pipeline_failure(
    err: Box<dyn Error>,
    failed_stage: PipelineStage,
    settings: &PipelineSettings,
    paths: &PipelinePaths,
    resume_target: &Path,
) -> Result<(), Box<dyn Error>> {
    let preserved_db = emit_failure_db(&paths.working_db_path, &paths.fail_db_path)?;
    let next_command = build_resume_command(failed_stage, settings, paths, resume_target);
    let fail_notice = if preserved_db {
        format!(
            "Preserved the best available DB state at {}.",
            paths.fail_db_path.display()
        )
    } else {
        format!(
            "No parsed DB state was available to preserve at {}.",
            paths.fail_db_path.display()
        )
    };

    Err(format!(
        "Pipeline failed while trying to {}: {}\n{}\nRun this next:\n{}",
        failed_stage.label(),
        err,
        fail_notice,
        next_command
    )
    .into())
}

fn emit_failure_db(working_db_path: &Path, fail_db_path: &Path) -> Result<bool, Box<dyn Error>> {
    if !working_db_path.exists() {
        return Ok(false);
    }

    remove_if_exists(fail_db_path)?;
    rename_or_copy_dir(working_db_path, fail_db_path)?;
    Ok(true)
}

fn build_resume_command(
    failed_stage: PipelineStage,
    settings: &PipelineSettings,
    paths: &PipelinePaths,
    snapshot_path: &Path,
) -> String {
    let base = "cargo run --release --bin zkscraper --";
    match failed_stage {
        PipelineStage::GatherBattleIds => format!(
            "{base} gather-battle-ids --initial-offset {} --gather-num {} --zk-path {} --out {}",
            settings.initial_offset,
            quoted(&settings.gather_num.to_string()),
            quoted_path(&settings.zk_path),
            quoted_path(&paths.battle_csv_path)
        ),
        PipelineStage::DownloadMaps => format!(
            "{base} download-maps --battle-ids {} --zk-path {}",
            quoted_path(&paths.battle_csv_path),
            quoted_path(&settings.zk_path)
        ),
        PipelineStage::DownloadReplays => format!(
            "{base} download-replays --battle-ids {} --sdfz-path {}",
            quoted_path(&paths.battle_csv_path),
            quoted_path(&paths.replay_dir_path)
        ),
        PipelineStage::ParseReplays => format!(
            "{base} parse-replays --sdfz-in {} --zk-path {} --snapshot-path {}",
            quoted_path(&paths.replay_dir_path),
            quoted_path(&settings.zk_path),
            quoted_path(snapshot_path)
        ),
    }
}

fn quoted_path(path: &Path) -> String {
    quoted(&path.display().to_string())
}

fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn rename_or_copy_dir(src: &Path, dst: &Path) -> Result<(), Box<dyn Error>> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            if dst.exists() {
                remove_if_exists(dst)?;
            }
            copy_dir_recursive(src, dst).map_err(|copy_err| {
                format!(
                    "failed to move '{}' to '{}': {}; fallback copy also failed: {}",
                    src.display(),
                    dst.display(),
                    rename_err,
                    copy_err
                )
            })?;
            remove_if_exists(src)?;
            Ok(())
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), Box<dyn Error>> {
    if !src.is_dir() {
        return Err(format!("source directory '{}' does not exist", src.display()).into());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let entry_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&entry_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&entry_path, &dst_path)?;
        }
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_settings() -> PipelineSettings {
        PipelineSettings {
            site_url: Url::parse("https://zero-k.info").unwrap(),
            min_req_wait: 1_000,
            initial_offset: 0,
            gather_num: 100,
            zk_path: PathBuf::from(r"C:\zk"),
            out_path: PathBuf::from(r".\target\parsed-db"),
            temp_root: None,
        }
    }

    #[test]
    fn derives_default_sibling_paths() {
        let paths = derive_pipeline_paths(&sample_settings()).unwrap();
        assert_eq!(paths.battle_csv_path, PathBuf::from(r".\target\parsed-db.battles.csv"));
        assert_eq!(paths.replay_dir_path, PathBuf::from(r".\target\parsed-db.replays"));
        assert_eq!(paths.working_db_path, PathBuf::from(r".\target\parsed-db_tmp"));
        assert_eq!(paths.fail_db_path, PathBuf::from(r".\target\parsed-db_fail"));
    }

    #[test]
    fn derives_temp_root_paths() {
        let mut settings = sample_settings();
        settings.temp_root = Some(PathBuf::from(r".\target\pipeline-temp"));
        let paths = derive_pipeline_paths(&settings).unwrap();
        assert_eq!(paths.battle_csv_path, PathBuf::from(r".\target\pipeline-temp\battles.csv"));
        assert_eq!(paths.replay_dir_path, PathBuf::from(r".\target\pipeline-temp\replays"));
        assert_eq!(paths.working_db_path, PathBuf::from(r".\target\pipeline-temp\working-db"));
        assert_eq!(paths.fail_db_path, PathBuf::from(r".\target\parsed-db_fail"));
    }

    #[test]
    fn formats_parse_resume_command_with_fail_db() {
        let settings = sample_settings();
        let paths = derive_pipeline_paths(&settings).unwrap();
        let command = build_resume_command(
            PipelineStage::ParseReplays,
            &settings,
            &paths,
            &paths.fail_db_path,
        );
        assert!(command.contains("parse-replays"));
        assert!(command.contains("--snapshot-path \".\\target\\parsed-db_fail\""));
    }
}
