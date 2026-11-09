use super::*;
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn keys_pressed_during_a_paste_follow_its_submit() {
    use crate::viewers::{AttachRequest, Display};
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let (core, root) = test_core();
    let (writer, frames) = mpsc::channel();
    let (bye, _) = mpsc::channel();
    let id = {
        let mut st = core.lock();
        let id =
            st.reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap();
        st.reg.terminals.get_mut(&id).unwrap().holder = Some("fake".into());
        st.screens.insert(id, Screen::new(80, 24, 1 << 20));
        st.holders.push(Link { writer, pid: 0, instance: "fake".into(), current: true, bye, bye_rx: None });
        id
    };
    let person = Arc::new(crate::wire::Conn::default());
    let channel = core
        .attach(
            &person,
            id,
            AttachRequest {
                observe: false,
                resize_notifications: false,
                history: 0,
                size: (80, 24),
                display: Display::default(),
                resumed: false,
            },
        )
        .unwrap();
    // Ignore attach's resize frames. From here the fake holder sees only input.
    while frames.try_recv().is_ok() {}
    {
        let sending = core.send(id, "abc", false, None);
        tokio::pin!(sending);
        poll_fn(|cx| {
            assert!(sending.as_mut().poll(cx).is_pending(), "the paste must pause before submit");
            Poll::Ready(())
        })
        .await;
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"abc"));
        assert!(frames.try_recv().is_err());
        core.viewer_input(&person, channel, b"Z", false);
        assert!(frames.try_recv().is_err(), "the person's keys must stay held");
        tokio::time::advance(crate::input::PASTE_GAP).await;
        sending.await.unwrap();
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"\r"));
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"Z"));
    }
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn server_exit_flushes_even_when_background_tasks_still_own_core() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let (release, blocked) = mpsc::channel();
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(())
    })
    .unwrap();
    let background_owner = core.clone();
    core.save(&core.lock());
    let finish = crate::server::finish(&core);
    tokio::pin!(finish);
    let waited = tokio::time::timeout(Duration::from_millis(10), &mut finish).await.is_err();
    release.send(()).unwrap();
    if waited {
        finish.await.unwrap();
    }
    drop(background_owner);
    remove_root(&root);
    assert!(waited, "serve returned before its queued snapshot finished");
}

#[tokio::test]
async fn server_stop_succeeds_even_when_the_registry_flush_times_out() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let (release, blocked) = mpsc::channel();
    let mut first = true;
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        if first {
            first = false;
            blocked.recv_timeout(Duration::from_secs(15)).unwrap();
        }
        Ok(())
    })
    .unwrap();
    let call = crate::api::Call { core: &core, conn: None, plain: None };
    let started = Instant::now();
    let stop = crate::api::dispatch(&call, "server.stop", json!({})).await;
    release.send(()).unwrap();
    core.flush_saved().await.unwrap();
    drop(core);
    remove_root(&root);
    assert!(started.elapsed() >= Duration::from_secs(10), "stuck disks must still time out");
    assert_eq!(stop.unwrap()["stopped"], true);
}

