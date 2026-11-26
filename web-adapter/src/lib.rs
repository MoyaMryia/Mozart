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
        "allowed_origins":config.web.allowed_origins,"port":config.web.listen.port(),"static_dir":config.web.static_dir,"upload_dir":config.web.upload_dir}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_reports_configured_origins() {
        let mut config = WebAdapterConfig::load_from(None).unwrap();
        config.web.allowed_origins = vec!["http://console.example".into()];
        assert_eq!(config_description(&config).unwrap()["allowed_origins"],
                   serde_json::json!(["http://console.example"]));
    }
}

pub async fn build(config: WebAdapterConfig, config_path: Option<PathBuf>) -> Result<Application, ApiError> {
    let logs_dir = config_path.as_ref().and_then(|path| path.parent()).map(|root| root.join("run/logs"));
    let rpc = rpc::RpcClient::new(config_path, config.clone());
    let streams = Streams::new(rpc.clone(), config.clone());
    let origins = routes::origins(&config.web.allowed_origins)?;
    let uploads = UploadStore::new(&config, rpc.clone())?;
    let state = routes::State {
        config, rpc, streams: streams.clone(), uploads: uploads.clone(), origins, logs_dir,
    };
    Ok(Application { router: routes::router(state), streams, uploads })
}
