use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use fastapi::{IntoParams, OpenApi, ToSchema};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use url::Url;

use crate::{
    db::{ReplayDb, ReplayListResponse, ReplaySummary},
    gather::{self, GatheredBattleRow},
    map_assets::{MapFeature, MapFeaturesResponse, MapListResponse, MapService, MetalSpot},
    maps, parse,
    parse::{
        AllyTeamSnapshotRecord, BuildCommand, CommandOptionFlags, CommandRecord, DecodedCommand,
        DecodedTarget, EventRecord, InsertedCommand, MapSize, ParsedReplay, PlayerMetadata,
        RadarContact, RemovedCommand, SnapshotRecord, TeamMetadata, UnitSnapshot,
    },
};

const SWAGGER_UI_HTML: &str = r##"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <title>zkscraper Replay DB API</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
      window.ui = SwaggerUIBundle({
        url: "/openapi.json",
        dom_id: "#swagger-ui",
      });
    </script>
  </body>
</html>
"##;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 3000;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

#[derive(Debug, Clone)]
pub struct AppState {
    pub db: ReplayDb,
    pub maps: Option<MapService>,
    pub site_url: Url,
    pub min_req_wait: u32,
    pub zk_path: Option<PathBuf>,
    pub append_guard: Arc<Semaphore>,
    pub openapi_json: Arc<String>,
}

