//! HTTP client

use failure::format_err;
use futures::{
	future::{self, Either::{A, B}},
	sync::mpsc,
	Future,
	Stream
};
use hyper::{http, rt, Client, Request};
use jsonrpc_core::{self, Call, Error, Id, MethodCall, Output, Params, Response, Version};

use crate::{RpcChannel, RpcError, RpcMessage};
use super::request_response;
use futures::sink::Sink;

/// Create a HTTP Client
pub fn http<TClient>(url: &str) -> impl Future<Item=TClient, Error=RpcError>
where
	TClient: From<RpcChannel>,
{
	let max_parallel = 8;
	let url = url.to_owned();
	let client = Client::new();

	let (sender, receiver) = mpsc::channel(0);

	let fut = receiver
		.map(move |msg: RpcMessage| {
			let request = jsonrpc_core::Request::Single(Call::MethodCall(MethodCall {
				jsonrpc: Some(Version::V2),
				method: msg.method.clone(),
				params: msg.params.clone(),
				id: Id::Num(1), // todo: [AJ] assign num
			}));
			let request_str = serde_json::to_string(&request).expect("Infallible serialization");

			let request = Request::post(&url)
				.header(http::header::CONTENT_TYPE, http::header::HeaderValue::from_static("application/json"))
				.body(request_str.into())
				.unwrap();

			client
				.request(request)
				.then(move |response| Ok((response, msg)))
		})
		.buffer_unordered(max_parallel)
		.for_each(|(result, msg)| {
			let future = match result {
				Ok(ref res) if !res.status().is_success() => {
					log::trace!("http result status {}", res.status());
					A(future::err(
						RpcError::Other(format_err!("Unexpected response status code: {}", res.status()))
					))
				},
				Ok(res) => B(
					res.into_body()
						.map_err(|e| RpcError::ParseError(e.to_string(), e.into()))
						.concat2()
				),
				Err(err) => A(future::err(RpcError::Other(err.into()))),
			};
			future.then(|result| {
				let result = result.and_then(|response| {
					let response_str = String::from_utf8_lossy(response.as_ref()).into_owned();
					serde_json::from_str::<Response>(&response_str)
						.map_err(|e| RpcError::ParseError(e.to_string(), e.into()))
						.and_then(|response| {
							let output: Output = match response {
								Response::Single(output) => output,
								Response::Batch(_) => unreachable!(),
							};
							let value: Result<serde_json::Value, Error> = output.into();
							value.map_err(|e| RpcError::JsonRpcError(e))
						})
					});

				if let Err(err) = msg.sender.send(result) {
					log::warn!("Error resuming asynchronous request: {:?}", err);
				}
				Ok(())
			})
		});

	rt::lazy(move|| {
		rt::spawn(fut.map_err(|e| log::error!("RPC Client error: {:?}", e)));
		Ok(TClient::from(sender))
	})
}

#[cfg(test)]
mod tests {
	use std::time::Duration;
	use std::net::SocketAddr;
	use jsonrpc_core::{IoHandler, Params, Error, ErrorCode, Value};
	use jsonrpc_http_server::*;
	use hyper::rt;
	use super::*;
	use crate::*;

	fn id<T>(t: T) -> T {
		t
	}

	struct TestServer {
		uri: String,
		socket_addr: SocketAddr,
		server: Option<Server>,
	}

	impl TestServer {
		fn serve<F: FnOnce(ServerBuilder) -> ServerBuilder>(alter: F) -> Self {
			let builder = ServerBuilder::new(io())
				.rest_api(RestApi::Unsecure);

			let server = alter(builder).start_http(&"127.0.0.1:0".parse().unwrap()).unwrap();
			let socket_addr = server.address().clone();
			let uri = format!("http://{}", socket_addr);

			TestServer {
				uri,
				socket_addr,
				server: Some(server)
			}
		}

		fn start(&mut self) {
			if self.server.is_none() {
				let server = ServerBuilder::new(io())
					.rest_api(RestApi::Unsecure)
					.start_http(&self.socket_addr)
					.unwrap();
				self.server = Some(server);
			} else {
				panic!("Server already running")
			}
		}

		fn stop(&mut self) {
			let server = self.server.take();
			if let Some(server) = server {
				server.close();
			}
		}
	}

