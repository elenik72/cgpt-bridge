//! A reply without the protocol block gets one repair prompt, and that
//! prompt carries the contract again: seen live, a `--continue` turn (sent
//! without the contract) and a bare repair both came back as prose.

use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

mod common;

use cgpt_bridge_protocol::agent::{AGENT_FENCE_INFO, AGENT_PROMPT_CONTRACT, REPAIR_PROMPT};
use cgpt_bridge_protocol::{accept, listen, read_frame, release, write_frame, BridgeResponse};

const CHAT: &str = "https://chatgpt.com/c/aaaaaaaa-0000";

fn scratch_dir(label: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-ar-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn final_reply(user_message: &str) -> String {
    let block = serde_json::json!({
        "version": 1,
        "status": "final",
        "user_message": user_message,
        "plan_update": { "summary": "", "events": [] },
        "command": null,
    });
    format!("```{}\n{}\n```\n", AGENT_FENCE_INFO, block)
}

/// Answers each request in turn and returns the prompt texts it got.
fn spawn_host(ep: String, replies: Vec<String>) -> common::Host<Vec<String>> {
    let listener = listen(&ep).expect("listen");
    let wake = ep.clone();
    let handle = thread::spawn(move || {
        let mut prompts = Vec::new();
        for text in replies {
            let stream = accept(&listener).expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut writer = BufWriter::new(&stream);
            let req: serde_json::Value =
                serde_json::from_slice(&read_frame(&mut reader).expect("read_frame")).unwrap();
            prompts.push(req["text"].as_str().unwrap_or_default().to_string());
            let reply = BridgeResponse::AskResult {
                id: req["id"].as_str().unwrap().to_string(),
                text,
                conversation_url: Some(CHAT.to_string()),
                warning: None,
                diagnostics: None,
            };
            write_frame(&mut writer, &serde_json::to_vec(&reply).unwrap()).unwrap();
            writer.flush().unwrap();
        }
        release(&ep);
        prompts
    });
    common::Host::new(wake, handle)
}

#[test]
fn the_repair_prompt_restates_the_contract_and_a_good_repair_goes_on() {
    let dir = scratch_dir("ok");
    let ep = format!("/tmp/cgb-ar-ok-{}.sock", std::process::id());
    let host = spawn_host(
        ep.clone(),
        vec!["Sure, I will delete it.".to_string(), final_reply("REPAIRED")],
    );
    let out = Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--socket", &ep, "agent", "--no-pretty", "go"])
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn cgpt");
    let prompts = host.join().expect("mock host panicked");

    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("REPAIRED"), "{:?}", out);
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    // The contract first, as on a first turn, then the repair text in the
    // place of the task the contract announces.
    assert!(prompts[1].starts_with(AGENT_PROMPT_CONTRACT), "the repair lacks the contract");
    assert!(prompts[1].trim_end().ends_with(REPAIR_PROMPT), "{}", prompts[1]);
    let _ = std::fs::remove_dir_all(&dir);
}
