use rustedoutclient::{
    connection::bounded_vnc_channels,
    ssh::ProxyStream,
    vnc::{VncClient, VncOptions},
};

async fn raw_stream_is_not_a_trusted_vnc_transport(stream: ProxyStream) {
    let (_ui, session) = bounded_vnc_channels();
    let _ = VncClient::run(
        stream,
        VncOptions::default(),
        session.event_tx,
        session.command_rx,
    )
    .await;
}

fn main() {}
