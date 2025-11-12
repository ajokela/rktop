use regex::Regex;
use std::fs;
use std::path::Path;
use goblin::Object;
use crate::file_cache::{read_cached_file, read_cached_u32, read_cached_i32};

/// Get total disk space usage across all mounted filesystems
pub fn get_disk_total() -> Option<(u64, u64)> {
    // Read /proc/mounts to find mounted filesystems
    let mounts = fs::read_to_string("/proc/mounts").ok()?;

    let mut total_size = 0u64;
    let mut total_used = 0u64;

    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let mount_point = parts[1];
        let fs_type = parts[2];

        // Skip virtual filesystems
        if fs_type == "tmpfs" || fs_type == "devtmpfs" || fs_type == "proc"
            || fs_type == "sysfs" || fs_type == "devpts" || fs_type == "cgroup"
            || fs_type == "cgroup2" || fs_type == "securityfs" || fs_type == "debugfs"
            || fs_type == "tracefs" || fs_type == "pstore" || fs_type == "bpf"
            || fs_type == "configfs" || fs_type == "hugetlbfs" || fs_type == "mqueue" {
            continue;
        }

        // Get statvfs info
        if let Ok(stat) = nix::sys::statvfs::statvfs(mount_point) {
            let block_size = stat.block_size() as u64;
            let total_blocks = stat.blocks() as u64;
            let free_blocks = stat.blocks_free() as u64;

            total_size += block_size * total_blocks;
            total_used += block_size * (total_blocks - free_blocks);
        }
    }

    if total_size > 0 {
        Some((total_used, total_size))
    } else {
        None
    }
}

/// Get list of network adapters, filtering out virtual interfaces
pub fn get_network_adapters() -> Vec<String> {
    let mut adapters = Vec::new();
    let net_dir = "/sys/class/net";

    if let Ok(entries) = fs::read_dir(net_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();

            // Filter out virtual/unwanted interfaces
            if name == "lo" || name.starts_with("dummy") || name.starts_with("veth")
                || name.starts_with("br-") || name.starts_with("docker") {
                continue;
            }

            adapters.push(name);
        }
    }

    adapters.sort();
    adapters
}

/// Get thermal zone paths (cached at startup to avoid repeated directory scans)
/// Returns (label, temp_path, type_path) tuples
pub fn get_thermal_zone_paths() -> Vec<(String, String, String)> {
    let mut paths = Vec::new();
    let thermal_dir = "/sys/class/thermal";

    if let Ok(entries) = fs::read_dir(thermal_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                if name_str.starts_with("thermal_zone") {
                    let temp_path = path.join("temp").to_string_lossy().to_string();
                    let type_path = path.join("type").to_string_lossy().to_string();

                    // Read the type to get the label
                    if let Ok(type_content) = fs::read_to_string(&type_path) {
                        let label = type_content.trim().replace("_thermal", "");
                        paths.push((label, temp_path, type_path));
                    }
                }
            }
        }
    }

    paths
}

/// Read thermal sensors using cached paths (avoids directory scanning)
pub fn get_thermal_cached(cached_paths: &[(String, String, String)]) -> Vec<(String, i32)> {
    let mut temps = Vec::new();

    for (label, temp_path, _) in cached_paths {
        if let Some(temp_millis) = read_cached_i32(temp_path) {
            let temp_celsius = temp_millis / 1000;
            temps.push((label.clone(), temp_celsius));
        }
    }

    temps
}

/// Read GPU temperature from hwmon
pub fn get_gpu_temperature() -> Option<i32> {
    // Try multiple paths for GPU temperature
    let paths = [
        "/sys/class/hwmon/hwmon0/temp1_input",
        "/sys/class/hwmon/hwmon1/temp1_input",
        "/sys/class/hwmon/hwmon2/temp1_input",
        "/sys/class/hwmon/hwmon3/temp1_input",
    ];

    for path in &paths {
        // Check if this is a GPU sensor by reading the name
        let name_path = path.replace("temp1_input", "name");
        if let Ok(name) = fs::read_to_string(&name_path) {
            if name.trim().contains("gpu") || name.trim().contains("mali") {
                if let Some(temp_millis) = read_cached_i32(path) {
                    return Some(temp_millis / 1000);
                }
            }
        }
    }

    None
}

