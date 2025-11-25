use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table},
    Frame, Terminal,
};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};

mod hardware;
mod sysinfo_ext;
mod file_cache;

use hardware::*;
use sysinfo_ext::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProcessSortMode {
    CpuAsc,
    CpuDesc,
    MemoryAsc,
    MemoryDesc,
    PidAsc,
    PidDesc,
    NameAsc,
    NameDesc,
}

/// Configurable refresh rates (in seconds)
#[derive(Debug, Clone)]
struct RefreshConfig {
    cpu_memory: u64,    // CPU and memory refresh
    network_disk: u64,  // Network and disk I/O
    processes: u64,     // Process list
    #[allow(dead_code)]
    stats: u64,         // Governor, TCP connections, etc. (currently unused, for future use)
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            cpu_memory: 1,
            network_disk: 2,
            processes: 2,     // Keep responsive at 2 seconds
            stats: 5,
        }
    }
}

fn main() -> Result<()> {
    // Check for root permissions
    if !nix::unistd::geteuid().is_root() {
        eprintln!("Root permissions required. Use: sudo rktop");
        std::process::exit(1);
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Initialize system info
    let mut sys = System::new_all();
    let mut networks = Networks::new_with_refreshed_list();
    let mut disks = Disks::new_with_refreshed_list();

    let mut app_state = AppState::new();
    let mut last_update = Instant::now();
    let mut last_process_update = Instant::now();
    let mut last_network_update = Instant::now();

    let result = run_app(&mut terminal, &mut sys, &mut networks, &mut disks, &mut app_state, &mut last_update, &mut last_process_update, &mut last_network_update);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

struct AppState {
    prev_disk_read: u64,
    prev_disk_write: u64,
    prev_net_rx: u64,
    prev_net_tx: u64,
    prev_time: Instant,
    disk_read_rate: f64,
    disk_write_rate: f64,
    net_rx_rate: f64,
    net_tx_rate: f64,
    // Per-adapter network stats
    prev_adapter_stats: HashMap<String, (u64, u64)>, // (rx, tx)
    adapter_rates: HashMap<String, (f64, f64)>,      // (rx_rate, tx_rate)
    // Cached static info
    board_name: String,
    rk_model: String,
    cpu_arch: String,
    npu_version: String,
    rga_version: String,
    rknn_version: String,
    rkllm_version: String,
    // Cached hardware availability
    has_gpu: bool,
    has_npu: bool,
    has_rga: bool,
    // Cached CPU frequency ranges (don't change at runtime)
    cpu_freq_ranges: Vec<(u32, u32)>,
    // Cached network adapters (to avoid scanning /sys/class/net repeatedly)
    network_adapters: Vec<String>,
    // Cached thermal zone paths (to avoid directory scanning)
    thermal_zone_paths: Vec<(String, String, String)>, // (label, temp_path, type_path)
    // Cached stats (updated periodically, not every frame)
    cpu_governor: String,
    tcp_connections: usize,
    last_stats_update: Instant,
    // Process sorting
    process_sort_mode: ProcessSortMode,
    // CPU stats tracking (for calculating rates)
    prev_ctx_switches: u64,
    prev_interrupts: u64,
    prev_softirqs: u64,
    ctx_switches_rate: u64,
    interrupts_rate: u64,
    softirqs_rate: u64,
    prev_cpu_stats_time: Instant,
    // CPU time breakdown (percentages)
    cpu_user_pct: f64,
    cpu_system_pct: f64,
    cpu_iowait_pct: f64,
    cpu_idle_pct: f64,
    prev_cpu_time: CpuStats,
    // Process counts
    running_procs: u64,
    blocked_procs: u64,
    // Historical data for sparklines (last 60 samples at 1 second each)
    cpu_history: VecDeque<f32>,
    gpu_history: VecDeque<f32>,
    npu_history: VecDeque<f32>,
    // Process filtering
    filter_text: String,
    filter_mode: bool, // true when actively editing filter
}

impl AppState {
    fn new() -> Self {
        // Cache all static system info at startup (expensive operations)
        let board_name = get_board_name();
        let rk_model = get_rk_model();
        let cpu_arch = get_cpu_architecture();
        let npu_version = get_npu_driver_version();
        let rga_version = get_rga_version();
        let rknn_version = get_librknnrt_version();
        let rkllm_version = get_librkllmrt_version();

        // Cache hardware availability (check once at startup)
        let has_gpu = get_gpu_usage().is_some();
        let has_npu = !get_npu_load().is_empty();
        let has_rga = get_rga_load().is_some();

        // Cache CPU frequency ranges (don't change at runtime)
        let cpu_freq_ranges = get_cpu_freq_ranges();

        // Cache network adapters and thermal zones (avoid repeated directory scans)
        let network_adapters = get_network_adapters();
        let thermal_zone_paths = get_thermal_zone_paths();

        Self {
            prev_disk_read: 0,
            prev_disk_write: 0,
            prev_net_rx: 0,
            prev_net_tx: 0,
            prev_time: Instant::now(),
            disk_read_rate: 0.0,
            disk_write_rate: 0.0,
            net_rx_rate: 0.0,
            net_tx_rate: 0.0,
            prev_adapter_stats: HashMap::new(),
            adapter_rates: HashMap::new(),
            board_name,
            rk_model,
            cpu_arch,
            npu_version,
            rga_version,
            rknn_version,
            rkllm_version,
            has_gpu,
            has_npu,
            has_rga,
            cpu_freq_ranges,
            network_adapters,
            thermal_zone_paths,
            cpu_governor: String::new(),
            tcp_connections: 0,
            last_stats_update: Instant::now(),
            process_sort_mode: ProcessSortMode::CpuDesc,
            prev_ctx_switches: 0,
            prev_interrupts: 0,
            prev_softirqs: 0,
            ctx_switches_rate: 0,
            interrupts_rate: 0,
            softirqs_rate: 0,
            prev_cpu_stats_time: Instant::now(),
            cpu_user_pct: 0.0,
            cpu_system_pct: 0.0,
            cpu_iowait_pct: 0.0,
            cpu_idle_pct: 0.0,
            prev_cpu_time: CpuStats::default(),
            running_procs: 0,
            blocked_procs: 0,
            cpu_history: VecDeque::new(),
            gpu_history: VecDeque::new(),
            npu_history: VecDeque::new(),
            filter_text: String::new(),
            filter_mode: false,
        }
    }

    fn update_history(&mut self, total_cpu: f32) {
        const MAX_HISTORY: usize = 60; // Keep 60 seconds of history

        // Update CPU history
        self.cpu_history.push_back(total_cpu);
        if self.cpu_history.len() > MAX_HISTORY {
            self.cpu_history.pop_front();
        }

        // Update GPU history
        if let Some(gpu_usage) = get_gpu_usage() {
            self.gpu_history.push_back(gpu_usage);
            if self.gpu_history.len() > MAX_HISTORY {
                self.gpu_history.pop_front();
            }
        }

        // Update NPU history (average across cores)
        let npu_loads = get_npu_load();
        if !npu_loads.is_empty() {
            let avg_npu: f32 = npu_loads.iter().map(|&x| x as f32).sum::<f32>() / npu_loads.len() as f32;
            self.npu_history.push_back(avg_npu);
            if self.npu_history.len() > MAX_HISTORY {
                self.npu_history.pop_front();
            }
        }
    }

    fn update_cpu_stats(&mut self) {
        // Update CPU stats every second for smooth rates
        let cpu_stats = get_cpu_stats();
        let elapsed = self.prev_cpu_stats_time.elapsed().as_secs_f64();

        if elapsed >= 1.0 {
            // Calculate rates per second
            self.ctx_switches_rate = ((cpu_stats.context_switches - self.prev_ctx_switches) as f64 / elapsed) as u64;
            self.interrupts_rate = ((cpu_stats.interrupts - self.prev_interrupts) as f64 / elapsed) as u64;
            self.softirqs_rate = ((cpu_stats.softirqs - self.prev_softirqs) as f64 / elapsed) as u64;

            // Calculate CPU time percentages
            let user_delta = cpu_stats.user - self.prev_cpu_time.user;
            let nice_delta = cpu_stats.nice - self.prev_cpu_time.nice;
            let system_delta = cpu_stats.system - self.prev_cpu_time.system;
            let idle_delta = cpu_stats.idle - self.prev_cpu_time.idle;
            let iowait_delta = cpu_stats.iowait - self.prev_cpu_time.iowait;
            let irq_delta = cpu_stats.irq - self.prev_cpu_time.irq;
            let softirq_delta = cpu_stats.softirq - self.prev_cpu_time.softirq;

            let total_delta = user_delta + nice_delta + system_delta + idle_delta + iowait_delta + irq_delta + softirq_delta;

            if total_delta > 0 {
                self.cpu_user_pct = ((user_delta + nice_delta) as f64 / total_delta as f64) * 100.0;
                self.cpu_system_pct = ((system_delta + irq_delta + softirq_delta) as f64 / total_delta as f64) * 100.0;
                self.cpu_iowait_pct = (iowait_delta as f64 / total_delta as f64) * 100.0;
                self.cpu_idle_pct = (idle_delta as f64 / total_delta as f64) * 100.0;
            }

            // Update process counts
            self.running_procs = cpu_stats.running_procs;
            self.blocked_procs = cpu_stats.blocked_procs;

            // Update previous values
            self.prev_ctx_switches = cpu_stats.context_switches;
            self.prev_interrupts = cpu_stats.interrupts;
            self.prev_softirqs = cpu_stats.softirqs;
            self.prev_cpu_time = cpu_stats;
            self.prev_cpu_stats_time = Instant::now();
        }
    }

    fn update_stats(&mut self) {
        // Update stats every 5 seconds instead of every frame
        if self.last_stats_update.elapsed() < Duration::from_secs(5) {
            return;
        }

        // CPU governor
        self.cpu_governor = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
            .unwrap_or_else(|_| "N/A".to_string())
            .trim()
            .to_string();

        // Active TCP connections (count external established connections only, IPv4 + IPv6)
        let mut count = 0;

        // Count IPv4 connections
        if let Ok(content) = std::fs::read_to_string("/proc/net/tcp") {
            count += content
                .lines()
                .skip(1)
                .filter(|line| {
                    if !line.contains(" 01 ") {
                        return false;
                    }
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() > 1 {
                        let local_addr = parts[1];
                        !local_addr.starts_with("0100007F") && !local_addr.starts_with("00000000")
                    } else {
                        false
                    }
                })
                .count();
        }

        // Count IPv6 connections
        if let Ok(content) = std::fs::read_to_string("/proc/net/tcp6") {
            count += content
                .lines()
                .skip(1)
                .filter(|line| {
                    if !line.contains(" 01 ") {
                        return false;
                    }
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() > 1 {
                        let local_addr = parts[1];
                        !local_addr.starts_with("00000000000000000000000001000000")
                            && !local_addr.starts_with("00000000000000000000000000000000")
                    } else {
                        false
                    }
                })
                .count();
        }

        self.tcp_connections = count;
        self.last_stats_update = Instant::now();
    }

    fn update_io_rates(&mut self, _disks: &Disks, networks: &Networks) {
        let now = Instant::now();
        let interval = now.duration_since(self.prev_time).as_secs_f64();

        // Calculate disk I/O
        let total_read = 0u64;
        let total_write = 0u64;
        // Note: sysinfo doesn't provide per-disk I/O stats on all platforms
        // This is a simplified version - would need platform-specific code

        // Calculate network I/O (total and per-adapter)
        let mut total_rx = 0u64;
        let mut total_tx = 0u64;
        let mut new_adapter_stats = HashMap::new();

        for (name, data) in networks.list() {
            // Skip interfaces not in our cached adapter list (filters out virtual interfaces)
            if !self.network_adapters.is_empty() && !self.network_adapters.contains(&name.to_string()) {
                continue;
            }

            let rx = data.total_received();
            let tx = data.total_transmitted();
            total_rx += rx;
            total_tx += tx;

            // Calculate per-adapter rates
            if interval > 0.0 {
                if let Some(&(prev_rx, prev_tx)) = self.prev_adapter_stats.get(name) {
                    let rx_rate = (rx.saturating_sub(prev_rx)) as f64 / interval;
                    let tx_rate = (tx.saturating_sub(prev_tx)) as f64 / interval;
                    self.adapter_rates.insert(name.to_string(), (rx_rate, tx_rate));
                }
            }

            new_adapter_stats.insert(name.to_string(), (rx, tx));
        }

        if interval > 0.0 {
            self.disk_read_rate = (total_read.saturating_sub(self.prev_disk_read)) as f64 / interval;
            self.disk_write_rate = (total_write.saturating_sub(self.prev_disk_write)) as f64 / interval;
            self.net_rx_rate = (total_rx.saturating_sub(self.prev_net_rx)) as f64 / interval;
            self.net_tx_rate = (total_tx.saturating_sub(self.prev_net_tx)) as f64 / interval;
        }

        self.prev_disk_read = total_read;
        self.prev_disk_write = total_write;
        self.prev_net_rx = total_rx;
        self.prev_net_tx = total_tx;
        self.prev_adapter_stats = new_adapter_stats;
        self.prev_time = now;
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    sys: &mut System,
    networks: &mut Networks,
    disks: &mut Disks,
    app_state: &mut AppState,
    last_update: &mut Instant,
    last_process_update: &mut Instant,
    last_network_update: &mut Instant,
) -> Result<()> {
    let mut last_render = Instant::now();
    let config = RefreshConfig::default();

    loop {
        // Update system info based on configured rate
        let mut should_render = false;
        if last_update.elapsed() >= Duration::from_secs(config.cpu_memory) {
            sys.refresh_cpu_all();
            sys.refresh_memory();
            app_state.update_cpu_stats(); // Update CPU stats (context switches, interrupts)
            app_state.update_stats(); // Update governor and TCP connections (throttled internally)

            // Calculate total CPU usage for history (100% - idle%)
            let total_cpu = 100.0 - app_state.cpu_idle_pct as f32;
            app_state.update_history(total_cpu);

            *last_update = Instant::now();
            should_render = true; // Force render after data update
        }

        // Update network stats based on configured rate
        if last_network_update.elapsed() >= Duration::from_secs(config.network_disk) {
            networks.refresh();
            disks.refresh();
            app_state.update_io_rates(disks, networks);
            *last_network_update = Instant::now();
        }

        // Update processes based on configured rate
        // Refresh with shallow read to limit syscalls, but remove dead processes
        if last_process_update.elapsed() >= Duration::from_secs(config.processes) {
            // Remove processes that no longer exist
            sys.refresh_processes(ProcessesToUpdate::All, true);
            *last_process_update = Instant::now();
        }

        // Render at most once per second, or immediately after data update
        if should_render || last_render.elapsed() >= Duration::from_secs(1) {
            terminal.draw(|f| ui(f, sys, app_state))?;
            last_render = Instant::now();
        }

        // Handle input with timeout - sleep most of the time to reduce CPU
        // Poll every 250ms instead of 100ms to reduce wake-ups
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // Handle filter mode input separately
                    if app_state.filter_mode {
                        match key.code {
                            KeyCode::Char(c) => {
                                app_state.filter_text.push(c);
                            }
                            KeyCode::Backspace => {
                                app_state.filter_text.pop();
                            }
                            KeyCode::Esc => {
                                app_state.filter_mode = false;
                                app_state.filter_text.clear();
                            }
                            KeyCode::Enter => {
                                app_state.filter_mode = false;
                            }
                            _ => {}
                        }
                    } else {
                        // Normal mode keyboard shortcuts
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                return Ok(());
                            }
                            KeyCode::Char('/') => {
                                app_state.filter_mode = true;
                                app_state.filter_text.clear();
                            }
                            KeyCode::Esc => {
                                // Clear filter when ESC pressed in normal mode
                                app_state.filter_text.clear();
                            }
                            KeyCode::Char('c') | KeyCode::Char('C') => {
                                app_state.process_sort_mode = match app_state.process_sort_mode {
                                    ProcessSortMode::CpuDesc => ProcessSortMode::CpuAsc,
                                    ProcessSortMode::CpuAsc => ProcessSortMode::CpuDesc,
                                    _ => ProcessSortMode::CpuDesc,
                                };
                            }
                            KeyCode::Char('m') | KeyCode::Char('M') => {
                                app_state.process_sort_mode = match app_state.process_sort_mode {
                                    ProcessSortMode::MemoryDesc => ProcessSortMode::MemoryAsc,
                                    ProcessSortMode::MemoryAsc => ProcessSortMode::MemoryDesc,
                                    _ => ProcessSortMode::MemoryDesc,
                                };
                            }
                            KeyCode::Char('p') | KeyCode::Char('P') => {
                                app_state.process_sort_mode = match app_state.process_sort_mode {
                                    ProcessSortMode::PidAsc => ProcessSortMode::PidDesc,
                                    ProcessSortMode::PidDesc => ProcessSortMode::PidAsc,
                                    _ => ProcessSortMode::PidAsc,
                                };
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                app_state.process_sort_mode = match app_state.process_sort_mode {
                                    ProcessSortMode::NameAsc => ProcessSortMode::NameDesc,
                                    ProcessSortMode::NameDesc => ProcessSortMode::NameAsc,
                                    _ => ProcessSortMode::NameAsc,
                                };
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

fn ui(f: &mut Frame, sys: &System, app_state: &AppState) {
    let size = f.area();

    // Main layout: top and bottom
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(20), Constraint::Percentage(50)])
        .split(size);

    // Top layout: left and right
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_chunks[0]);

    // Bottom layout: left and right
    let bottom_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(20), Constraint::Percentage(80)])
        .split(main_chunks[1]);

    // Left top: CPU and Memory
    let left_top_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(8)])
        .split(top_chunks[0]);

    render_cpu_panel(f, left_top_chunks[0], sys, app_state);
    render_memory_panel(f, left_top_chunks[1], sys);

    // Right top: System info and accelerators
    render_right_panels(f, top_chunks[1], app_state, sys);

    // Bottom left: I/O and Temperature
    let bottom_left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(bottom_chunks[0]);

    render_io_panel(f, bottom_left_chunks[0], app_state);
    render_temperature_panel(f, bottom_left_chunks[1], app_state);

    // Bottom right: Process table and help text
    let process_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(2)])
        .split(bottom_chunks[1]);

    render_process_panel(f, process_chunks[0], sys, app_state);
    render_help_text(f, process_chunks[1], app_state);
}

