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

fn parse_args<I: Iterator<Item = String>>(args: I) -> (Vec<(String, String)>, Vec<String>) {
    let mut args = args;
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
    (opts, flags)
}

fn opt_db_path(opts: &[(String, String)]) -> Option<std::path::PathBuf> {
    opts.iter()
        .find(|(k, _)| k == "db-path")
        .map(|(_, v)| std::path::PathBuf::from(v))
}

fn main() {
    let (opts, flags) = parse_args(std::env::args().skip(1));
    let get = |k: &str| {
        opts.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    if flags.iter().any(|f| f == "help") {
        println!(
            "native-engine-cli --provider nextcloud|opencloud --url URL --user U --password P \\\n  --local DIR [--remote PATH] [--space-id ID] [--dry-run] [--exclude PATTERN]... \\\n  [--no-hidden] [--insecure] [--proxy URL] [--max-retries N] [--db-path FILE]"
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
    cfg.db_path = opt_db_path(&opts);

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

#[cfg(test)]
mod tests {
    use super::{opt_db_path, parse_args};

    fn args(items: &[&str]) -> std::vec::IntoIter<String> {
        items
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn a_separate_journal_path_reaches_the_config() {
        let (opts, flags) = parse_args(args(&["--dry-run", "--db-path", "/tmp/scratch.db"]));
        assert_eq!(
            opt_db_path(&opts),
            Some(std::path::PathBuf::from("/tmp/scratch.db"))
        );
        assert!(flags.iter().any(|f| f == "dry-run"));
    }

    #[test]
    fn a_separate_journal_path_also_accepts_the_equals_form() {
        let (opts, _) = parse_args(args(&["--db-path=/tmp/other.db"]));
        assert_eq!(
            opt_db_path(&opts),
            Some(std::path::PathBuf::from("/tmp/other.db"))
        );
    }

    #[test]
    fn without_the_flag_the_engine_keeps_using_the_folder_journal() {
        let (opts, _) = parse_args(args(&["--local", "/home/u/Fotos"]));
        assert_eq!(opt_db_path(&opts), None);
    }
}
