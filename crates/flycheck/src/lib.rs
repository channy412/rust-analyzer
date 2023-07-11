//! Flycheck provides the functionality needed to run `cargo check` or
//! another compatible command (f.x. clippy) in a background thread and provide
//! LSP diagnostics based on the output of the command.

#![warn(rust_2018_idioms, unused_lifetimes, semicolon_in_expressions_from_macros)]

use std::{
    fmt, io,
    process::{ChildStderr, ChildStdout, Command, Stdio},
    time::Duration, path::Path,
};

use command_group::{CommandGroup, GroupChild};
use crossbeam_channel::{never, select, unbounded, Receiver, Sender};
use paths::AbsPathBuf;
use rustc_hash::FxHashMap;
use serde::Deserialize;
use stdx::process::streaming_output;

pub use cargo_metadata::diagnostic::{
    Applicability, Diagnostic, DiagnosticCode, DiagnosticLevel, DiagnosticSpan,
    DiagnosticSpanMacroExpansion,
};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum InvocationStrategy {
    Once,
    #[default]
    PerWorkspace,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InvocationLocation {
    Root(AbsPathBuf),
    #[default]
    Workspace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlycheckConfig {
    CargoCommand {
        command: String,
        target_triples: Vec<String>,
        all_targets: bool,
        no_default_features: bool,
        all_features: bool,
        features: Vec<String>,
        extra_args: Vec<String>,
        extra_env: FxHashMap<String, String>,
        ansi_color_output: bool,
    },
    CustomCommand {
        command: String,
        args: Vec<String>,
        extra_env: FxHashMap<String, String>,
        invocation_strategy: InvocationStrategy,
        invocation_location: InvocationLocation,
    },
}

impl fmt::Display for FlycheckConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlycheckConfig::CargoCommand { command, .. } => write!(f, "cargo {command}"),
            FlycheckConfig::CustomCommand { command, args, .. } => {
                write!(f, "{command} {}", args.join(" "))
            }
        }
    }
}

/// Flycheck wraps the shared state and communication machinery used for
/// running `cargo check` (or other compatible command) and providing
/// diagnostics based on the output.
/// The spawned thread is shut down when this struct is dropped.
#[derive(Debug)]
pub struct FlycheckHandle {
    // XXX: drop order is significant
    sender: Sender<StateChange>,
    _thread: stdx::thread::JoinHandle,
    id: usize,
}

impl FlycheckHandle {
    pub fn spawn(
        id: usize,
        sender: Box<dyn Fn(Message) + Send>,
        config: FlycheckConfig,
        workspace_root: AbsPathBuf,
    ) -> FlycheckHandle {
        let actor = FlycheckActor::new(id, sender, config, workspace_root);
        let (sender, receiver) = unbounded::<StateChange>();
        let thread = stdx::thread::Builder::new(stdx::thread::QoSClass::Utility)
            .name("Flycheck".to_owned())
            .spawn(move || actor.run(receiver))
            .expect("failed to spawn thread");
        FlycheckHandle { id, sender, _thread: thread }
    }

    /// Schedule a re-start of the cargo check worker.
    pub fn restart(&self) {
        self.sender.send(StateChange::Restart).unwrap();
    }

    /// Schedule a re-start of the cargo check worker.
    pub fn restart_verus(&self, file: String) {
        tracing::debug!("restart verus for {:?}", file);
        self.sender.send(StateChange::RestartVerus(file)).unwrap();
    }

    /// Stop this cargo check worker.
    pub fn cancel(&self) {
        self.sender.send(StateChange::Cancel).unwrap();
    }

    pub fn id(&self) -> usize {
        self.id
    }
}

pub enum Message {
    /// Request adding a diagnostic with fixes included to a file
    AddDiagnostic { id: usize, workspace_root: AbsPathBuf, diagnostic: Diagnostic },

    /// Request check progress notification to client
    Progress {
        /// Flycheck instance ID
        id: usize,
        progress: Progress,
    },
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::AddDiagnostic { id, workspace_root, diagnostic } => f
                .debug_struct("AddDiagnostic")
                .field("id", id)
                .field("workspace_root", workspace_root)
                .field("diagnostic_code", &diagnostic.code.as_ref().map(|it| &it.code))
                .finish(),
            Message::Progress { id, progress } => {
                f.debug_struct("Progress").field("id", id).field("progress", progress).finish()
            }
        }
    }
}

