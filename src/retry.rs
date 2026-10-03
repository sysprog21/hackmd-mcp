use std::{cell::RefCell, future::Future, time::Duration};

use rmcp::model::{CallToolResponse, MetaObject};
use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub(crate) struct RetryMetadata {
    attempts: u8,
    total_waited_seconds: f64,
    was_rate_limited: bool,
    readback_attempts: u8,
    readback_elapsed_seconds: f64,
}

tokio::task_local! {
    static RETRY_METADATA: RefCell<RetryMetadata>;
}

pub(crate) fn record_retry(waited: Duration, was_rate_limited: bool) {
    let _result = RETRY_METADATA.try_with(|metadata| {
        let mut metadata = metadata.borrow_mut();
        metadata.attempts = metadata.attempts.saturating_add(1);
        metadata.total_waited_seconds += waited.as_secs_f64();
        metadata.was_rate_limited |= was_rate_limited;
    });
}

pub(crate) fn record_readback(attempts: u8, elapsed: Duration) {
    let _result = RETRY_METADATA.try_with(|metadata| {
        let mut metadata = metadata.borrow_mut();
        metadata.readback_attempts = metadata.readback_attempts.saturating_add(attempts);
        metadata.readback_elapsed_seconds += elapsed.as_secs_f64();
    });
}

pub(crate) async fn scope_call<F>(future: F) -> Result<CallToolResponse, rmcp::ErrorData>
where
    F: Future<Output = Result<CallToolResponse, rmcp::ErrorData>>,
{
    RETRY_METADATA
        .scope(
            RefCell::new(RetryMetadata {
                attempts: 1,
                ..RetryMetadata::default()
            }),
            async move {
                let mut response = future.await?;
                let metadata = RETRY_METADATA.with(|metadata| {
                    let metadata = metadata.borrow();
                    (metadata.attempts > 1 || metadata.readback_attempts > 0).then(|| {
                        serde_json::to_value(&*metadata).expect("retry metadata serializes")
                    })
                });
                if let (CallToolResponse::Complete(result), Some(metadata)) =
                    (&mut response, metadata)
                {
                    let meta = result.meta.get_or_insert_with(MetaObject::default);
                    meta.0.insert("retry".to_owned(), metadata);
                }
                Ok(response)
            },
        )
        .await
}
