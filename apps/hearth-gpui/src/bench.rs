use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::catalog::{Catalog, ServiceGroup, service_groups_hiding, visible_groups};
use super::logbuf::LogPaint;
use super::{
    Attachment, Board, BoardCache, Counts, Instance, Service, SharedCache, UrlRow, count_services,
    display_instance_state, failed_label, group_recipes, hearth_yaml_entry, logview, loss_sentence,
    parent_hint, port_line, recipe_index, remove_confirm, remove_tip, restart_tip, scope_text,
    show_install, split_instances, start_tip, stop_confirm, stop_tip, store, workspace_count,
};

const WORKSPACES: usize = 30;
const SERVICES_PER_BOARD: usize = 40;
const GROUPS_PER_BOARD: usize = 5;
const LOG_LINES: usize = 50_000;
const LOG_COLUMNS: usize = 100;
const RECIPE_NAMES: usize = 200;
const RECIPE_VERSIONS: usize = 3;
const INSTANCES: usize = 120;
const SELECTED_WORKSPACE: usize = 0;
const WARMUP_RUNS: usize = 3;
const TIMED_RUNS: usize = 7;

struct LargeFixture {
    workspaces: Vec<store::Workspace>,
    boards: Vec<Board>,
    log_lines: Rc<Vec<String>>,
    log_paints: Rc<Vec<Vec<LogPaint>>>,
    log_rows: Rc<logview::Rows>,
    log_levels: Rc<Vec<logview::LineLevel>>,
    recipes: Vec<(String, String)>,
    instances: Vec<Instance>,
}

fn build_workspace_board(workspace: usize) -> Board {
    let services: Vec<Service> = (0..SERVICES_PER_BOARD)
        .map(|index| Service {
            id: format!("workspace-{workspace:02}-service-{index:02}"),
            state: match index % 8 {
                0 | 1 => "ready",
                2 => "running",
                3 => "stopped",
                4 => "failed",
                5 => "crashed",
                6 => "running-unready",
                _ => "starting",
            }
            .into(),
        })
        .collect();
    let ids: Vec<String> = services.iter().map(|service| service.id.clone()).collect();
    let groups = ids
        .chunks(SERVICES_PER_BOARD / GROUPS_PER_BOARD)
        .enumerate()
        .map(|(index, members)| (format!("group-{index:02}"), members.to_vec()))
        .collect();
    let disabled = ids.iter().step_by(5).cloned().collect();
    let urls = services
        .iter()
        .enumerate()
        .flat_map(|(index, service)| {
            (0..2).map(move |endpoint| UrlRow {
                service: service.id.clone(),
                label: if endpoint == 0 { "http" } else { "admin" }.into(),
                url: format!(
                    "http://127.0.0.1:{}/workspace/{workspace:02}/service/{index:02}",
                    10_000 + workspace * 100 + index * 2 + endpoint
                ),
                visible: endpoint == 0,
            })
        })
        .collect();
    Board {
        services,
        urls,
        catalog: Catalog {
            groups,
            disabled,
            shared: Vec::new(),
            services: ids,
        },
        daemon_up: true,
        port: Some(9_000 + workspace as u64),
        daemon_note: String::new(),
        note: String::new(),
        stamp: Some(1),
    }
}

