use super::*;

#[tokio::test]
async fn provisional_grants_survive_only_successful_or_ambiguous_launches() {
    let root = crate::test_tmp().join(format!("hc-run-grants-{}", crate::platform::random_hex()));
    let paths = crate::paths::Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine: Machine = serde_json::from_value(json!({
        "name": "away", "target": "away", "id": "away", "host": "away", "version": "test"
    }))
    .unwrap();
    let failed = || Err(RpcError::new("spawn_failed", "bad program", ""));
    for (first, second, retained) in [
        (Some(failed()), Some(failed()), false),
        (Some(failed()), Some(Ok(json!({}))), true),
        (Some(Ok(json!({}))), Some(failed()), true),
        (Some(failed()), None, true), // The forwarding future was cancelled.
        (Some(failed()), Some(Err(RpcError::new("unavailable", "lost the line", ""))), true),
        (Some(failed()), Some(Err(RpcError::new("spawn_failed", "the holder did not answer", ""))), true),
        (Some(failed()), Some(Err(RpcError::new("internal", "cannot save terminals", ""))), true),
    ] {
        let parent = core
            .lock()
            .reg
            .create(crate::terminals::NewTerminal { argv: vec!["true".into()], ..Default::default() }, 0)
            .unwrap();
        let params = json!({"caller": format!("here/{}", crate::terminals::address(parent))});
        let a = RunGrant::start(&core, &machine, "terminal.run", &params).unwrap();
        let b = RunGrant::start(&core, &machine, "terminal.run", &params).unwrap();
        a.answered(&first.unwrap());
        assert!(core.lock().reg.terminals[&parent].afar.contains("away"), "another run still needs the grant");
        match second {
            Some(result) => b.answered(&result),
            None => drop(b),
        }
        assert_eq!(core.lock().reg.terminals[&parent].afar.contains("away"), retained);
        if retained {
            assert!(RunGrant::start(&core, &machine, "terminal.run", &params).is_none());
            assert!(core.lock().reg.terminals[&parent].afar.contains("away"));
        }
        assert!(core.lock().afar_runs.is_empty());
    }
    core.flush_saved().await.unwrap();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
