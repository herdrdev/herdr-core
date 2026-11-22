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
        paint_boundaries: false,
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
fn paint_boundaries_cover_attach_resize_lag_and_supersede_and_never_leak_to_unopted_viewers() {
    let root = crate::test_tmp().join(format!("hc-paint-boundaries-{}", crate::platform::random_hex()));
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
    let make_req = |observe, resize_notifications, paint_boundaries| AttachRequest {
        observe,
        resize_notifications,
        paint_boundaries,
        history: 0,
        size: (80, 24),
        display: Display::default(),
        resumed: false,
    };
    let leader = Arc::new(Conn::default());
    let leader_ch = core.attach(&leader, id, make_req(false, false, false)).unwrap();

    // Unopted observer (paint_boundaries=false) never receives PAINT_END.
    let unopted = Arc::new(Conn::default());
    let unopted_ch = core.attach(&unopted, id, make_req(true, true, false)).unwrap();
    unopted.with(|out| {
        out.resume(unopted_ch);
        let mut kinds = Vec::new();
        while let Some(f) = out.next_frame() {
            kinds.push(f[4]);
        }
        assert!(kinds.contains(&wire::DATA));
        assert!(!kinds.contains(&wire::PAINT_END));
    });

    // Path 1: Initial attach with paint_boundaries=true emits RESIZE -> DATA -> PAINT_END.
    let opted = Arc::new(Conn::default());
    let opted_ch = core.attach(&opted, id, make_req(true, true, true)).unwrap();
    let drain_all = |conn: &Arc<Conn>| -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        conn.with(|ob| {
            while let Some(f) = ob.next_frame() {
                out.push((f[4], f[wire::HEADER..].to_vec()));
            }
        });
        out
    };
    opted.with(|out| out.resume(opted_ch));
    let initial = drain_all(&opted);
    assert_eq!(initial.first().map(|x| x.0), Some(wire::RESIZE));
    assert_eq!(initial.last().map(|x| x.0), Some(wire::PAINT_END));

    // Path 2: RESIZE redraw emits RESIZE -> DATA -> PAINT_END.
    core.viewer_resize(&leader, leader_ch, 100, 30);
    core.prepare_repaints(&opted);
    let resize_frames = drain_all(&opted);
    assert_eq!(resize_frames.first().map(|x| (x.0, x.1.as_slice())), Some((wire::RESIZE, &[100, 0, 30, 0][..])));
    assert_eq!(resize_frames.last().map(|x| x.0), Some(wire::PAINT_END));

    // Path 3: 4 MiB lag overflow emits RESIZE(same size) -> DATA -> PAINT_END.
    opted.with(|out| {
        assert!(out.data(opted_ch, &vec![b'z'; wire::MAX_PENDING - 10]));
    });
    core.output(&mut core.lock(), id, &[b'y'; 64]);
    opted.with(|out| out.credit(opted_ch, wire::WINDOW * 4));
    let lag_frames = drain_all(&opted);
    assert_eq!(lag_frames.first().map(|x| (x.0, x.1.as_slice())), Some((wire::RESIZE, &[100, 0, 30, 0][..])));
    assert_eq!(lag_frames.last().map(|x| x.0), Some(wire::PAINT_END));

    // Path 4: Superseded paint drops stale PAINT_END and emits only final RESIZE -> DATA -> PAINT_END.
    opted.with(|out| {
        assert!(out.data(opted_ch, &vec![b'w'; wire::WINDOW]));
        while out.next_frame().is_some() {}
    });
    core.viewer_resize(&leader, leader_ch, 110, 32);
    core.prepare_repaints(&opted);
    core.viewer_resize(&leader, leader_ch, 120, 36);
    opted.with(|out| out.credit(opted_ch, wire::WINDOW * 2));
    core.prepare_repaints(&opted);
    let sup = drain_all(&opted);
    assert_eq!(sup.first().map(|x| (x.0, x.1.as_slice())), Some((wire::RESIZE, &[120, 0, 36, 0][..])));
    assert_eq!(sup.iter().filter(|x| x.0 == wire::PAINT_END).count(), 1);
    assert_eq!(sup.last().map(|x| x.0), Some(wire::PAINT_END));

    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
