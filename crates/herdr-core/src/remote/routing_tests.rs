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
