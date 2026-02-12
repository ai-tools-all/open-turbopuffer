use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use crate::types::WriteOp;

const BATCH_WINDOW: Duration = Duration::from_millis(500);

pub struct WriteBatcher {
    #[allow(dead_code)]
    namespace: String,
    inner: Mutex<BatchState>,
}

struct BatchState {
    ops: Vec<WriteOp>,
    first_submit: Option<Instant>,
}

impl WriteBatcher {
    pub fn new(namespace: String) -> Self {
        Self {
            namespace,
            inner: Mutex::new(BatchState {
                ops: Vec::new(),
                first_submit: None,
            }),
        }
    }

    pub async fn submit(&self, ops: Vec<WriteOp>) {
        let mut state = self.inner.lock().await;
        if state.first_submit.is_none() {
            state.first_submit = Some(Instant::now());
        }
        state.ops.extend(ops);
    }

    pub async fn flush(&self) -> Vec<WriteOp> {
        let mut state = self.inner.lock().await;

        if let Some(first) = state.first_submit {
            let elapsed = first.elapsed();
            if elapsed < BATCH_WINDOW {
                let remaining = BATCH_WINDOW - elapsed;
                drop(state);
                tokio::time::sleep(remaining).await;
                state = self.inner.lock().await;
            }
        }

        let ops = std::mem::take(&mut state.ops);
        state.first_submit = None;
        ops
    }
}
