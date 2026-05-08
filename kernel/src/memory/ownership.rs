//! Per-task physical memory ownership tracking — §10.
//!
//! Every frame allocated for a task is recorded here. On task death the kernel
//! walks this set, issues a TLB shootdown (§10.5), then returns all frames to
//! the allocator. This is how memory limits are enforced at the per-task level.

use crate::memory::frame::Frame;
use crate::task::task::TaskId;

/// Tracks frames owned by one task.
pub struct TaskMemoryOwner {
    task_id: TaskId,
    frames: FrameSet,
    limit_bytes: u64,
    used_bytes: u64,
}

impl TaskMemoryOwner {
    pub fn new(task_id: TaskId, limit_bytes: u64) -> Self {
        Self {
            task_id,
            frames: FrameSet::new(),
            limit_bytes,
            used_bytes: 0,
        }
    }

    /// Attempt to record a new frame allocation for this task.
    /// Returns `Err(AllocDenied)` if adding `frame_count` would exceed the limit.
    pub fn track_alloc(&mut self, frame_count: u64) -> Result<(), AllocError> {
        let requested = frame_count * crate::memory::frame::FRAME_SIZE;
        if self.used_bytes + requested > self.limit_bytes {
            return Err(AllocError::AllocDenied);
        }
        self.used_bytes += requested;
        Ok(())
    }

    /// Record a frame as owned by this task.
    pub fn add_frame(&mut self, frame: Frame) {
        self.frames.insert(frame);
    }

    /// Reclaim all frames owned by this task. Called on task death.
    /// Caller must issue a TLB shootdown before calling this.
    pub fn reclaim_all(self) -> impl Iterator<Item = Frame> {
        self.frames.into_iter()
    }

    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
    }
}

#[derive(Debug)]
pub enum AllocError {
    AllocDenied,
}

// Placeholder: a real implementation would use a fixed-capacity array or
// a slab-allocated linked list — no heap allocator exists yet.
struct FrameSet {
    frames: [Option<Frame>; 4096],
    len: usize,
}

impl FrameSet {
    fn new() -> Self {
        Self { frames: [None; 4096], len: 0 }
    }

    fn insert(&mut self, frame: Frame) {
        assert!(self.len < self.frames.len(), "FrameSet capacity exceeded");
        self.frames[self.len] = Some(frame);
        self.len += 1;
    }

    fn into_iter(self) -> impl Iterator<Item = Frame> {
        self.frames.into_iter().flatten()
    }
}