#[derive(Debug, Clone, Deserialize, IntoParams, Serialize, ToSchema)]
pub struct ReplayListQuery {
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, IntoParams, Serialize, ToSchema)]
pub struct ReplayQuery {
    #[serde(default)]
    pub snapshot_frame: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ReplayFramesResponse {
    pub replay_id: String,
    pub frames: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct AppendReplaysRequest {
    pub battle_ids: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct AppendLocalReplaysRequest {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct AppendReplayResult {
    pub replay_id: String,
    pub battle_id: Option<u64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct AppendReplaysResponse {
    pub requested: usize,
    pub inserted: usize,
    pub skipped_existing: usize,
    pub failed: usize,
    pub items: Vec<AppendReplayResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ApiErrorBody {
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

fn append_busy_error(message: &str) -> bool {
    message.contains("another replay parse is already in progress")
        || message.contains("spring-headless.exe is already running")
        || message.contains("only one replay parse may run at a time")
}

fn append_api_error(message: String) -> ApiError {
    let status = if append_busy_error(&message) {
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    ApiError { status, message }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Cbor(ApiErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Clone)]
struct Cbor<T>(T);

impl<T> IntoResponse for Cbor<T>
where
    T: Serialize,
{
    fn into_response(self) -> Response {
        match minicbor_serde::to_vec(&self.0) {
            Ok(bytes) => {
                let mut headers = HeaderMap::new();
                headers.insert(header::CONTENT_TYPE, "application/cbor".parse().unwrap());
                (headers, bytes).into_response()
            }
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to encode cbor response: {err}"),
            )
                .into_response(),
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        list_replays,
        get_replay,
        get_replay_frames,
        append_replays,
        append_local_replays,
        list_maps,
        get_map_heightmap,
        get_map_features
    ),
    components(schemas(
        ApiErrorBody,
        HealthResponse,
        ReplayListQuery,
        ReplayQuery,
        ReplayFramesResponse,
        AppendReplaysRequest,
        AppendLocalReplaysRequest,
        AppendReplayResult,
        AppendReplaysResponse,
        ReplayListResponse,
        ReplaySummary,
        MapListResponse,
        MapFeaturesResponse,
        MetalSpot,
        MapFeature,
        ParsedReplay,
        PlayerMetadata,
        TeamMetadata,
        MapSize,
        SnapshotRecord,
        AllyTeamSnapshotRecord,
        UnitSnapshot,
        RadarContact,
        EventRecord,
        CommandRecord,
        DecodedCommand,
        CommandOptionFlags,
        DecodedTarget,
        BuildCommand,
        InsertedCommand,
        RemovedCommand
    )),
    info(
        title = "zkscraper Replay DB API",
        description = "HTTP API for a parsed replay sled database, with optional live replay append support"
    )
)]
struct ApiDoc;

pub fn app_state(
    db: ReplayDb,
    maps: Option<MapService>,
    site_url: Url,
    min_req_wait: u32,
    zk_path: Option<PathBuf>,
) -> AppState {
    let mut openapi_value =
        serde_json::to_value(ApiDoc::openapi()).expect("failed to serialize openapi document");
    rewrite_openapi_to_cbor(&mut openapi_value);
    let openapi_json =
        serde_json::to_string_pretty(&openapi_value).expect("failed to serialize openapi json");

    AppState {
        db,
        maps,
        site_url,
        min_req_wait,
        zk_path,
        append_guard: Arc::new(Semaphore::new(1)),
        openapi_json: Arc::new(openapi_json),
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/replays", get(list_replays))
        .route("/replays/append", post(append_replays))
        .route("/replays/append-local", post(append_local_replays))
        .route("/replays/{replay_id}", get(get_replay))
        .route("/replays/{replay_id}/frames", get(get_replay_frames))
        .route("/maps", get(list_maps))
        .route("/maps/{map_name}/heightmap.bmp", get(get_map_heightmap))
        .route("/maps/{map_name}/features", get(get_map_features))
        .route("/openapi.json", get(openapi_json))
        .route("/docs", get(swagger_ui))
        .with_state(state)
}

pub async fn serve(
    db: ReplayDb,
    maps: Option<MapService>,
    site_url: Url,
    min_req_wait: u32,
    zk_path: Option<PathBuf>,
    bind_addr: SocketAddr,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(
        listener,
        build_router(app_state(db, maps, site_url, min_req_wait, zk_path)),
    )
    .await?;
    Ok(())
}

#[fastapi::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Server is healthy", body = HealthResponse)
    )
)]
async fn healthz() -> Cbor<HealthResponse> {
    Cbor(HealthResponse {
        status: "ok".to_string(),
    })
}

#[fastapi::path(
    get,
    path = "/replays",
    params(ReplayListQuery),
    responses(
        (status = 200, description = "Paginated replay summaries", body = ReplayListResponse),
        (status = 400, description = "Invalid pagination request", body = ApiErrorBody),
        (status = 500, description = "Replay DB read failed", body = ApiErrorBody)
    )
)]
async fn list_replays(
    State(state): State<AppState>,
    Query(query): Query<ReplayListQuery>,
) -> Result<Cbor<ReplayListResponse>, ApiError> {
    let offset = query.offset.unwrap_or(0);
    let raw_limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if raw_limit == 0 {
        return Err(ApiError::bad_request("limit must be at least 1"));
    }
    let limit = raw_limit.min(MAX_LIMIT);
    let db = state.db.clone();
    let page = tokio::task::spawn_blocking(move || db.list_replay_summaries(offset, limit))
        .await
        .map_err(|err| ApiError::internal(format!("replay list task failed: {err}")))?
        .map_err(|err| ApiError::internal(format!("failed to list replays: {err}")))?;

    Ok(Cbor(page))
}

#[fastapi::path(
    get,
    path = "/replays/{replay_id}",
    responses(
        (status = 200, description = "Full replay record", body = ParsedReplay),
        (status = 404, description = "Replay not found", body = ApiErrorBody),
        (status = 500, description = "Replay DB read failed", body = ApiErrorBody)
    ),
    params(
        ReplayQuery,
        ("replay_id" = String, Path, description = "Replay ID to fetch")
    )
)]
async fn get_replay(
    State(state): State<AppState>,
    Path(replay_id): Path<String>,
    Query(query): Query<ReplayQuery>,
) -> Result<Cbor<serde_json::Value>, ApiError> {
    let db = state.db.clone();
    let replay_id_for_task = replay_id.clone();
    let replay = tokio::task::spawn_blocking(move || match query.snapshot_frame {
        Some(snapshot_frame) => {
            db.get_replay_frame_value_lossy(&replay_id_for_task, snapshot_frame)
        }
        None => db.get_replay_value_lossy(&replay_id_for_task),
    })
    .await
    .map_err(|err| ApiError::internal(format!("replay read task failed: {err}")))?
    .map_err(|err| ApiError::internal(format!("failed to read replay {replay_id}: {err}")))?;

    match (replay, query.snapshot_frame) {
        (Some(replay), _) => Ok(Cbor(replay)),
        (None, Some(snapshot_frame)) => Err(ApiError::not_found(format!(
            "replay_id {replay_id} has no snapshot at frame {snapshot_frame}"
        ))),
        (None, None) => Err(ApiError::not_found(format!(
            "replay_id {replay_id} not found"
        ))),
    }
}

#[fastapi::path(
    get,
    path = "/replays/{replay_id}/frames",
    responses(
        (status = 200, description = "Ordered snapshot frame index for the replay", body = ReplayFramesResponse),
        (status = 404, description = "Replay not found", body = ApiErrorBody),
        (status = 500, description = "Replay DB read failed", body = ApiErrorBody)
    ),
    params(
        ("replay_id" = String, Path, description = "Replay ID to fetch frame index for")
    )
)]
async fn get_replay_frames(
    State(state): State<AppState>,
    Path(replay_id): Path<String>,
) -> Result<Cbor<ReplayFramesResponse>, ApiError> {
    let db = state.db.clone();
    let replay_id_for_task = replay_id.clone();
    let frames = tokio::task::spawn_blocking(move || db.get_replay_frames(&replay_id_for_task))
        .await
        .map_err(|err| ApiError::internal(format!("replay frames task failed: {err}")))?
        .map_err(|err| {
            ApiError::internal(format!("failed to read replay frames {replay_id}: {err}"))
        })?;

    match frames {
        Some(frames) => Ok(Cbor(ReplayFramesResponse { replay_id, frames })),
        None => Err(ApiError::not_found(format!(
            "replay_id {replay_id} not found"
        ))),
    }
}

