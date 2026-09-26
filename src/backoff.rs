//! 反复失败的平台按指数退避：连续失败把下次请求的间隔翻倍（5 分钟起，1 小时封顶），
//! 成功一次立刻恢复。Kimi 这种服务端持续空转的平台不再每轮白跑一次请求；
//! Claude 的额度端点与 Claude Code / omp 共享限流，429 之后也不该按原节奏硬撞。

use crate::model::ProviderId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Entry {
    failures: u32,
    retry_at: Instant,
}

/// 每个平台各自的失败计数与下次可请求时刻。只活在内存里：进程重启后重新探一次，
/// 这是期望行为（重启往往就是因为凭据/网络刚变化）。
#[derive(Debug)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    entries: HashMap<ProviderId, Entry>,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            entries: HashMap::new(),
        }
    }

    /// 该平台是否还在退避中：返回（连续失败次数，还需等待）。
    pub fn deferred(&self, provider: ProviderId) -> Option<(u32, Duration)> {
        let entry = self.entries.get(&provider)?;
        let now = Instant::now();
        (entry.retry_at > now).then(|| (entry.failures, entry.retry_at - now))
    }

    /// 本轮成功（或压根没发请求，例如平台没有凭据）：清零失败计数。
    pub fn succeed(&mut self, provider: ProviderId) {
        self.entries.remove(&provider);
    }

    /// 记一次失败：下次请求的间隔按 2 的幂拉长，上限 `max`。
    pub fn fail(&mut self, provider: ProviderId) {
        let failures = self
            .entries
            .get(&provider)
            .map_or(0, |entry| entry.failures)
            .saturating_add(1);
        let wait = self.interval(failures);
        self.entries.insert(
            provider,
            Entry {
                failures,
                retry_at: Instant::now() + wait,
            },
        );
    }

    fn interval(&self, failures: u32) -> Duration {
        // 左移次数先压住，避免失败很多次后 1u32 << n 溢出（16 次即到 65536 倍，早已封顶）。
        let steps = failures.saturating_sub(1).min(16);
        self.base.saturating_mul(1u32 << steps).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff() -> Backoff {
        Backoff::new(Duration::from_millis(20), Duration::from_millis(80))
    }

    #[test]
    fn interval_doubles_up_to_the_cap() {
        let backoff = backoff();
        assert_eq!(backoff.interval(1), Duration::from_millis(20));
        assert_eq!(backoff.interval(2), Duration::from_millis(40));
        assert_eq!(backoff.interval(3), Duration::from_millis(80));
        assert_eq!(backoff.interval(9), Duration::from_millis(80));
    }

    #[test]
    fn failures_back_off_per_provider_and_success_resets() {
        let mut backoff = backoff();
        assert!(backoff.deferred(ProviderId::Kimi).is_none());

        backoff.fail(ProviderId::Kimi);
        assert!(backoff.deferred(ProviderId::Kimi).is_some());
        backoff.fail(ProviderId::Kimi);
        let (failures, _) = backoff.deferred(ProviderId::Kimi).expect("still deferred");
        assert_eq!(failures, 2);

        backoff.fail(ProviderId::Claude);
        assert!(backoff.deferred(ProviderId::Claude).is_some());
        assert!(
            backoff.deferred(ProviderId::Kimi).is_some(),
            "退避按平台各算各的"
        );

        backoff.succeed(ProviderId::Kimi);
        assert!(backoff.deferred(ProviderId::Kimi).is_none());
        assert!(backoff.deferred(ProviderId::Claude).is_some());
    }

    #[test]
    fn deferral_expires_on_its_own() {
        let mut backoff = backoff();
        backoff.fail(ProviderId::Kimi);
        std::thread::sleep(Duration::from_millis(30));
        assert!(backoff.deferred(ProviderId::Kimi).is_none());
    }
}
