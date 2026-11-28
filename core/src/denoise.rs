//! 降噪后端单点目录（SPEC-02）：统一管理所有可选降噪模型标识、所在阶段与能力元数据。
//! 新增降噪模型只需在此注册描述符并在 pre 实现对应处理模块，配置校验、控制协议、
//! 状态上报与前端选项列表自动联动扩展。

use serde::{Deserialize, Serialize};

use crate::ctl::schema::DenoiseBackendInfo;

/// 降噪模型在预处理链路中的物理执行阶段（SPEC-02）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseStage {
    /// 48 kHz 主干实时同步处理（位于 Decimator 与 direct 支路之前）。
    Trunk48k,
    /// 16 kHz 抽取后异步送入独立 `mozart-enhance` IPC 推理。
    Enhance16k,
}

impl DenoiseStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trunk48k => "48k_sync",
            Self::Enhance16k => "16k_enhance_ipc",
        }
    }
}

/// 可选降噪后端（不包括"关闭"态；开关由 `PreDspConfig::denoise` 控制，
/// 或经运行时复合键 `pre.dsp.denoise_mode` 统一选择 `"off" | <backend>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DenoiseBackend {
    #[default]
    Wola,
    Ulunas,
}

/// 降噪模型静态描述符：所有对外展示名、ID 与阶段依赖的唯一定义源。
#[derive(Debug, Clone, Copy)]
pub struct DenoiseBackendSpec {
    pub backend: DenoiseBackend,
    pub id: &'static str,
    pub label: &'static str,
    pub stage: DenoiseStage,
}

pub const DENOISE_BACKENDS: &[DenoiseBackendSpec] = &[
    DenoiseBackendSpec {
        backend: DenoiseBackend::Wola,
        id: "wola",
        label: "WOLA",
        stage: DenoiseStage::Trunk48k,
    },
    DenoiseBackendSpec {
        backend: DenoiseBackend::Ulunas,
        id: "ulunas",
        label: "UL-UNAS",
        stage: DenoiseStage::Enhance16k,
    },
];

impl DenoiseBackend {
    pub fn parse(raw: &str) -> Option<Self> {
        DENOISE_BACKENDS
            .iter()
            .find(|spec| spec.id == raw)
            .map(|spec| spec.backend)
    }

    pub const fn spec(self) -> &'static DenoiseBackendSpec {
        match self {
            Self::Wola => &DENOISE_BACKENDS[0],
            Self::Ulunas => &DENOISE_BACKENDS[1],
        }
    }

    pub const fn as_str(self) -> &'static str {
        self.spec().id
    }

    pub const fn label(self) -> &'static str {
        self.spec().label
    }

    pub const fn stage(self) -> DenoiseStage {
        self.spec().stage
    }

    pub const fn requires_enhance(self) -> bool {
        matches!(self.stage(), DenoiseStage::Enhance16k)
    }

    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Wola => 0,
            Self::Ulunas => 1,
        }
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Wola),
            1 => Some(Self::Ulunas),
            _ => None,
        }
    }
}

/// 对外统一的"关闭 / 某降噪后端"单选状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseMode {
    Off,
    Backend(DenoiseBackend),
}

impl DenoiseMode {
    pub fn parse(raw: &str) -> Option<Self> {
        if raw == "off" {
            Some(Self::Off)
        } else {
            DenoiseBackend::parse(raw).map(Self::Backend)
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Backend(backend) => backend.as_str(),
        }
    }

    pub const fn from_parts(enabled: bool, backend: DenoiseBackend) -> Self {
        if enabled {
            Self::Backend(backend)
        } else {
            Self::Off
        }
    }
}

/// 生成供 `PRE_GET_STATUS` 上报的降噪后端能力目录。
pub fn backend_infos(enhance_available: bool) -> Vec<DenoiseBackendInfo> {
    DENOISE_BACKENDS
        .iter()
        .map(|spec| {
            let available = !spec.backend.requires_enhance() || enhance_available;
            DenoiseBackendInfo {
                id: spec.id.to_string(),
                label: spec.label.to_string(),
                stage: spec.stage.as_str().to_string(),
                available,
                unavailable_reason: if available {
                    None
                } else {
                    Some("需在配置中启用 [enhance] 模型".to_string())
                },
            }
        })
        .collect()
}

/// 返回允许的配置值列表文案（调试/错误提示共用）。
pub fn supported_backend_list() -> String {
    DENOISE_BACKENDS
        .iter()
        .map(|spec| spec.id)
        .collect::<Vec<_>>()
        .join("/")
}

/// 缺少 `[enhance]` 服务配置时拒绝需要该阶段的降噪后端（单点报错文案）。
pub fn enhance_required_error(backend: DenoiseBackend) -> String {
    format!("{} backend requires [enhance] configuration", backend.as_str())
}
