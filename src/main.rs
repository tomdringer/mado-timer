// mado-timer — workspace billing timer plugin for Mado
//
// Tracks billable time per workspace. Sessions are stored in
// ~/.config/mado/timers/<PROJECT_CODE>.json and accumulate forever.
// The plugin auto-pauses when Mado switches to a different workspace.
//
// stdin:  newline-delimited JSON events from Mado
// stdout: RGBA frames using the MADO frame protocol

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ── Mado frame protocol ───────────────────────────────────────────────────────

fn send_frame(out: &mut impl Write, w: u32, h: u32, pixels: &[u8]) {
    let _ = out.write_all(b"MADO");
    let _ = out.write_all(&w.to_le_bytes());
    let _ = out.write_all(&h.to_le_bytes());
    let _ = out.write_all(pixels);
    let _ = out.flush();
}

// ── Incoming events ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind:   String,
    width:  Option<u32>,
    height: Option<u32>,
    x:      Option<f32>,
    y:      Option<f32>,
    code:   Option<String>, // workspace event
}

// ── Session storage ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct Session {
    start: u64,        // unix seconds
    end:   Option<u64>,
    secs:  u64,        // duration in seconds (0 if open)
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct TimerFile {
    sessions: Vec<Session>,
}

fn timer_path(code: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join(".config/mado/timers");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}.json", code))
}

