use native_engine::{
    CancelToken, Engine, EngineConfig, ExitState, Progress, ProgressSink, Provider, SyncSummary,
};
use std::time::Duration;

struct Printer;

impl ProgressSink for Printer {
    fn event(&self, p: Progress) {
        println!("{} {}", p.action, p.path);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut opts: Vec<(String, String)> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                opts.push((k.to_string(), v.to_string()));
            } else if matches!(
                rest,
                "dry-run" | "no-hidden" | "insecure" | "help" | "version"
            ) {
                flags.push(rest.to_string());
            } else {
                let v = args.next().unwrap_or_default();
                opts.push((rest.to_string(), v));
            }
        }
    }
    let get = |k: &str| {
        opts.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    if flags.iter().any(|f| f == "help") {
        println!(
            "native-engine-cli --provider nextcloud|opencloud --url URL --user U --password P \\\n  --local DIR [--remote PATH] [--space-id ID] [--dry-run] [--exclude PATTERN]... \\\n  [--no-hidden] [--insecure] [--proxy URL] [--max-retries N]"
        );
        return;
    }
    let provider = match get("provider").as_deref() {
        Some("opencloud") => Provider::OpenCloud,
        _ => Provider::Nextcloud,
    };
    let Some(url) = get("url") else {
        eprintln!("missing --url");
        std::process::exit(64);
    };
    let Some(user) = get("user") else {
        eprintln!("missing --user");
        std::process::exit(64);
    };
    let Some(password) = get("password") else {
        eprintln!("missing --password");
        std::process::exit(64);
    };
    let Some(local) = get("local") else {
        eprintln!("missing --local");
        std::process::exit(64);
    };
    let mut cfg = EngineConfig::new(
        provider,
        url,
        user,
        password,
        local,
        get("remote").unwrap_or_default(),
    );
    cfg.space_id = get("space-id");
    cfg.dry_run = flags.iter().any(|f| f == "dry-run");
    cfg.sync_hidden_files = !flags.iter().any(|f| f == "no-hidden");
    cfg.trust_invalid_certificates = flags.iter().any(|f| f == "insecure");
    cfg.proxy = get("proxy");
    if let Some(n) = get("max-retries").and_then(|v| v.parse().ok()) {
        cfg.max_retries = n;
    }
    cfg.exclude_patterns = opts
        .iter()
        .filter(|(k, _)| k == "exclude")
        .map(|(_, v)| v.clone())
        .collect();
    cfg.timeout = Duration::from_secs(120);

    match Engine::new(cfg).run(&Printer, &CancelToken::new()) {
        Ok(summary) => {
            print_summary(&summary);
            std::process::exit(match summary.exit {
                ExitState::Success => 0,
                ExitState::Conflict => 2,
                _ => 1,
            });
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn print_summary(s: &SyncSummary) {
    println!(
        "actions={} downloaded={} uploaded={} deleted_local={} deleted_remote={} conflicts={} errors={}",
        s.actions, s.downloaded, s.uploaded, s.deleted_local, s.deleted_remote, s.conflicts, s.errors
    );
}
