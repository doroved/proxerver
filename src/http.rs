use crate::{
    options::Opt,
    utils::{
        formatted_time, get_current_server_ip, get_rand_ipv4_socket_addr, is_allowed_credentials,
        is_host_allowed, require_basic_auth, to_sha256,
    },
};
use clap::Parser;
use hyper::{
    client::HttpConnector,
    header::PROXY_AUTHORIZATION,
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Body, Client, Method, Request, Response, Server, StatusCode,
};
use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpSocket,
};

#[derive(Debug, Clone)]
pub(crate) struct Proxy {
    pub allowed_credentials: Arc<Mutex<Vec<String>>>,
    pub allowed_hosts: Arc<Mutex<Vec<String>>>,
    pub secret_token: Arc<Mutex<String>>,
}

impl Proxy {
    pub(crate) async fn proxy(self, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
        println!("Method: {:?}", req.method());
        println!("URI: {:?}", req.uri());
        println!("Version: {:?}", req.version());
        println!("Headers: {:?}", req.headers());
        println!("Body: {:?}", req.body());

        let options = Opt::parse();

        // Check request for inclusion in the white list of hosts that can be proxied
        let host = req.uri().host().unwrap_or("");
        let allowed_hosts = self.allowed_hosts.lock().unwrap().to_vec();
        if !allowed_hosts.is_empty() && !is_host_allowed(host, &allowed_hosts) {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap());
        }

        // If secret token is not empty and no_http_token is false, check if the secret token is valid
        let secret_token = self.secret_token.lock().unwrap().to_string();
        if !secret_token.is_empty() && !options.no_http_token {
            if let Some(secret_token_header) = req.headers().get("x-http-secret-token") {
                if secret_token_header.to_str().unwrap_or_default().trim()
                    != to_sha256(secret_token.trim())
                {
                    return Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::empty())
                        .unwrap());
                }
            } else if req.headers().get("x-https-secret-token").is_none() {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::empty())
                    .unwrap());
            }
        }

        // Process authentication if a list of login:password pairs is specified
        let allowed_credentials = self.allowed_credentials.lock().unwrap().to_vec();
        if !allowed_credentials.is_empty() {
            if let Some(auth_header) = req.headers().get(PROXY_AUTHORIZATION) {
                let header_credentials = auth_header.to_str().unwrap_or_default();

                if !is_allowed_credentials(&header_credentials, allowed_credentials) {
                    return Ok(require_basic_auth());
                }
            } else {
                return Ok(require_basic_auth());
            }
        }

        // Process method and call the appropriate handler
        match req.method() {
            &Method::CONNECT => self.process_connect(req).await,
            _ => self.process_request(req).await,
        }
    }

    async fn process_connect(self, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
        tokio::task::spawn(async move {
            let remote_addr = req.uri().authority().map(|auth| auth.to_string()).unwrap();
            let mut upgraded = hyper::upgrade::on(req).await.unwrap();

            self.tunnel(&mut upgraded, remote_addr).await
        });

        Ok(Response::new(Body::empty()))
    }

    async fn process_request(self, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
        let bind_addr = get_current_server_ip().parse::<IpAddr>().unwrap();

        let mut http = HttpConnector::new();
        http.set_local_address(Some(bind_addr));

        let client = Client::builder()
            .http1_title_case_headers(true)
            .http1_preserve_header_case(true)
            .build(http);
        let res = client.request(req).await?;

        Ok(res)
    }

    async fn tunnel<A>(self, upgraded: &mut A, addr_str: String) -> std::io::Result<()>
    where
        A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    {
        if let Ok(addrs) = addr_str.to_socket_addrs() {
            for addr in addrs {
                let socket = TcpSocket::new_v4()?;
                let bind_addr = get_rand_ipv4_socket_addr();

                if socket.bind(bind_addr).is_ok() {
                    if let Ok(mut server) = socket.connect(addr).await {
                        tokio::io::copy_bidirectional(upgraded, &mut server).await?;
                        return Ok(());
                    }
                }
            }
        } else {
            println!("error: {addr_str}");
        }

        Ok(())
    }
}

pub async fn start_proxy(
    listen_addr: SocketAddr,
    allowed_credentials: Vec<String>,
    allowed_hosts: Vec<String>,
    secret_token: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let allowed_credentials_arc = Arc::new(Mutex::new(allowed_credentials));
    let allowed_hosts_arc = Arc::new(Mutex::new(allowed_hosts));
    let secret_token_arc = Arc::new(Mutex::new(secret_token));

    let make_service = make_service_fn(move |addr: &AddrStream| {
        let time = formatted_time();
        println!(
            "\n\x1b[1m[{time}] [HTTP server] New connection from: {}\x1b[0m",
            addr.remote_addr()
        );

        let allowed_credentials_clone = allowed_credentials_arc.clone();
        let allowed_hosts_clone = allowed_hosts_arc.clone();
        let secret_token_clone = secret_token_arc.clone();

        async move {
            Ok::<_, hyper::Error>(service_fn(move |req| {
                Proxy {
                    allowed_credentials: allowed_credentials_clone.clone(),
                    allowed_hosts: allowed_hosts_clone.clone(),
                    secret_token: secret_token_clone.clone(),
                }
                .proxy(req)
            }))
        }
    });

    Server::bind(&listen_addr)
        .http1_preserve_header_case(true)
        .http1_title_case_headers(true)
        .serve(make_service)
        .await
        .map_err(|err| err.into())
}
