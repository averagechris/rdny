//! Semantic validation and process-wide selection before browser I/O.

use super::*;

pub(super) fn pointer_target(
    selector: Option<String>,
    point: Option<PointerPoint>,
    pierce: bool,
) -> Result<PointerTarget> {
    match (selector, point) {
        (Some(selector), None) => Ok(PointerTarget::selector(ElementSelector::parse(
            selector, pierce,
        )?)),
        (None, Some(point)) => Ok(PointerTarget::point(point)),
        _ => anyhow::bail!("choose exactly one selector or coordinate pointer target"),
    }
}

pub(super) fn validate_download_output(format: OutputFormat, file: Option<&Path>) -> Result<()> {
    if format != OutputFormat::Human && file.is_none_or(|path| path == Path::new("-")) {
        anyhow::bail!(
            "download to stdout (omitted FILE or `FILE=-`) writes raw bytes and is incompatible with --format json/jsonl; use --format human or choose a file path"
        );
    }
    Ok(())
}

pub(super) fn validate_format_support_clap(
    format: OutputFormat,
    command: &Command,
) -> std::result::Result<(), clap::Error> {
    if format == OutputFormat::Human || command.supports_structured_output() {
        return Ok(());
    }
    Err(clap::Error::raw(
        clap::error::ErrorKind::ValueValidation,
        format!(
            "--format json/jsonl is not supported for `{}`; supported structured commands are {}",
            command.name(),
            arguments::STRUCTURED_COMMAND_INVENTORY
        ),
    ))
}

pub(super) fn configure_instance_selection(cli: &Cli) -> Result<()> {
    let env_state_dir = std::env::var_os("RDNY_STATE_DIR");
    validate_selection_inputs(
        cli.instance.as_deref(),
        cli.state_dir.as_deref(),
        env_state_dir.as_deref(),
        cli.command.targets_registered_instance(),
    )?;

    let selected = cli
        .instance
        .as_deref()
        .map(commands::instances::resolve_registered)
        .transpose()?;
    if let Some(selected) = &selected {
        // Bind state reads/mutations to the identity verified by the registry,
        // even if the directory is replaced between lookup and dispatch.
        crate::state::select_instance(&selected.instance_id)?;
    }
    let effective_dir = selected
        .as_ref()
        .map(|selected| selected.dir.as_path())
        .or(cli.state_dir.as_deref());
    if let Some(state_dir) = effective_dir {
        // SAFETY: rdny is still single-threaded here, before any command dispatch or
        // background work, so mutating the process environment cannot race other threads.
        unsafe { std::env::set_var("RDNY_STATE_DIR", state_dir) };
    }
    Ok(())
}

pub(super) fn validate_selection_inputs(
    instance: Option<&str>,
    cli_state_dir: Option<&std::path::Path>,
    env_state_dir: Option<&std::ffi::OsStr>,
    targets_registered_instance: bool,
) -> Result<()> {
    let Some(instance) = instance else {
        return Ok(());
    };
    if cli_state_dir.is_some() || env_state_dir.is_some() {
        anyhow::bail!(
            "instance selector `{instance}` conflicts with --state-dir/RDNY_STATE_DIR; unset the state-directory override or unset RDNY_INSTANCE/omit --instance"
        );
    }
    if !targets_registered_instance {
        anyhow::bail!(
            "--instance/RDNY_INSTANCE does not apply to this command; start/connect choose their destination with --state-dir, list/cleanup inspect the registry, and completion/sleep need no instance"
        );
    }
    Ok(())
}

pub(super) fn resolve_secret(
    value: Option<&str>,
    stdin: bool,
    file: Option<&std::path::Path>,
    fd: Option<i32>,
    label: &str,
) -> Result<String> {
    if let Some(value) = value {
        return Ok(value.to_string());
    }
    if stdin {
        let mut out = String::new();
        std::io::stdin()
            .read_to_string(&mut out)
            .with_context(|| format!("reading {label} from stdin"))?;
        return Ok(trim_one_trailing_newline(out));
    }
    if let Some(file) = file {
        return Ok(trim_one_trailing_newline(
            std::fs::read_to_string(file)
                .with_context(|| format!("reading {label} from {}", file.display()))?,
        ));
    }
    if let Some(fd) = fd {
        if fd < 0 {
            anyhow::bail!("invalid fd {fd} for {label}");
        }
        let path = std::path::PathBuf::from(format!("/dev/fd/{fd}"));
        return Ok(trim_one_trailing_newline(
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading {label} from fd {fd}"))?,
        ));
    }
    anyhow::bail!(
        "missing {label}; pass a value or --{}-stdin/--{}-file/--{}-fd",
        label.replace(' ', "-"),
        label.replace(' ', "-"),
        label.replace(' ', "-")
    )
}