/// Render a sparkline from historical data
/// Uses block characters to create a mini-chart: ▁▂▃▄▅▆▇█
fn render_sparkline(data: &VecDeque<f32>, max_value: f32) -> String {
    if data.is_empty() {
        return String::new();
    }

    let blocks = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    data.iter()
        .map(|&value| {
            let normalized = if max_value > 0.0 {
                (value / max_value).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let index = ((normalized * (blocks.len() - 1) as f32).round() as usize).min(blocks.len() - 1);
            blocks[index]
        })
        .collect()
}

fn render_cpu_panel(f: &mut Frame, area: Rect, sys: &System, app_state: &AppState) {
    let cpus = sys.cpus();
    let cpu_freqs = get_cpu_frequencies();

    // Calculate total CPU usage across all cores
    let total_cpu_usage: f32 = cpus.iter().map(|cpu| cpu.cpu_usage()).sum::<f32>() / cpus.len() as f32;

    let mut cpu_info: Vec<Line> = cpus
        .iter()
        .enumerate()
        .map(|(i, cpu)| {
            let usage = cpu.cpu_usage();
            let freq = cpu_freqs.get(i).copied().unwrap_or(0);
            let bar_width = 35;
            let filled = ((usage / 100.0) * bar_width as f32) as usize;
            let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);
            Line::from(vec![
                Span::raw(format!("CPU {} ", i)),
                Span::styled(bar, Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {:>3.0}% {:>4} MHz", usage, freq)),
            ])
        })
        .collect();

    // Add total CPU usage line with sparkline
    cpu_info.insert(0, Line::from(format!("Total CPU: {:.1}%", total_cpu_usage)));

    // Add sparkline if we have history
    if !app_state.cpu_history.is_empty() {
        let sparkline = render_sparkline(&app_state.cpu_history, 100.0);
        cpu_info.insert(1, Line::from(vec![
            Span::raw("History:   "),
            Span::styled(sparkline, Style::default().fg(Color::Cyan)),
        ]));
    }

    // Add blank line
    cpu_info.push(Line::from(""));

    // CPU time breakdown
    cpu_info.push(Line::from(format!(
        "User: {:.1}%  System: {:.1}%  IOWait: {:.1}%  Idle: {:.1}%",
        app_state.cpu_user_pct,
        app_state.cpu_system_pct,
        app_state.cpu_iowait_pct,
        app_state.cpu_idle_pct
    )));

    // Frequency ranges (show all clusters) - use cached value
    if !app_state.cpu_freq_ranges.is_empty() {
        let ranges_str: Vec<String> = app_state.cpu_freq_ranges
            .iter()
            .map(|(min, max)| format!("{}-{} MHz", min, max))
            .collect();
        cpu_info.push(Line::from(format!("Freq ranges: {}", ranges_str.join(", "))));
    }

    // Add blank line
    cpu_info.push(Line::from(""));

    // Process counts
    cpu_info.push(Line::from(format!(
        "Running: {}  Blocked: {}",
        app_state.running_procs,
        app_state.blocked_procs
    )));

    // Interrupt stats
    cpu_info.push(Line::from(format!("Ctx switches: {}/s", format_number(app_state.ctx_switches_rate))));
    cpu_info.push(Line::from(format!("Interrupts:   {}/s", format_number(app_state.interrupts_rate))));
    cpu_info.push(Line::from(format!("Softirqs:     {}/s", format_number(app_state.softirqs_rate))));

    let block = Block::default()
        .title("CPU")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(cpu_info).block(block);
    f.render_widget(paragraph, area);
}

fn render_memory_panel(f: &mut Frame, area: Rect, sys: &System) {
    let total = sys.total_memory();
    let used = sys.used_memory();
    let available = sys.available_memory();
    let swap_total = sys.total_swap();
    let swap_used = sys.used_swap();

    let ram_percent = if total > 0 {
        (used as f64 / total as f64 * 100.0) as u16
    } else {
        0
    };

    let swap_percent = if swap_total > 0 {
        (swap_used as f64 / swap_total as f64 * 100.0) as u16
    } else {
        0
    };

    let zram_info = get_zram_info();
    let zram_percent = if let Some(ref info) = zram_info {
        if info.limit > 0 {
            (info.used as f64 / info.limit as f64 * 100.0) as u16
        } else {
            0
        }
    } else {
        0
    };

    let lines = vec![
        Line::from(vec![
            Span::raw("Total: "),
            Span::styled(human_bytes(total), Style::default().fg(Color::White)),
            Span::raw("  Free: "),
            Span::styled(human_bytes(available), Style::default().fg(Color::White)),
            Span::raw("  Used: "),
            Span::styled(human_bytes(used), Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("RAM  "),
            Span::styled(
                format!("{:█<20}", "█".repeat((ram_percent / 5) as usize)),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!(
                " {:>3}% | {} / {}",
                ram_percent,
                human_bytes(used),
                human_bytes(total)
            )),
        ]),
        Line::from(vec![
            Span::raw("Swap "),
            Span::styled(
                format!("{:█<20}", "█".repeat((swap_percent / 5) as usize)),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!(
                " {:>3}% | {} / {}",
                swap_percent,
                human_bytes(swap_used),
                human_bytes(swap_total)
            )),
        ]),
        Line::from(vec![
            Span::raw("ZRAM "),
            Span::styled(
                format!("{:█<20}", "█".repeat((zram_percent / 5) as usize)),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(if let Some(ref info) = zram_info {
                let ratio = info.compression_ratio();
                format!(
                    " {:>3}% | {} / {} ({}x)",
                    zram_percent,
                    human_bytes(info.used),
                    human_bytes(info.limit),
                    if ratio > 0.0 { format!("{:.1}", ratio) } else { "N/A".to_string() }
                )
            } else {
                " N/A".to_string()
            }),
        ]),
    ];

    let block = Block::default()
        .title("Memory")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_right_panels(f: &mut Frame, area: Rect, app_state: &AppState, sys: &System) {
    let mut constraints = vec![Constraint::Length(9)]; // System info

    // Use cached hardware availability instead of checking every frame
    if app_state.has_gpu {
        constraints.push(Constraint::Length(6)); // Increased for sparkline
    }
    if app_state.has_npu {
        constraints.push(Constraint::Length(7)); // Increased for sparkline
    }
    if app_state.has_rga {
        constraints.push(Constraint::Length(5));
    }

    // Add stats panel
    constraints.push(Constraint::Length(8));
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut chunk_idx = 0;

    // System info
    render_system_panel(f, chunks[chunk_idx], app_state);
    chunk_idx += 1;

    // GPU
    if app_state.has_gpu {
        render_gpu_panel(f, chunks[chunk_idx], app_state);
        chunk_idx += 1;
    }

    // NPU
    if app_state.has_npu {
        render_npu_panel(f, chunks[chunk_idx], app_state);
        chunk_idx += 1;
    }

    // RGA
    if app_state.has_rga {
        render_rga_panel(f, chunks[chunk_idx]);
        chunk_idx += 1;
    }

    // Stats panel
    render_stats_panel(f, chunks[chunk_idx], sys, app_state);
}

fn render_system_panel(f: &mut Frame, area: Rect, app_state: &AppState) {
    // Use cached values instead of calling expensive functions every frame

    // Read hostname and kernel for right column
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|k| k.trim().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    // Build table with two columns
    let row_data = vec![
        (format!("Board: {}", app_state.board_name), format!("Host: {}", hostname)),
        (format!("SoC: {}", app_state.rk_model), format!("Kernel: {}", kernel)),
        (format!("NPU Driver:    {}", app_state.npu_version), format!("Arch: {}", app_state.cpu_arch)),
        (format!("RGA Driver:    {}", app_state.rga_version), String::new()),
        (format!("RKNN Runtime:  {}", app_state.rknn_version), String::new()),
        (format!("RKLLM Runtime: {}", app_state.rkllm_version), String::new()),
    ];

    let rows: Vec<Row> = row_data
        .iter()
        .map(|(left, right)| Row::new(vec![left.as_str(), right.as_str()]))
        .collect();

    let table = Table::new(
        rows,
        [Constraint::Percentage(50), Constraint::Percentage(50)],
    )
    .block(
        Block::default()
            .title("SYS")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red)),
    )
    .column_spacing(1);

    f.render_widget(table, area);
}

fn render_gpu_panel(f: &mut Frame, area: Rect, app_state: &AppState) {
    if let Some(usage) = get_gpu_usage() {
        let freq = get_gpu_frequency();
        let freq_str = freq.map(|f| format!(" {} MHz", f)).unwrap_or_default();

        let bar_width = 30;
        let filled = ((usage / 100.0) * bar_width as f32) as usize;
        let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);

        let mut lines = vec![Line::from(vec![
            Span::raw("Mali0 "),
            Span::styled(bar, Style::default().fg(Color::Green)),
            Span::raw(format!(" {:>5.2}%{}", usage, freq_str)),
        ])];

        // Add sparkline if we have history
        if !app_state.gpu_history.is_empty() {
            let sparkline = render_sparkline(&app_state.gpu_history, 100.0);
            lines.push(Line::from(vec![
                Span::raw("History: "),
                Span::styled(sparkline, Style::default().fg(Color::Green)),
            ]));
        }

        let block = Block::default()
            .title("GPU")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green));

        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(paragraph, area);
    }
}

