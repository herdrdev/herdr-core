//! Only an SSH bridge with destination opt-in may offer its own agent socket.

use std::io::{self, BufRead, Read, Write};

pub(super) fn forwarded_socket(paths: &crate::paths::Paths) -> Option<String> {
    let settings = crate::config::load(&paths.config).ok()?;
    if !settings.ssh_agent_forwarding || !crate::platform::SSH_LOGIN.iter().any(|name| std::env::var_os(name).is_some())
    {
        return None;
    }
    std::env::var("SSH_AUTH_SOCK").ok().filter(|path| !path.is_empty() && std::path::Path::new(path).is_absolute())
}

pub(super) fn offer(input: &mut impl BufRead, output: &mut impl Write, socket: &str) -> io::Result<()> {
    let mut line = Vec::new();
    (&mut *input).take(64 * 1024).read_until(b'\n', &mut line)?;
    if line.last() == Some(&b'\n')
        && let Ok(mut request) = serde_json::from_slice::<serde_json::Value>(&line)
        && request["method"] == "hello"
        && request["params"]["framed"] == true
        && let Some(params) = request["params"].as_object_mut()
    {
        params.insert("ssh_agent_socket".into(), socket.into());
        return writeln!(output, "{request}");
    }
    output.write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_framed_hello_is_augmented_and_all_other_fields_survive() {
        let source = b"{\"jsonrpc\":\"2.0\",\"id\":\"caller\",\"method\":\"hello\",\"params\":{\"generation\":1,\"framed\":true,\"features\":[\"resize_notifications\"],\"core\":{\"id\":\"other\",\"name\":\"laptop\"},\"ssh_agent_socket\":\"/untrusted\"}}\nmore bytes";
        let mut input = std::io::Cursor::new(source);
        let mut output = Vec::new();
        offer(&mut input, &mut output, "/forwarded").unwrap();
        let mut actual: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(actual["params"]["ssh_agent_socket"], "/forwarded");
        actual["params"]["ssh_agent_socket"] = "/untrusted".into();
        let expected: serde_json::Value =
            serde_json::from_slice(source.split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(actual, expected);
        let mut remaining = Vec::new();
        input.read_to_end(&mut remaining).unwrap();
        assert_eq!(remaining, b"more bytes");
    }

    #[test]
    fn plain_invalid_and_overlong_prefixes_pass_through_without_losing_bytes() {
        for source in [
            b"{\"method\":\"hello\",\"params\":{}}\nnext".to_vec(),
            b"{\"method\":\"server.status\",\"params\":{}}\nnext".to_vec(),
            b"not json\nnext".to_vec(),
            vec![b'x'; 70 * 1024],
        ] {
            let mut input = std::io::Cursor::new(&source);
            let mut output = Vec::new();
            offer(&mut input, &mut output, "/forwarded").unwrap();
            input.read_to_end(&mut output).unwrap();
            assert_eq!(output, source);
        }
    }
}
