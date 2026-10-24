//! `bevy_fairy/diagnostics`: frame time, fps, the entity count and the per-schedule timings.
//!
//! The handler is run the way the BRP server runs it: looked up in `RemoteMethods` and run with
//! the request's params. The app is driven frame by frame with `App::update`, so the numbers are
//! real measurements of these frames, not fixtures. No socket, no GPU.

use std::time::Duration;

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::prelude::*;
use bevy::remote::{error_codes, BrpResult, RemoteMethodSystemId, RemoteMethods};
use bevy_fairy::{DebuggerPlugin, MAIN_UPDATE, SCHEDULE_PREFIX};
use serde_json::{json, Value};

/// A host app: `MinimalPlugins`, `InputPlugin` (required by the plugin), then `plugin`. Built and
/// finished the way `App::run` does it, then run for `frames` frames.
fn run_app(plugin: DebuggerPlugin, frames: usize) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, bevy::input::InputPlugin, plugin));
    app.init_resource::<UpdateRuns>()
        .add_systems(Update, count_update);
    app.finish();
    app.cleanup();
    for _ in 0..frames {
        app.update();
        // Real time has to pass, or the frame-time diagnostic skips a zero-length frame.
        std::thread::sleep(Duration::from_millis(2));
    }
    app
}

/// How often the host's own `Update` system ran.
#[derive(Resource, Default)]
struct UpdateRuns(usize);

fn count_update(mut runs: ResMut<UpdateRuns>) {
    runs.0 += 1;
}

fn call(app: &mut App, params: Option<Value>) -> BrpResult {
    let id = match app
        .world()
        .resource::<RemoteMethods>()
        .get("bevy_fairy/diagnostics")
    {
        Some(RemoteMethodSystemId::Instant(id)) => *id,
        _ => panic!("bevy_fairy/diagnostics is not registered as an instant method"),
    };
    app.world_mut()
        .run_system_with(id, params)
        .expect("the diagnostics handler runs")
}

fn paths(reply: &Value) -> Vec<String> {
    reply["diagnostics"]
        .as_array()
        .expect("diagnostics is a list")
        .iter()
        .map(|d| {
            d["path"]
                .as_str()
                .expect("a diagnostic has a path")
                .to_string()
        })
        .collect()
}

#[test]
fn with_diagnostics_reports_measured_fps_and_frame_time() {
    let mut app = run_app(DebuggerPlugin::default().with_diagnostics(), 6);
    let reply = call(&mut app, None).expect("diagnostics answers");

    for key in ["fps", "frame_time_ms"] {
        let summary = &reply[key];
        let samples = summary["samples"].as_u64().expect("samples is a count");
        assert!(
            samples >= 4,
            "{key}: expected a sample per frame after the first, got {summary}"
        );
        let current = summary["current"].as_f64().expect("a current value");
        let min = summary["min"].as_f64().expect("a min");
        let max = summary["max"].as_f64().expect("a max");
        assert!(current > 0.0 && min > 0.0, "{key}: {summary}");
        assert!(min <= current && current <= max, "{key}: {summary}");
        assert_eq!(
            summary["window"],
            json!(120),
            "the default history of FrameTimeDiagnosticsPlugin"
        );
    }
    // Each frame slept 2 ms, so no frame can have been faster than that.
    let slowest_fps = reply["fps"]["max"].as_f64().expect("fps max");
    assert!(
        slowest_fps < 1000.0,
        "fps max {slowest_fps} is faster than the 2 ms sleep allows"
    );
    assert_eq!(reply["frame_time_ms"]["suffix"], "ms");
}

#[test]
fn the_entity_count_is_the_worlds() {
    let mut app = run_app(DebuggerPlugin::default().with_diagnostics(), 2);
    app.world_mut().spawn_empty();
    app.world_mut().spawn_empty();
    let expected = app.world().entities().count_spawned();
    let reply = call(&mut app, None).expect("diagnostics answers");
    assert_eq!(reply["entities"], json!(expected));
    assert!(
        paths(&reply).contains(&"entity_count".to_string()),
        "EntityCountDiagnosticsPlugin keeps a history: {reply}"
    );
}

#[test]
fn every_main_schedule_is_timed_and_the_host_schedules_still_run_once_per_frame() {
    let mut app = run_app(DebuggerPlugin::default().with_diagnostics(), 5);
    assert_eq!(
        app.world().resource::<UpdateRuns>().0,
        5,
        "Update ran once per frame"
    );

    let reply = call(&mut app, None).expect("diagnostics answers");
    let listed = paths(&reply);
    for schedule in ["First", "PreUpdate", "Update", "PostUpdate", "Last"] {
        let path = format!("{SCHEDULE_PREFIX}/{schedule}");
        assert!(
            listed.contains(&path),
            "{path} is not timed; listed: {listed:?}"
        );
    }
    let main = &reply["main_update_ms"];
    assert_eq!(main["path"], MAIN_UPDATE.as_str());
    assert_eq!(
        main["samples"],
        json!(5),
        "one main-update sample per frame: {main}"
    );
    // The main update is part of the frame, so it cannot be longer than the longest frame.
    let main_max = main["max"].as_f64().expect("main update max");
    let frame_max = reply["frame_time_ms"]["max"]
        .as_f64()
        .expect("frame time max");
    assert!(
        main_max <= frame_max,
        "main update {main_max} ms > frame {frame_max} ms"
    );
}

