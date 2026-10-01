//! Drag-and-drop between two elements over CDP `Input.dispatchDragEvent`.
//!
//! HTML5 drag-and-drop does not respond to synthesized mouse moves: the browser
//! only raises `dragstart`/`drop` from the compositor's drag protocol, so a page
//! that implements a kanban board, a file target, or a sortable list cannot be
//! exercised by clicking and moving. This action drives that protocol directly.
//!
//! Both endpoints must be real elements. The drag payload carries the data
//! transfer items the page will see, so what is dragged is explicit rather than
//! borrowed from whatever the source element happens to hold.

use super::cdp::Cdp;
use super::interaction::{self, References};
use super::{output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

/// The four events CDP requires, in order. `dragCancel` is sent instead of
/// `drop` when a step fails, so the page does not stay in a drag state.
const DRAG_ORDER: &[&str] = &["dragEnter", "dragOver", "drop"];

/// Cap on drag payload items and total bytes, so one call cannot stuff an
/// arbitrary amount of data into the page's dataTransfer.
const MAX_DRAG_ITEMS: usize = 32;
const MAX_DRAG_DATA_BYTES: usize = 64 * 1024;

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

/// Validate the arguments for a drag action.
pub(super) fn validate(args: &Value) -> Result<()> {
    required(args, "from")?;
    required(args, "to")?;
    let object = args
        .as_object()
        .ok_or_else(|| error("drag arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(field.as_str(), "action" | "from" | "to" | "data" | "tab") {
            return Err(error(format!("unsupported drag argument: {field}")));
        }
    }
    if let Some(data) = args.get("data") {
        let items = data
            .as_array()
            .ok_or_else(|| error("data must be an array of {mime, value} items"))?;
        if items.len() > MAX_DRAG_ITEMS {
            return Err(error(format!("at most {MAX_DRAG_ITEMS} drag items")));
        }
        let mut total = 0usize;
        for item in items {
            let mime = item
                .get("mime")
                .and_then(Value::as_str)
                .ok_or_else(|| error("each drag item needs a mime"))?;
            let value = item
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| error("each drag item needs a value"))?;
            total += mime.len() + value.len();
        }
        if total > MAX_DRAG_DATA_BYTES {
            return Err(error(format!(
                "drag data exceeds {MAX_DRAG_DATA_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

/// Resolve a selector-or-ref to the centre point of the element it names.
async fn point_of(
    owner: &AgentCx,
    cdp: &mut Cdp,
    selector: &str,
    refs: Option<&References>,
) -> Result<(f64, f64)> {
    let id = interaction::resolve(owner, cdp, selector, refs)
        .await?
        .ok_or_else(|| error(format!("no element matches {selector}")))?;
    cdp.command(
        owner,
        "DOM.scrollIntoViewIfNeeded",
        json!({"backendNodeId": id}),
    )
    .await?;
    let point = interaction::element_call(owner, cdp, id, "point", json!({})).await?;
    let x = point["x"]
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| error(format!("element {selector} has no draggable x coordinate")))?;
    let y = point["y"]
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| error(format!("element {selector} has no draggable y coordinate")))?;
    Ok((x, y))
}

/// Execute a drag from one element to another.
pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    refs: Option<&References>,
    args: &Value,
) -> Result<ToolOutput> {
    let from = required(args, "from")?;
    let to = required(args, "to")?;
    let (from_x, from_y) = point_of(owner, cdp, from, refs).await?;
    let (to_x, to_y) = point_of(owner, cdp, to, refs).await?;

    // An empty dataTransfer is valid for pages that only care about which
    // element was dropped on, so absence is not an error.
    let items = args
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    json!({
                        "mimeType": item.get("mime").and_then(Value::as_str).unwrap_or("text/plain"),
                        "data": item.get("value").and_then(Value::as_str).unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let data = json!({"items": items, "dragOperationsMask": 1});

    // Both endpoints move through the same coordinate space, so every event
    // carries the destination point; a dragEnter at the origin is what the
    // browser expects before the move events that follow.
    for kind in DRAG_ORDER {
        let payload = json!({
            "type": kind,
            "x": to_x, "y": to_y,
            "data": data,
            // modifiers is required by the protocol even when zero.
            "modifiers": 0,
        });
        cdp.command(owner, "Input.dispatchDragEvent", payload)
            .await?;
    }

    Ok(output(
        format!("Dragged {from} to {to} in tab {tab}"),
        json!({
            "from": from, "to": to,
            "fromPoint": {"x": from_x, "y": from_y},
            "toPoint": {"x": to_x, "y": to_y},
            "items": items.len(),
            "backend": "cdp"
        }),
    ))
}