#[fastapi::path(
    post,
    path = "/replays/append",
    responses(
        (status = 200, description = "Explicit battle IDs processed and merged into the live DB", body = AppendReplaysResponse),
        (status = 400, description = "Invalid append request", body = ApiErrorBody),
        (status = 409, description = "Another append request is already in progress", body = ApiErrorBody),
        (status = 503, description = "Append requires a configured Zero-K install", body = ApiErrorBody),
        (status = 500, description = "Append processing failed", body = ApiErrorBody)
    )
)]
async fn append_replays(
    State(state): State<AppState>,
    Json(payload): Json<AppendReplaysRequest>,
) -> Result<Cbor<AppendReplaysResponse>, ApiError> {
    if payload.battle_ids.is_empty() {
        return Err(ApiError::bad_request(
            "battle_ids must contain at least one battle ID",
        ));
    }

    let Some(zk_path) = state.zk_path.clone() else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "live append is unavailable without --zk-path".to_string(),
        });
    };

    let _guard = state
        .append_guard
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError {
            status: StatusCode::CONFLICT,
            message: "another append request is already in progress".to_string(),
        })?;

    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to create append runtime: {err}"))?
            .block_on(append_replays_impl(&state, payload, zk_path))
    })
    .await
    .map_err(|err| ApiError::internal(format!("append task failed: {err}")))?
    .map(Cbor)
    .map_err(append_api_error)
}

#[fastapi::path(
    post,
    path = "/replays/append-local",
    responses(
        (status = 200, description = "Local replay files processed and merged into the live DB", body = AppendReplaysResponse),
        (status = 400, description = "Invalid append request", body = ApiErrorBody),
        (status = 409, description = "Another append request is already in progress", body = ApiErrorBody),
        (status = 503, description = "Append requires a configured Zero-K install", body = ApiErrorBody),
        (status = 500, description = "Append processing failed", body = ApiErrorBody)
    )
)]
async fn append_local_replays(
    State(state): State<AppState>,
    Json(payload): Json<AppendLocalReplaysRequest>,
) -> Result<Cbor<AppendReplaysResponse>, ApiError> {
    if payload.paths.is_empty() {
        return Err(ApiError::bad_request(
            "paths must contain at least one replay path",
        ));
    }

    let Some(zk_path) = state.zk_path.clone() else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "live append is unavailable without --zk-path".to_string(),
        });
    };

    let _guard = state
        .append_guard
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError {
            status: StatusCode::CONFLICT,
            message: "another append request is already in progress".to_string(),
        })?;

    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to create append runtime: {err}"))?
            .block_on(append_local_replays_impl(&state, payload, zk_path))
    })
    .await
    .map_err(|err| ApiError::internal(format!("append task failed: {err}")))?
    .map(Cbor)
    .map_err(append_api_error)
}

#[fastapi::path(
    get,
    path = "/maps",
    responses(
        (status = 200, description = "Available maps from the local Zero-K maps directory", body = MapListResponse),
        (status = 503, description = "Map serving is not configured", body = ApiErrorBody),
        (status = 500, description = "Map list read failed", body = ApiErrorBody)
    )
)]
async fn list_maps(State(state): State<AppState>) -> Result<Cbor<MapListResponse>, ApiError> {
    let maps = state.maps.clone().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "map serving is unavailable without --zk-path".to_string(),
    })?;
    let response = tokio::task::spawn_blocking(move || maps.list_maps())
        .await
        .map_err(|err| ApiError::internal(format!("map list task failed: {err}")))?
        .map_err(|err| ApiError::internal(format!("failed to list maps: {err}")))?;

    Ok(Cbor(response))
}

#[fastapi::path(
    get,
    path = "/maps/{map_name}/heightmap.bmp",
    responses(
        (status = 200, description = "512x512 greyscale BMP heightmap"),
        (status = 404, description = "Map archive not found", body = ApiErrorBody),
        (status = 503, description = "Map serving is not configured", body = ApiErrorBody),
        (status = 500, description = "Map asset read failed", body = ApiErrorBody)
    ),
    params(
        ("map_name" = String, Path, description = "Map name to fetch")
    )
)]
async fn get_map_heightmap(
    State(state): State<AppState>,
    Path(map_name): Path<String>,
) -> Result<Response, ApiError> {
    let maps = state.maps.clone().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "map serving is unavailable without --zk-path".to_string(),
    })?;
    let requested_map = map_name.clone();
    let bmp = tokio::task::spawn_blocking(move || maps.heightmap_bmp(&requested_map))
        .await
        .map_err(|err| ApiError::internal(format!("map heightmap task failed: {err}")))?
        .map_err(|err| map_asset_error(&map_name, err))?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/bmp".parse().unwrap());
    Ok((headers, bmp).into_response())
}

