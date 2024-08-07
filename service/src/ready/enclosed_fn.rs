use crate::pipeline::{marker::AsyncFn, PipelineT};

use super::ReadyService;

impl<S, T> ReadyService for PipelineT<S, T, AsyncFn>
where
    S: ReadyService,
{
    type Ready = S::Ready;

    #[inline]
    async fn ready(&self) -> Self::Ready {
        self.first.ready().await
    }
}