#[test]
fn the_schedule_timings_add_up_to_the_main_update() {
    let app = run_app(DebuggerPlugin::default().with_diagnostics(), 3);
    let store = app.world().resource::<DiagnosticsStore>();
    let schedules: f64 = store
        .iter()
        .filter(|d| d.path().as_str().starts_with(SCHEDULE_PREFIX))
        .filter_map(|d| d.value())
        .sum();
    let main = store
        .get(&MAIN_UPDATE)
        .and_then(|d| d.value())
        .expect("a main update value");
    assert!(
        (schedules - main).abs() < 1e-6,
        "the schedules sum to {schedules} ms, the main update is {main} ms"
    );
}

#[test]
fn without_with_diagnostics_nothing_is_invented_and_the_fix_is_named() {
    let mut app = run_app(DebuggerPlugin::default(), 3);
    let reply = call(&mut app, None).expect("diagnostics answers without the plugins");
    assert_eq!(reply["fps"], Value::Null);
    assert_eq!(reply["frame_time_ms"], Value::Null);
    assert_eq!(reply["main_update_ms"], Value::Null);
    let missing = reply["missing"].to_string();
    assert!(missing.contains("FrameTimeDiagnosticsPlugin"), "{missing}");
    assert!(missing.contains("with_diagnostics"), "{missing}");
    // The entity count needs no plugin: it is read from the world.
    assert!(reply["entities"].as_u64().is_some(), "{reply}");
}

#[test]
fn a_host_that_added_the_frame_time_plugin_first_keeps_it() {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        bevy::input::InputPlugin,
        FrameTimeDiagnosticsPlugin::new(7),
        DebuggerPlugin::default().with_diagnostics(),
    ));
    app.finish();
    app.cleanup();
    app.update();
    let reply = call(&mut app, None).expect("diagnostics answers");
    assert_eq!(
        reply["fps"]["window"],
        json!(7),
        "the host's own history length is kept"
    );
}

#[test]
fn a_prefix_narrows_the_list() {
    let mut app = run_app(DebuggerPlugin::default().with_diagnostics(), 2);
    let reply =
        call(&mut app, Some(json!({ "prefix": SCHEDULE_PREFIX }))).expect("diagnostics answers");
    let listed = paths(&reply);
    assert!(!listed.is_empty());
    assert!(
        listed.iter().all(|path| path.starts_with(SCHEDULE_PREFIX)),
        "{listed:?}"
    );
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "the list is sorted by path");
}

#[test]
fn an_unknown_param_is_refused() {
    let mut app = run_app(DebuggerPlugin::default(), 1);
    let err = call(&mut app, Some(json!({ "window": 3 }))).expect_err("unknown params fail");
    assert_eq!(err.code, error_codes::INVALID_PARAMS);
    assert!(
        err.message.contains("bevy_fairy/diagnostics"),
        "{}",
        err.message
    );
}

/// With `plugin-sysinfo`, `with_diagnostics()` adds Bevy's `SystemInformationDiagnosticsPlugin`:
/// the CPU and memory paths are registered and nothing is reported missing for them.
#[cfg(feature = "plugin-sysinfo")]
#[test]
fn plugin_sysinfo_registers_cpu_and_memory() {
    let mut app = run_app(DebuggerPlugin::default().with_diagnostics(), 2);
    let reply = call(&mut app, None).expect("diagnostics answers");
    let listed = paths(&reply);
    for path in [
        "process/cpu_usage",
        "process/mem_usage",
        "system/cpu_usage",
        "system/mem_usage",
    ] {
        assert!(
            listed.contains(&path.to_string()),
            "{path} missing: {listed:?}"
        );
    }
    let missing = reply["missing"].to_string();
    assert!(!missing.contains("process/*"), "{missing}");
}

/// Without `plugin-sysinfo`, CPU and memory are named as missing, with the fix.
#[cfg(not(feature = "plugin-sysinfo"))]
#[test]
fn without_plugin_sysinfo_cpu_and_memory_are_named_missing() {
    let mut app = run_app(DebuggerPlugin::default().with_diagnostics(), 1);
    let reply = call(&mut app, None).expect("diagnostics answers");
    let missing = reply["missing"].to_string();
    assert!(
        missing.contains("SystemInformationDiagnosticsPlugin"),
        "{missing}"
    );
}