#[fastapi::path(
    get,
    path = "/maps/{map_name}/features",
    responses(
        (status = 200, description = "Map feature data", body = MapFeaturesResponse),
        (status = 404, description = "Map archive not found", body = ApiErrorBody),
        (status = 503, description = "Map serving is not configured", body = ApiErrorBody),
        (status = 500, description = "Map asset read failed", body = ApiErrorBody)
    ),
    params(
        ("map_name" = String, Path, description = "Map name to fetch")
    )
)]
async fn get_map_features(
    State(state): State<AppState>,
    Path(map_name): Path<String>,
) -> Result<Cbor<MapFeaturesResponse>, ApiError> {
    let maps = state.maps.clone().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "map serving is unavailable without --zk-path".to_string(),
    })?;
    let requested_map = map_name.clone();
    let features = tokio::task::spawn_blocking(move || maps.map_features(&requested_map))
        .await
        .map_err(|err| ApiError::internal(format!("map feature task failed: {err}")))?
        .map_err(|err| map_asset_error(&map_name, err))?;

    Ok(Cbor(features))
}

async fn openapi_json(State(state): State<AppState>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    (headers, state.openapi_json.as_str().to_string()).into_response()
}

async fn swagger_ui() -> Html<&'static str> {
    Html(SWAGGER_UI_HTML)
}

fn map_asset_error(map_name: &str, err: impl std::fmt::Display) -> ApiError {
    let message = err.to_string();
    if message.contains("not found") {
        ApiError::not_found(format!("map '{map_name}' not found"))
    } else {
        ApiError::internal(format!("failed to read map '{map_name}': {message}"))
    }
}

async fn append_replays_impl(
    state: &AppState,
    payload: AppendReplaysRequest,
    zk_path: PathBuf,
) -> Result<AppendReplaysResponse, String> {
    let requested = dedupe_battle_ids(&payload.battle_ids);
    let mut items = Vec::with_capacity(requested.len());
    let mut pending = Vec::new();

    for battle_id in requested.iter().copied() {
        if state
            .db
            .contains_replay(&battle_id.to_string())
            .map_err(|err| format!("failed to inspect existing replay {battle_id}: {err}"))?
        {
            items.push(AppendReplayResult {
                replay_id: battle_id.to_string(),
                battle_id: Some(battle_id),
                status: "skipped_existing".to_string(),
                message: None,
            });
        } else {
            pending.push(battle_id);
        }
    }

    if pending.is_empty() {
        return Ok(summarize_append_results(items));
    }

    let temp_root = make_append_temp_root()?;
    let append_result = async {
        let battle_csv_path = temp_root.join("battles.csv");
        let replay_dir_path = temp_root.join("replays");
        let snapshot_work_path = temp_root.join("snapshot-work");

        let (rows, failures) = gather::gather_battle_rows_for_ids(
            state.site_url.clone(),
            state.min_req_wait,
            Some(zk_path.as_path()),
            &pending,
        )
        .await
        .map_err(|err| format!("failed to resolve battle metadata: {err}"))?;

        let failed_ids_from_gather = gather_failure_ids(&pending, &rows);
        let mut failure_messages = HashMap::new();
        for failure in failures {
            if let Some(battle_id) = failure
                .split_whitespace()
                .next()
                .and_then(|raw| raw.parse::<u64>().ok())
            {
                failure_messages.insert(battle_id, failure);
            }
        }

        for battle_id in failed_ids_from_gather {
            items.push(AppendReplayResult {
                replay_id: battle_id.to_string(),
                battle_id: Some(battle_id),
                status: "failed".to_string(),
                message: Some(
                    failure_messages
                        .remove(&battle_id)
                        .unwrap_or_else(|| "failed to resolve battle metadata".to_string()),
                ),
            });
        }

        if rows.is_empty() {
            return Ok(summarize_append_results(items));
        }

        gather::write_gathered_battle_csv(&battle_csv_path, &rows)
            .map_err(|err| format!("failed to write append battle CSV: {err}"))?;

        maps::download_maps(maps::DownloadMapsSettings {
            site_url: state.site_url.clone(),
            csv_path: battle_csv_path.clone(),
            min_req_wait: state.min_req_wait,
            zk_path: zk_path.clone(),
        })
        .await
        .map_err(|err| format!("failed to download required maps: {err}"))?;

        crate::fetch::fetch_replays(crate::fetch::FetchReplaySettings {
            site_url: state.site_url.clone(),
            csv_path: battle_csv_path,
            min_req_wait: state.min_req_wait,
            out_path: replay_dir_path.clone(),
        })
        .await
        .map_err(|err| format!("failed to download replays: {err}"))?;

        let parse_result = parse::parse_replays_into_db(
            parse::ParseReplaySettings {
                sdfz_in: replay_dir_path,
                zk_path,
                snapshot_path: snapshot_work_path,
            },
            state.db.clone(),
        )
        .await;

        let parse_error = parse_result.err().map(|err| err.to_string());
        for battle_id in rows.iter().map(|row| row.battle_id) {
            if state
                .db
                .contains_replay(&battle_id.to_string())
                .map_err(|err| format!("failed to inspect replay {battle_id}: {err}"))?
            {
                items.push(AppendReplayResult {
                    replay_id: battle_id.to_string(),
                    battle_id: Some(battle_id),
                    status: "inserted".to_string(),
                    message: None,
                });
            } else {
                items.push(AppendReplayResult {
                    replay_id: battle_id.to_string(),
                    battle_id: Some(battle_id),
                    status: "failed".to_string(),
                    message: Some(
                        parse_error
                            .clone()
                            .unwrap_or_else(|| "replay was not inserted".to_string()),
                    ),
                });
            }
        }

        Ok(summarize_append_results(items))
    }
    .await;

    if let Err(err) = std::fs::remove_dir_all(&temp_root) {
        eprintln!(
            "failed to clean up append temp directory '{}': {}",
            temp_root.display(),
            err
        );
    }

    append_result
}

