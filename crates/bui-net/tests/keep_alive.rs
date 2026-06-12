//! Keep-alive pool behavior against a tiny in-process HTTP server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bui_net::{Client, Method, Request, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Read until the end of the request head. Returns false on EOF.
async fn read_request(sock: &mut tokio::net::TcpStream) -> bool {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match sock.read(&mut byte).await {
            Ok(0) | Err(_) => return false,
            Ok(_) => buf.push(byte[0]),
        }
        if buf.ends_with(b"\r\n\r\n") {
            return true;
        }
    }
}

/// Serve `responses_per_conn` framed responses on each accepted
/// connection (then close it), counting accepts.
fn spawn_server(
    listener: TcpListener,
    accepted: Arc<AtomicUsize>,
    responses_per_conn: usize,
) {
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                for _ in 0..responses_per_conn {
                    if !read_request(&mut sock).await {
                        return;
                    }
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await;
                    let _ = sock.flush().await;
                }
                // Server-side keep-alive expired: hang up.
            });
        }
    });
}

#[test]
fn sequential_gets_reuse_one_connection() {
    rt().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        spawn_server(listener, accepted.clone(), usize::MAX);

        let client = Client::new();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        for _ in 0..3 {
            let resp = client.get(&url).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body, b"ok");
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "all GETs share one connection");
    });
}

#[test]
fn stale_pooled_connection_retries_on_fresh_one() {
    rt().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        // One response per connection: the pooled socket is dead by the
        // time the second GET tries it.
        spawn_server(listener, accepted.clone(), 1);

        let client = Client::new();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let first = client.get(&url).await.unwrap();
        assert_eq!(first.body, b"ok");
        // Give the server task a beat to close its side.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = client.get(&url).await.unwrap();
        assert_eq!(second.body, b"ok", "stale socket retried transparently");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn posts_do_not_reuse_connections() {
    rt().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        spawn_server(listener, accepted.clone(), usize::MAX);

        let client = Client::new();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        for _ in 0..2 {
            let mut req = Request::get(url.clone());
            req.method = Method::Post;
            req.body = Some(b"a=1".to_vec());
            let resp = client.send(req).await.unwrap();
            assert_eq!(resp.status, 200);
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            2,
            "each POST gets a fresh connection"
        );
    });
}
