use axum::{http::StatusCode, response::{IntoResponse, Response}, Json};
use mozart_core::ctl::schema::Response as ControlResponse;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub reason: String,
    pub(crate) needs_refresh: bool,
}

pub fn code_status(code: &str) -> StatusCode {
    match code {
        "Ok" => StatusCode::OK,
        "InvalidArgument" | "BadRequest" => StatusCode::BAD_REQUEST,
        "NotLoaded" | "NotReady" | "Busy" | "Rejected" | "ResultUnavailable" => StatusCode::CONFLICT,
        "Unsupported" | "UnknownCommand" => StatusCode::NOT_IMPLEMENTED,
        "InferenceFailed" | "Internal" => StatusCode::INTERNAL_SERVER_ERROR,
        "NotFound" => StatusCode::NOT_FOUND,
        "MethodNotAllowed" => StatusCode::METHOD_NOT_ALLOWED,
        "PayloadTooLarge" => StatusCode::PAYLOAD_TOO_LARGE,
        "UnsupportedMediaType" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "Forbidden" => StatusCode::FORBIDDEN,
        "UploadTimeout" => StatusCode::REQUEST_TIMEOUT,
        "UploadBusy" => StatusCode::TOO_MANY_REQUESTS,
        "UpstreamUnavailable" | "SourceUnavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "GatewayTimeout" => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    }
}

impl ApiError {
    pub fn new(status: StatusCode, code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self { status, code: code.into(), reason: reason.into(), needs_refresh: false }
    }
    pub fn code(code: &str, reason: impl Into<String>) -> Self { Self::new(code_status(code), code, reason) }
    pub(crate) fn source_changed(reason: impl Into<String>) -> Self {
        let mut error = Self::code("SourceUnavailable", reason);
        error.needs_refresh = true;
        error
    }
    pub fn remote(response: ControlResponse) -> Self { Self::code(&response.code, response.reason) }
    pub fn body(&self) -> Value { json!({"status":"error", "code":self.code, "reason":self.reason, "data":null}) }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}: {}", self.code, self.reason) }
}
impl std::error::Error for ApiError {}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response { (self.status, Json(self.body())).into_response() }
}

pub fn ok(data: Value) -> Response {
    Json(json!({"status":"ok", "code":"Ok", "reason":"", "data":data})).into_response()
}
pub fn control(response: ControlResponse) -> Response {
    (code_status(&response.code), Json(response)).into_response()
}