pub(super) fn trim_one_trailing_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

pub(super) fn parse_address(address: &str) -> Result<(String, u16)> {
    if let Ok(socket) = address.parse::<std::net::SocketAddr>() {
        return Ok((socket.ip().to_string(), socket.port()));
    }
    let (host, port) = address.rsplit_once(':').ok_or_else(|| {
        crate::hint::hint_error(
            format!("invalid address `{address}`"),
            "use `<host>:<port>`, for example `127.0.0.1:9222`",
            None,
        )
    })?;
    if host.is_empty() || port.is_empty() || host.contains(':') {
        return Err(crate::hint::hint_error(
            format!("invalid address `{address}`"),
            "use `<host>:<port>`, for example `127.0.0.1:9222`",
            None,
        ));
    }
    let port = port.parse::<u16>().map_err(|_| {
        crate::hint::hint_error(
            format!("invalid port in address `{address}`"),
            "use a numeric TCP port, for example `127.0.0.1:9222`",
            None,
        )
    })?;
    Ok((host.to_string(), port))
}

pub(super) fn resolve_connect_target(
    arg: Option<&str>,
    config: &config::Config,
) -> Result<(String, u16)> {
    let Some(target) = arg else {
        let default = config
            .connect
            .as_ref()
            .and_then(|connect| connect.default.as_deref())
            .ok_or_else(|| {
                crate::hint::hint_error(
                    "missing connect target",
                    "pass `<host>:<port>` or configure [connect] default in the rdny config file",
                    None,
                )
            })?;
        return resolve_named_connect_target(default, config, true);
    };

    if target.contains(':') {
        parse_address(target)
    } else {
        resolve_named_connect_target(target, config, false)
    }
}

