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
use maa_framework::controller::Controller;
use maa_framework::resource::Resource;
use maa_framework::tasker::Tasker;

use super::maa_core::create_controller_from_config;
use super::types::{ControllerConfig, MaaState};
use super::utils::{emit_callback_event, normalize_path};

const PROJECT_NAME: &str = "MaaYYs";
const ENTRY: &str = "开始识别悬赏封印委托";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RESOURCE_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

struct MonitorControl {
    generation: u64,
    stop_requested: AtomicBool,
}

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

impl AssistRuntime {
    /// Maa 的停止请求是异步的。必须等 Tasker 完全停止后，才能释放其依赖对象。
    async fn stop_and_wait(self, instance_id: &str) {
        let AssistRuntime {
            _controller: controller,
            _resource: resource,
            tasker,
        } = self;

        let _ = tasker.post_stop();

        let started = Instant::now();
        while tasker.running() || tasker.stopping() {
            if started.elapsed() >= STOP_WAIT_TIMEOUT {
                log::error!(
                    "[assist-monitor] instance {} Tasker stop exceeded 2 minutes; keeping runtime alive",
                    instance_id
                );

                // Rust 无法安全强杀正在执行 Maa 原生调用的线程。泄漏这些句柄比
                // 立即析构并触发 MaaFramework 访问已释放内存更安全。
                std::mem::forget(tasker);
                std::mem::forget(resource);
                std::mem::forget(controller);
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        drop(tasker);
        drop(resource);
        drop(controller);
    }
}

#[derive(Default)]
pub struct AssistMonitorState {
    started: AtomicBool,
    generations: Mutex<HashMap<String, u64>>,
    controls: Mutex<HashMap<(String, u64), Arc<MonitorControl>>>,
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
/// 同一轮主任务重复提交时复用当前 generation；上一轮已收到停止请求后，
/// 下一轮会立即创建新的线程和 Maa 实例，不等待旧线程结束。
pub fn start_for_instance(app: tauri::AppHandle, maa_state: Arc<MaaState>, instance_id: String) {
    if !maa_state.assist_monitor.started.load(Ordering::SeqCst) {
        return;
    }

    let snapshot = match snapshot_instance(&maa_state, &instance_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            log::warn!(
                "[assist-monitor] instance {} skipped: {}",
                instance_id,
                error
            );
            return;
        }
    };
    let control = {
        let mut generations = match maa_state.assist_monitor.generations.lock() {
            Ok(value) => value,
            Err(_) => return,
        };
        let mut controls = match maa_state.assist_monitor.controls.lock() {
            Ok(value) => value,
            Err(_) => return,
        };
        if let Some(current) = generations.get(&instance_id).copied() {
            if let Some(existing) = controls.get(&(instance_id.clone(), current)) {
                if !existing.stop_requested.load(Ordering::SeqCst) {
                    return;
                }
                existing.stop_requested.store(true, Ordering::SeqCst);
            }
        }
        let generation = generations
            .get(&instance_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        generations.insert(instance_id.clone(), generation);
        let control = Arc::new(MonitorControl {
            generation,
            stop_requested: AtomicBool::new(false),
        });
        controls.insert((instance_id.clone(), generation), control.clone());
        control
    };

    std::thread::spawn(move || {
        let result = tauri::async_runtime::block_on(process_instance(
            &snapshot,
            control.clone(),
            app.clone(),
            &instance_id,
        ));
        if let Err(error) = result {
            // 主任务结束时的停止请求属于正常流程，不输出噪声日志。
            if !control.stop_requested.load(Ordering::SeqCst) {
                log::warn!(
                    "[assist-monitor] instance {} failed: {}",
                    instance_id,
                    error
                );
            }
        }
        clear_generation(&maa_state, &instance_id, control.generation);
    });
}

fn snapshot_instance(maa_state: &MaaState, instance_id: &str) -> Result<InstanceSnapshot, String> {
    let instances = maa_state
        .instances
        .lock()
        .map_err(|_| "读取实例状态失败，无法启动独立识别任务".to_string())?;
    let instance = instances
        .get(instance_id)
        .ok_or_else(|| "实例不存在，无法启动独立识别任务".to_string())?;
    let ready = instance
        .tasker
        .as_ref()
        .is_some_and(|tasker| tasker.inited());
    if !ready {
        return Err("主 Tasker 尚未初始化，跳过独立识别任务".to_string());
    }
    let controller_config = instance
        .controller_config
        .clone()
        .ok_or_else(|| "主实例没有 Controller 配置，跳过独立识别任务".to_string())?;
    if instance.resource_paths.is_empty() {
        return Err("主实例没有已加载资源，跳过独立识别任务".to_string());
    }
    Ok(InstanceSnapshot {
        controller_config,
        resource_paths: instance.resource_paths.clone(),
    })
}

async fn process_instance(
    snapshot: &InstanceSnapshot,
    control: Arc<MonitorControl>,
    app: tauri::AppHandle,
    instance_id: &str,
) -> Result<(), String> {
    let runtime = take_or_create_runtime(
        snapshot,
        &control.stop_requested,
        app.clone(),
        instance_id.to_string(),
    )
    .await?;

    let result = async {
        if control.stop_requested.load(Ordering::SeqCst) {
            return Ok(());
        }
        let task_job = runtime
            .tasker
            .post_task(ENTRY, "{}")
            .map_err(|e| format!("任务提交失败：{}", e))?;
        let status = wait_task_job(
            &runtime.tasker,
            task_job.id,
            &control.stop_requested,
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
    // post_stop 是异步请求，必须等待 Tasker 完全停止后才可析构运行时。
    runtime.stop_and_wait(instance_id).await;
    result
}

async fn take_or_create_runtime(
    snapshot: &InstanceSnapshot,
    stop_requested: &AtomicBool,
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<AssistRuntime, String> {
    if stop_requested.load(Ordering::SeqCst) {
        return Err("任务已停止".to_string());
    }

    let controller = create_controller_from_config(&snapshot.controller_config)?;
    controller
        .add_sink(move |_msg, _detail| {})
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
        ControllerConfig::Linux {
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
    resource
        .add_sink(move |_msg, _detail| {})
        .map_err(|e| format!("独立资源 sink 注册失败：{}", e))?;
    for path in &snapshot.resource_paths {
        let normalized = normalize_path(path).to_string_lossy().to_string();
        let job = resource
            .post_bundle(&normalized)
            .map_err(|e| format!("独立资源加载提交失败：{}", e))?;
        wait_resource_job(&resource, job.id, stop_requested, RESOURCE_TIMEOUT).await?;
    }

    let tasker = Tasker::new().map_err(|e| format!("独立 Tasker 创建失败：{}", e))?;
    tasker
        .add_sink(move |_msg, _detail| {})
        .map_err(|e| format!("独立 Tasker sink 注册失败：{}", e))?;
    let app_for_context = app.clone();
    let instance_for_context = instance_id.clone();
    tasker
        .add_context_sink(move |msg, detail| {
            super::telemetry::on_node_event(&instance_for_context, msg, detail);
            emit_callback_event(&app_for_context, msg, detail);
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
    if let Ok(controls) = maa_state.assist_monitor.controls.lock() {
        for ((id, _), control) in controls.iter() {
            if id == instance_id {
                control.stop_requested.store(true, Ordering::SeqCst);
            }
        }
    }
}

fn clear_generation(maa_state: &MaaState, instance_id: &str, generation: u64) {
    if let Ok(mut controls) = maa_state.assist_monitor.controls.lock() {
        controls.remove(&(instance_id.to_string(), generation));
    }
    if let Ok(generations) = maa_state.assist_monitor.generations.lock() {
        if generations.get(instance_id).copied() == Some(generation) {
            drop(generations);
            if let Ok(mut generations) = maa_state.assist_monitor.generations.lock() {
                if generations.get(instance_id).copied() == Some(generation) {
                    generations.remove(instance_id);
                }
            }
        }
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
    operation: &str,
) -> Result<MaaStatus, String> {
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
            return Err(format!("{}已停止", operation));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
