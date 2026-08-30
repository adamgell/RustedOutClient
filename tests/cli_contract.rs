use clap::Parser;
use rustedoutclient::cli::{Cli, Command, ViewerMode};

#[test]
fn cli_has_compatibility_commands_without_endpoint_or_secret_flags() {
    assert!(Cli::try_parse_from(["rustedoutclient", "list"]).is_ok());
    assert!(Cli::try_parse_from(["rustedoutclient", "open", "labz1-cm01", "--view-only"]).is_ok());
    assert!(Cli::try_parse_from(["rustedoutclient", "probe", "107", "--json"]).is_ok());
    assert!(Cli::try_parse_from(["rustedoutclient", "--password", "bad"]).is_err());
    assert!(Cli::try_parse_from(["rustedoutclient", "--host", "example.invalid"]).is_err());
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
            view_only: false,
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
            json: false,
        } if selector == "107"
    ));
}
