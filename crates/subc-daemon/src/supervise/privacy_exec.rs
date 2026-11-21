use super::*;

#[cfg(target_os = "macos")]
pub(super) struct PrivacyExec {
    pub(super) content_check:
        Option<tokio::task::JoinHandle<Result<(), crate::config_gate::GateError>>>,
    gated: bool,
    pub(super) admission_binding: Option<ProgramBindingPublication>,
    pub(super) reader: tokio::io::unix::AsyncFd<std::io::PipeReader>,
    pub(super) deadline: tokio::time::Instant,
    pub(super) expected: Option<subc_os::FileIdentity>,
    trampoline: Option<subc_os::FileIdentity>,
    pub(super) signature: Option<Arc<subc_os::privacy_identity::SuspendedSignature>>,
    module_id: String,
    #[cfg(test)]
    confirmation_pause: Option<(String, Option<tokio::net::TcpStream>)>,
    #[cfg(test)]
    pub(super) image_probe: Option<fn(u32) -> Option<subc_os::Observation>>,
    #[cfg(test)]
    image_read_delay: Option<Duration>,
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn test_privacy_trampoline() -> PathBuf {
    // The fixture bin lives in the profile folder the unit-test executable was
    // built into. This honors custom CARGO_TARGET_DIR too.
    let path = cortexkit_test_support::cargo_profile_bin("privacy-trampoline-fixture");
    // Without the fixture every macOS spawn is refused, and the tests that
    // spawn fail later as a module in state Failed, which names the wrong
    // cause. `cargo test -p subc-daemon --lib` alone does not build it.
    assert!(
        path.exists(),
        "privacy-trampoline-fixture not built at {}: run `cargo build -p subc-daemon \
         --bins --features test-support` or `cargo test -p subc-daemon` first",
        path.display()
    );
    path
}

#[cfg(target_os = "macos")]
pub(super) fn probe_privacy_trampoline(path: &std::path::Path) -> Result<(), String> {
    use std::io::Read;
    let mut probe = subc_os::child_spawn::spawn(
        std::process::Command::new(path)
            .args(["__disclaim-exec", "--probe-suspended"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .map_err(|error| {
        format!(
            "privacy trampoline probe failed for {}: {error}",
            path.display()
        )
    })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let status = loop {
        match probe.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            result => {
                let _ = probe.kill();
                let _ = probe.wait();
                return Err(format!(
                    "privacy trampoline probe failed or timed out for {}: {result:?}",
                    path.display()
                ));
            }
        }
    };
    let mut answer = String::new();
    if let Some(stdout) = probe.stdout.take() {
        let _ = stdout.take(256).read_to_string(&mut answer);
    }
    if status.success() && answer.trim() == subc_os::privacy_identity::SUSPENDED_TRAMPOLINE_PROBE {
        return Ok(());
    }
    let mut diagnostic = String::new();
    if let Some(stderr) = probe.stderr.take() {
        let _ = stderr.take(1024).read_to_string(&mut diagnostic);
    }
    let cause = diagnostic
        .trim()
        .strip_prefix("ck-subc: own privacy identity refused: ")
        .unwrap_or("binary does not implement the privacy trampoline protocol");
    Err(format!(
        "{cause}: probe of {} exited {status}",
        path.display()
    ))
}

#[cfg(target_os = "macos")]
pub(super) fn privacy_command(
    spec: &ModuleSpec,
    binding: &SpawnProgramBinding,
    roster: &ChildRoster,
    gated: bool,
) -> Result<
    (
        Command,
        Option<PrivacyExec>,
        subc_os::privacy_identity::ExecAcknowledgement,
    ),
    SuperviseError,
> {
    let failure = |cause: String| {
        warn!(module_id = %spec.module_id, %cause, "privacy identity trampoline refused module spawn");
        SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other(cause),
            cgroup_path: None,
        }
    };
    let trampoline = roster.privacy_trampoline().map_err(failure)?;
    let expected = Some(binding.identity);
    let trampoline_image = subc_os::file_identity(&trampoline);
    let signature = gated
        .then(|| {
            subc_os::privacy_identity::SuspendedSignature::capture(&binding._file, &binding.path)
                .map(Arc::new)
        })
        .transpose()
        .map_err(|error| failure(error.to_string()))?;
    if expected == trampoline_image {
        return Err(failure(
            "privacy identity module executable is missing or is the trampoline itself".to_string(),
        ));
    }
    let (reader, ack) = subc_os::privacy_identity::ExecAcknowledgement::pipe()
        .map_err(|error| failure(error.to_string()))?;
    let reader =
        tokio::io::unix::AsyncFd::new(reader).map_err(|error| failure(error.to_string()))?;
    let mut command = Command::new(&trampoline);
    command
        .arg(if gated {
            "__disclaim-exec-suspended"
        } else {
            "__disclaim-exec"
        })
        .arg(ack.fd().to_string())
        .arg(&binding.path);
    Ok((
        command,
        Some(PrivacyExec {
            gated,
            admission_binding: None,
            content_check: None,
            reader,
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            expected,
            trampoline: trampoline_image,
            signature,
            module_id: spec.module_id.clone(),
            #[cfg(test)]
            image_probe: None,
            #[cfg(test)]
            image_read_delay: spec
                .env
                .iter()
                .find(|(key, _)| key == "SUBC_TEST_PRIVACY_IMAGE_DELAY_MS")
                .map(|(_, millis)| Duration::from_millis(millis.parse().unwrap())),
            #[cfg(test)]
            confirmation_pause: spec
                .env
                .iter()
                .find(|(key, _)| key == "SUBC_TEST_BEFORE_EXEC_CONFIRM")
                .map(|(_, address)| (address.clone(), None)),
        }),
        ack,
    ))
}

impl SupervisedChild {
    #[cfg(target_os = "macos")]
    pub(super) async fn confirm_privacy_exec(&mut self) {
        if self
            .privacy_exec
            .as_ref()
            .is_some_and(|pending| pending.content_check.is_some())
        {
            self.finish_privacy_content_check().await;
            return;
        }
        let Some(pending) = &mut self.privacy_exec else {
            return;
        };
        let result = tokio::time::timeout_at(pending.deadline, async {
            let mut record = Vec::new();
            loop {
                let mut ready = pending.reader.readable().await?;
                let read = ready.try_io(|reader| {
                    use std::io::Read;
                    let mut reader = reader.get_ref();
                    let mut buffer = [0u8; 256];
                    reader.read(&mut buffer).map(|count| (count, buffer))
                });
                match read {
                    Ok(Ok((0, _))) => return Ok::<_, io::Error>(record),
                    Ok(Ok((count, buffer))) => {
                        if record.len() + count > 1024 {
                            return Err(io::Error::other(
                                "privacy exec refusal record is too long",
                            ));
                        }
                        record.extend_from_slice(&buffer[..count]);
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => continue,
                }
            }
        })
        .await;
        #[cfg(test)]
        let result = if let Some((address, stream)) = &mut pending.confirmation_pause {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if stream.is_none() {
                *stream = Some(
                    tokio::net::TcpStream::connect(address.as_str())
                        .await
                        .unwrap(),
                );
                stream.as_mut().unwrap().write_all(b"R").await.unwrap();
                stream
                    .as_mut()
                    .unwrap()
                    .write_all(&self.pid.to_ne_bytes())
                    .await
                    .unwrap();
            }
            let mut reply = [0];
            stream
                .as_mut()
                .unwrap()
                .read_exact(&mut reply)
                .await
                .unwrap();
            pending.confirmation_pause = None;
            if &reply == b"F" {
                Ok(Err(io::Error::other("injected late exec refusal")))
            } else {
                assert_eq!(&reply, b"X");
                result
            }
        } else {
            result
        };
        // Keep the reader in self across await: select cancellation must not
        // discard the handshake or reset its original five-second deadline.
        // Use try_wait so a child reaped during confirmation has its program
        // binding retired before another command can use its nonce.
        let waited = if matches!(&result, Ok(Ok(record)) if record.is_empty()) {
            Some(self.try_wait())
        } else {
            None
        };
        let pending = self.privacy_exec.as_ref().expect("pending exec");
        let cause = match result {
            Err(_) => Some("privacy identity trampoline did not exec within 5s".to_string()),
            Ok(Err(error)) => Some(format!(
                "privacy identity exec acknowledgement failed: {error}"
            )),
            Ok(Ok(record)) if !record.is_empty() => Some(
                std::str::from_utf8(&record)
                    .ok()
                    .and_then(|record| {
                        record
                            .trim()
                            .strip_prefix(subc_os::privacy_identity::EXEC_REFUSAL_TAG)
                    })
                    .filter(|cause| !cause.is_empty())
                    .unwrap_or("invalid privacy exec refusal record")
                    .to_string(),
            ),
            Ok(Ok(_)) if !pending.gated => match waited.expect("exec EOF checked child") {
                Ok(Some(_)) => None,
                Err(error) => Some(format!("privacy identity trampoline wait failed: {error}")),
                Ok(None) => {
                    #[cfg(test)]
                    if let Some(delay) = pending.image_read_delay {
                        tokio::time::sleep(delay).await;
                    }
                    let image = observe_spawned_image(self.pid);
                    #[cfg(test)]
                    let image = pending.image_probe.map_or(image, |probe| probe(self.pid));
                    // An unstamped module can re-exec before we wake to read its
                    // image. Only the trampoline image proves exec never happened;
                    // retain the observed image for later orphan identification.
                    if let Some(image) = image.filter(|image| {
                        image.executable.is_some() && image.executable != pending.trampoline
                    }) {
                        match self
                            .roster_guard
                            .as_ref()
                            .map(|guard| guard.confirm_executable(image))
                            .transpose()
                        {
                            Err(error) => Some(format!(
                                "could not durably record confirmed module image: {error}"
                            )),
                            Ok(_) => {
                                let _ = self.report_ready.set(());
                                None
                            }
                        }
                    } else if image.is_none()
                        || image.is_some_and(|image| image.executable.is_none())
                    {
                        // Empty EOF acknowledged exec. An image disappearing
                        // during observation is an ordinary module exit.
                        match tokio::time::timeout_at(pending.deadline, self.child.wait()).await {
                            Ok(result) => {
                                self.retire_registration();
                                self.registration_channel.take();
                                if let Some(binding) = &self._program_binding {
                                    binding.retire();
                                }
                                result.err().map(|error| format!("privacy identity trampoline wait failed: {error}"))
                            }
                            Err(_) => Some("privacy identity module executable remained unreadable until the 5s exec deadline".to_string()),
                        }
                    } else {
                        Some("privacy identity trampoline did not exec the module".to_string())
                    }
                }
            },
            Ok(Ok(_)) => match waited.expect("exec EOF checked child") {
                Ok(Some(_)) => {
                    Some("privacy identity target exited before suspended admission".to_string())
                }
                Err(error) => Some(format!("privacy identity trampoline wait failed: {error}")),
                Ok(None) => {
                    // CLOEXEC closes the ack just before the kernel reports the
                    // suspended target. EOF alone must never admit a running image.
                    let stopped = tokio::time::timeout_at(pending.deadline, async {
                        loop {
                            if subc_os::privacy_identity::SuspendedSignature::is_suspended(
                                self.pid,
                            )? {
                                return Ok::<_, io::Error>(());
                            }
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                    })
                    .await;
                    match stopped {
                        Err(_) => {
                            Some("privacy identity target was not suspended within 5s".to_string())
                        }
                        Ok(Err(error)) => Some(format!(
                            "privacy identity suspended target unavailable: {error}"
                        )),
                        Ok(Ok(())) => {
                            let image = observe_spawned_image(self.pid);
                            if image.is_some_and(|image| {
                                image.executable.is_some()
                                    && image.executable != pending.trampoline
                                    && image.executable == pending.expected
                            }) {
                                None
                            } else {
                                Some("privacy identity trampoline executable mismatch: expected the suspended module program".to_string())
                            }
                        }
                    }
                }
            },
        };
        #[cfg(test)]
        if let Some(binding) = &self._program_binding {
            binding.pause_after_wait_for_test().await;
        }
        if cause.is_none() && pending.gated {
            let publication = pending.admission_binding.clone();
            let live = self.live_program_binding.clone();
            let signature = Arc::clone(pending.signature.as_ref().expect("gated signature"));
            let pid = self.pid;
            let gated = pending.gated;
            self.privacy_exec
                .as_mut()
                .expect("pending exec")
                .content_check = Some(tokio::task::spawn_blocking(move || {
                signature.verify(pid).map_err(|error| {
                    crate::config_gate::GateError::new("program_refused", error)
                })?;
                if !gated {
                    return Ok(());
                }
                match (publication, live) {
                    (Some(publication), _) => publication.verify(),
                    (None, Some(live)) => live.check(),
                    _ => Err(crate::config_gate::GateError::new(
                        "config_delivery_deferred",
                        "live program witness is unavailable",
                    )),
                }
            }));
            self.finish_privacy_content_check().await;
        } else {
            self.finish_privacy_exec(cause);
        }
    }

    #[cfg(target_os = "macos")]
    async fn finish_privacy_content_check(&mut self) {
        let result = self
            .privacy_exec
            .as_mut()
            .expect("pending exec")
            .content_check
            .as_mut()
            .expect("pending content check")
            .await;
        let cause = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("{}: {}", error.code, error.detail)),
            Err(error) => Some(format!(
                "config_delivery_deferred: content check stopped: {error}"
            )),
        };
        self.finish_privacy_exec(cause);
    }

    #[cfg(target_os = "macos")]
    fn finish_privacy_exec(&mut self, mut cause: Option<String>) {
        let mut pending = self.privacy_exec.take().expect("pending exec");
        if cause.is_none() && pending.gated {
            let admission = (|| -> io::Result<()> {
                let image = observe_spawned_image(self.pid)
                    .filter(|image| image.executable == pending.expected)
                    .ok_or_else(|| io::Error::other("suspended executable mismatch"))?;
                if let Some(guard) = &self.roster_guard {
                    guard.confirm_executable(image)?;
                }
                pending
                    .signature
                    .as_ref()
                    .expect("gated signature")
                    .resume(self.pid)?;
                let _ = self.report_ready.set(());
                info!(module_id = %pending.module_id, pid = self.pid,
                    "verified suspended module resumed with own privacy identity");
                Ok(())
            })();
            if let Err(error) = admission {
                cause = Some(format!("program_refused: {error}"));
            }
        }
        if cause.is_some() {
            if let Some(spawn) = &self.registration_spawn {
                spawn.refuse();
            }
            if let Some(binding) = &mut pending.admission_binding {
                binding.reject();
            }
        } else if let Some(spawn) = &self.registration_spawn {
            spawn.ready();
        }
        // Exec confirmation describes launch success, not roster retirement.
        // Keep even a reaped child recorded until its terminal history exists.
        self.exec.0.finish(if cause.is_some() {
            crate::activity::ExecState::Refused
        } else {
            crate::activity::ExecState::CommittedExec
        });
        if let Some(cause) = cause {
            warn!(module_id = %pending.module_id, pid = self.pid, %cause, "privacy identity trampoline refused module spawn");
            self.spawn_failure = Some(
                if pending.gated
                    && (cause.contains("executable mismatch")
                        || cause.starts_with("program_refused:"))
                {
                    format!("program_refused: {cause}")
                } else if pending.gated {
                    format!("config_delivery_deferred: {cause}")
                } else {
                    cause
                },
            );
            // No resume signal is sent on failure; kill the entire fresh group
            // even if a faulty trampoline did not honor the suspended protocol.
            let _ = self.start_admission_kill();
        }
    }
}

impl Supervisor {
    /// Verify the trampoline once when configured. Missing private OS support
    /// refuses every macOS launch by name but does not stop the daemon's control
    /// server. Embedders must explicitly provide a binary with the subc-os hidden
    /// entry point; the library must not exec an arbitrary hosting program.
    pub fn with_privacy_trampoline(self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        #[cfg(target_os = "macos")]
        {
            let result = probe_privacy_trampoline(&path).map(|()| path);
            if let Err(cause) = &result {
                error!(%cause, "macOS privacy identity unavailable; supervised launches will refuse");
            }
            self.child_roster.set_privacy_trampoline(result);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = path;
        self
    }
}