fn render_npu_panel(f: &mut Frame, area: Rect, app_state: &AppState) {
    let loads = get_npu_load();
    if loads.is_empty() {
        return;
    }

    let freq = get_npu_frequency();
    let freq_str = freq.map(|f| format!(" {} MHz", f)).unwrap_or_default();

    let mut lines: Vec<Line> = loads
        .iter()
        .enumerate()
        .map(|(i, &load)| {
            let bar_width = 20;
            let filled = ((load as f32 / 100.0) * bar_width as f32) as usize;
            let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);
            Line::from(vec![
                Span::raw(format!("Core {} ", i)),
                Span::styled(bar, Style::default().fg(Color::Green)),
                Span::raw(format!(" {:>3}%{}", load, if i == 0 { freq_str.as_str() } else { "" })),
            ])
        })
        .collect();

    // Add sparkline if we have history (showing average across all cores)
    if !app_state.npu_history.is_empty() {
        let sparkline = render_sparkline(&app_state.npu_history, 100.0);
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("History: "),
            Span::styled(sparkline, Style::default().fg(Color::Green)),
        ]));
    }

    let block = Block::default()
        .title("NPU")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_rga_panel(f: &mut Frame, area: Rect) {
    if let Some(rga_loads) = get_rga_load() {
        let lines: Vec<Line> = rga_loads
            .iter()
            .map(|(name, load)| {
                let bar_width = 20;
                let filled = ((load / 100.0) * bar_width as f32) as usize;
                let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);
                Line::from(vec![
                    Span::raw(format!("{:<6} ", name)),
                    Span::styled(bar, Style::default().fg(Color::Green)),
                    Span::raw(format!(" {:>5.1}%", load)),
                ])
            })
            .collect();

        let block = Block::default()
            .title("RGA")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green));

        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(paragraph, area);
    }
}

