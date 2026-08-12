//! 阴阳师悬赏封印后台监控。
//!
//! 监控使用独立 Controller/Resource/Tasker，不进入主实例任务队列。

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use maa_framework::common::MaaStatus;
use maa_framework::common::{RecognitionDetail, TaskDetail};
use maa_framework::controller::Controller;
use maa_framework::resource::Resource;
use maa_framework::tasker::Tasker;

use super::maa_core::create_controller_from_config;
use super::types::{ControllerConfig, MaaState};
use super::utils::{emit_instance_log, normalize_path};

const PROJECT_NAME: &str = "MaaYYs";
const ENTRY: &str = "悬赏封印_协助战斗1";
const INTERVAL: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RESOURCE_TIMEOUT: Duration = Duration::from_secs(60);
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(15);
const RECOGNITION_TIMEOUT: Duration = Duration::from_secs(30);
const TASK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct InstanceSnapshot {
    id: String,
    controller_config: ControllerConfig,
    resource_paths: Vec<String>,
}

struct AssistRuntime {
    controller_config: ControllerConfig,
    resource_paths: Vec<String>,
    controller: Controller,
    resource: Resource,
    tasker: Tasker,
}

#[derive(Default)]
pub struct AssistMonitorState {
    started: AtomicBool,
    runtimes: Mutex<HashMap<String, AssistRuntime>>,
}

