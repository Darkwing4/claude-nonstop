use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

const MODEL_FALLBACK: &str = "claude-mock";
const USAGE_LIMIT_MESSAGE: &str =
    "This request would exceed your account's rate limit. Please try again later.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitClaim {
    FiveHour,
    SevenDay,
}

#[derive(Clone, Debug)]
pub enum Reply {
    Ok {
        text: String,
    },
    Status {
        code: u16,
        error_type: String,
        message: String,
        headers: Vec<(String, String)>,
    },
    UsageLimit {
        claim: LimitClaim,
        reset_at: u64,
    },
    ResetConnection,
    Hang,
    HangAfterHeaders,
    HangMidStream,
    CutStream,
    SseError {
        error_type: String,
    },
}

impl Reply {
    pub fn ok(text: &str) -> Reply {
        Reply::Ok {
            text: text.to_owned(),
        }
    }

    pub fn status(code: u16, error_type: &str, message: &str) -> Reply {
        Reply::Status {
            code,
            error_type: error_type.to_owned(),
            message: message.to_owned(),
            headers: Vec::new(),
        }
    }

    pub fn server_error() -> Reply {
        Reply::status(500, "api_error", "Internal server error")
    }

    pub fn overloaded() -> Reply {
        Reply::status(529, "overloaded_error", "Overloaded")
    }

    pub fn usage_limit(claim: LimitClaim, reset_at: u64) -> Reply {
        Reply::UsageLimit { claim, reset_at }
    }

    fn label(&self) -> String {
        match self {
            Reply::Ok { .. } => "ok".to_owned(),
            Reply::Status { code, .. } => format!("status {code}"),
            Reply::UsageLimit { claim, reset_at } => {
                format!("usage limit {claim:?} until {reset_at}")
            }
            other => format!("{other:?}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub number: usize,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub reply: String,
}

impl Request {
    pub fn is_messages(&self) -> bool {
        self.method == "POST"
            && self.path.starts_with("/v1/messages")
            && !self.path.starts_with("/v1/messages/count_tokens")
    }

    pub fn is_main_loop(&self) -> bool {
        self.is_messages()
            && self.body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
    }

    pub fn is_stream(&self) -> bool {
        self.body["stream"] == Value::Bool(true)
    }

    pub fn model(&self) -> &str {
        self.body["model"].as_str().unwrap_or(MODEL_FALLBACK)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn last_user_text(&self) -> Option<String> {
        let messages = self.body["messages"].as_array()?;
        let last = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")?;

        match &last["content"] {
            Value::String(text) => Some(text.clone()),
            Value::Array(blocks) => blocks
                .iter()
                .rev()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .find(|text| !text.starts_with("<system-reminder>"))
                .map(str::to_owned),
            _ => None,
        }
    }
}

pub struct MockApi {
    port: u16,
    shared: Arc<Shared>,
    acceptor: Option<JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    log: PathBuf,
}

struct State {
    script: VecDeque<Reply>,
    fallback: Reply,
    requests: Vec<Request>,
    open: HashMap<u64, TcpStream>,
    next_connection: u64,
    accepting: bool,
}

impl MockApi {
    pub fn start(dir: &Path) -> MockApi {
        std::fs::create_dir_all(dir).expect("create the mock directory");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock API");
        let port = listener.local_addr().expect("mock address").port();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                script: VecDeque::new(),
                fallback: Reply::ok("ok"),
                requests: Vec::new(),
                open: HashMap::new(),
                next_connection: 1,
                accepting: true,
            }),
            log: dir.join("requests.jsonl"),
        });
        let acceptor = spawn_acceptor(listener, Arc::clone(&shared));

        MockApi {
            port,
            shared,
            acceptor: Some(acceptor),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn enqueue(&self, replies: impl IntoIterator<Item = Reply>) {
        self.shared.lock().script.extend(replies);
    }

    pub fn set_fallback(&self, reply: Reply) {
        self.shared.lock().fallback = reply;
    }

    pub fn requests(&self) -> Vec<Request> {
        self.shared.lock().requests.clone()
    }

    pub fn main_loop_requests(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(Request::is_main_loop)
            .collect()
    }

    pub fn refuse_connections(&mut self) {
        self.stop_accepting();
        self.shared.drop_open_connections();
    }

    pub fn accept_connections(&mut self) {
        if self.acceptor.is_some() {
            return;
        }

        let listener = TcpListener::bind(("127.0.0.1", self.port)).expect("rebind the mock API");
        self.shared.lock().accepting = true;
        self.acceptor = Some(spawn_acceptor(listener, Arc::clone(&self.shared)));
    }

    fn stop_accepting(&mut self) {
        let Some(acceptor) = self.acceptor.take() else {
            return;
        };

        self.shared.lock().accepting = false;
        if let Err(error) = TcpStream::connect(("127.0.0.1", self.port)) {
            panic!("waking the mock acceptor failed: {error}");
        }

        acceptor.join().expect("mock acceptor thread panicked");
    }
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.stop_accepting();
        self.shared.drop_open_connections();
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn drop_open_connections(&self) {
        for (_, stream) in self.lock().open.drain() {
            if let Err(error) = stream.shutdown(Shutdown::Both) {
                if error.kind() != io::ErrorKind::NotConnected {
                    panic!("closing a mock connection failed: {error}");
                }
            }
        }
    }

    fn track(&self, stream: &TcpStream) -> u64 {
        let tracked = stream.try_clone().expect("clone a mock connection");
        let mut state = self.lock();
        let id = state.next_connection;
        state.next_connection += 1;
        state.open.insert(id, tracked);

        id
    }

    fn forget(&self, connection: u64) {
        self.lock().open.remove(&connection);
    }

    fn next_reply(&self, request: &Request) -> Option<Reply> {
        if !request.is_messages() {
            return None;
        }

        if !request.is_main_loop() {
            return Some(Reply::ok("ok"));
        }

        let mut state = self.lock();
        let fallback = state.fallback.clone();

        Some(state.script.pop_front().unwrap_or(fallback))
    }

    fn record(&self, mut request: Request, reply: Option<&Reply>) {
        let mut state = self.lock();
        request.number = state.requests.len() + 1;
        request.reply = reply.map_or_else(|| "stub".to_owned(), Reply::label);
        let line = json!({
            "number": request.number,
            "method": request.method,
            "path": request.path,
            "stream": request.is_stream(),
            "last_user_text": request.last_user_text(),
            "reply": request.reply,
            "headers": request.headers,
            "body": request.body,
        });
        state.requests.push(request);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .expect("open the mock request log");
        file.write_all(format!("{line}\n").as_bytes())
            .expect("append to the mock request log");
    }
}

fn spawn_acceptor(listener: TcpListener, shared: Arc<Shared>) -> JoinHandle<()> {
    thread::spawn(move || {
        for incoming in listener.incoming() {
            if !shared.lock().accepting {
                return;
            }

            let stream = match incoming {
                Ok(stream) => stream,
                Err(error) => panic!("mock accept failed: {error}"),
            };
            let connection = shared.track(&stream);
            let shared = Arc::clone(&shared);

            thread::spawn(move || {
                serve(stream, &shared);
                shared.forget(connection);
            });
        }
    })
}

fn serve(stream: TcpStream, shared: &Shared) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone a mock connection"));
    let mut writer = stream;

    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(error) if is_disconnect(&error) => return,
            Err(error) => panic!("mock could not read a request: {error}"),
        };