async fn append_local_replays_impl(
    state: &AppState,
    payload: AppendLocalReplaysRequest,
    zk_path: PathBuf,
) -> Result<AppendReplaysResponse, String> {
    let temp_root = make_append_temp_root()?;
    let append_result = async {
        let replay_dir_path = temp_root.join("replays");
        std::fs::create_dir_all(&replay_dir_path)
            .map_err(|err| format!("failed to create replay staging dir: {err}"))?;

        let base_local_index = state
            .db
            .next_local_replay_id()
            .map_err(|err| format!("failed to allocate local replay id: {err}"))?;
        let mut next_local_index = base_local_index
            .strip_prefix("local-")
            .and_then(|suffix| suffix.parse::<u64>().ok())
            .ok_or_else(|| format!("invalid generated local replay id: {base_local_index}"))?;

        let manifest_path = replay_dir_path.join("replay_manifest.csv");
        let mut manifest = csv::Writer::from_path(&manifest_path)
            .map_err(|err| format!("failed to create local replay manifest: {err}"))?;
        manifest
            .write_record([
                "replay_id",
                "battle_id",
                "headless_id",
                "replay_filename",
                "game_version",
            ])
            .map_err(|err| format!("failed to write local replay manifest header: {err}"))?;

        let mut items = Vec::new();
        for raw_path in payload.paths {
            let source_path = PathBuf::from(&raw_path);
            if !source_path.is_file() {
                items.push(AppendReplayResult {
                    replay_id: String::new(),
                    battle_id: None,
                    status: "failed".to_string(),
                    message: Some(format!("replay file not found: {raw_path}")),
                });
                continue;
            }

            let replay_id = format!("local-{next_local_index}");
            next_local_index += 1;
            let replay_filename = format!("{replay_id}.sdfz");
            std::fs::copy(&source_path, replay_dir_path.join(&replay_filename))
                .map_err(|err| format!("failed to stage local replay '{raw_path}': {err}"))?;
            manifest
                .write_record([
                    replay_id.clone(),
                    String::new(),
                    next_local_index.to_string(),
                    replay_filename,
                    "local".to_string(),
                ])
                .map_err(|err| format!("failed to write local replay manifest row: {err}"))?;
            items.push(AppendReplayResult {
                replay_id,
                battle_id: None,
                status: "pending".to_string(),
                message: None,
            });
        }
        manifest
            .flush()
            .map_err(|err| format!("failed to flush local replay manifest: {err}"))?;

        let snapshot_work_path = temp_root.join("snapshot-work");
        let parse_error = parse::parse_replays_into_db(
            parse::ParseReplaySettings {
                sdfz_in: replay_dir_path,
                zk_path,
                snapshot_path: snapshot_work_path,
            },
            state.db.clone(),
        )
        .await
        .err()
        .map(|err| err.to_string());

        for item in &mut items {
            if item.status == "failed" {
                continue;
            }
            if state
                .db
                .contains_replay(&item.replay_id)
                .map_err(|err| format!("failed to inspect replay {}: {err}", item.replay_id))?
            {
                item.status = "inserted".to_string();
            } else {
                item.status = "failed".to_string();
                item.message = Some(
                    parse_error
                        .clone()
                        .unwrap_or_else(|| "replay was not inserted".to_string()),
                );
            }
        }

        Ok(summarize_append_results(items))
    }
    .await;

    if let Err(err) = std::fs::remove_dir_all(&temp_root) {
        eprintln!(
            "failed to clean up append temp directory '{}': {}",
            temp_root.display(),
            err
        );
    }

    append_result
}

fn dedupe_battle_ids(battle_ids: &[u64]) -> Vec<u64> {
    let mut seen = std::collections::HashSet::new();
    battle_ids
        .iter()
        .copied()
        .filter(|battle_id| seen.insert(*battle_id))
        .collect()
}

fn gather_failure_ids(requested: &[u64], rows: &[GatheredBattleRow]) -> Vec<u64> {
    let gathered: std::collections::HashSet<u64> = rows.iter().map(|row| row.battle_id).collect();
    requested
        .iter()
        .copied()
        .filter(|battle_id| !gathered.contains(battle_id))
        .collect()
}

fn summarize_append_results(mut items: Vec<AppendReplayResult>) -> AppendReplaysResponse {
    items.sort_by(|left, right| left.replay_id.cmp(&right.replay_id));
    let inserted = items
        .iter()
        .filter(|item| item.status == "inserted")
        .count();
    let skipped_existing = items
        .iter()
        .filter(|item| item.status == "skipped_existing")
        .count();
    let failed = items.iter().filter(|item| item.status == "failed").count();
    AppendReplaysResponse {
        requested: items.len(),
        inserted,
        skipped_existing,
        failed,
        items,
    }
}

