extern crate reqwest;

use std::net::SocketAddr;
use std::ops::DerefMut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use flexi_logger::{Logger, opt_format};
use log::{debug, error, info};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Stdin, Stdout};
use tokio::net::TcpListener;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::runtime::Handle;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout, Duration};
use regex::Regex;

use crate::constants::*;

enum ClientOpStatus {
	OK,
	Done,
	Error,
	SessionId(String),
}

// Streams are not object safe. I have not figured out how to treat the read/write streams as dyn objects
// so for the time being I use an enum wrapper with different code paths in every place I read/write. This
// is not ideal but it's also not a blocker for initial release.

enum ClientReader {
	Stream(OwnedReadHalf),
	Stdin(Stdin),
}

enum ClientWriter {
	Stream(OwnedWriteHalf),
	Stdout(Stdout),
}

struct ClientSession {
	id: String,						// session ID recieved from server
	base_url: String,				// URL of the API root, including http/https
	reader: Mutex<ClientReader>,	// read half of the session connection
	writer: Mutex<ClientWriter>,	// write half of the session connection
	open: AtomicBool,				// flag to indicate the session is still open (futures will terminate when false)
	heartbeat: AtomicU64,			// futures increment this value when active as a non-idle indicator
	rth: Arc<Handle>,				// tokio runtime handle (to spawn futures)
	client: reqwest::Client,		// reqwest client instance
	timeout: u32,					// timeout configured for this session
}

enum ClientError {
	CLIENT(ClientOpStatus),
	IO(std::io::Error),
	JOIN(tokio::task::JoinError),
	REQWEST(reqwest::Error),
}

impl From<std::io::Error> for ClientError {
	fn from(e: std::io::Error) -> ClientError {
		ClientError::IO(e)
	}
}

impl From<tokio::task::JoinError> for ClientError {
	fn from(e: tokio::task::JoinError) -> ClientError {
		ClientError::JOIN(e)
	}
}

impl From<reqwest::Error> for ClientError {
	fn from(e: reqwest::Error) -> ClientError {
		ClientError::REQWEST(e)
	}
}

macro_rules! activity_heartbeat {
	($client_session:expr) => {
		$client_session.heartbeat.fetch_add(1, Ordering::AcqRel)
	}
}

// we have to use a match block even though the API is the same until I figure out the dyn obj way to do this
macro_rules! write_buff {
	($writer:expr, $buff:expr) => {
		match $writer.deref_mut() {
			ClientWriter::Stream(s) => s.write_all($buff).await?,
			ClientWriter::Stdout(s) => s.write_all($buff).await?,
		}
	}
}

// stdout is buffered, OwnedWriteHalf is not. we have to flush the buffer otherwise some protocols (eg ssh)
// will deadlock
macro_rules! flush_buff {
	($writer:expr) => {
		match $writer.deref_mut() {
			ClientWriter::Stdout(s) => s.flush().await?,
			_ => {},
		};
	}
}

impl ClientSession {
	async fn get_id(base_url: String) -> Result<(reqwest::StatusCode, String), ClientError> {
		let mut open_url = base_url.clone();
		open_url.push_str("create");
		let response = reqwest::get(&open_url).await?;
		let status = response.status();
		let buff: String = response.text().await?;
		debug!("acquired session id from server {}", buff);
		Ok((status, buff))
	}

	async fn request_session (h: Arc<Handle>, base_url: String, addr: Option<SocketAddr>) -> Result<ClientOpStatus, ClientError> {
		let id_pattern = Regex::new("[0-9a-f]{32}").expect("initialising regex");
		let jh = h.spawn(ClientSession::get_id(base_url.clone()));
		let (status, id) = match jh.await? {
			Ok((status, id)) => (status, id),
			_ => {
				error!("error openning session");
				return Ok(ClientOpStatus::Error);
			}
		};
		if status != reqwest::StatusCode::OK {
			error!("server error opening session: {}", status.as_u16());
			return Err(ClientError::CLIENT(ClientOpStatus::Error));
		}
		if id.len() != ID_SIZE_ASCII_HEX || !id_pattern.is_match(id.as_str()) {
			error!("recieved malformed session id from server");
			return Err(ClientError::CLIENT(ClientOpStatus::Error));
		}
		match addr {
			Some(addr) => info!("established session {} for client {}", id, addr),
			None => info!("established session {} for client stdin", id),
		};
		Ok(ClientOpStatus::SessionId(id))
	}

