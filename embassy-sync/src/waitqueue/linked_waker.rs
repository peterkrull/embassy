//! Intrusive linked-list based Waker queue

use crate::blocking_mutex::{raw::RawMutex, Mutex};
use core::{
    cell::RefCell,
    future::Future,
    ptr::null_mut,
    sync::atomic::{AtomicPtr, Ordering},
    task::{Poll, Waker},
};

/// A queue of wakers, implemented as a linked list
pub struct LinkedQueue {
    head: AtomicPtr<LinkedWaker>,
}

impl LinkedQueue {
    /// Create a new `WakerQueue`
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(null_mut()),
        }
    }

    /// Wake the entire queue
    pub fn wake(&mut self) {
        critical_section::with(|_| {
            let mut link = self.head.swap(null_mut(), Ordering::AcqRel);
            while !link.is_null() {
                // Safety: We are in a CS, the link is not null inside the loop
                unsafe { (&mut *link).waker.take().map(|w| w.wake()) };
                link = unsafe { &mut *link }.next;
            }
        })
    }
}

#[derive(Clone, Copy)]
enum PrevConnection {
    Queue(*mut LinkedQueue),
    Link(*mut LinkedWaker),
}

/// A link in the waker queue
pub struct LinkedWaker {
    waker: Option<Waker>,
    prev: PrevConnection,
    next: *mut LinkedWaker,
}

unsafe impl Sync for LinkedWaker {}

/// A guard for a `LinkedWaker` that removes the link from the queue when dropped
pub struct LinkedWakerGuard<'a> {
    link: &'a mut LinkedWaker,
}

impl LinkedWaker {
    /// Create a new `LinkedWaker`
    pub fn new() -> Self {
        Self {
            waker: None,
            prev: PrevConnection::Queue(null_mut()),
            next: null_mut(),
        }
    }

    /// Create a new `LinkedWakerGuard` for the `LinkedWaker` The intent is for this
    /// to be owned by a type implementing the `Future` trait, such that if the
    /// future is canceled, the `LinkedWaker` is automatically removed from the queue.
    ///
    /// # Safety
    ///
    /// The user must ensure that the guard is always dropped using its destuctor.
    /// So that means no `mem::forget` or `std::mem::ManuallyDrop` should be used.
    /// This can be done as stated above using a type implementing `Future`.
    pub unsafe fn guard(&mut self) -> LinkedWakerGuard {
        LinkedWakerGuard { link: self }
    }

    /// Insert a waker into the `queue`
    ///
    /// # Safety
    ///
    /// Inserting a link into a queue, and then dropping or moving (in memory) that link
    /// without removing it from the queue will cause UB. Prefer to use a guard/wrapper
    /// that removes the link from the queue when dropped.
    unsafe fn insert(&mut self, queue: &LinkedQueue, waker: &Waker) {
        // If a waker is already registered, we can skip the insertion
        if self.waker.is_some() {
            return;
        }

        critical_section::with(|_| {
            self.waker = Some(waker.clone());
            self.prev = PrevConnection::Queue(queue as *const _ as *mut LinkedQueue);
            self.next = queue.head.swap(self, Ordering::AcqRel);

            if !self.next.is_null() {
                (&mut *self.next).prev = PrevConnection::Link(self);
            }
        })
    }

    unsafe fn remove(&mut self) {
        // If the waker has already been consumed, it means that the entire chain
        // has been woken up, so we can skip the removal process.
        if self.waker.take().is_none() {
            return;
        }

        critical_section::with(|_| {
            // Link next to previous (unless next is null)
            if !self.next.is_null() {
                unsafe { &mut *self.next }.prev = self.prev;
            }

            // Link previous to next (even is next is null)
            match self.prev {
                PrevConnection::Queue(queue) => {
                    unsafe { &mut *queue }.head.store(self.next, Ordering::Release);
                }
                PrevConnection::Link(member) => {
                    unsafe { &mut *member }.next = self.next;
                }
            }
        })
    }
}

