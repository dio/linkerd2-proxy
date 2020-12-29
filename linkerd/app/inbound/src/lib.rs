//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

mod allow_discovery;
pub mod endpoint;
mod prevent_loop;
mod require_identity_for_ports;
pub mod tcp;

pub use self::endpoint::{
    HttpEndpoint, ProfileTarget, RequestTarget, Target, TcpAccept, TcpEndpoint,
};
use self::{
    allow_discovery::AllowProfile, prevent_loop::PreventLoop,
    require_identity_for_ports::RequireIdentityForPorts,
};
use futures::future;
use linkerd2_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    drain,
    dst,
    errors,
    metrics,
    //opaque_transport::DetectHeader,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        http::{self, orig_proto, strip_header},
        identity, tap,
    },
    reconnect,
    spans::SpanConverter,
    svc,
    transport::{self, io, listen, tls},
    Error,
    NameAddr,
    NameMatch,
    TraceContext,
    DST_OVERRIDE_HEADER,
};
use std::{collections::HashMap, time::Duration};
use tokio::{net::TcpStream, sync::mpsc};
use tracing::debug_span;

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
    pub disable_protocol_detection_for_ports: SkipByPort,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct SkipByPort(std::sync::Arc<indexmap::IndexSet<u16>>);

type SensorIo<T> = io::SensorIo<T, transport::metrics::Sensor>;

// === impl Config ===

#[allow(clippy::too_many_arguments)]
impl Config {
    pub fn build<L, S, P>(
        self,
        listen_addr: std::net::SocketAddr,
        local_identity: tls::Conditional<identity::Local>,
        _http_loopback: L,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl tower::Service<
            tokio::net::TcpStream,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Clone
           + Send
           + 'static
    where
        L: svc::NewService<Target, Service = S> + Unpin + Clone + Send + Sync + 'static,
        S: tower::Service<
                http::Request<http::boxed::BoxBody>,
                Response = http::Response<http::boxed::BoxBody>,
            > + Unpin
            + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Unpin + Send,
        P: profiles::GetProfile<NameAddr> + Unpin + Clone + Send + Sync + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
    {
        let prevent_loop = PreventLoop::from(listen_addr.port());
        let tcp_connect = self.build_tcp_connect(prevent_loop, &metrics);
        let http_router = self.build_http_router(
            tcp_connect.clone(),
            profiles_client,
            tap_layer,
            metrics.clone(),
            span_sink.clone(),
        );

        // Forwards TCP streams that cannot be decoded as HTTP.
        let tcp_forward = svc::stack(tcp_connect)
            .push_make_thunk()
            .push_on_response(
                svc::layers()
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(drain.clone())),
            )
            .instrument(|_: &_| debug_span!("tcp"))
            .into_inner();

        let accept = self.build_detect_http(
            tcp_forward.clone(),
            self.build_http_server(http_router, metrics.clone(), span_sink, drain),
        );

        self.build_tls_accept(accept, tcp_forward, local_identity, metrics.transport)
    }

    pub fn build_tcp_connect(
        &self,
        prevent_loop: PreventLoop,
        metrics: &metrics::Proxy,
    ) -> impl tower::Service<
        TcpEndpoint,
        Error = Error,
        Response = io::BoxedIo,
        Future = impl future::Future + Unpin + Send,
    > + tower::Service<
        HttpEndpoint,
        Error = Error,
        Response = io::BoxedIo,
        Future = impl future::Future + Unpin + Send,
    > + Unpin
           + Clone
           + Send {
        // Establishes connections to remote peers (for both TCP
        // forwarding and HTTP proxying).
        svc::connect(self.proxy.connect.keepalive)
            // Limits the time we wait for a connection to be established.
            .push_timeout(self.proxy.connect.timeout)
            .push(metrics.transport.layer_connect())
            .push_map_response(io::BoxedIo::new) // Ensures the transport propagates shutdown properly.
            .push_request_filter(prevent_loop)
            .into_inner()
    }