/// Read all hwmon sensors (fans, power, etc.)
pub fn get_hwmon_sensors() -> Vec<(String, String)> {
    let mut sensors = Vec::new();
    let hwmon_dir = "/sys/class/hwmon";

    if let Ok(entries) = fs::read_dir(hwmon_dir) {
        for entry in entries.flatten() {
            let path = entry.path();

            // Read device name
            let name_path = path.join("name");
            let device_name = fs::read_to_string(&name_path)
                .unwrap_or_else(|_| "unknown".to_string())
                .trim()
                .to_string();

            // Look for fan speeds
            for i in 1..=10 {
                let fan_path = path.join(format!("fan{}_input", i));
                if let Ok(rpm) = fs::read_to_string(&fan_path) {
                    if let Ok(rpm_val) = rpm.trim().parse::<u32>() {
                        sensors.push((format!("{} Fan{}", device_name, i), format!("{} RPM", rpm_val)));
                    }
                }
            }

            // Look for power sensors
            for i in 1..=10 {
                let power_path = path.join(format!("power{}_input", i));
                if let Ok(microwatts) = fs::read_to_string(&power_path) {
                    if let Ok(uw_val) = microwatts.trim().parse::<u64>() {
                        let watts = uw_val as f64 / 1_000_000.0;
                        sensors.push((format!("{} Power{}", device_name, i), format!("{:.2} W", watts)));
                    }
                }
            }
        }
    }

    sensors
}

/// Read GPU utilization from Mali debugfs (using cached file descriptors)
pub fn get_gpu_usage() -> Option<f32> {
    let path = "/sys/kernel/debug/mali0/dvfs_utilization";
    if let Ok(content) = read_cached_file(path) {
        // Parse "busy_time: X idle_time: Y" format
        let parts: Vec<&str> = content.split_whitespace().collect();
        let mut busy_time = 0u64;
        let mut idle_time = 0u64;

        for i in (0..parts.len()).step_by(2) {
            if i + 1 < parts.len() {
                let key = parts[i].trim_end_matches(':');
                let value = parts[i + 1].parse::<u64>().ok()?;
                match key {
                    "busy_time" => busy_time = value,
                    "idle_time" => idle_time = value,
                    _ => {}
                }
            }
        }

        let total_time = busy_time + idle_time;
        if total_time > 0 {
            return Some((busy_time as f32 / total_time as f32) * 100.0);
        }
    }
    None
}

/// Read CPU frequencies for each core (using cached file descriptors)
pub fn get_cpu_frequencies() -> Vec<u32> {
    let mut freqs = Vec::new();
    let mut cpu_id = 0;

    loop {
        let path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", cpu_id);
        if let Some(freq_khz) = read_cached_u32(&path) {
            freqs.push(freq_khz / 1000); // Convert to MHz
            cpu_id += 1;
            continue;
        }
        break;
    }

    freqs
}

/// Get CPU frequency scaling ranges for all clusters
/// Returns vector of (min, max) pairs for each unique cluster
pub fn get_cpu_freq_ranges() -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut seen_ranges = std::collections::HashSet::new();
    let mut cpu_id = 0;

    loop {
        let min_path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_min_freq", cpu_id);
        let max_path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_max_freq", cpu_id);

        if let (Ok(min_content), Ok(max_content)) = (fs::read_to_string(&min_path), fs::read_to_string(&max_path)) {
            if let (Ok(min_khz), Ok(max_khz)) = (min_content.trim().parse::<u32>(), max_content.trim().parse::<u32>()) {
                let range = (min_khz / 1000, max_khz / 1000); // Convert to MHz

                // Only add unique ranges (to handle clusters)
                if seen_ranges.insert(range) {
                    ranges.push(range);
                }

                cpu_id += 1;
                continue;
            }
        }
        break;
    }

    ranges
}

