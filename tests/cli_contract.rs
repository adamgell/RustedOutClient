use clap::Parser;
use rustedoutclient::cli::{Cli, Command, ViewerMode};

#[test]
fn cli_has_compatibility_commands_without_endpoint_or_secret_flags() {
    assert!(Cli::try_parse_from(["rustedoutclient", "list"]).is_ok());
    assert!(Cli::try_parse_from(["rustedoutclient", "open", "labz1-cm01", "--view-only"]).is_ok());
    assert!(
        Cli::try_parse_from(["rustedoutclient", "open", "labz1-cm01", "--view-only=false"])
            .is_err()
    );
    assert!(Cli::try_parse_from(["rustedoutclient", "probe", "107", "--json"]).is_ok());
    assert!(Cli::try_parse_from(["rustedoutclient", "--password", "bad"]).is_err());
    assert!(Cli::try_parse_from(["rustedoutclient", "--host", "example.invalid"]).is_err());
}

#[test]
fn probe_accepts_an_explicit_resize_target() {
    let command = Cli::try_parse_from(["rustedoutclient", "probe", "107", "--resize", "1600x900"])
        .unwrap()
        .command
        .unwrap();
    assert!(matches!(
        command,
        Command::Probe {
            resize: Some(size),
            ..
        } if size.width == 1_600 && size.height == 896
    ));

    for value in ["1920-by-1080", "639x480", "8193x480", "8192x8192"] {
        assert!(
            Cli::try_parse_from(["rustedoutclient", "probe", "107", "--resize", value]).is_err(),
            "accepted unsafe resize target {value}"
        );
    }
}

#[test]
fn cli_rejects_ticket_and_arbitrary_endpoint_flags() {
    for arguments in [
        &["rustedoutclient", "open", "107", "--ticket", "bad"][..],
        &["rustedoutclient", "open", "107", "--port", "5900"][..],
        &[
            "rustedoutclient",
            "probe",
            "107",
            "--endpoint",
            "example.invalid",
        ][..],
        &[
            "rustedoutclient",
            "list",
            "--ssh-target",
            "root@example.invalid",
        ][..],
    ] {
        assert!(
            Cli::try_parse_from(arguments).is_err(),
            "accepted: {arguments:?}"
        );
    }
}

#[test]
fn cli_defaults_to_gui_native_open_and_thirty_second_probe() {
    assert!(Cli::try_parse_from(["rustedoutclient"])
        .unwrap()
        .command
        .is_none());

    let open = Cli::try_parse_from(["rustedoutclient", "open", "107"])
        .unwrap()
        .command
        .unwrap();
    assert!(matches!(
        open,
        Command::Open {
            selector,
            fullscreen: false,
            view_only: None,
            viewer: ViewerMode::Native,
        } if selector == "107"
    ));

    let probe = Cli::try_parse_from(["rustedoutclient", "probe", "107"])
        .unwrap()
        .command
        .unwrap();
    assert!(matches!(
        probe,
        Command::Probe {
            selector,
            timeout_seconds: 30,
            resize: None,
            json: false,
        } if selector == "107"
    ));
}
