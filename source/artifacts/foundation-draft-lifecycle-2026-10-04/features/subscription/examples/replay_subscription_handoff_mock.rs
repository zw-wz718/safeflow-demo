//! Fixed local integration demonstration; no runtime input or real delivery.
#[path = "support/subscription_handoff_mock.rs"]
mod handoff;

fn main() {
    if std::env::args_os().len() != 1 {
        eprintln!("USAGE: replay_subscription_handoff_mock (fixed fictional scenarios only)");
        std::process::exit(2);
    }
    match handoff::replay_mock().and_then(|report| {
        serde_json::to_string_pretty(&report).map_err(|_| "HANDOFF_SERIALIZE_FAILED")
    }) {
        Ok(output) => println!("{output}"),
        Err(_) => {
            eprintln!("HANDOFF_REPLAY_FAILED");
            std::process::exit(1);
        }
    }
}
