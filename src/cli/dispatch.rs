//! Command dispatch and lifecycle sequencing after semantic validation.

use super::*;

pub(super) fn execute(cli: Cli) -> Result<()> {
    let timeout = cli.timeout.get();
    let command_budget = match &cli.command {
        Command::Logs(args) if !args.follow => args.duration.unwrap_or(cli.timeout).get(),
        Command::Sleep { seconds } => seconds.get(),
        _ => timeout,
    };
    let deadline = session::Deadline::after(command_budget);
    // Avoid unconditional lifecycle state reads before lifecycle commands have
    // taken .lifecycle.lock. Page commands may opt into draining once connected.
    let mut drain_after_dispatch = false;
    let mut page_session = None;
    macro_rules! sess {
        () => {{
            if page_session.is_none() {
                page_session = Some(session::connect(deadline, timeout)?);
                drain_after_dispatch = page_session
                    .as_ref()
                    .is_some_and(session::PageSession::is_recording);
            }
            page_session.as_mut().expect("session just connected")
        }};
    }
    match cli.command {
        Command::Start(args) => {
            let _lifecycle = crate::state::lifecycle_lock()?;
            refuse_occupied_lifecycle(&_lifecycle, "start", deadline)?;
            let opts = LaunchOpts {
                show: args.show,
                insecure: args.insecure,
                extra_args: vec![],
                label: args.label,
            };
            let instance_id = crate::state::new_instance_id();
            let mut launched = crate::broker::launch_armed(
                &opts,
                crate::state::browser_storage()?,
                instance_id,
                deadline,
            )?;
            #[cfg(debug_assertions)]
            if let Some(marker) = std::env::var_os("RDNY_TEST_CRASH_BEFORE_BROKER_COMMIT") {
                // Test-only abrupt client death: do not run destructors, so the
                // broker must observe startup-socket EOF and clean itself up.
                let endpoint = launched.state.endpoint.as_ref().expect("managed endpoint");
                let evidence = json!({
                    "broker": endpoint.broker_pid,
                    "browser": launched.state.pid,
                    "socket": endpoint.socket,
                });
                std::fs::write(marker, serde_json::to_vec(&evidence)?)?;
                unsafe { libc::_exit(86) }
            }
            let publish_state = launched.state.clone();
            if let Err(err) =
                crate::state::replace_lifecycle_and_commit(&_lifecycle, &publish_state, || {
                    launched.commit()
                })
            {
                return Err(err.context("publishing and committing managed broker ownership; browser was terminated and previous state preserved"));
            }
            let state = launched.state.clone();
            let browser = state
                .browser_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "browser".to_string());
            let pid = state
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "attached".to_string());
            println!("started {browser} pid {pid} via authenticated local broker");
        }
        Command::Connect {
            address,
            allow_remote,
        } => {
            let _lifecycle = crate::state::lifecycle_lock()?;
            refuse_occupied_lifecycle(&_lifecycle, "connect", deadline)?;
            let config = config::load()?;
            let (host, port) = resolve_connect_target(address.as_deref(), &config)?;
            let state = browser::connect_with_policy_until(&host, port, allow_remote, deadline)?;
            crate::state::replace_lifecycle(&_lifecycle, &state)
                .context("publishing attached browser ownership; previous state preserved")?;
            println!("connected to {host}:{port}");
        }
        Command::Stop => {
            let _lifecycle = crate::state::lifecycle_lock()?;
            let (state, generation) = match crate::state::inspect_for_lifecycle(&_lifecycle)? {
                crate::state::Inspection::Valid(state, generation) => (state, generation),
                crate::state::Inspection::Missing => {
                    return Err(crate::state::require().unwrap_err());
                }
                crate::state::Inspection::Malformed(m) => anyhow::bail!(
                    "state file contains malformed JSON: {}; not stopping or clearing; quarantine or remove it explicitly",
                    m.message()
                ),
                crate::state::Inspection::Incompatible(e, _) => anyhow::bail!(
                    "state file has incompatible schema: {e}; not stopping or clearing"
                ),
            };
            let observed_id = state.instance_id.clone();
            let outcome = browser::stop_until(&state, deadline)?;
            if !crate::state::clear_observed_lifecycle(
                &_lifecycle,
                observed_id.as_deref(),
                &generation,
            )? {
                anyhow::bail!(
                    "state changed while stopping; preserved concurrent replacement and did not clear it"
                )
            }
            match outcome {
                browser::StopOutcome::Stopped => println!("stopped"),
                browser::StopOutcome::Detached => println!("detached (browser left running)"),
            }
        }
        Command::Status(args) => match crate::state::load()? {
            None => {
                if cli.format == OutputFormat::Human {
                    println!("no session")
                } else {
                    cli.format.emit(&json!({"schemaVersion":1,"kind":"status","status":"missing","healthy":false}))?;
                }
                if args.check {
                    std::process::exit(1);
                }
            }
            Some(state) => match browser::status_until(&state, deadline)? {
                BrowserStatus::Running { browser } => {
                    let pid = state
                        .pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "attached".to_string());
                    let label = state
                        .label
                        .as_deref()
                        .map(|label| format!(" label={label}"))
                        .unwrap_or_default();
                    if cli.format == OutputFormat::Human {
                        if state.endpoint.is_some() {
                            println!(
                                "running: {browser} via authenticated local broker (pid {pid}){label}"
                            );
                        } else {
                            println!(
                                "running: {browser} on {}:{} (pid {pid}){label}",
                                state.host, state.port
                            );
                        }
                    } else {
                        cli.format.emit(&json!({"schemaVersion":1,"kind":"status","status":"running","healthy":true,"browser":browser,"host":state.host,"port":state.port,"pid":state.pid,"instance":state.instance_id,"target":state.target_id,"label":state.label}))?;
                    }
                }
                BrowserStatus::Stale => {
                    if cli.format == OutputFormat::Human {
                        println!("stale: state file exists but browser is not responding");
                    } else {
                        cli.format.emit(&json!({"schemaVersion":1,"kind":"status","status":"stale","healthy":false,"host":state.host,"port":state.port,"pid":state.pid,"instance":state.instance_id,"target":state.target_id}))?;
                    }
                    if args.check {
                        std::process::exit(1);
                    }
                }
            },
        },
        Command::List => commands::instances::list_format_until(cli.format, deadline)?,
        Command::Completion { shell } => {
            let mut cmd = arguments::adjusted_command();
            clap_complete::generate(shell, &mut cmd, "rdny", &mut std::io::stdout());
        }
        Command::Skills(args) => match args.command {
            SkillsCommand::List => commands::skills::list(cli.format)?,
            SkillsCommand::Show { name } => commands::skills::show(&name, cli.format)?,
            SkillsCommand::Install(args) => {
                commands::skills::install(&args.names, args.dir, args.force, cli.format)?
            }
        },
        Command::Cleanup(args) => {
            commands::instances::cleanup_until(args.all, cli.format, deadline)?
        }
        Command::Open { url, policy } => {
            let policy = (&policy).into();
            // Validate before `sess!()` connects so rejected input causes no browser I/O.
            commands::nav::normalize_url(&url, &policy)?;
            let opened = commands::nav::open_with_policy(sess!(), &url, &policy)?;
            if cli.format == OutputFormat::Human {
                println!("{}", opened.human_summary());
            } else {
                cli.format.emit(&json!({
                    "schemaVersion": 1,
                    "kind": "open",
                    "url": opened.url,
                }))?;
            }
        }
        Command::Back => commands::nav::back(sess!())?,
        Command::Forward => commands::nav::forward(sess!())?,
        Command::Reload(args) => commands::nav::reload(sess!(), args.hard)?,
        Command::ClearCache => commands::nav::clear_cache(sess!())?,
        Command::Cookie(args) => match args.command {
            CookieCommand::Set(args) => {
                let value = resolve_secret(
                    args.value.as_deref(),
                    args.value_stdin,
                    args.value_file.as_deref(),
                    args.value_fd,
                    "cookie value",
                )?;
                commands::cookie::set(
                    sess!(),
                    &commands::cookie::SetCookie {
                        name: &args.name,
                        value: &value,
                        domain: &args.domain,
                        path: &args.path,
                        secure: args.secure,
                        http_only: args.http_only,
                        same_site: args.same_site.map(Into::into),
                    },
                )?
            }
            CookieCommand::List => commands::cookie::list_format(sess!(), cli.format)?,
            CookieCommand::Get { name } => commands::cookie::get(sess!(), &name)?,
            CookieCommand::Delete(args) => {
                commands::cookie::delete(sess!(), &args.name, &args.domain, &args.path)?
            }
        },
        Command::Url => commands::pageinfo::url(sess!())?,
        Command::Title => commands::pageinfo::title(sess!())?,
        Command::Html { selector, pierce } => {
            let selector = selector
                .map(|selector| ElementSelector::parse(selector, pierce))
                .transpose()?;
            commands::pageinfo::html(sess!(), selector.as_ref())?
        }
        Command::Text { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::pageinfo::text(sess!(), &selector)?
        }
        Command::Attr {
            selector,
            name,
            pierce,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::pageinfo::attr(sess!(), &selector, &name)?
        }
        Command::Pdf(args) => {
            let artifact = commands::pageinfo::pdf(sess!(), args.file.as_deref(), args.force)?;
            cli.format.emit_artifact(&artifact)?;
        }
        Command::Js { expression } => {
            let stdin = std::io::stdin();
            let stdin_is_tty = stdin.is_terminal();
            let expression = resolve_js_expression(expression, stdin, stdin_is_tty)?;
            commands::interact::js(sess!(), &expression)?
        }
        Command::Logs(args) => commands::logs::logs_format(sess!(), args.follow, cli.format)?,
        Command::Viewport(args) => commands::viewport::viewport_format(
            sess!(),
            args.width,
            args.height,
            args.scale,
            args.mobile,
            args.reset,
            cli.format,
        )?,
        Command::Click { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::click(sess!(), &selector)?
        }
        Command::Key(args) => crate::input::key(sess!(), &args.chord)?,
        Command::Pointer(args) => match args.command {
            PointerCommand::Move(args) => {
                let target = pointer_target(args.selector, args.at, args.pierce)?;
                crate::input::pointer_move(sess!(), &target)?
            }
            PointerCommand::Down(args) => {
                let target =
                    pointer_target(args.target.selector, args.target.at, args.target.pierce)?;
                crate::input::pointer_down(sess!(), &target, args.button)?
            }
            PointerCommand::Up(args) => {
                let target =
                    pointer_target(args.target.selector, args.target.at, args.target.pierce)?;
                crate::input::pointer_up(sess!(), &target, args.button)?
            }
            PointerCommand::Drag(args) => {
                let from = pointer_target(args.from_selector, args.from_at, args.pierce)?;
                let to = pointer_target(args.to_selector, args.to_at, args.pierce)?;
                crate::input::drag(
                    sess!(),
                    &from,
                    &to,
                    args.button,
                    args.steps,
                    Duration::from_millis(args.duration_ms),
                )?
            }
        },
        Command::Input(args) => {
            let selector = ElementSelector::parse(args.selector, args.pierce)?;
            let text = resolve_secret(
                args.text.as_deref(),
                args.text_stdin,
                args.text_file.as_deref(),
                args.text_fd,
                "input text",
            )?;
            commands::interact::input(sess!(), &selector, &text)?
        }
        Command::Clear { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::clear(sess!(), &selector)?
        }
        Command::File {
            selector,
            path,
            pierce,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::file(sess!(), &selector, &path)?
        }
        Command::Download {
            selector,
            pierce,
            max_bytes,
            force,
            file,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            if let Some(artifact) =
                commands::interact::download(sess!(), &selector, file.as_deref(), force, max_bytes)?
            {
                cli.format.emit_artifact(&artifact)?;
            }
        }
        Command::Select {
            selector,
            value,
            pierce,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::select(sess!(), &selector, &value)?
        }
        Command::Submit { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::submit(sess!(), &selector)?
        }
        Command::Hover { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::hover(sess!(), &selector)?
        }
        Command::Focus { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::focus(sess!(), &selector)?
        }
        Command::Wait { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::wait::wait(sess!(), &selector)?
        }
        Command::Waitload => commands::wait::waitload(sess!())?,
        Command::Waitstable(args) => {
            if args.quiet_ms == 500 {
                commands::wait::waitstable(sess!())?
            } else {
                commands::wait::waitstable_quiet(
                    sess!(),
                    std::time::Duration::from_millis(args.quiet_ms),
                )?
            }
        }
        Command::Waitidle(args) => {
            if args.quiet_ms == 500 {
                commands::wait::waitidle(sess!())?
            } else {
                commands::wait::waitidle_quiet(
                    sess!(),
                    std::time::Duration::from_millis(args.quiet_ms),
                )?
            }
        }
        Command::Sleep { .. } => commands::wait::sleep(deadline)?,
        Command::Screenshot(args) => {
            let state = crate::state::require()?;
            let artifact = commands::shot::screenshot(
                sess!(),
                args.width,
                args.height,
                args.file.as_deref(),
                args.force,
                state.viewport.as_ref(),
            )?;
            cli.format.emit_artifact(&artifact)?;
        }
        Command::ScreenshotEl {
            selector,
            pierce,
            force,
            file,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            let artifact =
                commands::shot::screenshot_el(sess!(), &selector, file.as_deref(), force)?;
            cli.format.emit_artifact(&artifact)?;
        }
        Command::Pages => commands::tabs::pages_format_until(cli.format, deadline)?,
        Command::Page { index } => commands::tabs::page_until(index, deadline)?,
        Command::Newpage { url, policy } => commands::tabs::newpage_with_policy(
            url.as_deref(),
            timeout,
            deadline,
            &(&policy).into(),
        )?,
        Command::StartVideo => commands::video::start()?,
        Command::StopVideo(args) => {
            // Assemble even when the browser is gone: frames on disk
            // should never be stranded behind a dead session.
            let mut live = session::connect(deadline, timeout).ok();
            let artifact = commands::video::stop(live.as_mut(), args.file.as_deref(), args.force)?;
            cli.format.emit_artifact(&artifact)?;
        }
    }
    if drain_after_dispatch && let Some(session) = page_session.as_mut() {
        session.drain_events(std::time::Duration::from_millis(300))?;
    }
    Ok(())
}

