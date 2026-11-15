//! 生效配置共享句柄（SPEC-04 运行时键即时生效）。
//! 写时复制：config set 运行时键后整体替换快照；已发出的快照（如在跑文件任务）
//! 保持不变，新操作读到新值。重启类键不走这里（只写 TOML，运行进程不动）。

use mozart_core::config::Config;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct SharedConfig {
    inner: Arc<RwLock<Arc<Config>>>,
}

impl SharedConfig {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    /// 取当前生效配置快照（Arc 克隆，廉价）。
    pub fn snapshot(&self) -> Arc<Config> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 整体替换生效配置（运行时键写回后重建）。
    pub fn replace(&self, config: Config) {
        *self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(config);
    }
}