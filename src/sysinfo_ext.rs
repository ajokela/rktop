use std::collections::{HashMap, HashSet};
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
    // Use unwrap_or(Equal) to safely handle potential NaN values in CPU/memory percentages
    use std::cmp::Ordering;
    match sort_mode {
        ProcessSortMode::CpuDesc => {
            minimal_processes.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));
        }
        ProcessSortMode::CpuAsc => {
            minimal_processes.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(Ordering::Equal));
        }
        ProcessSortMode::MemoryDesc => {
            minimal_processes.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(Ordering::Equal));
        }
        ProcessSortMode::MemoryAsc => {
            minimal_processes.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(Ordering::Equal));
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

    let leaders = thread_group_leaders();

    // Second pass: only read detailed info for top N processes
    minimal_processes
        .into_iter()
        .take(count)
        .map(|(pid_u32, process, cpu, mem)| {
            let name = process.name().to_string_lossy().to_string();
            let user = get_process_user(process);
            let runtime = process.run_time();

            // One read covers nice, CPU core, state and thread count.
            let stat = read_proc_stat(pid_u32);
            let nice = stat.as_ref().map(|s| s.nice).unwrap_or(0);
            let cpu_core = stat.as_ref().map(|s| s.processor).unwrap_or(0);
            let num_threads = stat.as_ref().map(|s| s.num_threads).unwrap_or(1);
            let state = stat.as_ref().map(|s| s.state).unwrap_or('U');
            // A pid listed in /proc is its own thread group leader. For the
            // rest, sysinfo already recorded the owning process as the
            // parent, so no file has to be read at all.
            let thread_group_id = if leaders.contains(&pid_u32) {
                pid_u32
            } else {
                process.parent().map(|p| p.as_u32()).unwrap_or(pid_u32)
            };
            let is_thread = pid_u32 != thread_group_id;

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

/// The fields of /proc/<pid>/stat that this tool needs.
struct ProcStat {
    state: char,
    nice: i32,
    num_threads: u32,
    processor: u32,
}

/// Read and parse /proc/<pid>/stat once.
///
/// Fields are located relative to the last ')' rather than by splitting the
/// whole line: the command name is parenthesised and may contain spaces,
/// which shifts every later field.
fn read_proc_stat(pid: u32) -> Option<ProcStat> {
    let content = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let after_comm = &content[content.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();

    // fields[0] is stat field 3, so stat field N is at index N - 3.
    let field = |n: usize| fields.get(n - 3).copied();

    Some(ProcStat {
        state: field(3).and_then(|f| f.chars().next()).unwrap_or('U'),
        nice: field(19).and_then(|f| f.parse().ok()).unwrap_or(0),
        num_threads: field(20).and_then(|f| f.parse().ok()).unwrap_or(1),
        processor: field(39).and_then(|f| f.parse().ok()).unwrap_or(0),
    })
}

/// The pids listed in /proc, which are exactly the thread group leaders.
///
/// Reading this once per refresh avoids a /proc/<pid>/status read for every
/// process that is not a thread, which is most of them.
fn thread_group_leaders() -> HashSet<u32> {
    let mut leaders = HashSet::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() {
                leaders.insert(pid);
            }
        }
    }
    leaders
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

            // Not in cache, read from /etc/passwd (faster than spawning 'id' command)
            if let Ok(passwd_content) = fs::read_to_string("/etc/passwd") {
                for line in passwd_content.lines() {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() >= 3 {
                        if let Ok(line_uid) = parts[2].parse::<u32>() {
                            if line_uid == uid_num {
                                let username = parts[0].to_string();
                                map.insert(uid_num, username.clone());
                                return username;
                            }
                        }
                    }
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