	// future to poll the heartbeat field every second
	// this runs as long as the session and either terminates when the activity heartbeat is idle for the
	// configured timeout, or when the session is closed for another reason. in the case of idle timeout
	// it triggers a close before returning.
	async fn timeout_waiter(cs: Arc<ClientSession>) -> Result<ClientOpStatus, ClientError> {
		while cs.open.load(Ordering::Relaxed) {
			let mut idle_count: u32 = 0;
			let hb_before = cs.heartbeat.load(Ordering::Relaxed);
			while cs.open.load(Ordering::Relaxed) {
				sleep(ONE_SECOND).await;
				let hb_after = cs.heartbeat.load(Ordering::Relaxed);
				if hb_before == hb_after {
					idle_count += 1;
					if idle_count > cs.timeout {
						info!("inactivity timeout, closing session {}", cs.id);
						cs.rth.spawn(ClientSession::close(cs.clone()));
						return Ok(ClientOpStatus::Done);
					}
				}
				else {
					break;
				}
			}
		}
		debug!("timeout waiter for session {} terminating due to close", cs.id);
		Ok(ClientOpStatus::Done)
	}

	// thread safe close
	// this uses the "open" atomic field to cause all other futures to terminate, and to close the session
	// on the server side. the atomic is also used to prevent multiple close calls to the server side. in
	// some cases the server will have already closed the session returning a 404, but this is not an error
	// as it still represents a clean session end.
	async fn close(cs: Arc<ClientSession>) -> Result<ClientOpStatus, ClientError> {
		if !cs.open.swap(false, Ordering::SeqCst) {
			// we use the atomic swap to make sure every call after
			// the first one has no effect
			return Ok(ClientOpStatus::OK);
		}
		debug!("session close initiated {}", cs.id);
		let cs = cs.clone();
		let mut close_url = cs.base_url.clone();
		close_url.push_str("close");
		let resp = cs.client
			.delete(&close_url)
			.header("X-Layline-Session", cs.id.clone())
			.send()
			.await?;
		debug!("server responds to close with {}", resp.status().as_u16());
		info!("closed session {}", cs.id);
		Ok(ClientOpStatus::Done)
	}

	// long running future to copy socket reads to POST requests against the server
	async fn copy_sockreader_to_send(cs: Arc<ClientSession>) -> Result<ClientOpStatus, ClientError> {
		let cs = cs.clone();
		let mut write_url = cs.base_url.clone();
		write_url.push_str("send");
		let write_url = &*write_url;  // convert URL to immutable
		let mut reader = cs.reader.lock().await;  // we are the only user, lock indefinitely
		while cs.open.load(Ordering::Relaxed) {  // loop as long as the "open" atomic is true
			let mut buff = Vec::<u8>::with_capacity(BODY_SIZE_LIMIT);
			let ret = match reader.deref_mut() {
				ClientReader::Stream(s) => timeout(ONE_SECOND, s.read_buf(&mut buff)).await,
				ClientReader::Stdin(s) => timeout(ONE_SECOND, s.read_buf(&mut buff)).await,
			};
			let ret = match ret {
				Ok(r) => r,
				Err(_) => continue,  // read timeout is not an error, check open status again on iteration
			};
			let read_len = match ret {
				Ok(r) => r,
				Err(_) => {
					error!("socket read I/O error");
					return Err(ClientError::CLIENT(ClientOpStatus::Error));
				}
			};
			if read_len == 0 {  // socket is closed, tear down local and server side resources
				info!("socket closed for session {}", cs.id);
				cs.rth.spawn(ClientSession::close(cs.clone()));
				return Ok(ClientOpStatus::Done);
			}
			activity_heartbeat!(cs);
			let resp = cs.client
				.post(write_url)
				.header("X-Layline-Session", cs.id.clone())
				.body(buff)
				.send()
				.await?;
			match resp.status().as_u16() {
				200 => (),
				404 => {
					info!("server reports session {} no longer exists", cs.id);
					cs.rth.spawn(ClientSession::close(cs.clone()));
					return Ok(ClientOpStatus::Done);
				}
				e => {
					error!("server reports error {} for session {}", e, cs.id);
					cs.rth.spawn(ClientSession::close(cs.clone()));
					return Err(ClientError::CLIENT(ClientOpStatus::Error));
				}
			}
		}
		debug!("shutting down reader due to closed");
		Ok(ClientOpStatus::Done)
	}

