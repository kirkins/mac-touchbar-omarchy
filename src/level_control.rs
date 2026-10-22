//! The level-control socket clients: explicit mixer and display
//! endpoints for the slider widgets. The wire protocol types they
//! speak live in `dfr-render` with the widgets that render them.

pub use dfr_render::level::{Request, State, MAX_VOLUME_PERCENT};

use dfr_render::desktop::Endpoint;
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::net::{SocketAddr, UnixStream},
    },
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

fn request(
    address: &SocketAddr,
    uid: u32,
    action: Request,
    minimum: u8,
    maximum: u8,
) -> io::Result<State> {
    let command = match action {
        Request::Get => "get".into(),
        Request::Set(percent) if percent >= minimum && percent <= maximum => {
            format!("set {percent}")
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid level request",
            ))
        }
    };
    let mut connection = UnixStream::connect_addr(address)?;
    connection.set_read_timeout(Some(Duration::from_secs(10)))?;
    connection.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SO_PEERCRED is supplied by the kernel, not the peer's request payload.
    let rc = unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut _ as *mut libc::c_void,
            &mut length,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if credentials.uid != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unexpected level receiver UID",
        ));
    }
    connection.write_all(command.as_bytes())?;
    connection.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    connection.take(64).read_to_string(&mut response)?;
    let fields: Vec<_> = response.split_whitespace().collect();
    if let ["ok", percent, muted] = fields.as_slice() {
        if let (Ok(percent), "0" | "1") = (percent.parse::<u8>(), *muted) {
            if percent <= maximum {
                return Ok(State {
                    percent,
                    muted: *muted == "1",
                });
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Level receiver rejected request",
    ))
}

pub struct LevelControl {
    requests: Sender<(u64, Request)>,
    replies: Receiver<(u64, io::Result<State>)>,
    pub wake: UnixStream,
}

impl LevelControl {
    pub fn volume() -> Self {
        // Same desktop-user abstract-socket boundary as the optional voice
        // integration. No home/runtime file or PipeWire privilege is granted.
        Self::connect_to(Endpoint::Desktop("dfr-volume"), 0, MAX_VOLUME_PERCENT)
    }

    pub fn brightness() -> Self {
        Self::connect_to(Endpoint::Desktop("dfr-brightness"), 1, 100)
    }

    fn connect_to(endpoint: Endpoint, minimum: u8, maximum: u8) -> Self {
        let (requests, incoming) = mpsc::channel();
        let (outgoing, replies) = mpsc::channel();
        let (wake, mut notify) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        notify.set_nonblocking(true).unwrap();
        thread::spawn(move || {
            while let Ok((mut revision, mut action)) = incoming.recv() {
                // A fast drag may outpace the mixer. Keep the latest absolute
                // target; every executed command still finishes in order.
                while let Ok((next_revision, next_action)) = incoming.try_recv() {
                    if matches!(action, Request::Get)
                        || matches!((action, next_action), (Request::Set(_), Request::Set(_)))
                    {
                        revision = next_revision;
                        action = next_action;
                    } else {
                        let (address, uid) = endpoint.resolve();
                        if outgoing
                            .send((revision, request(&address, uid, action, minimum, maximum)))
                            .is_err()
                        {
                            return;
                        }
                        let _ = notify.write(&[1]);
                        revision = next_revision;
                        action = next_action;
                    }
                }
                let (address, uid) = endpoint.resolve();
                if outgoing
                    .send((revision, request(&address, uid, action, minimum, maximum)))
                    .is_err()
                {
                    break;
                }
                let _ = notify.write(&[1]);
            }
        });
        Self {
            requests,
            replies,
            wake,
        }
    }

    pub fn send(&self, revision: u64, action: Request) {
        let _ = self.requests.send((revision, action));
    }

