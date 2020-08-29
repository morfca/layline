use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use flexi_logger::{Logger, opt_format};
use flexi_logger::{Cleanup, Criterion, Naming};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use log::{debug, error, info, warn};
use rand_core::RngCore;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::prelude::*;
use tokio::stream::StreamExt;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{delay_for, timeout};

use crate::constants::*;

struct ServerState {
	sessions: Arc<RwLock<HashMap<String, Arc<Session>>>>,
	dest: SocketAddr,
	max_sessions: usize,
	session_timeout: u32,
}

impl ServerState {
	fn new(dest: SocketAddr, opts: (usize, u32)) -> ServerState {
		ServerState {
			sessions: Arc::new(RwLock::new(HashMap::new())),
			dest: dest,
			max_sessions: opts.0,
			session_timeout: opts.1,
		}
	}
}

struct Session {
	id: String,
	reader: Mutex<OwnedReadHalf>,
	writer: Mutex<OwnedWriteHalf>,
	open: AtomicBool,
	heartbeat: AtomicU64,
}

impl Session {
	async fn new(server_state: Arc<ServerState>) -> Result<Session, ServerError> {
		let mut newid: [u8; ID_SIZE_BINARY] = [0; ID_SIZE_BINARY];
		rand::thread_rng().fill_bytes(&mut newid);
		let newid = newid.to_vec().iter().map(|i| format!("{:02x}", i)).collect::<String>();
		let sock = TcpStream::connect(server_state.dest).await?;
		let (reader, writer) = sock.into_split();
		Ok(Session {
			id: newid,
			reader: Mutex::new(reader),
			writer: Mutex::new(writer),
			open: AtomicBool::from(true),
			heartbeat: AtomicU64::from(0),
		})
	}

	async fn shutdown(session: Arc<Session>, server_state: Arc<ServerState>) -> Result<(), ServerError> {
		if !session.open.swap(false, Ordering::SeqCst) {
			// we use the atomic swap to make sure every call after
			// the first one has no effect
			return Ok(());
		}
		let mut sessions = server_state.sessions.write().await;
		sessions.remove(&session.id);
		info!("shutdown session {}", session.id);
		Ok(())
	}
}

enum OpStatus {
	Done,
}

// tokio requires error types in futures, this enum is to allow futures with multiple possible error types
// below we provide impl's for ServerError to allow automatic wrapping and formatting for logging
enum ServerError {
	IO(std::io::Error),
	JOIN(tokio::task::JoinError),
	HYPER(hyper::error::Error),
}

impl From<std::io::Error> for ServerError {
	fn from(e: std::io::Error) -> ServerError {
		ServerError::IO(e)
	}
}

impl From<tokio::task::JoinError> for ServerError {
	fn from(e: tokio::task::JoinError) -> ServerError {
		ServerError::JOIN(e)
	}
}

impl From<hyper::error::Error> for ServerError {
	fn from(e: hyper::error::Error) -> ServerError {
		ServerError::HYPER(e)
	}
}

impl std::fmt::Display for ServerError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ServerError::IO(e) => e.fmt(f),
			ServerError::JOIN(e) => e.fmt(f),
			ServerError::HYPER(e) => e.fmt(f),
		}
	}
}

impl Drop for Session {
    fn drop(&mut self) {
        info!("cleanup complete for {}", self.id);
    }
}

// due to D.R.Y. we create all responses here
// since we want to run behind CDN's etc we can't afford to forget Cache-Control headers etc
macro_rules! make_response {
	($code:expr) => {
		make_response!($code, Body::empty())
	};
	($code:expr, $body:expr) => {
		Response::builder()
			.status($code)
			.header("Cache-Control", "no-cache")
			.body($body)
			.unwrap()
	};
	($code:expr, $body:expr, $( $header:expr ),*) => {
		Response::builder()
			.status($code)
			.header("Cache-Control", "no-cache")
			$(
				.header($header.0, $header.1)
			)*
			.body($body)
			.unwrap()
	};
}

// we need to use a macro rather than a function to run the response future because .await
// wants to be able to return. consolidating error handling is also convenient.
macro_rules! do_response {
	($resp_fn:expr) => {
		match tokio::spawn($resp_fn).await {
			Ok(r) => {
				match r {
					Ok(resp) => {
						return Ok(resp);
					}
					Err(_) => {
						warn!("problem with do_create, sending 500");
						return Ok(make_response!(StatusCode::INTERNAL_SERVER_ERROR));
					}
				}
			}
			Err(_) => {
				warn!("problem joining handler function, sending 500");
				return Ok(make_response!(StatusCode::INTERNAL_SERVER_ERROR));
			}
		};
	}
}