        let reply = shared.next_reply(&request);
        let stream = request.is_stream();
        let model = request.model().to_owned();
        let path = request.path.clone();
        shared.record(request, reply.as_ref());

        let outcome = match &reply {
            Some(reply) => respond(&mut writer, &mut reader, reply, stream, &model),
            None => answer_stub(&mut writer, &path),
        };

        match outcome {
            Ok(Connection::KeepAlive) => continue,
            Ok(Connection::Close) => return,
            Err(error) if is_disconnect(&error) => return,
            Err(error) => panic!("mock could not answer: {error}"),
        }
    }
}

enum Connection {
    KeepAlive,
    Close,
}

fn respond(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    reply: &Reply,
    stream: bool,
    model: &str,
) -> io::Result<Connection> {
    match reply {
        Reply::Ok { text } if stream => {
            start_sse(writer)?;
            writer.write_all(&message_start(model))?;
            writer.write_all(&text_block(text))?;
            writer.write_all(&message_end())?;

            Ok(Connection::Close)
        }
        Reply::Ok { text } => {
            write_json(writer, 200, &message_json(model, text), &[])?;

            Ok(Connection::KeepAlive)
        }
        Reply::Status {
            code,
            error_type,
            message,
            headers,
        } => {
            write_json(writer, *code, &error_json(error_type, message), headers)?;

            Ok(Connection::KeepAlive)
        }
        Reply::UsageLimit { claim, reset_at } => {
            let body = error_json("rate_limit_error", USAGE_LIMIT_MESSAGE);
            write_json(writer, 429, &body, &usage_limit_headers(*claim, *reset_at))?;

            Ok(Connection::KeepAlive)
        }
        Reply::ResetConnection => {
            rustix::net::sockopt::set_socket_linger(&*writer, Some(Duration::ZERO))?;

            Ok(Connection::Close)
        }
        Reply::Hang => {
            hang(reader);

            Ok(Connection::Close)
        }
        Reply::HangAfterHeaders => {
            start_sse(writer)?;
            writer.flush()?;
            hang(reader);

            Ok(Connection::Close)
        }
        Reply::HangMidStream => {
            start_sse(writer)?;
            writer.write_all(&message_start(model))?;
            writer.flush()?;
            hang(reader);

            Ok(Connection::Close)
        }
        Reply::CutStream => {
            start_sse(writer)?;
            writer.write_all(&message_start(model))?;
            writer.write_all(&text_block("partial answer before the cut"))?;
            writer.flush()?;
            writer.shutdown(Shutdown::Both)?;

            Ok(Connection::Close)
        }
        Reply::SseError { error_type } => {
            start_sse(writer)?;
            writer.write_all(&message_start(model))?;
            writer.write_all(&text_block("partial answer before the error"))?;
            writer.write_all(&sse(
                "error",
                &json!({"type": "error", "error": {"type": error_type, "message": "Overloaded"}}),
            ))?;

            Ok(Connection::Close)
        }
    }
}