	// long running future to copy GET requests from the server to the session's socket
	async fn copy_reads_to_sockwriter(cs: Arc<ClientSession>) -> Result<ClientOpStatus, ClientError> {
		let cs = cs.clone();
		let mut read_url = cs.base_url.clone();
		read_url.push_str("recv");
		let read_url = &*read_url;  // convert URL to immutable
		let mut writer = cs.writer.lock().await;  // we are the only user, lock indefintely
		while cs.open.load(Ordering::Relaxed) {  // loop as long as the "open" atomic is true
			let mut response: reqwest::Response = cs.client
				.get(read_url)
				.header("X-Layline-Session", cs.id.clone())
				.send()
				.await?;
			match response.status().as_u16() {
				200 => (),
				404 => {
					info!("server reports session {} no longer exists", cs.id);
					cs.rth.spawn(ClientSession::close(cs.clone()));
					return Ok(ClientOpStatus::Done);
				}
				e => {
					error!("server reports error {} for session {}", e, cs.id);
					cs.rth.spawn(ClientSession::close(cs.clone()));
					return Err(ClientError::CLIENT(ClientOpStatus::Error));
				}
			}
			let mut did_write = false;
			loop {
				let buff = response.chunk().await?;
				match (did_write, buff) {
					(_, Some(buff)) => {
						write_buff!(writer, &buff);
						did_write = true;
					}
					(true, None) => {
						flush_buff!(writer);  // ssh deadlocks if we don't flush stdout
						activity_heartbeat!(cs);
						break;
					}
					(false, None) => {
						break;  // no writes happened, nothing to do
					}
				}
			}
		}
		debug!("shutting down writer due to closed");
		Ok(ClientOpStatus::Done)
	}

	// request session allocation from the server, allocate session state in the local process, and spawn
	// futures to process io for the session as well as housekeeping such as signal handling and timeouts
	async fn initialize_session(h: Handle, base_url: String, reader: ClientReader, writer: ClientWriter,
								addr: Option<SocketAddr>, opts: (usize, u32, bool, bool, String)) -> Result<ClientOpStatus, ClientError> {
		let h = Arc::new(h);
		let id = match h.spawn(ClientSession::request_session(h.clone(), base_url.clone(), addr)).await? {
			Ok(ClientOpStatus::SessionId(id)) => id,
			Ok(status) => return Err(ClientError::CLIENT(status)),
			Err(e) => return Err(e),
		};
		debug!("initialization session for {}", id);
		let client = reqwest::Client::builder()
			.timeout(HTTP_TIMEOUT)
			.build()
			.expect("building client");
		let cs = Arc::new(ClientSession {
			id: id.clone(),
			base_url: base_url.clone(),
			reader: Mutex::new(reader),
			writer: Mutex::new(writer),
			heartbeat: AtomicU64::from(0),
			open: AtomicBool::new(true),
			rth: h.clone(),
			client: client,
			timeout: opts.1,
		});
		debug!("initializing signal handler for {}", id);
		h.spawn(ClientSession::signal_handler(cs.clone()));
		debug!("initializing writer for {}", id);
		h.spawn(ClientSession::copy_reads_to_sockwriter(cs.clone()));
		debug!("initializing reader for {}", id);
		h.spawn(ClientSession::copy_sockreader_to_send(cs.clone()));
		debug!("initializing timeout handler {}", id);
		let check_timeout = h.spawn(ClientSession::timeout_waiter(cs.clone()));
		match check_timeout.await? {
			Ok(_) => {},
			e => return e,
		};
		debug!("client futures complete for session {}", id);
		Ok(ClientOpStatus::Done)
	}

