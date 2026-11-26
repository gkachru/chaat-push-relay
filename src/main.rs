//! chaat-push-relay: a Chaat plugin that receives Microsoft Graph change
//! notifications through a cloudflared tunnel.
//!
//!   chaat-push-relay [serve]
//!       Talk to Chaat on stdin and stdout (this is how Chaat starts it).
//!
//! Logs go to stderr and never include notification bodies, channels or
//! tunnel addresses with a query.

use std::process::ExitCode;
use std::sync::Arc;

use chaat_push_relay::serve::{Cloudflared, serve};

const USAGE: &str = "usage: chaat-push-relay [serve]";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") if args.len() <= 1 => {
            let result = serve(
                tokio::io::stdin(),
                tokio::io::stdout(),
                Arc::new(Cloudflared::new()),
            )
            .await;
            let code = match result {
                Ok(()) => 0,
                Err(_) => {
                    eprintln!("chaat-push-relay: could not talk to Chaat");
                    1
                }
            };
            // Exit now: a read of stdin that is still waiting on its helper
            // thread would otherwise hold the process open after `shutdown`.
            std::process::exit(code);
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