// make sure we don't have panics later due to missing the session header
macro_rules! check_session_header {
	($request:expr) => {		
		if !$request.headers().contains_key("x-layline-session") {
			return Ok(make_response!(404));
		}
	}
}

macro_rules! get_session_id {
	($request:expr) => {
		String::from($request.headers()["x-layline-session"].to_str().expect("session identifier"))
	}
}

// retrieve the session given the session id
// clients are expected to 404 sometimes as there is no reliable way to guarantee which side will close the
// session first due to timeouts, downstream disconencts, etc
macro_rules! get_session {
	($req_id:expr, $server_state:expr) => {
		match get_session_from_req(&$req_id, $server_state).await {
			Ok(s) => match s {
				Some(s) => s,
				None => {
					return Ok(make_response!(404));
				}
			}
			Err(_) => {
				error!("error looking up session");
				return Ok(make_response!(500));
			}
		}
	}
}

fn get_xff_ip<T>(req: Request<T>) -> Option<String> {
	if !req.headers().contains_key("x-forwarded-for") {
		return None;
	}
	let xff = match req.headers()["x-forwarded-for"].to_str() {
		Ok(xff) => xff,
		_ => return None,
	};
	Some(String::from(xff))
}

async fn timeout_watchdog(session: Arc<Session>, server_state: Arc<ServerState>) -> Result<(), ServerError> {
	while session.open.load(Ordering::Relaxed) {
		debug!("timeout heartbeat reset for session {}", session.id);
		let mut idle_count: u32 = 0;
		let hb_before = session.heartbeat.load(Ordering::Relaxed);
		while session.open.load(Ordering::Relaxed) {
			delay_for(ONE_SECOND).await;
			let hb_after = session.heartbeat.load(Ordering::Relaxed);
			if hb_before == hb_after {
				debug!("session {} idle_count {}", session.id, idle_count);
				idle_count += 1;
				if idle_count > server_state.session_timeout {
					info!("session timeout {}", session.id);
					tokio::spawn(Session::shutdown(session.clone(), server_state.clone()));
					return Ok(());
				}
			}
			else {
				debug!("session {} reset idle count", session.id);
				break;
			}
		}
	}
	return Ok(());
}

async fn get_session_from_req(req_id: &String, server_state: &Arc<ServerState>) -> Result<Option<Arc<Session>>, ServerError> {
	let sessions = server_state.sessions.clone();
	let sessions = sessions.read().await;
	match sessions.get(req_id.as_str()) {
		Some(s) => Ok(Some(s.clone())),
		None => Ok(None),
	}
}

async fn do_send(req: Request<Body>, server_state: Arc<ServerState>) -> Result<Response<Body>, ServerError> {
	check_session_header!(req);
	let req_id = get_session_id!(req);
	let session = get_session!(req_id, &server_state);
	let mut locked_writer = session.writer.lock().await;
	let mut body = req.into_body();
	let mut nonempty = false;
	loop {
		let chunk = match body.next().await {
			Some(c) => {
				nonempty = true;
				c
			}
			None => break,
		};
		let buff = match chunk {
			Ok(b) => b,
			Err(_) => {
				error!("error reading post body");
				tokio::spawn(Session::shutdown(session.clone(), server_state.clone()));
				return Ok(make_response!(500));
			}
		};
		match locked_writer.write_all(&buff).await {
			Ok(_) => {},
			Err(_) => {
				error!("error sending to socket");
				tokio::spawn(Session::shutdown(session.clone(), server_state.clone()));
				return Ok(make_response!(500));
			}
		};
	}
	if nonempty {
		session.heartbeat.fetch_add(1, Ordering::AcqRel);
	}
	Ok(make_response!(200))
}

async fn do_recv(req: Request<Body>, server_state: Arc<ServerState>) -> Result<Response<Body>, ServerError> {
	check_session_header!(req);
	let req_id = get_session_id!(req);
	let session = get_session!(req_id, &server_state);
	let mut buff = Vec::<u8>::with_capacity(BODY_SIZE_LIMIT);
	let mut locked_reader = session.reader.lock().await;
	let res = timeout(LONG_POLL_TIMEOUT, locked_reader.read_buf(&mut buff)).await;
	let inner_res = match res {
		Ok(r) => r,
		Err(_) => {
			// timeout on socket read, send an empty 200 for long poll
			return Ok(make_response!(200));
		}
	};
	let read_len = match inner_res {
		Ok(n) => n,
		Err(_) => {
			error!("error reading from socket");
			tokio::spawn(Session::shutdown(session.clone(), server_state.clone()));
			return Ok(make_response!(500));
		}
	};
	if read_len == 0 {
		tokio::spawn(Session::shutdown(session.clone(), server_state.clone()));
	}
	session.heartbeat.fetch_add(1, Ordering::AcqRel);
	Ok(make_response!(200,  Body::from(buff)))
}