fn answer_stub(writer: &mut TcpStream, path: &str) -> io::Result<Connection> {
    if path.starts_with("/v1/messages/count_tokens") {
        write_json(writer, 200, &json!({"input_tokens": 100}), &[])?;
    } else {
        write_json(writer, 200, &json!({"data": [], "has_more": false}), &[])?;
    }

    Ok(Connection::KeepAlive)
}

fn hang(reader: &mut BufReader<TcpStream>) {
    let mut sink = [0u8; 4096];

    loop {
        match reader.read(&mut sink) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(error) if is_disconnect(&error) => return,
            Err(error) => panic!("mock lost a hanging connection: {error}"),
        }
    }
}

fn usage_limit_headers(claim: LimitClaim, reset_at: u64) -> Vec<(String, String)> {
    let reset = reset_at.to_string();
    let pairs: Vec<(&str, &str)> = match claim {
        LimitClaim::FiveHour => vec![
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", &reset),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
            ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
            ("anthropic-ratelimit-unified-5h-reset", &reset),
            ("anthropic-ratelimit-unified-5h-surpassed-threshold", "1.0"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "out_of_credits",
            ),
        ],
        LimitClaim::SevenDay => vec![
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", &reset),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "seven_day",
            ),
            ("anthropic-ratelimit-unified-7d-utilization", "1.0"),
            ("anthropic-ratelimit-unified-7d-reset", &reset),
        ],
    };

    pairs
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn start_sse(writer: &mut TcpStream) -> io::Result<()> {
    writer.write_all(
        b"HTTP/1.1 200 OK\r\n\
          content-type: text/event-stream\r\n\
          cache-control: no-cache\r\n\
          request-id: req_mock\r\n\
          connection: close\r\n\r\n",
    )
}

fn write_json(
    writer: &mut TcpStream,
    code: u16,
    body: &Value,
    headers: &[(String, String)],
) -> io::Result<()> {
    let body = body.to_string();
    let mut head = format!(
        "HTTP/1.1 {code} {}\r\ncontent-type: application/json\r\nrequest-id: req_mock\r\ncontent-length: {}\r\n",
        reason(code),
        body.len()
    );

    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }

    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.write_all(body.as_bytes())?;

    writer.flush()
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        529 => "Site Overloaded",
        _ => "Mock Status",
    }
}

fn sse(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

fn message_start(model: &str) -> Vec<u8> {
    sse(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_mock",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 10, "output_tokens": 1},
            },
        }),
    )
}

fn text_block(text: &str) -> Vec<u8> {
    let mut bytes = sse(
        "content_block_start",
        &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    );
    bytes.extend(sse(
        "content_block_delta",
        &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
    ));

    bytes
}

fn message_end() -> Vec<u8> {
    let mut bytes = sse(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    );
    bytes.extend(sse(
        "message_delta",
        &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 1}}),
    ));
    bytes.extend(sse("message_stop", &json!({"type": "message_stop"})));

    bytes
}

fn message_json(model: &str, text: &str) -> Value {
    json!({
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 1},
    })
}

fn error_json(error_type: &str, message: &str) -> Value {
    json!({
        "type": "error",
        "error": {"type": error_type, "message": message},
        "request_id": "req_mock",
    })
}

fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Request>> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None);
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }

        let line = line.trim_end();
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.contains("chunked"));
    if chunked {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "chunked request bodies are not supported by the mock",
        ));
    }

    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse::<usize>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let body = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));

    Ok(Some(Request {
        number: 0,
        method,
        path,
        headers,
        body,
        reply: String::new(),
    }))
}

fn is_disconnect(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionAborted
    )
}
