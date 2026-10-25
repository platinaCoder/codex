use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

// Full startup continues after the composer first appears and can be slower under Rosetta in CI.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 30);
const FOCUS_INPUT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 5);
const FOCUS_PROBE_INPUT: &str = "focus-palette-24527";

#[path = "external_editor_tests.rs"]
mod external_editor;
#[path = "gov_cloud_guidance_tests.rs"]
mod gov_cloud_guidance;
#[path = "tui_mode_picker_tests.rs"]
mod tui_mode_picker;

#[test]
fn focus_gained_with_unanswered_palette_queries_preserves_immediate_input() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_config(codex_home.path(), &repo_root)?;

    let mut terminal = PtyCodex::start(&repo_root, codex_home, &["--no-alt-screen"])?;
    terminal.wait_for_startup()?;

    let startup_output_len = terminal.output.len();
    let focus_started = Instant::now();
    terminal.write_input(format!("\u{1b}[I{FOCUS_PROBE_INPUT}").as_bytes())?;
    terminal.wait_for_focus_input(FOCUS_PROBE_INPUT, focus_started, startup_output_len)?;

    let delayed_input = format!("{FOCUS_PROBE_INPUT}-delayed");
    let delayed_focus_started = Instant::now();
    terminal.write_input(b"\x1b[I")?;
    terminal.read_output(Duration::from_millis(/*millis*/ 20))?;
    terminal.write_input(delayed_input.as_bytes())?;
    terminal.wait_for_focus_input(&delayed_input, delayed_focus_started, startup_output_len)?;

    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_startup_honors_codex_home_symlink_opt_out() -> Result<()> {
    use core_test_support::responses;
    use wiremock::matchers::body_string_contains;

    core_test_support::skip_if_sandbox!(Ok(()));
    let workspace = tempfile::tempdir()?;
    let workspace_path = workspace.path().canonicalize()?;
    let codex_home = tempfile::tempdir()?;
    let target = tempfile::tempdir()?;
    let visualizations = codex_home.path().join("visualizations");
    std::os::unix::fs::symlink(target.path(), &visualizations)?;
    write_test_config(codex_home.path(), &workspace_path)?;

    let server = responses::start_mock_server().await;
    // Automatic thread-title requests must not consume the tool responses.
    let _title_mock = responses::mount_sse_once_match(
        &server,
        body_string_contains(r#"\"thread_source\":\"system\""#),
        responses::sse(vec![
            responses::ev_assistant_message("title", r#"{"title":"Symlink probe"}"#),
            responses::ev_completed("title"),
        ]),
    )
    .await;
    let _write_mock = responses::mount_sse_once_match(
        &server,
        body_string_contains(r#"\"thread_source\":\"user\""#),
        responses::sse(vec![
            responses::ev_exec_command_call_with_args(
                "write",
                &serde_json::json!({
                    "cmd": "printf ready > ready.txt",
                    "workdir": visualizations,
                    "shell": "/bin/sh",
                    "login": false,
                }),
            ),
            responses::ev_completed("startup"),
        ]),
    )
    .await;
    let completion_mock = responses::mount_sse_once_match(
        &server,
        body_string_contains(r#"\"thread_source\":\"user\""#),
        responses::sse(vec![
            responses::ev_assistant_message("ready", "symlink-opt-out-ready"),
            responses::ev_completed("complete"),
        ]),
    )
    .await;
    let base_url = server.uri();
    let visualizations = toml::Value::String(visualizations.display().to_string());
    let config_path = codex_home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        config_path,
        format!(
            "allow_symlinked_codex_home = true\n\
             sandbox_mode = \"workspace-write\"\n\
             approval_policy = \"never\"\n\
             {config}\n\
             [sandbox_workspace_write]\nwritable_roots = [{visualizations}]\n\
             [model_providers.test]\nname = \"Mock\"\n\
             base_url = \"{base_url}/v1\"\nwire_api = \"responses\"\n\
             requires_openai_auth = false\nsupports_websockets = false\n"
        ),
    )?;

    // A fresh home has no daemon. Writing through the link exercises the embedded server's
    // local executor and its sandbox policy.
    let mut terminal = PtyCodex::start(
        &workspace_path,
        codex_home,
        &[
            "--no-alt-screen",
            "-c",
            "model_provider=\"test\"",
            "Write ready.txt",
        ],
    )?;
    terminal.wait_for_startup()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        terminal.read_output(Duration::from_millis(/*millis*/ 50))?;
        if terminal.screen_contains("symlink-opt-out-ready") {
            let tool_output = completion_mock
                .function_call_output_text("write")
                .context("missing symlink write tool output")?;
            ensure!(
                tool_output.contains("Process exited with code 0"),
                "{tool_output}"
            );
            assert_eq!(std::fs::read(target.path().join("ready.txt"))?, b"ready");
            return Ok(());
        }
        if terminal.child.try_wait()?.is_some() {
            break;
        }
    }
    bail!(
        "interactive startup did not honor the symlink opt-out; screen:\n{}",
        terminal.screen_contents(),
    );
}

#[test]
fn default_owned_screen_entry_paints_before_sync_ends_and_exit_clears_inline_draft() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_config(codex_home.path(), &repo_root)?;
    let mut terminal = PtyCodex::start(&repo_root, codex_home, &[])?;
    terminal.wait_for_startup()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while !terminal.parser.screen().alternate_screen() && Instant::now() < deadline {
        terminal.read_output(Duration::from_millis(/*millis*/ 50))?;
        terminal.answer_startup_queries()?;
        terminal.ensure_running()?;
    }
    ensure!(
        terminal.parser.screen().alternate_screen(),
        "owned screen did not open"
    );
    terminal.wait_for_screen("GPT-5.6-Terra")?;
    ensure!(
        terminal.parser.screen().alternate_screen(),
        "fullscreen did not survive application startup"
    );
    let enter_alt = b"\x1b[?1049h";
    let begin_sync = b"\x1b[?2026h";
    let end_sync = b"\x1b[?2026l";
    let entry = terminal
        .output
        .windows(enter_alt.len())
        .position(|bytes| bytes == enter_alt)
        .context("missing owned-screen entry")?;
    let begin = terminal.output[..entry]
        .windows(begin_sync.len())
        .rposition(|bytes| bytes == begin_sync)
        .context("missing synchronized update before owned-screen entry")?;
    ensure!(
        !terminal.output[begin..entry]
            .windows(end_sync.len())
            .any(|bytes| bytes == end_sync),
        "owned-screen entry happened outside a synchronized update"
    );
    let end = entry
        + terminal.output[entry..]
            .windows(end_sync.len())
            .position(|bytes| bytes == end_sync)
            .context("missing synchronized update end after owned-screen entry")?;
    let (rows, cols) = terminal.parser.screen().size();
    let mut first_frame = vt100::Parser::new(rows, cols, /*scrollback_len*/ 0);
    first_frame.process(&terminal.output[..end]);
    let first_contents = first_frame.screen().contents();
    ensure!(
        first_contents.contains("OpenAI Codex")
            && first_contents.contains("Ask Codex to do anything"),
        "owned-screen synchronization ended before its first complete loading frame:\n{first_contents}"
    );
    let composer_row = first_contents
        .lines()
        .position(|line| line.contains("Ask Codex to do anything"))
        .context("missing composer in first owned-screen frame")?;
    assert_eq!(
        (
            first_contents.matches("Ask Codex to do anything").count(),
            first_frame.screen().cursor_position(),
            first_frame.screen().hide_cursor(),
        ),
        (1, (u16::try_from(composer_row)?, 2), false),
        "first owned-screen frame must contain one composer with its visible cursor"
    );
    terminal.write_input(b"\x1b[99;5u")?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        terminal.read_output(Duration::from_millis(/*millis*/ 50))?;
        if let Some(status) = terminal.child.try_wait()? {
            ensure!(status.success(), "owned screen exited with {status}");
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "owned screen did not exit; screen:\n{}",
            terminal.screen_contents()
        );
    }
    ensure!(
        !terminal.parser.screen().alternate_screen(),
        "alternate screen was not restored"
    );
    ensure!(
        !terminal
            .screen_contents()
            .contains("Ask Codex to do anything"),
        "owned-screen exit left the inline composer visible"
    );
    Ok(())
}

