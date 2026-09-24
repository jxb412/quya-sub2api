//! 真实流量形状模板缓存（0.4.5 基线没有 refresh::CredCache；这里只保留
//! 巡检需要的最小能力：记住最近一条真实 ChatGPT Codex `/responses` 请求的
//! url / 请求头 / body 形状）。
//!
//! 巡检与 BPS 通道都借这条模板做「形状外衣」，只替换 model / input / bearer，
//! 因此出站特征与真实业务流量一致。模板只保留**形状**，bearer 会在使用前被换成
//! 目标账号自己的 token；模板本身不落盘。

use std::sync::Mutex;

/// 单条真实流量的形状：url + 请求头 + body。
#[derive(Debug, Clone, Default)]
pub struct Template {
    pub url: String,
    /// 保持原始顺序与多值，避免重建时破坏头顺序指纹。
    pub headers: Vec<(String, Vec<String>)>,
    pub body: Vec<u8>,
    pub at_ms: u64,
}

/// 只保留最近一条的模板缓存。
#[derive(Default)]
pub struct TemplateCache {
    inner: Mutex<Option<Template>>,
}

impl TemplateCache {
    /// 记录一条真实流量形状。body 超过上限时只保留形状（清空 body，
    /// 由调用方回退到内置 body 模板），避免把大请求长期驻留内存。
    pub fn record(
        &self,
        url: &str,
        headers: Vec<(String, Vec<String>)>,
        body: &[u8],
        at_ms: u64,
        max_body_bytes: usize,
    ) {
        let body = if body.len() > max_body_bytes {
            Vec::new()
        } else {
            body.to_vec()
        };
        let template = Template {
            url: url.to_string(),
            headers,
            body,
            at_ms,
        };
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(template);
    }

    pub fn any_recent(&self) -> Option<Template> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// 当前毫秒时间戳（巡检/面板共用）。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