#[derive(Debug)]
pub enum Progress {
    DidStart,
    DidCheckCrate(String),
    DidFinish(io::Result<()>),
    DidCancel,
    DidFailToRestart(String),
    VerusResult(String),
}

enum StateChange {
    Restart,
    Cancel,
    RestartVerus(String),
}

/// A [`FlycheckActor`] is a single check instance of a workspace.
struct FlycheckActor {
    /// The workspace id of this flycheck instance.
    id: usize,
    sender: Box<dyn Fn(Message) + Send>,
    config: FlycheckConfig,
    /// Either the workspace root of the workspace we are flychecking,
    /// or the project root of the project.
    root: AbsPathBuf,
    /// CargoHandle exists to wrap around the communication needed to be able to
    /// run `cargo check` without blocking. Currently the Rust standard library
    /// doesn't provide a way to read sub-process output without blocking, so we
    /// have to wrap sub-processes output handling in a thread and pass messages
    /// back over a channel.
    cargo_handle: Option<CargoHandle>,
}

enum Event {
    RequestStateChange(StateChange),
    CheckEvent(Option<CargoMessage>),
}

impl FlycheckActor {
    fn new(
        id: usize,
        sender: Box<dyn Fn(Message) + Send>,
        config: FlycheckConfig,
        workspace_root: AbsPathBuf,
    ) -> FlycheckActor {
        tracing::info!(%id, ?workspace_root, "Spawning flycheck");
        FlycheckActor { id, sender, config, root: workspace_root, cargo_handle: None }
    }

    fn report_progress(&self, progress: Progress) {
        self.send(Message::Progress { id: self.id, progress });
    }

    fn next_event(&self, inbox: &Receiver<StateChange>) -> Option<Event> {
        let check_chan = self.cargo_handle.as_ref().map(|cargo| &cargo.receiver);
        if let Ok(msg) = inbox.try_recv() {
            // give restarts a preference so check outputs don't block a restart or stop
            return Some(Event::RequestStateChange(msg));
        }
        select! {
            recv(inbox) -> msg => msg.ok().map(Event::RequestStateChange),
            recv(check_chan.unwrap_or(&never())) -> msg => Some(Event::CheckEvent(msg.ok())),
        }
    }

