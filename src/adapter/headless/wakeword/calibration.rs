//! 唤醒命中打点与边界标定：纯函数，不含推理与门控状态。
//!
//! 分类器原始分数（未平滑、未过阈）以 80ms 一块产出，序列本身不受 `DETECTION_BUFFER_SIZE`
//! 平滑窗口与阈值影响，因此「峰值块相对唤醒词末端的偏移」是可标定量；平滑均值的越阈位置
//! 才是受窗口影响的量。两者一并打点，用于决定裁切点估计方式。

/// 单块分类器输出：原始分数 + 该块在说话人会话内的序号
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScorePoint {
    pub raw: f32,
    pub chunk_index: u64,
}

/// 一次命中的原始分数序列摘要：绝对块序号 + 原始分数
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationStats {
    /// 序列首块的绝对块序号，配合下标还原峰值块位置
    pub first_chunk_index: u64,
    pub raw_scores: Vec<f32>,
}

impl CalibrationStats {
    /// 峰值所在块的绝对序号；无有限分数时为 None
    pub fn peak_chunk_index(&self) -> Option<u64> {
        Some(self.first_chunk_index + self.peak_index()? as u64)
    }

    /// 峰值处的原始分数
    pub fn peak_score(&self) -> Option<f32> {
        let index = self.peak_index()?;
        Some(self.raw_scores[index])
    }

    /// 序列内峰值下标
    fn peak_index(&self) -> Option<usize> {
        self.raw_scores
            .iter()
            .enumerate()
            .filter(|(_, score)| score.is_finite())
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(index, _)| index)
    }

    /// 日志用紧凑序列：4 位小数、空格分隔
    pub fn scores_compact(&self) -> String {
        self.raw_scores
            .iter()
            .map(|score| format!("{score:.4}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 由原始分数序列构造摘要；`history` 为按下标升序的采样点
pub fn calibration_stats(history: &[ScorePoint]) -> CalibrationStats {
    let first_chunk_index = history.first().map(|point| point.chunk_index).unwrap_or(0);
    CalibrationStats {
        first_chunk_index,
        raw_scores: history.iter().map(|point| point.raw).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(chunk_index: u64, raw: f32) -> ScorePoint {
        ScorePoint { raw, chunk_index }
    }

    #[test]
    fn peak_uses_raw_scores_and_absolute_chunk_index() {
        let stats = calibration_stats(&[
            point(100, 0.01),
            point(101, 0.42),
            point(102, 0.88),
            point(103, 0.10),
        ]);
        assert_eq!(stats.peak_chunk_index(), Some(102));
        assert_eq!(stats.peak_score(), Some(0.88));
    }

    #[test]
    fn scores_compact_is_ordered_and_fixed_precision() {
        let stats = calibration_stats(&[point(1, 0.5), point(2, 0.25)]);
        assert_eq!(stats.scores_compact(), "0.5000 0.2500");
    }

    #[test]
    fn empty_or_non_finite_series_has_no_peak() {
        assert_eq!(calibration_stats(&[]).peak_chunk_index(), None);
        let stats = calibration_stats(&[point(5, f32::NAN), point(6, f32::NEG_INFINITY)]);
        assert_eq!(stats.peak_chunk_index(), None);
        assert_eq!(stats.peak_score(), None);
    }

    /// 峰值块序号即「标定余量 0」时的估计位置，Phase 1 在其上加实测 lag 换算裁切点
    #[test]
    fn peak_chunk_index_is_the_raw_estimate_of_the_boundary() {
        let stats = calibration_stats(&[point(10, 0.2), point(11, 0.9), point(12, 0.3)]);
        assert_eq!(stats.peak_chunk_index(), Some(11));
    }
}
