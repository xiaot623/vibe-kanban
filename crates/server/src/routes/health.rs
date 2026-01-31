use axum::response::Json;
use serde::Serialize;
use utils::response::ApiResponse;

#[derive(Serialize)]
pub struct HealthResponse {
    service: String,
    version: String,
}

pub async fn health_check() -> Json<ApiResponse<HealthResponse>> {
    Json(ApiResponse::success(HealthResponse {
        service: "vibe-kanban".to_string(),
        version: "1".to_string(),
    }))
}