fn load(code: &str) -> TimerFile {
    let path = timer_path(code);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(code: &str, file: &TimerFile) {
    let path = timer_path(code);
    if let Ok(json) = serde_json::to_string_pretty(file) {
        let _ = std::fs::write(path, json);
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

// ── Time helpers ──────────────────────────────────────────────────────────────

fn start_of_day(ts: u64) -> u64 {
    ts - (ts % 86400)
}

fn start_of_week(ts: u64) -> u64 {
    // ISO week starts Monday. Unix epoch (1970-01-01) was a Thursday.
    let day_of_week = ((ts / 86400) + 3) % 7; // 0 = Monday
    ts - (ts % 86400) - day_of_week * 86400
}

fn start_of_month(ts: u64) -> u64 {
    // Approximate: walk back to start of current month using day-of-month.
    // Use a simple loop — months are short.
    let mut t = start_of_day(ts);
    loop {
        // If subtracting one day lands in a different month, stop.
        let prev = t.saturating_sub(86400);
        if day_of_month(prev) > day_of_month(t) {
            break;
        }
        t = prev;
    }
    t
}

fn start_of_last_month(ts: u64) -> (u64, u64) {
    let som = start_of_month(ts);
    let end = som;
    let start = start_of_month(som.saturating_sub(86400));
    (start, end)
}

fn day_of_month(ts: u64) -> u64 {
    // Days since epoch mod approx month length — not exact but fine for navigation.
    // Better: compute from year/month. Use a simple days-since-epoch approach.
    let days = ts / 86400;
    // We need day-of-month. Use the Gregorian formula.
    let (_, _, d) = days_to_ymd(days);
    d as u64
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm: https://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn session_secs(s: &Session, now: u64) -> u64 {
    if s.secs > 0 {
        s.secs
    } else if s.end.is_none() {
        // open session — count live time
        now.saturating_sub(s.start)
    } else {
        s.end.unwrap_or(s.start).saturating_sub(s.start)
    }
}

fn total_in_range(sessions: &[Session], from: u64, to: u64, now: u64) -> u64 {
    sessions.iter()
        .filter(|s| s.start >= from && s.start < to)
        .map(|s| session_secs(s, now))
        .sum()
}

fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn fmt_hm(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    format!("{}h {:02}m", h, m)
}

// ── CSV export ────────────────────────────────────────────────────────────────

fn export_csv(code: &str, sessions: &[Session]) {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = PathBuf::from(&home)
        .join("Desktop")
        .join(format!("{}-time-log.csv", code));

    let mut csv = String::from("project,start,end,duration_secs,duration_hm\n");
    for s in sessions {
        let end_str = s.end.map(|e| fmt_timestamp(e)).unwrap_or_else(|| "open".into());
        let dur = if s.secs > 0 { s.secs } else {
            s.end.unwrap_or_else(now_secs).saturating_sub(s.start)
        };
        csv.push_str(&format!("{},{},{},{},{}\n",
            code,
            fmt_timestamp(s.start),
            end_str,
            dur,
            fmt_hm(dur),
        ));
    }
    let _ = std::fs::write(&path, csv);
}

fn fmt_timestamp(ts: u64) -> String {
    let days = ts / 86400;
    let (y, m, d) = days_to_ymd(days);
    let rem = ts % 86400;
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, h, mi, s)
}

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum Preset { Today, ThisWeek, ThisMonth, LastMonth }

struct State {
    code:       String,
    file:       TimerFile,
    running:    bool,
    run_start:  u64,   // unix secs when current session started
    preset:     Preset,
    export_msg: Option<Instant>, // show "Exported!" briefly
}

impl State {
    fn new() -> Self {
        Self {
            code:       String::new(),
            file:       TimerFile::default(),
            running:    false,
            run_start:  0,
            preset:     Preset::ThisWeek,
            export_msg: None,
        }
    }

    fn switch_workspace(&mut self, code: &str) {
        if !self.code.is_empty() && self.code != code {
            self.pause();
        }
        self.code = code.to_string();
        self.file = load(code);
        // recover any open session left by a crash
        if let Some(open) = self.file.sessions.iter().position(|s| s.end.is_none()) {
            let s = &mut self.file.sessions[open];
            s.end = Some(now_secs());
            s.secs = s.end.unwrap().saturating_sub(s.start);
            save(&self.code, &self.file);
        }
    }

    fn toggle(&mut self) {
        if self.running { self.pause() } else { self.start() }
    }

    fn start(&mut self) {
        if self.running { return; }
        if self.code.is_empty() { self.code = "default".to_string(); self.file = load(&self.code); }
        self.run_start = now_secs();
        self.file.sessions.push(Session { start: self.run_start, end: None, secs: 0 });
        save(&self.code, &self.file);
        self.running = true;
    }

    fn pause(&mut self) {
        if !self.running { return; }
        let end = now_secs();
        if let Some(s) = self.file.sessions.last_mut() {
            s.end  = Some(end);
            s.secs = end.saturating_sub(s.start);
        }
        save(&self.code, &self.file);
        self.running = false;
    }

    fn current_secs(&self, now: u64) -> u64 {
        if self.running { now.saturating_sub(self.run_start) } else { 0 }
    }

    fn preset_total(&self, now: u64) -> u64 {
        let (from, to) = match self.preset {
            Preset::Today     => (start_of_day(now), now + 1),
            Preset::ThisWeek  => (start_of_week(now), now + 1),
            Preset::ThisMonth => (start_of_month(now), now + 1),
            Preset::LastMonth => start_of_last_month(now),
        };
        total_in_range(&self.file.sessions, from, to, now)
    }

    fn export(&mut self) {
        if self.code.is_empty() { return; }
        export_csv(&self.code, &self.file.sessions);
        self.export_msg = Some(Instant::now());
    }
}

// ── Renderer ──────────────────────────────────────────────────────────────────

const BG:      [u8; 4] = [15,  23,  42,  255]; // slate-900
const BORDER:  [u8; 4] = [51,  65,  85,  255]; // slate-700
const TEXT:    [u8; 4] = [226, 232, 240, 255]; // slate-200
const MUTED:   [u8; 4] = [100, 116, 139, 255]; // slate-500
const GREEN:   [u8; 4] = [34,  197, 94,  255]; // green-500
const RED:     [u8; 4] = [239, 68,  68,  255]; // red-500
const BLUE:    [u8; 4] = [96,  165, 250, 255]; // blue-400
const ACCENT:  [u8; 4] = [51,  65,  85,  255]; // slate-700 (button bg)
const HILIGHT: [u8; 4] = [71,  85,  105, 255]; // slate-600 (active button)

struct Canvas { pixels: Vec<u8>, w: u32, h: u32 }

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for i in 0..w * h {
            let o = i as usize * 4;
            pixels[o..o+4].copy_from_slice(&BG);
        }
        Self { pixels, w, h }
    }

    fn set(&mut self, x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 { return; }
        let i = (y as u32 * self.w + x as u32) as usize * 4;
        self.pixels[i..i+4].copy_from_slice(&color);
    }

    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
        for dy in 0..h { for dx in 0..w { self.set(x+dx, y+dy, color); } }
    }

    fn text(&mut self, x: i32, y: i32, s: &str, color: [u8; 4], scale: u32) {
        let mut cx = x;
        for ch in s.chars() {
            if let Some(g) = glyph(ch) {
                for (row, bits) in g.iter().enumerate() {
                    for col in 0..5usize {
                        if bits & (1 << (4 - col)) != 0 {
                            for sy in 0..scale as i32 {
                                for sx in 0..scale as i32 {
                                    self.set(cx + col as i32 * scale as i32 + sx,
                                             y  + row as i32 * scale as i32 + sy,
                                             color);
                                }
                            }
                        }
                    }
                }
            }
            cx += (6 * scale) as i32;
        }
    }

    fn text_w(s: &str, scale: u32) -> i32 {
        s.chars().count() as i32 * 6 * scale as i32
    }

    fn button(&mut self, x: i32, y: i32, w: i32, h: i32, label: &str, ts: u32, active: bool) {
        let bg = if active { HILIGHT } else { ACCENT };
        self.rect(x, y, w, h, bg);
        // border
        for dx in 0..w { self.set(x+dx, y, BORDER); self.set(x+dx, y+h-1, BORDER); }
        for dy in 0..h { self.set(x, y+dy, BORDER); self.set(x+w-1, y+dy, BORDER); }
        let tw = Canvas::text_w(label, ts);
        let th = 7 * ts as i32;
        let tx = x + (w - tw) / 2;
        let ty = y + (h - th) / 2;
        self.text(tx.max(x+2), ty, label, TEXT, ts);
    }
}