    pub fn update(&mut self) -> Vec<(u64, io::Result<State>)> {
        while matches!(self.wake.read(&mut [0; 64]), Ok(n) if n > 0) {}
        self.replies.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn address() -> SocketAddr {
        let name = format!(
            "dfr-shared-sliders-level-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        );
        assert!(name.starts_with("dfr-shared-sliders-level-test-"));
        assert_ne!(name, "dfr-volume-1000");
        assert_ne!(name, "dfr-brightness-1000");
        SocketAddr::from_abstract_name(name.as_bytes()).unwrap()
    }

    #[test]
    fn absolute_client_preserves_zero_before_a_following_read_and_reads_back_the_mixer() {
        let address = address();
        let listener = UnixListener::bind_addr(&address).unwrap();
        assert_eq!(
            listener.local_addr().unwrap().as_abstract_name(),
            address.as_abstract_name()
        );
        let actual = Arc::new(Mutex::new(100u8));
        let mixer = Arc::clone(&actual);
        let server = thread::spawn(move || {
            for _ in 0..5 {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut message = String::new();
                (&mut connection)
                    .take(32)
                    .read_to_string(&mut message)
                    .unwrap();
                if let Some(value) = message.strip_prefix("set ") {
                    *mixer.lock().unwrap() = value.parse().unwrap();
                } else {
                    assert_eq!(message, "get");
                }
                let percent = *mixer.lock().unwrap();
                write!(connection, "ok {percent} 0").unwrap();
            }
        });
        let uid = unsafe { libc::geteuid() };
        let mut client =
            LevelControl::connect_to(Endpoint::Fixed(address, uid), 0, MAX_VOLUME_PERCENT);
        let receive = |client: &mut LevelControl, revision| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                for (id, state) in client.update() {
                    if id == revision {
                        return state.unwrap();
                    }
                }
                assert!(Instant::now() < deadline, "audio client never replied");
                thread::sleep(Duration::from_millis(5));
            }
        };
        client.send(1, Request::Get);
        assert_eq!(receive(&mut client, 1).percent, 100);
        client.send(2, Request::Set(0));
        client.send(3, Request::Get);
        assert_eq!(receive(&mut client, 3).percent, 0);
        assert_eq!(*actual.lock().unwrap(), 0);
        client.send(4, Request::Set(65));
        assert_eq!(receive(&mut client, 4).percent, 65);
        client.send(5, Request::Set(130));
        assert_eq!(receive(&mut client, 5).percent, 130);
        assert_eq!(*actual.lock().unwrap(), 130);
        server.join().unwrap();
    }

    #[test]
    fn client_rejects_a_receiver_with_the_wrong_uid() {
        let address = address();
        let listener = UnixListener::bind_addr(&address).unwrap();
        assert_eq!(
            listener.local_addr().unwrap().as_abstract_name(),
            address.as_abstract_name()
        );
        let server = thread::spawn(move || {
            let _ = listener.accept().unwrap();
        });
        let uid = unsafe { libc::geteuid() }.wrapping_add(1);
        let error = request(&address, uid, Request::Set(0), 0, MAX_VOLUME_PERCENT).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        server.join().unwrap();
    }

    #[test]
    fn brightness_transport_uses_its_own_range_and_confirmed_readback() {
        let address = address();
        let listener = UnixListener::bind_addr(&address).unwrap();
        assert_eq!(
            listener.local_addr().unwrap().as_abstract_name(),
            address.as_abstract_name()
        );
        let server = thread::spawn(move || {
            for (command, reply) in [
                ("get", "ok 93 0"),
                ("set 1", "ok 1 0"),
                ("set 50", "ok 47 0"),
                ("set 100", "ok 100 0"),
            ] {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut message = String::new();
                (&mut connection)
                    .take(32)
                    .read_to_string(&mut message)
                    .unwrap();
                assert_eq!(message, command);
                connection.write_all(reply.as_bytes()).unwrap();
            }
        });
        let uid = unsafe { libc::geteuid() };
        for (action, expected) in [
            (Request::Get, 93),
            (Request::Set(1), 1),
            (Request::Set(50), 47),
            (Request::Set(100), 100),
        ] {
            assert_eq!(
                request(&address, uid, action, 1, 100).unwrap().percent,
                expected
            );
        }
        server.join().unwrap();
    }

    #[test]
    fn invalid_or_failed_brightness_readback_is_never_success() {
        for reply in [
            "error",
            "ok 101 0",
            "ok 47 2",
            "ok NaN 0",
            "ok 47 0 extra",
            "",
        ] {
            let address = address();
            let listener = UnixListener::bind_addr(&address).unwrap();
            assert_eq!(
                listener.local_addr().unwrap().as_abstract_name(),
                address.as_abstract_name()
            );
            let server = thread::spawn(move || {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut message = String::new();
                (&mut connection)
                    .take(32)
                    .read_to_string(&mut message)
                    .unwrap();
                assert_eq!(message, "get");
                connection.write_all(reply.as_bytes()).unwrap();
            });
            let uid = unsafe { libc::geteuid() };
            assert_eq!(
                request(&address, uid, Request::Get, 1, 100)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            server.join().unwrap();
        }
    }
}
