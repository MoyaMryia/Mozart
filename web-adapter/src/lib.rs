pub mod error;
pub mod journal;
pub mod rpc;
pub mod streams;
pub mod transport;
pub mod uploads;
mod routes;

use std::{path::PathBuf, sync::Arc};
use axum::Router;
use mozart_core::config::WebAdapterConfig;
use error::ApiError;
use streams::Streams;
use uploads::UploadStore;

pub struct Application {
    pub router: Router,
    pub streams: Streams,
    pub uploads: Arc<UploadStore>,
}

/// Validate and describe bootstrap settings without opening sockets or upload storage.
pub fn config_description(config: &WebAdapterConfig) -> Result<serde_json::Value, ApiError> {
    routes::origins(&config.web.allowed_origins)?;
    Ok(serde_json::json!({"configured":config.web_configured,"listen":config.web.listen,
        "port":config.web.listen.port(),"static_dir":config.web.static_dir,"upload_dir":config.web.upload_dir}))
}

pub async fn build(config: WebAdapterConfig, config_path: Option<PathBuf>) -> Result<Application, ApiError> {
    let rpc = rpc::RpcClient::new(config_path, config.clone());
    let streams = Streams::new(rpc.clone(), config.clone());
    let origins = routes::origins(&config.web.allowed_origins)?;
    let uploads = UploadStore::new(&config, rpc.clone())?;
    let state = routes::State {
        config, rpc, streams: streams.clone(), uploads: uploads.clone(), origins,
    };
    Ok(Application { router: routes::router(state), streams, uploads })
}
