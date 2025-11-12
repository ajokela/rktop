use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;
use sysinfo::{Process, System};
use crate::ProcessSortMode;

// Global cache for UID to username mappings
static USER_CACHE: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub user: String,
    pub cpu: f32,
    pub mem: f32,
    pub nice: i32,
    pub runtime: u64, // in seconds
    pub cpu_core: u32, // Which CPU core process is running on
    pub is_thread: bool, // Is this a thread of another process?
    pub thread_group_id: u32, // TGID - the main process ID for threads
    pub state: char, // Process state: R, S, D, Z, T, etc.
    pub num_threads: u32, // Number of threads in this process
}

#[derive(Debug, Clone)]
pub struct ZramInfo {
    pub orig_data_size: u64,
    pub compr_data_size: u64,
    pub used: u64,
    pub limit: u64,
}

impl ZramInfo {
    /// Calculate compression ratio (original / compressed)
    pub fn compression_ratio(&self) -> f64 {
        if self.compr_data_size > 0 {
            self.orig_data_size as f64 / self.compr_data_size as f64
        } else {
            0.0
        }
    }
}

/// Get top processes with configurable sorting
pub fn get_top_processes(sys: &System, count: usize, sort_mode: ProcessSortMode) -> Vec<ProcessInfo> {
    // First pass: collect minimal info and sort
    let mut minimal_processes: Vec<_> = sys
        .processes()
        .iter()
        .map(|(pid, process)| {
            (
                pid.as_u32(),
                process,
                process.cpu_usage(),
                process.memory() as f32 / sys.total_memory() as f32 * 100.0,
            )
        })
        .collect();

    // Sort the minimal list based on selected mode
    match sort_mode {
        ProcessSortMode::CpuDesc => {
            minimal_processes.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
        }
        ProcessSortMode::CpuAsc => {
            minimal_processes.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());
        }
        ProcessSortMode::MemoryDesc => {
            minimal_processes.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap());
        }
        ProcessSortMode::MemoryAsc => {
            minimal_processes.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap());
        }
        ProcessSortMode::PidAsc => {
            minimal_processes.sort_by_key(|p| p.0);
        }
        ProcessSortMode::PidDesc => {
            minimal_processes.sort_by(|a, b| b.0.cmp(&a.0));
        }
        ProcessSortMode::NameAsc => {
            minimal_processes.sort_by(|a, b| {
                a.1.name().to_string_lossy().to_lowercase()
                    .cmp(&b.1.name().to_string_lossy().to_lowercase())
            });
        }
        ProcessSortMode::NameDesc => {
            minimal_processes.sort_by(|a, b| {
                b.1.name().to_string_lossy().to_lowercase()
                    .cmp(&a.1.name().to_string_lossy().to_lowercase())
            });
        }
    }

    // Second pass: only read detailed info for top N processes
    minimal_processes
        .into_iter()
        .take(count)
        .map(|(pid_u32, process, cpu, mem)| {
            let name = process.name().to_string_lossy().to_string();
            let user = get_process_user(process);
            let runtime = process.run_time();

            // Only read extended info for top N processes
            let nice = get_process_nice(pid_u32);
            let cpu_core = get_process_cpu_core(pid_u32);
            let (thread_group_id, is_thread, num_threads, state, _num_fds) =
                get_process_extended_info(pid_u32);

            ProcessInfo {
                pid: pid_u32,
                name,
                user,
                cpu,
                mem,
                nice,
                runtime,
                cpu_core,
                is_thread,
                thread_group_id,
                state,
                num_threads,
            }
        })
        .collect()
}

fn get_process_nice(pid: u32) -> i32 {
    // Read nice level from /proc/<pid>/stat
    let stat_path = format!("/proc/{}/stat", pid);
    if let Ok(content) = fs::read_to_string(&stat_path) {
        // The nice value is the 19th field in /proc/pid/stat
        let fields: Vec<&str> = content.split_whitespace().collect();
        if fields.len() >= 19 {
            if let Ok(nice) = fields[18].parse::<i32>() {
                return nice;
            }
        }
    }
    0 // Default nice value
}

fn get_process_cpu_core(pid: u32) -> u32 {
    // Read current CPU core from /proc/<pid>/stat
    let stat_path = format!("/proc/{}/stat", pid);
    if let Ok(content) = fs::read_to_string(&stat_path) {
        // The processor (CPU core) is the 39th field in /proc/pid/stat
        let fields: Vec<&str> = content.split_whitespace().collect();
        if fields.len() >= 39 {
            if let Ok(cpu_core) = fields[38].parse::<u32>() {
                return cpu_core;
            }
        }
    }
    0 // Default to core 0
}

