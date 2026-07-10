use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::Deadline;

pub(crate) const VERSION: u32 = 1;
pub(crate) const TOKEN_LEN: usize = 32;
pub(crate) const MAX_FRAME: usize = 128 * 1024 * 1024;
pub(crate) const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token(pub(crate) [u8; TOKEN_LEN]);

impl Token {
    pub(crate) fn generate() -> Result<Self> {
        let mut f = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
        let mut out = [0u8; TOKEN_LEN];
        f.read_exact(&mut out)
            .context("reading broker token entropy")?;
        Ok(Self(out))
    }

    pub(crate) fn from_bytes(bytes: [u8; TOKEN_LEN]) -> Self {
        Self(bytes)
    }

    pub(crate) fn ct_eq(&self, other: &Self) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BrokerMessage {
    Hello {
        version: u32,
        instance_id: String,
        token: Vec<u8>,
    },
    HelloOk {
        version: u32,
        instance_id: String,
    },
    Error {
        message: String,
    },
    Ping {
        nonce: Option<String>,
    },
    Pong {
        nonce: Option<String>,
    },
    Stop {
        reason: Option<String>,
    },
    Cdp {
        client_id: u64,
        session_id: Option<String>,
        id: Option<u64>,
        method: Option<String>,
        params: Option<Value>,
        result: Option<Value>,
        error: Option<Value>,
    },
}

pub(crate) fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    write_frame_until(stream, value, Deadline::after(DEFAULT_DEADLINE))
}

pub(crate) fn write_frame_until<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    deadline: Deadline,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME {
        bail!("broker frame exceeds 128MiB cap");
    }
    let len = u32::try_from(body.len()).context("broker frame length overflow")?;
    write_all_until(
        stream,
        &len.to_be_bytes(),
        deadline,
        "writing broker frame length",
    )?;
    write_all_until(stream, &body, deadline, "writing broker frame body")?;
    Ok(())
}

pub(crate) fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T> {
    read_frame_until(stream, Deadline::after(DEFAULT_DEADLINE))
}

pub(crate) fn read_frame_until<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
    deadline: Deadline,
) -> Result<T> {
    let mut len = [0u8; 4];
    read_exact_until(stream, &mut len, deadline, "reading broker frame length")?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("broker frame exceeds 128MiB cap");
    }
    let mut body = vec![0u8; len];
    read_exact_until(stream, &mut body, deadline, "reading broker frame body")?;
    Ok(serde_json::from_slice(&body)?)
}

fn remaining(deadline: Deadline, operation: &str) -> Result<Duration> {
    deadline
        .remaining()
        .filter(|remaining| !remaining.is_zero())
        .with_context(|| format!("timed out {operation}"))
}

fn wait(
    stream: &UnixStream,
    events: libc::c_short,
    deadline: Deadline,
    operation: &str,
) -> Result<()> {
    let mut pfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    let timeout = remaining(deadline, operation)?
        .as_millis()
        .min(i32::MAX as u128) as i32;
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout) };
    if rc < 0 {
        return Err(io::Error::last_os_error()).with_context(|| operation.to_string());
    }
    if rc == 0 {
        bail!("timed out {operation}");
    }
    if pfd.revents & libc::POLLNVAL != 0 {
        bail!("broker socket fd invalid");
    }
    Ok(())
}

fn write_all_until(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Deadline,
    operation: &str,
) -> Result<()> {
    stream.set_nonblocking(true)?;
    while !bytes.is_empty() {
        wait(stream, libc::POLLOUT, deadline, operation)?;
        match stream.write(bytes) {
            Ok(0) => bail!("broker socket write returned zero bytes"),
            Ok(n) => bytes = &bytes[n..],
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                continue;
            }
            Err(e) => return Err(e).with_context(|| operation.to_string()),
        }
    }
    Ok(())
}

fn read_exact_until(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Deadline,
    operation: &str,
) -> Result<()> {
    stream.set_nonblocking(true)?;
    while !bytes.is_empty() {
        wait(stream, libc::POLLIN, deadline, operation)?;
        match stream.read(bytes) {
            Ok(0) => bail!("broker socket closed"),
            Ok(n) => {
                let tmp = bytes;
                bytes = &mut tmp[n..];
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                continue;
            }
            Err(e) => return Err(e).with_context(|| operation.to_string()),
        }
    }
    Ok(())
}

pub(crate) fn client_handshake(
    stream: &mut UnixStream,
    instance_id: &str,
    token: &Token,
) -> Result<()> {
    client_handshake_until(
        stream,
        instance_id,
        token,
        Deadline::after(DEFAULT_DEADLINE),
    )
}

pub(crate) fn client_handshake_until(
    stream: &mut UnixStream,
    instance_id: &str,
    token: &Token,
    deadline: Deadline,
) -> Result<()> {
    write_frame_until(
        stream,
        &BrokerMessage::Hello {
            version: VERSION,
            instance_id: instance_id.to_string(),
            token: token.0.to_vec(),
        },
        deadline,
    )?;
    match read_frame_until(stream, deadline)? {
        BrokerMessage::HelloOk {
            version: VERSION,
            instance_id: got,
        } if got == instance_id => Ok(()),
        BrokerMessage::Error { message } => bail!("broker handshake rejected: {message}"),
        other => bail!("unexpected broker handshake response: {other:?}"),
    }
}

pub(crate) fn server_handshake(
    stream: &mut UnixStream,
    instance_id: &str,
    token: &Token,
) -> Result<()> {
    server_handshake_until(
        stream,
        instance_id,
        token,
        Deadline::after(DEFAULT_DEADLINE),
    )
}

pub(crate) fn server_handshake_until(
    stream: &mut UnixStream,
    instance_id: &str,
    token: &Token,
    deadline: Deadline,
) -> Result<()> {
    let msg: BrokerMessage = read_frame_until(stream, deadline)?;
    let BrokerMessage::Hello {
        version,
        instance_id: got,
        token: got_token,
    } = msg
    else {
        bail!("missing broker hello");
    };
    let token_ok = got_token.len() == TOKEN_LEN
        && token.ct_eq(&Token(got_token.try_into().unwrap_or([0; TOKEN_LEN])));
    if version != VERSION || got != instance_id || !token_ok {
        let _ = write_frame_until(
            stream,
            &BrokerMessage::Error {
                message: "invalid broker handshake".into(),
            },
            deadline,
        );
        bail!("invalid broker handshake");
    }
    write_frame_until(
        stream,
        &BrokerMessage::HelloOk {
            version: VERSION,
            instance_id: instance_id.to_string(),
        },
        deadline,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::net::UnixStream, thread};

    #[test]
    fn handshake_accepts_matching_token_and_rejects_wrong_token() {
        let token = Token::from_bytes([7; TOKEN_LEN]);
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let server_token = token.clone();
        let t = thread::spawn(move || server_handshake(&mut b, "i", &server_token));
        client_handshake(&mut a, "i", &token).unwrap();
        t.join().unwrap().unwrap();

        let (mut a, mut b) = UnixStream::pair().unwrap();
        let t = thread::spawn(move || {
            server_handshake(&mut b, "i", &Token::from_bytes([1; TOKEN_LEN]))
        });
        assert!(client_handshake(&mut a, "i", &Token::from_bytes([2; TOKEN_LEN])).is_err());
        assert!(t.join().unwrap().is_err());
    }

    #[test]
    fn length_cap_fails_closed_before_allocation() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
            .unwrap();
        let got: Result<BrokerMessage> = read_frame(&mut b);
        assert!(got.is_err());
    }
}
