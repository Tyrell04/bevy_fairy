//! `bevy_fairy/diagnostics`: how fast the game runs, and where the frame goes.
//!
//! Everything else this crate exposes describes *what* the game is doing. None of it could say
//! how *fast*: an agent asked to fix a slow game had no number to start from and none to check
//! its fix against. This method reads Bevy's own [`DiagnosticsStore`], the store
//! `FrameTimeDiagnosticsPlugin`, `EntityCountDiagnosticsPlugin` and
//! `SystemInformationDiagnosticsPlugin` write into, and reports every diagnostic in it as the
//! current value, the average, the smoothed value and the min/max over the kept history.
//!
//! It never invents a number. A diagnostic nobody registered is reported as missing, with the
//! plugin that would provide it, rather than as zero.
//!
//! # Where the frame goes
//!
//! [`DebuggerPlugin::with_diagnostics`](crate::DebuggerPlugin::with_diagnostics) also times every
//! schedule of the main loop (`First`, `PreUpdate`, `Update`, `PostUpdate`, ...). It inserts a
//! tiny stamp schedule between each pair in [`MainScheduleOrder`], so the cost is one clock read
//! per schedule and frame, and no system of the host is wrapped or reordered. The sum is reported
//! as `bevy_fairy/main_update` (ms). When the frame time is much larger than the main update, the
//! frame is spent outside the main world: in rendering, in the GPU, or waiting for the present.
//!
//! Per-*system* timings are not offered: Bevy 0.20 only produces them through its tracing
//! features (`trace_tracy`, `trace_chrome`), which have to be compiled into the game.

use bevy::app::MainScheduleOrder;
use bevy::diagnostic::{
    Diagnostic, DiagnosticPath, Diagnostics, DiagnosticsStore, EntityCountDiagnosticsPlugin,
    FrameTimeDiagnosticsPlugin, RegisterDiagnostic,
};
use bevy::ecs::entity::Entities;
use bevy::ecs::schedule::ScheduleLabel;
use bevy::platform::time::Instant;
use bevy::prelude::*;
use bevy::remote::{error_codes, BrpResult};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::plugin::brp_error;
use crate::plugin::ready::FrameTicks;

/// The sum of the main schedules of one frame, in milliseconds.
pub const MAIN_UPDATE: DiagnosticPath = DiagnosticPath::const_new("bevy_fairy/main_update");

/// The prefix of the per-schedule timings: `bevy_fairy/schedule/<Schedule>`, in milliseconds.
pub const SCHEDULE_PREFIX: &str = "bevy_fairy/schedule";

/// Params of `bevy_fairy/diagnostics`. All optional; an empty call reports everything.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsParams {
    /// Only list the diagnostics whose path starts with this, for example `bevy_fairy/schedule`.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Adds the frame-time and entity-count diagnostics, unless the host already did. With the
/// `plugin-sysinfo` feature, the CPU and memory diagnostics too.
pub(crate) fn add_diagnostic_plugins(app: &mut App) {
    if !app.is_plugin_added::<FrameTimeDiagnosticsPlugin>() {
        app.add_plugins(FrameTimeDiagnosticsPlugin::default());
    }
    if !app.is_plugin_added::<EntityCountDiagnosticsPlugin>() {
        app.add_plugins(EntityCountDiagnosticsPlugin::default());
    }
    #[cfg(feature = "plugin-sysinfo")]
    if !app.is_plugin_added::<bevy::diagnostic::SystemInformationDiagnosticsPlugin>() {
        app.add_plugins(bevy::diagnostic::SystemInformationDiagnosticsPlugin);
    }
}

/// One stamp between two main schedules. `ScheduleStamp(i)` runs just before the `i`-th main
/// schedule; the last one runs after `Last`.
#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
struct ScheduleStamp(usize);

/// The clock reads of the current frame, and the diagnostic each interval feeds.
#[derive(Resource)]
struct ScheduleClock {
    paths: Vec<DiagnosticPath>,
    marks: Vec<Option<Instant>>,
}

/// Interleaves a stamp schedule with every main schedule and registers one diagnostic per
/// schedule. Runs in `Plugin::finish`, after every plugin has inserted its own schedules.
pub(crate) fn time_main_schedules(app: &mut App) {
    let Some(labels) = app
        .world()
        .get_resource::<MainScheduleOrder>()
        .map(|order| order.labels.clone())
    else {
        return;
    };
    if labels.is_empty() {
        return;
    }
    let paths: Vec<DiagnosticPath> = labels
        .iter()
        .map(|label| DiagnosticPath::new(format!("{SCHEDULE_PREFIX}/{}", schedule_name(label))))
        .collect();
    for path in &paths {
        app.register_diagnostic(Diagnostic::new(path.clone()).with_suffix("ms"));
    }
    app.register_diagnostic(Diagnostic::new(MAIN_UPDATE).with_suffix("ms"));
    app.insert_resource(ScheduleClock {
        paths,
        marks: vec![None; labels.len() + 1],
    });

    let mut order = Vec::with_capacity(labels.len() * 2 + 1);
    for (index, label) in labels.iter().enumerate() {
        order.push(ScheduleStamp(index).intern());
        order.push(*label);
    }
    order.push(ScheduleStamp(labels.len()).intern());
    for index in 0..=labels.len() {
        app.add_systems(
            ScheduleStamp(index),
            move |clock: ResMut<ScheduleClock>, diagnostics: Diagnostics| {
                stamp(index, clock, diagnostics);
            },
        );
    }
    if let Some(mut main) = app.world_mut().get_resource_mut::<MainScheduleOrder>() {
        main.labels = order;
    }
}