fn build_large_fixture() -> LargeFixture {
    let workspaces: Vec<store::Workspace> = (0..WORKSPACES)
        .map(|index| store::Workspace {
            id: format!("workspace-{index:02}"),
            path: format!(
                "/gn-121/projects/team-{:02}/workspace-{index:02}",
                index % 5
            ),
            trusted: true,
            added_at: "2026-01-01T00:00:00Z".into(),
        })
        .collect();
    let boards = (0..WORKSPACES).map(build_workspace_board).collect();
    let mut log_lines = Vec::with_capacity(LOG_LINES);
    let mut log_paints = Vec::with_capacity(LOG_LINES);
    for index in 0..LOG_LINES {
        let line = match index % 10 {
            0 => format!(
                "2026-01-01T12:00:00Z ERROR request={index:05} database connection failed; retry scheduled for workspace service"
            ),
            1 => format!(
                "    at database::connect /gn-121/projects/service/src/database.rs:42 request={index:05}"
            ),
            2 => format!(
                "2026-01-01T12:00:02Z WARN request={index:05} queue is near capacity; waiting for the worker pool"
            ),
            3 => format!("    retry detail request={index:05} backoff=250ms remaining=3"),
            4 | 5 => String::new(),
            6 => format!(
                "2026-01-01T12:00:06Z INFO request={index:05} GET /health status=200 elapsed=4ms"
            ),
            7 => format!("2026-01-01T12:00:07Z DEBUG request={index:05} cache key=café result=hit"),
            8 => format!(
                "2026-01-01T12:00:08Z INFO request={index:05} processing a deliberately long payload: {}",
                "segment/path item=café elapsed=12ms; ".repeat(6)
            ),
            _ => format!(
                "2026-01-01T12:00:09Z INFO request={index:05} transaction committed rows=24"
            ),
        };
        let paints = if line.is_empty() {
            Vec::new()
        } else {
            vec![
                LogPaint {
                    start: 0,
                    end: 20,
                    fg: Some(0x808080),
                    bg: None,
                    bold: false,
                    dim: true,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                LogPaint {
                    start: 20,
                    end: line.len(),
                    fg: Some(if index % 10 < 2 { 0xcc3333 } else { 0x33aa66 }),
                    bg: None,
                    bold: index % 10 == 0,
                    dim: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
            ]
        };
        log_lines.push(line);
        log_paints.push(paints);
    }
    // The shared log buffers the window now hands the list closure. Steady
    // frames only clone Rc's; the rebuild cost is log_wrap/log_levels below.
    let log_lines = Rc::new(log_lines);
    let log_paints = Rc::new(log_paints);
    let log_rows = Rc::new(logview::wrap(&log_lines, LOG_COLUMNS));
    let log_levels = Rc::new(logview::levels(&log_lines));
    let recipes: Vec<(String, String)> = (0..RECIPE_NAMES)
        .flat_map(|name| {
            (1..=RECIPE_VERSIONS)
                .map(move |version| (format!("recipe-{name:03}"), format!("{version}.0.0")))
        })
        .collect();
    let instances = (0..INSTANCES)
        .map(|index| {
            // Missing installations exercise exhaustive recipe lookups.
            let (name, version) = &recipes[index * 5];
            let mut attachments: Vec<Attachment> = (0..if index % 4 == 0 { 0 } else { 3 })
                .map(|offset| {
                    let workspace = &workspaces[(index + offset * 7) % WORKSPACES];
                    Attachment {
                        project_id: workspace.id.clone(),
                        root: workspace.path.clone(),
                        attached_at: "2026-01-01T00:00:00Z".into(),
                        url: Some(format!("http://127.0.0.1:{}", 20_000 + index * 3)),
                        env: vec![("SERVICE_MODE".into(), "benchmark".into())],
                        error: None,
                    }
                })
                .collect();
            attachments.sort_by(|left, right| left.root.cmp(&right.root));
            Instance {
                id: format!("{name}@{version}"),
                port: Some(20_000 + index as u64 * 3),
                extra_ports: vec![20_001 + index as u64 * 3, 20_002 + index as u64 * 3],
                extra_labels: vec!["console".into()],
                install: if index % 10 == 0 {
                    "failed"
                } else {
                    "installed"
                }
                .into(),
                install_error: (index % 10 == 0).then(|| "Synthetic install failure".into()),
                actual: match index % 4 {
                    0 => "stopped",
                    1 => "ready",
                    2 => "running-unready",
                    _ => "failed",
                }
                .into(),
                attachments,
            }
        })
        .collect();
    LargeFixture {
        workspaces,
        boards,
        log_lines,
        log_paints,
        log_rows,
        log_levels,
        recipes,
        instances,
    }
}

fn workspace_sidebar_paths(workspaces: &[store::Workspace]) {
    for workspace in workspaces {
        let path = workspace.path.clone();
        let label = ns_ui::path_name(&path);
        let tooltip = ns_ui::display_path(&path);
        let hint = parent_hint(&path);
        black_box((path, label, tooltip, hint));
    }
}

/// Reference input clone from before the board cache: urls, disabled ids and
/// service ids, deep-cloned per frame.
fn clone_workspace_render_inputs(board: &Board) -> (Vec<UrlRow>, Vec<String>, Vec<String>) {
    (
        board.urls.clone(),
        board.catalog.disabled.clone(),
        board
            .services
            .iter()
            .map(|service| service.id.clone())
            .collect(),
    )
}

/// The render path's per-row URL data work, now driven by the cached index
/// instead of a full-list scan per service.
fn workspace_row_urls(services: &[Service], urls: &[UrlRow], cache: &BoardCache) {
    for section in &cache.sections {
        for &index in &section.indexes {
            let id = services[index].id.clone();
            let is_disabled = cache.disabled.contains(&id);
            if let Some(url_indexes) = cache.urls_by_service.get(&id) {
                for &url_index in url_indexes {
                    let url = &urls[url_index];
                    let target = url.url.clone();
                    let label = (!url.label.is_empty()).then(|| url.label.clone());
                    let tooltip = format!("Open {}", url.url);
                    let shown = url.url.clone();
                    black_box((url_index, target, label, tooltip, shown, url.visible));
                }
            }
            black_box((id, is_disabled));
        }
    }
}

fn workspace_scope_text(workspace: &store::Workspace, cache: &BoardCache) -> String {
    let root = workspace.path.clone();
    let name = ns_ui::path_name(&root);
    scope_text(&name, &cache.counts)
}

/// Reference failed-name scan from before the board cache.
fn workspace_failed_names(board: &Board) -> Vec<String> {
    board
        .services
        .iter()
        .filter(|service| matches!(service.state.as_str(), "failed" | "crashed"))
        .filter(|service| {
            !board
                .catalog
                .disabled
                .iter()
                .any(|item| item == &service.id)
        })
        .map(|service| service.id.clone())
        .collect()
}

/// One steady workspace frame after caching: sidebar paths, cached section and
/// URL reads, cached aggregates, and the shared log buffers.
fn workspace_frame_data_work(fixture: &LargeFixture, cache: &BoardCache) {
    workspace_sidebar_paths(&fixture.workspaces);
    // Exactly ONE selected board, never the sum of all 30 stored boards.
    let board = &fixture.boards[SELECTED_WORKSPACE];
    workspace_row_urls(&board.services, &board.urls, cache);
    let summary = workspace_scope_text(&fixture.workspaces[SELECTED_WORKSPACE], cache);
    let failed = failed_label(&cache.failed);
    let lines = Rc::clone(&fixture.log_lines);
    let paints = Rc::clone(&fixture.log_paints);
    let rows = Rc::clone(&fixture.log_rows);
    let levels = Rc::clone(&fixture.log_levels);
    black_box((summary, failed, lines, paints, rows, levels));
}

fn shared_sidebar_text(fixture: &LargeFixture) {
    for instance in &fixture.instances {
        let id = instance.id.clone();
        let state = if instance.actual.is_empty() {
            "Unknown state".to_string()
        } else {
            instance.actual.clone()
        };
        black_box((id.clone(), state.clone(), id, state));
    }
    for (name, version) in &fixture.recipes {
        let name = name.clone();
        let version = version.clone();
        let full = format!("{name}@{version}");
        black_box((name, version, full));
    }
}

/// The render path's recipe-version reads, now a cached row per version
/// instead of a linear recipes scan plus an instances scan.
fn shared_recipe_version_lookups(cache: &SharedCache) {
    for group in &cache.recipe_groups {
        for entry in &group.versions {
            black_box((entry.index, entry.id.as_str(), entry.installed));
        }
    }
}

/// The render path's per-instance text, now cloned from the shared cache
/// instead of reformatted every frame.
fn shared_instance_row_text(instances: &[Instance], cache: &SharedCache) {
    for index in cache.in_use.iter().chain(cache.idle.iter()) {
        let instance = &instances[*index];
        let id = instance.id.clone();
        let state = display_instance_state(&instance.actual, &id, &[], true);
        let text = &cache.rows[*index];
        let stop = text.stop_message.clone();
        let remove = text.remove_message.clone();
        let restart = text.restart_message.clone();
        let facts = text.facts.clone();
        let error = text.error.clone();
        black_box((id, state, stop, remove, restart, facts, error));
    }
}

/// One steady shared frame after caching.
fn shared_frame_data_work(fixture: &LargeFixture, cache: &SharedCache) {
    // An alternative pane, with its log dock closed. No workspace work here.
    shared_sidebar_text(fixture);
    shared_instance_row_text(&fixture.instances, cache);
    shared_recipe_version_lookups(cache);
    let summary = format!(
        "{} recipes · {} instances",
        fixture.recipes.len(),
        fixture.instances.len()
    );
    black_box((summary, cache.in_use.len(), cache.idle.len()));
}

fn measure_median_ms<T>(name: &str, mut operation: impl FnMut() -> T) {
    for _ in 0..WARMUP_RUNS {
        black_box(operation());
    }
    let mut samples = [Duration::ZERO; TIMED_RUNS];
    for sample in &mut samples {
        let start = Instant::now();
        // Consume outputs before stopping the clock, including their disposal.
        black_box(operation());
        *sample = start.elapsed();
    }
    samples.sort_unstable();
    let milliseconds = samples[TIMED_RUNS / 2].as_secs_f64() * 1_000.;
    println!("BENCH {name}_ms={milliseconds:.6}");
}

#[test]
#[ignore]
fn perf_bench_large() {
    let fixture = build_large_fixture();
    let board = &fixture.boards[SELECTED_WORKSPACE];
    // Cold recompute: the cache refresh a board load or a filter change pays.
    let board_cache = BoardCache::refresh(&board.services, &board.urls, &board.catalog, true);
    let shared_cache = SharedCache::refresh(&fixture.recipes, &fixture.instances);
    // Cold-path components of the refresh, kept as isolated measurements.
    let inputs = clone_workspace_render_inputs(board);
    let groups = visible_groups(&board.catalog.groups, &inputs.1);
    let recipe_groups = group_recipes(&fixture.recipes);
    let (in_use, idle) = split_instances(&fixture.instances);
    println!(
        "DATASET workspaces={} boards={} services_per_board={} groups_per_board={} disabled_per_board={} urls_per_service=2 urls_per_board={}",
        fixture.workspaces.len(),
        fixture.boards.len(),
        board.services.len(),
        board.catalog.groups.len(),
        board.catalog.disabled.len(),
        board.urls.len()
    );
    println!(
        "DATASET log_lines={} log_paint_vectors={} log_paint_spans={} log_bytes={} wrapped_rows={} log_columns={}",
        fixture.log_lines.len(),
        fixture.log_paints.len(),
        fixture.log_paints.iter().map(Vec::len).sum::<usize>(),
        fixture.log_lines.iter().map(String::len).sum::<usize>(),
        fixture.log_rows.len(),
        LOG_COLUMNS
    );
    println!(
        "DATASET recipe_names={} versions_per_name={} recipes={} instances={} in_use={} idle={} attachments={}",
        recipe_groups.len(),
        RECIPE_VERSIONS,
        fixture.recipes.len(),
        fixture.instances.len(),
        in_use.len(),
        idle.len(),
        fixture
            .instances
            .iter()
            .map(|item| item.attachments.len())
            .sum::<usize>()
    );
    println!(
        "SCOPE workspace: 30 sidebar rows + ONE 40-service board (hide_disabled=true, 32 visible rows, all groups expanded) + shared 50k-line log buffers; no shared work."
    );
    println!(
        "SCOPE shared: 120 instance and 600 recipe sidebar rows + shared body; log dock closed; no workspace work."
    );
    println!(
        "NOTE no layout/GPU/element building, event wiring, or selected-instance details; totals are data-work lower bounds, not actual frame times. No in-flight jobs."
    );
    println!(
        "NOTE board_cache_refresh/shared_cache_refresh are the cold recompute a board or shared load pays; every other metric is the cached per-frame path. log_wrap/log_levels run on resize or log refresh."
    );
    println!(
        "NOTE median of {TIMED_RUNS} runs after {WARMUP_RUNS} warm-ups per measurement; output disposal included; totals timed independently, not summed component medians."
    );

    measure_median_ms("sidebar_paths", || {
        workspace_sidebar_paths(black_box(&fixture.workspaces))
    });
    measure_median_ms("workspace_input_clones", || {
        // Render borrows the cache; the old per-frame input clone is gone.
        (
            black_box(&board_cache)
                .sections
                .iter()
                .map(|s| s.indexes.len())
                .sum::<usize>(),
            board_cache.disabled.len(),
            board_cache.urls_by_service.len(),
        )
    });
    measure_median_ms("visible_groups", || {
        visible_groups(black_box(&board.catalog.groups), black_box(&inputs.1))
    });
    measure_median_ms("service_groups_hiding", || {
        service_groups_hiding(
            black_box(&inputs.2),
            black_box(&groups),
            black_box(&inputs.1),
        )
    });
    measure_median_ms("workspace_row_urls", || {
        workspace_row_urls(
            black_box(&board.services),
            black_box(&board.urls),
            black_box(&board_cache),
        )
    });
    measure_median_ms("workspace_counts_scope", || {
        workspace_scope_text(
            black_box(&fixture.workspaces[SELECTED_WORKSPACE]),
            black_box(&board_cache),
        )
    });
    measure_median_ms("workspace_failed_names", || {
        failed_label(black_box(&board_cache.failed))
    });
    measure_median_ms("log_lines_clone", || {
        Rc::clone(black_box(&fixture.log_lines))
    });
    measure_median_ms("log_paints_clone", || {
        Rc::clone(black_box(&fixture.log_paints))
    });
    measure_median_ms("log_cache_rc_clones", || {
        (
            Rc::clone(black_box(&fixture.log_rows)),
            Rc::clone(black_box(&fixture.log_levels)),
        )
    });
    measure_median_ms("board_cache_refresh", || {
        BoardCache::refresh(
            black_box(&board.services),
            black_box(&board.urls),
            black_box(&board.catalog),
            black_box(true),
        )
    });
    measure_median_ms("per_frame_total", || {
        workspace_frame_data_work(black_box(&fixture), black_box(&board_cache))
    });

    measure_median_ms("shared_sidebar_text", || {
        shared_sidebar_text(black_box(&fixture))
    });
    measure_median_ms("split_instances", || {
        split_instances(black_box(&fixture.instances))
    });
    measure_median_ms("group_recipes", || {
        group_recipes(black_box(&fixture.recipes))
    });
    measure_median_ms("recipe_version_lookups", || {
        shared_recipe_version_lookups(black_box(&shared_cache))
    });
    measure_median_ms("shared_instance_row_text", || {
        shared_instance_row_text(black_box(&fixture.instances), black_box(&shared_cache))
    });
    measure_median_ms("shared_cache_refresh", || {
        SharedCache::refresh(black_box(&fixture.recipes), black_box(&fixture.instances))
    });
    measure_median_ms("shared_frame_total", || {
        shared_frame_data_work(black_box(&fixture), black_box(&shared_cache))
    });

    measure_median_ms("log_wrap", || {
        logview::wrap(black_box(&fixture.log_lines), black_box(LOG_COLUMNS))
    });
    measure_median_ms("log_levels", || {
        logview::levels(black_box(&fixture.log_lines))
    });
}

/// The old per-frame computation of the board sections, kept as the reference
/// the cached helper must match exactly.
fn reference_sections(board: &Board, hide_disabled: bool) -> Vec<ServiceGroup> {
    let ids: Vec<String> = board
        .services
        .iter()
        .map(|service| service.id.clone())
        .collect();
    let groups = if hide_disabled {
        visible_groups(&board.catalog.groups, &board.catalog.disabled)
    } else {
        board.catalog.groups.clone()
    };
    service_groups_hiding(
        &ids,
        &groups,
        if hide_disabled {
            &board.catalog.disabled
        } else {
            &[]
        },
    )
}

fn reference_failed(board: &Board, hide_disabled: bool) -> Vec<String> {
    board
        .services
        .iter()
        .filter(|service| matches!(service.state.as_str(), "failed" | "crashed"))
        .filter(|service| {
            !(hide_disabled
                && board
                    .catalog
                    .disabled
                    .iter()
                    .any(|item| item == &service.id))
        })
        .map(|service| service.id.clone())
        .collect()
}

fn assert_board_cache_matches_reference(board: &Board, hide_disabled: bool) {
    let cache = BoardCache::refresh(&board.services, &board.urls, &board.catalog, hide_disabled);
    assert_eq!(cache.sections, reference_sections(board, hide_disabled));
    for service in &board.services {
        let expected: Vec<usize> = board
            .urls
            .iter()
            .enumerate()
            .filter(|(_, url)| url.service == service.id)
            .map(|(index, _)| index)
            .collect();
        // An absent entry and an empty one both render zero URL rows.
        let cached = cache
            .urls_by_service
            .get(&service.id)
            .map_or([].as_slice(), Vec::as_slice);
        assert_eq!(
            cached,
            expected.as_slice(),
            "url indexes for {}",
            service.id
        );
        assert_eq!(
            cache.disabled.contains(&service.id),
            board
                .catalog
                .disabled
                .iter()
                .any(|item| item == &service.id),
            "disabled flag for {}",
            service.id
        );
    }
    assert_eq!(
        cache.counts,
        count_services(&board.services, &board.catalog.disabled, hide_disabled)
    );
    assert_eq!(cache.failed, reference_failed(board, hide_disabled));
}

#[test]
fn board_cache_matches_the_per_frame_reference_on_bench_data() {
    let fixture = build_large_fixture();
    for board in &fixture.boards {
        assert_board_cache_matches_reference(board, true);
        assert_board_cache_matches_reference(board, false);
    }
}

#[test]
fn board_cache_matches_the_per_frame_reference_on_edge_cases() {
    // Empty board.
    let empty = Board {
        services: Vec::new(),
        urls: Vec::new(),
        catalog: Catalog::default(),
        daemon_up: false,
        port: None,
        daemon_note: String::new(),
        note: String::new(),
        stamp: None,
    };
    assert_board_cache_matches_reference(&empty, true);
    assert_board_cache_matches_reference(&empty, false);

    // Every service disabled: hiding leaves nothing to list.
    let all_disabled = Board {
        services: vec![
            Service {
                id: "api".into(),
                state: "failed".into(),
            },
            Service {
                id: "web".into(),
                state: "stopped".into(),
            },
        ],
        urls: Vec::new(),
        catalog: Catalog {
            disabled: vec!["api".into(), "web".into()],
            ..Catalog::default()
        },
        daemon_up: true,
        port: None,
        daemon_note: String::new(),
        note: String::new(),
        stamp: None,
    };
    assert_board_cache_matches_reference(&all_disabled, true);
    assert_board_cache_matches_reference(&all_disabled, false);
    let hidden = BoardCache::refresh(
        &all_disabled.services,
        &all_disabled.urls,
        &all_disabled.catalog,
        true,
    );
    assert_eq!(
        hidden.counts,
        Counts {
            ready: 0,
            stopped: 0,
            failed: 0,
            hidden: 2
        }
    );
    assert!(hidden.failed.is_empty());

    // A dropped group, an Other trail, a URL for a service the board does not
    // list, and a repeated URL service.
    let grouped = Board {
        services: vec![
            Service {
                id: "mongod".into(),
                state: "ready".into(),
            },
            Service {
                id: "redis".into(),
                state: "failed".into(),
            },
            Service {
                id: "api".into(),
                state: "running".into(),
            },
            Service {
                id: "fe".into(),
                state: "stopped".into(),
            },
        ],
        urls: vec![
            UrlRow {
                service: "api".into(),
                label: "http".into(),
                url: "http://127.0.0.1:1".into(),
                visible: true,
            },
            UrlRow {
                service: "api".into(),
                label: String::new(),
                url: "http://127.0.0.1:2".into(),
                visible: false,
            },
            UrlRow {
                service: "ghost".into(),
                label: "gone".into(),
                url: "http://127.0.0.1:3".into(),
                visible: true,
            },
        ],
        catalog: Catalog {
            groups: vec![
                ("infra".into(), vec!["mongod".into(), "redis".into()]),
                ("core".into(), vec!["api".into()]),
            ],
            disabled: vec!["redis".into()],
            shared: Vec::new(),
            services: Vec::new(),
        },
        daemon_up: true,
        port: None,
        daemon_note: String::new(),
        note: String::new(),
        stamp: None,
    };
    assert_board_cache_matches_reference(&grouped, true);
    assert_board_cache_matches_reference(&grouped, false);
    let cache = BoardCache::refresh(&grouped.services, &grouped.urls, &grouped.catalog, true);
    assert_eq!(
        cache.urls_by_service.get("api").map(Vec::as_slice),
        Some(&[0usize, 1usize][..])
    );
    // A URL for a service the board does not list still indexes by its id.
    assert_eq!(
        cache.urls_by_service.get("ghost").map(Vec::as_slice),
        Some(&[2usize][..])
    );
}

fn assert_shared_cache_matches_reference(
    recipes: &[(String, String)],
    instances: &[Instance],
) -> SharedCache {
    let cache = SharedCache::refresh(recipes, instances);
    let (in_use, idle) = split_instances(instances);
    assert_eq!(cache.in_use, in_use);
    assert_eq!(cache.idle, idle);
    let reference = group_recipes(recipes);
    assert_eq!(cache.recipe_groups.len(), reference.len());
    for (group, ref_group) in cache.recipe_groups.iter().zip(&reference) {
        assert_eq!(group.name, ref_group.name);
        assert_eq!(group.versions.len(), ref_group.versions.len());
        for (entry, version) in group.versions.iter().zip(&ref_group.versions) {
            assert_eq!(entry.version, *version);
            assert_eq!(
                entry.index,
                recipe_index(recipes, &group.name, version).unwrap()
            );
            let id = format!("{}@{version}", group.name);
            assert_eq!(entry.id, id);
            assert_eq!(
                entry.installed,
                instances.iter().any(|item| item.id == id),
                "installed flag for {id}"
            );
            assert_eq!(entry.copy_entry, hearth_yaml_entry(&group.name, version));
            assert_eq!(
                entry.copy_tip,
                format!("Copy a hearth.yaml entry for {id}.")
            );
            assert_eq!(
                entry.install_tip,
                if entry.installed {
                    format!(
                        "Reinstall {id}. This replaces the installed copy. This can start the shared daemon."
                    )
                } else {
                    format!(
                        "Install {id}. This can start the shared daemon, then install the recipe."
                    )
                }
            );
        }
    }
    for (index, instance) in instances.iter().enumerate() {
        let count = instance.attachments.len();
        let ports = port_line(instance.port, &instance.extra_ports, &instance.extra_labels);
        let mut facts = workspace_count(count);
        if !ports.is_empty() {
            facts = format!("{ports} · {facts}");
        }
        if show_install(&instance.install) {
            facts = format!("{facts} · {}", instance.install);
        }
        let text = &cache.rows[index];
        assert_eq!(text.stop_tip, stop_tip(&instance.id, count));
        assert_eq!(text.start_tip, start_tip(&instance.id));
        assert_eq!(text.restart_tip, restart_tip(&instance.id, count));
        assert_eq!(text.remove_tip, remove_tip(&instance.id, count));
        assert_eq!(text.stop_message, stop_confirm(instance));
        assert_eq!(text.remove_message, remove_confirm(instance));
        assert_eq!(
            text.restart_message,
            format!(
                "The instance stops, then starts. {}",
                loss_sentence(count, " until it is back.")
            )
        );
        assert_eq!(text.facts, facts);
        assert_eq!(text.error, instance.install_error);
    }
    cache
}

#[test]
fn shared_cache_matches_the_per_frame_reference_on_bench_data() {
    let fixture = build_large_fixture();
    assert_shared_cache_matches_reference(&fixture.recipes, &fixture.instances);
}

#[test]
fn shared_cache_matches_the_per_frame_reference_on_edge_cases() {
    // Empty registry.
    assert_shared_cache_matches_reference(&[], &[]);

    // One idle instance, no ports, blank install, and a recipe that is not
    // installed anywhere.
    let instance = Instance {
        id: "redis@8".into(),
        port: None,
        extra_ports: Vec::new(),
        extra_labels: Vec::new(),
        install: String::new(),
        install_error: None,
        actual: String::new(),
        attachments: Vec::new(),
    };
    let cache = assert_shared_cache_matches_reference(
        &[("redis".into(), "8".into()), ("minio".into(), "1".into())],
        &[instance],
    );
    assert_eq!(cache.idle, vec![0]);
    assert_eq!(cache.recipe_groups.len(), 2);
    assert_eq!(cache.recipe_groups[0].name, "minio");
    assert!(!cache.recipe_groups[0].versions[0].installed);
    assert_eq!(cache.recipe_groups[1].versions[0].id, "redis@8");
    assert!(cache.recipe_groups[1].versions[0].installed);
    assert_eq!(cache.rows[0].facts, "0 workspaces");

    // An in-use instance with a failing install and port labels.
    let attached = Instance {
        id: "kafka@4.3.1".into(),
        port: Some(43110),
        extra_ports: vec![43111],
        extra_labels: vec!["controller".into()],
        install: "failed".into(),
        install_error: Some("extract failed".into()),
        actual: "ready".into(),
        attachments: vec![Attachment {
            project_id: "zeta".into(),
            root: "/tmp/zeta".into(),
            attached_at: "2026-01-01T00:00:00Z".into(),
            url: None,
            env: Vec::new(),
            error: None,
        }],
    };
    let cache =
        assert_shared_cache_matches_reference(&[("kafka".into(), "4.3.1".into())], &[attached]);
    assert_eq!(cache.in_use, vec![0]);
    assert_eq!(
        cache.rows[0].facts,
        "port 43110 · controller 43111 · 1 workspace · failed"
    );
    assert_eq!(cache.rows[0].error.as_deref(), Some("extract failed"));
}

#[test]
fn large_fixture_has_expected_sizes_membership_and_paint_ranges() {
    let fixture = build_large_fixture();
    assert_eq!(fixture.workspaces.len(), WORKSPACES);
    assert_eq!(fixture.boards.len(), WORKSPACES);
    for board in &fixture.boards {
        assert_eq!(board.services.len(), SERVICES_PER_BOARD);
        assert_eq!(board.catalog.groups.len(), GROUPS_PER_BOARD);
        assert_eq!(board.catalog.disabled.len(), 8);
        assert_eq!(board.urls.len(), SERVICES_PER_BOARD * 2);
        let inputs = clone_workspace_render_inputs(board);
        assert_eq!(board.catalog.services, inputs.2);
        let mut membership = [0; SERVICES_PER_BOARD];
        for (_, members) in &board.catalog.groups {
            assert_eq!(members.len(), SERVICES_PER_BOARD / GROUPS_PER_BOARD);
            for member in members {
                let index = inputs.2.iter().position(|id| id == member).unwrap();
                membership[index] += 1;
            }
        }
        assert!(membership.iter().all(|count| *count == 1));
        for disabled in &inputs.1 {
            assert!(inputs.2.contains(disabled));
        }
        for id in &inputs.2 {
            assert_eq!(
                board.urls.iter().filter(|url| &url.service == id).count(),
                2
            );
        }
        let groups = visible_groups(&board.catalog.groups, &inputs.1);
        let sections = service_groups_hiding(&inputs.2, &groups, &inputs.1);
        assert_eq!(sections.len(), GROUPS_PER_BOARD);
        let mut visible_membership = [0; SERVICES_PER_BOARD];
        for section in sections {
            let (_, members) = groups
                .iter()
                .find(|(name, _)| name == &section.title)
                .unwrap();
            assert!(section.actions);
            for index in section.indexes {
                assert!(members.contains(&inputs.2[index]));
                visible_membership[index] += 1;
            }
        }
        for (index, id) in inputs.2.iter().enumerate() {
            assert_eq!(
                visible_membership[index],
                if inputs.1.contains(id) { 0 } else { 1 }
            );
        }
        let counts = count_services(&board.services, &inputs.1, true);
        assert_eq!(counts.hidden, 8);
        assert_eq!(counts.ready + counts.stopped + counts.failed, 32);
        assert_eq!(workspace_failed_names(board).len(), counts.failed);
    }
    assert_eq!(fixture.log_lines.len(), LOG_LINES);
    assert_eq!(fixture.log_paints.len(), LOG_LINES);
    assert_eq!(fixture.log_levels.len(), LOG_LINES);
    assert!(fixture.log_rows.rows.iter().any(|row| row.cont));
    assert_eq!(
        fixture.log_paints.iter().map(Vec::len).sum::<usize>(),
        80_000
    );
    for (line, paints) in fixture.log_lines.iter().zip(fixture.log_paints.iter()) {
        assert_eq!(paints.len(), if line.is_empty() { 0 } else { 2 });
        for paint in paints {
            assert!(paint.start < paint.end && paint.end <= line.len());
            assert!(line.is_char_boundary(paint.start));
            assert!(line.is_char_boundary(paint.end));
        }
    }
    assert_eq!(fixture.recipes.len(), RECIPE_NAMES * RECIPE_VERSIONS);
    let groups = group_recipes(&fixture.recipes);
    assert_eq!(groups.len(), RECIPE_NAMES);
    assert!(
        groups
            .iter()
            .all(|group| group.versions.len() == RECIPE_VERSIONS)
    );
    assert_eq!(fixture.instances.len(), INSTANCES);
    let (in_use, idle) = split_instances(&fixture.instances);
    assert_eq!((in_use.len(), idle.len()), (90, 30));
    assert_eq!(
        fixture
            .instances
            .iter()
            .map(|item| item.attachments.len())
            .sum::<usize>(),
        270
    );
    for instance in &fixture.instances {
        assert!(
            fixture
                .recipes
                .iter()
                .any(|(name, version)| instance.id == format!("{name}@{version}"))
        );
        assert!(
            instance
                .attachments
                .windows(2)
                .all(|pair| pair[0].root < pair[1].root)
        );
        for attachment in &instance.attachments {
            assert!(
                fixture
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.id == attachment.project_id
                        && workspace.path == attachment.root)
            );
        }
    }
}
