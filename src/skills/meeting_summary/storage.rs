use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::skill_storage_dir;

#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingSummaryData {
    pub title: String,
    pub time_range: String,
    pub participants: Vec<String>,
    pub discussion_points: Vec<DiscussionPoint>,
    pub action_items: Vec<ActionItem>,
    pub decisions: Vec<Decision>,
    pub full_transcript: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiscussionPoint {
    pub topic: String,
    pub summary: String,
    pub speakers: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActionItem {
    pub task: String,
    pub assignee: String,
    pub deadline: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Decision {
    pub decision: String,
    pub context: String,
    pub participants: Vec<String>,
}

pub fn storage_dir() -> PathBuf {
    skill_storage_dir!()
}

pub fn recordings_dir() -> PathBuf {
    storage_dir().join("recordings")
}

pub fn create_recording_dir() -> Result<PathBuf> {
    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let dir = recordings_dir().join(&timestamp);
    fs::create_dir_all(&dir).context(format!("创建录制目录失败: {}", dir.display()))?;
    Ok(dir)
}

pub fn save_transcript(dir: &PathBuf, transcript: &str) -> Result<()> {
    let path = dir.join("transcript.txt");
    fs::write(&path, transcript).context(format!("保存转录文本失败: {}", path.display()))?;
    Ok(())
}

pub fn save_summary_json(dir: &PathBuf, summary: &MeetingSummaryData) -> Result<()> {
    let path = dir.join("summary.json");
    let json = serde_json::to_string_pretty(summary).context("序列化总结数据失败")?;
    fs::write(&path, json).context(format!("保存总结JSON失败: {}", path.display()))?;
    Ok(())
}

pub fn save_summary_markdown(dir: &PathBuf, summary: &MeetingSummaryData) -> Result<()> {
    let path = dir.join("summary.md");
    let md = format_markdown(summary);
    fs::write(&path, md).context(format!("保存总结Markdown失败: {}", path.display()))?;
    Ok(())
}

fn format_markdown(summary: &MeetingSummaryData) -> String {
    let mut md = format!("# {}\n\n", summary.title);
    md.push_str(&format!("**时间**: {}\n\n", summary.time_range));

    if !summary.participants.is_empty() {
        md.push_str(&format!(
            "**参与者**: {}\n\n",
            summary.participants.join(", ")
        ));
    }

    if !summary.discussion_points.is_empty() {
        md.push_str("## 讨论要点\n\n");
        for point in &summary.discussion_points {
            md.push_str(&format!("### {}\n\n", point.topic));
            md.push_str(&format!("{}\n\n", point.summary));
            if !point.speakers.is_empty() {
                md.push_str(&format!("**发言人**: {}\n\n", point.speakers.join(", ")));
            }
        }
    }

    if !summary.action_items.is_empty() {
        md.push_str("## 行动项\n\n");
        for item in &summary.action_items {
            let deadline = item.deadline.as_deref().unwrap_or("无");
            md.push_str(&format!(
                "- [ ] {} (负责人: {}, 截止: {})\n",
                item.task, item.assignee, deadline
            ));
        }
        md.push('\n');
    }

    if !summary.decisions.is_empty() {
        md.push_str("## 决策记录\n\n");
        for decision in &summary.decisions {
            md.push_str(&format!("- **{}**\n", decision.decision));
            md.push_str(&format!("  背景: {}\n", decision.context));
            if !decision.participants.is_empty() {
                md.push_str(&format!("  参与者: {}\n", decision.participants.join(", ")));
            }
        }
        md.push('\n');
    }

    if !summary.full_transcript.is_empty() {
        md.push_str("## 完整转录\n\n");
        md.push_str(&format!("```\n{}\n```\n", summary.full_transcript));
    }

    md
}