async fn do_close(req: Request<Body>, server_state: Arc<ServerState>) -> Result<Response<Body>, ServerError> {
	check_session_header!(req);
	let req_id = get_session_id!(req);
	let session = get_session!(req_id, &server_state);
	tokio::spawn(Session::shutdown(session, server_state));
	Ok(make_response!(200))
}

async fn do_create(req: Request<Body>, client_addr: SocketAddr, server_state: Arc<ServerState>) -> Result<Response<Body>, ServerError> {
	let temp_state = server_state.clone();
	{
		let sessions_count = server_state.sessions.read().await.len();
		debug!("create called, sessions count {}", sessions_count);
		if sessions_count > server_state.max_sessions as usize {
			error!("maximum sessions reached");
			return Ok(make_response!(500));
		};
	}
	let session: Session = match tokio::spawn(Session::new(temp_state)).await {
		Ok(s) => match s {
			Ok(ss) => ss,
			Err(e) => {
				error!("encountered error with setup when creating new session: {}", e);
				return Ok(make_response!(500));
			}
		}
		Err(_) => {
			error!("encountered error with join handle when creating new session");
			return Ok(make_response!(500));
		}
	};
	let session = Arc::new(session);
	tokio::spawn(timeout_watchdog(session.clone(), server_state.clone()));
	let id = session.id.clone();
	{
		let sessions = server_state.sessions.clone();
		let mut wsessions = sessions.write().await;
		wsessions.insert(id.clone(), session);
	}
	match get_xff_ip(req) {
		Some(xff) => info!("created session with id {} for {} with X-Forwarded-For: \"{}\"", id, client_addr, xff),
		None => info!("created session with id {} for {}", id, client_addr),
	};
	Ok(make_response!(200, Body::from(id)))
}

async fn router(req: Request<Body>, client_addr: SocketAddr, server_state: Arc<ServerState>) -> Result<Response<Body>, hyper::Error> {
	let uri = req.uri().to_string();
	match (uri.as_str(), req.method().as_str()) {
		("/create", "GET") => {
			do_response!(do_create(req, client_addr, server_state.clone()));
		}
		("/recv", "GET") => {
			do_response!(do_recv(req, server_state.clone()));
		}
		("/send", "POST") => {
			do_response!(do_send(req, server_state.clone()));
		}
		("/close", "DELETE") => {
			do_response!(do_close(req, server_state.clone()));
		}
		_ => {
			return Ok(make_response!(StatusCode::NOT_FOUND));
		}
	};
}

#[tokio::main]
async fn listen(listen_port: SocketAddr, server_state: Arc<ServerState>) -> Result<OpStatus, ServerError> {
	use hyper::server::conn::AddrStream;
	let server_state = server_state.clone();
	let service = make_service_fn(move |socket: &AddrStream| {
		let client_addr = socket.remote_addr();
		let temp_state = server_state.clone();
		async move {
			Ok::<_, hyper::Error>(service_fn(move |req| { router(req, client_addr, temp_state.clone())}))
		}
	});
	Server::bind(&listen_port).serve(service).await?;
	Ok(OpStatus::Done)
}

pub fn run(listen_port: &str, dest_port: &str, log_path: &str, opts: (usize, u32)) -> i32 {
	if log_path == "stderr" {
		Logger::with_env_or_str("layline=info, server=info")
			.format(opt_format)
			.start()
			.unwrap_or_else(|e| panic!("Logger initialization failed with {}", e));
	}
	else {
		Logger::with_env_or_str("layline=info, server=info")
			.log_to_file()
			.directory("/var/log/layline/")
			.rotate(Criterion::Size(LOG_SIZE as u64), Naming::Numbers, Cleanup::KeepLogFiles(5))
			.format(opt_format)
			.start()
			.unwrap_or_else(|e| panic!("Logger initialization failed with {}", e));
	}
	let listen_port: SocketAddr = listen_port.parse().expect("ip:port for webserver");
	let dest_port: SocketAddr = dest_port.parse().expect("destination ip:port");
	let server_state: Arc<ServerState> = Arc::new(ServerState::new(dest_port, opts));
	match listen(listen_port, server_state) {
		Ok(_) => 0,
		_ => {
			error!("listen returned with error");
			-1
		}
	}
}
