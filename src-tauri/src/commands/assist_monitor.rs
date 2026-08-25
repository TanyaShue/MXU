//! 阴阳师悬赏封印后台监控。
//!
//! 监控使用独立 Controller/Resource/Tasker，不进入主实例任务队列。

use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use maa_framework::common::MaaStatus;
use maa_framework::controller::Controller;
use maa_framework::resource::Resource;
use maa_framework::tasker::Tasker;

use super::maa_core::create_controller_from_config;
use super::types::{ControllerConfig, MaaState};
use super::utils::{emit_callback_event, emit_instance_log, normalize_path};

const PROJECT_NAME: &str = "MaaYYs";
const ENTRY: &str = "开始识别悬赏封印委托";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RESOURCE_TIMEOUT: Duration = Duration::from_secs(60);
const TASK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct InstanceSnapshot {
    controller_config: ControllerConfig,
    resource_paths: Vec<String>,
}

struct AssistRuntime {
    // Tasker 仅借用 controller/resource；运行期间必须保持它们存活。
    _controller: Controller,
    _resource: Resource,
    tasker: Tasker,
}

#[derive(Default)]
pub struct AssistMonitorState {
    started: AtomicBool,
    active: Mutex<HashSet<String>>,
    stop_requests: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

pub fn start(
    _app: tauri::AppHandle,
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

    log::info!("[assist-monitor] enabled for {}", PROJECT_NAME);
}

/// 在主实例任务开始时启动一次独立的悬赏识别任务。
///
/// `run_task_impl` 可能在同一批任务中被调用多次，因此通过 `active` 保证每个
/// 实例只会提交一次监控任务。
pub fn start_for_instance(app: tauri::AppHandle, maa_state: Arc<MaaState>, instance_id: String) {
    if !maa_state.assist_monitor.started.load(Ordering::SeqCst) {
        return;
    }

    let Some(snapshot) = snapshot_instance(&maa_state, &instance_id) else {
        log::debug!("[assist-monitor] instance {} is not ready", instance_id);
        return;
    };
    let stop_requested = Arc::new(AtomicBool::new(false));
    let should_start = maa_state
        .assist_monitor
        .active
        .lock()
        .map(|mut active| active.insert(instance_id.clone()))
        .unwrap_or(false);
    if !should_start {
        return;
    }

    if let Ok(mut requests) = maa_state.assist_monitor.stop_requests.lock() {
        requests.insert(instance_id.clone(), stop_requested.clone());
    }

    tauri::async_runtime::spawn(async move {
        let result = process_instance(
            &snapshot,
            stop_requested,
            app.clone(),
            maa_state.clone(),
            &instance_id,
        )
        .await;
        match result {
            Ok(()) => log::info!("[assist-monitor] instance {} stopped", instance_id),
            Err(error) => log::warn!(
                "[assist-monitor] instance {} failed: {}",
                instance_id,
                error
            ),
        }
        if let Err(error) = &result {
            emit_instance_log(
                &maa_state,
                &app,
                &instance_id,
                "error",
                format!("[悬赏封印监控]{}", error),
            );
        }
        clear_active(&maa_state, &instance_id);
        emit_instance_log(
            &maa_state,
            &app,
            &instance_id,
            "success",
            "[悬赏封印监控]任务已结束",
        );
    });
}

fn snapshot_instance(maa_state: &MaaState, instance_id: &str) -> Option<InstanceSnapshot> {
    let Ok(instances) = maa_state.instances.lock() else {
        return None;
    };
    let instance = instances.get(instance_id)?;
    let ready = instance
        .tasker
        .as_ref()
        .is_some_and(|tasker| tasker.inited());
    let controller_config = instance.controller_config.clone()?;
    if !ready || instance.resource_paths.is_empty() {
        return None;
    }
    Some(InstanceSnapshot {
        controller_config,
        resource_paths: instance.resource_paths.clone(),
    })
}

async fn process_instance(
    snapshot: &InstanceSnapshot,
    stop_requested: Arc<AtomicBool>,
    app: tauri::AppHandle,
    maa_state: Arc<MaaState>,
    instance_id: &str,
) -> Result<(), String> {
    let runtime = take_or_create_runtime(
        snapshot,
        &stop_requested,
        app.clone(),
        maa_state.clone(),
        instance_id.to_string(),
    )
    .await?;

    emit_instance_log(
        &maa_state,
        &app,
        instance_id,
        "info",
        format!("[悬赏封印监控]开始执行{}", ENTRY),
    );

    let result = async {
        if stop_requested.load(Ordering::SeqCst) {
            return Ok(());
        }
        let task_job = runtime
            .tasker
            .post_task(ENTRY, "{}")
            .map_err(|e| format!("任务提交失败：{}", e))?;
        let status = wait_task_job(
            &runtime.tasker,
            task_job.id,
            &stop_requested,
            TASK_TIMEOUT,
            "任务执行",
        )
        .await?;
        if status != MaaStatus::SUCCEEDED {
            return Err("任务执行失败".to_string());
        }
        Ok(())
    }
    .await;

    // 监控任务无论是自然结束、主任务结束还是出错，都不再复用。
    let _ = runtime.tasker.post_stop();
    result
}

async fn take_or_create_runtime(
    snapshot: &InstanceSnapshot,
    stop_requested: &AtomicBool,
    app: tauri::AppHandle,
    maa_state: Arc<MaaState>,
    instance_id: String,
) -> Result<AssistRuntime, String> {
    if stop_requested.load(Ordering::SeqCst) {
        return Err("任务已停止".to_string());
    }

    let controller = create_controller_from_config(&snapshot.controller_config)?;
    let app_for_controller = app.clone();
    let state_for_controller = maa_state.clone();
    let instance_for_controller = instance_id.clone();
    controller
        .add_sink(move |msg, detail| {
            emit_callback_event(&app_for_controller, msg, detail);
            emit_instance_log(
                &state_for_controller,
                &app_for_controller,
                &instance_for_controller,
                "info",
                format!("[悬赏封印监控][Controller] {}: {}", msg, detail),
            );
        })
        .map_err(|e| format!("独立控制器 sink 注册失败：{}", e))?;
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
        stop_requested,
        CONNECT_TIMEOUT,
        "独立控制器连接",
    )
    .await?;

