//! 会话自动重连的退避参数。
//!
//! 与隧道那条重连循环（`tunnel.rs` 里的固定阶梯）分开，是因为两边的取舍不同：
//! 隧道是常驻后台、没人盯着，可以一路退避到 60 秒反复试；会话是用户正在看的
//! 那一路，重试必须**看得见、能中止、有尽头**，否则网络断了界面就一直"连接中"。
//!
//! 三个概念各管一件事：
//! - `max_attempts`：一轮断开后最多自动试几次，0 = 完全关闭（回到"弹窗问你"）。
//! - 指数 + 抖动：`initial_delay_ms * 2^(n-1)`，封顶 `max_delay_ms`，再按
//!   `jitter_ratio` 向上加一点随机量 —— 一堆终端同时掉线时不要齐刷刷一起冲。
//! - `stable_secs`：连上撑过这么久再断，就当作一次全新的断开重新从第一次算，
//!   免得几周后偶发的一次掉线也要先等 60 秒。

use std::time::Duration;

/// 一次都算不过头的重试次数上限：设置里"无限"也只用这个封顶。
pub const MAX_ATTEMPTS_CAP: u32 = 9_999;

/// 自动重连退避参数。全部字段都能从 `settings.ini` 配。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconnectParams {
    /// 最多自动重连几次；0 表示关掉自动重连，改为弹窗询问。
    pub max_attempts: u32,
    /// 第一次重连前的等待基数。
    pub initial_delay_ms: u64,
    /// 等待的封顶，防止指数把用户晾在十分钟外。
    pub max_delay_ms: u64,
    /// 连上超过这么多秒后再断，重试轮次重新从第一次开始。
    pub stable_secs: u64,
    /// 抖动比例，0.0~0.5：只向上加，不会把等待压成 0。
    pub jitter_ratio: f64,
}

impl Default for ReconnectParams {
    fn default() -> Self {
        Self {
            // 3 次：够盖掉一次网络切换、又不至于让用户对着"连接中"干等
            max_attempts: 3,
            initial_delay_ms: 1_000,
            max_delay_ms: 60_000,
            stable_secs: 30,
            jitter_ratio: 0.2,
        }
    }
}

impl ReconnectParams {
    /// 第 `attempt` 次（从 1 起）重连前的等待基数，指数增长并封顶。
    pub fn base_delay_ms(&self, attempt: u32) -> u64 {
        // 先限住指数位，再乘：2^40 这种直接把 u64 乘溢出的写法在这里会炸
        let shift = attempt.saturating_sub(1).min(20);
        let step = self.initial_delay_ms.max(1).saturating_mul(1u64 << shift);
        step.clamp(1, self.max_delay_ms.max(1))
    }

    /// 加抖动后的实际等待。`spread` 是 [0,1) 的伪随机数，由调用方提供
    /// （这里不引随机依赖：退避要的可复现性远大于要的好熵）。
    pub fn delay_with_jitter(&self, attempt: u32, spread: f64) -> Duration {
        let base = self.base_delay_ms(attempt) as f64;
        let extra = base * self.jitter_ratio.clamp(0.0, 0.5) * spread.clamp(0.0, 1.0);
        Duration::from_millis((base + extra) as u64)
    }

    /// 这一轮断开算不算"上次那通已经站稳了"。
    pub fn was_stable(&self, up_secs: u64) -> bool {
        up_secs >= self.stable_secs
    }

    /// 界面上"重连第 n/m 次"的 m：0 显示成"关"。
    pub fn attempts_label(&self) -> String {
        match self.max_attempts {
            0 => "关".to_string(),
            n if n >= MAX_ATTEMPTS_CAP => "无限".to_string(),
            n => n.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(initial: u64, max: u64) -> ReconnectParams {
        ReconnectParams {
            max_attempts: 5,
            initial_delay_ms: initial,
            max_delay_ms: max,
            stable_secs: 30,
            jitter_ratio: 0.0,
        }
    }

    #[test]
    fn delays_double_then_cap() {
        let params = p(1_000, 8_000);
        assert_eq!(params.base_delay_ms(1), 1_000);
        assert_eq!(params.base_delay_ms(2), 2_000);
        assert_eq!(params.base_delay_ms(4), 8_000);
        assert_eq!(params.base_delay_ms(9), 8_000, "超过封顶不能继续涨");
    }

    #[test]
    fn huge_attempt_does_not_overflow() {
        let params = p(1_000, 60_000);
        assert_eq!(params.base_delay_ms(u32::MAX), 60_000);
    }

    #[test]
    fn jitter_only_pushes_up_within_ratio() {
        let params = ReconnectParams {
            jitter_ratio: 0.2,
            ..p(1_000, 60_000)
        };
        assert_eq!(params.delay_with_jitter(1, 0.0), Duration::from_millis(1_000));
        assert_eq!(params.delay_with_jitter(1, 1.0), Duration::from_millis(1_200));
        // 乱来的 spread 也不能把等待压到 0 或放大到天上
        assert_eq!(params.delay_with_jitter(1, -5.0), Duration::from_millis(1_000));
        assert_eq!(params.delay_with_jitter(1, 99.0), Duration::from_millis(1_200));
    }

    #[test]
    fn zero_initial_delay_still_waits_a_moment() {
        let params = p(0, 0);
        assert_eq!(params.base_delay_ms(1), 1, "退避为 0 会变成疯狂重连");
    }

    #[test]
    fn stability_is_a_threshold_not_a_duration() {
        let params = ReconnectParams::default();
        assert!(!params.was_stable(29));
        assert!(params.was_stable(30));
        assert_eq!(params.attempts_label(), "3");
        assert_eq!(ReconnectParams { max_attempts: 0, ..params }.attempts_label(), "关");
        assert_eq!(
            ReconnectParams { max_attempts: MAX_ATTEMPTS_CAP, ..params }.attempts_label(),
            "无限"
        );
    }
}
