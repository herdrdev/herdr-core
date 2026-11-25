//! `terminal.put` here, and the pieces another core sends a put in.

use super::*;
use crate::put::{self, Transfer};
use crate::runtime::State;

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn too_big() -> RpcError {
    RpcError::invalid_params(format!("at most {} MB per put", put::MAX_BYTES >> 20))
}

/// A live terminal here, or why a file for it would go unread.
fn live(core: Ctx<'_>, terminal: &str) -> Result<u32, RpcError> {
    let id = resolve(core, terminal)?;
    record(core, id)?;
    if !is_live(&core.lock(), id) {
        return Err(finished(id));
    }
    Ok(id)
}

fn is_live(st: &State, id: u32) -> bool {
    st.reg.terminals.get(&id).is_some_and(|t| t.is_live())
}

fn finished(id: u32) -> RpcError {
    RpcError::new("finished", format!("{} has finished", crate::terminals::address(id)), "nothing there to read it")
}

fn cannot_write(e: std::io::Error) -> RpcError {
    RpcError::internal(format!("cannot write the file: {e}"))
}

pub(super) async fn terminal_put(core: Ctx<'_>, p: PutParams) -> Result<PutResult, RpcError> {
    if p.data.len() / 4 * 3 > put::MAX_BYTES + 3 {
        return Err(too_big());
    }
    let bytes = put::decode(&p.data).ok_or_else(|| RpcError::invalid_params("data is not base64"))?;
    if bytes.len() > put::MAX_BYTES {
        return Err(too_big());
    }
    let id = live(core, &p.terminal)?;
    let path = put::stage(&core.paths.state, id, &p.name, &bytes).map_err(cannot_write)?;
    Ok(PutResult { path: path.display().to_string(), bytes: bytes.len() })
}

/// Pieces come from the connection that began the transfer; it ends with it.
fn conn(core: Ctx<'_>) -> Result<(&Arc<crate::wire::Conn>, usize), RpcError> {
    core.conn.map(|c| (c, crate::remote::conn_key(c))).ok_or_else(|| {
        RpcError::new("needs_framed", "a put in pieces needs a framed connection", "send hello with framed: true")
    })
}

/// A transfer's handle: what the sender picked, checked before anything keeps it.
fn handle(transfer: &str) -> Result<(), RpcError> {
    match put::valid_handle(transfer) {
        true => Ok(()),
        false => Err(RpcError::invalid_params("transfer must be 16 to 64 hex digits")),
    }
}

fn transfer<'a>(st: &'a mut State, conn: usize, handle: &str) -> Result<&'a mut Transfer, RpcError> {
    st.puts.get_mut(handle).filter(|t| t.conn == conn).ok_or_else(|| gone(handle))
}

fn gone(handle: &str) -> RpcError {
    RpcError::new("not_found", format!("no put {handle} here"), "it was aborted, or its terminal or connection went")
}

/// A begin read on `conn`, noted before its call runs (in frame order), so an
/// abort read after it finds it pending. The call's task keeps what this
/// returns until the begin has an answer or its task is cancelled.
pub fn begin_read(conn: &Arc<crate::wire::Conn>, request: &serde_json::Value) -> Option<Pending> {
    let h = request["params"]["transfer"].as_str().filter(|h| handle(h).is_ok());
    let h = h.filter(|_| request["method"] == "terminal.put_begin")?;
    lock(&conn.puts_pending).entry(h.to_string()).or_default().0 += 1;
    Some(Pending(conn.clone(), h.to_string()))
}

pub struct Pending(Arc<crate::wire::Conn>, String);

impl Drop for Pending {
    fn drop(&mut self) {
        let mut pending = lock(&self.0.puts_pending);
        if let Some(n) = pending.get_mut(&self.1).map(|(n, _)| {
            *n -= 1;
            *n
        }) && n == 0
        {
            pending.remove(&self.1);
        }
    }
}

pub(super) async fn terminal_put_begin(core: Ctx<'_>, p: PutBeginParams) -> Result<PutBeginResult, RpcError> {
    let (c, conn) = conn(core)?;
    handle(&p.transfer)?;
    if p.bytes > put::MAX_BYTES {
        return Err(too_big());
    }
    let id = live(core, &p.terminal)?;
    let busy = || RpcError::new("unavailable", put::BUSY, "try again when one is done");
    let taken = || RpcError::invalid_params(format!("put {} is taken", p.transfer));
    {
        let st = core.lock();
        if st.puts.contains_key(&p.transfer) {
            return Err(taken());
        }
        if st.puts.len() >= put::MAX_TRANSFERS {
            return Err(busy());
        }
    }
    let (state, handle) = (core.paths.state.clone(), p.transfer.clone());
    let part = blocking(move || Ok(put::open_part(&state, id, &handle))).await?.map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => taken(),
        _ => cannot_write(e),
    })?;
    // Checked again where it is kept: meanwhile the connection, the terminal or
    // the caller (an abort that overtook this begin) may have gone, or another
    // begin (for another terminal) may have taken the handle.
    let mut st = core.lock();
    let refused = if c.gone.load(std::sync::atomic::Ordering::SeqCst) {
        Some(RpcError::new("unavailable", "its connection closed", ""))
    } else if lock(&c.puts_pending).get(&p.transfer).is_some_and(|(_, aborted)| *aborted) {
        Some(RpcError::new("not_found", format!("put {} was aborted", p.transfer), ""))
    } else if st.puts.contains_key(&p.transfer) {
        Some(taken())
    } else if !is_live(&st, id) {
        Some(finished(id))
    } else if st.puts.len() >= put::MAX_TRANSFERS {
        Some(busy())
    } else {
        None
    };
    if let Some(e) = refused {
        drop(st);
        let _ = std::fs::remove_file(&part);
        // A terminal removed meanwhile keeps no folder made for it.
        if e.kind == "finished" {
            let _ = part.parent().map(std::fs::remove_dir);
        }
        return Err(e);
    }
    let t = Transfer { terminal: id, conn, name: p.name, part, total: p.bytes, got: 0, writing: false };
    st.puts.insert(p.transfer.clone(), t);
    Ok(PutBeginResult { transfer: p.transfer })
}

