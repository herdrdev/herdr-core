//! A put to another machine, in acknowledged pieces: a line carries one
//! piece at a time, so typing, credit and other calls cross between them.
//! Concurrent puts take turns. A core without pieces gets one whole put.

use super::*;
use crate::put;
use std::time::Instant;

/// A line's lane for pieces: one piece on it at a time, holding the quickest
/// answer to a piece so far (the line's round trip).
pub(super) struct Lane {
    turn: Arc<tokio::sync::Mutex<Duration>>,
    /// The piece on it now is past its deadline: its bytes may still be on the
    /// way, so it keeps the lane, and puts waiting for it stop waiting.
    stuck: AtomicBool,
}

impl Default for Lane {
    fn default() -> Self {
        Lane { turn: Arc::new(tokio::sync::Mutex::new(Duration::MAX)), stuck: AtomicBool::new(false) }
    }
}

fn late(machine: &str, what: &str, deadline: Duration) -> RpcError {
    RpcError::new(
        "unavailable",
        format!("{machine} didn't answer {what} within {} s", deadline.as_secs()),
        "it may be busy or stuck; hc machine ls shows its line",
    )
}

pub(super) async fn forward(core: &Core, m: &Machine, params: Value) -> Result<Value, RpcError> {
    // Only terminal.put carries a terminal address the next core can route again.
    if params["terminal"].as_str().is_some_and(|t| t.contains('/')) {
        return forward_once(core, m, "terminal.put", params).await;
    }
    let line = line(core, m).await?;
    if !line.features.iter().any(|f| f == "put_chunks") {
        return forward_once(core, m, "terminal.put", params).await;
    }
    let mut p: crate::api::PutParams = crate::rpc::parse_params(params)?;
    let too_big = || RpcError::invalid_params(format!("at most {} MB per put", put::MAX_BYTES >> 20));
    if p.data.len() / 4 * 3 > put::MAX_BYTES + 3 {
        return Err(too_big());
    }
    let bytes =
        put::decode(&std::mem::take(&mut p.data)).ok_or_else(|| RpcError::invalid_params("data is not base64"))?;
    if bytes.len() > put::MAX_BYTES {
        return Err(too_big());
    }
    // Each step has the ordinary deadline; the whole put takes what the link needs.
    let deadline = crate::config::load(&core.paths.config).unwrap_or_default().machine_timeout();
    // Named here, so a begin whose answer never comes can still be aborted.
    let transfer = crate::platform::random_hex();
    let mut abort = Abort { line: &line, transfer: &transfer, armed: false };
    let begin = json!({"terminal": p.terminal, "name": p.name, "bytes": bytes.len(), "transfer": transfer});
    let stuck = || late(&m.name, "an earlier piece", deadline);
    loop {
        if line.bulk.stuck.load(Ordering::SeqCst) {
            return Err(stuck());
        }
        abort.armed = true;
        match line.call(&m.name, "terminal.put_begin", begin.clone(), deadline).await {
            Ok(_) => break,
            Err(e) if e.kind == "unavailable" && e.message == put::BUSY => {
                // Wait for a slot without holding the lane other puts need to finish.
                abort.armed = false;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                // A definite refusal began nothing there, so there is nothing to abort.
                abort.armed = line_lost(&e) || e.message.contains("didn't answer");
                return Err(e);
            }
        }
    }
    // Each put starts small: the link may be slower than when the last one ran.
    let (mut offset, mut size) = (0, put::FIRST_PIECE);
    while offset < bytes.len() {
        let end = (offset + size).min(bytes.len());
        let piece = json!({"transfer": transfer, "offset": offset, "data": put::encode(&bytes[offset..end])});
        // Fair: tokio's mutex is first come, first served. The piece holds the
        // lane until it is answered or the line goes, even past its deadline or
        // its caller: no other piece may queue behind its bytes meanwhile.
        if line.bulk.stuck.load(Ordering::SeqCst) {
            return Err(stuck());
        }
        // One wait, polled again and again, keeps its place in the queue.
        let wait = line.bulk.turn.clone().lock_owned();
        tokio::pin!(wait);
        let mut turn = loop {
            match tokio::time::timeout(deadline, &mut wait).await {
                Ok(turn) => break turn,
                Err(_) if line.bulk.stuck.load(Ordering::SeqCst) => return Err(stuck()),
                Err(_) => {} // other puts' pieces, still coming back
            }
        };
        let (tx, rx) = oneshot::channel();
        let (to, name) = (line.clone(), m.name.clone());
        tokio::spawn(async move {
            let sent = Instant::now();
            let call = to.call(&name, "terminal.put_chunk", piece, Duration::MAX);
            tokio::pin!(call);
            let (r, tx) = match tokio::time::timeout(deadline, &mut call).await {
                Ok(r) => (r, Some(tx)),
                Err(_) => {
                    // Its caller hears now; the lane waits for the answer.
                    to.bulk.stuck.store(true, Ordering::SeqCst);
                    let _ = tx.send(Err(late(&name, "a piece", deadline)));
                    // Still none long after: whatever holds it there, this
                    // line can't carry puts again. A fresh one can.
                    let r = match tokio::time::timeout(2 * deadline, &mut call).await {
                        Ok(r) => r,
                        Err(_) => {
                            let _ = writeln!(io::stderr(), "core: {name} never answered a piece: line given up");
                            to.retired.notify_one();
                            call.await
                        }
                    };
                    (r, None)
                }
            };
            to.bulk.stuck.store(false, Ordering::SeqCst);
            let took = sent.elapsed();
            if r.is_ok() {
                *turn = (*turn).min(took);
            }
            if let Some(tx) = tx {
                let _ = tx.send(r.map(|_| (took, *turn)));
            }
        });
        let (took, base) = rx.await.map_err(|_| RpcError::internal("a piece's sender ended"))??;
        size = put::next_piece(size, took, base);
        offset = end;
    }
    let done = line.call(&m.name, "terminal.put_finish", json!({"transfer": transfer}), deadline).await;
    let done = done.map_err(|mut e| {
        if e.kind == "unavailable" {
            e.hint = "it may have landed there; check before sending it again".into();
        }
        e
    })?;
    abort.armed = false;
    Ok(done)
}

/// A put given up on (an error, or its caller went) is removed there. A
/// finish that got no answer is not retried: the file may have landed.
struct Abort<'a> {
    line: &'a Line,
    transfer: &'a str,
    armed: bool,
}

impl Drop for Abort<'_> {
    fn drop(&mut self) {
        if self.armed {
            let params = json!({"transfer": self.transfer});
            self.line.message(&json!({"jsonrpc": "2.0", "method": "terminal.put_abort", "params": params}));
        }
    }
}
