//! Env-gated TUI timings. Set `HEARTH_TUI_PROFILE=1` to write a log. The file is truncated when
//! the TUI starts.
//! `HEARTH_TUI_PROFILE_PATH` overrides the file (default `/tmp/hearth-tui-profile.log`).
//! Nothing is written unless the variable is exactly `1`.
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Bytes the terminal writer flushed since the last [`take_write_bytes`] — always tracked
/// (one relaxed atomic add per write call) so `finish_draw` can pair ms spent with bytes sent.
static WRITE_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn note_write_bytes(n: usize) {
    WRITE_BYTES.fetch_add(n as u64, Ordering::Relaxed);
}

pub fn take_write_bytes() -> u64 {
    WRITE_BYTES.swap(0, Ordering::Relaxed)
}

/// Packets the writer channel had to drop because the terminal could not drain them — each
/// drop is followed by a full repaint once the queue empties.
static FRAME_DROPS: AtomicU64 = AtomicU64::new(0);

pub fn note_frame_drop() {
    FRAME_DROPS.fetch_add(1, Ordering::Relaxed);
}

fn frame_drops() -> u64 {
    FRAME_DROPS.load(Ordering::Relaxed)
}

struct Stats {
    window_start: Instant,
    sse: HashMap<String, u64>,
    http_n: HashMap<String, u64>,
    http_us: HashMap<String, u64>,
    http_max_us: HashMap<String, u64>,
    branch_n: HashMap<String, u64>,
    branch_us: HashMap<String, u64>,
    branch_max_us: HashMap<String, u64>,
    draws: u64,
    draw_us: u64,
    draw_max_us: u64,
    draw_bytes: u64,
    draw_max_bytes: u64,
    keys: u64,
    key_us: u64,
    key_max_us: u64,
    queue_max: usize,
}

struct Profiler {
    file: Mutex<BufWriter<std::fs::File>>,
    stats: Mutex<Stats>,
}

static PROFILER: OnceLock<Option<Profiler>> = OnceLock::new();

fn profiler() -> Option<&'static Profiler> {
    PROFILER
        .get_or_init(|| {
            if std::env::var("HEARTH_TUI_PROFILE").ok().as_deref() != Some("1") {
                return None;
            }
            let path = std::env::var("HEARTH_TUI_PROFILE_PATH")
                .unwrap_or_else(|_| "/tmp/hearth-tui-profile.log".to_string());
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .ok()?;
            let profiler = Profiler {
                file: Mutex::new(BufWriter::new(file)),
                stats: Mutex::new(Stats {
                    window_start: Instant::now(),
                    sse: HashMap::new(),
                    http_n: HashMap::new(),
                    http_us: HashMap::new(),
                    http_max_us: HashMap::new(),
                    branch_n: HashMap::new(),
                    branch_us: HashMap::new(),
                    branch_max_us: HashMap::new(),
                    draws: 0,
                    draw_us: 0,
                    draw_max_us: 0,
                    draw_bytes: 0,
                    draw_max_bytes: 0,
                    keys: 0,
                    key_us: 0,
                    key_max_us: 0,
                    queue_max: 0,
                }),
            };
            let _ = profiler.write_line(&format!("START unix_us={}", unix_us()));
            Some(profiler)
        })
        .as_ref()
}

fn unix_us() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