#[test]
fn fullscreen_transcript_can_opt_out_to_terminal_scrollback() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_config(codex_home.path(), &repo_root)?;
    let mut terminal = PtyCodex::start(
        &repo_root,
        codex_home,
        &["-c", "tui.fullscreen_transcript=false"],
    )?;
    terminal.wait_for_startup()?;
    terminal.wait_for_screen("GPT-5.6-Terra")?;
    ensure!(
        !terminal
            .output
            .windows(b"\x1b[?1049h".len())
            .any(|bytes| bytes == b"\x1b[?1049h"),
        "fullscreen opt-out entered the alternate screen"
    );
    Ok(())
}

pub(super) struct PtyCodex {
    master: File,
    child: Child,
    parser: vt100::Parser,
    output: Vec<u8>,
    cursor_answered: bool,
    palette_answered: bool,
    keyboard_answered: bool,
    _codex_home: TempDir,
}

impl PtyCodex {
    pub(super) fn start(
        repo_root: &Path,
        codex_home: TempDir,
        extra_args: &[&str],
    ) -> Result<Self> {
        let codex = codex_utils_cargo_bin::cargo_bin("codex-tui")
            .or_else(|_| codex_utils_cargo_bin::cargo_bin("codex"))?;
        Self::start_binary(
            &codex, repo_root, codex_home, extra_args, /*editor*/ None,
        )
    }