pub(super) fn resolve_named_connect_target(
    name: &str,
    config: &config::Config,
    from_default: bool,
) -> Result<(String, u16)> {
    if let Some(address) = config
        .connect
        .as_ref()
        .and_then(|connect| connect.targets.as_ref())
        .and_then(|targets| targets.get(name))
    {
        return parse_address(address);
    }

    let known = config
        .connect
        .as_ref()
        .and_then(|connect| connect.targets.as_ref())
        .map(|targets| targets.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let hint = if known.is_empty() {
        "pass `<host>:<port>` or add [connect.targets] entries to the rdny config file".to_string()
    } else {
        format!("known connect targets: {}", known.join(", "))
    };
    let message = if from_default {
        format!("default connect target `{name}` is not configured")
    } else {
        format!("unknown connect target `{name}`")
    };
    Err(crate::hint::hint_error(message, hint, None))
}

pub(super) fn resolve_js_expression(
    expression: Option<String>,
    reader: impl Read,
    stdin_is_tty: bool,
) -> Result<String> {
    match expression {
        Some(expression) if expression == "-" => read_js_expression_from_stdin(reader),
        Some(expression) => Ok(expression),
        None if stdin_is_tty => Err(crate::hint::hint_error(
            "missing JavaScript expression",
            "pass an expression or pipe one on stdin",
            None,
        )),
        None => read_js_expression_from_stdin(reader),
    }
}

pub(super) fn read_js_expression_from_stdin(reader: impl Read) -> Result<String> {
    std::io::read_to_string(reader).context("failed to read JavaScript expression from stdin")
}

pub(super) fn before_dispatch(cli: &Cli) -> Result<()> {
    if let Command::Download { file, .. } = &cli.command {
        validate_download_output(cli.format, file.as_deref())?;
    }
    configure_instance_selection(cli)
}

#[cfg(test)]
mod focused_tests {
    use super::*;

    #[test]
    fn semantic_output_validation_rejects_raw_structured_downloads() {
        let error = validate_download_output(OutputFormat::Json, None).unwrap_err();
        assert!(error.to_string().contains("raw bytes"));
        validate_download_output(OutputFormat::Human, None).unwrap();
    }

    #[test]
    fn semantic_selection_validation_precedes_registry_resolution() {
        let error =
            validate_selection_inputs(Some("browser"), Some(Path::new("state")), None, true)
                .unwrap_err();
        assert!(error.to_string().contains("conflicts with --state-dir"));

        let error = validate_selection_inputs(Some("browser"), None, None, false).unwrap_err();
        assert!(error.to_string().contains("does not apply to this command"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::arguments::{Cli, STRUCTURED_COMMAND_INVENTORY};
    use clap::Parser;
    use std::collections::BTreeMap;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args).unwrap().command
    }

    fn parse_cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }
    fn connect_config(default: Option<&str>, targets: &[(&str, &str)]) -> config::Config {
        config::Config {
            binaries: None,
            connect: Some(config::Connect {
                default: default.map(str::to_string),
                targets: Some(
                    targets
                        .iter()
                        .map(|(name, address)| (name.to_string(), address.to_string()))
                        .collect::<BTreeMap<_, _>>(),
                ),
            }),
        }
    }

    #[test]
    fn unsupported_structured_formats_are_clap_exit_2_errors() {
        for format in ["json", "jsonl"] {
            let cli = Cli::try_parse_adjusted_from([
                "rdny",
                "--format",
                format,
                "open",
                "https://example.com/",
            ])
            .expect("open supports structured output");
            assert!(matches!(cli.command, Command::Open { .. }));
        }

        let err = Cli::try_parse_adjusted_from(["rdny", "--format", "json", "title"])
            .expect_err("unsupported format should be a clap validation error");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        assert_eq!(err.exit_code(), 2);
        let message = err.to_string();
        assert!(message.contains("--format json/jsonl is not supported for `title`"));
        assert!(message.contains(STRUCTURED_COMMAND_INVENTORY));
    }

    #[test]
    fn structured_raw_download_is_rejected_before_dispatch() {
        for format in [OutputFormat::Json, OutputFormat::Jsonl] {
            let error = validate_download_output(format, Some(Path::new("-"))).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("raw bytes"));
            assert!(message.contains("--format human"));
            assert!(message.contains("file path"));
        }
        assert!(validate_download_output(OutputFormat::Human, Some(Path::new("-"))).is_ok());
        assert!(validate_download_output(OutputFormat::Json, Some(Path::new("out.bin"))).is_ok());
        assert!(validate_download_output(OutputFormat::Human, None).is_ok());
        assert!(validate_download_output(OutputFormat::Jsonl, None).is_err());
    }

    #[test]
    fn address_parsing() {
        assert_eq!(
            parse_address("127.0.0.1:9222").unwrap(),
            ("127.0.0.1".to_string(), 9222)
        );
        assert_eq!(
            parse_address("[::1]:9222").unwrap(),
            ("::1".to_string(), 9222)
        );
        assert!(parse_address("9222").is_err());
        assert!(parse_address("foo:bar").is_err());
    }

    #[test]
    fn parses_global_state_dir() {
        let cli = parse_cli(&["rdny", "--state-dir", "/tmp/x", "connect", "foo"]);
        assert_eq!(cli.state_dir, Some(PathBuf::from("/tmp/x")));
        assert!(
            matches!(cli.command, Command::Connect { address: Some(address), .. } if address == "foo")
        );
    }

    #[test]
    fn parses_global_instance_and_rejects_cli_state_dir_conflict() {
        let cli = parse_cli(&["rdny", "status", "--instance", "agent-a"]);
        assert_eq!(cli.instance.as_deref(), Some("agent-a"));
        assert!(
            Cli::try_parse_from([
                "rdny",
                "--instance",
                "agent-a",
                "--state-dir",
                "/tmp/state",
                "status",
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_instance_env_process_helper() {
        let Ok(mode) = std::env::var("RDNY_CLI_INSTANCE_ENV_HELPER") else {
            return;
        };
        match mode.as_str() {
            "env" => {
                let cli = parse_cli(&["rdny", "status"]);
                assert_eq!(cli.instance.as_deref(), Some("from-env"));
            }
            "override" => {
                let cli = parse_cli(&["rdny", "--instance", "from-cli", "status"]);
                assert_eq!(cli.instance.as_deref(), Some("from-cli"));
            }
            _ => panic!("unknown helper mode"),
        }
    }

    #[test]
    fn rdny_instance_is_equivalent_and_cli_overrides_it() {
        for mode in ["env", "override"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("cli::tests::cli_instance_env_process_helper")
                .arg("--exact")
                .env("RDNY_INSTANCE", "from-env")
                .env("RDNY_CLI_INSTANCE_ENV_HELPER", mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "helper mode {mode} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn effective_instance_conflicts_with_environment_state_dir() {
        let error = validate_selection_inputs(
            Some("agent-a"),
            None,
            Some(std::ffi::OsStr::new("/tmp/state")),
            true,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("conflicts with --state-dir/RDNY_STATE_DIR"));

        assert!(
            validate_selection_inputs(
                None,
                Some(std::path::Path::new("/cli")),
                Some(std::ffi::OsStr::new("/env")),
                true,
            )
            .is_ok()
        );
    }

    #[test]
    fn non_target_commands_reject_instance_selector() {
        for command in [
            parse(&["rdny", "start"]),
            parse(&["rdny", "connect", "127.0.0.1:9222"]),
            parse(&["rdny", "list"]),
            parse(&["rdny", "cleanup"]),
            parse(&["rdny", "completion", "bash"]),
            parse(&["rdny", "sleep", "1"]),
        ] {
            assert!(!command.targets_registered_instance());
            let error = validate_selection_inputs(Some("agent-a"), None, None, false).unwrap_err();
            assert!(format!("{error:#}").contains("does not apply to this command"));
        }
        for command in [
            parse(&["rdny", "status"]),
            parse(&["rdny", "stop"]),
            parse(&["rdny", "title"]),
            parse(&["rdny", "screenshot"]),
            parse(&["rdny", "pages"]),
            parse(&["rdny", "start-video"]),
            parse(&["rdny", "stop-video"]),
            parse(&["rdny", "key", "Enter"]),
            parse(&["rdny", "pointer", "move", "--at", "1,2"]),
        ] {
            assert!(command.targets_registered_instance());
        }
    }

    #[test]
    fn resolves_connect_target_explicit_address() {
        assert_eq!(
            resolve_connect_target(Some("127.0.0.1:9333"), &config::Config::default()).unwrap(),
            ("127.0.0.1".to_string(), 9333)
        );
    }

    #[test]
    fn resolves_connect_target_named_hit() {
        let config = connect_config(None, &[("helium", "127.0.0.1:9333")]);
        assert_eq!(
            resolve_connect_target(Some("helium"), &config).unwrap(),
            ("127.0.0.1".to_string(), 9333)
        );
    }

    #[test]
    fn resolves_connect_target_named_miss_lists_known_names() {
        let config = connect_config(None, &[("helium", "127.0.0.1:9333")]);
        let err = resolve_connect_target(Some("chrome"), &config).unwrap_err();
        assert!(format!("{err}").contains("helium"));
    }

    #[test]
    fn resolves_connect_target_default() {
        let config = connect_config(Some("helium"), &[("helium", "127.0.0.1:9333")]);
        assert_eq!(
            resolve_connect_target(None, &config).unwrap(),
            ("127.0.0.1".to_string(), 9333)
        );
    }

    #[test]
    fn rejects_connect_target_without_default() {
        let err = resolve_connect_target(None, &config::Config::default()).unwrap_err();
        assert!(format!("{err}").contains("default"));
    }

    #[test]
    fn rejects_connect_default_pointing_at_missing_target() {
        let config = connect_config(Some("helium"), &[]);
        let err = resolve_connect_target(None, &config).unwrap_err();
        assert!(format!("{err}").contains("default connect target `helium`"));
    }

    #[test]
    fn resolves_js_expression_from_argument() {
        let expression = resolve_js_expression(
            Some("document.title".to_string()),
            "ignored".as_bytes(),
            true,
        )
        .unwrap();

        assert_eq!(expression, "document.title");
    }

    #[test]
    fn resolves_js_expression_from_dash_stdin() {
        let expression =
            resolve_js_expression(Some("-".to_string()), "a\nb\n".as_bytes(), true).unwrap();

        assert_eq!(expression, "a\nb\n");
    }

    #[test]
    fn resolves_js_expression_from_omitted_non_tty_stdin() {
        let expression =
            resolve_js_expression(None, "(() => {\n  return 42;\n})()".as_bytes(), false).unwrap();

        assert_eq!(expression, "(() => {\n  return 42;\n})()");
    }

    #[test]
    fn rejects_omitted_js_expression_on_tty() {
        let err = resolve_js_expression(None, "ignored".as_bytes(), true).unwrap_err();

        assert!(format!("{err}").contains("pass an expression or pipe one on stdin"));
    }
}
