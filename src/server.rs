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
    pub openapi_json: Arc<String>,
}

#[derive(Debug, Clone, Deserialize, IntoParams, Serialize, ToSchema)]
pub struct ReplayListQuery {
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
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
    paths(healthz, list_replays, get_replay),
    components(
        schemas(
            ApiErrorBody,
            HealthResponse,
            ReplayListQuery,
            ReplayListResponse,
            ReplaySummary,
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
        )
    ),
    info(
        title = "zkscraper Replay DB API",
        description = "Read-only HTTP API for a parsed replay sled database"
    )
)]
struct ApiDoc;

pub fn app_state(db: ReplayDb) -> AppState {
    let openapi_json = ApiDoc::openapi()
        .to_pretty_json()
        .expect("failed to serialize openapi document");

    AppState {
        db,
        openapi_json: Arc::new(openapi_json),
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/replays", get(list_replays))
        .route("/replays/{battle_id}", get(get_replay))
        .route("/openapi.json", get(openapi_json))
        .route("/docs", get(swagger_ui))
        .with_state(state)
}

pub async fn serve(db: ReplayDb, bind_addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, build_router(app_state(db))).await?;
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
) -> Result<Json<ParsedReplay>, ApiError> {
    let db = state.db.clone();
    let replay = tokio::task::spawn_blocking(move || db.get_replay(battle_id))
        .await
        .map_err(|err| ApiError::internal(format!("replay read task failed: {err}")))?
        .map_err(|err| ApiError::internal(format!("failed to read replay {battle_id}: {err}")))?;

    match replay {
        Some(replay) => Ok(Json(replay)),
        None => Err(ApiError::not_found(format!("battle_id {battle_id} not found"))),
    }
}

async fn openapi_json(State(state): State<AppState>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    (headers, state.openapi_json.as_str().to_string()).into_response()
}

async fn swagger_ui() -> Html<&'static str> {
    Html(SWAGGER_UI_HTML)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::{app_state, build_router};
    use crate::{
        db::ReplayDb,
        parse::{
            AllyTeamSnapshotRecord, CommandOptionFlags, CommandRecord, DecodedCommand,
            EventRecord, MapSize, ParsedReplay, PlayerMetadata, RadarContact, SnapshotRecord,
            TeamMetadata, UnitSnapshot,
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
            global_snapshots: vec![SnapshotRecord {
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
            }],
            allyteam_snapshots: std::iter::once((
                0,
                vec![AllyTeamSnapshotRecord {
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
                }],
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

    fn seeded_router() -> Result<axum::Router, Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = tempfile::tempdir()?;
        let db = sled::open(temp_dir.path())?;
        for battle_id in [10_u64, 2_u64] {
            let replay = sample_replay(battle_id);
            let payload = serde_json::to_vec(&replay)?;
            let compressed = zstd::encode_all(payload.as_slice(), 3)?;
            db.insert(battle_id.to_string().as_bytes(), compressed)?;
        }
        db.flush()?;
        drop(db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        Ok(build_router(app_state(replay_db)))
    }

    #[tokio::test]
    async fn healthz_route_works() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router()?;
        let response = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn replay_list_is_paginated_and_sorted(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router()?;
        let response = app
            .oneshot(Request::builder().uri("/replays?offset=0&limit=1").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(parsed["items"][0]["battle_id"], 2);
        Ok(())
    }

    #[tokio::test]
    async fn replay_lookup_returns_404() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router()?;
        let response = app
            .oneshot(Request::builder().uri("/replays/999").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn openapi_route_exposes_replay_paths(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = seeded_router()?;
        let response = app
            .oneshot(Request::builder().uri("/openapi.json").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
        let paths = parsed["paths"]
            .as_object()
            .expect("openapi document should have paths");
        assert!(paths.contains_key("/replays"));
        assert!(paths.contains_key("/replays/{battle_id}"));
        Ok(())
    }
}
