use clap::{App, Arg};

mod constants;

mod client;
mod server;

fn main() {
	let matches = App::new("layline")
		.version("0.1")
		.author("Anthony Roberts <acrobert@gmail.com>")
		.about("tunnel connections via HTTP")
		.arg(Arg::with_name("SESSION_TIMEOUT")
			.about("inactivity timeout for sessions in seconds, default=900")
			.long("session-timeout")
			.takes_value(true))
		.arg(Arg::with_name("MAX_SESSIONS")
			.about("maximum simultaneous sessions allowed, default=100")
			.long("max-sessions")
			.takes_value(true))
		.arg(Arg::with_name("ALLOW_PLAINTEXT")
			.about("allow outbound connections to use plaintext in client modes")
			.long("allow-plaintext"))
		.subcommand(App::new("server")
			.about("run as a server")
			.arg(Arg::with_name("LISTEN_PORT")
				.about("ip:port to listen on for tunneled connections")
				.required(true)
				.index(1))
			.arg(Arg::with_name("DEST_PORT")
				.about("ip:port to connect to for tunneled connections")
				.required(true)
				.index(2))
			.arg(Arg::with_name("LOG_DEST")
				.long("log-dest")
				.about("a directory to log to, if unspecified stderr will be used")
				.takes_value(true)))
		.subcommand(App::new("client")
			.about("run as a client, with a persistent process to forward connections")
			.arg(Arg::with_name("LISTEN_PORT")
				.about("ip:port to listen on for tunneled connections")
				.required(true)
				.index(1))
			.arg(Arg::with_name("DEST_URL")
				.about("URL prefix for the HTTP path that exposes the layline API")
				.required(true)
				.index(2)))
		.subcommand(App::new("proxyclient")
			.about("run as a client, automatically establishing a session and using stdin/stdout")
			.arg(Arg::with_name("DEST_URL")
				.about("URL prefix for the HTTP path that exposes the layline API")
				.required(true)
				.index(1)))
		.get_matches();
	let max_sessions: usize = match matches.value_of("MAX_SESSIONS") {
		Some(s) => s.parse::<usize>().unwrap(),
		None => constants::DEFAULT_MAX_SESSIONS,
	};
	let timeout_sessions: u32 = match matches.value_of("SESSION_TIMEOUT") {
		Some(s) => s.parse().unwrap(),
		None => constants::DEFAULT_SESSION_TIMEOUT,
	};
	let allow_plaintext = matches.is_present("ALLOW_PLAINTEXT");
	let opts = (max_sessions, timeout_sessions, allow_plaintext);
	match matches.subcommand() {
		("proxyclient", Some(matches)) => {
			let dest_port = matches.value_of("DEST_URL").unwrap();
			let ret = client::proxy_run(dest_port, opts);
			std::process::exit(ret);
		}
		("client", Some(matches)) => {
			let listen_port = matches.value_of("LISTEN_PORT").unwrap();
			let dest_port = matches.value_of("DEST_URL").unwrap();
			let ret = client::run(listen_port, dest_port, opts);
			std::process::exit(ret);
		}
		("server", Some(matches)) => {
			let listen_port = matches.value_of("LISTEN_PORT").unwrap();
			let dest_port = matches.value_of("DEST_PORT").unwrap();
			let log_dest = match matches.value_of("LOG_DEST") {
				Some(d) => d,
				None => "stderr",
			};
			let ret = server::run(listen_port, dest_port, log_dest, opts);
			std::process::exit(ret);
		}
		_ => {
			eprintln!("no or unknown options specified");
			std::process::exit(-1);
		}
	};
}