impl Profiler {
    fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(file, "{line}")?;
        file.flush()?;
        Ok(())
    }

    fn bump(map: &mut HashMap<String, u64>, key: &str, by: u64) {
        *map.entry(key.to_string()).or_insert(0) += by;
    }

    fn maybe_roll(&self, stats: &mut Stats) {
        if stats.window_start.elapsed() < Duration::from_secs(1) {
            return;
        }
        let elapsed_ms = stats.window_start.elapsed().as_millis();
        let sse = format_map(&stats.sse);
        let http_n = format_map(&stats.http_n);
        let http_us = format_map(&stats.http_us);
        let http_max = format_map(&stats.http_max_us);
        let branch_n = format_map(&stats.branch_n);
        let branch_us = format_map(&stats.branch_us);
        let branch_max = format_map(&stats.branch_max_us);
        let line = format!(
            "SUM window_ms={elapsed_ms} sse={{{sse}}} http_n={{{http_n}}} http_us={{{http_us}}} http_max_us={{{http_max}}} branch_n={{{branch_n}}} branch_us={{{branch_us}}} branch_max_us={{{branch_max}}} draws={} draw_us={} draw_max_us={} draw_bytes={} draw_max_bytes={} drops={} keys={} key_us={} key_max_us={} queue_max={}",
            stats.draws,
            stats.draw_us,
            stats.draw_max_us,
            stats.draw_bytes,
            stats.draw_max_bytes,
            frame_drops(),
            stats.keys,
            stats.key_us,
            stats.key_max_us,
            stats.queue_max,
        );
        let _ = self.write_line(&line);
        *stats = Stats {
            window_start: Instant::now(),
            sse: HashMap::new(),
            http_n: HashMap::new(),
            http_us: HashMap::new(),
            http_max_us: HashMap::new(),
            branch_n: HashMap::new(),
            branch_us: HashMap::new(),
            branch_max_us: HashMap::new(),
            draws: 0,
            draw_us: 0,
            draw_max_us: 0,
            draw_bytes: 0,
            draw_max_bytes: 0,
            keys: 0,
            key_us: 0,
            key_max_us: 0,
            queue_max: 0,
        };
    }
}

fn format_map(map: &HashMap<String, u64>) -> String {
    let mut pairs: Vec<_> = map.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn sse(event_type: &str, queue_depth: usize) {
    let Some(profiler) = profiler() else {
        return;
    };
    let mut stats = profiler.stats.lock().unwrap_or_else(|e| e.into_inner());
    Profiler::bump(&mut stats.sse, event_type, 1);
    stats.queue_max = stats.queue_max.max(queue_depth);
    profiler.maybe_roll(&mut stats);
}

pub fn http(name: &str, elapsed: Duration) {
    let Some(profiler) = profiler() else {
        return;
    };
    let us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    let mut stats = profiler.stats.lock().unwrap_or_else(|e| e.into_inner());
    Profiler::bump(&mut stats.http_n, name, 1);
    Profiler::bump(&mut stats.http_us, name, us);
    let slot = stats.http_max_us.entry(name.to_string()).or_insert(0);
    *slot = (*slot).max(us);
    profiler.maybe_roll(&mut stats);
}

pub fn branch(name: &str, elapsed: Duration, queue_depth: usize) {
    let Some(profiler) = profiler() else {
        return;
    };
    let us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    let mut stats = profiler.stats.lock().unwrap_or_else(|e| e.into_inner());
    Profiler::bump(&mut stats.branch_n, name, 1);
    Profiler::bump(&mut stats.branch_us, name, us);
    let slot = stats.branch_max_us.entry(name.to_string()).or_insert(0);
    *slot = (*slot).max(us);
    stats.queue_max = stats.queue_max.max(queue_depth);
    profiler.maybe_roll(&mut stats);
}

pub fn draw(elapsed: Duration, bytes: u64) {
    let Some(profiler) = profiler() else {
        return;
    };
    let us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    let mut stats = profiler.stats.lock().unwrap_or_else(|e| e.into_inner());
    stats.draws += 1;
    stats.draw_us += us;
    stats.draw_max_us = stats.draw_max_us.max(us);
    stats.draw_bytes += bytes;
    stats.draw_max_bytes = stats.draw_max_bytes.max(bytes);
    profiler.maybe_roll(&mut stats);
}

/// Wall time from reading a key or wheel event until the draw that follows it.
pub fn input_latency(kind: &str, elapsed: Duration) {
    let Some(profiler) = profiler() else {
        return;
    };
    let us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    let _ = profiler.write_line(&format!(
        "INPUT kind={kind} unix_us={} lat_us={us}",
        unix_us()
    ));
    let mut stats = profiler.stats.lock().unwrap_or_else(|e| e.into_inner());
    stats.keys += 1;
    stats.key_us += us;
    stats.key_max_us = stats.key_max_us.max(us);
    profiler.maybe_roll(&mut stats);
}

pub fn input_dropped(kind: &str) {
    let Some(profiler) = profiler() else {
        return;
    };
    let _ = profiler.write_line(&format!(
        "INPUT kind={kind} unix_us={} lat_us=none",
        unix_us()
    ));
}
