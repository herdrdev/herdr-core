//! Human docs use the shipped help, without starting a core or running examples.
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

type Reference = BTreeMap<String, String>;

fn read_help(words: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-core"))
        .args(words)
        .env("NO_COLOR", "1")
        .env("COLUMNS", "100")
        .output()
        .unwrap();
    assert!(output.status.success(), "help {words:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout)
        .unwrap()
        .replace("\r\n", "\n")
        .replace("Usage: herdr-core.exe", "Usage: hc")
        .replace("Usage: herdr-core", "Usage: hc")
}

fn reference() -> Reference {
    let mut reference = Reference::new();
    let mut pending = vec![String::new()];
    while let Some(path) = pending.pop() {
        let mut words: Vec<_> = path.split_whitespace().collect();
        // The single --help form is the short human tour, not clap's tree.
        if path.is_empty() {
            words.push("--json");
        }
        words.push("--help");
        let help = read_help(&words);
        if let Some((_, commands)) = help.split_once("Commands:\n") {
            for line in commands.lines().take_while(|line| line.starts_with("  ")) {
                let child = line.split_whitespace().next().unwrap();
                if child != "help" {
                    pending.push(format!("{path} {child}").trim().to_owned());
                }
            }
        }
        reference.insert(path, help);
    }
    reference
}

fn flag_value(help: &str, name: &str) -> Option<bool> {
    help.lines().filter(|line| line.starts_with("  ")).find_map(|line| {
        let declaration = line.trim().split("  ").next().unwrap();
        declaration
            .split_whitespace()
            .any(|word| word.trim_end_matches(',') == name)
            .then(|| declaration.contains(['<', '[']))
    })
}

fn render(reference: &Reference) -> String {
    let mut doc = String::from(
        "# Commands\n\nGenerated from the built binary's help. Do not edit by hand.\n\
         Run `just docs` to refresh it and `just docs-check` to check it.\n\n\
         [Start here](../README.md) for a short tour.\n",
    );
    for (path, help) in reference {
        let title = format!("hc {path}").trim().to_owned();
        let help = if path.is_empty() { read_help(&["--help"]) } else { help.clone() };
        doc.push_str(&format!("\n## {title}\n\n```text\n{help}```\n"));
    }
    doc
}

fn check_span(span: &str, reference: &Reference) -> Result<(), String> {
    // Quoted payloads are opaque; this is a docs check, not a shell parser.
    let unquoted =
        span.split(['\'', '"']).enumerate().map(|(i, s)| if i % 2 == 0 { s } else { "_" }).collect::<Vec<_>>().join("");
    let tokens: Vec<_> = unquoted.split_whitespace().collect();
    let start = tokens.iter().enumerate().position(|(i, w)| {
        *w == "hc" && (tokens[..i].iter().all(|w| w.contains('=')) || i > 0 && tokens[i - 1] == "|")
    });
    if start.is_none() && !tokens.first().is_some_and(|w| w.starts_with("--") && *w != "---") {
        return Ok(());
    }
    let mut path = String::new();
    let mut positionals = 0usize;
    let mut i = start.map_or(0, |i| i + 1);
    while i < tokens.len() {
        let word = tokens[i];
        if matches!(word, "--" | "#" | "|" | ";" | "&&") {
            break;
        }
        if word.starts_with("--") {
            let flag = word.split('=').next().unwrap();
            let known = if start.is_some() {
                flag_value(&reference[&path], flag).or_else(|| flag_value(&reference[""], flag))
            } else {
                reference.values().find_map(|help| flag_value(help, flag))
            }
            .or_else(|| ["--skill", "--licenses"].contains(&flag).then_some(false));
            let Some(takes_value) = known else {
                return Err(format!("unknown flag {flag} for hc {path}"));
            };
            if takes_value && !word.contains('=') {
                i += 1;
            }
        } else if start.is_some() && !word.starts_with('-') {
            if path.is_empty() || reference.keys().any(|p| p.starts_with(&format!("{path} "))) {
                if word.starts_with(['<', '[']) {
                    break;
                }
                path = format!("{path} {word}").trim().into();
                if !reference.contains_key(&path) {
                    return Err(format!("unknown command hc {path}"));
                }
            } else {
                positionals += 1;
                if matches!(path.as_str(), "run" | "exec") || (path == "start" && positionals >= 2) {
                    break;
                }
            }
        }
        i += 1;
    }
    Ok(())
}

fn check_markdown(path: &str, text: &str, reference: &Reference) -> Vec<String> {
    let mut errors = Vec::new();
    for (line, source) in text.lines().enumerate() {
        for span in source.split('`') {
            if let Err(error) = check_span(span, reference) {
                errors.push(format!("{path}:{}: {error}", line + 1));
            }
        }
    }
    errors
}

#[test]
fn help_reference_and_examples_are_current() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let reference = reference();
    let rendered = render(&reference);
    let generated = root.join("docs/commands.md");
    if std::env::var_os("HC_UPDATE_DOCS").is_some() {
        std::fs::create_dir_all(generated.parent().unwrap()).unwrap();
        std::fs::write(&generated, &rendered).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(generated).unwrap_or_default().replace("\r\n", "\n"),
        rendered,
        "docs/commands.md is stale; run just docs"
    );
    let mut paths = vec![root.join("README.md")];
    paths.extend(
        std::fs::read_dir(root.join("docs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md") && p.file_name().unwrap() != "commands.md"),
    );
    paths.sort();
    let errors: Vec<_> = paths
        .into_iter()
        .flat_map(|path| {
            check_markdown(
                path.strip_prefix(&root).unwrap().to_str().unwrap(),
                &std::fs::read_to_string(&path).unwrap(),
                &reference,
            )
        })
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn checker_catches_drift_without_treating_program_arguments_as_hc_flags() {
    let reference = reference();
    for good in [
        "hc machine add --via 'ssh -p 2222 host' --name box",
        "hc create --name worker --label role=yolo",
        "hc run --bg pi --provider whatever",
        "hc start worker --bg pi --provider whatever",
        "hc exec --on box git diff --stat",
        "hc file read --on box /srv/app/a.rs --limit 10",
        "hc send review 'check hc nonsense --not-an-hc-flag'",
        "--via",
        "--old X --new Y",
        "HC_SESSION=scratch hc ls",
        "printf hello | hc exec -i cat",
    ] {
        assert!(check_span(good, &reference).is_ok(), "{good}");
    }
    for bad in [
        "hc made-up",
        "hc machine nope",
        "hc run --bgg pi",
        "hc start worker --bgg pi",
        "hc ls --via box",
        "hc --agent-help",
        "--agent-help",
        "--old X --typo Y",
    ] {
        assert!(check_span(bad, &reference).is_err(), "{bad}");
    }
    let errors = check_markdown("README.md", "prose hc nonsense\n`hc nope`\n```sh\nhc ls --oops\n```", &reference);
    assert_eq!(errors.len(), 2);
    assert!(errors[0].starts_with("README.md:2:"));
    assert!(errors[1].starts_with("README.md:4:"));
}