pub fn start(
    app: tauri::AppHandle,
    maa_state: Arc<MaaState>,
    app_config: Arc<super::app_config::AppConfigState>,
) {
    let enabled = app_config
        .project_name
        .lock()
        .ok()
        .and_then(|name| name.clone())
        .is_some_and(|name| name == PROJECT_NAME);
    if !enabled {
        log::debug!("[assist-monitor] disabled for current interface");
        return;
    }
    if maa_state
        .assist_monitor
        .started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    tauri::async_runtime::spawn(async move {
        log::info!("[assist-monitor] started for {}", PROJECT_NAME);
        loop {
            run_cycle(&app, &maa_state).await;
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

async fn run_cycle(app: &tauri::AppHandle, maa_state: &Arc<MaaState>) {
    let snapshots = snapshot_running_instances(maa_state);
    if snapshots.is_empty() {
        return;
    }

    for snapshot in snapshots {
        let instance_id = snapshot.id.clone();
        let result = process_instance(maa_state, &snapshot).await;
        match result {
            Ok(ProcessResult::NotFound) => {}
            Ok(ProcessResult::Executed) => emit_instance_log(
                maa_state,
                app,
                &instance_id,
                "success",
                "[悬赏封印监控]\"拒绝悬赏封印\"",
            ),
            Err(error) => {
                discard_runtime(maa_state, &instance_id);
                log::warn!("[assist-monitor] instance {} failed: {}", instance_id, error);
            }
        }
    }
}

fn snapshot_running_instances(maa_state: &MaaState) -> Vec<InstanceSnapshot> {
    let Ok(instances) = maa_state.instances.lock() else {
        return Vec::new();
    };
    instances
        .iter()
        .filter_map(|(id, instance)| {
            let running = instance
                .tasker
                .as_ref()
                .is_some_and(|tasker| tasker.running());
            let controller_config = instance.controller_config.clone()?;
            if !running || instance.resource_paths.is_empty() {
                return None;
            }
            Some(InstanceSnapshot {
                id: id.clone(),
                controller_config,
                resource_paths: instance.resource_paths.clone(),
            })
        })
        .collect()
}

enum ProcessResult {
    NotFound,
    Executed,
}

async fn process_instance(
    maa_state: &MaaState,
    snapshot: &InstanceSnapshot,
) -> Result<ProcessResult, String> {
    let runtime = take_or_create_runtime(maa_state, snapshot).await?;

    let result = async {
        let screenshot_id = runtime
            .controller
            .post_screencap()
            .map_err(|e| format!("截图提交失败：{}", e))?;
        wait_controller_job(
            &runtime.controller,
            screenshot_id,
            SCREENSHOT_TIMEOUT,
            "截图",
        )
        .await?;

        let image = runtime
            .controller
            .cached_image()
            .map_err(|e| format!("读取截图缓存失败：{}", e))?;
        let node = runtime
            .resource
            .get_node_object(ENTRY)
            .map_err(|e| format!("读取识别节点失败：{}", e))?
            .ok_or_else(|| format!("资源中不存在识别节点 {}", ENTRY))?;
        let recognition = serde_json::to_value(&node.recognition)
            .map_err(|e| format!("序列化识别配置失败：{}", e))?;
        let recognition_type = recognition
            .get("type")
            .and_then(|value| value.as_str())
            .ok_or("识别节点缺少 type")?;
        let recognition_param = recognition
            .get("param")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let recognition_job = runtime
            .tasker
            .post_recognition(recognition_type, &recognition_param.to_string(), &image)
            .map_err(|e| format!("识别提交失败：{}", e))?;
        let started = Instant::now();
        let recognition_status = loop {
            let status = recognition_job.status();
            if status == MaaStatus::SUCCEEDED || status == MaaStatus::FAILED {
                break status;
            }
            if started.elapsed() >= RECOGNITION_TIMEOUT {
                return Err("识别超时".to_string());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        if recognition_status != MaaStatus::SUCCEEDED {
            return Err("识别任务失败".to_string());
        }
        // MaaTaskerPostRecognition 返回 MaaTaskId，而识别详情 API 需要内部 MaaRecoId。
        // Rust crate 的 recognition_job.get() 直接混用了这两个 ID，会错误返回 None；
        // 应先读取 TaskDetail，再从其 NodeDetail 获取已水合的 RecognitionDetail。
        let task_detail = runtime
            .tasker
            .get_task_detail(recognition_job.id)
            .map_err(|e| format!("读取识别任务详情失败：{}", e))?
            .ok_or("识别任务详情不存在")?;
        let detail = extract_recognition_detail(task_detail)?;
        let hit = detail.hit ^ node.inverse;
        if !hit {
            return Ok(ProcessResult::NotFound);
        }

        let task_job = runtime
            .tasker
            .post_task(ENTRY, "{}")
            .map_err(|e| format!("任务提交失败：{}", e))?;
        let task_status =
            wait_task_job(&runtime.tasker, task_job.id, TASK_TIMEOUT, "任务执行").await?;
        if task_status != MaaStatus::SUCCEEDED {
            return Err("任务执行失败".to_string());
        }
        Ok(ProcessResult::Executed)
    }
    .await;

    if result.is_ok() && snapshot_is_current(maa_state, snapshot) {
        maa_state
            .assist_monitor
            .runtimes
            .lock()
            .map_err(|e| e.to_string())?
            .insert(snapshot.id.clone(), runtime);
    } else {
        let _ = runtime.tasker.post_stop();
    }
    result
}

fn extract_recognition_detail(task_detail: TaskDetail) -> Result<RecognitionDetail, String> {
    task_detail
        .nodes
        .into_iter()
        .flatten()
        .find_map(|node| node.recognition)
        .ok_or_else(|| "识别节点详情不存在".to_string())
}

fn snapshot_is_current(maa_state: &MaaState, snapshot: &InstanceSnapshot) -> bool {
    maa_state
        .instances
        .lock()
        .ok()
        .and_then(|instances| {
            instances.get(&snapshot.id).map(|instance| {
                instance.tasker.as_ref().is_some_and(|tasker| tasker.running())
                    && instance.controller_config.as_ref() == Some(&snapshot.controller_config)
                    && instance.resource_paths == snapshot.resource_paths
            })
        })
        .unwrap_or(false)
}

async fn take_or_create_runtime(
    maa_state: &MaaState,
    snapshot: &InstanceSnapshot,
) -> Result<AssistRuntime, String> {
    if let Some(runtime) = maa_state
        .assist_monitor
        .runtimes
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&snapshot.id)
    {
        if runtime.controller_config == snapshot.controller_config
            && runtime.resource_paths == snapshot.resource_paths
            && runtime.controller.connected()
            && runtime.resource.loaded()
            && runtime.tasker.inited()
        {
            return Ok(runtime);
        }
    }

    let controller = create_controller_from_config(&snapshot.controller_config)?;
    let display_short_side = match &snapshot.controller_config {
        ControllerConfig::Adb {
            display_short_side, ..
        }
        | ControllerConfig::Win32 {
            display_short_side, ..
        }
        | ControllerConfig::MacOS {
            display_short_side, ..
        }
        | ControllerConfig::WlRoots {
            display_short_side, ..
        }
        | ControllerConfig::Gamepad {
            display_short_side, ..
        }
        | ControllerConfig::PlayCover {
            display_short_side, ..
        }
        | ControllerConfig::Dummy {
            display_short_side, ..
        } => display_short_side.unwrap_or(720),
    };
    controller
        .set_screenshot_target_short_side(display_short_side)
        .map_err(|e| format!("独立控制器截图尺寸设置失败：{}", e))?;
    let connection_id = controller
        .post_connection()
        .map_err(|e| format!("独立控制器连接提交失败：{}", e))?;
    wait_controller_job(
        &controller,
        connection_id,
        CONNECT_TIMEOUT,
        "独立控制器连接",
    )
    .await?;

    let resource = Resource::new().map_err(|e| format!("独立资源创建失败：{}", e))?;
    for path in &snapshot.resource_paths {
        let normalized = normalize_path(path).to_string_lossy().to_string();
        let job = resource
            .post_bundle(&normalized)
            .map_err(|e| format!("独立资源加载提交失败：{}", e))?;
        wait_resource_job(&resource, job.id, RESOURCE_TIMEOUT).await?;
    }

    let tasker = Tasker::new().map_err(|e| format!("独立 Tasker 创建失败：{}", e))?;
    tasker
        .bind(&resource, &controller)
        .map_err(|e| format!("独立 Tasker 绑定失败：{}", e))?;

    Ok(AssistRuntime {
        controller_config: snapshot.controller_config.clone(),
        resource_paths: snapshot.resource_paths.clone(),
        controller,
        resource,
        tasker,
    })
}

pub(crate) fn discard_runtime(maa_state: &MaaState, instance_id: &str) {
    if let Ok(mut runtimes) = maa_state.assist_monitor.runtimes.lock() {
        runtimes.remove(instance_id);
    }
}

async fn wait_controller_job(
    controller: &Controller,
    job_id: i64,
    timeout: Duration,
    operation: &str,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let status = controller.status(job_id);
        if status == MaaStatus::SUCCEEDED {
            return Ok(());
        }
        if status == MaaStatus::FAILED || status == MaaStatus::INVALID {
            return Err(format!("{}失败", operation));
        }
        if started.elapsed() >= timeout {
            return Err(format!("{}超时", operation));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_resource_job(
    resource: &Resource,
    job_id: i64,
    timeout: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let status = resource.status(job_id);
        if status == MaaStatus::SUCCEEDED {
            return Ok(());
        }
        if status == MaaStatus::FAILED || status == MaaStatus::INVALID {
            return Err("独立资源加载失败".to_string());
        }
        if started.elapsed() >= timeout {
            return Err("独立资源加载超时".to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_task_job(
    tasker: &Tasker,
    job_id: i64,
    timeout: Duration,
    operation: &str,
) -> Result<MaaStatus, String> {
    let started = Instant::now();
    loop {
        let status = tasker
            .get_task_detail(job_id)
            .map_err(|e| format!("{}状态读取失败：{}", operation, e))?
            .map(|detail| detail.status)
            .unwrap_or(MaaStatus::PENDING);
        if status == MaaStatus::SUCCEEDED || status == MaaStatus::FAILED {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = tasker.post_stop();
            return Err(format!("{}超时", operation));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