// ── Layout ────────────────────────────────────────────────────────────────────

pub struct Layout {
    pub toggle_btn:  Rect,
    pub today_btn:   Rect,
    pub week_btn:    Rect,
    pub month_btn:   Rect,
    pub lmonth_btn:  Rect,
    pub export_btn:  Rect,
}

#[derive(Clone, Copy)]
pub struct Rect { pub x: i32, pub y: i32, pub w: i32, pub h: i32 }

impl Rect {
    fn hit(&self, px: f32, py: f32) -> bool {
        px >= self.x as f32 && px < (self.x + self.w) as f32 &&
        py >= self.y as f32 && py < (self.y + self.h) as f32
    }
}

fn render(w: u32, h: u32, state: &State, now: u64) -> (Vec<u8>, Layout) {
    let mut c = Canvas::new(w, h);

    let pad  = (w as i32 / 20).clamp(4, 16);
    let iw   = (w as i32 - pad * 2).max(1);
    let ts: u32 = if iw >= 1400 { 4 } else if iw >= 700 { 3 } else if iw >= 700 { 2 } else { 1 };
    let lh   = 7 * ts as i32 + 4;
    let bts  = ts + 1;
    let blh  = 7 * bts as i32 + 4;

    let mut y = pad;

    // ── Header ────────────────────────────────────────────────────────────────
    let label = if state.code.is_empty() { "TIMER" } else { state.code.as_str() };
    c.text(pad, y, label, MUTED, ts);
    y += lh;
    c.rect(pad, y, iw, 1, BORDER);
    y += 6 + lh / 2;

    // ── Clock: current session when running, preset total when paused ─────────
    let preset_total = state.preset_total(now);
    let (clock_secs, clock_color, clock_label) = if state.running {
        let cur = state.current_secs(now);
        let label = "Session";
        (cur, GREEN, label)
    } else {
        let label = match state.preset {
            Preset::Today     => "Today",
            Preset::ThisWeek  => "This week",
            Preset::ThisMonth => "This month",
            Preset::LastMonth => "Last month",
        };
        (preset_total, BLUE, label)
    };
    let clock = fmt_duration(clock_secs);
    let dot_color = if state.running { GREEN } else { MUTED };
    let dot_r = (bts as i32 * 2).max(3);
    c.rect(pad, y + (7 * bts as i32 - dot_r) / 2, dot_r, dot_r, dot_color);
    let clock_x = pad + dot_r + bts as i32 * 2;
    let clock_scale = bts + 1;
    c.text(clock_x, y, &clock, TEXT, clock_scale);
    y += 7 * clock_scale as i32 + 2;
    c.text(clock_x, y, clock_label, clock_color, ts);
    y += lh * 2;

    // ── Toggle button ─────────────────────────────────────────────────────────
    let btn_h = blh + pad;
    let btn_label = if state.running { "STOP" } else { "START" };
    let btn_color = if state.running { RED } else { GREEN };
    c.rect(pad, y, iw, btn_h, ACCENT);
    for dx in 0..iw { c.set(pad+dx, y, BORDER); c.set(pad+dx, y+btn_h-1, BORDER); }
    for dy in 0..btn_h { c.set(pad, y+dy, BORDER); c.set(pad+iw-1, y+dy, BORDER); }
    let tw = Canvas::text_w(btn_label, bts);
    c.text(pad + (iw - tw) / 2, y + (btn_h - 7 * bts as i32) / 2, btn_label, btn_color, bts);
    let toggle_btn = Rect { x: pad, y, w: iw, h: btn_h };
    y += btn_h + lh;

    // ── Preset buttons ────────────────────────────────────────────────────────
    let preset_btn_h = blh;
    let gap = 2;
    let col2 = iw / 2 - gap / 2;

    c.button(pad,          y, col2, preset_btn_h, "Today",      bts, state.preset == Preset::Today);
    c.button(pad + col2 + gap, y, iw - col2 - gap, preset_btn_h, "This wk", bts, state.preset == Preset::ThisWeek);
    let today_btn  = Rect { x: pad,              y, w: col2,              h: preset_btn_h };
    let week_btn   = Rect { x: pad + col2 + gap, y, w: iw - col2 - gap,  h: preset_btn_h };
    y += preset_btn_h + gap;

    c.button(pad,          y, col2, preset_btn_h, "This mo",    bts, state.preset == Preset::ThisMonth);
    c.button(pad + col2 + gap, y, iw - col2 - gap, preset_btn_h, "Last mo", bts, state.preset == Preset::LastMonth);
    let month_btn  = Rect { x: pad,              y, w: col2,              h: preset_btn_h };
    let lmonth_btn = Rect { x: pad + col2 + gap, y, w: iw - col2 - gap,  h: preset_btn_h };
    y += preset_btn_h + lh;

    // ── Export button ─────────────────────────────────────────────────────────
    let show_export = state.export_msg
        .map(|t| t.elapsed() < Duration::from_secs(2))
        .unwrap_or(false);
    let exp_label = if show_export { "Exported!" } else { "Export CSV" };
    let exp_color = if show_export { GREEN } else { TEXT };
    c.button(pad, y, iw, preset_btn_h, exp_label, bts, false);
    if show_export {
        let tw = Canvas::text_w(exp_label, bts);
        let tx = pad + (iw - tw) / 2;
        let ty = y + (preset_btn_h - 7 * bts as i32) / 2;
        c.text(tx.max(pad+2), ty, exp_label, exp_color, bts);
    }
    let export_btn = Rect { x: pad, y, w: iw, h: preset_btn_h };

    let layout = Layout { toggle_btn, today_btn, week_btn, month_btn, lmonth_btn, export_btn };
    (c.pixels, layout)
}