pub(super) async fn terminal_put_chunk(core: Ctx<'_>, p: PutPieceParams) -> Result<PutPieceResult, RpcError> {
    let (_, conn) = conn(core)?;
    handle(&p.transfer)?;
    let too_big = || RpcError::invalid_params(format!("at most {} MB per piece", put::MAX_PIECE >> 20));
    if p.data.len() / 4 * 3 > put::MAX_PIECE + 3 {
        return Err(too_big());
    }
    let bytes = put::decode(&p.data).ok_or_else(|| RpcError::invalid_params("data is not base64"))?;
    if bytes.len() > put::MAX_PIECE {
        return Err(too_big());
    }
    let (part, len) = {
        let mut st = core.lock();
        let t = transfer(&mut st, conn, &p.transfer)?;
        if t.writing || p.offset != t.got {
            return Err(RpcError::invalid_params(format!("the next piece starts at {}", t.got)));
        }
        if t.got + bytes.len() > t.total {
            return Err(RpcError::invalid_params(format!("more than the {} bytes begun with", t.total)));
        }
        let id = t.terminal;
        if !is_live(&st, id) {
            drop_transfer(&mut st, &p.transfer);
            return Err(finished(id));
        }
        let t = transfer(&mut st, conn, &p.transfer)?;
        t.writing = true;
        (t.part.clone(), bytes.len())
    };
    let at = part.clone();
    let wrote = blocking(move || put::write_at(&at, p.offset, &bytes).map_err(cannot_write)).await;
    let mut st = core.lock();
    // Its transfer may have been dropped while this piece was written (aborted,
    // or its connection or terminal went), and the handle begun again for
    // another terminal. The part tells them apart: the same terminal can't
    // begin the handle again while this part exists (it is made create_new).
    let Some(t) = transfer(&mut st, conn, &p.transfer).ok().filter(|t| t.part == part) else {
        // The part was left for this write to remove.
        let _ = std::fs::remove_file(&part);
        return Err(gone(&p.transfer));
    };
    t.writing = false;
    match wrote {
        Ok(()) => {
            t.got += len;
            Ok(PutPieceResult { bytes: t.got })
        }
        Err(e) => {
            drop_transfer(&mut st, &p.transfer);
            Err(e)
        }
    }
}

pub(super) async fn terminal_put_finish(core: Ctx<'_>, p: PutTransferParams) -> Result<PutResult, RpcError> {
    let (_, conn) = conn(core)?;
    handle(&p.transfer)?;
    let t = {
        let mut st = core.lock();
        let t = transfer(&mut st, conn, &p.transfer)?;
        if t.writing {
            return Err(RpcError::invalid_params("a piece is still being written"));
        }
        st.puts.remove(&p.transfer).expect("found above")
    };
    let published = if t.got != t.total {
        Err(RpcError::invalid_params(format!("got {} of {} bytes", t.got, t.total)))
    } else if !is_live(&core.lock(), t.terminal) {
        Err(finished(t.terminal))
    } else {
        let (part, name) = (t.part.clone(), t.name.clone());
        blocking(move || put::publish(&part, &name).map_err(cannot_write)).await
    };
    match published {
        Ok(path) => Ok(PutResult { path: path.display().to_string(), bytes: t.total }),
        Err(e) => {
            let _ = std::fs::remove_file(&t.part);
            Err(e)
        }
    }
}

pub(super) async fn terminal_put_abort(core: Ctx<'_>, p: PutTransferParams) -> Result<Empty, RpcError> {
    let (c, conn) = conn(core)?;
    handle(&p.transfer)?;
    let mut st = core.lock();
    if transfer(&mut st, conn, &p.transfer).is_ok() {
        drop_transfer(&mut st, &p.transfer);
    } else if let Some(aborted) = lock(&c.puts_pending).get_mut(&p.transfer).map(|(_, aborted)| aborted) {
        // It overtook its begin: that begin is refused when it gets that far.
        *aborted = true;
    }
    let early = lock(&c.puts_pending).values().filter(|(_, aborted)| *aborted).count();
    if early > put::MAX_EARLY_ABORTS {
        c.gone.store(true, std::sync::atomic::Ordering::SeqCst);
        c.end.notify_one();
        let _ = writeln!(io::stderr(), "core: a connection aborted too many puts it never began: disconnected");
        return Err(RpcError::new("unavailable", "too many aborts of puts never begun: disconnected", ""));
    }
    Ok(Empty {})
}

/// Forgets a transfer. Its part goes now, unless a piece is being written into
/// it: that write removes it once it finds the transfer gone.
fn drop_transfer(st: &mut State, handle: &str) {
    if let Some(t) = st.puts.remove(handle)
        && !t.writing
    {
        let _ = std::fs::remove_file(&t.part);
    }
}

/// Transfers begun on a connection that closed, or for a terminal removed.
pub fn forget_puts(st: &mut State, gone: impl Fn(&Transfer) -> bool) {
    let handles: Vec<String> = st.puts.iter().filter(|(_, t)| gone(t)).map(|(h, _)| h.clone()).collect();
    handles.iter().for_each(|h| drop_transfer(st, h));
}
