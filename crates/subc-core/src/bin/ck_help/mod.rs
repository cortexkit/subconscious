use super::discover_external_domains;

pub(super) const BUILTIN_DOMAINS: &[&str] = &[
    "activity",
    "catalog",
    "config",
    "daemon",
    "doctor",
    "health",
    "help",
    "machine",
    "module",
    "policy-root",
    "provenance",
    "quota",
    "routes",
    "setup",
    "stack",
    "upgrade",
    "window",
];

const TOP_HELP_BASE: &str = "ck — CortexKit operator CLI\n\nusage:\n  ck [--subc <connection-file>] [--json] <domain> [<verb>] [<args>]\n\ndomains:\n  setup     plan and apply the managed CortexKit installation\n  upgrade   plan managed component upgrades\n  config    status, plan, apply, stamp, unstamp, history, rollback, place-program\n  module    supervised modules: list, status, resources, profile, stderr, terminals, restart, stop, start, rescan, release\n  catalog   what is registered on the wire (not the supervised roster)
  routes    live consumers for one module or the whole daemon\n  provenance daemon-attested and module-declared build/process facts\n  health    one-line health for every supervised module\n  quota     AI-provider quota and usage windows\n  daemon    daemon version, uptime, connection info, offline triage, and CI lint
  doctor    diagnose the stack, even when the daemon is down
  machine   this machine's id: show it, or adopt a restored one
  activity  consequential agent and module events
  stack     stop, start, and check the stack
  window    record, keep, revert, or abandon placement changes";

const TOP_HELP_TAIL: &str = "flags:\n  --subc <file>   use a specific connection file (default: auto-discover)\n  --json          raw JSON output instead of tables\n  --verbose       include diagnostic detail and complete metrics\n\nrun 'ck <domain>' with no verb to see that domain's commands";

pub(super) fn is_builtin_domain(domain: &str) -> bool {
    BUILTIN_DOMAINS.contains(&domain)
}

/// Top-level help has one domain list. External commands have to opt in so a
/// module binary on PATH cannot accidentally become an operator command.
pub(super) fn top_help() -> String {
    top_help_with_tail(TOP_HELP_TAIL)
}

fn top_help_with_tail(tail: &str) -> String {
    let external = discover_external_domains();
    let mut out = String::from(TOP_HELP_BASE);
    for domain in &external {
        out.push_str(&format!("\n  {:<9} {}", domain.name, domain.headline));
    }
    out.push_str("\n\n");
    out.push_str(tail);
    out
}
