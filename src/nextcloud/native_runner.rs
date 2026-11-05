//! [`SyncRunner`] backed by the native engine crate.
//!
//! Replaces the `nextcloudcmd` / `opencloudcmd` child process with an
//! in-process WebDAV client. Enabled per account through
//! `SyncConfig::native_engine`.

use crate::core::scheduler::{SyncOutcome, SyncRunner};
use crate::nextcloud::nextcloudcmd_progress::SyncProgress;
use crate::nextcloud::sync_engine::{CredentialLookup, CredentialSource, KeyringCredentialSource};
use crate::storage::config::{AccountConfig, FolderConfig, NetworkConfig};
use native_engine::{
    CancelToken, Engine, EngineConfig, ExitState, Progress, ProgressSink, Provider,
};
use std::sync::Arc;

struct ChannelSink(async_channel::Sender<SyncProgress>);

impl ProgressSink for ChannelSink {
    fn event(&self, p: Progress) {
        let mut progress = SyncProgress::new(p.action, p.path);
        progress.processed = p.processed;
        progress.bytes_done = p.bytes_done;
        progress.bytes_total = p.bytes_total;
        if progress.bytes_total > 0 {
            eprintln!(
                "sync: {} {} ({}/{})",
                progress.action,
                progress.path,
                progress.bytes_done,
                progress.bytes_total
            );
        } else {
            eprintln!("sync: {} {}", progress.action, progress.path);
        }
        let _ = self.0.send_blocking(progress);
    }
}

pub struct NativeEngineRunner {
    account: AccountConfig,
    folder: FolderConfig,
    network: NetworkConfig,
    progress: async_channel::Sender<SyncProgress>,
    credentials: Option<Arc<dyn CredentialSource>>,
    cancel: CancelToken,
}

impl NativeEngineRunner {
    pub fn new(
        account: AccountConfig,
        folder: FolderConfig,
        network: NetworkConfig,
        progress: async_channel::Sender<SyncProgress>,
    ) -> Self {
        Self {
            account,
            folder,
            network,
            progress,
            credentials: Some(Arc::new(KeyringCredentialSource)),
            cancel: CancelToken::new(),
        }
    }

    pub fn with_credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = Some(source);
        self
    }

    fn config(&self, password: &str) -> EngineConfig {
        let mut cfg = EngineConfig::new(
            match self.account.provider {
                crate::nextcloud::driver::Provider::OpenCloud => Provider::OpenCloud,
                crate::nextcloud::driver::Provider::Nextcloud => Provider::Nextcloud,
            },
            self.account.server_url.clone(),
            self.account.login_name.clone(),
            password,
            self.folder.local_root.clone(),
            self.folder.remote_path.clone(),
        );
        cfg.space_id = self.folder.space_id.clone();
        cfg.trust_invalid_certificates =
            self.account.trust_invalid_certificates || self.network.trust_invalid_certificates;
        cfg.proxy = self
            .account
            .custom_proxy
            .clone()
            .or_else(|| self.network.custom_proxy.clone());
        cfg.max_retries = self.account.sync.max_sync_retries.clamp(0, 10) as u32;
        cfg.sync_hidden_files = self.account.sync.sync_hidden_files;
        cfg.exclude_patterns = if self.account.sync.exclude_patterns_enabled {
            self.account.sync.exclude_patterns.clone()
        } else {
            Vec::new()
        };
        cfg.delete_guard.enabled = self.account.delete_guard.enabled;
        cfg
    }
}

