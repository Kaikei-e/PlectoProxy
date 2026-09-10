//! Test-only outbound-TCP guest which retains native socket resources across hook calls.
//! It proves that the host, rather than guest cooperation, owns the descriptor lifetime limit.

wit_bindgen::generate!({
    path: "../../../../wit/v0.3.0",
    world: "filter",
});

use std::cell::RefCell;

use wasi::sockets::network::IpAddressFamily;
use wasi::sockets::tcp::TcpSocket;
use wasi::sockets::tcp_create_socket;

thread_local! {
    static HELD: RefCell<Vec<TcpSocket>> = const { RefCell::new(Vec::new()) };
}

struct FilterTcpRetain;

fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
}

fn response(status: u16, body: &[u8]) -> RequestDecision {
    RequestDecision::ShortCircuit(HttpResponse {
        status,
        headers: Vec::new(),
        body: body.to_vec(),
    })
}

impl Guest for FilterTcpRetain {
    fn init() {}

    fn on_request(req: HttpRequest) -> RequestDecision {
        match header(&req, "x-tcp-retain") {
            Some("clear") => {
                HELD.with(|held| held.borrow_mut().clear());
                RequestDecision::Continue
            }
            Some("hold") => match tcp_create_socket::create_tcp_socket(IpAddressFamily::Ipv4) {
                Ok(socket) => {
                    HELD.with(|held| held.borrow_mut().push(socket));
                    RequestDecision::Continue
                }
                Err(error) => response(503, format!("socket unavailable: {error:?}").as_bytes()),
            },
            _ => response(400, b"missing x-tcp-retain"),
        }
    }

    fn on_response(_req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterTcpRetain);