    fn run(mut self, inbox: Receiver<StateChange>) {
        'event: while let Some(event) = self.next_event(&inbox) {
            match event {
                Event::RequestStateChange(StateChange::Cancel) => {
                    tracing::debug!(flycheck_id = self.id, "flycheck cancelled");
                    self.cancel_check_process();
                }
                Event::RequestStateChange(StateChange::Restart) => {
                    // Cancel the previously spawned process
                    self.cancel_check_process();
                    while let Ok(restart) = inbox.recv_timeout(Duration::from_millis(50)) {
                        // restart chained with a stop, so just cancel
                        if let StateChange::Cancel = restart {
                            continue 'event;
                        }
                    }
                    let command = self.check_command();
                    tracing::debug!(?command, "will restart flycheck");
                    match CargoHandle::spawn(command) {
                        Ok(cargo_handle) => {
                            tracing::debug!(
                                command = ?self.check_command(),
                                "did  restart flycheck"
                            );
                            self.cargo_handle = Some(cargo_handle);
                            self.report_progress(Progress::DidStart);
                        }
                        Err(error) => {
                            self.report_progress(Progress::DidFailToRestart(format!(
                                "Failed to run the following command: {:?} error={}",
                                self.check_command(),
                                error
                            )));
                        }
                    }
                }
                Event::RequestStateChange(StateChange::RestartVerus(filename)) => {
                    // verus: copied from above `Event::RequestStateChange(StateChange::Restart)`
                    // Cancel the previously spawned process
                    self.cancel_check_process();
                    while let Ok(restart) = inbox.recv_timeout(Duration::from_millis(50)) {
                        // restart chained with a stop, so just cancel
                        if let StateChange::Cancel = restart {
                            continue 'event;
                        }
                    }

                    let command = self.run_verus(filename.clone());
                    tracing::error!(?command, "will restart verus");
                    match CargoHandle::spawn(command) {
                        Ok(cargo_handle) => {
                            tracing::error!(
                                // command = ?self.check_command(),
                                "did  restart Verus"
                            );
                            
                            self.cargo_handle = Some(cargo_handle);
                            // self.report_progress(Progress::DidStart);
                            self.report_progress(Progress::VerusResult(format!(
                                "Started running the following Verus command: {:?}",
                                self.run_verus(filename),
                            )));
                            self.report_progress(Progress::DidStart); // this is important -- otherewise, previous diagnostic stays
                        }
                        Err(error) => {
                            tracing::error!(?error, "got this running Verus");
                            self.report_progress(Progress::VerusResult(format!(
                                "Failed to run the following Verus command: {:?} error={}",
                                self.run_verus(filename),
                                error
                            )));
                        }
                    }
                }
                Event::CheckEvent(None) => {
                    tracing::error!(flycheck_id = self.id, "flycheck finished");

                    // Watcher finished
                    let cargo_handle = self.cargo_handle.take().unwrap();
                    let res = cargo_handle.join();
                    if res.is_err() {
                        tracing::error!(
                            "Flycheck failed to run the following command: {:?}",
                            self.check_command()
                        );
                    }
                    self.report_progress(Progress::DidFinish(res));
                }
                Event::CheckEvent(Some(message)) => match message {
                    CargoMessage::CompilerArtifact(msg) => {
                        tracing::trace!(
                            flycheck_id = self.id,
                            artifact = msg.target.name,
                            "artifact received"
                        );
                        self.report_progress(Progress::DidCheckCrate(msg.target.name));
                    }

                    CargoMessage::Diagnostic(msg) => {
                        tracing::trace!(
                            flycheck_id = self.id,
                            message = msg.message,
                            "diagnostic received"
                        );
                        self.send(Message::AddDiagnostic {
                            id: self.id,
                            workspace_root: self.root.clone(),
                            diagnostic: msg,
                        });
                    }
                    CargoMessage::VerusResult(res) => {
                        // self.send(Message::)
                        tracing::error!(?res, "verus result");
                        self.report_progress(Progress::VerusResult(res));
                    },
                },
            }
        }
        // If we rerun the thread, we need to discard the previous check results first
        self.cancel_check_process();
    }

    fn cancel_check_process(&mut self) {
        if let Some(cargo_handle) = self.cargo_handle.take() {
            tracing::debug!(
                command = ?self.check_command(),
                "did  cancel flycheck"
            );
            cargo_handle.cancel();
            self.report_progress(Progress::DidCancel);
        }
    }

    fn check_command(&self) -> Command {
        let (mut cmd, args) = match &self.config {
            FlycheckConfig::CargoCommand {
                command,
                target_triples,
                no_default_features,
                all_targets,
                all_features,
                extra_args,
                features,
                extra_env,
                ansi_color_output,
            } => {
                let mut cmd = Command::new(toolchain::cargo());
                cmd.arg(command);
                cmd.current_dir(&self.root);
                cmd.arg("--workspace");

                cmd.arg(if *ansi_color_output {
                    "--message-format=json-diagnostic-rendered-ansi"
                } else {
                    "--message-format=json"
                });

                cmd.arg("--manifest-path");
                cmd.arg(self.root.join("Cargo.toml").as_os_str());

                for target in target_triples {
                    cmd.args(["--target", target.as_str()]);
                }
                if *all_targets {
                    cmd.arg("--all-targets");
                }
                if *all_features {
                    cmd.arg("--all-features");
                } else {
                    if *no_default_features {
                        cmd.arg("--no-default-features");
                    }
                    if !features.is_empty() {
                        cmd.arg("--features");
                        cmd.arg(features.join(" "));
                    }
                }
                cmd.envs(extra_env);
                (cmd, extra_args)
            }
            FlycheckConfig::CustomCommand {
                command,
                args,
                extra_env,
                invocation_strategy,
                invocation_location,
            } => {
                let mut cmd = Command::new(command);
                cmd.envs(extra_env);

                match invocation_location {
                    InvocationLocation::Workspace => {
                        match invocation_strategy {
                            InvocationStrategy::Once => {
                                cmd.current_dir(&self.root);
                            }
                            InvocationStrategy::PerWorkspace => {
                                // FIXME: cmd.current_dir(&affected_workspace);
                                cmd.current_dir(&self.root);
                            }
                        }
                    }
                    InvocationLocation::Root(root) => {
                        cmd.current_dir(root);
                    }
                }

                (cmd, args)
            }
        };

        cmd.args(args);
        cmd
    }

    // copied from above check_command
    fn run_verus(&self, file: String) -> Command {
        tracing::debug!(flycheck_id = self.id, "run verus");
        let (mut cmd, args) = match &self.config {
            FlycheckConfig::CargoCommand {..} => panic!("verus: please set cargo override command"),
            FlycheckConfig::CustomCommand {
                command,
                args,
                extra_env,
                invocation_strategy,
                invocation_location,
            } => {
                tracing::error!(?command, ?args, ?extra_env, "run_verus");
                let mut cmd = Command::new(command);
                
                let file = Path::new(&file);
                let mut file_as_module = None;
                let mut root: Option<std::path::PathBuf> = None;
                let mut extra_args_from_toml = None;
                for ans in file.ancestors() {
                    tracing::error!(?ans, "ancestors");
                    if ans.join("Cargo.toml").exists() {
                        let toml = std::fs::read_to_string(ans.join("Cargo.toml")).unwrap();
                        let mut found_verus_settings = false;
                        for line in toml.lines() {
                            if found_verus_settings {
                                if line.contains("extra_args") {
                                    let start = "extra_args".len() + 1;
                                    let mut arguments = line[start..line.len()-1].trim().to_string();
                                    if arguments.starts_with("=") {
                                        arguments.remove(0);
                                        arguments = arguments.trim().to_string();
                                    }
                                    if arguments.starts_with("\"") {
                                        arguments.remove(0);
                                    }
                                    if arguments.ends_with("\"") {
                                        arguments.remove(arguments.len()-1);
                                    }

                                    let arguments_vec = arguments.split(" ").map(|it| it.to_string()).collect::<Vec<_>>();
                                    extra_args_from_toml = Some(arguments_vec);
                                }
                                break;
                            }
                            if line.contains("[package.metadata.verus.ide]") {
                                found_verus_settings = true;
                            }
                        }

                        if ans.join("src/main.rs").exists() {
                            root = Some(ans.join("src/main.rs"));
                            file_as_module = Some(file.strip_prefix(ans.join("src")).unwrap().to_str().unwrap().replace("/", "::").replace(".rs", ""));
                        } else if ans.join("src/lib.rs").exists() {
                            root = Some(ans.join("src/lib.rs"));
                            file_as_module = Some(file.strip_prefix(ans.join("src")).unwrap().to_str().unwrap().replace("/", "::").replace(".rs", ""));
                        } else {
                            continue;
                        }
                        break;
                    }
                }


                let mut args = args.to_vec();

                let root = root.unwrap(); // FIXME
                args.insert(0, root.to_str().unwrap().to_string());
                if root == file {
                    tracing::error!("root == file");
                } else {
                    tracing::error!(?root, "root");
                    args.insert(1, "--verify-module".to_string());
                    args.insert(2, file_as_module.unwrap().to_string());
                }

                args.append(&mut extra_args_from_toml.unwrap_or_default());
                args.push("--".to_string());
                args.push("--error-format=json".to_string());
                cmd.envs(extra_env);

                match invocation_location {
                    InvocationLocation::Workspace => {
                        match invocation_strategy {
                            InvocationStrategy::Once => {
                                cmd.current_dir(&self.root);
                            }
                            InvocationStrategy::PerWorkspace => {
                                // FIXME: cmd.current_dir(&affected_workspace);
                                cmd.current_dir(&self.root);
                            }
                        }
                    }
                    InvocationLocation::Root(root) => {
                        cmd.current_dir(root);
                    }
                }

                (cmd, args)
            }
        };

        cmd.args(args);
        dbg!(&cmd);
        cmd
    }

    fn send(&self, check_task: Message) {
        (self.sender)(check_task);
    }
}

