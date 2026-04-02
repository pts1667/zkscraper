use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use fastapi::{IntoParams, OpenApi, ToSchema};
use serde::{Deserialize, Serialize};

use crate::{
    db::{ReplayDb, ReplayListResponse, ReplaySummary},
    map_assets::{MapFeature, MapFeaturesResponse, MapListResponse, MapService, MetalSpot},
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        list_replays,
        get_replay,
        list_maps,
        get_map_heightmap,
        get_map_features
    ),
    components(schemas(
        ApiErrorBody,
        HealthResponse,
        ReplayListQuery,
        ReplayQuery,
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
        description = "Read-only HTTP API for a parsed replay sled database"
    )
)]
struct ApiDoc;

pub fn app_state(db: ReplayDb, maps: Option<MapService>) -> AppState {
    let openapi_json = ApiDoc::openapi()
        .to_pretty_json()
        .expect("failed to serialize openapi document");

    AppState {
        db,
        maps,
        openapi_json: Arc::new(openapi_json),
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/replays", get(list_replays))
        .route("/replays/{battle_id}", get(get_replay))
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
    bind_addr: SocketAddr,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, build_router(app_state(db, maps))).await?;
    Ok(())
}

#[fastapi::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Server is healthy", body = HealthResponse)
    )
)]
async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
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
) -> Result<Json<ReplayListResponse>, ApiError> {
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

    Ok(Json(page))
}

#[fastapi::path(
    get,
    path = "/replays/{battle_id}",
    params(ReplayQuery),
    responses(
        (status = 200, description = "Full replay record", body = ParsedReplay),
        (status = 404, description = "Replay not found", body = ApiErrorBody),
        (status = 500, description = "Replay DB read failed", body = ApiErrorBody)
    ),
    params(
        ("battle_id" = u64, Path, description = "Battle ID to fetch")
    )
)]
async fn get_replay(
    State(state): State<AppState>,
    Path(battle_id): Path<u64>,
    Query(query): Query<ReplayQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = state.db.clone();
    let replay = tokio::task::spawn_blocking(move || match query.snapshot_frame {
        Some(snapshot_frame) => db.get_replay_frame_value_lossy(battle_id, snapshot_frame),
        None => db.get_replay_value_lossy(battle_id),
    })
    .await
    .map_err(|err| ApiError::internal(format!("replay read task failed: {err}")))?
    .map_err(|err| ApiError::internal(format!("failed to read replay {battle_id}: {err}")))?;

    match (replay, query.snapshot_frame) {
        (Some(replay), _) => Ok(Json(replay)),
        (None, Some(snapshot_frame)) => Err(ApiError::not_found(format!(
            "battle_id {battle_id} has no snapshot at frame {snapshot_frame}"
        ))),
        (None, None) => Err(ApiError::not_found(format!(
            "battle_id {battle_id} not found"
        ))),
    }
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
async fn list_maps(State(state): State<AppState>) -> Result<Json<MapListResponse>, ApiError> {
    let maps = state.maps.clone().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "map serving is unavailable without --zk-path".to_string(),
    })?;
    let response = tokio::task::spawn_blocking(move || maps.list_maps())
        .await
        .map_err(|err| ApiError::internal(format!("map list task failed: {err}")))?
        .map_err(|err| ApiError::internal(format!("failed to list maps: {err}")))?;

    Ok(Json(response))
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
) -> Result<Json<MapFeaturesResponse>, ApiError> {
    let maps = state.maps.clone().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "map serving is unavailable without --zk-path".to_string(),
    })?;
    let requested_map = map_name.clone();
    let features = tokio::task::spawn_blocking(move || maps.map_features(&requested_map))
        .await
        .map_err(|err| ApiError::internal(format!("map feature task failed: {err}")))?
        .map_err(|err| map_asset_error(&map_name, err))?;

    Ok(Json(features))
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

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use std::io::Write;
    use tower::ServiceExt;

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

    fn sample_replay(battle_id: u64) -> ParsedReplay {
        ParsedReplay {
            battle_id,
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
        Ok(build_router(app_state(replay_db, maps)))
    }

    #[tokio::test]
    async fn healthz_route_works() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router(false)?;
        let response = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
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
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
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
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(parsed["global_snapshots"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["global_snapshots"][0]["frame"], 240);
        assert_eq!(parsed["allyteam_snapshots"]["0"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["allyteam_snapshots"]["0"][0]["frame"], 240);
        assert_eq!(parsed["economy_snapshots"]["0"].as_array().unwrap().len(), 1);
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
        assert!(paths.contains_key("/maps"));
        assert!(paths.contains_key("/replays/{battle_id}"));
        assert!(paths.contains_key("/maps/{map_name}/features"));
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
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
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
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
        let items = parsed["items"]
            .as_array()
            .expect("items should be an array");
        assert!(items.iter().any(|item| item == "TestMap"));
        Ok(())
    }
}
