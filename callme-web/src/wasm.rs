use anyhow::Result;
use iroh::NodeId;
use n0_future::{Stream, StreamExt};
use serde::Serialize;
use tracing::level_filters::LevelFilter;
use tracing_subscriber_wasm::MakeConsoleWriter;
use wasm_bindgen::{prelude::wasm_bindgen, JsError};
use wasm_streams::{readable::sys::ReadableStream as JsReadableStream, ReadableStream};

use crate::Node;

#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();

    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::TRACE)
        .with_writer(
            // To avoide trace events in the browser from showing their JS backtrace
            MakeConsoleWriter::default().map_trace_level_to(tracing::Level::DEBUG),
        )
        // If we don't do this in the browser, we get a runtime error.
        .without_time()
        .with_ansi(false)
        .init();

    tracing::info!("(testing logging) Logging setup");
}

#[wasm_bindgen]
pub struct CallmeNode(Node);

#[wasm_bindgen]
impl CallmeNode {
    pub async fn spawn() -> Result<Self, JsError> {
        Ok(Self(Node::spawn().await.map_err(to_js_err)?))
    }

    pub fn node_id(&self) -> String {
        self.0.ep.node_id().to_string()
    }

    pub fn connect(&self, node_id: String) -> Result<JsReadableStream, JsError> {
        let node_id: NodeId = node_id.parse().map_err(to_js_err)?;
        let events = self.0.connect(node_id);
        Ok(into_js_readable_stream(events))
    }

    pub fn accept(&self) -> JsReadableStream {
        let events = self.0.accept();
        into_js_readable_stream(events)
    }
}

fn to_js_err(err: impl Into<anyhow::Error>) -> JsError {
    let err: anyhow::Error = err.into();
    JsError::new(&err.to_string())
}

fn into_js_readable_stream<T: Serialize>(
    stream: impl Stream<Item = T> + 'static,
) -> wasm_streams::readable::sys::ReadableStream {
    let stream = stream.map(|event| Ok(serde_wasm_bindgen::to_value(&event).unwrap()));
    ReadableStream::from_stream(stream).into_raw()
}
