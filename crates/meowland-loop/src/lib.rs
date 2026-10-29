//! Project event sources onto a host loop whose data owns the projected state.
//!
//! A host loop stores one server type, but libraries often need to register
//! sources against a smaller state they do not own. A [`Handle`] captures the
//! projection once, and every registered callback receives only that state.

use std::{fmt, io, rc::Rc};

use calloop::{
    EventIterator, EventSource, LoopHandle, Poll, PostAction, Readiness, RegistrationToken, Token,
    TokenFactory,
};

trait Source<T> {
    fn dispatch(
        &mut self,
        readiness: Readiness,
        token: Token,
        state: &mut T,
    ) -> io::Result<PostAction>;
    fn register(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()>;
    fn reregister(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()>;
    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()>;
    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>>;
    fn before_handle_events(&mut self, events: EventIterator<'_>);
}

struct Callback<S, F> {
    source: S,
    callback: F,
}

impl<S, F, T> Source<T> for Callback<S, F>
where
    S: EventSource,
    F: FnMut(S::Event, &mut S::Metadata, &mut T) -> S::Ret,
{
    fn dispatch(
        &mut self,
        readiness: Readiness,
        token: Token,
        state: &mut T,
    ) -> io::Result<PostAction> {
        self.source
            .process_events(readiness, token, |event, metadata| {
                (self.callback)(event, metadata, state)
            })
            .map_err(|error| io::Error::other(error.into()))
    }

    fn register(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()> {
        self.source.register(poll, tokens)
    }

    fn reregister(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()> {
        self.source.reregister(poll, tokens)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.source.unregister(poll)
    }

    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        self.source.before_sleep()
    }

    fn before_handle_events(&mut self, events: EventIterator<'_>) {
        self.source.before_handle_events(events);
    }
}

struct Projected<T>(Box<dyn Source<T>>);

impl<T: 'static> EventSource for Projected<T> {
    type Error = io::Error;
    type Event = (Readiness, Token);
    type Metadata = Box<dyn Source<T>>;
    type Ret = io::Result<PostAction>;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> io::Result<PostAction>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        callback((readiness, token), &mut self.0)
    }

    fn register(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()> {
        self.0.register(poll, tokens)
    }

    fn reregister(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()> {
        self.0.reregister(poll, tokens)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.0.unregister(poll)
    }

    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        self.0.before_sleep()
    }

    fn before_handle_events(&mut self, events: EventIterator<'_>) {
        self.0.before_handle_events(events);
    }
}

type Register<T> = dyn Fn(Projected<T>) -> io::Result<RegistrationToken>;

/// A projection of a host event loop onto one owned state type.
///
/// The handle registers callbacks whose third argument is the projected state
/// rather than the host loop's data, and removes those registrations again
/// through [`Sources`] or [`Handle::remove`].
pub struct Handle<T> {
    register: Rc<Register<T>>,
    remove: Rc<dyn Fn(RegistrationToken)>,
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self {
            register: Rc::clone(&self.register),
            remove: Rc::clone(&self.remove),
        }
    }
}

impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Handle").finish_non_exhaustive()
    }
}

impl<T: 'static> Handle<T> {
    /// Project the host loop's data onto a field that owns `T`.
    pub fn new<D: 'static>(handle: LoopHandle<'static, D>, project: fn(&mut D) -> &mut T) -> Self {
        let removing = handle.clone();
        Self {
            register: Rc::new(move |source| {
                handle
                    .insert_source(source, move |(readiness, token), source, data| {
                        source.dispatch(readiness, token, project(data))
                    })
                    .map_err(|error| io::Error::from(error.error))
            }),
            remove: Rc::new(move |token| removing.remove(token)),
        }
    }

    /// Register an event source whose callback receives the projected state.
    ///
    /// # Errors
    /// Returns the host loop's insertion error.
    pub fn insert_source<S, F>(&self, source: S, callback: F) -> io::Result<RegistrationToken>
    where
        S: EventSource + 'static,
        F: FnMut(S::Event, &mut S::Metadata, &mut T) -> S::Ret + 'static,
    {
        (self.register)(Projected(Box::new(Callback { source, callback })))
    }
}

impl<T> Handle<T> {
    /// Remove a source registered through this handle.
    pub fn remove(&self, token: RegistrationToken) {
        (self.remove)(token);
    }
}

/// Operation-owned readiness and deadline registrations.
///
/// Dropping the set removes both registrations from the host loop.
pub struct Sources<T> {
    handle: Handle<T>,
    /// Registration of the operation's data readiness.
    pub io: Option<RegistrationToken>,
    /// Registration of the operation's deadline.
    pub timer: Option<RegistrationToken>,
}

impl<T> Sources<T> {
    /// Track registrations for one operation on the given projection.
    pub fn new(handle: &Handle<T>) -> Self {
        Self {
            handle: handle.clone(),
            io: None,
            timer: None,
        }
    }
}

impl<T> fmt::Debug for Sources<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sources")
            .field("io", &self.io)
            .field("timer", &self.timer)
            .finish_non_exhaustive()
    }
}

impl<T> Drop for Sources<T> {
    fn drop(&mut self) {
        for token in [self.io.take(), self.timer.take()].into_iter().flatten() {
            self.handle.remove(token);
        }
    }
}
