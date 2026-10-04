use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub id: String,
    pub timestamp_ms: i64,
    pub cpu_load_1: Option<f64>,
    pub mem_total_kb: Option<u64>,
    pub mem_available_kb: Option<u64>,
    pub mem_used_percent: Option<f64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_available_bytes: Option<u64>,
    pub disk_used_percent: Option<f64>,
    pub battery_percent: Option<u8>,
    pub battery_charging: Option<bool>,
}

pub fn sample_now(id: String, timestamp_ms: i64) -> Sample {
    let (cpu_load_1, mem_total_kb, mem_available_kb, mem_used_percent) = sample_cpu_mem();
    let (disk_total_bytes, disk_available_bytes, disk_used_percent) = sample_disk();
    let (battery_percent, battery_charging) = sample_battery();
    Sample {
        id,
        timestamp_ms,
        cpu_load_1,
        mem_total_kb,
        mem_available_kb,
        mem_used_percent,
        disk_total_bytes,
        disk_available_bytes,
        disk_used_percent,
        battery_percent,
        battery_charging,
    }
}

fn sample_cpu_mem() -> (Option<f64>, Option<u64>, Option<u64>, Option<f64>) {
    let cpu = fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());
    let meminfo = fs::read_to_string("/proc/meminfo").ok();
    let (total, avail) = if let Some(text) = meminfo {
        let mut total = None;
        let mut avail = None;
        for line in text.lines() {
            if line.starts_with("MemTotal:") {
                total = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
            } else if line.starts_with("MemAvailable:") {
                avail = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
            }
        }
        (total, avail)
    } else { (None, None) };
    let used_percent = match (total, avail) {
        (Some(t), Some(a)) if t > 0 => Some(((t - a) as f64 / t as f64) * 100.0),
        _ => None,
    };
    (cpu, total, avail, used_percent)
}

fn sample_disk() -> (Option<u64>, Option<u64>, Option<f64>) {
    // use statvfs for "/"
    let path = "/";
    let cstr = std::ffi::CString::new(path).unwrap();
    let mut stat: libc_statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { statvfs(cstr.as_ptr(), &mut stat as *mut _) };
    if ret != 0 { return (None, None, None); }
    let bsize = stat.f_bsize as u64;
    let total = stat.f_blocks as u64 * bsize;
    let avail = stat.f_bavail as u64 * bsize;
    let used_percent = if total > 0 { Some(((total - avail) as f64 / total as f64)*100.0) } else { None };
    (Some(total), Some(avail), used_percent)
}

#[repr(C)]
struct libc_statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    _pad: [u64; 6],
}
extern "C" { fn statvfs(path: *const std::os::raw::c_char, buf: *mut libc_statvfs) -> i32; }

fn sample_battery() -> (Option<u8>, Option<bool>) {
    // try /sys/class/power_supply/BAT*/capacity and status
    let base = "/sys/class/power_supply";
    let entries = fs::read_dir(base).ok();
    if let Some(dir) = entries {
        for entry in dir.flatten() {
            let p = entry.path();
            let cap_path = p.join("capacity");
            if cap_path.exists() {
                let cap = fs::read_to_string(&cap_path).ok().and_then(|s| s.trim().parse::<u8>().ok());
                let charging = fs::read_to_string(p.join("status")).ok().map(|s| s.trim().to_lowercase() == "charging");
                return (cap, charging);
            }
        }
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sample_does_not_panic() {
        let s = sample_now("1".into(), 123);
        assert_eq!(s.id, "1");
        // at least timestamp set, other fields optional
        assert_eq!(s.timestamp_ms, 123);
    }
}
