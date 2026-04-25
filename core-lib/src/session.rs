use async_std::channel::{self, Receiver, Sender};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::Frame;

pub type FrameTx = Sender<Frame>;
pub type FrameRx = Receiver<Frame>;
pub type DataTx = Sender<Vec<u8>>;
pub type DataRx = Receiver<Vec<u8>>;

/// Create a bounded frame channel for the mux writer loop.
pub fn frame_channel(capacity: usize) -> (FrameTx, FrameRx) {
    channel::bounded(capacity)
}

/// Routes incoming DATA/CLOSE frames to the correct per-stream channel.
///
/// Internally uses a `std::sync::Mutex` (not async) because HashMap operations
/// are instantaneous — holding the lock across an `.await` is never required.
#[derive(Clone)]
pub struct StreamRegistry {
    inner: Arc<Mutex<HashMap<u32, DataTx>>>,
}

impl Default for StreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamRegistry {
    pub fn new() -> Self {
        StreamRegistry {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a new stream and return the receiving end of its data channel.
    pub fn register(&self, id: u32) -> DataRx {
        let (tx, rx) = channel::bounded::<Vec<u8>>(32);
        self.inner.lock().unwrap().insert(id, tx);
        rx
    }

    /// Route a DATA payload to the stream's receiver.
    ///
    /// Blocks (yields) when the channel is full, providing back-pressure that
    /// propagates to the WebSocket reader and ultimately to the TCP receive
    /// window.  The Mutex is released before the await so it is never held
    /// across a yield point.
    pub async fn route(&self, id: u32, data: Vec<u8>) {
        let tx = self.inner.lock().unwrap().get(&id).cloned();
        if let Some(tx) = tx {
            let _ = tx.send(data).await;
        }
    }

    /// Unregister a stream (drops the sender, which closes the DataRx).
    pub fn close(&self, id: u32) {
        self.inner.lock().unwrap().remove(&id);
    }

    /// Drop all streams (called when the ws connection dies).
    pub fn close_all(&self) {
        self.inner.lock().unwrap().clear();
    }
}
