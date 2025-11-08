# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**rktop** - Rockchip System Monitor (Rust implementation)

A Rust-based system monitoring tool specifically designed for Rockchip SoC devices (RK3588, RK3399, etc.). It provides real-time visualization of:
- CPU usage and frequencies per core
- GPU (Mali) utilization and frequency
- NPU (Neural Processing Unit) load across cores
- RGA (Rockchip Graphics Accelerator) scheduler load
- Memory (RAM, Swap, ZRAM) usage
- Disk I/O and network traffic rates
- Temperature sensors
- Top processes by CPU/memory usage (sortable)
- Driver versions (NPU, RGA, RKNN, RKLLM)
- System stats (uptime, load average, governor, TCP connections)

The tool uses the Ratatui library for terminal UI rendering and requires root privileges to access hardware-specific sysfs/debugfs interfaces.

## Development Environment

**Remote Testing System:**
- Orange Pi 5 Max (RK3588 SoC) at 10.1.1.224
- User: alex
- Cargo is installed in `~/.cargo/` directory
- To build on remote: `ssh alex@10.1.1.224 'cd rk_top && ~/.cargo/bin/cargo build --release'`

**Deployment:**
- Build locally on macOS (cross-compilation target: aarch64-unknown-linux-gnu)
- Deploy binary: `scp target/release/rktop alex@10.1.1.224:~/`
- Install: `ssh alex@10.1.1.224 'sudo mv rktop /usr/local/bin/ && sudo chmod +x /usr/local/bin/rktop'`

## Running the Application

```bash
# Must run with root permissions
sudo rktop
```

**Key controls:**
- Press 'q', 'Q', or 'Esc' to quit
- Press 'c' to toggle CPU sort (ascending/descending)
- Press 'm' to toggle Memory sort (ascending/descending)
- Press 'p' to toggle PID sort (ascending/descending)
- Press 'n' to toggle Name sort (ascending/descending)
- The display refreshes with different intervals (CPU/memory: 1s, network/disk: 2s, processes: 3s, stats: 5s)

## Architecture

### Multi-Module Design
The application is split across multiple source files:

**src/main.rs** - Main application, TUI rendering, event loop, state management
- `AppState` struct - Caches static info and I/O rates to reduce file I/O
- Event loop with multiple refresh intervals (1s/2s/3s/5s)
- Ratatui rendering functions for each panel
- Process sorting with 8 modes (CPU/Memory/PID/Name × Asc/Desc)

**src/hardware.rs** - Rockchip-specific hardware detection and monitoring
- `get_gpu_usage()` - Reads Mali GPU utilization from `/sys/kernel/debug/mali0/dvfs_utilization`
- `get_cpu_frequencies()` - Reads per-core CPU frequencies from sysfs
- `get_gpu_frequency()` - Tries multiple paths for GPU devfreq
- `get_npu_frequency()` / `get_npu_load()` - NPU frequency and per-core load percentages
- `get_rga_load()` - Parses RGA scheduler load from debugfs (with scheduler indices)
- `get_thermal()` - Temperature sensors from thermal_zone and hwmon
- `get_board_name()` - Full board name from device tree
- `get_rk_model()` - Detects SoC model (RK3588, etc.) from device tree
- `get_rga_version()` / `get_npu_driver_version()` - Driver versions from debugfs
- `get_librknnrt_version()` / `get_librkllmrt_version()` - Parse library versions using `strings` command

**src/sysinfo_ext.rs** - Extended system information
- `get_top_processes()` - Process enumeration with sorting
- `get_process_user()` - UID-to-username mapping with global cache (Mutex-wrapped HashMap)
- `get_zram_stats()` - ZRAM compression statistics
- `get_tcp_connections()` - Count external TCP connections (excluding localhost)

### Key Design Patterns

**Caching for Performance:** `AppState` caches static information at startup (board name, versions, hardware availability) to avoid repeated file I/O and command execution. Hardware availability checks (`has_gpu`, `has_npu`, `has_rga`) prevent unnecessary sysfs reads every frame.

**UID-to-Username Caching:** Global `Mutex<HashMap>` in `sysinfo_ext.rs` caches UID-to-username mappings to eliminate process spawning (previously 260+ spawns/sec, now 0-2/sec).

**Throttled Refresh Intervals:** Different data types refresh at different rates:
- CPU/Memory: 1 second
- Network/Disk I/O: 2 seconds
- Processes: 3 seconds
- Stats (uptime, load, governor, TCP): 5 seconds

**Graceful Hardware Detection:** Most hardware reading functions return `Option<T>` or empty values when devices aren't available (e.g., NPU, RGA may not exist on all boards). UI panels conditionally render based on cached availability checks.

**Rockchip-Specific Paths:** The tool is tightly coupled to Rockchip kernel interfaces:
- `/sys/kernel/debug/mali0/`, `/sys/kernel/debug/rknpu/`, `/sys/kernel/debug/rkrga/`
- `/sys/devices/platform/fb000000.gpu-panthor/devfreq/`
- `/sys/class/devfreq/fdab0000.npu/`
- `/proc/device-tree/model` - Board name and SoC detection

## Dependencies

**Rust Crates:**
- `ratatui` - Terminal UI framework
- `crossterm` - Terminal manipulation and event handling
- `sysinfo` - System and process information
- `regex` - Regular expression parsing for hardware data

**System Requirements:**
- Root/sudo access (required for debugfs reads and process enumeration)
- Rockchip SoC hardware (code expects Mali GPU, potentially NPU/RGA)
- Linux with sysfs/debugfs mounted

## Testing

This project does not include automated tests. Manual testing requires:
1. Running on actual Rockchip hardware (Orange Pi 5 Max with RK3588)
2. Verifying hardware detection for available components
3. Checking UI rendering in different terminal sizes
4. Testing keyboard input (q/c/m/p/n keys)
5. Profiling with `strace` to verify low overhead

## Performance Notes

**CPU Usage:** The tool is optimized to use <1% CPU through:
- Caching static information at startup
- UID-to-username caching to eliminate process spawns
- Throttled refresh intervals (1s/2s/3s/5s)
- Hardware availability checks to skip unavailable devices

**Profiling:** Use `strace -c` to monitor system calls and verify low overhead:
```bash
sudo strace -c -p $(pgrep rktop)
```

## Important Notes

**Hardware-Specific:** This tool will partially fail on non-Rockchip systems. GPU/NPU/RGA panels will show "not available" but CPU/memory/process monitoring should still work via sysinfo.

**Debugfs Access:** Several features require `/sys/kernel/debug/` to be mounted and accessible by root. Some kernel configurations may restrict debugfs access.

**Library Version Detection:** The `get_librknnrt_version()` and `get_librkllmrt_version()` functions use shell commands (`strings /usr/lib/*.so | grep`) which may fail if libraries are in different paths or stripped.

**RGA Hardware:** The RK3588 has 3 RGA schedulers: 2× RGA3 cores (rga3_0, rga3_1) + 1× RGA2 core (rga2_2). The display shows each scheduler with its index.