impl LinkedWakerGuard<'_> {
    /// Insert a waker into the `queue`. If the LinkedWaker already has
    /// a waker registered this function will do nothing.
    pub fn insert(&mut self, queue: &LinkedQueue, waker: &Waker) {
        // Safety: The link is removed if the guard is dropped
        unsafe { self.link.insert(queue, waker) };
    }

    /// Remove the waker from the `queue`, linking together the previous and next links
    pub fn remove(&mut self) {
        unsafe { self.link.remove() };
    }
}

impl Drop for LinkedWakerGuard<'_> {
    fn drop(&mut self) {
        self.remove();
    }
}

/// ---------------------------------------------------------------------------
/// ----------------------- Example use case below ----------------------------
/// ---------------------------------------------------------------------------

/// A `Watch` channel that can be used to send data to a receivers
pub struct LinkWatch<M: RawMutex, T: Clone> {
    mutex: Mutex<M, RefCell<WatchState<T>>>,
}

struct WatchState<T: Clone> {
    data: Option<T>,
    current_id: u64,
    queue: LinkedQueue,
}

/// A receiver for a `Watch` channel
pub struct Receiver<'a, M: RawMutex, T: Clone> {
    inner: &'a LinkWatch<M, T>,
    link: LinkedWaker,
    id: u64,
}

/// A sender for a `Watch` channel
pub struct Sender<'a, M: RawMutex, T: Clone> {
    inner: &'a LinkWatch<M, T>,
}

impl<M: RawMutex, T: Clone> LinkWatch<M, T> {
    /// Create a new `Watch` channel
    pub const fn new() -> Self {
        Self {
            mutex: Mutex::new(RefCell::new(WatchState {
                data: None,
                current_id: 0,
                queue: LinkedQueue::new(),
            })),
        }
    }

    /// Create a new receiver for the `Watch` channel
    pub fn receiver(&self) -> Receiver<M, T> {
        Receiver {
            inner: self,
            link: LinkedWaker::new(),
            id: 0,
        }
    }

    /// Create a new sender for the `Watch` channel
    pub fn sender(&self) -> Sender<M, T> {
        Sender { inner: self }
    }
}

/// A future that resolves when the data in the `Watch` channel changes
#[must_use]
pub struct ChangedFuture<'b, M: RawMutex, T: Clone> {
    inner: &'b LinkWatch<M, T>,
    guard: LinkedWakerGuard<'b>,
    id: &'b mut u64,
}

impl<M: RawMutex, T: Clone> Future for ChangedFuture<'_, M, T> {
    type Output = T;

    fn poll(mut self: core::pin::Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> Poll<Self::Output> {
        self.inner.mutex.lock(|state| {
            let s = state.borrow();
            match (&s.data, s.current_id > *self.id) {
                (Some(data), true) => {
                    *self.id = s.current_id;
                    Poll::Ready(data.clone())
                }
                _ => {
                    self.guard.insert(&s.queue, cx.waker());
                    Poll::Pending
                }
            }
        })
    }
}

impl<M: RawMutex, T: Clone> Receiver<'_, M, T> {
    /// Wait for the data in the `Watch` channel to change
    pub async fn changed(&mut self) -> T {
        ChangedFuture {
            inner: self.inner,
            // Safety: It is not possible to drop the `ChangedFuture` while
            // the `LinkedWaker` is still in the queue. Canceling the future will
            // remove the `LinkedWaker` from the queue by dropping the guard.
            guard: unsafe { self.link.guard() },
            id: &mut self.id,
        }
        .await
    }
}

impl<M: RawMutex, T: Clone> Sender<'_, M, T> {
    /// Send data to the `Watch` channel
    pub fn send(&self, data: T) {
        self.inner.mutex.lock(|state| {
            let mut s = state.borrow_mut();
            s.data = Some(data);
            s.current_id += 1;
            s.queue.wake();
        })
    }
}