    /// Include the CLI dispatch futures when testing production stack headroom.
    pub(super) fn start_cli(
        repo_root: &Path,
        codex_home: TempDir,
        extra_args: &[&str],
    ) -> Result<Self> {
        let codex = codex_utils_cargo_bin::cargo_bin("codex")
            .context("build codex-cli and set CARGO_BIN_EXE_codex to its executable")?;
        Self::start_binary(
            &codex, repo_root, codex_home, extra_args, /*editor*/ None,
        )
    }

    fn start_binary(
        codex: &Path,
        repo_root: &Path,
        codex_home: TempDir,
        extra_args: &[&str],
        editor: Option<&Path>,
    ) -> Result<Self> {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut window_size = libc::winsize {
            ws_row: 32,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: `openpty` initializes both file descriptors on success, and the supplied window
        // size remains valid for the duration of the call.
        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                /*name*/ std::ptr::null_mut(),
                /*termp*/ std::ptr::null_mut(),
                &raw mut window_size,
            )
        };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("open focus-test pseudo-terminal");
        }

        // SAFETY: a successful `openpty` transfers ownership of both unique file descriptors.
        let master = File::from(unsafe { OwnedFd::from_raw_fd(master_fd) });
        // SAFETY: `slave_fd` is the second unique descriptor initialized by `openpty`.
        let slave = File::from(unsafe { OwnedFd::from_raw_fd(slave_fd) });
        let stdin = slave.try_clone().context("clone pseudo-terminal stdin")?;
        let stdout = slave.try_clone().context("clone pseudo-terminal stdout")?;

        let mut command = Command::new(codex);
        if let Some(editor) = editor {
            command.env("VISUAL", editor);
        }
        let child = command
            .args(extra_args)
            .arg("-C")
            .arg(repo_root)
            .env("TERM", "xterm-256color")
            // This PTY answers its own capability probes; it is not inside the caller's mux.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("STY")
            .env("TERM_PROGRAM", "kitty")
            .env_remove("TERM_PROGRAM_VERSION")
            .env("OPENAI_API_KEY", "focus-palette-test")
            .env("CODEX_HOME", codex_home.path())
            .stdin(stdin)
            .stdout(stdout)
            .stderr(slave)
            .spawn()
            .context("start Codex in focus-test pseudo-terminal")?;

        Ok(Self {
            master,
            child,
            parser: vt100::Parser::new(
                /*rows*/ 32, /*cols*/ 120, /*scrollback_len*/ 0,
            ),
            output: Vec::new(),
            cursor_answered: false,
            palette_answered: false,
            keyboard_answered: false,
            _codex_home: codex_home,
        })
    }

    pub(super) fn wait_for_startup(&mut self) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(/*millis*/ 50))?;
            self.answer_startup_queries()?;

            if self.palette_answered && self.screen_contains("OpenAI Codex") {
                return Ok(());
            }

            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "Codex exited before the focus test started ({status}); screen:\n{}",
                    self.screen_contents(),
                );
            }
        }

        bail!(
            "Codex did not initialize within {:?}; screen:\n{}",
            STARTUP_TIMEOUT,
            self.screen_contents(),
        );
    }

    fn wait_for_focus_input(
        &mut self,
        input: &str,
        focus_started: Instant,
        startup_output_len: usize,
    ) -> Result<()> {
        while focus_started.elapsed() < FOCUS_INPUT_TIMEOUT {
            self.read_output(Duration::from_millis(/*millis*/ 20))?;
            let focus_output = &self.output[startup_output_len..];
            ensure!(
                !contains_bytes(focus_output, b"\x1b]10;?")
                    && !contains_bytes(focus_output, b"\x1b]11;?"),
                "focus regain queried terminal colors after the startup palette was cached",
            );
            if self.screen_contains(input) {
                return Ok(());
            }
        }

        bail!(
            "focus-time palette refresh blocked or discarded {input:?} for more than {:?}; \
             screen:\n{}",
            FOCUS_INPUT_TIMEOUT,
            self.screen_contents(),
        );
    }

    fn answer_startup_queries(&mut self) -> Result<()> {
        if !self.cursor_answered && contains_bytes(&self.output, b"\x1b[6n") {
            self.write_input(b"\x1b[1;1R")?;
            self.cursor_answered = true;
        }

        if !self.keyboard_answered && contains_bytes(&self.output, b"\x1b[?u") {
            self.write_input(b"\x1b[?7u")?;
            self.write_input(b"\x1b[?1;2c")?;
            self.keyboard_answered = true;
        }

        if !self.palette_answered
            && contains_bytes(&self.output, b"\x1b]10;?")
            && contains_bytes(&self.output, b"\x1b]11;?")
        {
            self.write_input(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\")?;
            self.palette_answered = true;
        }

        Ok(())
    }

    pub(super) fn read_output(&mut self, timeout: Duration) -> Result<()> {
        let timeout_ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        // SAFETY: `descriptor` points to one initialized poll descriptor.
        let ready = unsafe {
            libc::poll(&mut descriptor, /*nfds*/ 1, timeout_ms)
        };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error).context("poll focus-test pseudo-terminal");
        }
        if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
            return Ok(());
        }

        let mut chunk = [0_u8; 8192];
        let count = self.master.read(&mut chunk)?;
        self.output.extend_from_slice(&chunk[..count]);
        self.parser.process(&chunk[..count]);
        Ok(())
    }

    pub(super) fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.master.write_all(bytes)?;
        self.master.flush()?;
        Ok(())
    }

    pub(super) fn screen_contains(&self, text: &str) -> bool {
        self.parser.screen().contents().contains(text)
    }

    pub(super) fn screen_contents(&self) -> String {
        self.parser.screen().contents()
    }

    pub(super) fn wait_for_screen(&mut self, text: &str) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(/*millis*/ 50))?;
            self.answer_startup_queries()?;
            if self.screen_contains(text) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                bail!("Codex exited while waiting for {text:?} ({status})");
            }
        }
        bail!("missing {text:?}; screen:\n{}", self.screen_contents())
    }

    pub(super) fn ensure_running(&mut self) -> Result<()> {
        ensure!(
            self.child.try_wait()?.is_none(),
            "Codex exited unexpectedly; screen:\n{}",
            self.screen_contents()
        );
        Ok(())
    }
}

