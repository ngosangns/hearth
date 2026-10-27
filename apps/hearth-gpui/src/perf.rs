use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Context, ScrollStrategy, UniformListScrollHandle};
use serde_json::{Value, json};

use crate::{HearthWindow, Pane, logbuf, logview};

const LOG_LINES: usize = 50_000;
const SCENARIOS: &[&str] = &[
    "idle-overview",
    "workspace-open",
    "service-select",
    "log-scroll",
    "fold-toggle",
    "shared-settings",
];

#[derive(Default)]
struct Probe {
    running: bool,
    driving: bool,
    recording: bool,
    renders: u64,
    render_us: Vec<u64>,
}

thread_local! {
    static PROBE: RefCell<Probe> = RefCell::new(Probe::default());
}

pub(crate) struct Clock(Instant);

impl Drop for Clock {
    fn drop(&mut self) {
        let micros = u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX);
        PROBE.with(|probe| {
            let mut probe = probe.borrow_mut();
            probe.renders += 1;
            if probe.recording {
                probe.render_us.push(micros);
            }
        });
    }
}

pub(crate) fn clock() -> Clock {
    Clock(Instant::now())
}

pub(crate) fn driving() -> bool {
    PROBE.with(|probe| probe.borrow().driving)
}

struct Run;

impl Drop for Run {
    fn drop(&mut self) {
        PROBE.with(|probe| {
            let mut probe = probe.borrow_mut();
            probe.running = false;
            probe.driving = false;
            probe.recording = false;
            probe.render_us.clear();
        });
    }
}

fn renders() -> u64 {
    PROBE.with(|probe| probe.borrow().renders)
}

fn record(on: bool) -> Vec<u64> {
    PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        probe.recording = on;
        std::mem::take(&mut probe.render_us)
    })
}

fn percentile(values: &mut [f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() as f64 * fraction) as usize).min(values.len() - 1)]
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

pub(crate) fn args(args: &Value) -> Result<(String, usize), String> {
    let label = match args.get("label") {
        None => "run",
        Some(value) => value.as_str().ok_or("label must be a string")?,
    };
    if label.is_empty()
        || label.len() > 80
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("label must contain 1–80 letters, digits, - or _".into());
    }
    let frames = match args.get("frames") {
        None => 60,
        Some(value) => value
            .as_u64()
            .ok_or("frames must be a nonnegative integer")?,
    };
    Ok((label.to_string(), frames.clamp(1, 1000) as usize))
}

fn ready(window: &HearthWindow) -> Result<(), String> {
    if window.busy
        || window.board_pending
        || !window.inflight.is_empty()
        || window.log_loading
        || window.log_refreshing
        || window.dialog_open()
    {
        return Err("wait for loading, actions and dialogs to finish before perf".into());
    }
    if window.current().is_none() || window.services.is_empty() {
        return Err("load a workspace with services before perf".into());
    }
    Ok(())
}

struct Saved {
    pane: Pane,
    selected: Option<usize>,
    selected_service: Option<usize>,
    shared_pick: Option<usize>,
    recipe_pick: Option<usize>,
    settings_open: bool,
    palette_open: bool,
    collapsed: HashSet<String>,
    log_lines: Rc<Vec<String>>,
    log_paints: Rc<Vec<Vec<logbuf::LogPaint>>>,
    log_rows: Rc<logview::Rows>,
    log_levels: Rc<Vec<logview::LineLevel>>,
    log_cols: usize,
    log_scroll: UniformListScrollHandle,
    log_follow: bool,
    log_sel: Option<logbuf::LogSelection>,
    log_selecting: bool,
    log_service: String,
    log_shared: bool,
}

impl Saved {
    fn take(window: &mut HearthWindow) -> Self {
        Self {
            pane: window.pane,
            selected: window.selected,
            selected_service: window.selected_service,
            shared_pick: window.shared_pick,
            recipe_pick: window.recipe_pick,
            settings_open: window.settings_open,
            palette_open: window.palette_open,
            collapsed: std::mem::take(&mut window.collapsed),
            log_lines: std::mem::replace(&mut window.log_lines, Rc::new(Vec::new())),
            log_paints: std::mem::replace(&mut window.log_paints, Rc::new(Vec::new())),
            log_rows: window.log_rows.clone(),
            log_levels: window.log_levels.clone(),
            log_cols: window.log_cols,
            log_scroll: std::mem::replace(&mut window.log_scroll, UniformListScrollHandle::new()),
            log_follow: window.log_follow,
            log_sel: window.log_sel.take(),
            log_selecting: window.log_selecting,
            log_service: std::mem::take(&mut window.log_service),
            log_shared: window.log_shared,
        }
    }

