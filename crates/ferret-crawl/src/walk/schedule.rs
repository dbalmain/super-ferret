//! Sharing a walk's directory jobs between workers: each keeps a local LIFO
//! stack and hands its oldest job to an idle worker, and open directory
//! descriptors in queued jobs stay within [`MAX_OPEN_JOBS`].

use super::*;

const MAX_OPEN_JOBS: usize = 128;

pub(super) struct Shared<D> {
    jobs: Mutex<Vec<Job<D>>>,
    ready: Condvar,
    idle: AtomicUsize,
    outstanding: AtomicUsize,
    open_jobs: AtomicUsize,
    cancelled: AtomicBool,
}

impl<D> Shared<D> {
    pub(super) fn new(root: Job<D>) -> Self {
        Self {
            jobs: Mutex::new(vec![root]),
            ready: Condvar::new(),
            idle: AtomicUsize::new(0),
            outstanding: AtomicUsize::new(1),
            open_jobs: AtomicUsize::new(1),
            cancelled: AtomicBool::new(false),
        }
    }

    fn take(&self) -> Option<Job<D>> {
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return None;
            }
            if let Some(job) = jobs.pop() {
                self.release_open(&job);
                return Some(job);
            }
            if self.outstanding.load(Ordering::Acquire) == 0 {
                return None;
            }
            // Mark idle while holding the queue lock, before wait releases it.
            self.idle.fetch_add(1, Ordering::SeqCst);
            while jobs.is_empty()
                && !self.cancelled.load(Ordering::Acquire)
                && self.outstanding.load(Ordering::Acquire) != 0
            {
                jobs = self
                    .ready
                    .wait(jobs)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            self.idle.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn reserve_or_spill(&self, parent: &mut Job<D>) {
        let reserved = self
            .open_jobs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                (open < MAX_OPEN_JOBS).then_some(open + 1)
            });
        if reserved.is_err() {
            parent.dir.take();
        }
    }

    fn release_open(&self, job: &Job<D>) {
        if job.dir.is_some() {
            self.open_jobs.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn share_oldest(&self, local: &mut VecDeque<Job<D>>) {
        if local.is_empty() || self.idle.load(Ordering::SeqCst) == 0 {
            return;
        }
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Nearest the root is likely to expose the most independent work.
        let Some(oldest) = local.pop_front() else {
            return;
        };
        jobs.push(oldest);
        if self.idle.load(Ordering::SeqCst) != 0 {
            self.ready.notify_one();
        }
    }

    fn finish_one(&self) {
        if self.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Synchronize with a worker about to wait, or it can miss this
            // wake.
            let _jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.ready.notify_all();
        }
    }
}

pub(super) fn run_guarded<V: EventVisitor>(walker: Walker<'_, V>, shared: &Shared<V::Dir>) -> V {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_worker(walker, shared))) {
        Ok(visitor) => visitor,
        Err(panic) => {
            shared.cancelled.store(true, Ordering::Release);
            let _jobs = shared
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.ready.notify_all();
            std::panic::resume_unwind(panic);
        }
    }
}

pub(super) fn run_worker<V: EventVisitor>(mut walker: Walker<'_, V>, shared: &Shared<V::Dir>) -> V {
    walker.visit.worker_started();
    let mut local = VecDeque::new();
    let Some(mut current) = shared.take() else {
        return walker.visit;
    };
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return walker.visit;
        }
        match walker.process(current) {
            Some((mut parent, child)) => {
                if parent.next != parent.children.entries.len() {
                    shared.outstanding.fetch_add(1, Ordering::AcqRel);
                    shared.reserve_or_spill(&mut parent);
                    local.push_back(parent);
                    shared.share_oldest(&mut local);
                }
                current = child;
            }
            None => {
                shared.finish_one();
                shared.share_oldest(&mut local);
                if let Some(job) = local.pop_back() {
                    shared.release_open(&job);
                    current = job;
                } else if let Some(job) = shared.take() {
                    current = job;
                } else {
                    return walker.visit;
                }
            }
        }
    }
}
