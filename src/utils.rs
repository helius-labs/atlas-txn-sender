use std::time::SystemTime;

pub fn unix_to_time(unix_timestamp: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(unix_timestamp as u64)
}

pub fn elapsed(start: SystemTime) -> u64 {
    start.elapsed().unwrap().as_millis() as u64
}
