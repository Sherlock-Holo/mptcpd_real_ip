use std::borrow::Cow;
use std::ffi::{c_int, CStr};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use std::{env, io};

use libc::{sockaddr_in, sockaddr_in6, AF_INET, AF_INET6};
use reqwest::{ClientBuilder, StatusCode};
use socket2::SockAddr;
use tracing::field::display;
use tracing::level_filters::LevelFilter;
use tracing::{error, field, info, info_span, Instrument, Span};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, Registry};

use crate::ffi::{
    mptcpd_idm_get_id, mptcpd_interface, mptcpd_kpm_add_addr, mptcpd_plugin_desc,
    mptcpd_plugin_ops, mptcpd_plugin_register_ops, mptcpd_pm, mptcpd_pm_get_idm, sockaddr,
    MPTCPD_ADDR_FLAG_SIGNAL, MPTCPD_ADDR_FLAG_SUBFLOW, MPTCPD_PLUGIN_PRIORITY_DEFAULT,
};

const NAME: &CStr = c"real_ip";
const GET_MY_IP: &str = "https://icanhazip.com";

#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[allow(non_upper_case_globals)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/ffi.rs"));
}

static OPS: mptcpd_plugin_ops = mptcpd_plugin_ops {
    new_connection: None,
    connection_established: None,
    connection_closed: None,
    new_address: None,
    address_removed: None,
    new_subflow: None,
    subflow_closed: None,
    subflow_priority: None,
    new_interface: None,
    update_interface: None,
    delete_interface: None,
    new_local_address: Some(addr_add),
    delete_local_address: None,
};

#[allow(non_upper_case_globals)]
#[no_mangle]
pub static mut _mptcpd_plugin: mptcpd_plugin_desc = mptcpd_plugin_desc {
    name: NAME.as_ptr(),
    description: c"mptcpd real ip plugin".as_ptr(),
    version: c"0.1.0".as_ptr(),
    priority: MPTCPD_PLUGIN_PRIORITY_DEFAULT as _,
    init: Some(init),
    exit: Some(exit),
};

extern "C" fn init(_: *mut mptcpd_pm) -> c_int {
    init_log();

    unsafe {
        if !mptcpd_plugin_register_ops(NAME.as_ptr(), &OPS as *const _) {
            error!("failed init real_ip plugin");

            return -1;
        }

        info!("init real_ip plugin done");

        0
    }
}

extern "C" fn exit(_: *mut mptcpd_pm) {
    info!("exit real_ip plugin");
}

fn init_log() {
    let layer = fmt::layer()
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(io::stderr);

    let targets = Targets::new().with_default(LevelFilter::INFO);
    Registry::default().with(targets).with(layer).init();
}

extern "C" fn addr_add(i: *const mptcpd_interface, sa: *const sockaddr, pm: *mut mptcpd_pm) {
    let iface_index = unsafe { (*i).index };

    let http_server = env::var("REAL_IP_HTTP_SERVER")
        .ok()
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(GET_MY_IP));

    let span = info_span!(
        "get_ip",
        %http_server,
        iface_index,
        src_addr = field::Empty
    );
    let _entered = span.enter();

    info!("start add addr");

    let sa = sa as *const libc::sockaddr;
    let src_addr: IpAddr = unsafe {
        let sa_ref = &*sa;
        if sa_ref.sa_family as c_int == AF_INET {
            let sockaddr = &*(sa as *const sockaddr_in);
            Ipv4Addr::from(u32::from_be(sockaddr.sin_addr.s_addr)).into()
        } else if sa_ref.sa_family as c_int == AF_INET6 as _ {
            let sockaddr = &*(sa as *const sockaddr_in6);
            Ipv6Addr::from(u128::from_be_bytes(sockaddr.sin6_addr.s6_addr)).into()
        } else {
            error!(sa_family = sa_ref.sa_family, "unknown sa family");

            return;
        }
    };

    span.record("src_addr", display(src_addr));

    let timeout = env::var("REAL_IP_TIMEOUT_SECONDS")
        .ok()
        .and_then(|timeout| timeout.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(10));

    let client = match ClientBuilder::new()
        .local_address(src_addr)
        .timeout(timeout)
        .build()
    {
        Err(err) => {
            error!(%err, %src_addr, "build http client failed");

            return;
        }

        Ok(client) => client,
    };

    let ip = block_on(
        async {
            let resp = client
                .get(http_server.as_ref())
                .send()
                .await
                .inspect_err(|err| error!(%err, "send get ip http request failed"))?;

            let status_code = resp.status();
            if status_code != StatusCode::OK {
                let body = resp.bytes().await.ok();
                let body = body.as_ref().map(|body| String::from_utf8_lossy(body));

                error!(%status_code, ?body, "http response status code not OK");

                return Err(anyhow::anyhow!("http response status code not OK"));
            }

            let body = resp
                .bytes()
                .await
                .inspect_err(|err| error!(%err, "get http body failed"))?;

            let body = String::from_utf8_lossy(&body);
            let ip = body
                .trim()
                .parse::<IpAddr>()
                .inspect_err(|err| error!(%err, %body, "parse http body failed"))?;

            Ok::<_, anyhow::Error>(ip)
        }
        .instrument(Span::current()),
    );
    let ip = match ip {
        Err(_) => return,
        Ok(ip) => ip,
    };

    info!(%ip, "get real ip done");

    let sock_addr = SockAddr::from(SocketAddr::new(ip, 0));

    let res = unsafe {
        let idm = mptcpd_pm_get_idm(pm);
        let id = mptcpd_idm_get_id(idm, sock_addr.as_ptr() as _);

        mptcpd_kpm_add_addr(
            pm,
            sock_addr.as_ptr() as _,
            id,
            MPTCPD_ADDR_FLAG_SIGNAL | MPTCPD_ADDR_FLAG_SUBFLOW,
            iface_index,
        )
    };

    if res != 0 {
        error!(res, %ip, "unable to advertise ip");

        return;
    }

    info!(%ip, "advertise ip done");
}

fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}
