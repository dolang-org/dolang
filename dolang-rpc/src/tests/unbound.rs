use crate::{Builder, Error, Protocol};

struct Test;
impl Protocol for Test {
    type Request = u32;
    type Response = u32;
}

#[tokio::test]
async fn unbound_client_and_server_negotiate_and_bind_to_matching_protocol() {
    let (client_io, server_io) = crate::tests::duplex(4096);
    let (client_result, server_result) = tokio::join!(
        Builder::new("test", &[1]).client(client_io),
        Builder::new("test", &[1]).server(server_io),
    );
    let unbound_client = client_result.unwrap();
    let unbound_server = server_result.unwrap();
    assert_eq!(unbound_client.name(), "test");
    assert_eq!(unbound_client.version(), 1);
    assert_eq!(unbound_server.name(), "test");
    assert_eq!(unbound_server.version(), 1);

    let client = unbound_client.bind::<Test>();
    let server = unbound_server.bind::<Test>();
    tokio::spawn(server.serve(async |context, request: u32| {
        context.respond(request * 2);
    }));
    let response = client.call(21).await.unwrap().into_response();
    assert_eq!(response, 42);
}

#[tokio::test]
async fn unbound_negotiate_selects_max_overlapping_app_protocol_version() {
    let (client_io, server_io) = crate::tests::duplex(4096);
    let (client_result, server_result) = tokio::join!(
        Builder::new("test", &[1, 2, 3]).client(client_io),
        Builder::new("test", &[2, 3, 4]).server(server_io),
    );
    let unbound_client = client_result.unwrap();
    let unbound_server = server_result.unwrap();
    assert_eq!(unbound_client.version(), 3);
    assert_eq!(unbound_server.version(), 3);
}

#[tokio::test]
async fn unbound_negotiate_fails_on_mismatched_app_protocol_name() {
    let (client_io, server_io) = crate::tests::duplex(4096);
    let (client_result, server_result) = tokio::join!(
        Builder::new("test", &[1]).client(client_io),
        Builder::new("other", &[1]).server(server_io),
    );
    assert!(matches!(client_result, Err(Error::Protocol(_))));
    assert!(matches!(server_result, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn unbound_negotiate_fails_on_no_overlapping_app_protocol_version() {
    let (client_io, server_io) = crate::tests::duplex(4096);
    let (client_result, server_result) = tokio::join!(
        Builder::new("test", &[1]).client(client_io),
        Builder::new("test", &[2]).server(server_io),
    );
    assert!(matches!(client_result, Err(Error::Protocol(_))));
    assert!(matches!(server_result, Err(Error::Protocol(_))));
}
