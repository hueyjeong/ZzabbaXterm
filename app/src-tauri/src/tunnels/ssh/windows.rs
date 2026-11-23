use std::{io, mem, net::{Ipv4Addr, Ipv6Addr, SocketAddr}, path::Path, process::{Child, Command}, ptr};

use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR},
    NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCP_STATE_ESTAB, TCP_TABLE_CLASS, TCP_TABLE_OWNER_PID_CONNECTIONS, TCP_TABLE_OWNER_PID_LISTENER},
};

use super::{Listener, SshPlan, require_supported_listener};

pub(super) struct Readiness;

impl Readiness {
    pub(super) fn configure(command: &mut Command, _: &Path, plan: &SshPlan) -> Result<Self, String> {
        require_supported_listener(plan.listener())?;
        command.args(plan.approved_options());
        Ok(Self)
    }

    pub(super) fn poll(&mut self, child: &Child, plan: &SshPlan) -> Result<bool, String> {
        let Listener::Local(address) = plan.listener() else { unreachable!("로컬 수신만 지원") };
        owns_listener(address, child.id()).map_err(|error| format!("SSH 수신 소유권: {error}"))
    }

    pub(super) fn owns_stream(&self, child: &Child, listen: SocketAddr, peer: SocketAddr) -> io::Result<bool> {
        owns_stream(&[child.id()], listen, peer)
    }

    pub(super) fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 승인 수신 주소에서 받아들인 `peer` 연결의 서버 쪽 소켓을 `pids` 중 하나가 소유하는지 확인한다.
pub(in crate::tunnels) fn owns_stream(pids: &[u32], listen: SocketAddr, peer: SocketAddr) -> io::Result<bool> {
    let (SocketAddr::V4(listen), SocketAddr::V4(peer)) = (listen, peer) else { return Ok(false); };
    table::<MIB_TCPROW_OWNER_PID>(2, TCP_TABLE_OWNER_PID_CONNECTIONS, |row| {
        pids.contains(&row.dwOwningPid) && row.dwState == MIB_TCP_STATE_ESTAB as u32
            && u16::from_be(row.dwLocalPort as u16) == listen.port()
            && Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()) == *listen.ip()
            && u16::from_be(row.dwRemotePort as u16) == peer.port()
            && Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes()) == *peer.ip()
    })
}

pub(super) fn owns_listener(address: SocketAddr, pid: u32) -> io::Result<bool> {
    let port = address.port();
    match address {
        SocketAddr::V4(address) => table::<MIB_TCPROW_OWNER_PID>(2, TCP_TABLE_OWNER_PID_LISTENER, |row| {
            row.dwOwningPid == pid && u16::from_be(row.dwLocalPort as u16) == port
                && Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()) == *address.ip()
        }),
        SocketAddr::V6(address) => table::<MIB_TCP6ROW_OWNER_PID>(23, TCP_TABLE_OWNER_PID_LISTENER, |row| {
            row.dwOwningPid == pid && u16::from_be(row.dwLocalPort as u16) == port
                && Ipv6Addr::from(row.ucLocalAddr) == *address.ip() && row.dwLocalScopeId == address.scope_id()
        }),
    }
}

fn table<Row>(family: u32, class: TCP_TABLE_CLASS, matches: impl Fn(&Row) -> bool) -> io::Result<bool> {
    let mut words = Vec::<u32>::new();
    let written = loop {
        let mut size = u32::try_from(mem::size_of_val(words.as_slice())).map_err(|_| io::ErrorKind::InvalidData)?;
        let buffer = if words.is_empty() { ptr::null_mut() } else { words.as_mut_ptr().cast() };
        // SAFETY: size는 채워진 u32 버퍼의 바이트 크기 이하이며, 첫 호출의 버퍼는 null이다.
        let result = unsafe { GetExtendedTcpTable(buffer, &mut size, 0, family, class, 0) };
        match result {
            ERROR_INSUFFICIENT_BUFFER => words.resize((size as usize).div_ceil(4), 0),
            NO_ERROR => break size as usize,
            code => return Err(io::Error::from_raw_os_error(code as i32)),
        }
    };
    let header = mem::size_of::<u32>();
    if written < header || written > mem::size_of_val(words.as_slice()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let count = words[0] as usize;
    if count > (written - header) / mem::size_of::<Row>() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // SAFETY: 두 Windows 행 형식의 정렬은 4바이트이며, 행 개수는 위에서 검사했다.
    let rows = unsafe { std::slice::from_raw_parts(words[1..].as_ptr().cast::<Row>(), count) };
    Ok(rows.iter().any(matches))
}