/// Read GPU frequency
pub fn get_gpu_frequency() -> Option<u32> {
    let paths = [
        "/sys/devices/platform/fb000000.gpu-panthor/devfreq/fb000000.gpu-panthor/cur_freq",
        "/sys/class/devfreq/fb000000.gpu/cur_freq",
    ];

    for path in &paths {
        if let Ok(content) = fs::read_to_string(path) {
            if let Ok(freq_hz) = content.trim().parse::<u64>() {
                return Some((freq_hz / 1_000_000) as u32); // Convert to MHz
            }
        }
    }
    None
}

/// Read NPU frequency (using cached file descriptors)
pub fn get_npu_frequency() -> Option<u32> {
    let path = "/sys/class/devfreq/fdab0000.npu/cur_freq";
    read_cached_file(path)
        .ok()
        .and_then(|content| content.trim().parse::<u64>().ok())
        .map(|freq_hz| (freq_hz / 1_000_000) as u32)
}

/// Read NPU load percentages for each core (using cached file descriptors)
pub fn get_npu_load() -> Vec<u8> {
    let path = "/sys/kernel/debug/rknpu/load";
    if let Ok(content) = read_cached_file(path) {
        let re = Regex::new(r"Core(\d+):\s*(\d+)%").unwrap();
        let mut loads = Vec::new();

        for cap in re.captures_iter(&content) {
            if let Ok(pct) = cap[2].parse::<u8>() {
                loads.push(pct);
            }
        }

        return loads;
    }
    Vec::new()
}

/// Read RGA (Rockchip Graphics Accelerator) load (using cached file descriptors)
/// Returns a map of scheduler names to load percentages
pub fn get_rga_load() -> Option<Vec<(String, f32)>> {
    let path = "/sys/kernel/debug/rkrga/load";
    if let Ok(content) = read_cached_file(path) {
        let lines: Vec<&str> = content.lines().collect();
        let mut rga_loads = Vec::new();
        let mut current_scheduler = String::new();
        let mut scheduler_index = 0;

        for line in lines {
            let line = line.trim();

            if line.contains("-") || line.contains("= load =") {
                continue;
            }

            if line.starts_with("scheduler[") {
                // Extract scheduler index and name
                if let Some(bracket_end) = line.find(']') {
                    if let Some(idx_str) = line.get(10..bracket_end) {
                        scheduler_index = idx_str.parse::<usize>().unwrap_or(0);
                    }
                }
                if let Some(name) = line.split(':').nth(1) {
                    let base_name = name.trim().to_string();
                    // Create unique name with index (e.g., "rga3_0", "rga3_1", "rga2")
                    current_scheduler = format!("{}_{}", base_name, scheduler_index);
                }
            } else if line.starts_with("load =") {
                if let Some(load_str) = line.split('=').nth(1) {
                    let load_str = load_str.replace('%', "").trim().to_string();
                    if let Ok(load) = load_str.parse::<f32>() {
                        if !current_scheduler.is_empty() {
                            rga_loads.push((current_scheduler.clone(), load));
                        }
                    }
                }
            }
        }

        if !rga_loads.is_empty() {
            return Some(rga_loads);
        }
    }
    None
}

/// Get full board name
pub fn get_board_name() -> String {
    let paths = [
        "/proc/device-tree/model",
        "/sys/firmware/devicetree/base/model",
    ];

    for path in &paths {
        if Path::new(path).exists() {
            if let Ok(content) = fs::read(path) {
                let model = String::from_utf8_lossy(&content)
                    .trim_end_matches('\0')
                    .trim()
                    .to_string();
                if !model.is_empty() {
                    return model;
                }
            }
        }
    }

    "Unknown Board".to_string()
}

/// Detect Rockchip SoC model
pub fn get_rk_model() -> String {
    let board_name = get_board_name();

    // Extract RKxxxx pattern
    let re = Regex::new(r"\b(RK\d+)\b").unwrap();
    if let Some(cap) = re.captures(&board_name) {
        return cap[1].to_uppercase();
    }

    "Unknown RK".to_string()
}

