//! Helpers for asserting on metrics recorded in tests.
//!
//! The global recorder is shared by every test in a process, so tests
//! record into a thread-local recorder instead and inspect a snapshot of
//! just what they recorded.

use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use std::collections::BTreeMap;
use std::future::Future;

/// A recorded metric value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Counter(u64),
    Gauge(f64),
    Histogram(Vec<f64>),
}

/// Metrics recorded by a test, keyed by `name{label="value",...}`.
pub type Snapshot = BTreeMap<String, Value>;

/// Run `f` with a thread-local recorder and return what it recorded.
pub fn capture(f: impl FnOnce()) -> Snapshot {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, f);
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .map(|(key, _unit, _description, value)| {
            let key = key.key();
            let labels = key
                .labels()
                .map(|l| format!("{}={:?}", l.key(), l.value()))
                .collect::<Vec<_>>();
            let name = if labels.is_empty() {
                key.name().to_string()
            } else {
                format!("{}{{{}}}", key.name(), labels.join(","))
            };
            let value = match value {
                DebugValue::Counter(v) => Value::Counter(v),
                DebugValue::Gauge(v) => Value::Gauge(v.into_inner()),
                DebugValue::Histogram(v) => {
                    Value::Histogram(v.into_iter().map(|v| v.into_inner()).collect())
                }
            };
            (name, value)
        })
        .collect()
}

/// Like [`capture`], but drives `fut` to completion on a single-threaded
/// runtime so that everything it records lands in the thread-local
/// recorder.
pub fn capture_async<T>(fut: impl Future<Output = T>) -> (T, Snapshot) {
    let mut output = None;
    let snapshot = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        output = Some(runtime.block_on(fut));
    });
    (output.expect("future completed"), snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture() {
        let snapshot = capture(|| {
            metrics::counter!("requests", "method" => "GET", "path" => "/").increment(2);
            metrics::gauge!("open").set(3.0);
            metrics::histogram!("latency").record(0.5);
        });
        assert_eq!(
            snapshot,
            Snapshot::from([
                ("latency".to_string(), Value::Histogram(vec![0.5])),
                ("open".to_string(), Value::Gauge(3.0)),
                (
                    "requests{method=\"GET\",path=\"/\"}".to_string(),
                    Value::Counter(2)
                ),
            ])
        );
    }

    #[test]
    fn test_capture_async() {
        let (output, snapshot) = capture_async(async {
            tokio::task::yield_now().await;
            metrics::counter!("done").increment(1);
            42
        });
        assert_eq!(output, 42);
        assert_eq!(
            snapshot,
            Snapshot::from([("done".to_string(), Value::Counter(1))])
        );
    }
}