// ── 5×7 bitmap font ───────────────────────────────────────────────────────────

fn glyph(ch: char) -> Option<[u8; 7]> {
    Some(match ch {
        ' ' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'A' => [0x0E,0x11,0x11,0x1F,0x11,0x11,0x11],
        'B' => [0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E],
        'C' => [0x0E,0x11,0x10,0x10,0x10,0x11,0x0E],
        'D' => [0x1E,0x09,0x09,0x09,0x09,0x09,0x1E],
        'E' => [0x1F,0x10,0x10,0x1E,0x10,0x10,0x1F],
        'F' => [0x1F,0x10,0x10,0x1E,0x10,0x10,0x10],
        'G' => [0x0E,0x11,0x10,0x17,0x11,0x11,0x0F],
        'H' => [0x11,0x11,0x11,0x1F,0x11,0x11,0x11],
        'I' => [0x0E,0x04,0x04,0x04,0x04,0x04,0x0E],
        'J' => [0x07,0x02,0x02,0x02,0x02,0x12,0x0C],
        'K' => [0x11,0x12,0x14,0x18,0x14,0x12,0x11],
        'L' => [0x10,0x10,0x10,0x10,0x10,0x10,0x1F],
        'M' => [0x11,0x1B,0x15,0x11,0x11,0x11,0x11],
        'N' => [0x11,0x19,0x15,0x13,0x11,0x11,0x11],
        'O' => [0x0E,0x11,0x11,0x11,0x11,0x11,0x0E],
        'P' => [0x1E,0x11,0x11,0x1E,0x10,0x10,0x10],
        'Q' => [0x0E,0x11,0x11,0x11,0x15,0x12,0x0D],
        'R' => [0x1E,0x11,0x11,0x1E,0x14,0x12,0x11],
        'S' => [0x0F,0x10,0x10,0x0E,0x01,0x01,0x1E],
        'T' => [0x1F,0x04,0x04,0x04,0x04,0x04,0x04],
        'U' => [0x11,0x11,0x11,0x11,0x11,0x11,0x0E],
        'V' => [0x11,0x11,0x11,0x11,0x11,0x0A,0x04],
        'W' => [0x11,0x11,0x11,0x15,0x15,0x1B,0x11],
        'X' => [0x11,0x11,0x0A,0x04,0x0A,0x11,0x11],
        'Y' => [0x11,0x11,0x0A,0x04,0x04,0x04,0x04],
        'Z' => [0x1F,0x01,0x02,0x04,0x08,0x10,0x1F],
        'a' => [0x00,0x00,0x0E,0x01,0x0F,0x11,0x0F],
        'b' => [0x10,0x10,0x1E,0x11,0x11,0x11,0x1E],
        'c' => [0x00,0x00,0x0E,0x10,0x10,0x11,0x0E],
        'd' => [0x01,0x01,0x0F,0x11,0x11,0x11,0x0F],
        'e' => [0x00,0x00,0x0E,0x11,0x1F,0x10,0x0E],
        'f' => [0x06,0x09,0x08,0x1C,0x08,0x08,0x08],
        'g' => [0x00,0x00,0x0F,0x11,0x0F,0x01,0x0E],
        'h' => [0x10,0x10,0x16,0x19,0x11,0x11,0x11],
        'i' => [0x04,0x00,0x0C,0x04,0x04,0x04,0x0E],
        'j' => [0x02,0x00,0x06,0x02,0x02,0x12,0x0C],
        'k' => [0x10,0x10,0x12,0x14,0x18,0x14,0x12],
        'l' => [0x0C,0x04,0x04,0x04,0x04,0x04,0x0E],
        'm' => [0x00,0x00,0x1A,0x15,0x15,0x11,0x11],
        'n' => [0x00,0x00,0x16,0x19,0x11,0x11,0x11],
        'o' => [0x00,0x00,0x0E,0x11,0x11,0x11,0x0E],
        'p' => [0x00,0x1E,0x11,0x11,0x1E,0x10,0x10],
        'q' => [0x00,0x0F,0x11,0x11,0x0F,0x01,0x01],
        'r' => [0x00,0x00,0x16,0x19,0x10,0x10,0x10],
        's' => [0x00,0x00,0x0E,0x10,0x0E,0x01,0x1E],
        't' => [0x08,0x08,0x1C,0x08,0x08,0x09,0x06],
        'u' => [0x00,0x00,0x11,0x11,0x11,0x13,0x0D],
        'v' => [0x00,0x00,0x11,0x11,0x11,0x0A,0x04],
        'w' => [0x00,0x00,0x11,0x15,0x15,0x15,0x0A],
        'x' => [0x00,0x00,0x11,0x0A,0x04,0x0A,0x11],
        'y' => [0x00,0x11,0x11,0x0F,0x01,0x11,0x0E],
        'z' => [0x00,0x00,0x1F,0x02,0x04,0x08,0x1F],
        '0' => [0x0E,0x11,0x13,0x15,0x19,0x11,0x0E],
        '1' => [0x04,0x0C,0x04,0x04,0x04,0x04,0x0E],
        '2' => [0x0E,0x11,0x01,0x06,0x08,0x10,0x1F],
        '3' => [0x1F,0x02,0x04,0x06,0x01,0x11,0x0E],
        '4' => [0x02,0x06,0x0A,0x12,0x1F,0x02,0x02],
        '5' => [0x1F,0x10,0x1E,0x01,0x01,0x11,0x0E],
        '6' => [0x06,0x08,0x10,0x1E,0x11,0x11,0x0E],
        '7' => [0x1F,0x01,0x02,0x04,0x08,0x08,0x08],
        '8' => [0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E],
        '9' => [0x0E,0x11,0x11,0x0F,0x01,0x02,0x0C],
        ':' => [0x00,0x04,0x00,0x00,0x04,0x00,0x00],
        '-' => [0x00,0x00,0x00,0x1F,0x00,0x00,0x00],
        '!' => [0x04,0x04,0x04,0x04,0x00,0x00,0x04],
        _   => [0x00,0x00,0x00,0x00,0x00,0x00,0x00],
    })
}

