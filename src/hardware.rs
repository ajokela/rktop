use regex::Regex;
use std::fs;
use std::path::Path;
use goblin::Object;

/// Read GPU utilization from Mali debugfs
pub fn get_gpu_usage() -> Option<f32> {
    let path = "/sys/kernel/debug/mali0/dvfs_utilization";
    if let Ok(content) = fs::read_to_string(path) {
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

/// Read CPU frequencies for each core
pub fn get_cpu_frequencies() -> Vec<u32> {
    let mut freqs = Vec::new();
    let mut cpu_id = 0;

    loop {
        let path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", cpu_id);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(freq_khz) = content.trim().parse::<u32>() {
                freqs.push(freq_khz / 1000); // Convert to MHz
                cpu_id += 1;
                continue;
            }
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

/// Read NPU frequency
pub fn get_npu_frequency() -> Option<u32> {
    let path = "/sys/class/devfreq/fdab0000.npu/cur_freq";
    if let Ok(content) = fs::read_to_string(path) {
        if let Ok(freq_hz) = content.trim().parse::<u64>() {
            return Some((freq_hz / 1_000_000) as u32); // Convert to MHz
        }
    }
    None
}

/// Read NPU load percentages for each core
pub fn get_npu_load() -> Vec<u8> {
    let path = "/sys/kernel/debug/rknpu/load";
    if let Ok(content) = fs::read_to_string(path) {
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

/// Read RGA (Rockchip Graphics Accelerator) load
/// Returns a map of scheduler names to load percentages
pub fn get_rga_load() -> Option<Vec<(String, f32)>> {
    let path = "/sys/kernel/debug/rkrga/load";
    if let Ok(content) = fs::read_to_string(path) {
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

/// Read temperature sensors from both thermal_zone and hwmon
pub fn get_thermal() -> Vec<(String, i32)> {
    let mut temps = Vec::new();

    // Read from thermal_zone (SoC temperatures)
    let thermal_dir = "/sys/class/thermal";
    if let Ok(entries) = fs::read_dir(thermal_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                if name_str.starts_with("thermal_zone") {
                    let temp_path = path.join("temp");
                    let type_path = path.join("type");

                    if let (Ok(temp_content), Ok(type_content)) =
                        (fs::read_to_string(&temp_path), fs::read_to_string(&type_path))
                    {
                        if let Ok(temp_millis) = temp_content.trim().parse::<i32>() {
                            let temp_celsius = temp_millis / 1000;
                            let label = type_content.trim().replace("_thermal", "");
                            temps.push((label, temp_celsius));
                        }
                    }
                }
            }
        }
    }

    // Read from hwmon (NVMe, fan controllers, etc.)
    // Skip hwmon devices that are duplicates of thermal zones (they end with "_thermal")
    let hwmon_dir = "/sys/class/hwmon";
    if let Ok(entries) = fs::read_dir(hwmon_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name_path = path.join("name");

            // Read the device name
            if let Ok(device_name) = fs::read_to_string(&name_path) {
                let device_name = device_name.trim().to_string();

                // Skip thermal zone duplicates (they're already read above)
                if device_name.ends_with("_thermal") {
                    continue;
                }

                // Look for temp inputs in this hwmon device
                if let Ok(hwmon_entries) = fs::read_dir(&path) {
                    for hwmon_entry in hwmon_entries.flatten() {
                        let hwmon_file = hwmon_entry.path();
                        if let Some(filename) = hwmon_file.file_name() {
                            let filename_str = filename.to_string_lossy();

                            // Match temp*_input files
                            if filename_str.starts_with("temp") && filename_str.ends_with("_input") {
                                // Try to read the corresponding label
                                let label_file = filename_str.replace("_input", "_label");
                                let label_path = path.join(&label_file);

                                let label = if let Ok(label_content) = fs::read_to_string(&label_path) {
                                    label_content.trim().to_string()
                                } else {
                                    // Use device name if no label available
                                    device_name.clone()
                                };

                                // Read the temperature
                                if let Ok(temp_content) = fs::read_to_string(&hwmon_file) {
                                    if let Ok(temp_millis) = temp_content.trim().parse::<i32>() {
                                        let temp_celsius = temp_millis / 1000;
                                        temps.push((label, temp_celsius));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    temps
}
