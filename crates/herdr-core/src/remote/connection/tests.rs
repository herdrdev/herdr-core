use super::*;
use std::future::Future;
use std::pin::Pin;
use tokio::io::AsyncWriteExt;

fn fixture() -> (Arc<Core>, Machine) {
    let root = std::path::PathBuf::from(format!("/var/tmp/hc-wake-{}", crate::platform::random_hex()));
    let paths = crate::paths::Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "away".into(),
        target: "away".into(),
        via: false,
        temporary: false,
        session: None,
        id: "there".into(),
        host: "away".into(),
        version: "test".into(),
    };
    (core, machine)
}

fn clean(core: Arc<Core>) {
    let root = core.paths.state.clone();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

async fn probe(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>, receiving: Pin<&mut impl Future<Output = ()>>) -> Value {
    tokio::select! {
        _ = receiving => panic!("healthy connection ended"),
        frame = rx.recv() => serde_json::from_slice(&frame.unwrap()[wire::HEADER..]).unwrap(),
        _ = tokio::time::sleep(Duration::from_secs(1)) => panic!("wake did not send a health probe"),
    }
}

async fn poll(receiving: Pin<&mut impl Future<Output = ()>>) {
    tokio::select! {
        biased;
        _ = receiving => panic!("healthy connection ended"),
        _ = tokio::task::yield_now() => {},
    }
}

#[tokio::test(start_paused = true)]
async fn wake_probes_a_healthy_line_without_losing_a_partial_frame() {
    let (core, machine) = fixture();
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let (wake, mut wakes) = tokio::sync::watch::channel(0);
    let (mut reader, mut server) = tokio::io::duplex(1024);
    let mut retry = Retry::default();
    {
        let receiving = receive(&core, &machine, &line, &mut reader, &mut wakes, &mut retry, Duration::from_secs(45));
        tokio::pin!(receiving);
        let partial = wire::encode(0, wire::DATA, br#"{"id":999,"result":{}}"#);
        server.write_all(&partial[..3]).await.unwrap();
        poll(receiving.as_mut()).await;
        wake.send_replace(1);
        let first = probe(&mut rx, receiving.as_mut()).await;
        assert_eq!(first["method"], "server.status");
        server.write_all(&partial[3..]).await.unwrap();
        server
            .write_all(&wire::encode(0, wire::DATA, json!({"id":first["id"],"result":{}}).to_string().as_bytes()))
            .await
            .unwrap();
        poll(receiving.as_mut()).await;
        tokio::time::advance(Duration::from_secs(4)).await;
        poll(receiving.as_mut()).await;
        // Repeated wakes still probe, instead of replacing the healthy line.
        wake.send_replace(2);
        let second = probe(&mut rx, receiving.as_mut()).await;
        assert!(second["id"].as_u64() > first["id"].as_u64());
        server
            .write_all(&wire::encode(0, wire::DATA, json!({"id":second["id"],"result":{}}).to_string().as_bytes()))
            .await
            .unwrap();
        poll(receiving.as_mut()).await;
        drop(server);
        receiving.await;
    }
    assert!(!retry.fresh, "a working shared login must be preserved");
    clean(core);
}

#[tokio::test(start_paused = true)]
async fn wake_discards_a_dead_line_in_three_seconds_despite_buffered_traffic() {
    let (core, machine) = fixture();
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let (wake, mut wakes) = tokio::sync::watch::channel(0);
    let (mut reader, mut server) = tokio::io::duplex(1024);
    let mut retry = Retry::default();
    {
        let receiving = receive(&core, &machine, &line, &mut reader, &mut wakes, &mut retry, Duration::from_secs(45));
        tokio::pin!(receiving);
        wake.send_replace(1);
        probe(&mut rx, receiving.as_mut()).await;
        for _ in 0..2 {
            server.write_all(&wire::encode(9, wire::DATA, b"stale buffered heartbeat")).await.unwrap();
            poll(receiving.as_mut()).await;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        tokio::time::timeout(Duration::from_secs(2), receiving.as_mut()).await.expect("must not wait 45 s");
    }
    assert!(retry.fresh, "the replacement must bypass a stale SSH master");
    clean(core);
}

#[tokio::test(start_paused = true)]
async fn a_connection_closed_during_its_wake_probe_also_needs_a_fresh_login() {
    let (core, machine) = fixture();
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let (wake, mut wakes) = tokio::sync::watch::channel(0);
    let (mut reader, server) = tokio::io::duplex(1024);
    let mut retry = Retry::default();
    {
        let receiving = receive(&core, &machine, &line, &mut reader, &mut wakes, &mut retry, Duration::from_secs(45));
        tokio::pin!(receiving);
        wake.send_replace(1);
        probe(&mut rx, receiving.as_mut()).await;
        drop(server);
        receiving.await;
    }
    assert!(retry.fresh);
    clean(core);
}

#[tokio::test(start_paused = true)]
async fn wake_interrupts_backoff_and_fast_retries_are_bounded() {
    let mut retry = Retry::default();
    for _ in 0..12 {
        retry.delay();
    }
    assert_eq!(retry.delay(), Duration::from_secs(30));
    let (wake, mut wakes) = tokio::sync::watch::channel(0);
    wake.send_replace(1);
    retry.wait(&mut wakes).await;
    assert_eq!(retry.delay(), Duration::from_millis(250));
    assert!(retry.fresh);
    for _ in 0..12 {
        assert!(retry.delay() <= Duration::from_secs(1));
    }
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(retry.delay(), Duration::from_secs(30), "an offline host must not be hammered forever");
}

#[cfg(unix)]
#[tokio::test]
async fn wake_replaces_a_stalled_handshake_without_leaving_its_process() {
    use std::os::unix::fs::PermissionsExt;
    let (core, mut machine) = fixture();
    let script = core.paths.state.join("silent-ssh");
    let attempts = core.paths.state.join("attempts");
    std::fs::write(&script, format!("#!/bin/sh\necho $$ >> '{}'\nread hello\nexec sleep 300\n", attempts.display()))
        .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    machine.via = true;
    machine.target = format!("exec '{}'", script.display());
    core.lock().machines.push(machine.clone());
    let (wake, wakes) = tokio::sync::watch::channel(0);
    let task = tokio::spawn(supervise(core.clone(), machine.id, wakes));
    let pids = || {
        std::fs::read_to_string(&attempts)
            .unwrap_or_default()
            .lines()
            .filter_map(|s| s.parse::<i32>().ok())
            .collect::<Vec<_>>()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while pids().is_empty() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    wake.send_replace(1);
    while pids().len() < 2 {
        assert!(std::time::Instant::now() < deadline, "wake must cancel the 20 s handshake");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pids().len() < 3 {
        assert!(std::time::Instant::now() < deadline, "wake retries must not wait 20 s for an unready network");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    while pids().iter().any(|pid| unsafe { libc::kill(*pid, 0) } == 0) {
        assert!(std::time::Instant::now() < deadline, "abandoned transport still running");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    clean(core);
}