    fn restore(self, window: &mut HearthWindow) {
        window.pane = self.pane;
        window.selected = self.selected;
        window.selected_service = self.selected_service;
        window.shared_pick = self.shared_pick;
        window.recipe_pick = self.recipe_pick;
        window.settings_open = self.settings_open;
        window.palette_open = self.palette_open;
        window.collapsed = self.collapsed;
        window.log_lines = self.log_lines;
        window.log_paints = self.log_paints;
        window.log_rows = self.log_rows;
        window.log_levels = self.log_levels;
        window.log_cols = self.log_cols;
        window.log_scroll = self.log_scroll;
        window.log_follow = self.log_follow;
        window.log_sel = self.log_sel;
        window.log_selecting = self.log_selecting;
        window.log_service = self.log_service;
        window.log_shared = self.log_shared;
    }
}

fn prepare(window: &mut HearthWindow) -> (Saved, Value, Vec<String>) {
    let saved = Saved::take(window);
    let synthetic = saved.log_lines.len() < LOG_LINES;
    if synthetic {
        window.log_lines = Rc::new(
            (0..LOG_LINES)
                .map(|line| format!("2026-10-25T12:00:00Z INFO service request={line:06} completed status=200 elapsed=12ms worker=baseline"))
                .collect(),
        );
        window.log_paints = Rc::new(vec![Vec::new(); LOG_LINES]);
    } else {
        window.log_lines = saved.log_lines.clone();
        window.log_paints = saved.log_paints.clone();
    }
    window.rebuild_log_rows();
    window.log_selecting = false;
    window.log_follow = false;
    window.palette_open = false;
    window.settings_open = false;
    let ids: Vec<String> = window
        .services
        .iter()
        .map(|service| service.id.clone())
        .collect();
    let groups = if window.hide_disabled {
        crate::visible_groups(&window.catalog.groups, &window.catalog.disabled)
    } else {
        window.catalog.groups.clone()
    };
    let sections = crate::service_groups_hiding(
        &ids,
        &groups,
        if window.hide_disabled {
            &window.catalog.disabled
        } else {
            &[]
        },
    );
    let titles = sections.into_iter().map(|section| section.title).collect();
    let data = json!({
        "workspaces": window.workspaces.len(),
        "services": window.services.len(),
        "urls": window.urls.len(),
        "instances": window.instances.len(),
        "recipes": window.recipes.len(),
        "loaded_log_lines": saved.log_lines.len(),
        "log_lines": window.log_lines.len(),
        "log_bytes": window.log_lines.iter().map(String::len).sum::<usize>(),
        "log_source": if synthetic { "synthetic-50k" } else { "loaded" },
        "workspace_board": "loaded board reused across workspace selections; no reloads",
        "service_logs": "one in-memory buffer reused across service selections",
        "viewport_width": ns_ui::viewport_width(),
    });
    (saved, data, titles)
}

// Selection handlers reload the CLI; perf must change only the presentation state.
fn step(
    window: &mut HearthWindow,
    name: &str,
    frame: usize,
    home: Option<usize>,
    groups: &[String],
) {
    if frame == 0 {
        window.pane = Pane::Workspaces;
        window.selected = home;
        window.selected_service = None;
        window.shared_pick = None;
        window.recipe_pick = None;
        window.settings_open = false;
        window.collapsed.clear();
        window.log_shared = false;
    }
    match name {
        "idle-overview" => {}
        "workspace-open" => window.selected = Some(frame % window.workspaces.len()),
        "service-select" | "log-scroll" => {
            let index = if name == "service-select" {
                frame % window.services.len()
            } else {
                0
            };
            window.selected_service = Some(index);
            window.log_service = window.services[index].id.clone();
            let row = if name == "log-scroll" {
                frame * 200 % window.log_rows.len().max(1)
            } else {
                window.log_rows.len().saturating_sub(1)
            };
            window.log_follow = name == "service-select";
            window
                .log_scroll
                .scroll_to_item_strict(row, ScrollStrategy::Top);
        }
        "fold-toggle" => {
            if let Some(group) = groups.get((frame / 2) % groups.len().max(1))
                && !window.collapsed.remove(group)
            {
                window.collapsed.insert(group.clone());
            }
        }
        "shared-settings" => {
            window.pane = Pane::Shared;
            window.shared_pick =
                (!window.instances.is_empty()).then(|| frame % window.instances.len());
            window.settings_open = frame.is_multiple_of(2);
        }
        _ => unreachable!("unknown perf scenario"),
    }
}

