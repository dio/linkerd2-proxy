use crate::endpoint::{HttpConcrete, TcpAccept, TcpLogical};
use linkerd2_app_core::{discovery_rejected, svc::stack::FilterRequest, Addr, Error, IpMatch};
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

#[derive(Clone, Debug)]
pub struct AllowTcpResolve(pub IpMatch);

#[derive(Copy, Clone, Debug)]
pub struct AllowHttpResolve;

impl FilterRequest<TcpAccept> for AllowProfile {
    type Request = SocketAddr;

    fn filter(&self, target: TcpAccept) -> Result<SocketAddr, Error> {
        if self.0.matches(target.addr.ip()) {
            Ok(target.addr)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

impl FilterRequest<HttpConcrete> for AllowHttpResolve {
    type Request = Addr;

    fn filter(&self, target: HttpConcrete) -> Result<Addr, Error> {
        target.resolve.ok_or_else(|| discovery_rejected().into())
    }
}

impl FilterRequest<TcpLogical> for AllowTcpResolve {
    type Request = SocketAddr;

    fn filter(&self, target: TcpLogical) -> Result<SocketAddr, Error> {
        if self.0.matches(target.addr.ip()) {
            Ok(target.addr)
        } else {
            Err(discovery_rejected().into())
        }
    }
}
