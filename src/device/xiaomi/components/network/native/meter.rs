use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct BandwidthMeter {
    window: Duration,
    write_events: Arc<Mutex<VecDeque<(Instant, u64)>>>,
    read_events: Arc<Mutex<VecDeque<(Instant, u64)>>>,
}

impl BandwidthMeter {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            write_events: Arc::new(Mutex::new(VecDeque::new())),
            read_events: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn evict_old(&self, queue: &mut VecDeque<(Instant, u64)>, now: Instant) {
        while let Some(&(timestamp, _)) = queue.front() {
            if now.duration_since(timestamp) > self.window {
                queue.pop_front();
            } else {
                break;
            }
        }
    }

    fn push_event(&self, queue: &Arc<Mutex<VecDeque<(Instant, u64)>>>, length: usize) {
        let now = Instant::now();
        let mut guard = queue.lock().unwrap();
        guard.push_back((now, length as u64));
        self.evict_old(&mut guard, now);
    }

    pub fn add_written(&self, length: usize) {
        self.push_event(&self.write_events, length);
    }

    pub fn add_read(&self, length: usize) {
        self.push_event(&self.read_events, length);
    }

    fn speed_inner(&self, queue: &Arc<Mutex<VecDeque<(Instant, u64)>>>) -> f64 {
        let now = Instant::now();
        let mut guard = queue.lock().unwrap();
        self.evict_old(&mut guard, now);
        let total: u64 = guard.iter().map(|(_, bytes)| *bytes).sum();
        if total == 0 {
            0.0
        } else if let Some(&(first, _)) = guard.front() {
            let elapsed = now
                .checked_duration_since(first)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.001)
                .max(0.001);
            total as f64 / elapsed
        } else {
            0.0
        }
    }

    pub fn write_speed(&self) -> f64 {
        self.speed_inner(&self.write_events)
    }

    pub fn read_speed(&self) -> f64 {
        self.speed_inner(&self.read_events)
    }
}
