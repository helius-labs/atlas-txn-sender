use std::time::SystemTime;

pub fn unix_to_time(unix_timestamp: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(unix_timestamp as u64)
}

pub fn elapsed(start: SystemTime) -> u64 {
    start.elapsed().unwrap().as_millis() as u64
}

pub fn round_to_nearest_10000(num: u32) -> u32 {
    let divisor = 10_000;
    let rounded = (num as f32 / divisor as f32).round() * divisor as f32;
    rounded as u32
}
