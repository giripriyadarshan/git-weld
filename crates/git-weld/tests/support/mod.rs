//! A minimal HTTP/1.1 server for tests. It answers each connection with the
//! next scripted response and records every request it read.

#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the request body is JSON")
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    delay: Duration,
}

impl Response {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

pub struct Server {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    /// Serves `responses` in order, one per connection, on a loopback port.
    pub fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                if let Some(request) = read_request(&stream) {
                    recorded.lock().unwrap().push(request);
                }
                thread::sleep(response.delay);
                // The client may have given up already.
                let _ = write_response(stream, &response);
            }
        });
        Self { address, requests }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

/// An address on which nothing listens, so connections are refused.
pub fn closed_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn write_response(mut stream: TcpStream, response: &Response) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} Scripted\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()?;
    stream.shutdown(Shutdown::Both)
}
