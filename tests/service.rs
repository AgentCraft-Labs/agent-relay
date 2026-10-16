use agent_relay::service;
use std::{
    io::{Read, Write},
    net::TcpListener,
};

fn health_response(body: &'static str) -> anyhow::Result<bool> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let count = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.starts_with("GET /version HTTP/1.1\r\n"));
        assert!(!request.to_lowercase().contains("authorization"));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let result = service::healthy(address);
    server.join().unwrap();
    result
}

#[test]
fn local_health_checks_the_service_identity_without_credentials() {
    assert!(health_response(r#"{"service":"AgentRelay","version":"test"}"#).unwrap());
    assert!(health_response(r#"{"service":"unrelated"}"#).is_err());
    assert!(health_response("not JSON").is_err());
}

#[test]
fn missing_listener_is_recoverable_but_nonlocal_addresses_are_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    assert!(!service::healthy(address).unwrap());
    assert!(service::healthy("0.0.0.0:39091".parse().unwrap()).is_err());
}