/// Get CPU architecture information
pub fn get_cpu_architecture() -> String {
    // Try to read from /proc/cpuinfo
    if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
        let mut arch = None;
        let mut parts = std::collections::HashSet::new();

        for line in content.lines() {
            if line.starts_with("CPU architecture:") {
                arch = line.split(':').nth(1).map(|s| s.trim().to_string());
            } else if line.starts_with("CPU part") {
                if let Some(part) = line.split(':').nth(1).map(|s| s.trim().to_string()) {
                    parts.insert(part);
                }
            }
        }

        // Build architecture string
        if let Some(arch_val) = arch {
            let mut result = format!("ARMv{}", arch_val);

            // Collect all core types found
            let mut core_names = Vec::new();
            for part_val in &parts {
                let core_name = match part_val.as_str() {
                    "0xd03" => Some("A53"),
                    "0xd04" => Some("A35"),
                    "0xd05" => Some("A55"),
                    "0xd07" => Some("A57"),
                    "0xd08" => Some("A72"),
                    "0xd09" => Some("A73"),
                    "0xd0a" => Some("A75"),
                    "0xd0b" => Some("A76"),
                    "0xd0d" => Some("A77"),
                    "0xd40" => Some("N1"),
                    "0xd41" => Some("A78"),
                    "0xd44" => Some("X1"),
                    "0xd46" => Some("A510"),
                    "0xd47" => Some("A710"),
                    "0xd48" => Some("X2"),
                    "0xd4d" => Some("A715"),
                    _ => None,
                };
                if let Some(name) = core_name {
                    core_names.push(name);
                }
            }

            // Sort and add to result
            if !core_names.is_empty() {
                core_names.sort();
                result.push_str(&format!(" ({})", core_names.join("+")));
            }

            return result;
        }
    }

    // Fallback to uname
    if let Ok(output) = std::process::Command::new("uname").arg("-m").output() {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }

    "Unknown".to_string()
}

/// Read RGA driver version
pub fn get_rga_version() -> String {
    let path = "/sys/kernel/debug/rkrga/driver_version";
    if let Ok(content) = fs::read_to_string(path) {
        if let Some(version) = content.split(':').nth(1) {
            return version.trim().to_string();
        }
    }
    "Not Detected".to_string()
}

/// Read NPU kernel driver version
pub fn get_npu_driver_version() -> String {
    let path = "/sys/kernel/debug/rknpu/version";
    if let Ok(content) = fs::read_to_string(path) {
        if let Some(version) = content.split(':').nth(1) {
            return version.trim().to_string();
        }
    }
    "Not Detected".to_string()
}

/// Extract version string from binary using goblin
fn extract_version_from_binary(path: &str, pattern: &str) -> String {
    // Try to read the binary file
    let buffer = match fs::read(path) {
        Ok(buf) => buf,
        Err(_) => return "Not Detected".to_string(),
    };

    // Parse the binary with goblin
    let obj = match Object::parse(&buffer) {
        Ok(obj) => obj,
        Err(_) => return "Not Detected".to_string(),
    };

    // For ELF binaries, search through the .rodata section
    if let Object::Elf(elf) = obj {
        for section in elf.section_headers.iter() {
            // Look in .rodata or any section that might contain strings
            if let Some(name) = elf.shdr_strtab.get_at(section.sh_name) {
                if name == ".rodata" || name.contains("data") {
                    let start = section.sh_offset as usize;
                    let end = start + section.sh_size as usize;

                    if end <= buffer.len() {
                        let section_data = &buffer[start..end];

                        // Convert to lossy UTF-8 and search for pattern
                        let text = String::from_utf8_lossy(section_data);

                        // Find the pattern and extract version number
                        if let Some(pos) = text.find(pattern) {
                            let substr = &text[pos..];
                            let re = Regex::new(r"(\d+\.\d+\.\d+)").unwrap();
                            if let Some(cap) = re.captures(substr) {
                                return cap[1].to_string();
                            }
                        }
                    }
                }
            }
        }
    }

    "Not Detected".to_string()
}

/// Read librknnrt library version
pub fn get_librknnrt_version() -> String {
    extract_version_from_binary("/usr/lib/librknnrt.so", "librknnrt version:")
}

/// Read librkllmrt library version
pub fn get_librkllmrt_version() -> String {
    extract_version_from_binary("/usr/lib/librkllmrt.so", "RKLLM SDK (version:")
}
