//! Wait queues: Linux's wait-queue model.
//!
//! Every waitable object -- a pipe, a socket, an eventfd, an epoll set, the
//! terminal, a process's children -- owns a [`Queue`]. A thread that waits
//! registers its own wait slot on the queue of every object it waits for,
//! checks readiness, and parks on its slot; whoever changes the object's
//! readiness calls [`Queue::notify`], which wakes the slots registered on
//! that queue and nothing else. A thread parked on a pipe is not woken by a
//! syscall in some unrelated process, as it was by the one global wake word.
//!
//! The queues themselves live with the host, because a waiter and a notifier
//! can be in different processes (different memories): in the page they are
//! in the global kernel module's shared memory (`hosts/kernel`, `k_wq_*`),
//! which every Worker reaches. This crate only names them, by a number, and
//! asks the host through [`Hooks`]. A host with no hooks (native, the unit
//! tests) gets queue 0 for everything and every call is a no-op: it wakes
//! its waiters its own way (`hosts/native/src/ready.rs`).
//!
//! A queue can also be *chained* onto another ([`chain`]): an epoll set's
//! queue is chained onto the queue of every descriptor it watches, so a
//! change on a watched descriptor notifies the set, which notifies the
//! threads waiting in `epoll_wait` on it.

use std::sync::OnceLock;

/// What a host supplies.
#[derive(Clone, Copy)]
pub struct Hooks {
    /// A new, empty queue; never 0.
    pub new: fn() -> u32,
    /// The object is gone. Its waiters are notified first; the number may be
    /// reused, and a stale notify to it only wakes someone who re-checks.
    pub free: fn(u32),
    /// Wake every slot registered on the queue, and every queue chained to it.
    pub notify: fn(u32),
    /// Notify `to` whenever `from` is notified.
    pub chain: fn(from: u32, to: u32),
    /// Undo one [`Hooks::chain`].
    pub unchain: fn(from: u32, to: u32),
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// Install the host's queues. Once per module instance; later calls are
/// ignored (every Worker of a process runs the same setup).
pub fn set_hooks(h: Hooks) {
    let _ = HOOKS.set(h);
}

/// Whether a host has installed queues.
pub fn present() -> bool {
    HOOKS.get().is_some()
}

/// Notify a queue by number. 0 is no queue.
pub fn notify(id: u32) {
    if id != 0 {
        if let Some(h) = HOOKS.get() {
            (h.notify)(id);
        }
    }
}

/// Chain `from` onto `to`: notifying `from` notifies `to`.
pub fn chain(from: u32, to: u32) {
    if from != 0 && to != 0 {
        if let Some(h) = HOOKS.get() {
            (h.chain)(from, to);
        }
    }
}

/// Undo a [`chain`].
pub fn unchain(from: u32, to: u32) {
    if from != 0 && to != 0 {
        if let Some(h) = HOOKS.get() {
            (h.unchain)(from, to);
        }
    }
}

/// One object's wait queue, freed with the object. It also counts its
/// wakeups ([`Queue::wakes`]), with a host's queues or without.
#[derive(Debug)]
pub struct Queue(u32, std::sync::atomic::AtomicU64);

impl Queue {
    pub fn new() -> Queue {
        Queue(HOOKS.get().map_or(0, |h| (h.new)()), std::sync::atomic::AtomicU64::new(0))
    }

    /// How many times the object has been notified: an edge-triggered epoll
    /// watch (`EPOLLET`) reports again when this has moved since it last
    /// reported, as Linux's does at each wakeup of the file.
    pub fn wakes(&self) -> u64 {
        self.1.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The queue's number, for a waiter to register on; 0 when the host has
    /// no queues.
    pub fn id(&self) -> u32 {
        self.0
    }

    /// The object's readiness may have changed: wake whoever waits on it.
    pub fn notify(&self) {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        notify(self.0);
    }
}

impl Default for Queue {
    fn default() -> Queue {
        Queue::new()
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        if self.0 != 0 {
            if let Some(h) = HOOKS.get() {
                (h.free)(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_hooks_everything_is_queue_zero() {
        // The unit tests install no hooks, as natively.
        let q = Queue::new();
        assert_eq!(q.id(), 0);
        q.notify();
        notify(0);
        chain(0, 1);
    }
}