	// waits indefinitely for a SIGHUP, after which it will close the session
	// this was added because when this utility is used with the ProxyCommand option in OpenSSH, the only
	// way to detect when the session is terminated is a SIGHUP
	async fn signal_handler(cs: Arc<ClientSession>) -> Result<(), ClientError> {
		let mut stream = match signal(SignalKind::hangup()) {
			Ok(s) => s,
			_ => return Err(ClientError::CLIENT(ClientOpStatus::Error)),
		};
		stream.recv().await;
		debug!("recieved SIGHUP, initiating shutdown");
		ClientSession::close(cs).await?;
		Ok(())
	}
}

// listener acceptor for handling incoming connections
async fn client_listen(h: Handle, listen_port: String, base_url: String, opts: (usize, u32, bool, bool, String)) -> Result<ClientOpStatus, tokio::io::Error> {
	let listener = TcpListener::bind(listen_port).await?;
	loop {
		let (sock, addr) = listener.accept().await?;
		let (reader, writer) = sock.into_split();
		let reader = ClientReader::Stream(reader);
		let writer = ClientWriter::Stream(writer);
        let newopts = (opts.0, opts.1, opts.2, opts.3, opts.4.clone());
		h.spawn(ClientSession::initialize_session(h.clone(), base_url.clone(), reader, writer, Some(addr), newopts));
	}
}

fn validate_url(base_url: &str, opts: (usize, u32, bool, bool, String)) {
	if !base_url.starts_with("https://") && !opts.2 {
		panic!("URL must start with \"https://\" unless --allow-plaintext is set");
	}
	if opts.2 && !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
		panic!("URL must start with \"https://\" or \"https://\"")
	}
}

// synchronous initialization function for running as a standing listener that creates
// sessions for each incoming connection
pub fn run(listen_port: &str, base_url: &str, opts: (usize, u32, bool, bool, String)) -> i32 {
	match Logger::try_with_env_or_str("layline=info, client=info") {
		Ok(l) => l.format(opt_format).start().unwrap(),
		Err(e) => panic!("Logger initialization failed with {}", e),
	};
	validate_url(&base_url, (opts.0, opts.1, opts.2, opts.3, opts.4.clone()));
	let rt = tokio::runtime::Runtime::new().unwrap();
	let h = rt.handle();
	let ret = match h.block_on(client_listen(h.clone(), String::from(listen_port), String::from(base_url), opts)) {
		Ok(ClientOpStatus::Done) => 0,
		_ => -1,
	};
	rt.shutdown_timeout(Duration::from_secs(1));
	ret
}

// asynchronous entry point for running as an stdin/stdout proxy
async fn do_proxy(h: Handle, base_url: String, opts: (usize, u32, bool, bool, String)) -> Result<ClientOpStatus, tokio::io::Error> {
	let stdin = ClientReader::Stdin(tokio::io::stdin());
	let stdout = ClientWriter::Stdout(tokio::io::stdout());
	let ret = match h.spawn(ClientSession::initialize_session(h.clone(), base_url.clone(), stdin, stdout, None, opts)).await {
		Ok(_) => Ok(ClientOpStatus::Done),
		_ => Ok(ClientOpStatus::Error),
	};
	ret
}

// synchronous initialization function for running as an stdin/stdout proxy
pub fn proxy_run(base_url: &str, opts: (usize, u32, bool, bool, String)) -> i32 {
	match Logger::try_with_env_or_str("layline=info, client=info") {
		Ok(l) => l.format(opt_format).start().unwrap(),
		Err(e) => panic!("Logger initialization failed with {}", e),
	};
	validate_url(&base_url, (opts.0, opts.1, opts.2, opts.3, opts.4.clone()));
	let rt = tokio::runtime::Runtime::new().unwrap();
	let h = rt.handle();
	let ret = match h.block_on(do_proxy(h.clone(), String::from(base_url), (opts.0, opts.1, opts.2, opts.3, opts.4.clone()))) {
		Ok(ClientOpStatus::Done) => 0,
		_ => -1,
	};
	debug!("returning from proxy_run");
	ret
}