	fn io() -> IoHandler {
		let mut io = IoHandler::default();
		io.add_method("hello", |params: Params| match params.parse::<(String,)>() {
			Ok((msg,)) => Ok(Value::String(format!("hello {}", msg))),
			_ => Ok(Value::String("world".into())),
		});
		io.add_method("fail", |_: Params| Err(Error::new(ErrorCode::ServerError(-34))));

		io
	}

	#[derive(Clone)]
	struct TestClient(TypedClient);

	impl From<RpcChannel> for TestClient {
		fn from(channel: RpcChannel) -> Self {
			TestClient(channel.into())
		}
	}

	impl TestClient {
		fn hello(&self, msg: &'static str) -> impl Future<Item=String, Error=RpcError> {
			self.0.call_method("hello", "String", (msg,))
		}
		fn fail(&self) -> impl Future<Item=(), Error=RpcError> {
			self.0.call_method("fail", "()", ())
		}
	}

	#[test]
	fn should_work() {
		crate::logger::init_log();

		// given
		let server = TestServer::serve(id);
		let (tx, rx) = std::sync::mpsc::channel();

		// when
		let run =
			http(&server.uri)
				.and_then(|client: TestClient| {
					client.hello("http")
						.and_then(move |result| {
							drop(client);
							let _ = tx.send(result);
							Ok(())
						})
				})
				.map_err(|e| log::error!("RPC Client error: {:?}", e));

		rt::run(run);

		// then
		let result = rx.recv_timeout(Duration::from_secs(3)).unwrap();
		assert_eq!("hello http", result);
	}

	#[test]
	fn handles_server_error() {
		crate::logger::init_log();

		// given
		let server = TestServer::serve(id);
		let (tx, rx) = std::sync::mpsc::channel();

		// when
		let run =
			http(&server.uri)
				.and_then(|client: TestClient| {
					client
						.fail()
						.then(move |res| {
							let _ = tx.send(res);
							Ok(())
						})
				})
				.map_err(|e| log::error!("RPC Client error: {:?}", e));
		rt::run(run);

		// then
		let res = rx.recv_timeout(Duration::from_secs(3)).unwrap();

		if let Err(RpcError::JsonRpcError(err)) = res {
			assert_eq!(err, Error { code: ErrorCode::ServerError(-34), message: "Server error".into(), data: None })
		} else {
			panic!("Expected JsonRpcError. Received {:?}", res)
		}
	}

	#[test]
	fn handles_connection_refused_error() {
		// given
		let mut server = TestServer::serve(id);
		// stop server so that we get a connection refused
		server.stop();
		let (tx, rx) = std::sync::mpsc::channel();

		let client = http(&server.uri);

		let call = client
			.and_then(|client: TestClient| {
				client
					.hello("http")
					.then(move |res| {
						let _ = tx.send(res);
						Ok(())
					})
			})
			.map_err(|e| log::error!("RPC Client error: {:?}", e));

		rt::run(call);

		// then
		let res = rx.recv_timeout(Duration::from_secs(3)).unwrap();

		if let Err(RpcError::Other(err)) = res {
			if let Some(err) = err.downcast_ref::<hyper::Error>() {
				assert!(err.is_connect())
			} else {
				panic!("Expected a hyper::Error")
			}
		} else {
			panic!("Expected JsonRpcError. Received {:?}", res)
		}
	}

	#[test]
	#[ignore] // todo: [AJ] make it pass
	fn client_still_works_after_http_connect_error() {
		// given
		let mut server = TestServer::serve(id);

		// stop server so that we get a connection refused
		server.stop();

		let (tx, rx) = std::sync::mpsc::channel();
		let tx2 = tx.clone();

		let client = http(&server.uri);

		let call = client
			.and_then(move |client: TestClient| {
				client
					.hello("http")
					.then(move |res| {
						let _ = tx.send(res);
						Ok(())
					})
					.and_then(move |_| {
						server.start(); // todo: make the server start on the main thread
						client
							.hello("http2")
							.then(move |res| {
								let _ = tx2.send(res);
								Ok(())
							})
					})
			})
			.map_err(|e| log::error!("RPC Client error: {:?}", e));

		// when
		rt::run(call);

		let res = rx.recv_timeout(Duration::from_secs(3)).unwrap();
		assert!(res.is_err());

		// then
		let result = rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
		assert_eq!("hello http", result);
	}
}
