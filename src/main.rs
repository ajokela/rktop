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
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};

mod hardware;
mod sysinfo_ext;

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
}

impl AppState {
    fn new() -> Self {
        // Cache all static system info at startup (expensive operations)
        let board_name = get_board_name();
        let rk_model = get_rk_model();
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
            npu_version,
            rga_version,
            rknn_version,
            rkllm_version,
            has_gpu,
            has_npu,
            has_rga,
            cpu_freq_ranges,
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

    loop {
        // Update system info every second
        let mut should_render = false;
        if last_update.elapsed() >= Duration::from_secs(1) {
            sys.refresh_cpu_all();
            sys.refresh_memory();
            app_state.update_cpu_stats(); // Update CPU stats (context switches, interrupts)
            app_state.update_stats(); // Update governor and TCP connections (throttled internally)
            *last_update = Instant::now();
            should_render = true; // Force render after data update
        }

        // Update network stats every 2 seconds (reduces file I/O significantly)
        if last_network_update.elapsed() >= Duration::from_secs(2) {
            networks.refresh();
            disks.refresh();
            app_state.update_io_rates(disks, networks);
            *last_network_update = Instant::now();
        }

        // Update processes less frequently (every 3 seconds) to reduce overhead
        if last_process_update.elapsed() >= Duration::from_secs(3) {
            sys.refresh_processes(ProcessesToUpdate::All, true);
            *last_process_update = Instant::now();
        }

        // Render at most once per second, or immediately after data update
        if should_render || last_render.elapsed() >= Duration::from_secs(1) {
            terminal.draw(|f| ui(f, sys, app_state))?;
            last_render = Instant::now();
        }

        // Handle input with timeout - sleep most of the time to reduce CPU
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                            return Ok(());
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
    render_temperature_panel(f, bottom_left_chunks[1]);

    // Bottom right: Process table and help text
    let process_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(2)])
        .split(bottom_chunks[1]);

    render_process_panel(f, process_chunks[0], sys, app_state);
    render_help_text(f, process_chunks[1], app_state);
}

fn render_cpu_panel(f: &mut Frame, area: Rect, sys: &System, app_state: &AppState) {
    let cpus = sys.cpus();
    let cpu_freqs = get_cpu_frequencies();

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
        constraints.push(Constraint::Length(5));
    }
    if app_state.has_npu {
        constraints.push(Constraint::Length(5));
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
        render_gpu_panel(f, chunks[chunk_idx]);
        chunk_idx += 1;
    }

    // NPU
    if app_state.has_npu {
        render_npu_panel(f, chunks[chunk_idx]);
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
    let lines = vec![
        Line::from(vec![
            Span::styled("Board: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(&app_state.board_name, Style::default().add_modifier(Modifier::ITALIC)),
        ]),
        Line::from(vec![
            Span::styled("SoC: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(&app_state.rk_model, Style::default().add_modifier(Modifier::ITALIC)),
        ]),
        Line::from(format!("NPU Driver:    {}", app_state.npu_version)),
        Line::from(format!("RGA Driver:    {}", app_state.rga_version)),
        Line::from(format!("RKNN Runtime:  {}", app_state.rknn_version)),
        Line::from(format!("RKLLM Runtime: {}", app_state.rkllm_version)),
    ];

    let block = Block::default()
        .title("SYS")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_gpu_panel(f: &mut Frame, area: Rect) {
    if let Some(usage) = get_gpu_usage() {
        let freq = get_gpu_frequency();
        let freq_str = freq.map(|f| format!(" {} MHz", f)).unwrap_or_default();

        let bar_width = 30;
        let filled = ((usage / 100.0) * bar_width as f32) as usize;
        let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);

        let line = Line::from(vec![
            Span::raw("Mali0 "),
            Span::styled(bar, Style::default().fg(Color::Green)),
            Span::raw(format!(" {:>5.2}%{}", usage, freq_str)),
        ]);

        let block = Block::default()
            .title("GPU")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green));

        let paragraph = Paragraph::new(vec![line]).block(block);
        f.render_widget(paragraph, area);
    }
}

fn render_npu_panel(f: &mut Frame, area: Rect) {
    let loads = get_npu_load();
    if loads.is_empty() {
        return;
    }

    let freq = get_npu_frequency();
    let freq_str = freq.map(|f| format!(" {} MHz", f)).unwrap_or_default();

    let lines: Vec<Line> = loads
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

fn render_temperature_panel(f: &mut Frame, area: Rect) {
    let temps = get_thermal();

    let temp_strings: Vec<String> = temps.iter().map(|(_, t)| format!("{}°C", t)).collect();
    let rows: Vec<Row> = temps
        .iter()
        .enumerate()
        .map(|(i, (name, _))| Row::new(vec![name.as_str(), temp_strings[i].as_str()]))
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
    let process_count = available_rows.max(1); // At least 1 process

    let top_processes = get_top_processes(sys, process_count, app_state.process_sort_mode);

    let rows: Vec<Row> = top_processes
        .iter()
        .map(|p| {
            // Format runtime as HH:MM:SS or MM:SS for processes under 1 hour
            let hours = p.runtime / 3600;
            let minutes = (p.runtime % 3600) / 60;
            let seconds = p.runtime % 60;
            let runtime_str = if hours > 0 {
                format!("{}:{:02}:{:02}", hours, minutes, seconds)
            } else {
                format!("{}:{:02}", minutes, seconds)
            };

            Row::new(vec![
                p.pid.to_string(),
                p.user.clone(),
                format!("{:>3}", p.nice),
                format!("{}", p.cpu_core),
                runtime_str,
                p.name.clone(),
                format!("{:.1}", p.cpu),
                format!("{:.1}", p.mem),
            ])
        })
        .collect();

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
        Span::styled("NI", bold),
        Span::styled("C", bold),
        Span::styled("Time", bold),
        Span::styled(name_text, name_style),
        Span::styled(cpu_text, cpu_style),
        Span::styled(mem_text, mem_style),
    ])
    .bottom_margin(1);

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),   // PID
            Constraint::Length(10),  // User
            Constraint::Length(4),   // Nice
            Constraint::Length(2),   // CPU core
            Constraint::Length(9),   // Runtime
            Constraint::Percentage(45), // Name (reduced further)
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

    let help_text = Line::from(vec![
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
        Span::styled("[Q]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("uit", Style::default().fg(Color::Gray)),
    ]);

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
