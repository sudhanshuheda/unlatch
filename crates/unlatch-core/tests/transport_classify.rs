//! ssh failure classification (real OpenSSH / Tailscale / device-code stderr samples), ssh argv
//! construction and client-name sanitizing.

use std::path::PathBuf;
use unlatch_core::transport::{classify_ssh_failure, sanitize_client_name, ssh_argv};
use unlatch_core::{EngineConfig, Transport};
use unlatch_proto::ErrorCode::{self, NeedsUser, Offline, Protocol};

/// (stderr as printed by ssh, exit code, expected class, expected url)
const SAMPLES: &[(&str, Option<i32>, ErrorCode, Option<&str>)] = &[
    // Captured on this machine (OpenSSH 9.6).
    (
        "No ED25519 host key is known for localhost and you have requested strict checking.\r\nHost key verification failed.\r\n",
        Some(255),
        NeedsUser,
        None,
    ),
    ("ssh: connect to host localhost port 1: Connection refused\r\n", Some(255), Offline, None),
    ("ssh: Could not resolve hostname nosuchhost.invalid: Name or service not known\r\n", Some(255), Offline, None),
    ("me@localhost: Permission denied (publickey).\r\n", Some(255), NeedsUser, None),
    (
        "Load key \"/dev/null\": error in libcrypto\r\nme@localhost: Permission denied (publickey).\r\n",
        Some(255),
        NeedsUser,
        None,
    ),
    ("ssh: connect to host 10.255.255.1 port 22: Connection timed out\r\n", Some(255), Offline, None),
    // OpenSSH, from its source / common reports.
    (
        "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
         @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
         @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
         IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\n\
         Host key for devbox has changed and you have requested strict checking.\n\
         Host key verification failed.\n",
        Some(255),
        NeedsUser,
        None,
    ),
    (
        "Received disconnect from 10.0.0.7 port 22:2: Too many authentication failures\r\nDisconnected from 10.0.0.7 port 22\r\n",
        Some(255),
        NeedsUser,
        None,
    ),
    ("dev@10.0.0.7: Permission denied (keyboard-interactive).\r\n", Some(255), NeedsUser, None),
    ("(dev@bastion) Verification code: \r\n", Some(255), NeedsUser, None),
    (
        "sign_and_send_pubkey: signing failed for ED25519 \"op://Private/dev\" from agent: agent refused operation\r\ndev@vm: Permission denied (publickey).\r\n",
        Some(255),
        NeedsUser,
        None,
    ),
    (
        "Load key \"/Users/me/.ssh/id_ed25519\": incorrect passphrase supplied to decrypt private key\r\n",
        Some(255),
        NeedsUser,
        None,
    ),
    ("ssh_askpass: exec(/usr/X11R6/bin/ssh-askpass): No such file or directory\r\n", Some(255), NeedsUser, None),
    // Tailscale SSH check mode (blocks; seen on timeout).
    (
        "# Tailscale SSH requires an additional check.\n# To authenticate, visit: https://login.tailscale.com/a/l2f9a8c7d6e5\n",
        None,
        NeedsUser,
        Some("https://login.tailscale.com/a/l2f9a8c7d6e5"),
    ),
    // Device-code logins in a ProxyCommand (cloud CLIs).
    (
        "To sign in, use a web browser to open the page https://microsoft.com/devicelogin and enter the code F7X2KQ9LM to authenticate.\n",
        None,
        NeedsUser,
        Some("https://microsoft.com/devicelogin"),
    ),
    (
        "If the browser does not open, visit (https://teleport.example.com:3080/web/login?redirect=1).\n",
        None,
        NeedsUser,
        Some("https://teleport.example.com:3080/web/login?redirect=1"),
    ),
    // Network trouble.
    ("kex_exchange_identification: read: Connection reset by peer\r\nConnection reset by 10.0.0.7 port 22\r\n", Some(255), Offline, None),
    ("kex_exchange_identification: Connection closed by remote host\r\nConnection closed by 10.0.0.7 port 22\r\n", Some(255), Offline, None),
    ("ssh: connect to host 192.168.64.3 port 22: No route to host\r\n", Some(255), Offline, None),
    ("ssh: connect to host vm port 22: Network is unreachable\r\n", Some(255), Offline, None),
    ("ssh: connect to host mac-vm.local port 22: Operation timed out\r\n", Some(255), Offline, None),
    ("ssh: Could not resolve hostname vm: nodename nor servname provided, or not known\r\n", Some(255), Offline, None),
    ("ssh: Could not resolve hostname vm: Temporary failure in name resolution\r\n", Some(255), Offline, None),
    ("client_loop: send disconnect: Broken pipe\r\n", Some(255), Offline, None),
    ("Timeout, server vm not responding.\r\n", Some(255), Offline, None),
    // A benign warning followed by an unrecognised ssh failure: retry.
    ("Warning: Permanently added 'vm' (ED25519) to the list of known hosts.\r\nmux_client_request_session: read from master failed\r\n", Some(255), Offline, None),
    // The remote command itself failed (not an ssh problem).
    ("sh: 1: exec: /home/me/.unlatch/unlatchd: Exec format error\n", Some(126), Protocol, None),
    // A URL without login wording is not a login prompt.
    ("docs at https://example.com/x\n", Some(255), Offline, None),
];

