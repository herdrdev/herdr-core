use super::*;
use crate::wire;
use std::cell::Cell;

thread_local! {
    pub(super) static PAINTS: Cell<usize> = const { Cell::new(0) };
}

#[test]
fn resize_drags_serialize_nothing_without_observers_and_only_latest_when_stalled() {
    let root = crate::test_tmp().join(format!("hc-resize-{}", crate::platform::random_hex()));
    let paths = crate::paths::Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let id = {
        let mut st = core.lock();
        let id = st
            .reg
            .create(
                crate::terminals::NewTerminal { argv: vec!["sh".into()], cols: 80, rows: 24, ..Default::default() },
                0,
            )
            .unwrap();
        st.screens.insert(id, ghostty_vt::Terminal::new(80, 24, 1 << 20));
        id
    };
    let leader = Arc::new(Conn::default());
    let request = |observe, resize_notifications| AttachRequest {
        observe,
        resize_notifications,
        keep_size_on_detach: false,
        history: 0,
        size: (80, 24),
        display: Display::default(),
        resumed: false,
    };
    let channel = core.attach(&leader, id, request(false, false)).unwrap();
    PAINTS.set(0);
    for cols in 81..100 {
        core.viewer_resize(&leader, channel, cols, 24);
    }
    assert_eq!(PAINTS.get(), 0, "desktop-only drags must not serialize paints");
    let observer = Arc::new(Conn::default());
    let observed = core.attach(&observer, id, request(true, true)).unwrap();
    observer.with(|out| {
        out.resume(observed);
        out.data(observed, &vec![b'x'; wire::WINDOW]);
        while out.next_frame().is_some() {}
    });
    PAINTS.set(0);
    for cols in 100..200 {
        core.viewer_resize(&leader, channel, cols, 24);
        core.prepare_repaints(&observer);
    }
    assert_eq!(PAINTS.get(), 0, "no serialization while credit stalled");
    observer.with(|out| {
        assert_eq!(out.queued(observed), 0, "no obsolete paints accumulated");
        assert!(out.next_frame().is_none());
        out.credit(observed, wire::WINDOW);
    });
    core.prepare_repaints(&observer);
    assert_eq!(PAINTS.get(), 1);
    observer.with(|out| {
        let frame = out.next_frame().unwrap();
        assert_eq!(frame[4], wire::RESIZE);
        assert_eq!(&frame[wire::HEADER..], &[199, 0, 24, 0]);
        let frame = out.next_frame().unwrap();
        assert_eq!(frame[4], wire::DATA);
        assert_eq!(frame[wire::HEADER], 0x18, "cancel a partial old-grid sequence");
    });
    core.prepare_repaints(&observer);
    assert_eq!(PAINTS.get(), 1, "unchanged grid is not repainted twice");
    // Exit before the writer next wakes must still deliver the final screen.
    core.viewer_resize(&leader, channel, 200, 24);
    core.record_finish(&mut core.lock(), id, proto::Exit { code: Some(0), signal: None }, "exited");
    observer.with(|out| {
        let size = out.next_frame().expect("final resize");
        assert_eq!(size[4], wire::RESIZE);
        assert_eq!(&size[wire::HEADER..], &[200, 0, 24, 0]);
        assert_eq!(out.next_frame().unwrap()[4], wire::DATA);
        while let Some(frame) = out.next_frame() {
            if frame[4] == wire::CLOSE {
                break;
            }
        }
    });
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn keep_size_on_detach_preserves_size_for_departing_leader_only() {
    use std::time::Duration;

    let root = crate::test_tmp().join(format!("hc-keep-size-{}", crate::platform::random_hex()));
    let paths = crate::paths::Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let id = {
        let mut st = core.lock();
        let id = st
            .reg
            .create(
                crate::terminals::NewTerminal { argv: vec!["sh".into()], cols: 100, rows: 30, ..Default::default() },
                0,
            )
            .unwrap();
        st.screens.insert(id, ghostty_vt::Terminal::new(100, 30, 1 << 20));
        id
    };
    let req = |keep| AttachRequest {
        observe: false,
        resize_notifications: false,
        keep_size_on_detach: keep,
        history: 0,
        size: (110, 32),
        display: Display::default(),
        resumed: false,
    };
    let size = || {
        let st = core.lock();
        let t = &st.reg.terminals[&id];
        (t.cols, t.rows)
    };

    // 1. Opted-in leader keeps its resized dimensions after detaching.
    let c1 = Arc::new(Conn::default());
    let ch1 = core.attach(&c1, id, req(true)).unwrap();
    core.viewer_resize(&c1, ch1, 118, 28);
    core.detach(&c1, Some(ch1));
    assert_eq!(size(), (118, 28));

    // 2. Opt-out leader reverts to headless on detach.
    let c2 = Arc::new(Conn::default());
    let ch2 = core.attach(&c2, id, req(false)).unwrap();
    core.detach(&c2, Some(ch2));
    assert_eq!(size(), core.headless());

    // 3. Whole connection closes with an inactive opted-in viewer and an active opt-out leader:
    // the departing leader decides, so it reverts to headless.
    let dual = Arc::new(Conn::default());
    let _inactive_kept = core.attach(&dual, id, req(true)).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    let active_optout = core.attach(&dual, id, req(false)).unwrap();
    core.viewer_resize(&dual, active_optout, 96, 26);
    assert_eq!(size(), (96, 26));
    core.detach(&dual, None);
    assert_eq!(size(), core.headless());

    // 4. Opted-in leader detaches while another control viewer remains: remaining viewer wins.
    let staying = Arc::new(Conn::default());
    let staying_ch = core.attach(&staying, id, req(false)).unwrap();
    core.viewer_resize(&staying, staying_ch, 90, 24);
    std::thread::sleep(Duration::from_millis(2));
    let leaving = Arc::new(Conn::default());
    let leaving_ch = core.attach(&leaving, id, req(true)).unwrap();
    core.viewer_resize(&leaving, leaving_ch, 130, 40);
    assert_eq!(size(), (130, 40));
    core.detach(&leaving, Some(leaving_ch));
    assert_eq!(size(), (90, 24));

    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