pub(super) fn refuse_occupied_lifecycle(
    lock: &crate::state::LifecycleLock,
    action: &str,
    deadline: session::Deadline,
) -> Result<()> {
    match crate::state::inspect_for_lifecycle(lock)? {
        crate::state::Inspection::Missing => Ok(()),
        crate::state::Inspection::Malformed(m) => anyhow::bail!(
            "refusing to {action}: state file contains malformed JSON: {}; run `rdny cleanup` or remove/quarantine the corrupt state after verifying no browser owns it",
            m.message()
        ),
        crate::state::Inspection::Incompatible(e, _) => anyhow::bail!(
            "refusing to {action}: state file has incompatible schema: {e}; upgrade rdny or inspect/remove the state explicitly"
        ),
        crate::state::Inspection::Valid(state, _) => {
            if state.endpoint.is_some() {
                if crate::broker::ping(&state, deadline).is_ok() {
                    return Err(crate::hint::hint_error(
                        format!(
                            "refusing to {action}: live managed broker already owns this state dir"
                        ),
                        "run `rdny stop` first, or use a different --state-dir",
                        None,
                    ));
                }
                return Ok(());
            }
            if let Some(id) = &state.process_identity {
                crate::process_identity::validate_persisted(
                    state.pid,
                    state.browser_path.as_deref(),
                    state.user_data_dir.as_deref(),
                    id,
                )
                .with_context(|| {
                    format!("refusing to {action}: existing managed state identity is inconsistent")
                })?;
            }
            let reachable =
                crate::cdp::http::version_until(&state.host, state.port, deadline.instant())
                    .is_ok();
            match crate::process_identity::classify(
                state.pid,
                state.process_identity.as_ref(),
                reachable,
            ) {
                crate::process_identity::ProcessClass::ManagedMatching
                | crate::process_identity::ProcessClass::AttachedReachable
                | crate::process_identity::ProcessClass::LegacyUnverifiable => {
                    let owner = state
                        .pid
                        .map(|pid| format!("managed pid {pid}"))
                        .unwrap_or_else(|| "attached browser".to_string());
                    Err(crate::hint::hint_error(
                        format!(
                            "refusing to {action}: live {owner} already owns state dir on port {}",
                            state.port
                        ),
                        "run `rdny stop` first, or use a different --state-dir",
                        None,
                    ))
                }
                crate::process_identity::ProcessClass::ManagedDead
                | crate::process_identity::ProcessClass::AttachedDead
                | crate::process_identity::ProcessClass::PidReusedOrUnrelated => Ok(()),
            }
        }
    }
}

pub(super) fn run(cli: Cli) -> Result<()> {
    execute(cli)
}