fn make_append_temp_root() -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system clock error: {err}"))?
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("zkscraper-append-{}-{}", std::process::id(), stamp));
    std::fs::create_dir_all(&path)
        .map_err(|err| format!("failed to create append temp directory: {err}"))?;
    Ok(path)
}

fn rewrite_openapi_to_cbor(value: &mut serde_json::Value) {
    let Some(paths) = value
        .get_mut("paths")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for path_item in paths.values_mut() {
        let Some(operations) = path_item.as_object_mut() else {
            continue;
        };
        for operation in operations.values_mut() {
            let Some(responses) = operation
                .get_mut("responses")
                .and_then(serde_json::Value::as_object_mut)
            else {
                continue;
            };
            for response in responses.values_mut() {
                let Some(content) = response
                    .get_mut("content")
                    .and_then(serde_json::Value::as_object_mut)
                else {
                    continue;
                };
                if let Some(json_content) = content.remove("application/json") {
                    content.insert("application/cbor".to_string(), json_content);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
    };
    use std::io::Write;
    use tower::ServiceExt;
    use url::Url;

    use super::{app_state, build_router};
    use crate::{
        db::ReplayDb,
        map_assets::MapService,
        parse::{
            AllyTeamSnapshotRecord, CommandOptionFlags, CommandRecord, DecodedCommand,
            EconomySnapshot, EconomySnapshotRecord, EventRecord, MapSize, ParsedReplay,
            PlayerMetadata, RadarContact, SnapshotRecord, TeamMetadata, UnitSnapshot,
        },
    };

    fn decode_cbor_body<T: serde::de::DeserializeOwned>(
        body: &[u8],
    ) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
        minicbor_serde::from_slice(body).map_err(|err| err.into())
    }

    fn sample_replay(battle_id: u64) -> ParsedReplay {
        ParsedReplay {
            replay_id: battle_id.to_string(),
            battle_id: Some(battle_id),
            replay_filename: format!("{battle_id}.sdfz"),
            game_version: "1.0".to_string(),
            engine_version: "105.1".to_string(),
            map_name: Some("TestMap".to_string()),
            game_name: Some("Zero-K v1.0".to_string()),
            zksearchtag: Some("tag".to_string()),
            players: vec![PlayerMetadata {
                player_id: 1,
                name: Some("Tester".to_string()),
                team: Some(0),
                spectator: false,
                elo: None,
                lobby_id: None,
                country_code: None,
                clan: None,
                level: None,
            }],
            teams: vec![TeamMetadata {
                team_id: 0,
                allyteam: Some(0),
                teamleader: Some(1),
                handicap: Some(0.0),
            }],
            map_size: Some(MapSize { x: 16, z: 16 }),
            global_snapshots: vec![
                SnapshotRecord {
                    frame: 120,
                    game_seconds: 5.0,
                    units: vec![UnitSnapshot {
                        unit_id: 1,
                        unit_def_name: "cloakcon".to_string(),
                        team_id: 0,
                        allyteam_id: 0,
                        x: 1.0,
                        y: 2.0,
                        z: 3.0,
                        hp: 100.0,
                        max_hp: 100.0,
                        build_progress: 1.0,
                        heading: 0,
                        experience: 0.0,
                    }],
                },
                SnapshotRecord {
                    frame: 240,
                    game_seconds: 10.0,
                    units: vec![UnitSnapshot {
                        unit_id: 3,
                        unit_def_name: "cloakraid".to_string(),
                        team_id: 0,
                        allyteam_id: 0,
                        x: 7.0,
                        y: 8.0,
                        z: 9.0,
                        hp: 90.0,
                        max_hp: 100.0,
                        build_progress: 1.0,
                        heading: 128,
                        experience: 1.0,
                    }],
                },
            ],
            allyteam_snapshots: std::iter::once((
                0,
                vec![
                    AllyTeamSnapshotRecord {
                        allyteam_id: 0,
                        frame: 120,
                        game_seconds: 5.0,
                        los_units: vec![],
                        radar_contacts: vec![RadarContact {
                            unit_id: 2,
                            team_id: 1,
                            allyteam_id: 1,
                            x: 4.0,
                            y: 5.0,
                            z: 6.0,
                        }],
                    },
                    AllyTeamSnapshotRecord {
                        allyteam_id: 0,
                        frame: 240,
                        game_seconds: 10.0,
                        los_units: vec![UnitSnapshot {
                            unit_id: 3,
                            unit_def_name: "cloakraid".to_string(),
                            team_id: 0,
                            allyteam_id: 0,
                            x: 7.0,
                            y: 8.0,
                            z: 9.0,
                            hp: 90.0,
                            max_hp: 100.0,
                            build_progress: 1.0,
                            heading: 128,
                            experience: 1.0,
                        }],
                        radar_contacts: vec![],
                    },
                ],
            ))
            .collect(),
            economy_snapshots: std::iter::once((
                0,
                vec![
                    EconomySnapshotRecord {
                        team_id: 0,
                        allyteam_id: 0,
                        frame: 120,
                        game_seconds: 5.0,
                        economy: EconomySnapshot {
                            metal_income: 1.0,
                            energy_income: 2.0,
                            metal_stored: 3.0,
                            energy_stored: 4.0,
                            metal_storage: 5.0,
                            energy_storage: 6.0,
                            metal_pull: 7.0,
                            energy_pull: 8.0,
                            metal_expense: 9.0,
                            energy_expense: 10.0,
                            metal_share: 11.0,
                            energy_share: 12.0,
                            metal_sent: 13.0,
                            energy_sent: 14.0,
                            metal_received: 15.0,
                            energy_received: 16.0,
                        },
                    },
                    EconomySnapshotRecord {
                        team_id: 0,
                        allyteam_id: 0,
                        frame: 240,
                        game_seconds: 10.0,
                        economy: EconomySnapshot {
                            metal_income: 2.0,
                            energy_income: 3.0,
                            metal_stored: 4.0,
                            energy_stored: 5.0,
                            metal_storage: 6.0,
                            energy_storage: 7.0,
                            metal_pull: 8.0,
                            energy_pull: 9.0,
                            metal_expense: 10.0,
                            energy_expense: 11.0,
                            metal_share: 12.0,
                            energy_share: 13.0,
                            metal_sent: 14.0,
                            energy_sent: 15.0,
                            metal_received: 16.0,
                            energy_received: 17.0,
                        },
                    },
                ],
            ))
            .collect(),
            command_history: vec![CommandRecord {
                frame: 120,
                game_seconds: 5.0,
                player_id: 1,
                ai_id: None,
                command_id: 10,
                options: 0,
                params: vec![10.0, 20.0, 30.0],
                selected_unit_ids: vec![1],
                decoded: DecodedCommand {
                    kind: "move".to_string(),
                    option_flags: CommandOptionFlags {
                        alt: false,
                        ctrl: false,
                        meta: false,
                        shift: false,
                        right: false,
                        internal: false,
                    },
                    target: None,
                    state: None,
                    build: None,
                    inserted: None,
                    removed: None,
                },
            }],
            events: vec![EventRecord {
                event_type: "test".to_string(),
                frame: 120,
                game_seconds: 5.0,
                payload: serde_json::json!({"ok": true}),
            }],
            springie_stats: vec![],
        }
    }

    fn write_test_archive(
        root: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let maps_dir = root.join("maps");
        std::fs::create_dir_all(&maps_dir)?;
        let archive_path = maps_dir.join("TestMap.sdz");
        let file = std::fs::File::create(archive_path)?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        let width = 64_u32;
        let height = 64_u32;
        let sample_width = width + 1;
        let sample_height = height + 1;
        let sample_count = (sample_width * sample_height) as usize;
        let mut smf = Vec::new();
        smf.extend_from_slice(b"spring map file");
        smf.push(0);
        smf.extend_from_slice(&1_u32.to_le_bytes());
        smf.extend_from_slice(&0_u32.to_le_bytes());
        smf.extend_from_slice(&1_u32.to_le_bytes());
        smf.extend_from_slice(&1_u32.to_le_bytes());
        smf.extend_from_slice(&8_u32.to_le_bytes());
        smf.extend_from_slice(&8_u32.to_le_bytes());
        smf.extend_from_slice(&32_u32.to_le_bytes());
        smf.extend_from_slice(&0.0_f32.to_le_bytes());
        smf.extend_from_slice(&255.0_f32.to_le_bytes());
        smf.extend_from_slice(&76_u32.to_le_bytes());
        smf.extend_from_slice(&0_u32.to_le_bytes());
        smf.extend_from_slice(&0_u32.to_le_bytes());
        smf.extend_from_slice(&0_u32.to_le_bytes());
        smf.extend_from_slice(&0_u32.to_le_bytes());
        smf.extend_from_slice(&0_u32.to_le_bytes());
        for index in 0..sample_count {
            smf.extend_from_slice(&((index % 2048) as u16).to_le_bytes());
        }

        writer.start_file("maps/TestMap.smf", options)?;
        writer.write_all(&smf)?;
        writer.start_file("mapconfig/map_metal_layout.lua", options)?;
        writer.write_all(b"return { { x = 100, z = 200, metal = 2.5 } }")?;
        writer.start_file("mapconfig/featureplacer/set.lua", options)?;
        writer.write_all(b"return { { name = 'treetype1', x = 64, z = 96 } }")?;
        writer.finish()?;
        Ok(())
    }

    fn seeded_router(
        with_maps: bool,
    ) -> Result<axum::Router, Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path = temp_dir.keep();
        let replay_db = ReplayDb::open(&temp_path)?;
        for battle_id in [10_u64, 2_u64] {
            let replay = sample_replay(battle_id);
            replay_db.put_replay(&replay)?;
        }
        drop(replay_db);
        let replay_db = ReplayDb::open(&temp_path)?;
        let maps = if with_maps {
            write_test_archive(&temp_path)?;
            Some(MapService::from_zk_path(&temp_path)?)
        } else {
            None
        };
        Ok(build_router(app_state(
            replay_db,
            maps,
            Url::parse("https://zero-k.info")?,
            1_000,
            if with_maps {
                Some(temp_path.clone())
            } else {
                None
            },
        )))
    }

    #[tokio::test]
    async fn healthz_route_works() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/cbor"
        );
        Ok(())
    }

    #[tokio::test]
    async fn replay_list_is_paginated_and_sorted(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/replays?offset=0&limit=1")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = decode_cbor_body(&body)?;
        assert_eq!(parsed["items"][0]["battle_id"], 2);
        Ok(())
    }

    #[tokio::test]
    async fn replay_lookup_returns_404() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(Request::builder().uri("/replays/999").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn replay_frames_lookup_returns_frame_index(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/replays/2/frames")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = decode_cbor_body(&body)?;
        assert_eq!(parsed["replay_id"], "2");
        assert_eq!(parsed["frames"], serde_json::json!([120, 240]));
        Ok(())
    }

    #[tokio::test]
    async fn replay_lookup_can_filter_to_specific_snapshot_frame(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/replays/2?snapshot_frame=240")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/cbor"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = decode_cbor_body(&body)?;
        assert_eq!(parsed["global_snapshots"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["global_snapshots"][0]["frame"], 240);
        assert_eq!(
            parsed["allyteam_snapshots"]["0"].as_array().unwrap().len(),
            1
        );
        assert_eq!(parsed["allyteam_snapshots"]["0"][0]["frame"], 240);
        assert_eq!(
            parsed["economy_snapshots"]["0"].as_array().unwrap().len(),
            1
        );
        assert_eq!(parsed["economy_snapshots"]["0"][0]["frame"], 240);
        Ok(())
    }

    #[tokio::test]
    async fn replay_lookup_returns_404_for_missing_snapshot_frame(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/replays/2?snapshot_frame=999")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn openapi_route_exposes_replay_paths(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
        let paths = parsed["paths"]
            .as_object()
            .expect("openapi document should have paths");
        assert!(paths.contains_key("/replays"));
        assert!(paths.contains_key("/replays/append"));
        assert!(paths.contains_key("/replays/append-local"));
        assert!(paths.contains_key("/maps"));
        assert!(paths.contains_key("/replays/{replay_id}"));
        assert!(paths.contains_key("/replays/{replay_id}/frames"));
        assert!(paths.contains_key("/maps/{map_name}/features"));
        assert!(
            parsed["paths"]["/replays/append"]["post"]["requestBody"]["required"]
                .as_bool()
                .unwrap_or(false)
        );
        let replay_get = parsed["paths"]["/replays/{replay_id}"]["get"]
            .as_object()
            .expect("replay get operation should be an object");
        let parameters = replay_get["parameters"]
            .as_array()
            .expect("replay get operation should expose parameters");
        assert!(parameters.iter().any(|param| param["name"] == "replay_id"));
        assert!(parameters
            .iter()
            .any(|param| param["name"] == "snapshot_frame"));
        Ok(())
    }

    #[tokio::test]
    async fn map_routes_serve_assets() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(true)?;

        let bmp_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/maps/TestMap/heightmap.bmp")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(bmp_response.status(), StatusCode::OK);
        let bmp = to_bytes(bmp_response.into_body(), usize::MAX).await?;
        assert!(bmp.starts_with(b"BM"));

        let features_response = app
            .oneshot(
                Request::builder()
                    .uri("/maps/TestMap/features")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(features_response.status(), StatusCode::OK);
        let body = to_bytes(features_response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = decode_cbor_body(&body)?;
        assert_eq!(parsed["metal_spots"][0]["x"], 100.0);
        assert_eq!(parsed["features"][0]["name"], "treetype1");
        Ok(())
    }

    #[tokio::test]
    async fn map_list_route_lists_available_maps(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(true)?;
        let response = app
            .oneshot(Request::builder().uri("/maps").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = decode_cbor_body(&body)?;
        let items = parsed["items"]
            .as_array()
            .expect("items should be an array");
        assert!(items.iter().any(|item| item == "TestMap"));
        Ok(())
    }

    #[tokio::test]
    async fn append_route_returns_503_without_zk_path(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/replays/append")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"battle_ids":[123]}"#))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        Ok(())
    }

    #[tokio::test]
    async fn append_route_returns_409_when_append_is_in_progress(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = tempfile::tempdir()?;
        let replay_db = ReplayDb::open(temp_dir.path())?;
        let state = app_state(
            replay_db,
            None,
            Url::parse("https://zero-k.info")?,
            1_000,
            Some(temp_dir.path().to_path_buf()),
        );
        let _guard = state.append_guard.clone().try_acquire_owned()?;
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/replays/append")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"battle_ids":[123]}"#))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::CONFLICT);
        Ok(())
    }
}
