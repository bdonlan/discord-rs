use super::imports::*;
use std::marker::PhantomData;


/// A sink that applies incoming items to a state, updating a state, and propagating that state
/// to the upstream sink. If the upstream sink rejects the send, this sink will retry sending only
/// the most recent state version when the upstream unblocks.
///
/// This sink never returns AsyncSink::NotReady from start_send.
pub struct StateUpdateSink<State, Item, Err, Func, Upstream> {
    current_state: Option<State>,
    upstream: Upstream,
    transform: Func,
    phantom: PhantomData<(Item,Err)>
}

impl<State, Item, Err, Func, Upstream> StateUpdateSink<State, Item, Err, Func, Upstream> where
    Func: FnMut(Item)->Result<State, Err>,
    Upstream: Sink<SinkItem=State, SinkError=Err>
{
    pub fn new(upstream: Upstream, func: Func) -> Self {
        StateUpdateSink { current_state: None, upstream: upstream, transform: func, phantom: PhantomData }
    }
}

impl<State, Item, Err,
    Func: FnMut(Item)->Result<State, Err>,
    Upstream: Sink<SinkItem=State, SinkError=Err>>
        Sink for StateUpdateSink<State, Item, Err, Func, Upstream>
{
    type SinkItem = Item;
    type SinkError = Upstream::SinkError;

    fn start_send(&mut self, item: Item) -> StartSend<Self::SinkItem, Self::SinkError> {
        let new_state = (self.transform)(item)?;
        self.current_state = Some(new_state);

        return Ok(AsyncSink::Ready);
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        if let Some(state) = self.current_state.take() {
            match self.upstream.start_send(state)? {
                AsyncSink::Ready => {},
                AsyncSink::NotReady(state) => self.current_state = Some(state)
            }
        }

        // at this point pending_send is false
        self.upstream.poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        // Flush the last message before propagating the close
        if self.current_state.is_some() {
            if let Ok(Async::NotReady) = self.poll_complete() {
                return Ok(Async::NotReady);
            }
        }

       self.upstream.close()
    }
}