struct JodGroupChild(GroupChild);

impl Drop for JodGroupChild {
    fn drop(&mut self) {
        _ = self.0.kill();
        _ = self.0.wait();
    }
}

/// A handle to a cargo process used for fly-checking.
struct CargoHandle {
    /// The handle to the actual cargo process. As we cannot cancel directly from with
    /// a read syscall dropping and therefore terminating the process is our best option.
    child: JodGroupChild,
    thread: stdx::thread::JoinHandle<io::Result<(bool, String)>>,
    receiver: Receiver<CargoMessage>,
}

impl CargoHandle {
    fn spawn(mut command: Command) -> std::io::Result<CargoHandle> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
        let mut child = command.group_spawn().map(JodGroupChild)?;

        let stdout = child.0.inner().stdout.take().unwrap();
        let stderr = child.0.inner().stderr.take().unwrap();

        let (sender, receiver) = unbounded();
        let actor = CargoActor::new(sender, stdout, stderr);
        let thread = stdx::thread::Builder::new(stdx::thread::QoSClass::Utility)
            .name("CargoHandle".to_owned())
            .spawn(move || actor.run())
            .expect("failed to spawn thread");
        Ok(CargoHandle { child, thread, receiver })
    }

    fn cancel(mut self) {
        let _ = self.child.0.kill();
        let _ = self.child.0.wait();
    }

    fn join(mut self) -> io::Result<()> {
        let _ = self.child.0.kill();
        let exit_status = self.child.0.wait()?;
        let (read_at_least_one_message, error) = self.thread.join()?;
        if read_at_least_one_message || exit_status.success() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other, format!(
                "Cargo watcher failed, the command produced no valid metadata (exit code: {exit_status:?}):\n{error}"
            )))
        }
    }
}