fn render_stats_panel(f: &mut Frame, area: Rect, sys: &System, app_state: &AppState) {
    // Uptime
    let uptime_secs = System::uptime();
    let uptime_days = uptime_secs / 86400;
    let uptime_hours = (uptime_secs % 86400) / 3600;
    let uptime_mins = (uptime_secs % 3600) / 60;
    let uptime_str = if uptime_days > 0 {
        format!("{}d {}h {}m", uptime_days, uptime_hours, uptime_mins)
    } else if uptime_hours > 0 {
        format!("{}h {}m", uptime_hours, uptime_mins)
    } else {
        format!("{}m", uptime_mins)
    };

    // Load average
    let load_avg = System::load_average();
    let load_str = format!("{:.2} {:.2} {:.2}", load_avg.one, load_avg.five, load_avg.fifteen);

    // Total processes
    let total_processes = sys.processes().len();

    let lines = vec![
        Line::from(format!("Uptime:     {}", uptime_str)),
        Line::from(format!("Load Avg:   {}", load_str)),
        Line::from(format!("Governor:   {}", app_state.cpu_governor)),
        Line::from(format!("Processes:  {}", total_processes)),
        Line::from(format!("TCP Conns:  {}", app_state.tcp_connections)),
    ];

    let block = Block::default()
        .title("Stats")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_io_panel(f: &mut Frame, area: Rect, app_state: &AppState) {
    // Build all row data as owned strings
    let mut row_data = vec![
        ("Disk Read".to_string(), format!("{}/s", human_bytes_f64(app_state.disk_read_rate))),
        ("Disk Write".to_string(), format!("{}/s", human_bytes_f64(app_state.disk_write_rate))),
        ("Net RX (Tot)".to_string(), format!("{}/s", human_bytes_f64(app_state.net_rx_rate))),
        ("Net TX (Tot)".to_string(), format!("{}/s", human_bytes_f64(app_state.net_tx_rate))),
    ];

    // Disk space summary
    if let Some((used, total)) = get_disk_total() {
        let percent = (used as f64 / total as f64 * 100.0) as u32;
        row_data.push(("Disk Space".to_string(), format!("{} / {} ({}%)",
            human_bytes(used), human_bytes(total), percent)));
    }

    // Add individual network adapters
    let mut adapters: Vec<_> = app_state.adapter_rates.iter().collect();
    adapters.sort_by(|a, b| a.0.cmp(b.0)); // Sort by adapter name

    for (name, (rx_rate, tx_rate)) in adapters {
        row_data.push((
            format!("{} RX", name),
            format!("{}/s", human_bytes_f64(*rx_rate)),
        ));
        row_data.push((
            format!("{} TX", name),
            format!("{}/s", human_bytes_f64(*tx_rate)),
        ));
    }

    // Create rows from owned strings
    let rows: Vec<Row> = row_data
        .iter()
        .map(|(label, value)| Row::new(vec![label.as_str(), value.as_str()]))
        .collect();

    let table = Table::new(
        rows,
        [Constraint::Percentage(50), Constraint::Percentage(50)],
    )
    .block(
        Block::default()
            .title("I/O")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    )
    .column_spacing(2)
    .style(Style::default().fg(Color::Cyan));

    f.render_widget(table, area);
}

fn render_temperature_panel(f: &mut Frame, area: Rect, app_state: &AppState) {
    let mut row_data = Vec::new();

    // Add thermal zone temperatures
    let temps = get_thermal_cached(&app_state.thermal_zone_paths);
    for (name, temp) in temps {
        row_data.push((name, format!("{}°C", temp)));
    }

    // Add GPU temperature if available
    if let Some(gpu_temp) = get_gpu_temperature() {
        row_data.push(("GPU".to_string(), format!("{}°C", gpu_temp)));
    }

    // Add hwmon sensors (fans, power)
    let hwmon = get_hwmon_sensors();
    for (sensor_name, value) in hwmon {
        row_data.push((sensor_name, value));
    }

    let rows: Vec<Row> = row_data
        .iter()
        .map(|(name, value)| Row::new(vec![name.as_str(), value.as_str()]))
        .collect();

    let table = Table::new(rows, [Constraint::Percentage(60), Constraint::Percentage(40)])
        .block(
            Block::default()
                .title("Temperatures")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .column_spacing(1)
        .style(Style::default().fg(Color::Cyan));

    f.render_widget(table, area);
}

fn render_process_panel(f: &mut Frame, area: Rect, sys: &System, app_state: &AppState) {
    // Calculate how many processes will fit (area height - borders - header - bottom margin)
    let available_rows = area.height.saturating_sub(4) as usize; // 2 for borders, 1 for header, 1 for margin

    // Request more processes than available rows to account for threads being grouped
    // We'll fetch 3x the available rows, assuming on average each process has a few threads
    let process_count = (available_rows * 3).max(20); // At least 20 processes

    let top_processes = get_top_processes(sys, process_count, app_state.process_sort_mode);

    // Apply filter if present
    let filter_lower = app_state.filter_text.to_lowercase();
    let filtered_processes: Vec<_> = if filter_lower.is_empty() {
        top_processes.iter().collect()
    } else {
        top_processes.iter()
            .filter(|p| {
                p.name.to_lowercase().contains(&filter_lower) ||
                p.user.to_lowercase().contains(&filter_lower)
            })
            .collect()
    };

    // Organize processes: group threads under their parent process
    let mut rows: Vec<Row> = Vec::new();
    let mut seen_processes = std::collections::HashSet::new();

    for p in &filtered_processes {
        // Skip threads for now, we'll add them under their parent
        if p.is_thread {
            continue;
        }

        // Skip if we've already added this process
        if seen_processes.contains(&p.pid) {
            continue;
        }
        seen_processes.insert(p.pid);

        // Add the main process
        let hours = p.runtime / 3600;
        let minutes = (p.runtime % 3600) / 60;
        let seconds = p.runtime % 60;
        let runtime_str = if hours > 0 {
            format!("{}:{:02}:{:02}", hours, minutes, seconds)
        } else {
            format!("{}:{:02}", minutes, seconds)
        };

        rows.push(Row::new(vec![
            p.pid.to_string(),
            p.user.clone(),
            format!("{}", p.state),
            format!("{:>3}", p.nice),
            format!("{}", p.cpu_core),
            format!("{}", p.num_threads),
            runtime_str,
            p.name.clone(),
            format!("{:.1}", p.cpu),
            format!("{:.1}", p.mem),
        ]));

        // Now find and add threads for this process
        let threads: Vec<_> = filtered_processes.iter()
            .filter(|t| t.is_thread && t.thread_group_id == p.pid)
            .collect();

        for (idx, t) in threads.iter().enumerate() {
            let is_last = idx == threads.len() - 1;
            let prefix = if is_last { " └─" } else { " ├─" };

            let hours = t.runtime / 3600;
            let minutes = (t.runtime % 3600) / 60;
            let seconds = t.runtime % 60;
            let runtime_str = if hours > 0 {
                format!("{}:{:02}:{:02}", hours, minutes, seconds)
            } else {
                format!("{}:{:02}", minutes, seconds)
            };

            rows.push(Row::new(vec![
                format!("{}{}", prefix, t.pid),
                String::new(), // Empty user for threads
                format!("{}", t.state),
                format!("{:>3}", t.nice),
                format!("{}", t.cpu_core),
                String::new(), // Empty threads count for threads
                runtime_str,
                format!("{} [thread]", t.name),
                format!("{:.1}", t.cpu),
                format!("{:.1}", t.mem),
            ]));
        }
    }

    // Highlight the sorted column in the header and show sort direction
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let bold_underline = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

    let (pid_text, pid_style) = match app_state.process_sort_mode {
        ProcessSortMode::PidAsc => ("PID↑", bold_underline),
        ProcessSortMode::PidDesc => ("PID↓", bold_underline),
        _ => ("PID", bold),
    };

    let (name_text, name_style) = match app_state.process_sort_mode {
        ProcessSortMode::NameAsc => ("Name↑", bold_underline),
        ProcessSortMode::NameDesc => ("Name↓", bold_underline),
        _ => ("Name", bold),
    };

    let (cpu_text, cpu_style) = match app_state.process_sort_mode {
        ProcessSortMode::CpuAsc => ("CPU%↑", bold_underline),
        ProcessSortMode::CpuDesc => ("CPU%↓", bold_underline),
        _ => ("CPU%", bold),
    };

    let (mem_text, mem_style) = match app_state.process_sort_mode {
        ProcessSortMode::MemoryAsc => ("Mem%↑", bold_underline),
        ProcessSortMode::MemoryDesc => ("Mem%↓", bold_underline),
        _ => ("Mem%", bold),
    };

    let header = Row::new(vec![
        Span::styled(pid_text, pid_style),
        Span::styled("User", bold),
        Span::styled("S", bold),         // State
        Span::styled("NI", bold),
        Span::styled("C", bold),
        Span::styled("THR", bold),       // Threads
        Span::styled("Time", bold),
        Span::styled(name_text, name_style),
        Span::styled(cpu_text, cpu_style),
        Span::styled(mem_text, mem_style),
    ])
    .bottom_margin(1);

    let table = Table::new(
        rows,
        [
            Constraint::Length(11),  // PID (wider to accommodate thread prefixes " └─12345")
            Constraint::Length(10),  // User
            Constraint::Length(2),   // State
            Constraint::Length(4),   // Nice
            Constraint::Length(2),   // CPU core
            Constraint::Length(4),   // Threads
            Constraint::Length(9),   // Runtime
            Constraint::Percentage(45), // Name (more space without FDs column)
            Constraint::Length(6),   // CPU%
            Constraint::Length(6),   // Mem%
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title("Processes")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    )
    .column_spacing(1);

    f.render_widget(table, area);
}

fn render_help_text(f: &mut Frame, area: Rect, app_state: &AppState) {
    let sort_name = match app_state.process_sort_mode {
        ProcessSortMode::CpuDesc => "CPU↓",
        ProcessSortMode::CpuAsc => "CPU↑",
        ProcessSortMode::MemoryDesc => "Mem↓",
        ProcessSortMode::MemoryAsc => "Mem↑",
        ProcessSortMode::PidAsc => "PID↑",
        ProcessSortMode::PidDesc => "PID↓",
        ProcessSortMode::NameAsc => "Name↑",
        ProcessSortMode::NameDesc => "Name↓",
    };

    let mut help_spans = vec![
        Span::styled("Sort: ", Style::default().fg(Color::Gray)),
        Span::styled("[C]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("PU  ", Style::default().fg(Color::Gray)),
        Span::styled("[M]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("em  ", Style::default().fg(Color::Gray)),
        Span::styled("[P]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("ID  ", Style::default().fg(Color::Gray)),
        Span::styled("[N]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("ame", Style::default().fg(Color::Gray)),
        Span::styled("  |  Current: ", Style::default().fg(Color::Gray)),
        Span::styled(sort_name, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled("  |  ", Style::default().fg(Color::Gray)),
        Span::styled("[/]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("Filter", Style::default().fg(Color::Gray)),
    ];

    // Show filter status
    if app_state.filter_mode {
        help_spans.push(Span::styled(": ", Style::default().fg(Color::Gray)));
        help_spans.push(Span::styled(&app_state.filter_text, Style::default().fg(Color::Yellow)));
        help_spans.push(Span::styled("_", Style::default().fg(Color::Yellow))); // cursor
    } else if !app_state.filter_text.is_empty() {
        help_spans.push(Span::styled(": ", Style::default().fg(Color::Gray)));
        help_spans.push(Span::styled(&app_state.filter_text, Style::default().fg(Color::Green)));
    }

    help_spans.push(Span::styled("  |  ", Style::default().fg(Color::Gray)));
    help_spans.push(Span::styled("[Q]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    help_spans.push(Span::styled("uit", Style::default().fg(Color::Gray)));

    let help_text = Line::from(help_spans);

    let paragraph = Paragraph::new(help_text);
    f.render_widget(paragraph, area);
}

fn human_bytes(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes as f64;
    let mut unit_idx = 0;

    while val >= 1024.0 && unit_idx < units.len() - 1 {
        val /= 1024.0;
        unit_idx += 1;
    }

    format!("{:.1} {}", val, units[unit_idx])
}

fn human_bytes_f64(bytes: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes;
    let mut unit_idx = 0;

    while val >= 1024.0 && unit_idx < units.len() - 1 {
        val /= 1024.0;
        unit_idx += 1;
    }

    format!("{:.1} {}", val, units[unit_idx])
}

fn format_number(num: u64) -> String {
    if num < 1000 {
        return num.to_string();
    }
    if num < 1_000_000 {
        return format!("{:.1}K", num as f64 / 1000.0);
    }
    if num < 1_000_000_000 {
        return format!("{:.1}M", num as f64 / 1_000_000.0);
    }
    format!("{:.1}G", num as f64 / 1_000_000_000.0)
}