/// Get consolidated process info from /proc/[pid]/status and /proc/[pid]/stat
/// Returns (tgid, is_thread, num_threads, state, num_fds)
/// This reads files once instead of multiple times
fn get_process_extended_info(pid: u32) -> (u32, bool, u32, char, u32) {
    let mut tgid = pid;
    let mut num_threads = 1;
    let mut state = 'U';

    // Read /proc/[pid]/status once for TGID and thread count
    let status_path = format!("/proc/{}/status", pid);
    if let Ok(content) = fs::read_to_string(&status_path) {
        for line in content.lines() {
            if line.starts_with("Tgid:") {
                if let Some(tgid_str) = line.split_whitespace().nth(1) {
                    if let Ok(parsed_tgid) = tgid_str.parse::<u32>() {
                        tgid = parsed_tgid;
                    }
                }
            } else if line.starts_with("Threads:") {
                if let Some(threads_str) = line.split_whitespace().nth(1) {
                    if let Ok(threads) = threads_str.parse::<u32>() {
                        num_threads = threads;
                    }
                }
            }
        }
    }

    let is_thread = pid != tgid;

    // Read /proc/[pid]/stat once for state
    let stat_path = format!("/proc/{}/stat", pid);
    if let Ok(content) = fs::read_to_string(&stat_path) {
        // State is the field after the command name (which is in parentheses)
        if let Some(paren_end) = content.rfind(')') {
            let after_name = &content[paren_end + 1..];
            if let Some(state_char) = after_name.trim().chars().next() {
                state = state_char;
            }
        }
    }

    // Skip file descriptor counting entirely - it's very expensive
    // Counting FDs requires opening and iterating /proc/[pid]/fd directory
    // For a system with 200+ processes, this adds significant overhead
    let num_fds = 0;

    (tgid, is_thread, num_threads, state, num_fds)
}

fn get_process_user(process: &Process) -> String {
    if let Some(uid) = process.user_id() {
        let uid_num = uid.to_string().parse::<u32>().unwrap_or(0);

        // Try to get from cache first
        let mut cache = USER_CACHE.lock().unwrap();
        if cache.is_none() {
            *cache = Some(HashMap::new());
        }

        if let Some(ref mut map) = *cache {
            // Check cache
            if let Some(username) = map.get(&uid_num) {
                return username.clone();
            }

            // Not in cache, look it up once and cache it
            if let Ok(output) = std::process::Command::new("id")
                .arg("-nu")
                .arg(uid.to_string())
                .output()
            {
                if output.status.success() {
                    let username = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    map.insert(uid_num, username.clone());
                    return username;
                }
            }

            // Failed to resolve, cache the UID as string
            let uid_str = uid.to_string();
            map.insert(uid_num, uid_str.clone());
            return uid_str;
        }
    }
    "unknown".to_string()
}

/// Read ZRAM statistics
pub fn get_zram_info() -> Option<ZramInfo> {
    let path = "/sys/block/zram0/mm_stat";
    if let Ok(content) = fs::read_to_string(path) {
        let parts: Vec<&str> = content.split_whitespace().collect();
        if parts.len() >= 4 {
            return Some(ZramInfo {
                orig_data_size: parts[0].parse().ok()?,
                compr_data_size: parts[1].parse().ok()?,
                used: parts[2].parse().ok()?,
                limit: parts[3].parse().ok()?,
            });
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
pub struct CpuStats {
    pub context_switches: u64,
    pub interrupts: u64,
    pub softirqs: u64,
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub running_procs: u64,
    pub blocked_procs: u64,
}

/// Read CPU statistics from /proc/stat
pub fn get_cpu_stats() -> CpuStats {
    let mut stats = CpuStats::default();

    if let Ok(content) = fs::read_to_string("/proc/stat") {
        for line in content.lines() {
            if line.starts_with("cpu ") {
                // Parse aggregate CPU time: user nice system idle iowait irq softirq...
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 8 {
                    stats.user = parts[1].parse().unwrap_or(0);
                    stats.nice = parts[2].parse().unwrap_or(0);
                    stats.system = parts[3].parse().unwrap_or(0);
                    stats.idle = parts[4].parse().unwrap_or(0);
                    stats.iowait = parts[5].parse().unwrap_or(0);
                    stats.irq = parts[6].parse().unwrap_or(0);
                    stats.softirq = parts[7].parse().unwrap_or(0);
                }
            } else if line.starts_with("ctxt ") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    stats.context_switches = value.parse().unwrap_or(0);
                }
            } else if line.starts_with("intr ") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    stats.interrupts = value.parse().unwrap_or(0);
                }
            } else if line.starts_with("softirq ") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    stats.softirqs = value.parse().unwrap_or(0);
                }
            } else if line.starts_with("procs_running ") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    stats.running_procs = value.parse().unwrap_or(0);
                }
            } else if line.starts_with("procs_blocked ") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    stats.blocked_procs = value.parse().unwrap_or(0);
                }
            }
        }
    }

    stats
}
