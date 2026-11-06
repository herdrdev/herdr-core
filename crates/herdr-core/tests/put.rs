#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

#[test]
fn a_file_lands_beside_the_terminal_keeps_its_name_and_goes_with_it() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let image: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let file = s.root.join("shot.png");
    std::fs::write(&file, &image).unwrap();

    let here = run(&s, &["sleep", "100"]);
    let first = s.json(&["put", &here, file.to_str().unwrap()]);
    let path = first["path"].as_str().unwrap();
    assert!(path.starts_with(&s.root.join("state").display().to_string()) && path.ends_with("/shot.png"), "{first}");
    assert_eq!((std::fs::read(path).unwrap(), first["bytes"].as_u64()), (image.clone(), Some(300_000)));
    let second = s.json(&["put", &here, file.to_str().unwrap()]);
    assert!(second["path"].as_str().unwrap().ends_with("/shot-2.png"), "{second}");

    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    let afar = s.json(&["put", &there, file.to_str().unwrap()]);
    let afar_path = afar["path"].as_str().unwrap();
    assert!(afar_path.starts_with(&rohan.root.display().to_string()), "lands on rohan: {afar}");
    assert_eq!(std::fs::read(afar_path).unwrap(), image);

    s.json(&["kill", &here]);
    s.json(&["rm", &here]);
    assert!(!std::path::Path::new(path).exists(), "removed with the terminal");

    // Past one message (and past one frame to rohan): sent in parts, landed whole.
    let big: Vec<u8> = (0..(21 << 20) as u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &big).unwrap();
    let landed = s.json(&["put", &there, file.to_str().unwrap()]);
    assert_eq!(landed["bytes"].as_u64(), Some(21 << 20), "{landed}");
    let landed = std::path::Path::new(landed["path"].as_str().unwrap());
    assert!(std::fs::read(landed).unwrap() == big, "same bytes on rohan");
    let state = landed.ancestors().nth(3).unwrap(); // <state>/files/<terminal>/shot.png
    assert_eq!(std::fs::read_dir(state.join("uploads")).unwrap().count(), 0, "the upload was used up");
    std::fs::write(&file, b"small").unwrap();
    s.json(&["put", &there, file.to_str().unwrap()]);
}

#[test]
fn an_upload_is_taken_once_and_only_whole() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let mut conn = s.connect();
    let mut call = |method: &str, params: serde_json::Value| conn.call(1, method, params);
    let first = call("file.upload", serde_json::json!({"offset": 0, "data": "aGVsbG8g"})); // "hello "
    let id = first["result"]["upload"].clone();
    let resent = call("file.upload", serde_json::json!({"upload": id, "offset": 0, "data": "d29ybGQ="}));
    assert_eq!(resent["error"]["data"]["kind"], "invalid_params", "offset is what is staged: {resent}");
    let second = call("file.upload", serde_json::json!({"upload": id, "offset": 6, "data": "d29ybGQ="}));
    assert_eq!(second["result"]["bytes"], 11, "{second}");
    let put = call("terminal.put", serde_json::json!({"terminal": t, "name": "a.txt", "upload": id}));
    assert_eq!(std::fs::read(put["result"]["path"].as_str().unwrap()).unwrap(), b"hello world");
    let again = call("terminal.put", serde_json::json!({"terminal": t, "name": "a.txt", "upload": id}));
    assert_eq!(again["error"]["data"]["kind"], "not_found", "used up: {again}");
}
