use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

/// Global cache for open file descriptors
/// Maps file path to (File, buffer)
static FILE_CACHE: Mutex<Option<HashMap<String, File>>> = Mutex::new(None);

/// Read a file using cached file descriptor
/// This avoids repeated open/close syscalls for frequently-read files
pub fn read_cached_file(path: &str) -> std::io::Result<String> {
    let mut cache = FILE_CACHE.lock().unwrap();

    // Initialize cache if needed
    if cache.is_none() {
        *cache = Some(HashMap::new());
    }

    let cache_map = cache.as_mut().unwrap();

    // Get or create file descriptor
    let file = if let Some(f) = cache_map.get_mut(path) {
        f
    } else {
        // Open new file and cache it
        let f = File::open(path)?;
        cache_map.insert(path.to_string(), f);
        cache_map.get_mut(path).unwrap()
    };

    // Seek to beginning
    file.seek(SeekFrom::Start(0))?;

    // Read entire file
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    Ok(contents)
}

/// Read a file and parse as u32
pub fn read_cached_u32(path: &str) -> Option<u32> {
    read_cached_file(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Read a file and parse as u64
#[allow(dead_code)]
pub fn read_cached_u64(path: &str) -> Option<u64> {
    read_cached_file(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Read a file and parse as i32
pub fn read_cached_i32(path: &str) -> Option<i32> {
    read_cached_file(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Clear the file descriptor cache (useful for cleanup)
#[allow(dead_code)]
pub fn clear_cache() {
    let mut cache = FILE_CACHE.lock().unwrap();
    *cache = None;
}