struct CargoActor {
    sender: Sender<CargoMessage>,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

impl CargoActor {
    fn new(sender: Sender<CargoMessage>, stdout: ChildStdout, stderr: ChildStderr) -> CargoActor {
        CargoActor { sender, stdout, stderr }
    }

    fn run(self) -> io::Result<(bool, String)> {
        // We manually read a line at a time, instead of using serde's
        // stream deserializers, because the deserializer cannot recover
        // from an error, resulting in it getting stuck, because we try to
        // be resilient against failures.
        //
        // Because cargo only outputs one JSON object per line, we can
        // simply skip a line if it doesn't parse, which just ignores any
        // erroneous output.

        tracing::debug!("cargo actor run");

        let mut stdout_errors = String::new();
        let mut stderr_errors = String::new();
        let mut read_at_least_one_stdout_message = false;
        let mut read_at_least_one_stderr_message = false;
        let process_line = |line: &str, error: &mut String| {
            // Try to deserialize a message from Cargo or Rustc.
            let mut deserializer = serde_json::Deserializer::from_str(line);
            deserializer.disable_recursion_limit();
            if let Ok(message) = JsonMessage::deserialize(&mut deserializer) {
                match message {
                    // Skip certain kinds of messages to only spend time on what's useful
                    JsonMessage::Cargo(message) => match message {
                        cargo_metadata::Message::CompilerArtifact(artifact) if !artifact.fresh => {
                            self.sender.send(CargoMessage::CompilerArtifact(artifact)).unwrap();
                        }
                        cargo_metadata::Message::CompilerMessage(msg) => {
                            self.sender.send(CargoMessage::Diagnostic(msg.message)).unwrap();
                        }
                        // cargo_metadata::Message::TextLine(l) => {
                        //     tracing::error!("cargo text line: {:?}", l);
                        // }
                        _ => (),
                    },
                    JsonMessage::Rustc(message) => {
                        self.sender.send(CargoMessage::Diagnostic(message)).unwrap();
                    }
                }
                return true;
            } else {
                tracing::error!("deserialize error: {:?}", line);
                if line.contains("verification results::") {
                    self.sender.send(CargoMessage::VerusResult(line.to_string())).unwrap();
                }
            }

            error.push_str(line);
            error.push('\n');
            false
        };
        let output = streaming_output(
            self.stdout,
            self.stderr,
            &mut |line| {
                if process_line(line, &mut stdout_errors) {
                    read_at_least_one_stdout_message = true;
                }
            },
            &mut |line| {
                if process_line(line, &mut stderr_errors) {
                    read_at_least_one_stderr_message = true;
                }
            },
        );

        let read_at_least_one_message =
            read_at_least_one_stdout_message || read_at_least_one_stderr_message;
        let mut error: String = stdout_errors;
        tracing::error!("stderr_errors: {:?}", stderr_errors);
        error.push_str(&stderr_errors);
        match output {
            Ok(_) => Ok((read_at_least_one_message, error)),
            Err(e) => Err(io::Error::new(e.kind(), format!("{e:?}: {error}"))),
        }
    }
}

enum CargoMessage {
    CompilerArtifact(cargo_metadata::Artifact),
    Diagnostic(Diagnostic),
    VerusResult(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JsonMessage {
    Cargo(cargo_metadata::Message),
    Rustc(Diagnostic),
}
