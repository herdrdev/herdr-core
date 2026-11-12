use super::*;

fn machine(id: &str, name: &str) -> Machine {
    Machine {
        id: id.into(),
        name: name.into(),
        target: "unused".into(),
        via: false,
        temporary: false,
        session: None,
        host: "unused".into(),
        version: "test".into(),
        keeps_running_after_logout: None,
        logout_linger_user: None,
    }
}

fn peer(name: &str) -> Peer {
    Peer { name: name.into(), calls_me: "unused".into(), outbox: Vec::new() }
}

fn destination(info: &crate::api::CoreInfo, st: &State, selector: &str) -> String {
    match named(info, st, selector).unwrap() {
        None => info.id.clone(),
        Some(To::Machine(m)) => m.id,
        Some(To::Peer { id, .. }) => id,
    }
}

#[tokio::test]
async fn returned_addresses_route_back_to_the_same_machine() {
    let root = crate::test_tmp().join(format!("hc-address-{}", crate::platform::random_hex()));
    let paths = crate::paths::Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let first = machine("core-a", "core-b");
    core.lock().machines = vec![first.clone(), machine("core-b", "second")];
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    core.lock().lines.insert(first.id.clone(), LineState::Connected(line.clone()));
    let reply_to_run = async {
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        assert_eq!(request["method"], "terminal.run");
        line.take_reply(json!({"id": request["id"], "result": {"address": "t1"}}));
    };
    let (reply, ()) =
        tokio::join!(forward_once(&core, &first, "terminal.run", json!({"argv": ["echo"]})), reply_to_run);
    let reply = reply.unwrap();
    mirror_replace(&core, &first, vec![json!({"address": "t1"})]);
    let listed = core.matching(&core.lock(), &crate::events::Filter::default()).pop().unwrap();
    for record in [reply, listed] {
        let address = record["address"].as_str().unwrap();
        let (to, params) = route(&core, "terminal.get", &json!({"terminal": address})).unwrap().unwrap();
        assert!(matches!(to, To::Machine(m) if m.id == first.id), "{address} reached another core");
        assert_eq!(params["terminal"], "t1");
    }
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn permanent_ids_cannot_be_shadowed_by_display_names() {
    let mut info = crate::api::CoreInfo { id: "local-id".into(), name: "local-name".into() };
    let mut st = State { machines: vec![machine("core-a", "core-b"), machine("core-b", "second")], ..State::default() };
    for _ in 0..2 {
        assert_eq!(destination(&info, &st, "core-b"), "core-b");
        st.machines.reverse();
    }
    info.name = "core-b".into();
    assert_eq!(destination(&info, &st, "core-b"), "core-b", "local hostname is only an alias");
    info.name = "peer-b".into();
    st.machines[0].name = "peer-b".into();
    st.peers.insert("peer-a".into(), peer("peer-b"));
    st.peers.insert("peer-b".into(), peer("visible-peer"));
    assert_eq!(destination(&info, &st, "peer-b"), "peer-b", "local/machine/peer aliases cannot shadow a peer id");
    st.machines[0].name = "local-id".into();
    assert_eq!(destination(&info, &st, "local-id"), "local-id", "the local permanent id stays local");
}

#[test]
fn unambiguous_display_names_keep_working() {
    let info = crate::api::CoreInfo { id: "local-id".into(), name: "local-name".into() };
    let mut st = State { machines: vec![machine("core-a", "machine-name")], ..State::default() };
    st.peers.insert("peer-a".into(), peer("peer-name"));
    for (selector, expected) in [("local-name", "local-id"), ("machine-name", "core-a"), ("peer-name", "peer-a")] {
        assert_eq!(destination(&info, &st, selector), expected);
    }
    assert!(matches!(named(&info, &st, "missing"), Err(e) if e.kind == "machine_unknown"));
}