impl SyncRunner for NativeEngineRunner {
    fn start(
        &mut self,
        _reasons: &[crate::core::triggers::Trigger],
        on_finished: Box<dyn FnOnce(SyncOutcome) + 'static>,
    ) {
        let Some(source) = self.credentials.clone() else {
            glib::spawn_future_local(async move { on_finished(SyncOutcome::Failed) });
            return;
        };
        let password = match source.lookup(&self.account) {
            CredentialLookup::Found(p) => p,
            CredentialLookup::Missing => {
                glib::spawn_future_local(async move { on_finished(SyncOutcome::NoCredentials) });
                return;
            }
            CredentialLookup::Locked => {
                glib::spawn_future_local(async move { on_finished(SyncOutcome::KeyringLocked) });
                return;
            }
            CredentialLookup::Unavailable => {
                glib::spawn_future_local(async move { on_finished(SyncOutcome::Failed) });
                return;
            }
        };
        let engine = Engine::new(self.config(&password));
        let sink = ChannelSink(self.progress.clone());
        let cancel = self.cancel.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || engine.run(&sink, &cancel)).await;
            if let Ok(Ok(summary)) = &result {
                eprintln!(
                    "sync: done actions={} downloaded={} uploaded={} deleted_local={} deleted_remote={} conflicts={} errors={}",
                    summary.actions,
                    summary.downloaded,
                    summary.uploaded,
                    summary.deleted_local,
                    summary.deleted_remote,
                    summary.conflicts,
                    summary.errors
                );
            }
            let outcome = match result {
                Ok(Ok(summary)) => match summary.exit {
                    ExitState::Success => SyncOutcome::Success,
                    ExitState::Conflict => SyncOutcome::Conflict,
                    ExitState::AuthFailed => SyncOutcome::AuthFailed,
                    ExitState::Failed => SyncOutcome::Failed,
                },
                Ok(Err(native_engine::Error::Auth(_))) => SyncOutcome::AuthFailed,
                Ok(Err(native_engine::Error::Cancelled)) => SyncOutcome::Failed,
                Ok(Err(e)) => {
                    eprintln!("native engine failed: {e}");
                    SyncOutcome::Failed
                }
                Err(e) => {
                    eprintln!("native engine task failed: {e:?}");
                    SyncOutcome::Failed
                }
            };
            on_finished(outcome);
        });
    }

    fn cancel(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::config::{AccountConfig, FolderConfig, NetworkConfig};
    use native_engine::Engine;

    fn account(provider: crate::nextcloud::driver::Provider, url: &str) -> AccountConfig {
        let sync = crate::storage::config::SyncConfig {
            max_sync_retries: 7,
            exclude_patterns_enabled: true,
            exclude_patterns: vec!["*.tmp".into()],
            sync_hidden_files: false,
            ..crate::storage::config::SyncConfig::default()
        };
        AccountConfig {
            id: "acc-test".into(),
            server_url: url.into(),
            login_name: "admin".into(),
            provider,
            sync,
            custom_proxy: Some("http://proxy.invalid:3128".into()),
            trust_invalid_certificates: true,
            ..AccountConfig::default()
        }
    }

    fn folder(local_root: &str) -> FolderConfig {
        FolderConfig {
            id: "folder-test".into(),
            local_root: local_root.into(),
            remote_path: "/Docs".into(),
            space_id: Some("space-1".into()),
            size_confirmed: true,
        }
    }

    #[test]
    fn config_maps_account_and_network_settings() {
        let (tx, _rx) = async_channel::bounded(4);
        let runner = NativeEngineRunner::new(
            account(
                crate::nextcloud::driver::Provider::OpenCloud,
                "https://cloud.example.com/",
            ),
            folder("/tmp/ignored"),
            NetworkConfig::default(),
            tx,
        );
        let cfg = runner.config("secret");
        assert_eq!(cfg.provider, native_engine::Provider::OpenCloud);
        assert_eq!(cfg.base_url, "https://cloud.example.com");
        assert_eq!(cfg.username, "admin");
        assert_eq!(cfg.remote_path, "/Docs");
        assert_eq!(cfg.space_id.as_deref(), Some("space-1"));
        assert_eq!(cfg.max_retries, 7);
        assert_eq!(cfg.exclude_patterns, vec!["*.tmp".to_string()]);
        assert!(!cfg.sync_hidden_files);
        assert_eq!(cfg.proxy.as_deref(), Some("http://proxy.invalid:3128"));
        assert!(cfg.trust_invalid_certificates);
        assert!(!cfg.dry_run);
    }

    #[test]
    fn channel_sink_forwards_progress_to_the_app_channel() {
        let (tx, rx) = async_channel::bounded(4);
        let sink = ChannelSink(tx);
        sink.event(native_engine::Progress::new("upload", "Docs/a.txt"));
        let received = rx.try_recv().expect("progress event forwarded");
        assert_eq!(received.action, "upload");
        assert_eq!(received.path, "Docs/a.txt");
        assert!(received.is_operation());
    }

    /// Runs the native engine through the same config path and progress sink
    /// the app uses. Needs a live server, so it is ignored by default:
    /// `NXS_E2E_NC_URL=http://127.0.0.1:8081 cargo test -- --ignored native_engine_runs`
    #[test]
    #[ignore]
    fn native_engine_runs_through_app_wiring() {
        let url =
            std::env::var("NXS_E2E_NC_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
        let password = std::env::var("NXS_E2E_PASS").unwrap_or_else(|_| "admin1234".into());
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("runner.txt"), b"runner wiring").expect("write file");
        let local = dir.path().to_string_lossy().to_string();

        let (tx, rx) = async_channel::bounded(64);
        let acc = AccountConfig {
            custom_proxy: None, // the config test uses a dead proxy on purpose
            trust_invalid_certificates: false,
            ..account(crate::nextcloud::driver::Provider::Nextcloud, &url)
        };
        let runner = NativeEngineRunner::new(
            acc,
            FolderConfig {
                id: "runner".into(),
                local_root: local,
                remote_path: format!("/nxs-runner-e2e-{}", std::process::id()),
                space_id: None,
                size_confirmed: true,
            },
            NetworkConfig::default(),
            tx.clone(),
        );
        let cfg = runner.config(&password);
        let summary = Engine::new(cfg)
            .run(&ChannelSink(tx), &native_engine::CancelToken::new())
            .expect("engine run");
        assert_eq!(summary.exit, native_engine::ExitState::Success);
        assert_eq!(summary.errors, 0);
        assert!(summary.uploaded >= 1, "expected the file to be uploaded");
        let event = rx.try_recv().expect("progress event");
        assert!(!event.action.is_empty());
    }
}