pub(crate) fn start(
    window: &mut HearthWindow,
    label: &str,
    frames: usize,
    cx: &mut Context<HearthWindow>,
) -> Result<Value, String> {
    ready(window)?;
    let handle = cx.windows().into_iter().next().ok_or("no window")?;
    if PROBE.with(|probe| probe.borrow().running) {
        return Err("perf is already running".into());
    }
    PROBE.with(|probe| probe.borrow_mut().running = true);
    let run = Run;
    let path = std::env::temp_dir().join(format!("hearth-perf-{label}.json"));
    let out = path.clone();
    let label = label.to_string();
    cx.spawn(async move |this, cx| {
        let _run = run;
        let idle_from = renders();
        let idle_started = Instant::now();
        cx.background_executor().timer(Duration::from_secs(3)).await;
        let idle_renders = renders() - idle_from;
        let idle_ms = idle_started.elapsed().as_secs_f64() * 1000.0;
        let mut report = json!({
            "label": label,
            "profile": if cfg!(debug_assertions) { "dev" } else { "release" },
            "idle": {
                "ms": round(idle_ms),
                "renders": idle_renders,
                "renders_per_s": round(idle_renders as f64 * 1000.0 / idle_ms.max(1.0)),
                "polling": "unchanged during idle; suspended during forced draws",
            },
        });
        let prepared = this
            .update(cx, |this, _| {
                ready(this)?;
                PROBE.with(|probe| probe.borrow_mut().driving = true);
                Ok::<_, String>(prepare(this))
            })
            .map_err(|error| error.to_string())
            .and_then(|value| value);
        match prepared {
            Err(error) => report["error"] = json!(error),
            Ok((saved, data, groups)) => {
                report["data"] = data;
                let home = saved.selected;
                let mut results = Vec::new();
                for name in SCENARIOS {
                    let mut frame_ms = Vec::with_capacity(frames);
                    record(true);
                    let started = Instant::now();
                    let mut error = None;
                    for frame in 0..frames {
                        let at = Instant::now();
                        let stepped = this.update(cx, |this, cx| {
                            step(this, name, frame, home, &groups);
                            cx.notify();
                        });
                        if let Err(failed) = stepped {
                            error = Some(failed.to_string());
                            break;
                        }
                        let drawn = cx.update(|app| {
                            app.update_window(handle, |_, window, cx| {
                                window.refresh();
                                window.draw(cx).clear(cx);
                            })
                        });
                        if let Err(failed) = drawn {
                            error = Some(failed.to_string());
                            break;
                        }
                        frame_ms.push(at.elapsed().as_secs_f64() * 1000.0);
                        cx.background_executor()
                            .timer(Duration::from_millis(1))
                            .await;
                    }
                    let mut render_us: Vec<f64> =
                        record(false).into_iter().map(|us| us as f64).collect();
                    let total: f64 = frame_ms.iter().sum();
                    results.push(json!({
                        "name": name,
                        "frames": frame_ms.len(),
                        "requested_frames": frames,
                        "elapsed_ms": round(started.elapsed().as_secs_f64() * 1000.0),
                        "fps": round(frame_ms.len() as f64 * 1000.0 / total.max(0.001)),
                        "frame_ms_median": round(percentile(&mut frame_ms, 0.5)),
                        "frame_ms_p95": round(percentile(&mut frame_ms, 0.95)),
                        "renders": render_us.len(),
                        "render_us_median": round(percentile(&mut render_us, 0.5)),
                        "error": error,
                    }));
                    if error.is_some() {
                        break;
                    }
                }
                report["scenarios"] = json!(results);
                let _ = this.update(cx, |this, cx| {
                    saved.restore(this);
                    cx.notify();
                });
                PROBE.with(|probe| probe.borrow_mut().driving = false);
            }
        }
        if let Err(error) = std::fs::write(&out, serde_json::to_vec_pretty(&report).unwrap()) {
            eprintln!("could not write {}: {error}", out.display());
        }
    })
    .detach();
    Ok(json!({"started": true, "path": path.display().to_string(), "frames": frames}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_have_defaults_and_safe_report_names() {
        assert_eq!(args(&json!({})).unwrap(), ("run".into(), 60));
        assert_eq!(
            args(&json!({"label": "before_1", "frames": 0})).unwrap().1,
            1
        );
        assert_eq!(args(&json!({"frames": u64::MAX})).unwrap().1, 1000);
        for value in [
            json!({"label": "../../config"}),
            json!({"label": ""}),
            json!({"label": 2}),
            json!({"frames": -1}),
            json!({"frames": 1.5}),
        ] {
            assert!(args(&value).is_err());
        }
    }

    #[test]
    fn percentiles_cover_empty_and_single_samples() {
        assert_eq!(percentile(&mut [], 0.95), 0.0);
        assert_eq!(percentile(&mut [3.0], 0.5), 3.0);
        assert_eq!(percentile(&mut [4.0, 1.0, 3.0, 2.0], 0.5), 3.0);
        assert_eq!(percentile(&mut [4.0, 1.0, 3.0, 2.0], 0.95), 4.0);
    }

    #[test]
    fn clock_counts_idle_renders_and_records_only_when_enabled() {
        let _run = Run;
        let before = renders();
        drop(clock());
        assert_eq!(renders(), before + 1);
        assert!(record(true).is_empty());
        drop(clock());
        assert_eq!(record(false).len(), 1);
        drop(clock());
        assert!(record(false).is_empty());
    }
}
