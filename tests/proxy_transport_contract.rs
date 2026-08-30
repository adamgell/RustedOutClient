use std::path::PathBuf;

use rustedoutclient::{
    model::{NodeName, PveProfile, SshTarget, VmId},
    ssh::{ProxyStream, ProxyTicket, SshCommandFactory},
};
use tokio::io::{AsyncRead, AsyncWrite};

const SOCKET: &str = "/tmp/roc-test/c";

fn fixture_profile() -> PveProfile {
    PveProfile {
        name: "Synthetic Proxmox".to_owned(),
        ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    }
}

#[test]
fn ticket_public_contract_is_fixed_length_alphanumeric_and_redacted() {
    let ticket = ProxyTicket::generate();

    assert_eq!(ticket.auth_len(), 8);
    assert!(ticket.auth_is_ascii_alphanumeric());
    assert_eq!(format!("{ticket:?}"), "ProxyTicket([REDACTED])");
}

#[test]
fn only_proxy_has_one_sendenv_and_the_fixed_single_remote_argument() {
    let profile = fixture_profile();
    let factory = SshCommandFactory::new(PathBuf::from(SOCKET));
    let ticket = ProxyTicket::generate();
    let proxy = factory
        .proxy(&profile, VmId::new(107).unwrap(), &ticket)
        .unwrap();

    assert_eq!(
        proxy
            .args
            .iter()
            .filter(|arg| *arg == "SendEnv=LC_PVE_TICKET")
            .count(),
        1
    );
    assert_eq!(proxy.args.last().unwrap(), "qm vncproxy 107");
    assert_eq!(proxy.environment_variable_count(), 1);

    for spec in [
        factory.master(&profile).unwrap(),
        factory.check(&profile).unwrap(),
        factory.exit(&profile).unwrap(),
        factory.inventory(&profile).unwrap(),
    ] {
        assert_eq!(spec.environment_variable_count(), 0);
        assert!(!spec.args.iter().any(|arg| arg == "SendEnv=LC_PVE_TICKET"));
    }
}

#[test]
fn proxy_stream_is_a_native_async_byte_stream() {
    fn assert_stream<T: AsyncRead + AsyncWrite + Unpin>() {}
    assert_stream::<ProxyStream>();
}