impl Drop for PtyCodex {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn contains_bytes(buffer: &[u8], needle: &[u8]) -> bool {
    buffer.windows(needle.len()).any(|window| window == needle)
}

pub(super) fn write_test_config(codex_home: &Path, repo_root: &Path) -> Result<()> {
    let repo_root = repo_root.display();
    let config = format!(
        "model = \"gpt-5.6-terra\"\nmodel_provider = \"openai\"\n\
         suppress_unstable_features_warning = true\nanalytics.enabled = false\n\
         features.daemon_auto_start = false\n\
         notice.model_migrations.\"gpt-5.6-terra\" = \"gpt-6-sol\"\n\n\
         [projects.\"{repo_root}\"]\ntrust_level = \"trusted\"\n"
    );
    std::fs::write(codex_home.join("config.toml"), config)
        .context("write focus-test Codex configuration")?;
    std::fs::write(
        codex_home.join("auth.json"),
        r#"{"OPENAI_API_KEY":"focus-palette-test","tokens":null,"last_refresh":null}"#,
    )
    .context("write focus-test API-key authentication")
}

#[test]
fn no_daemon_skips_startup_and_discovery() -> Result<()> {
    for running in [false, true] {
        let workspace = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        write_test_config(home.path(), workspace.path())?;
        let config = home.path().join("config.toml");
        let contents = std::fs::read_to_string(&config)?;
        std::fs::write(
            config,
            contents.replace("features.daemon_auto_start = false\n", ""),
        )?;
        let socket_path = codex_app_server_client::app_server_control_socket_path(home.path())?;
        std::fs::create_dir_all(socket_path.as_path().parent().unwrap())?;
        let listener = if running {
            let listener = std::os::unix::net::UnixListener::bind(socket_path.as_path())?;
            listener.set_nonblocking(true)?;
            Some(listener)
        } else {
            None
        };
        let mut terminal = PtyCodex::start(workspace.path(), home, &["--no-daemon"])?;
        terminal.wait_for_startup()?;
        terminal.write_input(b"/status")?;
        terminal.wait_for_screen("show current session configuration")?;
        terminal.read_output(Duration::from_millis(/*millis*/ 200))?;
        terminal.write_input(b"\r")?;
        terminal.wait_for_screen("Model:")?;
        ensure!(
            !terminal
                ._codex_home
                .path()
                .join("app-server-daemon")
                .exists()
        );
        if let Some(listener) = listener {
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        ensure!(
            !String::from_utf8_lossy(&terminal.output)
                .contains("Running without the shared background server")
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_daemon_side_model_tools_and_main_dispatch_are_end_to_end() -> Result<()> {
    use core_test_support::responses;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use wiremock::Mock;
    use wiremock::Respond;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path_regex;

    struct SequenceResponder {
        call_count: AtomicUsize,
        bodies: Vec<String>,
    }

    fn is_thread_title_body(body: &serde_json::Value) -> bool {
        body.pointer("/client_metadata/x-codex-turn-metadata")
            .and_then(serde_json::Value::as_str)
            .and_then(|metadata| serde_json::from_str::<serde_json::Value>(metadata).ok())
            .and_then(|metadata| metadata.get("thread_source").cloned())
            .and_then(|source| source.as_str().map(str::to_owned))
            .is_some_and(|source| source == "thread_title")
    }

    impl Respond for SequenceResponder {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let is_thread_title = serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .is_some_and(|body| is_thread_title_body(&body));
            if is_thread_title {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(responses::sse(vec![
                        responses::ev_assistant_message(
                            "side-smoke-title",
                            r#"{"title":"SIDE smoke"}"#,
                        ),
                        responses::ev_completed("side-smoke-title-response"),
                    ]));
            }
            let index = self.call_count.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    self.bodies
                        .get(index)
                        .unwrap_or_else(|| panic!("unexpected model request {index}"))
                        .clone(),
                )
        }
    }

    fn submit(terminal: &mut PtyCodex, input: &str) -> Result<()> {
        terminal.write_input(format!("\x1b[200~{input}\x1b[201~").as_bytes())?;
        terminal.wait_for_screen(input)?;
        let deadline = Instant::now() + Duration::from_secs(/*secs*/ 30);
        while terminal.screen_contains("tab to queue message") {
            ensure!(
                Instant::now() < deadline,
                "composer remained busy before {input:?}"
            );
            terminal.read_output(Duration::from_millis(/*millis*/ 50))?;
            terminal.ensure_running()?;
        }
        terminal.write_input(b"\r")?;
        Ok(())
    }

    fn submit_typed(terminal: &mut PtyCodex, input: &str) -> Result<()> {
        for byte in input.as_bytes() {
            terminal.write_input(&[*byte])?;
            // Stay beyond the composer's 8ms paste-burst threshold so this
            // exercises literal user typing instead of paste normalization.
            std::thread::sleep(Duration::from_millis(/*millis*/ 10));
            terminal.read_output(Duration::from_millis(/*millis*/ 10))?;
        }
        terminal.wait_for_screen(input)?;
        terminal.write_input(b"\r")?;
        Ok(())
    }

    let workspace = tempfile::tempdir()?;
    let workspace_path = workspace.path().canonicalize()?;
    std::fs::write(
        workspace_path.join("side-smoke.txt"),
        "SIDE_MODEL_READ_NEEDLE\n",
    )?;
    let home = tempfile::tempdir()?;
    write_test_config(home.path(), &workspace_path)?;
    let model = responses::start_mock_server().await;
    let config_path = home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?.replace(
        "model_provider = \"openai\"",
        "model_provider = \"side-smoke\"",
    );
    std::fs::write(
        &config_path,
        format!(
            "{config}\n[model_providers.side-smoke]\nname = \"SIDE smoke\"\nbase_url = \"{}/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = false\nrequest_max_retries = 0\nstream_max_retries = 0\n",
            model.uri()
        ),
    )?;

    let model_responses = vec![
        responses::sse(vec![
            responses::ev_assistant_message("main-seeded", "MAIN_SEEDED"),
            responses::ev_completed("main-seeded-response"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("side-read-response"),
            responses::ev_function_call_with_namespace(
                "side-read",
                "mcp__codex_tui",
                "read_workspace_file",
                r#"{"path":"side-smoke.txt"}"#,
            ),
            responses::ev_completed("side-read-response"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("side-list-response"),
            responses::ev_function_call_with_namespace(
                "side-list",
                "mcp__codex_tui",
                "list_workspace_directory",
                r#"{"path":"."}"#,
            ),
            responses::ev_completed("side-list-response"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("side-search-response"),
            responses::ev_function_call_with_namespace(
                "side-search",
                "mcp__codex_tui",
                "search_workspace_text",
                r#"{"query":"SIDE_MODEL_READ_NEEDLE","path":"."}"#,
            ),
            responses::ev_completed("side-search-response"),
        ]),
        responses::sse(vec![
            responses::ev_assistant_message("side-done", "SIDE_MODEL_READS_OK"),
            responses::ev_completed("side-done-response"),
        ]),
        responses::sse(vec![
            responses::ev_assistant_message("main-done", "MAIN_SLASH_DISPATCH_OK"),
            responses::ev_completed("main-done-response"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("side-proactive-response"),
            responses::ev_function_call_with_namespace(
                "side-proactive",
                "mcp__codex_tui",
                "send_message_to_main",
                r#"{"prompt":"PROACTIVE_SIDE_MESSAGE"}"#,
            ),
            responses::ev_completed("side-proactive-response"),
        ]),
    ];
    Mock::given(method("POST"))
        .and(path_regex(r"/v1/responses$"))
        .respond_with(SequenceResponder {
            call_count: AtomicUsize::new(0),
            bodies: model_responses,
        })
        .mount(&model)
        .await;

    let mut terminal = if let Some(candidate) = std::env::var_os("AI_LAB_CODEX_STAGED") {
        PtyCodex::start_binary(
            Path::new(&candidate),
            &workspace_path,
            home,
            &[
                "--no-daemon",
                "--no-alt-screen",
                "Seed MAIN before SIDE smoke.",
            ],
            /*editor*/ None,
        )?
    } else {
        PtyCodex::start(
            &workspace_path,
            home,
            &[
                "--no-daemon",
                "--no-alt-screen",
                "Seed MAIN before SIDE smoke.",
            ],
        )?
    };
    terminal.wait_for_startup()?;
    terminal.wait_for_screen("MAIN_SEEDED")?;
    submit(&mut terminal, "/side")?;
    terminal.wait_for_screen("Side from main thread")?;
    submit(&mut terminal, "READ_SIDE_MARKER")?;
    terminal.wait_for_screen("SIDE_MODEL_READS_OK")?;

    let requests = responses::received_responses_requests(&model)
        .await
        .into_iter()
        .filter(|request| !is_thread_title_body(&request.body_json()))
        .collect::<Vec<_>>();
    ensure!(
        requests.len() == 5,
        "expected MAIN seed plus four SIDE model requests, got {}",
        requests.len()
    );
    ensure!(
        requests[2]
            .function_call_output("side-read")
            .to_string()
            .contains("SIDE_MODEL_READ_NEEDLE"),
        "read output missing from request: {}",
        requests[2].body_json()
    );
    ensure!(
        requests[3]
            .function_call_output("side-list")
            .to_string()
            .contains("side-smoke.txt"),
        "list output missing from request: {}",
        requests[3].body_json()
    );
    ensure!(
        requests[4]
            .function_call_output("side-search")
            .to_string()
            .contains("SIDE_MODEL_READ_NEEDLE"),
        "search output missing from request: {}",
        requests[4].body_json()
    );

    submit_typed(&mut terminal, "/main verify deterministic dispatch")?;
    let dispatch_deadline = Instant::now() + Duration::from_secs(/*secs*/ 5);
    let dispatched_requests = loop {
        terminal.read_output(Duration::from_millis(/*millis*/ 50))?;
        let requests = responses::received_responses_requests(&model)
            .await
            .into_iter()
            .filter(|request| !is_thread_title_body(&request.body_json()))
            .collect::<Vec<_>>();
        if requests.len() >= 6 || Instant::now() >= dispatch_deadline {
            break requests;
        }
    };
    ensure!(
        dispatched_requests.len() == 6,
        "/main did not start the paired MAIN turn; got {} model requests; screen:\n{}",
        dispatched_requests.len(),
        terminal.screen_contents()
    );
    terminal.wait_for_screen("MAIN_SLASH_DISPATCH_OK")?;
    submit(&mut terminal, "PROACTIVE_SIDE_SEND")?;
    terminal.wait_for_screen("Allow the codex_tui MCP server")?;
    ensure!(
        responses::received_responses_requests(&model)
            .await
            .into_iter()
            .filter(|request| !is_thread_title_body(&request.body_json()))
            .count()
            == 7,
        "proactive SIDE dispatch must pause before a follow-up model request"
    );
    ensure!(terminal.screen_contains("Allow"));

    Ok(())
}

#[test]
fn auto_daemon_start_failure_exits_with_manual_fallback_hint() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    write_test_config(home.path(), workspace.path())?;
    let config = home.path().join("config.toml");
    let contents = std::fs::read_to_string(&config)?;
    std::fs::write(
        config,
        contents.replace("features.daemon_auto_start = false\n", ""),
    )?;
    // An incomplete selected package must fail without installing a replacement.
    std::fs::create_dir_all(home.path().join("packages/app-server-daemon/current"))?;
    let mut terminal = PtyCodex::start(
        workspace.path(),
        home,
        &[
            "-c",
            "features.api_key_model_discovery=true",
            "-c",
            "suppress_unstable_features_warning=true",
        ],
    )?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        terminal.read_output(Duration::from_millis(/*millis*/ 50))?;
        terminal.answer_startup_queries()?;
        if let Some(status) = terminal.child.try_wait()? {
            ensure!(!status.success());
            let output = String::from_utf8_lossy(&terminal.output);
            let failure = &output[output.find("Error:").context("missing fatal error")?..];
            let failure = codex_ansi_escape::ansi_escape(failure)
                .to_string()
                .replace(
                    terminal
                        ._codex_home
                        .path()
                        .canonicalize()?
                        .to_string_lossy()
                        .as_ref(),
                    "[CODEX_HOME]",
                )
                .replace(
                    terminal._codex_home.path().to_string_lossy().as_ref(),
                    "[CODEX_HOME]",
                )
                .replace('\r', "");
            insta::assert_snapshot!("daemon_auto_start_failure", failure.trim());
            return Ok(());
        }
    }
    bail!("auto-start did not fail: {}", terminal.screen_contents())
}
