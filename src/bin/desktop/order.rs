//! GUI 卡片顺序：拖放后保存到 `%APPDATA%\coding-quota\card_order.txt`。
//! 隐藏或未登录的平台仍占有排序位置，重新显示时不会丢失用户安排。

use coding_quota::model::{ProviderId, ProviderReport};
use std::io;
use std::path::{Path, PathBuf};

fn order_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("coding-quota/card_order.txt"))
}

fn decode(text: &str) -> Vec<ProviderId> {
    let mut order = Vec::with_capacity(ProviderId::ALL.len());
    for provider in text.lines().filter_map(ProviderId::parse_filter) {
        if !order.contains(&provider) {
            order.push(provider);
        }
    }
    // 新平台或旧的部分顺序文件：按默认顺序补齐，不抹掉已有的排序。
    for provider in ProviderId::ALL {
        if !order.contains(&provider) {
            order.push(provider);
        }
    }
    order
}

pub fn load() -> Vec<ProviderId> {
    let text = order_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    decode(&text)
}

fn save_to(path: &Path, order: &[ProviderId]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for provider in order {
        text.push_str(super::tray::provider_key(*provider));
        text.push('\n');
    }
    let pending = path.with_extension(format!("txt.tmp.{}", std::process::id()));
    let result = std::fs::write(&pending, text).and_then(|()| std::fs::rename(&pending, path));
    if result.is_err() {
        let _ = std::fs::remove_file(pending);
    }
    result
}

pub fn save(order: &[ProviderId]) -> io::Result<()> {
    let path = order_file()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA is not set"))?;
    save_to(&path, order)
}

/// 只在报表到达或顺序改变时排序；不在每帧分配、复制报表或模型用量。
pub fn apply(reports: &mut [ProviderReport], order: &[ProviderId]) {
    reports.sort_unstable_by_key(|report| {
        order
            .iter()
            .position(|provider| *provider == report.provider)
            .unwrap_or(order.len() + report.provider.ordinal())
    });
}

/// 把 source 放到 target 前/后。原地旋转，仅改变位置，不丢失隐藏平台。
pub fn move_relative(
    order: &mut [ProviderId],
    source: ProviderId,
    target: ProviderId,
    before: bool,
) -> bool {
    let Some(from) = order.iter().position(|provider| *provider == source) else {
        return false;
    };
    let Some(target_index) = order.iter().position(|provider| *provider == target) else {
        return false;
    };
    if source == target {
        return false;
    }
    let boundary = target_index + usize::from(!before);
    let to = boundary - usize::from(boundary > from);
    if from < to {
        order[from..=to].rotate_left(1);
    } else if from > to {
        order[to..=from].rotate_right(1);
    } else {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_up_down_and_adjacent_drops_preserves_every_provider() {
        let mut order = vec![ProviderId::Codex, ProviderId::Glm, ProviderId::Kimi];
        assert!(move_relative(
            &mut order,
            ProviderId::Codex,
            ProviderId::Glm,
            false
        ));
        assert_eq!(
            order,
            [ProviderId::Glm, ProviderId::Codex, ProviderId::Kimi]
        );
        assert!(!move_relative(
            &mut order,
            ProviderId::Codex,
            ProviderId::Glm,
            false
        ));
        assert!(move_relative(
            &mut order,
            ProviderId::Kimi,
            ProviderId::Glm,
            true
        ));
        assert_eq!(
            order,
            [ProviderId::Kimi, ProviderId::Glm, ProviderId::Codex]
        );
        assert!(!move_relative(
            &mut order,
            ProviderId::Kimi,
            ProviderId::Kimi,
            false
        ));
        assert!(!move_relative(
            &mut order,
            ProviderId::Deepseek,
            ProviderId::Codex,
            true
        ));
        assert!(!move_relative(
            &mut order,
            ProviderId::Codex,
            ProviderId::Deepseek,
            false
        ));
    }

    #[test]
    fn partial_order_ignores_unknowns_and_duplicates_and_keeps_hidden_positions() {
        let mut order = decode("kimi\nunknown-provider\nkimi\ncodex\n");
        assert_eq!(&order[..2], [ProviderId::Kimi, ProviderId::Codex]);
        assert_eq!(order.len(), ProviderId::ALL.len());
        let hidden_index = order.iter().position(|p| *p == ProviderId::Claude).unwrap();
        assert!(move_relative(
            &mut order,
            ProviderId::Deepseek,
            ProviderId::Glm,
            true
        ));
        assert!(order.contains(&ProviderId::Claude));
        assert_eq!(
            order.iter().filter(|p| **p == ProviderId::Claude).count(),
            1
        );
        // 当前没授权的 Claude 不显示；恢复授权后仍在它保存的位置。
        let mut reports: Vec<_> = [
            ProviderId::Glm,
            ProviderId::Codex,
            ProviderId::Kimi,
            ProviderId::Claude,
        ]
        .into_iter()
        .map(|p| ProviderReport::ok(p, p.title(), None, None, vec![]))
        .collect();
        apply(&mut reports, &order);
        assert_eq!(
            reports.iter().map(|r| r.provider).collect::<Vec<_>>(),
            [
                ProviderId::Kimi,
                ProviderId::Codex,
                ProviderId::Claude,
                ProviderId::Glm
            ]
        );
        assert!(order.iter().position(|p| *p == ProviderId::Claude).unwrap() >= hidden_index);
    }

    #[test]
    fn saved_reorder_survives_restart_and_replaces_previous_preference() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "cq-card-order-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let path = dir.join("card_order.txt");
        let result = (|| {
            let mut order = decode("");
            move_relative(&mut order, ProviderId::Deepseek, ProviderId::Codex, true);
            save_to(&path, &order)?;
            let mut restarted = decode(&std::fs::read_to_string(&path)?);
            assert_eq!(restarted[0], ProviderId::Deepseek);
            move_relative(
                &mut restarted,
                ProviderId::Codex,
                ProviderId::Deepseek,
                true,
            );
            save_to(&path, &restarted)?;
            let restarted = decode(&std::fs::read_to_string(&path)?);
            assert_eq!(&restarted[..2], [ProviderId::Codex, ProviderId::Deepseek]);
            assert_eq!(restarted.len(), ProviderId::ALL.len());
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(dir);
        result
    }
}