#[test]
fn classification_table() {
    for (stderr, exit, code, url) in SAMPLES {
        let (got, got_url) = classify_ssh_failure(stderr, *exit);
        assert_eq!(got, *code, "wrong class for {stderr:?}");
        assert_eq!(got_url.as_deref(), *url, "wrong url for {stderr:?}");
    }
}

#[test]
fn needs_user_url_survives_in_error_messages() {
    // `open` embeds stderr in the error message; re-classifying the message recovers the URL.
    let msg = "ssh: no unlatch handshake within 30s: # To authenticate, visit: https://login.tailscale.com/a/abc";
    assert_eq!(
        classify_ssh_failure(msg, None),
        (NeedsUser, Some("https://login.tailscale.com/a/abc".into()))
    );
}

fn ssh_cfg(extra: Vec<String>, port: Option<u16>, identity: Option<PathBuf>) -> EngineConfig {
    EngineConfig::new(
        "d",
        Transport::Ssh {
            destination: "dev@vm".into(),
            port,
            identity,
            extra_args: extra,
        },
        "/w",
        PathBuf::from("/tmp/s"),
        "mac",
    )
}

fn has_opt(a: &[String], o: &str) -> bool {
    a.windows(2).any(|w| w[0] == "-o" && w[1] == o)
}

#[test]
fn argv_background_vs_interactive() {
    let cfg = ssh_cfg(
        vec!["-o".into(), "ProxyJump=bastion".into()],
        Some(2222),
        Some("/k/id".into()),
    );
    let bg = ssh_argv(&cfg, false);
    assert_eq!(&bg[..2], ["ssh", "-T"]);
    for o in [
        "Compression=no",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "BatchMode=yes",
        "ControlMaster=auto",
        "ControlPath=~/.ssh/unlatch-%C",
        "ControlPersist=10m",
        "IdentitiesOnly=yes",
    ] {
        assert!(has_opt(&bg, o), "missing {o} in {bg:?}");
    }
    let s = bg.join(" ");
    assert!(s.contains("-i /k/id"), "{s}");
    assert!(s.contains("-p 2222"), "{s}");
    assert!(s.ends_with("-- dev@vm"), "{s}");
    // User args come first: ssh keeps the first value of an option, so they win.
    let user = bg
        .iter()
        .position(|x| x == "ProxyJump=bastion")
        .expect("extra arg");
    let ours = bg
        .iter()
        .position(|x| x == "Compression=no")
        .expect("default");
    assert!(user < ours);

    let it = ssh_argv(&cfg, true);
    assert!(
        !has_opt(&it, "BatchMode=yes"),
        "interactive must allow askpass: {it:?}"
    );
    assert!(
        has_opt(&it, "ControlMaster=auto"),
        "interactive connect creates the shared master"
    );
}

#[test]
fn argv_minimal_and_hostile_destination() {
    let cfg = EngineConfig::new(
        "d",
        Transport::Ssh {
            destination: "-oProxyCommand=evil".into(),
            port: None,
            identity: None,
            extra_args: vec![],
        },
        "/w",
        PathBuf::from("/tmp/s"),
        "mac",
    );
    let a = ssh_argv(&cfg, false);
    assert!(!a
        .iter()
        .any(|x| x == "-p" || x == "-i" || x == "IdentitiesOnly=yes"));
    let n = a.len();
    assert_eq!(
        &a[n - 2..],
        ["--", "-oProxyCommand=evil"],
        "destination must follow --"
    );
}

#[test]
fn client_names() {
    assert_eq!(
        sanitize_client_name("Sudhanshu’s MacBook Pro"),
        "Sudhanshu’s MacBook Pro"
    );
    assert_eq!(sanitize_client_name("a/b\0c\nd\te"), "abcde");
    assert_eq!(sanitize_client_name(""), "mac");
    assert_eq!(sanitize_client_name("  \u{7}/ "), "mac");
    let long = "é".repeat(40); // 80 bytes
    let s = sanitize_client_name(&long);
    assert!(s.len() <= 32 && s.len() == 32, "{} bytes", s.len());
    assert!(s.chars().all(|c| c == 'é'));
    let mixed = format!("{}€", "a".repeat(31)); // € is 3 bytes: must not be split
    assert_eq!(sanitize_client_name(&mixed), "a".repeat(31));
    assert_eq!(sanitize_client_name("  Mac mini  "), "Mac mini");
}