    let resource = Resource::new().map_err(|e| format!("独立资源创建失败：{}", e))?;
    let app_for_resource = app.clone();
    let state_for_resource = maa_state.clone();
    let instance_for_resource = instance_id.clone();
    resource
        .add_sink(move |msg, detail| {
            emit_callback_event(&app_for_resource, msg, detail);
            emit_instance_log(
                &state_for_resource,
                &app_for_resource,
                &instance_for_resource,
                "info",
                format!("[悬赏封印监控][Resource] {}: {}", msg, detail),
            );
        })
        .map_err(|e| format!("独立资源 sink 注册失败：{}", e))?;
    for path in &snapshot.resource_paths {
        let normalized = normalize_path(path).to_string_lossy().to_string();
        let job = resource
            .post_bundle(&normalized)
            .map_err(|e| format!("独立资源加载提交失败：{}", e))?;
        wait_resource_job(&resource, job.id, stop_requested, RESOURCE_TIMEOUT).await?;
    }

    let tasker = Tasker::new().map_err(|e| format!("独立 Tasker 创建失败：{}", e))?;
    let app_for_tasker = app.clone();
    let state_for_tasker = maa_state.clone();
    let instance_for_tasker = instance_id.clone();
    tasker
        .add_sink(move |msg, detail| {
            emit_callback_event(&app_for_tasker, msg, detail);
            emit_instance_log(
                &state_for_tasker,
                &app_for_tasker,
                &instance_for_tasker,
                "info",
                format!("[悬赏封印监控][Tasker] {}: {}", msg, detail),
            );
        })
        .map_err(|e| format!("独立 Tasker sink 注册失败：{}", e))?;
    let app_for_context = app.clone();
    let state_for_context = maa_state.clone();
    let instance_for_context = instance_id.clone();
    tasker
        .add_context_sink(move |msg, detail| {
            emit_callback_event(&app_for_context, msg, detail);
            emit_instance_log(
                &state_for_context,
                &app_for_context,
                &instance_for_context,
                "info",
                format!("[悬赏封印监控][Context] {}: {}", msg, detail),
            );
        })
        .map_err(|e| format!("独立 Tasker context sink 注册失败：{}", e))?;
    tasker
        .bind(&resource, &controller)
        .map_err(|e| format!("独立 Tasker 绑定失败：{}", e))?;

    Ok(AssistRuntime {
        _controller: controller,
        _resource: resource,
        tasker,
    })
}

pub(crate) fn discard_runtime(maa_state: &MaaState, instance_id: &str) {
    request_stop(maa_state, instance_id);
}

pub(crate) fn request_stop(maa_state: &MaaState, instance_id: &str) {
    if let Ok(requests) = maa_state.assist_monitor.stop_requests.lock() {
        if let Some(request) = requests.get(instance_id) {
            request.store(true, Ordering::SeqCst);
        }
    }
}

fn clear_active(maa_state: &MaaState, instance_id: &str) {
    if let Ok(mut active) = maa_state.assist_monitor.active.lock() {
        active.remove(instance_id);
    }
    if let Ok(mut requests) = maa_state.assist_monitor.stop_requests.lock() {
        requests.remove(instance_id);
    }
}

async fn wait_controller_job(
    controller: &Controller,
    job_id: i64,
    stop_requested: &AtomicBool,
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
        if stop_requested.load(Ordering::SeqCst) {
            return Err(format!("{}已停止", operation));
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
    stop_requested: &AtomicBool,
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
        if stop_requested.load(Ordering::SeqCst) {
            return Err("独立资源加载已停止".to_string());
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
    stop_requested: &AtomicBool,
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
        if stop_requested.load(Ordering::SeqCst) {
            let _ = tasker.post_stop();
            return Err(format!("{}已停止", operation));
        }
        if started.elapsed() >= timeout {
            let _ = tasker.post_stop();
            return Err(format!("{}超时", operation));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