    pub fn build_http_router<C, P>(
        &self,
        tcp_connect: C,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl svc::NewService<
        Target,
        Service = impl tower::Service<
            http::Request<http::boxed::BoxBody>,
            Response = http::Response<http::boxed::BoxBody>,
            Error = Error,
            Future = impl Send,
        > + Clone
                      + Send
                      + Sync
                      + Unpin,
    > + Unpin
           + Clone
           + Send
    where
        C: tower::Service<HttpEndpoint> + Unpin + Clone + Send + Sync + 'static,
        C::Error: Into<Error>,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        P: profiles::GetProfile<NameAddr> + Unpin + Clone + Send + Sync + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
    {
        let Config {
            allow_discovery,
            proxy:
                ProxyConfig {
                    connect,
                    buffer_capacity,
                    cache_max_idle_age,
                    dispatch_timeout,
                    ..
                },
            ..
        } = self.clone();

        // Creates HTTP clients for each inbound port & HTTP settings.
        let target = svc::stack(tcp_connect)
            .push(http::client::layer(
                connect.h1_settings,
                connect.h2_settings,
            ))
            .push(reconnect::layer({
                let backoff = connect.backoff;
                move |_| Ok(backoff.stream())
            }))
            .check_new_service::<HttpEndpoint, http::Request<_>>()
            .push_map_target(HttpEndpoint::from)
            // Registers the stack to be tapped.
            .push(tap_layer)
            // Records metrics for each `Target`.
            .push(metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(TraceContext::layer(
                span_sink.map(|span_sink| SpanConverter::client(span_sink, trace_labels())),
            ))
            .push_on_response(http::boxed::BoxResponse::layer())
            .check_new_service::<Target, http::Request<_>>();

        // Attempts to discover a service profile for each logical target (as
        // informed by the request's headers). The stack is cached until a
        // request has not been received for `cache_max_idle_age`.
        target
            .clone()
            .check_new_service::<Target, http::Request<http::boxed::BoxBody>>()
            .push_on_response(http::boxed::BoxRequest::layer())
            // The target stack doesn't use the profile resolution, so drop it.
            .push_map_target(endpoint::Target::from)
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    // Sets the route as a request extension so that it can be used
                    // by tap.
                    .push_http_insert_target()
                    // Records per-route metrics.
                    .push(metrics.http_route.to_layer::<classify::Response, _>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::NewClassify::layer())
                    .check_new_clone::<dst::Route>()
                    .push_map_target(endpoint::route)
                    .into_inner(),
            ))
            .push_map_target(endpoint::Logical::from)
            .push(profiles::discover::layer(
                profiles_client,
                AllowProfile(allow_discovery),
            ))
            .push_on_response(http::boxed::BoxResponse::layer())
            .instrument(|_: &Target| debug_span!("profile"))
            // Skip the profile stack if it takes too long to become ready.
            .push_when_unready(target.clone(), self.profile_idle_timeout)
            .check_new_service::<Target, http::Request<http::boxed::BoxBody>>()
            .push_on_response(
                svc::layers()
                    .push(svc::FailFast::layer("Logical", dispatch_timeout))
                    .push_spawn_buffer(buffer_capacity)
                    .push(metrics.stack.layer(stack_labels("http", "logical"))),
            )
            .push_cache(cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::boxed::BoxResponse::layer()),
            )
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .push(svc::BoxNewService::layer())
            .check_new_service::<Target, http::Request<http::boxed::BoxBody>>()
            .into_inner()
    }

    pub fn build_http_server<I, H, HSvc>(
        &self,
        http_router: H,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        (http::Version, TcpAccept),
        Service = impl tower::Service<
            I,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Clone
                      + Send
                      + 'static,
    > + Clone
           + Send
           + 'static
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Unpin + Send + 'static,
        H: svc::NewService<Target, Service = HSvc> + Unpin + Clone + Send + Sync + 'static,
        HSvc: tower::Service<
                http::Request<http::boxed::BoxBody>,
                Response = http::Response<http::boxed::BoxBody>,
                Error = Error,
            > + Clone
            + Send
            + 'static,
        HSvc::Future: Send,
    {
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            dispatch_timeout,
            max_in_flight_requests,
            ..
        } = self.proxy.clone();

        svc::stack(http_router)
            // Removes the override header after it has been used to
            // determine a reuquest target.
            .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
            // Routes each request to a target, obtains a service for that
            // target, and dispatches the request.
            .instrument_from_target()
            .push(svc::NewRouter::layer(RequestTarget::from))
            .check_new_service::<TcpAccept, http::Request<_>>()
            // Used by tap.
            .push_http_insert_target()
            .push_on_response(
                svc::layers()
                    // Downgrades the protocol if upgraded by an outbound proxy.
                    .push(orig_proto::Downgrade::layer())
                    // Limits the number of in-flight requests.
                    .push(svc::ConcurrencyLimit::layer(max_in_flight_requests))
                    // Eagerly fail requests when the proxy is out of capacity for a
                    // dispatch_timeout.
                    .push(svc::FailFast::layer("HTTP Server", dispatch_timeout))
                    .push(metrics.http_errors)
                    // Synthesizes responses for proxy errors.
                    .push(errors::layer())
                    .push(TraceContext::layer(span_sink.map(|span_sink| {
                        SpanConverter::server(span_sink, trace_labels())
                    })))
                    .push(metrics.stack.layer(stack_labels("http", "server")))
                    .push(http::boxed::BoxRequest::layer())
                    .push(http::boxed::BoxResponse::layer()),
            )
            .push(http::NewNormalizeUri::layer())
            .push_map_target(|(_, accept): (_, TcpAccept)| accept)
            .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
            .check_new_service::<(http::Version, TcpAccept), http::Request<_>>()
            .push(http::NewServeHttp::layer(h2_settings, drain))
            .into_inner()
    }

    pub fn build_detect_http<I, F, FSvc, H, HSvc>(
        &self,
        tcp_forward: F,
        http: H,
    ) -> impl svc::NewService<
        TcpAccept,
        Service = impl tower::Service<
            I,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Clone
           + Send
           + 'static
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Unpin + Send + 'static,
        F: svc::NewService<TcpEndpoint, Service = FSvc> + Unpin + Clone + Send + Sync + 'static,
        FSvc: tower::Service<io::PrefixedIo<I>, Response = ()> + Clone + Send + Sync + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
        H: svc::NewService<(http::Version, TcpAccept), Service = HSvc>
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
        HSvc: tower::Service<io::PrefixedIo<I>, Response = ()> + Clone + Send + Sync + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        // When HTTP detection fails, forward the connection to the application
        // as an opaque TCP stream.
        let tcp = svc::stack(tcp_forward)
            .push_map_target(TcpEndpoint::from)
            // .push_switch(
            //     prevent_loop.into(),
            //     // If the connection targets the inbound port, try to detect an
            //     // opaque transport header and rewrite the target port
            //     // accordingly. If there was no opaque transport header, the
            //     // forwarding will fail when the tcp connect stack applies loop
            //     // prevention.
            //     svc::stack(tcp_forward)
            //         .push_map_target(TcpEndpoint::from)
            //         .push(transport::NewDetectService::layer(
            //             transport::detect::DetectTimeout::new(
            //                 self.proxy.detect_protocol_timeout,
            //                 DetectHeader::default(),
            //             ),
            //         )),
            // )
            .into_inner();

        svc::stack(http)
            .push(svc::stack::NewOptional::layer(tcp))
            .push_cache(self.proxy.cache_max_idle_age)
            .push(transport::NewDetectService::layer(
                transport::detect::DetectTimeout::new(
                    self.proxy.detect_protocol_timeout,
                    http::DetectHttp::default(),
                ),
            ))
            .into_inner()
    }

    pub fn build_tls_accept<D, DSvc, F, FSvc>(
        &self,
        detect: D,
        tcp_forward: F,
        identity: tls::Conditional<identity::Local>,
        metrics: transport::Metrics,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl tower::Service<
            TcpStream,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Clone
           + Send
           + 'static
    where
        D: svc::NewService<TcpAccept, Service = DSvc> + Clone + Send + Sync + Unpin + 'static,
        DSvc: tower::Service<SensorIo<io::BoxedIo>, Response = ()> + Send + Unpin + 'static,
        DSvc::Error: Into<Error>,
        DSvc::Future: Send,
        F: svc::NewService<TcpEndpoint, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: tower::Service<SensorIo<TcpStream>, Response = ()> + Send + Unpin + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
    {
        svc::stack(detect)
            .push_request_filter(self.require_identity_for_inbound_ports.clone())
            .push(metrics.layer_accept())
            .push_map_target(TcpAccept::from)
            .push(tls::DetectTls::layer(
                identity,
                self.proxy.detect_protocol_timeout,
            ))
            .push_switch(
                self.disable_protocol_detection_for_ports.clone(),
                svc::stack(tcp_forward)
                    .push_map_target(TcpEndpoint::from)
                    .push(metrics.layer_accept())
                    .push_map_target(TcpAccept::from)
                    .into_inner(),
            )
            .into_inner()
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}

// === impl SkipByPort ===

impl From<indexmap::IndexSet<u16>> for SkipByPort {
    fn from(ports: indexmap::IndexSet<u16>) -> Self {
        SkipByPort(ports.into())
    }
}

impl svc::stack::Switch<listen::Addrs> for SkipByPort {
    fn use_primary(&self, t: &listen::Addrs) -> bool {
        !self.0.contains(&t.target_addr().port())
    }
}
