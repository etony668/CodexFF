//! Codex 高效工作流预设 — Sol 规划 + Luna 执行
//!
//! 把社区流行的 "Sol 派发/规划 + Luna Max 执行" 用法固化为可直接安装的
//! 自定义 Agent 文件 (~/.codex/agents/*.toml)。安装前自动备份原文件,
//! 支持卸载 (移入备份) 与从备份恢复, 全程不覆盖用户数据。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::codex_config;

/// 预设 Agent 文件名 (不含扩展名)
pub const PRESET_IDS: [&str; 3] = ["luna-worker", "sol-planner", "sol-reviewer"];

const BACKUP_DIR_NAME: &str = ".codexff-backup";

/// 读取当前供应商模型目录 (~/.codex/codexff-model-catalog.json) 的模型 slug 列表,
/// 供高效工作流自定义模型时下拉选择。文件缺失/损坏返回空列表。
pub fn list_catalog_models() -> Vec<String> {
    let path = codex_config::codex_config_dir().join(codex_config::CODEXFF_MODEL_CATALOG_FILENAME);
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(models) = json.get("models").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|m| m.get("slug").and_then(|s| s.as_str()).map(String::from))
        .collect()
}

/// Codex 支持的思考档位 (含社区常见的 minimal)
pub const ALLOWED_EFFORTS: [&str; 7] = [
    "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("未知的预设: {0}")]
    UnknownPreset(String),
    #[error("Agent 文件解析失败 ({path}): {detail}")]
    Parse { path: String, detail: String },
    #[error("{0}")]
    NotFound(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowAgentInfo {
    /// 文件名主干, 如 "luna-worker"
    pub id: String,
    /// Agent 文件绝对路径
    pub path: String,
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// 文件当前是否已安装 (存在于 agents 目录)
    pub installed: bool,
    /// 是否 CodexFF 内置预设 (luna-worker / sol-planner / sol-reviewer)
    pub preset: bool,
    /// 已安装的预设文件是否被用户自定义过 (内容与默认预设不一致)
    pub customized: bool,
    /// 是否存在可恢复备份
    pub backup_exists: bool,
    /// 文件最后修改时间 (unix 毫秒)
    pub modified_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowAgentsResult {
    pub codex_home: String,
    pub agents_dir: String,
    pub agents: Vec<WorkflowAgentInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowActionOutcome {
    pub id: String,
    pub removed: bool,
    pub backup_exists: bool,
}

#[derive(Debug, Deserialize)]
struct AgentToml {
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
    #[serde(rename = "model_reasoning_effort")]
    model_reasoning_effort: Option<String>,
}

struct PresetDef {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    model: &'static str,
    reasoning_effort: &'static str,
    instructions: &'static str,
}

const PRESETS: [PresetDef; 3] = [
    PresetDef {
        id: "luna-worker",
        name: "luna_worker",
        description: "使用 GPT-5.6 Luna Max 执行边界清晰的委派任务",
        model: "gpt-5.6-luna",
        reasoning_effort: "max",
        instructions: "luna_worker 只负责处理范围明确、边界清晰、可以独立完成的委派任务。不要修改整体任务目标，也不要自行扩大工作范围；完成后汇报改动清单。",
    },
    PresetDef {
        id: "sol-planner",
        name: "sol_planner",
        description: "使用 GPT-5.6 Sol xHigh 拆解复杂任务并产出可执行计划",
        model: "gpt-5.6-sol",
        reasoning_effort: "xhigh",
        instructions: "sol_planner 负责把复杂目标拆解为边界清晰、可独立执行的子任务清单，并为每个子任务写明验收标准。不要擅自实现代码；输出计划后等待主任务派发。",
    },
    PresetDef {
        id: "sol-reviewer",
        name: "sol_reviewer",
        description: "使用 GPT-5.6 Sol xHigh 审查复杂代码与改动",
        model: "gpt-5.6-sol",
        reasoning_effort: "xhigh",
        instructions: "sol_reviewer 负责复杂任务的整体审查：正确性、边界条件、安全性、回归风险，按优先级输出问题清单与修改建议。普通审查交给 luna_worker。",
    },
];

fn agents_dir() -> PathBuf {
    codex_config::codex_config_dir().join("agents")
}

fn backup_dir() -> PathBuf {
    agents_dir().join(BACKUP_DIR_NAME)
}

fn agent_path(id: &str) -> PathBuf {
    agents_dir().join(format!("{id}.toml"))
}

fn backup_path(id: &str) -> PathBuf {
    backup_dir().join(format!("{id}.toml.bak"))
}

fn preset_def(id: &str) -> Option<&'static PresetDef> {
    PRESETS.iter().find(|p| p.id == id)
}

fn preset_toml(def: &PresetDef) -> String {
    format!(
        "# CodexFF 高效工作流预设 — 由 CodexFF 生成, 覆盖前自动备份\n\
         name = \"{}\"\n\
         description = \"{}\"\n\
         model = \"{}\"\n\
         model_reasoning_effort = \"{}\"\n\
         developer_instructions = \"\"\"{}\"\"\"\n",
        def.name, def.description, def.model, def.reasoning_effort, def.instructions
    )
}

fn modified_ms(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    m.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

fn read_agent_info(path: &Path, id: &str) -> Result<WorkflowAgentInfo, WorkflowError> {
    let text = std::fs::read_to_string(path).map_err(WorkflowError::Io)?;
    let parsed: AgentToml = toml::from_str(&text).map_err(|e| WorkflowError::Parse {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    let customized = preset_def(id)
        .map(|d| text.trim() != preset_toml(d).trim())
        .unwrap_or(false);
    Ok(WorkflowAgentInfo {
        id: id.to_string(),
        path: path.display().to_string(),
        name: parsed.name.unwrap_or_else(|| id.to_string()),
        description: parsed.description.unwrap_or_default(),
        model: parsed.model,
        reasoning_effort: parsed.model_reasoning_effort,
        installed: true,
        preset: PRESET_IDS.contains(&id),
        customized,
        backup_exists: backup_path(id).exists(),
        modified_ms: modified_ms(path),
    })
}

fn backup_existing(id: &str) -> Result<bool, WorkflowError> {
    let src = agent_path(id);
    if !src.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(backup_dir())?;
    std::fs::copy(&src, backup_path(id))?;
    Ok(true)
}

/// 扫描 ~/.codex/agents 下的全部自定义 Agent (预设优先排序)
pub fn list_workflow_agents() -> Result<WorkflowAgentsResult, WorkflowError> {
    let dir = agents_dir();
    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem == BACKUP_DIR_NAME {
                continue;
            }
            match read_agent_info(&path, stem) {
                Ok(info) => agents.push(info),
                // 解析失败的文件也列出, 便于用户发现格式问题 (保留文件不动)
                Err(e) => agents.push(WorkflowAgentInfo {
                    id: stem.to_string(),
                    path: path.display().to_string(),
                    name: stem.to_string(),
                    description: format!("解析失败: {e}"),
                    model: None,
                    reasoning_effort: None,
                    installed: true,
                    preset: PRESET_IDS.contains(&stem),
                    customized: false,
                    backup_exists: backup_path(stem).exists(),
                    modified_ms: modified_ms(&path),
                }),
            }
        }
    }
    // 预设优先, 其次按名称排序
    agents.sort_by_key(|a| {
        let order = PRESET_IDS.iter().position(|p| *p == a.id).unwrap_or(usize::MAX);
        (order, a.id.clone())
    });
    // 未安装的预设也补齐展示 (backup 可能存在)
    for def in PRESETS.iter() {
        if !agents.iter().any(|a| a.id == def.id) {
            agents.push(WorkflowAgentInfo {
                id: def.id.to_string(),
                path: agent_path(def.id).display().to_string(),
                name: def.name.to_string(),
                description: def.description.to_string(),
                model: Some(def.model.to_string()),
                reasoning_effort: Some(def.reasoning_effort.to_string()),
                installed: false,
                preset: true,
                customized: false,
                backup_exists: backup_path(def.id).exists(),
                modified_ms: None,
            });
        }
    }
    agents.sort_by_key(|a| {
        let order = PRESET_IDS.iter().position(|p| *p == a.id).unwrap_or(usize::MAX);
        (order, a.id.clone())
    });
    Ok(WorkflowAgentsResult {
        codex_home: codex_config::codex_config_dir().display().to_string(),
        agents_dir: dir.display().to_string(),
        agents,
    })
}

/// 安装预设 (已存在则先备份再覆盖)
pub fn install_workflow_preset(id: &str) -> Result<WorkflowAgentInfo, WorkflowError> {
    let def = preset_def(id).ok_or_else(|| WorkflowError::UnknownPreset(id.to_string()))?;
    std::fs::create_dir_all(agents_dir())?;
    backup_existing(id)?;
    std::fs::write(agent_path(id), preset_toml(def))?;
    read_agent_info(&agent_path(id), id)
}

fn sanitize_model(model: &str) -> Result<String, WorkflowError> {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.len() > 100 {
        return Err(WorkflowError::NotFound(
            "模型 ID 不能为空且长度不能超过 100 个字符".to_string(),
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '/'))
    {
        return Err(WorkflowError::NotFound(
            "模型 ID 只能包含字母、数字与 . - _ : /".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

/// 自定义预设的模型与思考档位 (覆盖前自动备份)
pub fn update_workflow_preset(
    id: &str,
    model: &str,
    reasoning_effort: &str,
) -> Result<WorkflowAgentInfo, WorkflowError> {
    let def = preset_def(id).ok_or_else(|| WorkflowError::UnknownPreset(id.to_string()))?;
    let model = sanitize_model(model)?;
    let effort = reasoning_effort.trim().to_ascii_lowercase();
    if !ALLOWED_EFFORTS.contains(&effort.as_str()) {
        return Err(WorkflowError::NotFound(format!(
            "不支持的思考档位: {effort}（可选: {}）",
            ALLOWED_EFFORTS.join(" / ")
        )));
    }
    let target = agent_path(id);
    if !target.exists() {
        return Err(WorkflowError::NotFound(format!(
            "「{}」尚未启用，请先启用后再自定义",
            def.name
        )));
    }
    backup_existing(id)?;
    let toml = format!(
        "# CodexFF 高效工作流预设 — 由 CodexFF 生成, 覆盖前自动备份\n\
         name = \"{}\"\n\
         description = \"{}\"\n\
         model = \"{}\"\n\
         model_reasoning_effort = \"{}\"\n\
         developer_instructions = \"\"\"{}\"\"\"\n",
        def.name, def.description, model, effort, def.instructions
    );
    std::fs::write(&target, toml)?;
    read_agent_info(&target, id)
}

/// 恢复全部预设到默认模型与档位 (各自先备份当前内容)
pub fn reset_workflow_presets() -> Result<Vec<WorkflowAgentInfo>, WorkflowError> {
    let mut infos = Vec::new();
    for def in PRESETS.iter() {
        infos.push(install_workflow_preset(def.id)?);
    }
    Ok(infos)
}

/// 卸载预设 (当前文件移入备份, 可恢复)
pub fn uninstall_workflow_preset(id: &str) -> Result<WorkflowActionOutcome, WorkflowError> {
    if !PRESET_IDS.contains(&id) {
        return Err(WorkflowError::UnknownPreset(id.to_string()));
    }
    let src = agent_path(id);
    let removed = if src.exists() {
        std::fs::create_dir_all(backup_dir())?;
        std::fs::copy(&src, backup_path(id))?;
        std::fs::remove_file(&src)?;
        true
    } else {
        false
    };
    Ok(WorkflowActionOutcome {
        id: id.to_string(),
        removed,
        backup_exists: backup_path(id).exists(),
    })
}

/// 从备份恢复预设 (仅当目标文件不存在时)
pub fn restore_workflow_preset(id: &str) -> Result<WorkflowAgentInfo, WorkflowError> {
    let def = preset_def(id).ok_or_else(|| WorkflowError::UnknownPreset(id.to_string()))?;
    let target = agent_path(id);
    if target.exists() {
        return Err(WorkflowError::NotFound(format!(
            "「{}」已安装, 无需恢复",
            def.name
        )));
    }
    let backup = backup_path(id);
    if !backup.exists() {
        return Err(WorkflowError::NotFound(format!(
            "「{}」没有可恢复的备份",
            def.name
        )));
    }
    std::fs::create_dir_all(agents_dir())?;
    std::fs::copy(&backup, &target)?;
    read_agent_info(&target, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_lifecycle() {
        let _env = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "codexff-workflow-test-{}",
            std::process::id()
        ));
        let agents = tmp.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        // 临时目录, 用 CODEX_HOME 指向它 (每次调用实时读取)
        unsafe {
            std::env::set_var("CODEX_HOME", &tmp);
        }

        // 初始: 三个预设都应列出且未安装
        let list = list_workflow_agents().unwrap();
        assert_eq!(list.agents.len(), 3);
        assert!(list.agents.iter().all(|a| !a.installed && a.preset));

        // 安装 luna-worker
        let info = install_workflow_preset("luna-worker").unwrap();
        assert!(info.installed);
        assert_eq!(info.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(info.reasoning_effort.as_deref(), Some("max"));
        assert!(agent_path("luna-worker").exists());
        assert!(!backup_path("luna-worker").exists());

        // 自定义模型与档位
        let updated = update_workflow_preset("luna-worker", "gpt-5.6-terra", "ultra").unwrap();
        assert_eq!(updated.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(updated.reasoning_effort.as_deref(), Some("ultra"));
        let list = list_workflow_agents().unwrap();
        let luna = list.agents.iter().find(|a| a.id == "luna-worker").unwrap();
        assert!(luna.customized);

        // 非法模型 / 档位拒绝
        assert!(update_workflow_preset("luna-worker", "bad model!", "max").is_err());
        assert!(update_workflow_preset("luna-worker", "gpt-5.6-terra", "insane").is_err());
        assert!(update_workflow_preset("nope", "gpt-5.6-terra", "max").is_err());

        // 恢复默认: 三个预设全部还原, 自定义标记清除
        let infos = reset_workflow_presets().unwrap();
        assert_eq!(infos.len(), 3);
        let list = list_workflow_agents().unwrap();
        let luna = list.agents.iter().find(|a| a.id == "luna-worker").unwrap();
        assert!(!luna.customized);
        assert_eq!(luna.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(luna.reasoning_effort.as_deref(), Some("max"));

        // 再次安装: 旧文件应被备份
        std::fs::write(agent_path("luna-worker"), "name = \"custom_edit\"\n").unwrap();
        let _ = install_workflow_preset("luna-worker").unwrap();
        assert!(backup_path("luna-worker").exists());
        let backed = std::fs::read_to_string(backup_path("luna-worker")).unwrap();
        assert!(backed.contains("custom_edit"));

        // 卸载: 文件移除, 备份仍在
        let outcome = uninstall_workflow_preset("luna-worker").unwrap();
        assert!(outcome.removed && outcome.backup_exists);
        assert!(!agent_path("luna-worker").exists());

        // 恢复: 从备份还原
        let restored = restore_workflow_preset("luna-worker").unwrap();
        assert!(restored.installed);
        assert!(agent_path("luna-worker").exists());

        // 已安装时再次恢复应报错
        assert!(restore_workflow_preset("luna-worker").is_err());

        // 未知预设拒绝
        assert!(install_workflow_preset("nope").is_err());
        assert!(uninstall_workflow_preset("nope").is_err());

        // 清理
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