fn test_core() -> (Arc<Core>, PathBuf) {
    let root = std::env::temp_dir().join(format!("hc-persistence-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    (core, root)
}

#[tokio::test]
async fn label_and_kill_reply_with_the_record_observed_before_flushing() {
    for method in ["terminal.kill", "terminal.label"] {
        let (mut core, root) = test_core();
        core.flush_saved().await.unwrap();
        let (release, blocked) = mpsc::channel();
        Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
        let id = {
            let mut st = core.lock();
            let id = st
                .reg
                .create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, now_ms())
                .unwrap();
            st.reg.finish(id, Some(0), None, "exited", now_ms());
            core.save(&st);
            id
        };
        let reply = {
            let call = crate::api::Call { core: &core, conn: None, plain: None };
            let request =
                crate::api::dispatch(&call, method, json!({"terminal": crate::terminals::address(id), "labels": {}}));
            tokio::pin!(request);
            assert!(tokio::time::timeout(Duration::from_millis(10), &mut request).await.is_err());
            core.lock().reg.terminals.get_mut(&id).unwrap().turn = 1;
            // Label queued a second snapshot; kill reused the existing one.
            release.send(()).unwrap();
            let _ = release.send(());
            request.await.unwrap()
        };
        let captured = reply["turn"].clone();
        drop(core);
        remove_root(&root);
        assert_eq!(captured, 0, "{method} returned a turn observed after its barrier: {reply}");
    }
}

#[cfg(unix)]
#[test]
fn a_blocked_registry_write_does_not_hold_the_core_lock() {
    use std::os::unix::ffi::OsStrExt;
    let root = PathBuf::from(format!("/var/tmp/hc-save-lock-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !core.paths.state.join("terminals.json").exists() {
        assert!(Instant::now() < deadline, "startup registry was not written");
        std::thread::sleep(Duration::from_millis(1));
    }
    let tmp = core.paths.state.join("terminals.tmp");
    let name = std::ffi::CString::new(tmp.as_os_str().as_bytes()).unwrap();
    // Opening this temporary file for writing waits for a reader, like a stalled disk.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (saved, done) = mpsc::channel();
    let saving = core.clone();
    let worker = std::thread::spawn(move || {
        saving.save(&saving.lock());
        saved.send(()).unwrap();
    });
    let returned = done.recv_timeout(Duration::from_millis(250)).is_ok();
    let unlocked = core.state.try_lock().is_ok();
    // Always release the blocked write, even when checking the old synchronous implementation.
    let reader = std::fs::File::open(&tmp).unwrap();
    worker.join().unwrap();
    drop(core);
    drop(reader);
    remove_root(&root);
    assert!(returned && unlocked, "saving to a stalled disk must leave output and input free to take the core lock");
}

#[tokio::test]
async fn a_prepared_restart_survives_sigterm_and_rejects_stop() {
    let root = std::env::temp_dir().join(format!("hc-restart-stop-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let call = crate::api::Call { core: &core, conn: None, plain: None };
    crate::api::dispatch(&call, "server.restart", serde_json::json!({})).await.unwrap();
    crate::restart::on_sigterm(&core);
    assert!(core.paths.state.join("restart-env.json").exists(), "SIGTERM keeps the explicit restart's note");
    let error = crate::api::dispatch(&call, "server.stop", serde_json::json!({})).await.unwrap_err();
    assert_eq!(error.kind, "unavailable");
    assert!(!core.lock().stopping, "a prepared handoff keeps its holders");
    crate::restart::discard(&core.paths);
    core.restart.store(false, std::sync::atomic::Ordering::SeqCst);
    crate::api::dispatch(&call, "server.stop", serde_json::json!({})).await.unwrap();
    assert!(crate::restart::prepare(&core, None).is_err(), "a stop already in progress wins");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn host_shutdown_is_lost_without_overriding_explicit_actions() {
    for (killed, stopping, shutdown, expected) in [
        (false, false, false, "exited"),
        (false, false, true, "host_shutdown"),
        (false, true, false, "server_stop"),
        (false, true, true, "server_stop"),
        (true, false, false, "killed"),
        (true, false, true, "killed"),
        (true, true, false, "killed"),
        (true, true, true, "killed"),
    ] {
        assert_eq!(finish_reason(killed, stopping, shutdown), expected);
    }
}

#[cfg(unix)]
#[test]
fn a_new_holder_of_another_build_reports_both_identities() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-build-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let (stream, mut holder) = UnixStream::pair().unwrap();
    let fake = std::thread::spawn(move || {
        assert_eq!(proto::read(&mut holder).unwrap().unwrap().0, proto::HELLO);
        let welcome = proto::Welcome {
            version: proto::VERSION,
            instance: "other".into(),
            pid: 0,
            build: "other-build".into(),
            login_session: None,
            draining: false,
            previous: "none".into(),
            terminals: vec![],
        };
        io::Write::write_all(&mut holder, &proto::json_frame(proto::WELCOME, 0, &welcome)).unwrap();
        assert_eq!(proto::read(&mut holder).unwrap().unwrap().0, proto::DRAIN);
    });
    let err = core.take_over(stream, true, &root.join("run/core.holder.x.sock"), None).unwrap_err().to_string();
    assert!(err.contains("other-build"), "{err}");
    assert!(err.contains(&crate::client::build_id()), "{err}");
    assert!(core.lock().holders.is_empty());
    fake.join().unwrap();
    drop(core);
    remove_root(&root);
}

/// A frame no core can read: the reader of a link that gets it fails.
#[cfg(unix)]
const GARBAGE: [u8; 9] = [proto::OUTPUT, 1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];

/// A core with one live terminal on a fake holder. Each connection's hello is
/// answered as told ("welcome", "garbage", or both), and the fake reports
/// what it saw: connection number, then "hello", "no hello" or "closed".
#[cfg(unix)]
fn on_fake_holder(root: &std::path::Path, answers: &[&str]) -> (Arc<Core>, u32, mpsc::Receiver<String>) {
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let id = {
        let mut st = core.lock();
        let new = crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() };
        let id = st.reg.create(new, now_ms()).unwrap();
        st.reg.terminals.get_mut(&id).unwrap().holder = Some("fake".into());
        id
    };
    let held = proto::HeldTerminal { id, pid: 0, cols: 80, rows: 24, exit: None };
    let welcome = proto::Welcome {
        version: proto::VERSION,
        instance: "fake".into(),
        pid: 0,
        build: crate::client::build_id(),
        login_session: None,
        draining: false,
        previous: "crash".into(),
        terminals: vec![held],
    };
    let mut welcome = proto::json_frame(proto::WELCOME, 0, &welcome);
    welcome.extend(proto::frame(proto::TAIL, id, b""));
    let socket = root.join("run/core.holder.fake.sock");
    let listener = platform::listen(&socket).unwrap();
    let answers: Vec<Vec<u8>> = answers
        .iter()
        .map(|a| {
            let mut frames = if a.contains("welcome") { welcome.clone() } else { vec![] };
            if a.contains("garbage") {
                frames.extend(GARBAGE);
            }
            frames
        })
        .collect();
    let (heard, reports) = mpsc::channel();
    std::thread::spawn(move || {
        for (n, (stream, answer)) in listener.incoming().zip(answers).enumerate() {
            let (mut stream, heard) = (stream.unwrap(), heard.clone());
            std::thread::spawn(move || {
                let Ok(Some(_)) = proto::read(&mut stream) else {
                    return heard.send(format!("{n} no hello")).unwrap();
                };
                heard.send(format!("{n} hello")).unwrap();
                std::thread::sleep(Duration::from_millis(50)); // time for the test to act, well within RELINK_WAIT
                let _ = io::Write::write_all(&mut stream, &answer);
                while let Ok(Some(_)) = proto::read(&mut stream) {} // the core's frames, until it closes
                let _ = heard.send(format!("{n} closed"));
            });
        }
    });
    core.take_over(platform::connect(&socket).unwrap(), false, &socket, None).unwrap();
    (core, id, reports)
}

#[cfg(unix)]
fn reported(reports: &mpsc::Receiver<String>, n: usize) -> Vec<String> {
    let mut seen: Vec<String> = (0..n).map_while(|_| reports.recv_timeout(Duration::from_secs(5)).ok()).collect();
    seen.sort();
    seen
}

/// Live and not ended by losing its holder: left for whoever adopts it next.
#[cfg(unix)]
fn still_live(core: &Core, id: u32) -> bool {
    core.lock().reg.terminals.get(&id).is_some_and(|t| t.is_live() && t.reason.is_none())
}

#[cfg(unix)]
#[test]
fn a_link_whose_reading_fails_reconnects_but_not_twice_in_a_row() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "welcome garbage", ""]);
    assert_eq!(
        reported(&reports, 5),
        ["0 closed", "0 hello", "1 closed", "1 hello", "2 no hello"],
        "each failed link closes; the first failure reconnects, the second soon after does not"
    );
    assert!(core.lock().holders.is_empty());
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_the_holder_never_answers_gives_up_and_frees_holder_start() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", ""]);
    assert_eq!(reported(&reports, 4), ["0 closed", "0 hello", "1 closed", "1 hello"]);
    let start = Instant::now();
    while core.holder_start.try_lock().is_err() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(core.holder_start.try_lock().is_ok(), "runs and stops can start holders again");
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_whose_handshake_is_done_is_not_cut_however_slow_the_rest() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "welcome"]);
    while reports.recv_timeout(Duration::from_secs(5)).unwrap() != "1 hello" {}
    // Adopting waits for this lock: slow, but after the welcome came in time.
    let st = core.lock();
    std::thread::sleep(RELINK_WAIT * 3);
    drop(st);
    let start = Instant::now();
    while core.lock().holders.is_empty() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(RELINK_WAIT * 2);
    assert_eq!(core.lock().holders.len(), 1, "the new link stays");
    assert!(reports.try_iter().all(|r| r != "1 closed"), "and is not cut");
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_that_fails_leaves_the_holders_terminals_live() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "garbage"]);
    assert_eq!(reported(&reports, 4), ["0 closed", "0 hello", "1 closed", "1 hello"]);
    assert!(core.lock().holders.is_empty());
    assert!(still_live(&core, id), "its holder still answers: nothing ends");
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn waiting_for_holder_start_does_not_block_the_core_executor() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-start-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let (ready, entered) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let c = core.clone();
    let blocker = std::thread::spawn(move || {
        let _starting = c.holder_start.lock().unwrap();
        ready.send(()).unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(1));
        // Prevent any real holder from starting, including if the regression stalls the executor.
        c.lock().stopping = true;
    });
    entered.recv().unwrap();
    let c = core.clone();
    let start = tokio::spawn(async move { c.ensure_holder().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let answered_while_starting = !core.lock().stopping;
    let _ = release.send(());
    blocker.join().unwrap();
    assert!(start.await.unwrap().is_err());
    drop(core);
    remove_root(&root);
    assert!(answered_while_starting, "holder startup blocked the core executor");
}

/// Holder threads can keep the core, and so its registry writer, alive a
/// moment after the test drops it: a last snapshot may still land.
fn remove_root(root: impl AsRef<std::path::Path>) {
    let start = Instant::now();
    loop {
        match std::fs::remove_dir_all(root.as_ref()) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if start.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("removing {}: {e}", root.as_ref().display()),
        }
    }
}
