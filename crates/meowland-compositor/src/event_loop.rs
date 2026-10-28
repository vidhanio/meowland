//! Project compositor event sources onto the host's single-owner loop data.

use std::{fmt, io, rc::Rc};

use calloop::{
    EventIterator, EventSource, LoopHandle, Poll, PostAction, Readiness, RegistrationToken, Token,
    TokenFactory,
};

use super::State;

trait Source {
    fn dispatch(
        &mut self,
        readiness: Readiness,
        token: Token,
        state: &mut State,
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

impl<S, F> Source for Callback<S, F>
where
    S: EventSource,
    F: FnMut(S::Event, &mut S::Metadata, &mut State) -> S::Ret,
{
    fn dispatch(
        &mut self,
        readiness: Readiness,
        token: Token,
        state: &mut State,
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

struct Projected(Box<dyn Source>);

impl EventSource for Projected {
    type Error = io::Error;
    type Event = (Readiness, Token);
    type Metadata = Box<dyn Source>;
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

type Register = dyn Fn(Projected) -> io::Result<RegistrationToken>;

/// A compositor projection of a host event loop, without shared mutable state.
#[derive(Clone)]
pub struct CompositorHandle {
    register: Rc<Register>,
    remove: Rc<dyn Fn(RegistrationToken)>,
}

impl fmt::Debug for CompositorHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompositorHandle").finish_non_exhaustive()
    }
}

impl CompositorHandle {
    /// Register compositor callbacks against a field of the host's loop data.
    pub fn new<D: 'static>(
        handle: LoopHandle<'static, D>,
        project: fn(&mut D) -> &mut State,
    ) -> Self {
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

    pub(crate) fn insert_source<S, F>(
        &self,
        source: S,
        callback: F,
    ) -> io::Result<RegistrationToken>
    where
        S: EventSource + 'static,
        F: FnMut(S::Event, &mut S::Metadata, &mut State) -> S::Ret + 'static,
    {
        (self.register)(Projected(Box::new(Callback { source, callback })))
    }

    pub(crate) fn remove(&self, token: RegistrationToken) {
        (self.remove)(token);
    }
}