/// A schedule label as a diagnostic path component: `Update`, `PostUpdate`, ...
fn schedule_name(label: &impl std::fmt::Debug) -> String {
    let name: String = format!("{label:?}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        "unnamed".to_string()
    } else {
        name
    }
}

/// Reads the clock. The last stamp of the frame turns the reads into measurements.
fn stamp(index: usize, mut clock: ResMut<ScheduleClock>, mut diagnostics: Diagnostics) {
    let now = Instant::now();
    if let Some(mark) = clock.marks.get_mut(index) {
        *mark = Some(now);
    }
    if index + 1 != clock.marks.len() {
        return;
    }
    for (pair, path) in clock.marks.windows(2).zip(&clock.paths) {
        if let [Some(start), Some(end)] = pair {
            let ms = end.saturating_duration_since(*start).as_secs_f64() * 1000.0;
            diagnostics.add_measurement(path, || ms);
        }
    }
    if let (Some(Some(first)), Some(Some(last))) = (clock.marks.first(), clock.marks.last()) {
        let ms = last.saturating_duration_since(*first).as_secs_f64() * 1000.0;
        diagnostics.add_measurement(&MAIN_UPDATE, || ms);
    }
    clock.marks.iter_mut().for_each(|mark| *mark = None);
}

/// One diagnostic as JSON: the latest value, the averages and the extremes of the kept history.
fn summary(diagnostic: &Diagnostic) -> Value {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for value in diagnostic.values() {
        min = min.min(*value);
        max = max.max(*value);
    }
    let samples = diagnostic.history_len();
    let extreme = |v: f64| if samples == 0 { Value::Null } else { json!(v) };
    json!({
        "path": diagnostic.path().as_str(),
        "suffix": diagnostic.suffix,
        "enabled": diagnostic.is_enabled,
        "current": diagnostic.value(),
        "average": diagnostic.average(),
        "smoothed": diagnostic.smoothed(),
        "min": extreme(min),
        "max": extreme(max),
        "samples": samples,
        "window": diagnostic.get_max_history_length(),
    })
}

/// BRP handler: `bevy_fairy/diagnostics`. Not ready-gated: a game that is still loading is
/// exactly the game whose frame time is worth reading.
pub(crate) fn handle_diagnostics(
    In(params): In<Option<Value>>,
    store: Option<Res<DiagnosticsStore>>,
    entities: &Entities,
    ticks: Res<FrameTicks>,
) -> BrpResult {
    let params: DiagnosticsParams = match params {
        None | Some(Value::Null) => DiagnosticsParams::default(),
        Some(value) => serde_json::from_value(value).map_err(|e| {
            brp_error(
                error_codes::INVALID_PARAMS,
                format!("bevy_fairy/diagnostics: {e}"),
            )
        })?,
    };
    let lookup = |path: &DiagnosticPath| store.as_ref().and_then(|s| s.get(path)).map(summary);
    let mut missing = Vec::new();
    let fps = lookup(&FrameTimeDiagnosticsPlugin::FPS);
    let frame_time = lookup(&FrameTimeDiagnosticsPlugin::FRAME_TIME);
    if fps.is_none() || frame_time.is_none() {
        missing.push(Value::from(
            "fps and frame_time_ms: add bevy::diagnostic::FrameTimeDiagnosticsPlugin, or build DebuggerPlugin with .with_diagnostics()",
        ));
    }
    let main_update = lookup(&MAIN_UPDATE);
    if main_update.is_none() {
        missing.push(Value::from(
            "main_update_ms and the bevy_fairy/schedule/* timings: build DebuggerPlugin with .with_diagnostics()",
        ));
    }
    let cpu_or_memory = store.as_ref().is_some_and(|s| {
        s.iter().any(|d| {
            d.path().as_str().starts_with("system/") || d.path().as_str().starts_with("process/")
        })
    });
    if !cpu_or_memory {
        missing.push(Value::from(
            "system/* and process/* (CPU, memory): add bevy::diagnostic::SystemInformationDiagnosticsPlugin (Bevy feature sysinfo_plugin), or build bevy_fairy with plugin-sysinfo and use .with_diagnostics()",
        ));
    }

    let mut listed: Vec<Value> = store
        .as_ref()
        .map(|s| {
            s.iter()
                .filter(|d| {
                    params
                        .prefix
                        .as_deref()
                        .is_none_or(|prefix| d.path().as_str().starts_with(prefix))
                })
                .map(summary)
                .collect()
        })
        .unwrap_or_default();
    listed.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));

    let mut reply = Map::new();
    reply.insert("frame".into(), json!(ticks.0));
    reply.insert("entities".into(), json!(entities.count_spawned()));
    reply.insert("fps".into(), fps.unwrap_or(Value::Null));
    reply.insert("frame_time_ms".into(), frame_time.unwrap_or(Value::Null));
    reply.insert("main_update_ms".into(), main_update.unwrap_or(Value::Null));
    reply.insert("diagnostics".into(), Value::Array(listed));
    reply.insert("missing".into(), Value::Array(missing));
    Ok(Value::Object(reply))
}
