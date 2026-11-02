//! cgpt-gateway: a local OpenAI-compatible server that lets OpenCode talk to
//! ChatGPT through the cgpt-bridge native host.
//!
//! `cgpt-gateway`                serve
//! `cgpt-gateway --print-config` print `{"port":…,"token":"…"}` for installers

mod backend;
#[cfg(test)]
mod bridge_e2e;
mod config;
mod cooldown;
mod engine;
mod http;
mod log;
mod openai;
mod prompt;
mod reply;
mod safety;
mod session;
mod title;

use std::process::ExitCode;

use log::log;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let print_config = match args.as_slice() {
        [] => false,
        [flag] if flag == "--print-config" => true,
        _ => {
            eprintln!("usage: cgpt-gateway [--print-config]");
            return ExitCode::from(2);
        }
    };
    match run(print_config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(format_args!("cgpt-gateway: {}", e));
            ExitCode::FAILURE
        }
    }
}

fn run(print_config: bool) -> std::io::Result<()> {
    let dir = config::gateway_dir()?;
    let (cfg, config_warning) = config::load_or_create(&dir)?;
    if print_config {
        println!(
            "{}",
            serde_json::json!({ "port": cfg.port, "token": cfg.token })
        );
        return Ok(());
    }
    log(start_line());
    log(format_args!(
        "cgpt-gateway: config {}",
        dir.join("config.json").display()
    ));
    if let Some(warning) = config_warning {
        log(warning);
    }
    let sessions = session::Sessions::new(dir.join("sessions"));
    let cooldown = cooldown::Cooldown::new(dir.join("cooldown.json"));
    let engine = engine::Engine::new(sessions, cfg.allow_tools.clone(), cooldown);
    http::serve(
        cfg,
        Box::new(backend::BridgeBackend::with_default_endpoint()),
        engine,
    )
}

/// The gateway's first log line: its version and build, to compare with the
/// host's and the extension's (see `engine::builds_line`).
fn start_line() -> String {
    format!(
        "cgpt-gateway v{} (build {}) starting",
        env!("CARGO_PKG_VERSION"),
        cgpt_bridge_protocol::BUILD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_line_names_the_version_and_the_build() {
        let line = start_line();
        assert!(line.starts_with("cgpt-gateway v"), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        // The id the host's `pong` and the gateway's builds line carry.
        assert!(
            line.contains(&format!("(build {})", cgpt_bridge_protocol::BUILD)),
            "{line}"
        );
    }
}
