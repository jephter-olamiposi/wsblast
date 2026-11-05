//! Signed and fetched tokens through agents: the compiled binary
//! coordinates agents running in this process, signs a JWT or fetches an
//! OAuth2 token from an in-process endpoint, and pushes every new token to
//! the agents, whose handshakes an in-process server checks.
#![cfg(all(feature = "distributed", any(feature = "jwt", feature = "oauth2")))]

mod common;

use common::{Verify, VerifyingServer};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use wsblast::distributed::{AgentConfig, AgentServer};

const TOKEN: &str = "auth-agents-token-0123456789";

/// `count` agents in this process, stopped when the token is dropped.
struct Agents {
    addresses: Vec<String>,
    stop: CancellationToken,
}

impl Agents {
    async fn spawn(count: usize) -> Self {
        let stop = CancellationToken::new();
        let mut addresses = Vec::new();
        for _ in 0..count {
            let config = AgentConfig::builder()
                .listen("127.0.0.1:0")
                .token(TOKEN)
                .build()
                .unwrap();
            let agent = AgentServer::bind(config).await.unwrap();
            addresses.push(agent.local_addr().to_string());
            tokio::spawn(agent.run(stop.clone()));
        }
        Self { addresses, stop }
    }

    fn list(&self) -> String {
        self.addresses.join(",")
    }
}

impl Drop for Agents {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn coordinator(args: &[&str], env: &[(&str, &str)]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .env("WSBLAST_AGENT_TOKEN", TOKEN)
        .env_remove("WSBLAST_JWT_KEY_FILE")
        .env_remove("WSBLAST_OAUTH2_CLIENT_SECRET")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("wsblast did not exit")
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(feature = "jwt")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_jwt_is_re_signed_on_the_coordinator_and_reaches_every_agent() {
    let secret = b"distributed-jwt-secret-0123456789abcdef";
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let verify: Verify = Arc::new(move |token| {
        common::split_jwt(token).is_some_and(|(input, claims, signature)| {
            ring::hmac::verify(&key, input.as_bytes(), &signature).is_ok()
                && common::jwt_is_current(&claims)
                && claims["sub"] == "agents"
        })
    });
    // Sessions end after 20 replies, so every agent reconnects often.
    let server = VerifyingServer::spawn(20, verify).await;
    let agents = Agents::spawn(2).await;
    let key_file = common::write_file("distributed-hs256.key", secret);
    let list = agents.list();
    let output = coordinator(
        &[
            &server.url,
            "-c",
            "2",
            "-d",
            "3s",
            "-r",
            "100",
            "--no-progress",
            "--auth-type",
            "jwt",
            "--auth-jwt-key",
            key_file.to_str().unwrap(),
            "--auth-jwt-claims",
            r#"{"sub":"agents"}"#,
            "--auth-jwt-ttl",
            "2s",
            "--agents",
            &list,
        ],
        &[],
    )
    .await;

    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let seen = server.seen();
    assert!(seen.len() >= 6, "too few handshakes: {seen:?}");
    assert!(seen.iter().all(|(_, valid)| *valid), "{seen:?}");
    // A 2 s token is re-signed before the 3 s run ends, on the coordinator
    // only: the server sees two different valid signatures, and the
    // reconnects after the refresh carry the new one.
    let signatures: Vec<Vec<u8>> = seen
        .iter()
        .map(|(token, _)| common::split_jwt(token).unwrap().2)
        .collect();
    let distinct: std::collections::HashSet<&Vec<u8>> = signatures.iter().collect();
    assert!(
        distinct.len() >= 2,
        "the agents never received a re-signed token: {seen:?}"
    );
    assert_ne!(signatures.first(), signatures.last(), "{seen:?}");
    // Neither the key nor a token is printed.
    for (token, _) in &seen {
        assert!(!stderr.contains(token.as_str()), "{stderr}");
        assert!(!text(&output.stdout).contains(token.as_str()));
    }
}

#[cfg(feature = "oauth2")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oauth2_token_is_refreshed_on_the_coordinator_and_reaches_every_agent() {
    let tokens = common::TokenServer::spawn(Arc::new(|n| {
        (
            "200 OK",
            format!(r#"{{"access_token":"at-{n}","token_type":"Bearer","expires_in":2}}"#),
        )
    }))
    .await;
    let server = VerifyingServer::spawn(20, Arc::new(|token| token.starts_with("at-"))).await;
    let agents = Agents::spawn(2).await;
    let list = agents.list();
    let output = coordinator(
        &[
            &server.url,
            "-c",
            "2",
            "-d",
            "3s",
            "-r",
            "100",
            "--no-progress",
            "--auth-type",
            "oauth2",
            "--auth-oauth2-token-url",
            &tokens.url,
            "--auth-oauth2-client-id",
            "load-test",
            "--agents",
            &list,
        ],
        &[("WSBLAST_OAUTH2_CLIENT_SECRET", "s3cret")],
    )
    .await;

    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    // Only the coordinator talks to the token endpoint: one grant at the
    // start and one refresh before the 2 s token expires, not one per agent.
    let requests = tokens.requests();
    assert!(
        (2..=3).contains(&requests.len()),
        "expected the coordinator's grant and its refresh: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.body.starts_with("grant_type=client_credentials")),
        "{requests:?}"
    );
    let numbers: Vec<usize> = server
        .seen()
        .iter()
        .map(|(token, valid)| {
            assert!(valid, "{token}");
            token.strip_prefix("at-").unwrap().parse().unwrap()
        })
        .collect();
    assert!(numbers.len() >= 6, "too few handshakes: {numbers:?}");
    // Both agents' first handshakes carry the first token, and the
    // reconnects after the refresh carry the refreshed one.
    assert_eq!(&numbers[..2], [1, 1], "{numbers:?}");
    assert!(
        *numbers.last().unwrap() >= 2,
        "the agents never received the refreshed token: {numbers:?}"
    );
    assert!(!stderr.contains("at-1"), "{stderr}");
    assert!(!stderr.contains("s3cret"), "{stderr}");
}