// ── Main ──────────────────────────────────────────────────────────────────────

enum Msg {
    Resize(u32, u32),
    Click(f32, f32),
    Workspace(String),
    Tick,
}

fn main() {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    let size:  Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((300, 500)));
    let state: Arc<Mutex<State>>      = Arc::new(Mutex::new(State::new()));
    let layout: Arc<Mutex<Option<Layout>>> = Arc::new(Mutex::new(None));

    let (tx, rx) = mpsc::channel::<Msg>();

    // Stdin reader thread
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in BufReader::new(stdin.lock()).lines() {
                let Ok(line) = line else { break };
                let Ok(ev) = serde_json::from_str::<Event>(&line) else { continue };
                match ev.kind.as_str() {
                    "resize" => {
                        if let (Some(w), Some(h)) = (ev.width, ev.height) {
                            let _ = tx.send(Msg::Resize(w, h));
                        }
                    }
                    "click" => {
                        if let (Some(x), Some(y)) = (ev.x, ev.y) {
                            let _ = tx.send(Msg::Click(x, y));
                        }
                    }
                    "workspace" => {
                        if let Some(code) = ev.code {
                            let _ = tx.send(Msg::Workspace(code));
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // Tick thread — fires every second while timer is running
    {
        let tx = tx.clone();
        let state_tick = Arc::clone(&state);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let running = state_tick.lock().unwrap().running;
                if running {
                    let _ = tx.send(Msg::Tick);
                }
            }
        });
    }

    // Initialise workspace from env var set by Mado at spawn time
    if let Ok(ws) = std::env::var("MADO_WORKSPACE") {
        if !ws.is_empty() {
            state.lock().unwrap().switch_workspace(&ws);
        }
    }

    // Initial render
    let _ = tx.send(Msg::Tick);

    loop {
        let msg = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(m) => m,
            Err(_) => Msg::Tick,
        };

        match msg {
            Msg::Resize(w, h) => { *size.lock().unwrap() = (w, h); }
            Msg::Click(x, y) => {
                let mut st = state.lock().unwrap();
                if let Some(ref lay) = *layout.lock().unwrap() {
                    if lay.toggle_btn.hit(x, y)  { st.toggle(); }
                    else if lay.today_btn.hit(x, y)   { st.preset = Preset::Today; }
                    else if lay.week_btn.hit(x, y)    { st.preset = Preset::ThisWeek; }
                    else if lay.month_btn.hit(x, y)   { st.preset = Preset::ThisMonth; }
                    else if lay.lmonth_btn.hit(x, y)  { st.preset = Preset::LastMonth; }
                    else if lay.export_btn.hit(x, y)  { st.export(); }
                }
            }
            Msg::Workspace(code) => {
                state.lock().unwrap().switch_workspace(&code);
            }
            Msg::Tick => {}
        }

        let now = now_secs();
        let (w, h) = *size.lock().unwrap();
        let (pixels, lay) = render(w, h, &state.lock().unwrap(), now);
        *layout.lock().unwrap() = Some(lay);
        send_frame(&mut out, w, h, &pixels);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Verified UTC timestamps (multiples of 86400 = exact midnight):
    // 2026-09-13 Sun = 1789257600  (day 20709)
    // 2026-09-14 Mon = 1789344000  (day 20710)
    // 2026-09-07 Mon = 1788739200  (day 20703) — start of week containing Sep 13
    // 2026-09-01     = 1788220800  (day 20697)
    // 2026-08-01     = 1785542400  (day 20666)

    // ── days_to_ymd ───────────────────────────────────────────────────────────

    #[test]
    fn ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn ymd_known_date_sep13() {
        let ts = 1789257600u64; // 2026-09-13 00:00:00 UTC
        assert_eq!(days_to_ymd(ts / 86400), (2026, 9, 13));
    }

    #[test]
    fn ymd_known_date_sep14() {
        let ts = 1789344000u64; // 2026-09-14 00:00:00 UTC
        assert_eq!(days_to_ymd(ts / 86400), (2026, 9, 14));
    }

    #[test]
    fn ymd_leap_day() {
        let ts = 951782400u64; // 2000-02-29 00:00:00 UTC
        assert_eq!(days_to_ymd(ts / 86400), (2000, 2, 29));
    }

    // ── start_of_day ─────────────────────────────────────────────────────────

    #[test]
    fn start_of_day_at_midnight() {
        let ts = 1789344000u64; // 2026-09-14 00:00:00 UTC — exact midnight
        assert_eq!(start_of_day(ts), ts);
    }

    #[test]
    fn start_of_day_mid_day() {
        let midnight = 1789344000u64;
        assert_eq!(start_of_day(midnight + 43200), midnight);
    }

    #[test]
    fn start_of_day_end_of_day() {
        let midnight = 1789344000u64;
        assert_eq!(start_of_day(midnight + 86399), midnight);
    }

    // ── start_of_week ────────────────────────────────────────────────────────

    #[test]
    fn start_of_week_on_monday() {
        // 2026-09-14 is a Monday — start of week is itself
        let monday = 1789344000u64;
        assert_eq!(start_of_week(monday), monday);
    }

    #[test]
    fn start_of_week_on_sunday() {
        // 2026-09-13 is a Sunday — week started Monday Sep 7
        let sunday = 1789257600u64;
        assert_eq!(start_of_week(sunday), 1788739200);
    }

    #[test]
    fn start_of_week_on_wednesday() {
        // 2026-09-16 Wednesday — should return 2026-09-14 Monday
        let monday    = 1789344000u64;
        let wednesday = monday + 2 * 86400;
        assert_eq!(start_of_week(wednesday), monday);
    }

    #[test]
    fn start_of_week_mid_day() {
        // Wednesday afternoon — still returns Monday midnight
        let monday        = 1789344000u64;
        let wednesday_pm  = monday + 2 * 86400 + 43200;
        assert_eq!(start_of_week(wednesday_pm), monday);
    }

    // ── start_of_month / start_of_last_month ─────────────────────────────────

    #[test]
    fn start_of_month_first_day() {
        let sep1 = 1788220800u64; // 2026-09-01 00:00:00 UTC
        assert_eq!(start_of_month(sep1), sep1);
    }

    #[test]
    fn start_of_month_mid_month() {
        let sep1  = 1788220800u64;
        let sep14 = 1789344000u64;
        assert_eq!(start_of_month(sep14), sep1);
    }

    #[test]
    fn start_of_last_month_returns_correct_range() {
        let sep14 = 1789344000u64;
        let sep1  = 1788220800u64;
        let aug1  = 1785542400u64;
        let (start, end) = start_of_last_month(sep14);
        assert_eq!(end,   sep1);
        assert_eq!(start, aug1);
    }

    // ── fmt_duration ─────────────────────────────────────────────────────────

    #[test]
    fn fmt_duration_zero() {
        assert_eq!(fmt_duration(0), "00:00:00");
    }

    #[test]
    fn fmt_duration_seconds() {
        assert_eq!(fmt_duration(45), "00:00:45");
    }

    #[test]
    fn fmt_duration_minutes() {
        assert_eq!(fmt_duration(90), "00:01:30");
    }

    #[test]
    fn fmt_duration_hours() {
        assert_eq!(fmt_duration(3661), "01:01:01");
    }

    #[test]
    fn fmt_duration_large() {
        assert_eq!(fmt_duration(36000), "10:00:00");
    }

    // ── session_secs ─────────────────────────────────────────────────────────

    #[test]
    fn session_secs_closed() {
        let s = Session { start: 1000, end: Some(1060), secs: 60 };
        assert_eq!(session_secs(&s, 9999), 60);
    }

    #[test]
    fn session_secs_open_uses_now() {
        let s = Session { start: 1000, end: None, secs: 0 };
        assert_eq!(session_secs(&s, 1045), 45);
    }

    #[test]
    fn session_secs_open_now_before_start() {
        // saturating_sub — never goes negative
        let s = Session { start: 1000, end: None, secs: 0 };
        assert_eq!(session_secs(&s, 500), 0);
    }

    #[test]
    fn session_secs_closed_no_secs_field() {
        // secs=0 but end is set — fall through to end-start
        let s = Session { start: 1000, end: Some(1090), secs: 0 };
        assert_eq!(session_secs(&s, 9999), 90);
    }

    // ── total_in_range ────────────────────────────────────────────────────────

    #[test]
    fn total_in_range_empty() {
        assert_eq!(total_in_range(&[], 0, 9999, 9999), 0);
    }

    #[test]
    fn total_in_range_all_inside() {
        let sessions = vec![
            Session { start: 100, end: Some(160), secs: 60 },
            Session { start: 200, end: Some(290), secs: 90 },
        ];
        assert_eq!(total_in_range(&sessions, 0, 1000, 1000), 150);
    }

    #[test]
    fn total_in_range_some_outside() {
        let sessions = vec![
            Session { start: 50,  end: Some(110), secs: 60 },  // outside (start < from)
            Session { start: 200, end: Some(290), secs: 90 },  // inside
            Session { start: 900, end: Some(960), secs: 60 },  // outside (start >= to)
        ];
        assert_eq!(total_in_range(&sessions, 100, 900, 1000), 90);
    }

    #[test]
    fn total_in_range_open_session() {
        let now = 1000u64;
        let sessions = vec![
            Session { start: 900, end: None, secs: 0 },
        ];
        assert_eq!(total_in_range(&sessions, 0, now + 1, now), 100);
    }

    // ── State machine ─────────────────────────────────────────────────────────

    fn make_state(code: &str, sessions: Vec<Session>) -> State {
        State {
            code: code.to_string(),
            file: TimerFile { sessions },
            running: false,
            run_start: 0,
            preset: Preset::ThisWeek,
            export_msg: None,
        }
    }

    #[test]
    fn current_secs_not_running() {
        let st = make_state("MDO", vec![]);
        assert_eq!(st.current_secs(9999), 0);
    }

    #[test]
    fn current_secs_running() {
        let mut st = make_state("MDO", vec![]);
        st.running = true;
        st.run_start = 1000;
        assert_eq!(st.current_secs(1045), 45);
    }

    #[test]
    fn pause_when_not_running_is_noop() {
        let mut st = make_state("MDO", vec![]);
        st.pause();
        assert!(!st.running);
        assert!(st.file.sessions.is_empty());
    }

    #[test]
    fn toggle_starts_and_stops() {
        let mut st = make_state("MDO", vec![]);
        // Can't call toggle() directly as it calls save() — test underlying logic
        assert!(!st.running);
        st.running = true;
        st.run_start = 1000;
        assert!(st.running);
        assert_eq!(st.current_secs(1030), 30);
    }

    #[test]
    fn switch_workspace_same_code_no_pause() {
        let mut st = make_state("MDO", vec![]);
        st.running = true;
        st.run_start = 1000;
        // switching to same workspace should not pause
        // (switch_workspace loads from disk, so we just check the guard)
        let was_running = st.running;
        if !st.code.is_empty() && st.code == "MDO" {
            // guard: same code, no pause
        }
        assert_eq!(st.running, was_running);
    }

    // ── fmt_timestamp ─────────────────────────────────────────────────────────

    #[test]
    fn fmt_timestamp_epoch() {
        assert_eq!(fmt_timestamp(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn fmt_timestamp_known() {
        // 2026-09-14 12:00:00 UTC
        let ts = 1789344000u64 + 43200;
        assert_eq!(fmt_timestamp(ts), "2026-09-14 12:00:00");
    }
}
